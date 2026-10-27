use ladder::app::{App, Loaded};
use ladder::case::Case;
use ladder::journal::expand_tilde;
use ladder::status::Status;
use ladder::ui::{LineUi, TermUi};
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
ladder - add transactions to an hledger journal, with fuzzy completion as you type

Usage: ladder [-f FILE] [ANSWERS...]

Like `hledger add`: prompts for date, description, then account/amount pairs
until the transaction balances. Optional ANSWERS pre-fill the first prompts.

Options:
  -f, --file FILE   journal to append to (default: $LEDGER_FILE or ~/.hledger.journal)
      --case CASE   how to show past descriptions that differ only in case, which are
                    merged: usual (default; the most used spelling), title, lower,
                    upper, or original (don't merge)
      --status S    status to mark added transactions with: unmarked (default),
                    pending (!) or cleared (*)
  -h, --help        show this help
  -V, --version     show version

Keys:
  type              fuzzy-filter (space-separated terms; fzf syntax: ^prefix suffix$ 'exact !not)
  Up/Down, C-p/C-n  move selection (Up past the top selects your typed text)
  Tab               insert the selected candidate for editing
  Enter             accept selection, else typed text, else the [default]
  Alt-Enter         accept the typed text exactly as typed
  C-y               save the transaction shown in the preview box
  C-t               the same, but first change this posting's amount
  Esc               clear input; on an empty prompt go back a step (also: < Enter)
  C-d / C-c         quit (the unfinished transaction is discarded)
";

fn default_journal() -> PathBuf {
    match std::env::var("LEDGER_FILE") {
        Ok(f) if !f.is_empty() => expand_tilde(&f),
        _ => expand_tilde("~/.hledger.journal"),
    }
}

fn main() -> ExitCode {
    let mut file = None;
    let mut case = Case::default();
    let mut status = Status::default();
    let mut answers = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "-V" | "--version" => {
                println!("ladder {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "-f" | "--file" => match args.next() {
                Some(f) => file = Some(expand_tilde(&f)),
                None => {
                    eprintln!("ladder: {a} needs a file argument");
                    return ExitCode::FAILURE;
                }
            },
            "--case" => match args.next().map(|c| c.parse()) {
                Some(Ok(c)) => case = c,
                Some(Err(e)) => {
                    eprintln!("ladder: {e}");
                    return ExitCode::FAILURE;
                }
                None => {
                    eprintln!("ladder: {a} needs an argument");
                    return ExitCode::FAILURE;
                }
            },
            "--status" => match args.next().map(|s| s.parse()) {
                Some(Ok(s)) => status = s,
                Some(Err(e)) => {
                    eprintln!("ladder: {e}");
                    return ExitCode::FAILURE;
                }
                None => {
                    eprintln!("ladder: {a} needs an argument");
                    return ExitCode::FAILURE;
                }
            },
            "--" => answers.extend(args.by_ref()),
            _ => {
                if let Some(f) = a.strip_prefix("--file=") {
                    file = Some(expand_tilde(f));
                } else if let Some(c) = a.strip_prefix("--case=") {
                    match c.parse() {
                        Ok(c) => case = c,
                        Err(e) => {
                            eprintln!("ladder: {e}");
                            return ExitCode::FAILURE;
                        }
                    }
                } else if let Some(s) = a.strip_prefix("--status=") {
                    match s.parse() {
                        Ok(s) => status = s,
                        Err(e) => {
                            eprintln!("ladder: {e}");
                            return ExitCode::FAILURE;
                        }
                    }
                } else {
                    answers.push(a);
                }
            }
        }
    }
    let path = file.unwrap_or_else(default_journal);
    // Parse the journal and build the search indexes while the user types the date.
    let loader = Loaded::spawn(path.clone(), case);
    let today = chrono::Local::now().date_naive();

    let result = if io::stdin().is_terminal() && io::stdout().is_terminal() {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = crossterm::terminal::disable_raw_mode();
            default_hook(info);
        }));
        TermUi::new().and_then(|ui| {
            App::new(ui, path, loader, today, answers)
                .status(status)
                .run()
        })
    } else {
        App::new(
            LineUi::new(io::stdin().lock()),
            path,
            loader,
            today,
            answers,
        )
        .status(status)
        .run()
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ladder: {e}");
            ExitCode::FAILURE
        }
    }
}
