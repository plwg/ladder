//! Print the top-ranked accounts for a query: `cargo run --example top -- FILE [QUERY]`
use ladder::app::Loaded;
use ladder::fuzzy::Searcher;
use ladder::journal::Journal;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: top FILE [QUERY]");
    let query = args.next().unwrap_or_default();
    let data = Loaded::new(Journal::load(path.as_ref()).expect("load"));
    let mut s = Searcher::new();
    s.search(&data.accounts, &query);
    for &id in s.results.iter().take(8) {
        let item = data.accounts.get(id);
        println!("{:>3}  {}", item.bonus, item.text);
    }
}
