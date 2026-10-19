//! The `add` conversation: date → description → (account, amount)* → confirm → append.

use crate::amount::{Amount, Mixed, parse_posting_amount};
use crate::dates::parse_smart_date;
use crate::fuzzy::Index;
use crate::journal::{Journal, TemplatePosting, strip_wrap};
use crate::ui::{Answer, Spec, Ui};
use chrono::NaiveDate;
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use unicode_width::UnicodeWidthStr;

/// Everything derived from the journal; built off the main thread at startup.
pub struct Loaded {
    pub journal: Journal,
    pub descriptions: Index,
    pub accounts: Index,
}

impl Loaded {
    pub fn new(journal: Journal) -> Loaded {
        let descriptions = Index::build(
            journal
                .descriptions
                .iter()
                .map(|d| (d.text.clone(), d.usage.clone()))
                .collect(),
        );
        let accounts = Index::build(
            journal
                .accounts
                .iter()
                .map(|a| (a.name.clone(), a.usage.clone()))
                .collect(),
        );
        Loaded {
            journal,
            descriptions,
            accounts,
        }
    }

    pub fn spawn(path: PathBuf) -> JoinHandle<io::Result<Loaded>> {
        std::thread::spawn(move || Journal::load(&path).map(Loaded::new))
    }
}

struct Posting {
    account: String,
    amount: String,
    comment: Option<String>,
    weight: Amount,
}

#[derive(Default)]
struct Draft {
    date: Option<NaiveDate>,
    description: Option<String>,
    comment: Option<String>,
    template: Vec<TemplatePosting>,
    postings: Vec<Posting>,
    pending_account: Option<String>,
    finished: bool,
}

impl Draft {
    fn is_empty(&self) -> bool {
        self.date.is_none()
    }
}

/// Split `text ; comment` into its parts.
fn split_comment(s: &str) -> (String, Option<String>) {
    match s.split_once(';') {
        Some((a, c)) => (
            a.trim().to_owned(),
            Some(c.trim().to_owned()).filter(|c| !c.is_empty()),
        ),
        None => (s.trim().to_owned(), None),
    }
}

/// Prompting plus the per-prompt state that survives between prompts.
struct Prompter<U: Ui> {
    ui: U,
    /// Answers given on the command line, consumed before prompting interactively.
    args: VecDeque<String>,
    /// Initial input for the next prompt (the previous answer after going back).
    prefill: String,
    error: Option<String>,
}

impl<U: Ui> Prompter<U> {
    fn ask(&mut self, mut spec: Spec) -> io::Result<Answer> {
        spec.initial = std::mem::take(&mut self.prefill);
        spec.error = self.error.take();
        match self.args.pop_front() {
            Some(a) => {
                self.ui.echo(&spec, &a)?;
                Ok(match a.trim() {
                    "" => Answer::Text(spec.default.unwrap_or_default()),
                    t => Answer::Text(t.to_owned()),
                })
            }
            None => self.ui.ask(&spec),
        }
    }

    fn say(&mut self, text: &str) -> io::Result<()> {
        self.ui.say(text)
    }

    /// Re-ask the same prompt, showing `error` and keeping what was typed.
    fn retry(&mut self, error: String, input: &str) {
        self.error = Some(error);
        self.prefill = input.to_owned();
    }
}

pub struct App<U: Ui> {
    p: Prompter<U>,
    path: PathBuf,
    loader: Option<JoinHandle<io::Result<Loaded>>>,
    data: Option<Loaded>,
    today: NaiveDate,
    last_date: Option<NaiveDate>,
    draft: Draft,
}

impl<U: Ui> App<U> {
    pub fn new(
        ui: U,
        path: PathBuf,
        loader: JoinHandle<io::Result<Loaded>>,
        today: NaiveDate,
        args: Vec<String>,
    ) -> Self {
        App {
            p: Prompter {
                ui,
                args: args.into(),
                prefill: String::new(),
                error: None,
            },
            path,
            loader: Some(loader),
            data: None,
            today,
            last_date: None,
            draft: Draft::default(),
        }
    }

