//! Load a journal and time fuzzy searches: `cargo run --release --example bench -- FILE [QUERY...]`
use ladder::app::Loaded;
use ladder::fuzzy::Searcher;
use ladder::journal::Journal;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: bench FILE [QUERY...]");
    let t = Instant::now();
    let journal = Journal::load(path.as_ref()).expect("load");
    let parsed = t.elapsed();
    let data = Loaded::new(journal);
    println!(
        "{} txns, {} descriptions, {} accounts: parse {parsed:?}, index {:?}",
        data.journal.transactions,
        data.descriptions.len(),
        data.accounts.len(),
        t.elapsed() - parsed
    );
    let queries: Vec<String> = args.collect();
    let queries = if queries.is_empty() {
        vec!["groc store".to_owned()]
    } else {
        queries
    };
    for q in queries {
        let mut s = Searcher::new();
        let mut line = format!("{q:?}:");
        // Simulate typing the query one char at a time, as the UI does.
        for end in q.char_indices().map(|(i, c)| i + c.len_utf8()) {
            let t = Instant::now();
            s.search(&data.descriptions, &q[..end]);
            line += &format!(" {:.2}ms/{}", t.elapsed().as_secs_f64() * 1e3, s.total);
        }
        println!("{line}");
    }
}
