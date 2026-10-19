//! Replay a journal's last N transactions as if typing them into ladder, and count
//! the keystrokes each ranking configuration needs to select every description and
//! account. Lower is better.
//!
//! `cargo run --release --example replay -- FILE [N]`
//!
//! Typing models (the query for a target after k keystrokes):
//!   full: the first k chars of the whole text ("expenses:fo")
//!   leaf: the first k chars of its last segment ("fo" for expenses:food)
//! Cost of a target = min over k of k + Down presses to reach it (0 if a template
//! default already matches). New descriptions/accounts can't be completed; they're
//! counted separately and don't depend on ranking.

use ladder::app::Loaded;
use ladder::fuzzy::{Index, Searcher, Weights};
use ladder::journal::{Journal, strip_wrap};

#[derive(Clone, Copy)]
enum Model {
    Full,
    Leaf,
}

fn typed(text: &str, model: Model) -> &str {
    match model {
        Model::Full => text,
        Model::Leaf => text.rsplit(':').next().unwrap_or(text),
    }
}

/// Keystrokes to get `target` selected, or None if it isn't in the index.
fn cost(index: &Index, s: &mut Searcher, target: &str, model: Model) -> Option<usize> {
    let id = index.id(target)?;
    let q = typed(target, model);
    let mut best = usize::MAX;
    let ends = std::iter::once(0).chain(q.char_indices().map(|(i, c)| i + c.len_utf8()));
    for (k, end) in ends.enumerate() {
        if k >= best {
            break;
        }
        s.search(index, &q[..end]);
        if let Some(pos) = s.results.iter().position(|&r| r == id) {
            // An empty query selects nothing, so even the top row needs one Down.
            best = best.min(k + pos + usize::from(k == 0));
        }
    }
    Some(best)
}

#[derive(Default, Clone, Copy)]
struct Tally {
    desc: usize,
    acct: usize,
    desc_top1_empty: usize,
    acct_top1_empty: usize,
}

struct Stats {
    txns: usize,
    new_desc: usize,
    new_acct: usize,
    by_default: usize,
    ranked_accts: usize,
}

fn replay(prefix: &Journal, held_out: &[&str], weights: &Weights, model: Model) -> (Tally, Stats) {
    let mut data = Loaded::with_weights(prefix.clone(), weights.clone());
    let mut s = Searcher::new();
    let mut t = Tally::default();
    let mut st = Stats {
        txns: held_out.len(),
        new_desc: 0,
        new_acct: 0,
        by_default: 0,
        ranked_accts: 0,
    };
    for txn in held_out {
        let parsed = Journal::parse_str(txn);
        let Some(desc) = parsed.descriptions.first().map(|d| d.text.clone()) else {
            continue;
        };
        match cost(&data.descriptions, &mut s, &desc, model) {
            Some(c) => {
                t.desc += c;
                t.desc_top1_empty += usize::from(c == 1);
            }
            None => st.new_desc += 1,
        }
        let template = data.journal.template(&desc);
        for (i, p) in parsed.template(&desc).iter().enumerate() {
            if template.get(i).is_some_and(|d| d.account == p.account) {
                st.by_default += 1;
                continue;
            }
            match cost(&data.accounts, &mut s, strip_wrap(&p.account), model) {
                Some(c) => {
                    st.ranked_accts += 1;
                    t.acct += c;
                    t.acct_top1_empty += usize::from(c == 1);
                }
                None => st.new_acct += 1,
            }
        }
        data.record(txn);
    }
    (t, st)
}