    /// Wait for the background load (normally finished while the date was being entered).
    fn ensure_loaded(&mut self) -> io::Result<()> {
        if let Some(h) = self.loader.take() {
            let loaded = h
                .join()
                .map_err(|_| io::Error::other("journal loader panicked"))??;
            for w in &loaded.journal.warnings {
                self.p.say(&format!("warning: {w}"))?;
            }
            self.data = Some(loaded);
        }
        Ok(())
    }

    fn data(&self) -> &Loaded {
        self.data.as_ref().expect("journal loaded before use")
    }

    pub fn run(&mut self) -> io::Result<()> {
        self.p.say(&format!(
            "Adding transactions to journal file {}\n\
             Type to fuzzy-search; ↑/↓ select, Tab inserts, Enter accepts, Alt-Enter takes input as typed.\n\
             Esc on an empty prompt (or <) goes back a step; Ctrl-D quits.\n",
            self.path.display()
        ))?;
        loop {
            let answer = if self.draft.date.is_none() {
                self.ask_date()?
            } else if self.draft.description.is_none() {
                self.ask_description()?
            } else if self.draft.finished {
                self.ask_confirm()?
            } else if self.draft.pending_account.is_none() {
                self.ask_account()?
            } else {
                self.ask_amount()?
            };
            match answer {
                Answer::Quit => {
                    if !self.draft.is_empty() {
                        self.p.say("Discarded unfinished transaction.")?;
                    }
                    return Ok(());
                }
                Answer::Back => self.back(),
                Answer::Text(_) => {}
            }
        }
    }

    fn back(&mut self) {
        let d = &mut self.draft;
        if d.finished {
            d.finished = false;
        } else if let Some(acct) = d.pending_account.take() {
            self.p.prefill = acct;
        } else if let Some(p) = d.postings.pop() {
            d.pending_account = Some(p.account);
            self.p.prefill = match p.comment {
                Some(c) => format!("{} ; {c}", p.amount),
                None => p.amount,
            };
        } else if let Some(desc) = d.description.take() {
            d.template.clear();
            self.p.prefill = match d.comment.take() {
                Some(c) => format!("{desc} ; {c}"),
                None => desc,
            };
        } else if let Some(date) = d.date.take() {
            self.p.prefill = date.to_string();
        }
    }

    fn ask_date(&mut self) -> io::Result<Answer> {
        let default = self.last_date.unwrap_or(self.today);
        let mut spec = Spec::new("Date");
        spec.default = Some(default.to_string());
        let answer = self.p.ask(spec)?;
        if let Answer::Text(t) = &answer {
            match parse_smart_date(t, self.today, default) {
                Some(d) => self.draft.date = Some(d),
                None => {
                    let e = format!(
                        "Can't understand date {t:?} (try 2026-10-19, 10/19, 19, today, yesterday, -3)"
                    );
                    self.p.retry(e, t);
                }
            }
        }
        Ok(answer)
    }

    fn ask_description(&mut self) -> io::Result<Answer> {
        self.ensure_loaded()?;
        let data = self
            .data
            .get_or_insert_with(|| Loaded::new(Journal::default()));
        let journal = &data.journal;
        let detail = |text: &str| {
            let accounts: Vec<String> = journal
                .template(text)
                .into_iter()
                .map(|p| p.account)
                .collect();
            accounts.join(", ")
        };
        let mut spec = Spec::new("Description");
        spec.completion = Some(&data.descriptions);
        spec.detail = Some(&detail);
        let answer = self.p.ask(spec)?;
        if let Answer::Text(t) = &answer {
            let (desc, comment) = split_comment(t);
            self.draft.template = journal.template(&desc);
            self.draft.description = Some(desc);
            self.draft.comment = comment;
        }
        Ok(answer)
    }

