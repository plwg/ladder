//! Fuzzy candidate index and an incremental, multi-threaded searcher built on nucleo-matcher.

use crate::journal::Usage;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32String};
use rustc_hash::FxHashMap;
use std::cmp::Reverse;

/// How many ranked results are kept per query (what the dropdown can scroll through).
pub const LIMIT: usize = 200;
/// Below this many candidates a single thread is faster than spawning workers.
const PARALLEL_THRESHOLD: usize = 32_768;

pub struct Item {
    pub text: String,
    hay: Utf32String,
    /// Frecency bonus added to the match score (roughly one matched char at most).
    pub bonus: u32,
}

#[derive(Default)]
pub struct Index {
    items: Vec<Item>,
    ids: FxHashMap<String, u32>,
    by_bonus: Vec<u32>,
    generation: u64,
}

fn frecency(usage: &Usage, recency_rank: f64) -> u32 {
    let freq = (usage.count + 1).ilog2().min(4) * 2;
    freq + (recency_rank * 8.0) as u32
}

impl Index {
    pub fn build(entries: Vec<(String, Usage)>) -> Index {
        let n = entries.len();
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_unstable_by_key(|&i| entries[i].1.last_seq);
        let mut rank = vec![0.0; n];
        for (r, &i) in order.iter().enumerate() {
            rank[i] = (r + 1) as f64 / n as f64;
        }
        let mut index = Index::default();
        index.items.reserve(n);
        for (i, (text, usage)) in entries.into_iter().enumerate() {
            index.ids.insert(text.clone(), i as u32);
            let hay = Utf32String::from(text.as_str());
            index.items.push(Item {
                text,
                hay,
                bonus: frecency(&usage, rank[i]),
            });
        }
        index.sort_by_bonus();
        index
    }

    fn sort_by_bonus(&mut self) {
        let items = &self.items;
        self.by_bonus = (0..items.len() as u32).collect();
        self.by_bonus
            .sort_by_key(|&i| (Reverse(items[i as usize].bonus), i));
        self.generation += 1;
    }

    /// Add or refresh a candidate that was just used, ranking it as most recent.
    pub fn upsert(&mut self, text: &str, usage: &Usage) {
        let bonus = frecency(usage, 1.0);
        match self.ids.get(text) {
            Some(&i) => self.items[i as usize].bonus = bonus,
            None => {
                self.ids.insert(text.to_owned(), self.items.len() as u32);
                let hay = Utf32String::from(text);
                self.items.push(Item {
                    text: text.to_owned(),
                    hay,
                    bonus,
                });
            }
        }
        self.sort_by_bonus();
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn get(&self, id: u32) -> &Item {
        &self.items[id as usize]
    }
}

pub struct Searcher {
    matchers: Vec<Matcher>,
    pattern: Pattern,
    query: String,
    generation: u64,
    /// Identity of the index `pool` refers to (compared, never dereferenced).
    index: *const Index,
    /// Every item matching `query`; the next query can narrow this instead of rescanning.
    pool: Option<Vec<u32>>,
    /// Ranked best matches (ids into the index).
    pub results: Vec<u32>,
    /// Number of items matching the current query.
    pub total: usize,
}

impl Default for Searcher {
    fn default() -> Self {
        Self::new()
    }
}

fn matcher() -> Matcher {
    let mut config = Config::DEFAULT;
    config.prefer_prefix = true;
    Matcher::new(config)
}

/// Whether every string matching `new` also matches `old`, so `old`'s pool can be reused.
fn narrows(old: &str, new: &str) -> bool {
    !old.is_empty() && new.starts_with(old) && !old.contains(['!', '^', '$', '\'', '\\'])
}

impl Searcher {
    pub fn new() -> Self {
        Searcher {
            matchers: vec![matcher()],
            pattern: Pattern::default(),
            query: String::new(),
            generation: 0,
            index: std::ptr::null(),
            pool: None,
            results: Vec::new(),
            total: 0,
        }
    }

