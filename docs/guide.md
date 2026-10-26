# ladder guide

The full reference for ladder. For a quick introduction, see the
[README](../README.md).

It asks the same questions as `hledger add` (date, description, then account and
amount pairs until the transaction balances) and appends the result to your
journal. The difference is a live dropdown under the prompt that fuzzy-matches
your past descriptions and accounts on every keystroke.

![ladder adding a transaction](screenshot.png)

## Install

```sh
cargo install --git https://github.com/plwg/ladder
```

Or from a local checkout:

```sh
cargo install --path .
```

## Usage

```sh
ladder                    # uses $LEDGER_FILE, else ~/.hledger.journal
ladder -f my.journal
ladder -f my.journal 2026-10-19 "Grocery Store"   # pre-fill the first answers
ladder --case title       # show past descriptions in Title Case (see below)
ladder --status cleared   # mark added transactions cleared (*) or pending (!)
```

If stdin isn't a terminal, ladder falls back to plain line-by-line prompts,
so it can be scripted.

### Keys

| Key | Action |
| --- | --- |
| type | fuzzy-filter; space-separated terms all must match (fzf syntax: `^prefix`, `suffix$`, `'exact`, `!not`) |
| ↑ / ↓, Ctrl-P / Ctrl-N | move the selection (↑ past the top selects your typed text) |
| Tab | copy the selected match into the prompt for editing |
| Enter | accept the selection, else your typed text, else the `[default]` |
| Alt-Enter | accept exactly what you typed (use this for a new description) |
| Ctrl-Y | save the transaction shown in the preview box |
| Ctrl-T | take the preview box's transaction, but change this posting's amount first (see [Changing just the amount](#changing-just-the-amount)) |
| Esc | clear the input; on an empty prompt, go back a step (or type `<`) |
| `.` | finish the transaction when it already balances |
| Ctrl-D / Ctrl-C | quit (an unfinished transaction is discarded) |

The usual editing keys also work: Ctrl-A, Ctrl-E, Ctrl-W, Ctrl-U and Ctrl-K.

### Behaviour

- **Ranking** combines match quality with how often and how recently each
  entry was used. The last few distinct entries get an extra boost, so a run of
  transactions against the same account keeps it at the top. A clearly better
  match still wins.
- **Description case**: spellings of a past description that differ only in
  case (`GROCERY STORE`, `Grocery Store`, `grocery store`) appear once, ranked by their combined use, and
  shown in the spelling you've used most. Choosing one saves it as shown and
  uses the most recent spelling's transaction as the template. `--case title`,
  `--case lower` or `--case upper` show every description in that case instead,
  and `--case original` shows each spelling separately.
- **Templates**: choosing a past description uses that transaction's accounts
  and amounts as defaults. A new description borrows the accounts (not the
  amounts) of the past description sharing most of its distinctive words, e.g.
  `Blue Bottle Roasters` takes `Blue Bottle Coffee`'s as the account
  defaults. Common words count for little, so `Main Street Store` borrows
  nothing.
- **Account context**: accounts that usually appear with the ones already in
  the transaction rank higher, so after `expenses:coffee` the card you pay for
  coffee with comes first. When there's no template account and one is likely
  enough, it becomes the prompt's default, so Enter or Ctrl-Y takes it.
- **Preview**: a box below the matches (it keeps its place as the list
  narrows) shows the transaction that would be
  saved: what you've entered so far, the answer Enter would give right now
  (the selected match, what you've typed, or the default), and the template's
  remaining postings, with the last amount balancing. It updates as you type,
  and Ctrl-Y saves it straight away, skipping the remaining prompts. The box is
  hidden when the transaction would still need input, e.g. two postings
  without an amount.
- **Balancing**: later amounts default to whatever balances the transaction. A
  bare number such as `12.5` takes the default's commodity and is written as
  `$12.50`.
- **Arithmetic**: amounts can be expressions using `+ - * /`, parentheses and
  unary minus, e.g. `S$(45.80 + 4.20)/3` or `10+2.5`. The result is rounded to
  the commodity's precision, shown as `= S$16.67`, and written to the journal as
  a plain amount.
- **Dates**: `today`, `y`, `tomorrow`, `-3`, `+2`, `19`, `10/19`,
  `2026-10-19` and `20261019`.
- **Comments**: text after `;` in a description or amount is kept as a comment.
- **Journal support**: `include` files, `account` and `commodity` directives,
  `comment` blocks, virtual postings, and prices (`@`, `@@`) when checking
  balance.
- Transactions you add are completable straight away in the same session.

### Changing just the amount

Often the template is right except for the amount: the same café, the same
card, a different bill. At a description or account prompt, while the preview
box is showing, press **Ctrl-T** instead of Ctrl-Y. ladder keeps the previewed
transaction and asks only for the amount of the posting that prompt is for,
naming its account:

```
Description: Blue Bottle Coffee          ← Ctrl-T here
Amount 1 for expenses:coffee (Enter saves) [$4.00]: 6
2026-10-26 Blue Bottle Coffee
    expenses:coffee    $6.00
    liabilities:amex  $-6.00
Saved to ~/.hledger.journal.
```

- The preview box keeps updating as you type the amount, and the usual amount
  rules apply: arithmetic, a bare number taking the default's commodity, and
  `; comment`.
- Enter saves. The remaining postings come from the template, with the last
  one balancing the new amount.
- At the description prompt it's the first posting's amount. At an account
  prompt it's that account's, so you can change both the account and the
  amount: type or select the account, then press Ctrl-T.
- Check the account in the prompt before typing: if the template lists the
  paying account first (`assets:checking  $-40` above `expenses:food`), the
  amount asked for is the checking one, and needs its minus sign.
- If the transaction can't be balanced without more input (e.g. the new amount
  leaves more than one commodity to balance), ladder carries on with the usual
  prompts instead of saving.
- Esc at the amount prompt goes back as usual.
- The box's bottom edge reads `Ctrl-Y saves this · Ctrl-T edits the amount`
  where Ctrl-T is available.

## Developer tools

The `examples/` folder has a few tools for checking search and ranking against
your own journal.

Time searches against a journal:

```sh
cargo run --release --example bench -- my.journal "groc store"
```

To see how accounts rank for a query:

```sh
cargo run --release --example top -- my.journal "bank"
```

To check the ranking against your own history, replay your last N
transactions and count the keystrokes each set of ranking weights would need,
and how many the account context and borrowed templates save:

```sh
cargo run --release --example replay -- my.journal 50
```

## Limitations

- `include` with wildcards is skipped with a warning.
- Borrowing a template matches words, not meaning: a new payee that shares no
  distinctive word with a past one borrows nothing.
- Amounts you type are saved as written; only bare numbers are reformatted.

## License

MIT. See [LICENSE](../LICENSE).