    fn balance(&self) -> Mixed {
        let mut m = Mixed::default();
        for p in &self.draft.postings {
            m.add(&p.weight);
        }
        m
    }

    fn is_balanced(&self) -> bool {
        let styles = &self.data().journal.styles;
        self.draft.postings.len() >= 2 && self.balance().is_balanced(styles)
    }

    fn ask_account(&mut self) -> io::Result<Answer> {
        let n = self.draft.postings.len();
        let balanced = self.is_balanced();
        let default = self.draft.template.get(n).map(|p| p.account.clone());
        let data = self.data.as_ref().expect("loaded");
        let label = if balanced {
            format!(
                "Account {} (Enter{} to finish)",
                n + 1,
                if default.is_some() { " ." } else { "" }
            )
        } else {
            format!("Account {}", n + 1)
        };
        let mut spec = Spec::new(label);
        spec.default = default;
        spec.completion = Some(&data.accounts);
        let answer = self.p.ask(spec)?;
        if let Answer::Text(t) = &answer {
            let t = t.trim();
            if t.is_empty() || t == "." {
                if balanced {
                    self.draft.finished = true;
                    let txn = self.render_transaction();
                    self.p.say(&txn)?;
                } else if n < 2 {
                    self.p.error = Some("A transaction needs at least two postings.".into());
                } else {
                    let styles = &self.data.as_ref().expect("loaded").journal.styles;
                    self.p.error = Some(format!(
                        "Transaction is unbalanced by {}; add another posting.",
                        self.balance().display(styles)
                    ));
                }
            } else if t.contains("  ") || t.contains('\t') {
                self.p
                    .retry("Account names can't contain two spaces or tabs.".into(), t);
            } else {
                self.draft.pending_account = Some(t.to_owned());
            }
        }
        Ok(answer)
    }

    fn default_amount(&self) -> Option<String> {
        let n = self.draft.postings.len();
        let tpl = &self.draft.template;
        let tpl_amount = tpl.get(n).and_then(|p| p.amount.clone());
        let is_last_tpl = n + 1 >= tpl.len();
        if !is_last_tpl && tpl_amount.is_some() {
            return tpl_amount;
        }
        let styles = &self.data().journal.styles;
        let balance = self.balance();
        let balancing = balance.nonzero(styles).next().map(|a| {
            styles.format(&Amount {
                qty: -a.qty,
                commodity: a.commodity.clone(),
            })
        });
        balancing.or(tpl_amount)
    }

    fn ask_amount(&mut self) -> io::Result<Answer> {
        let n = self.draft.postings.len();
        let default = self.default_amount();
        let mut spec = Spec::new(format!("Amount {}", n + 1));
        spec.default = default.clone();
        let styles = &self.data.as_ref().expect("loaded").journal.styles;
        if n > 0 && !self.balance().is_balanced(styles) {
            spec.hint = Some(format!(
                "  balance so far: {}",
                self.balance().display(styles)
            ));
        }
        let answer = self.p.ask(spec)?;
        let Answer::Text(t) = &answer else {
            return Ok(answer);
        };
        let (text, comment) = split_comment(t);
        if text.is_empty() {
            self.p
                .retry("An amount is required (Esc goes back).".into(), t);
            return Ok(answer);
        }
        let styles = &mut self.data.as_mut().expect("loaded").journal.styles;
        let mut pa = match parse_posting_amount(&text, Some(styles)) {
            Ok(pa) => pa,
            Err(e) => {
                self.p.retry(
                    format!("{e} (e.g. $12.50, 10 EUR, $(20+5)/2, -3 AAPL @ $150)"),
                    t,
                );
                return Ok(answer);
            }
        };
        let mut text = pa.evaluated.clone().unwrap_or(text);
        // A bare number takes the commodity of the suggested amount.
        let default_commodity = default
            .as_deref()
            .and_then(|d| parse_posting_amount(d, Some(styles)).ok())
            .map(|d| d.parsed.amount.commodity)
            .filter(|c| !c.is_empty());
        if pa.parsed.amount.commodity.is_empty()
            && pa.weight.commodity.is_empty()
            && let Some(c) = default_commodity
        {
            pa.parsed.amount.commodity = c.clone();
            pa.weight.commodity = c;
            text = styles.format(&pa.parsed.amount);
        } else if pa.evaluated.is_none() {
            styles.observe(&pa.parsed.amount.commodity, &pa.parsed.style);
        }
        if pa.evaluated.is_some() {
            self.p.say(&format!("  = {text}"))?;
        }
        let account = self.draft.pending_account.take().expect("pending account");
        self.draft.postings.push(Posting {
            account,
            amount: text,
            comment,
            weight: pa.weight,
        });
        Ok(answer)
    }

