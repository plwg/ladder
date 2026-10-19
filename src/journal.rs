//! A fast, tolerant single-pass reader that extracts what `add` needs from a journal:
//! descriptions, account names, usage statistics for ranking, and commodity display
//! styles. Templates (the postings of a description's most recent transaction) are
//! not built while reading; each description remembers where its latest transaction
//! is, and that one transaction is parsed on demand.

use crate::amount::{Styles, scan_amount};
use rustc_hash::{FxHashMap, FxHashSet};
use std::io;
use std::path::{Path, PathBuf};

/// Strip virtual-posting brackets: `(acct)` / `[acct]` → `acct`.
pub fn strip_wrap(raw: &str) -> &str {
    let b = raw.as_bytes();
    match (b.first(), b.last()) {
        (Some(b'('), Some(b')')) | (Some(b'['), Some(b']')) if b.len() > 2 => {
            &raw[1..raw.len() - 1]
        }
        _ => raw,
    }
}

#[derive(Clone, Debug, Default)]
pub struct Usage {
    pub count: u32,
    /// Sequence number of the most recent use (higher is more recent).
    pub last_seq: u32,
}

impl Usage {
    fn bump(&mut self, seq: u32) {
        self.count += 1;
        self.last_seq = seq;
    }
}

/// Where a transaction's header line is: `sources[file][offset..]`.
#[derive(Clone, Copy, Debug, Default)]
struct Loc {
    file: u32,
    offset: usize,
}

#[derive(Clone, Debug)]
pub struct Description {
    pub text: String,
    pub usage: Usage,
    last: Loc,
}

#[derive(Clone, Debug)]
pub struct Account {
    pub name: String,
    pub usage: Usage,
}

/// A posting of a previous transaction, as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemplatePosting {
    pub account: String,
    pub amount: Option<String>,
}

#[derive(Default, Debug, Clone)]
pub struct Journal {
    pub descriptions: Vec<Description>,
    desc_ix: FxHashMap<String, u32>,
    pub accounts: Vec<Account>,
    acct_ix: FxHashMap<String, u32>,
    pub styles: Styles,
    pub transactions: usize,
    pub warnings: Vec<String>,
    /// Text of every file read (and of transactions added this session).
    sources: Vec<String>,
    seq: u32,
}

/// Split a posting/directive body at the first double space or tab.
fn split_field(s: &str) -> (&str, &str) {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\t' || (b[i] == b' ' && b.get(i + 1) == Some(&b' ')) {
            return (&s[..i], &s[i..]);
        }
        i += 1;
    }
    (s, "")
}

fn strip_comment(s: &str) -> &str {
    s.find(';').map_or(s, |i| &s[..i]).trim()
}

/// Split a posting line into (account as written, amount); `None` for comment lines.
fn split_posting(line: &str) -> Option<(&str, Option<&str>)> {
    let mut t = line.trim_start();
    if t.is_empty() || t.starts_with(';') {
        return None;
    }
    if let Some(r) = t.strip_prefix(['*', '!']) {
        t = r.trim_start();
    }
    let (account, rest) = split_field(t);
    let account = account.trim_end();
    if account.is_empty() {
        return None;
    }
    let amount = strip_comment(rest);
    Some((account, (!amount.is_empty()).then_some(amount)))
}

/// Byte offsets splitting a large `text` into one chunk per core, each starting at a
/// top-level (unindented) line, so chunks can be parsed independently.
fn chunk_bounds(text: &str, min_chunk: usize) -> Vec<usize> {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let n = threads.min(text.len() / min_chunk).max(1);
    let b = text.as_bytes();
    let mut bounds = vec![0];
    for k in 1..n {
        let mut p = text.len() * k / n;
        while p < b.len() {
            match b[p..].iter().position(|&c| c == b'\n') {
                Some(i) => p += i + 1,
                None => p = b.len(),
            }
            if p < b.len() && !matches!(b[p], b' ' | b'\t' | b'\n' | b'\r') {
                break;
            }
        }
        if p < b.len() && p > bounds[bounds.len() - 1] {
            bounds.push(p);
        }
    }
    bounds.push(text.len());
    bounds
}

