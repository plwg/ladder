# ladder

A faster way to add transactions to your [hledger](https://hledger.org) journal.

ladder works like `hledger add`: it asks for a date, a description, then
accounts and amounts until the transaction balances, and appends the result to
your journal. As you type, a dropdown fuzzy-matches your past descriptions and
accounts, so most transactions take only a few keystrokes.

![ladder adding a transaction](docs/screenshot.png)

## Key features

- **Fuzzy completion** for descriptions and accounts, ranked by how often and
  how recently you've used them.
- **Templates**: pick a past description and its accounts and amounts become
  the defaults. A new description borrows accounts from a similar past one
  (`Blue Bottle Roasters` borrows from `Blue Bottle Coffee`).
- **Smart account suggestions**: accounts you usually use together rank first,
  for example the card you normally pay for coffee with.
- **Live preview** of the transaction that will be saved. Press Ctrl-Y to save
  it straight away.
- **Automatic balancing and arithmetic**: the last amount fills itself in, and
  amounts can be expressions like `45.80 + 4.20`.
- **Flexible dates** such as `today`, `y`, `-3` or `10/19`.
- **Works with your existing journal**, including `include` files, directives,
  comments and prices.

## Requirements

- [Rust and Cargo](https://www.rust-lang.org/tools/install), version 1.88 or
  newer. The easiest way to get them is `rustup`.
- An hledger journal file. ladder only reads and appends to it, so hledger
  itself doesn't need to be installed.

## Install

```sh
cargo install --git https://github.com/plwg/ladder
```

Or, from a clone of this repository:

```sh
cargo install --path .
```

## Quick start

```sh
ladder                    # uses $LEDGER_FILE, else ~/.hledger.journal
ladder -f my.journal      # use a specific journal
```

Then:

1. Enter a date, or press Enter for the default (today, then the date of the
   transaction you last added).
2. Start typing a description. Press Enter to pick the highlighted match, or
   Alt-Enter to use exactly what you typed.
3. Accept or change each account and amount. Press Ctrl-Y at any point to save
   the transaction shown in the preview.
4. Press Ctrl-D to quit.

## Learn more

See the [full guide](docs/guide.md) for every option, all the key bindings,
how ranking and templates work, and the developer tools.

## License

MIT. See [LICENSE](LICENSE).