    fn ask_confirm(&mut self) -> io::Result<Answer> {
        let mut spec = Spec::new("Save this transaction to the journal?");
        spec.default = Some("y".into());
        let answer = self.p.ask(spec)?;
        if let Answer::Text(t) = &answer {
            match t.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" => {
                    self.save()?;
                    self.p
                        .say(&format!("Saved to {}.\n", self.path.display()))?;
                    self.last_date = self.draft.date;
                    self.draft = Draft::default();
                }
                "n" | "no" => {
                    self.p.say("Discarded.\n")?;
                    self.draft = Draft::default();
                }
                _ => self.p.error = Some("Please answer y or n (or < to go back).".into()),
            }
        }
        Ok(answer)
    }

    fn render_transaction(&self) -> String {
        let d = &self.draft;
        let mut s = format!(
            "{} {}",
            d.date.expect("date"),
            d.description.as_deref().unwrap_or("")
        );
        if let Some(c) = &d.comment {
            s.push_str(&format!("  ; {c}"));
        }
        s.push('\n');
        let aw = d
            .postings
            .iter()
            .map(|p| p.account.width())
            .max()
            .unwrap_or(0);
        let mw = d
            .postings
            .iter()
            .map(|p| p.amount.width())
            .max()
            .unwrap_or(0);
        for p in &d.postings {
            let pad_a = aw - p.account.width();
            let pad_m = mw - p.amount.width();
            s.push_str(&format!(
                "    {}{}  {}{}",
                p.account,
                " ".repeat(pad_a),
                " ".repeat(pad_m),
                p.amount
            ));
            if let Some(c) = &p.comment {
                s.push_str(&format!("  ; {c}"));
            }
            s.push('\n');
        }
        s
    }

    fn save(&mut self) -> io::Result<()> {
        let txn = self.render_transaction();
        append_transaction(&self.path, &txn)?;
        let data = self.data.as_mut().expect("loaded");
        data.journal.record(&txn);
        let desc = self.draft.description.as_deref().unwrap_or_default();
        if let Some(d) = data.journal.description(desc) {
            data.descriptions.upsert(desc, &d.usage);
        }
        for p in &self.draft.postings {
            let name = strip_wrap(&p.account);
            if let Some(a) = data.journal.account(name) {
                data.accounts.upsert(name, &a.usage);
            }
        }
        Ok(())
    }
}

/// Append a transaction, keeping exactly one blank line between entries.
pub fn append_transaction(path: &Path, txn: &str) -> io::Result<()> {
    let mut f = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)?;
    let len = f.metadata()?.len();
    let mut sep = "";
    if len > 0 {
        let mut tail = [0u8; 2];
        let k = len.min(2) as usize;
        f.seek(SeekFrom::End(-(k as i64)))?;
        f.read_exact(&mut tail[..k])?;
        sep = match &tail[..k] {
            [b'\n', b'\n'] => "",
            [.., b'\n'] => "\n",
            _ => "\n\n",
        };
    }
    f.write_all(format!("{sep}{txn}").as_bytes())?;
    f.flush()
}