pub fn expand_tilde(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map_or_else(|| PathBuf::from(p), |h| PathBuf::from(h).join(rest)),
        None => PathBuf::from(p),
    }
}

impl Journal {
    /// Load a journal and its includes. A missing top-level file yields an empty journal.
    pub fn load(path: &Path) -> io::Result<Journal> {
        let mut j = Journal::default();
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let mut seen = FxHashSet::default();
                seen.insert(path.canonicalize().unwrap_or_else(|_| path.to_owned()));
                j.parse_source(text, path.parent().unwrap_or(Path::new(".")), &mut seen);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(j)
    }

    pub fn parse_str(text: &str) -> Journal {
        let mut j = Journal::default();
        j.parse_source(text.to_owned(), Path::new("."), &mut FxHashSet::default());
        j
    }

    /// Record a transaction (journal text) entered this session so it completes immediately.
    pub fn record(&mut self, transaction: &str) {
        self.parse_source(
            transaction.to_owned(),
            Path::new("."),
            &mut FxHashSet::default(),
        );
    }

    pub fn description(&self, text: &str) -> Option<&Description> {
        self.desc_ix
            .get(text)
            .map(|&i| &self.descriptions[i as usize])
    }

    pub fn account(&self, name: &str) -> Option<&Account> {
        self.acct_ix.get(name).map(|&i| &self.accounts[i as usize])
    }

    /// Postings of the most recent transaction with this description.
    pub fn template(&self, description: &str) -> Vec<TemplatePosting> {
        let Some(d) = self.description(description) else {
            return Vec::new();
        };
        let text = &self.sources[d.last.file as usize][d.last.offset..];
        text.lines()
            .skip(1)
            .take_while(|l| l.starts_with([' ', '\t']))
            .filter_map(split_posting)
            .map(|(account, amount)| TemplatePosting {
                account: account.to_owned(),
                amount: amount.map(str::to_owned),
            })
            .collect()
    }

    fn account_id(&mut self, name: &str) -> u32 {
        if let Some(&i) = self.acct_ix.get(name) {
            return i;
        }
        let i = self.accounts.len() as u32;
        self.accounts.push(Account {
            name: name.to_owned(),
            usage: Usage::default(),
        });
        self.acct_ix.insert(name.to_owned(), i);
        i
    }

    fn description_id(&mut self, text: &str) -> u32 {
        if let Some(&i) = self.desc_ix.get(text) {
            return i;
        }
        let i = self.descriptions.len() as u32;
        self.descriptions.push(Description {
            text: text.to_owned(),
            usage: Usage::default(),
            last: Loc::default(),
        });
        self.desc_ix.insert(text.to_owned(), i);
        i
    }

    /// Parse a file's text (large files in parallel chunks) and keep it for template lookups.
    fn parse_source(&mut self, text: String, dir: &Path, seen: &mut FxHashSet<PathBuf>) {
        self.parse_source_chunked(text, dir, seen, 1 << 19);
    }