/// Split into (text before the last `n` transactions, those transactions).
fn split(text: &str, n: usize) -> (&str, Vec<&str>) {
    let mut starts = Vec::new();
    let mut off = 0;
    for line in text.split_inclusive('\n') {
        if line.starts_with(|c: char| c.is_ascii_digit()) {
            starts.push(off);
        }
        off += line.len();
    }
    let first = starts.len().saturating_sub(n);
    let held: Vec<&str> = starts[first..]
        .iter()
        .enumerate()
        .map(|(i, &a)| &text[a..starts.get(first + i + 1).copied().unwrap_or(text.len())])
        .collect();
    (
        &text[..starts.get(first).copied().unwrap_or(text.len())],
        held,
    )
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: replay FILE [N]");
    let n: usize = args.next().map_or(50, |a| a.parse().expect("N"));
    let text = std::fs::read_to_string(&path).expect("read");
    let (before, held_out) = split(&text, n);
    let prefix = Journal::parse_str(before);

    let base = [12u32, 10, 8, 6, 4, 2];
    let mut configs = Vec::new();
    for freq_step in [0, 1, 2, 4] {
        for recency in [0.0, 5.0, 10.0, 20.0, 40.0] {
            for scale in [0, 1, 2, 3] {
                let recent_boost = base.iter().map(|b| b * scale).collect();
                configs.push(Weights {
                    freq_step,
                    recency,
                    recent_boost,
                });
            }
        }
    }
    configs.push(Weights {
        freq_step: 0,
        recency: 0.0,
        recent_boost: vec![],
    }); // match quality only

    let current = Weights::default();
    for (name, model) in [("full-prefix", Model::Full), ("leaf-prefix", Model::Leaf)] {
        let mut rows: Vec<(usize, &Weights, Tally)> = configs
            .iter()
            .map(|w| {
                let (t, _) = replay(&prefix, &held_out, w, model);
                (t.desc + t.acct, w, t)
            })
            .collect();
        rows.sort_by_key(|r| r.0);
        let (_, st) = replay(&prefix, &held_out, &current, model);
        println!(
            "\n== typing model: {name} ==  {} txns; {} new descriptions, {} new accounts (not rankable); \
             {} accounts taken from template default; {} accounts ranked",
            st.txns, st.new_desc, st.new_acct, st.by_default, st.ranked_accts
        );
        println!(
            "{:>5} {:>5} {:>5} {:>6} {:>6}  {:<5} {:<7} boost",
            "total", "desc", "acct", "d@1", "a@1", "freq", "recency"
        );
        let show = |r: &(usize, &Weights, Tally), mark: &str| {
            let (total, w, t) = r;
            println!(
                "{total:>5} {:>5} {:>5} {:>6} {:>6}  {:<5} {:<7} {:?}{mark}",
                t.desc,
                t.acct,
                t.desc_top1_empty,
                t.acct_top1_empty,
                w.freq_step,
                w.recency,
                w.recent_boost
            );
        };
        for r in rows.iter().take(10) {
            show(r, if *r.1 == current { "   <- current" } else { "" });
        }
        if let Some(pos) = rows.iter().position(|r| *r.1 == current)
            && pos >= 10
        {
            println!("  ...");
            show(
                &rows[pos],
                &format!("   <- current (#{} of {})", pos + 1, rows.len()),
            );
        }
        println!("worst: {}", rows.last().map_or(0, |r| r.0));
        // Descriptions and accounts are separate indexes, so each can use its own weights.
        for (kind, key) in [("descriptions", 0), ("accounts", 1)] {
            let k = |t: &Tally| if key == 0 { t.desc } else { t.acct };
            let mut by: Vec<_> = rows.iter().map(|r| (k(&r.2), r.1)).collect();
            by.sort_by_key(|r| r.0);
            let cur = by.iter().find(|r| *r.1 == current).map_or(0, |r| r.0);
            let best: Vec<String> = by
                .iter()
                .take_while(|r| r.0 == by[0].0)
                .take(4)
                .map(|r| {
                    format!(
                        "freq {} recency {} boost x{}",
                        r.1.freq_step,
                        r.1.recency,
                        r.1.recent_boost.first().map_or(0, |b| b / 12)
                    )
                })
                .collect();
            println!(
                "  best for {kind}: {} keystrokes (current {cur}) with {}",
                by[0].0,
                best.join(" | ")
            );
        }
    }
}