    pub fn search(&mut self, index: &Index, query: &str) {
        if !std::ptr::eq(self.index, index) {
            self.index = index;
            self.pool = None;
        }
        if query.trim().is_empty() {
            self.results.clear();
            self.results.extend(index.by_bonus.iter().take(LIMIT));
            self.total = index.len();
            self.query.clear();
            self.pool = None;
            return;
        }
        let reuse = self.generation == index.generation && narrows(&self.query, query);
        let candidates = if reuse { self.pool.take() } else { None };
        self.pattern
            .reparse(query, CaseMatching::Smart, Normalization::Smart);
        let mut scored = self.score(index, candidates.as_deref());
        self.total = scored.len();
        self.pool = Some(scored.iter().map(|&(_, id)| id).collect());
        self.query.clear();
        self.query.push_str(query);
        self.generation = index.generation;

        let key =
            |&(score, id): &(u32, u32)| (Reverse(score), index.items[id as usize].text.len(), id);
        if scored.len() > LIMIT {
            scored.select_nth_unstable_by_key(LIMIT, key);
            scored.truncate(LIMIT);
        }
        scored.sort_unstable_by_key(key);
        self.results.clear();
        self.results.extend(scored.iter().map(|&(_, id)| id));
    }

    fn score(&mut self, index: &Index, candidates: Option<&[u32]>) -> Vec<(u32, u32)> {
        let n = candidates.map_or(index.len(), <[u32]>::len);
        let id_at = |i: usize| candidates.map_or(i as u32, |c| c[i]);
        let pattern = &self.pattern;
        let run = |matcher: &mut Matcher, lo: usize, hi: usize| {
            let mut out = Vec::new();
            for i in lo..hi {
                let id = id_at(i);
                let item = &index.items[id as usize];
                if let Some(s) = pattern.score(item.hay.slice(..), matcher) {
                    out.push((s + item.bonus, id));
                }
            }
            out
        };
        if n < PARALLEL_THRESHOLD {
            return run(&mut self.matchers[0], 0, n);
        }
        let threads = std::thread::available_parallelism()
            .map_or(1, |t| t.get())
            .min(16);
        while self.matchers.len() < threads {
            self.matchers.push(matcher());
        }
        let chunk = n.div_ceil(threads);
        std::thread::scope(|s| {
            let handles: Vec<_> = self
                .matchers
                .iter_mut()
                .take(threads)
                .enumerate()
                .map(|(t, m)| {
                    let run = &run;
                    s.spawn(move || run(m, (t * chunk).min(n), ((t + 1) * chunk).min(n)))
                })
                .collect();
            let mut all = Vec::new();
            for h in handles {
                all.extend(h.join().unwrap_or_default());
            }
            all
        })
    }

    /// Char indices of `id`'s text matched by the current query, for highlighting.
    pub fn highlight(&mut self, index: &Index, id: u32) -> Vec<u32> {
        let mut out = Vec::new();
        if !self.query.is_empty() {
            self.pattern.indices(
                index.items[id as usize].hay.slice(..),
                &mut self.matchers[0],
                &mut out,
            );
            out.sort_unstable();
            out.dedup();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(names: &[&str]) -> Index {
        Index::build(
            names
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    (
                        n.to_string(),
                        Usage {
                            count: 1,
                            last_seq: i as u32,
                        },
                    )
                })
                .collect(),
        )
    }

    fn texts<'a>(ix: &'a Index, s: &Searcher) -> Vec<&'a str> {
        s.results.iter().map(|&i| ix.get(i).text.as_str()).collect()
    }

    #[test]
    fn ranks_and_narrows() {
        let ix = index(&[
            "expenses:food:groceries",
            "expenses:fuel",
            "assets:checking",
            "income:salary",
        ]);
        let mut s = Searcher::new();
        s.search(&ix, "ex");
        assert_eq!(s.total, 2);
        s.search(&ix, "ex groc");
        assert_eq!(texts(&ix, &s), ["expenses:food:groceries"]);
        s.search(&ix, "chk");
        assert_eq!(texts(&ix, &s), ["assets:checking"]);
        s.search(&ix, "");
        assert_eq!(s.total, 4);
        assert_eq!(texts(&ix, &s)[0], "income:salary"); // most recent first
    }

    #[test]
    fn parallel_matches_serial() {
        let names: Vec<String> = (0..100_000)
            .map(|i| format!("expenses:item{i}:sub{}", i % 97))
            .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let ix = index(&refs);
        let mut s = Searcher::new();
        s.search(&ix, "item99");
        assert!(s.total >= names.iter().filter(|n| n.contains("item99")).count());
        // all contiguous "item99" matches score alike, so the most recent one ranks first
        assert_eq!(texts(&ix, &s)[0], "expenses:item99999:sub89");
        assert!(texts(&ix, &s).iter().take(50).all(|t| t.contains("item99")));
    }
}