    fn parse_source_chunked(
        &mut self,
        text: String,
        dir: &Path,
        seen: &mut FxHashSet<PathBuf>,
        min_chunk: usize,
    ) {
        let file = self.sources.len() as u32;
        self.sources.push(String::new());
        let bounds = chunk_bounds(&text, min_chunk);
        if bounds.len() <= 2 {
            self.parse(file, &text, 0, false, dir, seen);
        } else {
            let parts: Vec<(Journal, bool)> = std::thread::scope(|s| {
                let handles: Vec<_> = bounds
                    .windows(2)
                    .map(|w| {
                        let (start, chunk) = (w[0], &text[w[0]..w[1]]);
                        let mut seen = seen.clone();
                        s.spawn(move || {
                            let mut j = Journal {
                                sources: vec![String::new()],
                                ..Journal::default()
                            };
                            let open_comment = j.parse(0, chunk, start, false, dir, &mut seen);
                            (j, open_comment)
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("parser thread panicked"))
                    .collect()
            });
            let mut in_comment = false;
            let mut spent = Vec::with_capacity(parts.len());
            for (w, (part, open_comment)) in bounds.windows(2).zip(parts) {
                if in_comment {
                    // The chunk began inside a comment block its worker didn't know about.
                    in_comment = self.parse(file, &text[w[0]..w[1]], w[0], true, dir, seen);
                } else {
                    in_comment = open_comment;
                    spent.push(self.merge(part, file));
                }
            }
            // Freeing the workers' maps is surprisingly costly; do it off the critical path.
            std::thread::spawn(move || drop(spent));
        }
        self.sources[file as usize] = text;
    }

    /// Fold in `part`, parsed by a worker from text that follows everything parsed so far.
    /// Its file 0 is `file`. Returns what's left of `part` for the caller to free.
    fn merge(&mut self, mut part: Journal, file: u32) -> Journal {
        let seq0 = self.seq;
        let src0 = self.sources.len() as u32 - 1;
        let remap = |loc: Loc| Loc {
            file: if loc.file == 0 { file } else { src0 + loc.file },
            offset: loc.offset,
        };
        self.sources.extend(part.sources.drain(1..));
        for d in part.descriptions.drain(..) {
            let id = match self.desc_ix.get(d.text.as_str()) {
                Some(&id) => id,
                None => {
                    let id = self.descriptions.len() as u32;
                    self.desc_ix.insert(d.text.clone(), id);
                    self.descriptions.push(Description {
                        text: d.text,
                        usage: Usage::default(),
                        last: Loc::default(),
                    });
                    id
                }
            };
            let e = &mut self.descriptions[id as usize];
            e.usage.count += d.usage.count;
            e.usage.last_seq = seq0 + d.usage.last_seq;
            e.last = remap(d.last);
        }
        for a in part.accounts.drain(..) {
            let id = self.account_id(&a.name);
            let u = &mut self.accounts[id as usize].usage;
            u.count += a.usage.count;
            if a.usage.count > 0 {
                u.last_seq = seq0 + a.usage.last_seq;
            }
        }
        self.styles.merge(&part.styles);
        self.transactions += part.transactions;
        self.warnings.append(&mut part.warnings);
        self.seq += part.seq;
        part
    }

    /// Parse `text`, which starts at byte `base` of source `file`. Returns whether it
    /// ended inside an unterminated `comment` block.
    fn parse(
        &mut self,
        file: u32,
        text: &str,
        base: usize,
        in_comment: bool,
        dir: &Path,
        seen: &mut FxHashSet<PathBuf>,
    ) -> bool {
        let mut in_txn = false;
        // Inside a `commodity` directive, whose indented lines may hold `format AMOUNT`.
        let mut in_commodity = false;
        let mut in_comment_block = in_comment;
        let mut offset = base;
        for raw in text.split_inclusive('\n') {
            let line_start = offset;
            offset += raw.len();
            let line = raw.trim_end_matches(['\n', '\r']);
            if in_comment_block {
                in_comment_block = line.trim_end() != "end comment";
                continue;
            }
            match line.as_bytes().first() {
                None => (in_txn, in_commodity) = (false, false),
                Some(b' ' | b'\t') => {
                    if in_txn {
                        self.parse_posting(line);
                    } else if in_commodity
                        && let Some(fmt) = line.trim_start().strip_prefix("format")
                        && let Some((_, commodity, style, _)) =
                            scan_amount(strip_comment(fmt), None)
                    {
                        self.styles.declare(commodity, &style);
                    }
                }
                Some(b'0'..=b'9') => {
                    self.parse_header(
                        line,
                        Loc {
                            file,
                            offset: line_start,
                        },
                    );
                    in_txn = true;
                }
                Some(_) => {
                    in_txn = false;
                    in_commodity = line.starts_with("commodity");
                    let (word, rest) = line.split_once([' ', '\t']).unwrap_or((line, ""));
                    match word {
                        "account" => {
                            let name = split_field(rest.trim_start()).0.trim();
                            if !name.is_empty() {
                                self.account_id(name);
                            }
                        }
                        "commodity" | "D" => {
                            if let Some((_, commodity, style, _)) =
                                scan_amount(strip_comment(rest), None)
                            {
                                self.styles.declare(commodity, &style);
                            }
                        }
                        "include" => self.include(strip_comment(rest), dir, seen),
                        "comment" => in_comment_block = rest.trim().is_empty(),
                        _ => {}
                    }
                }
            }
        }
        in_comment_block
    }

    fn include(&mut self, target: &str, dir: &Path, seen: &mut FxHashSet<PathBuf>) {
        if target.contains(['*', '?', '[']) {
            self.warnings
                .push(format!("include globs are not supported: {target}"));
            return;
        }
        let path = expand_tilde(target);
        let path = if path.is_absolute() {
            path
        } else {
            dir.join(path)
        };
        let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
        if !seen.insert(canon) {
            return;
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => self.parse_source(text, path.parent().unwrap_or(dir), seen),
            Err(e) => self
                .warnings
                .push(format!("can't include {}: {e}", path.display())),
        }
    }

    fn parse_header(&mut self, line: &str, loc: Loc) {
        self.seq += 1;
        self.transactions += 1;
        let mut rest = line
            .split_once([' ', '\t'])
            .map_or("", |(_, r)| r)
            .trim_start();
        if let Some(r) = rest.strip_prefix(['*', '!']) {
            rest = r.trim_start();
        }
        if rest.starts_with('(')
            && let Some(close) = rest.find(')')
        {
            rest = rest[close + 1..].trim_start();
        }
        let d = self.description_id(strip_comment(rest));
        let entry = &mut self.descriptions[d as usize];
        entry.usage.bump(self.seq);
        entry.last = loc;
    }

    fn parse_posting(&mut self, line: &str) {
        let Some((account, amount)) = split_posting(line) else {
            return;
        };
        if let Some((_, commodity, style, _)) = amount.and_then(|a| scan_amount(a, None)) {
            self.styles.observe(commodity, &style);
        }
        let a = self.account_id(strip_wrap(account));
        self.accounts[a as usize].usage.bump(self.seq);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
; a comment
account assets:checking   ; with comment
commodity $1,000.00

2026-01-01 * (123) Grocery Store  ; note
    expenses:food:groceries        $45.10
    assets:checking

2026-01-02 Rent
    expenses:rent  $1,200.00
    (budget:rent)  $-1,200.00
    assets:checking

comment
2026-01-03 Hidden
    a  1
end comment

~ monthly
    expenses:rent  $1
    assets:checking

2026/01/05 Grocery Store
    ! expenses:food:groceries        $12
    ; a posting comment
    assets:savings  ; transfer
";

    fn tp(account: &str, amount: Option<&str>) -> TemplatePosting {
        TemplatePosting {
            account: account.into(),
            amount: amount.map(Into::into),
        }
    }

    #[test]
    fn parses_sample() {
        let j = Journal::parse_str(SAMPLE);
        assert_eq!(j.transactions, 3);
        let names: Vec<&str> = j.descriptions.iter().map(|d| d.text.as_str()).collect();
        assert_eq!(names, ["Grocery Store", "Rent"]);
        assert_eq!(j.description("Grocery Store").unwrap().usage.count, 2);
        assert_eq!(
            j.template("Grocery Store"),
            [
                tp("expenses:food:groceries", Some("$12")),
                tp("assets:savings", None)
            ]
        );
        assert_eq!(
            j.template("Rent")[1],
            tp("(budget:rent)", Some("$-1,200.00"))
        );
        assert_eq!(j.account("budget:rent").unwrap().usage.count, 1);
        assert_eq!(j.account("assets:checking").unwrap().usage.count, 2);
        assert!(j.account("a").is_none());
        let st = j.styles.get("$").unwrap();
        assert_eq!((st.precision, st.thousands, st.left), (2, Some(','), true));
    }

    #[test]
    fn chunked_parse_matches_sequential() {
        let mut text = String::new();
        for i in 0..40 {
            text.push_str(
                &SAMPLE
                    .replace("Grocery Store", &format!("Store {}", i % 7))
                    .replace("Rent", &format!("Rent {i}")),
            );
            text.push('\n');
        }
        let seq = Journal::parse_str(&text);
        let mut par = Journal::default();
        par.parse_source_chunked(text.clone(), Path::new("."), &mut FxHashSet::default(), 64);
        assert!(chunk_bounds(&text, 64).len() > 3);
        let summary = |j: &Journal| {
            let mut d: Vec<_> = j
                .descriptions
                .iter()
                .map(|d| {
                    (
                        d.text.clone(),
                        d.usage.count,
                        d.usage.last_seq,
                        j.template(&d.text),
                    )
                })
                .collect();
            let mut a: Vec<_> = j
                .accounts
                .iter()
                .map(|a| (a.name.clone(), a.usage.count, a.usage.last_seq))
                .collect();
            d.sort_by(|x, y| x.0.cmp(&y.0));
            a.sort();
            (d, a, j.transactions, j.styles.get("$").cloned())
        };
        assert_eq!(seq.transactions, 120);
        assert_eq!(summary(&seq), summary(&par));

        // A comment block spanning most chunks: workers that start inside it misparse,
        // and must be redone with the block state carried over.
        let hidden: String = (0..500)
            .map(|i| format!("2026-01-01 Hidden {i}\n    x:y  $1\n    z\n\n"))
            .collect();
        let text = format!("{SAMPLE}\ncomment\n{hidden}end comment\n\n{SAMPLE}");
        let mut par = Journal::default();
        par.parse_source_chunked(text.clone(), Path::new("."), &mut FxHashSet::default(), 64);
        assert_eq!(summary(&Journal::parse_str(&text)), summary(&par));
        assert_eq!(par.transactions, 6);
        assert!(par.account("x:y").is_none());
    }

    #[test]
    fn declared_style_beats_inferred_precision() {
        let j = Journal::parse_str(
            "D S$1,000.00\n\n2026-01-01 open\n    assets:bank  S$-1,234.56789\n    equity\n\n\
             commodity EUR\n    format 1.000,0 EUR\n\n2026-01-02 x\n    a  1,23456 EUR\n    b\n",
        );
        let sgd = j.styles.get("S$").unwrap();
        assert_eq!((sgd.precision, sgd.thousands), (2, Some(',')));
        let eur = j.styles.get("EUR").unwrap();
        assert_eq!((eur.precision, eur.decimal_mark), (1, ','));
        let amt = crate::amount::Amount {
            qty: crate::amount::Decimal::new(1234, 2),
            commodity: "S$".into(),
        };
        assert_eq!(j.styles.format(&amt), "S$12.34");
    }

    #[test]
    fn records_session_transactions() {
        let mut j = Journal::parse_str(SAMPLE);
        j.record("2026-02-01 Rent\n    expenses:rent  $900\n    assets:cash  $-900\n");
        assert_eq!(j.description("Rent").unwrap().usage.count, 2);
        assert_eq!(
            j.template("Rent"),
            [
                tp("expenses:rent", Some("$900")),
                tp("assets:cash", Some("$-900"))
            ]
        );
        assert_eq!(
            j.account("assets:cash").unwrap().usage.last_seq,
            j.description("Rent").unwrap().usage.last_seq
        );
    }
}
