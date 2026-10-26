//! The `add` conversation: date → description → (account, amount)* → confirm → append.

use crate::amount::{Amount, Mixed, PostingAmount, Styles, parse_posting_amount};
use crate::case::Case;
use crate::dates::parse_smart_date;
use crate::fuzzy::{Boost, Index, Weights};
use crate::journal::{Journal, TemplatePosting, Usage, strip_wrap};
use crate::status::Status;
use crate::ui::{Answer, Spec, Ui};
use chrono::NaiveDate;
use rustc_hash::FxHashMap;
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use unicode_width::UnicodeWidthStr;

/// Everything derived from the journal; built off the main thread at startup.
pub struct Loaded {
    pub journal: Journal,
    /// Past descriptions, one per group of spellings (see [`Case`]).
    pub descriptions: Index,
    pub accounts: Index,
    case: Case,
    /// Description groups by [`Case::key`].
    groups: FxHashMap<String, Group>,
    /// Group keys, in the order their words are indexed under.
    keys: Vec<String>,
    /// Each word of a description, with the groups (indices into `keys`) using it.
    words: FxHashMap<String, Vec<u32>>,
    /// Points for an account that always went with one already entered.
    context: f64,
}

/// How much of a new description's word weight a past description must share to
/// lend it its accounts.
const BORROW_MIN: f64 = 0.5;
/// How likely an account must be, given those already entered, to be the default.
const SUGGEST_MIN: f64 = 0.5;

/// The distinct lowercase words of a description, leaving out bare numbers and
/// single characters, which say little about the payee.
fn words(text: &str) -> Vec<String> {
    let mut out: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().nth(1).is_some() && !w.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_lowercase)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Accounts lent to a new description by a similar past one.
#[derive(Debug, PartialEq)]
pub struct Borrowed {
    /// The past description, as shown.
    pub source: String,
    /// Its latest transaction's postings, without amounts.
    pub postings: Vec<TemplatePosting>,
    /// An amount it was written with, whose commodity a bare number takes.
    pub sample: Option<String>,
}

/// The spellings of a description that are shown as one entry.
#[derive(Default)]
struct Group {
    shown: String,
    /// The spelling used most recently; its transaction is the template.
    latest: String,
    /// Combined usage of every spelling.
    usage: Usage,
    /// Count and recency of the spelling `shown` comes from.
    best: (u32, u32),
}

impl Loaded {
    pub fn new(journal: Journal) -> Loaded {
        Loaded::build(journal, Weights::default(), Case::Original)
    }

    pub fn with_weights(journal: Journal, weights: Weights) -> Loaded {
        Loaded::build(journal, weights, Case::Original)
    }

    pub fn with_case(journal: Journal, case: Case) -> Loaded {
        Loaded::build(journal, Weights::default(), case)
    }

    fn build(journal: Journal, weights: Weights, case: Case) -> Loaded {
        let mut order = Vec::new();
        let mut groups: FxHashMap<String, Group> = FxHashMap::default();
        for d in &journal.descriptions {
            let g = groups.entry(case.key(&d.text)).or_insert_with_key(|k| {
                order.push(k.clone());
                Group::default()
            });
            if d.usage.last_seq >= g.usage.last_seq {
                g.latest.clone_from(&d.text);
                g.usage.last_seq = d.usage.last_seq;
            }
            g.usage.count += d.usage.count;
            let rank = (d.usage.count, d.usage.last_seq);
            if rank > g.best {
                g.best = rank;
                g.shown = case.apply(&d.text);
            }
        }
        let descriptions = Index::build_with(
            order
                .iter()
                .map(|k| (groups[k].shown.clone(), groups[k].usage.clone()))
                .collect(),
            weights.clone(),
        );
        let context = weights.context;
        let accounts = Index::build_with(
            journal
                .accounts
                .iter()
                .map(|a| (a.name.clone(), a.usage.clone()))
                .collect(),
            weights,
        );
        let mut loaded = Loaded {
            journal,
            descriptions,
            accounts,
            case,
            groups,
            keys: Vec::with_capacity(order.len()),
            words: FxHashMap::default(),
            context,
        };
        for key in order {
            loaded.add_key(key);
        }
        loaded
    }

    fn add_key(&mut self, key: String) {
        let g = self.keys.len() as u32;
        for w in words(&key) {
            self.words.entry(w).or_default().push(g);
        }
        self.keys.push(key);
    }

    /// For a description never used before, the past description that shares most
    /// of its distinctive words.
    pub fn borrow(&self, text: &str) -> Option<Borrowed> {
        if self.groups.contains_key(&self.case.key(text)) {
            return None;
        }
        // Rare words say more about the payee than common ones.
        let n = self.keys.len() as f64;
        let idf = |df: usize| ((n + 1.0) / (df as f64 + 1.0)).ln();
        let mut total = 0.0;
        let mut shared: FxHashMap<u32, f64> = FxHashMap::default();
        for w in words(text) {
            let groups = self.words.get(&w).map_or(&[][..], Vec::as_slice);
            let weight = idf(groups.len());
            total += weight;
            for &g in groups {
                *shared.entry(g).or_default() += weight;
            }
        }
        let rank = |g: u32, score: f64| {
            let u = &self.groups[&self.keys[g as usize]].usage;
            (score, u.count, u.last_seq)
        };
        let (g, _) = shared
            .into_iter()
            .filter(|&(_, score)| total > 0.0 && score / total >= BORROW_MIN)
            .max_by(|a, b| rank(a.0, a.1).partial_cmp(&rank(b.0, b.1)).expect("finite"))?;
        let group = &self.groups[&self.keys[g as usize]];
        let mut postings = self.journal.template(&group.latest);
        let sample = postings.iter().find_map(|p| p.amount.clone());
        for p in &mut postings {
            p.amount = None;
        }
        (!postings.is_empty()).then(|| Borrowed {
            source: group.shown.clone(),
            postings,
            sample,
        })
    }

    /// A ranking boost for accounts by how often they went with the `entered` ones,
    /// and the one to suggest if it's likely enough.
    pub fn account_context(&self, entered: &[&str]) -> (Boost, Option<String>) {
        let mut likely: FxHashMap<&str, f64> = FxHashMap::default();
        for a in entered {
            let Some((txns, partners)) = self.journal.partners(a) else {
                continue;
            };
            for (b, n) in partners {
                // Smoothed, so one or two sightings count for less than many.
                let p = f64::from(n) / f64::from(txns + 2);
                let e = likely.entry(b).or_default();
                *e = e.max(p);
            }
        }
        for a in entered {
            likely.remove(a);
        }
        let boost = likely
            .iter()
            .filter_map(|(b, p)| {
                let points = (p * self.context) as u32;
                (points > 0).then_some((self.accounts.id(b)?, points))
            })
            .collect();
        let suggestion = likely
            .into_iter()
            .filter(|&(_, p)| p >= SUGGEST_MIN)
            .max_by(|a, b| a.1.partial_cmp(&b.1).expect("finite").then(b.0.cmp(a.0)))
            .map(|(b, _)| b.to_owned());
        (boost, suggestion)
    }

    /// Postings of the most recent transaction with a spelling of `text`.
    pub fn template(&self, text: &str) -> Vec<TemplatePosting> {
        let exact = self
            .groups
            .get(&self.case.key(text))
            .map_or(text, |g| g.latest.as_str());
        self.journal.template(exact)
    }

    /// Add a just-entered transaction (journal text) so it ranks as most recent.
    pub fn record(&mut self, txn: &str) {
        self.journal.record(txn);
        let added = Journal::parse_str(txn);
        let Some(desc) = added.descriptions.first().map(|d| d.text.as_str()) else {
            return;
        };
        if let Some(last_seq) = self.journal.description(desc).map(|d| d.usage.last_seq) {
            let case = self.case;
            let key = case.key(desc);
            if !self.groups.contains_key(&key) {
                self.add_key(key.clone());
            }
            let g = self.groups.entry(key).or_insert_with(|| Group {
                shown: case.apply(desc),
                ..Group::default()
            });
            desc.clone_into(&mut g.latest);
            g.usage.count += 1;
            g.usage.last_seq = last_seq;
            self.descriptions.upsert(&g.shown, &g.usage);
        }
        for p in added.template(desc) {
            let name = strip_wrap(&p.account);
            if let Some(a) = self.journal.account(name) {
                self.accounts.upsert(name, &a.usage);
            }
        }
    }

    pub fn spawn(path: PathBuf, case: Case) -> JoinHandle<io::Result<Loaded>> {
        std::thread::spawn(move || Journal::load(&path).map(|j| Loaded::with_case(j, case)))
    }
}

#[derive(Clone)]
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
    /// An amount of the description `template` was borrowed from, if it was, whose
    /// commodity a bare number takes.
    sample_amount: Option<String>,
    postings: Vec<Posting>,
    pending_account: Option<String>,
    /// Ctrl-T was used: save once the pending amount is given, the template
    /// supplying the rest.
    amending: bool,
    finished: bool,
    status: Status,
}

impl Draft {
    fn is_empty(&self) -> bool {
        self.date.is_none()
    }

    fn render(&self, postings: &[Posting]) -> String {
        render_transaction(
            self.date.expect("date"),
            self.status,
            self.description.as_deref().unwrap_or(""),
            self.comment.as_deref(),
            postings,
        )
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

/// An amount answer as it will be written: arithmetic evaluated, and a bare number
/// given the commodity of `default`. Also says whether the bare number was restyled.
fn resolve_amount(
    text: &str,
    default: Option<&str>,
    styles: &Styles,
) -> Result<(PostingAmount, String, bool), String> {
    let mut pa = parse_posting_amount(text, Some(styles))?;
    let mut written = pa.evaluated.clone().unwrap_or_else(|| text.to_owned());
    let default_commodity = default
        .and_then(|d| parse_posting_amount(d, Some(styles)).ok())
        .map(|d| d.parsed.amount.commodity)
        .filter(|c| !c.is_empty());
    let mut restyled = false;
    if pa.parsed.amount.commodity.is_empty()
        && pa.weight.commodity.is_empty()
        && let Some(c) = default_commodity
    {
        pa.parsed.amount.commodity = c.clone();
        pa.weight.commodity = c;
        written = styles.format(&pa.parsed.amount);
        restyled = true;
    }
    Ok((pa, written, restyled))
}

fn sum(postings: &[Posting]) -> Mixed {
    let mut m = Mixed::default();
    for p in postings {
        m.add(&p.weight);
    }
    m
}

fn is_balanced(postings: &[Posting], styles: &Styles) -> bool {
    postings.len() >= 2 && sum(postings).is_balanced(styles)
}

/// The suggested amount for the next posting: the template's, or what balances.
fn default_amount(draft: &Draft, styles: &Styles) -> Option<String> {
    let n = draft.postings.len();
    let tpl = &draft.template;
    let tpl_amount = tpl.get(n).and_then(|p| p.amount.clone());
    let is_last_tpl = n + 1 >= tpl.len();
    if !is_last_tpl && tpl_amount.is_some() {
        return tpl_amount;
    }
    let balance = sum(&draft.postings);
    let balancing = balance.nonzero(styles).next().map(|a| {
        styles.format(&Amount {
            qty: -a.qty,
            commodity: a.commodity.clone(),
        })
    });
    balancing.or(tpl_amount)
}

/// `fixed` followed by postings for `rest` with amounts filled in as far as they can
/// be without asking: amounts as last written, and a single missing amount takes
/// whatever balances the rest. Stops at the first posting that needs input.
fn fill_template(fixed: Vec<Posting>, rest: &[TemplatePosting], styles: &Styles) -> Vec<Posting> {
    let filled = fill(fixed.clone(), rest, styles, false);
    if is_balanced(&filled, styles) || rest.iter().any(|t| t.amount.is_none()) {
        return filled;
    }
    // Amounts differ from last time: let the last posting take up the difference.
    fill(fixed, rest, styles, true)
}

fn fill(
    mut out: Vec<Posting>,
    rest: &[TemplatePosting],
    styles: &Styles,
    balance_last: bool,
) -> Vec<Posting> {
    let mut given = Vec::new();
    for (i, t) in rest.iter().enumerate() {
        let amount = t
            .amount
            .as_ref()
            .filter(|_| !(balance_last && i + 1 == rest.len()));
        match amount {
            Some(a) => match parse_posting_amount(a, Some(styles)) {
                Ok(pa) => given.push((
                    t,
                    Some((pa.evaluated.unwrap_or_else(|| a.clone()), pa.weight)),
                )),
                Err(_) => break,
            },
            None => given.push((t, None)),
        }
    }
    let mut balance = sum(&out);
    for (_, w) in given.iter().filter_map(|(_, a)| a.as_ref()) {
        balance.add(w);
    }
    let missing = given.iter().filter(|(_, a)| a.is_none()).count();
    for (t, a) in given {
        let (amount, weight) = match a {
            Some(a) => a,
            None => {
                let mut nz = balance.nonzero(styles);
                match (missing, nz.next(), nz.next()) {
                    (1, Some(b), None) => {
                        let w = Amount {
                            qty: -b.qty,
                            commodity: b.commodity.clone(),
                        };
                        (styles.format(&w), w)
                    }
                    _ => break,
                }
            }
        };
        out.push(Posting {
            account: t.account.clone(),
            amount,
            comment: None,
            weight,
        });
    }
    out
}

/// The prompt being previewed, with the answer it would get.
enum Step<'a> {
    Description,
    Account(&'a str),
    Amount(&'a str),
}

/// The postings the transaction would be saved with if the current prompt got
/// `step`'s answer and the template supplied the rest, or `None` if it would still
/// need input.
fn complete(draft: &Draft, step: Step, styles: &Styles) -> Option<Vec<Posting>> {
    let n = draft.postings.len();
    let tail = draft.template.get(n + 1..).unwrap_or_default();
    let mut fixed = draft.postings.clone();
    let mut rest = Vec::new();
    match step {
        Step::Description => rest = draft.template.clone(),
        Step::Account(a) => {
            let a = a.trim();
            if a.contains("  ") || a.contains('\t') {
                return None;
            }
            if !a.is_empty() && a != "." {
                rest.push(TemplatePosting {
                    account: a.to_owned(),
                    amount: draft.template.get(n).and_then(|t| t.amount.clone()),
                });
                rest.extend_from_slice(tail);
            }
        }
        Step::Amount(a) => {
            let (text, comment) = split_comment(a);
            let default = default_amount(draft, styles);
            let like = default.as_deref().or(draft.sample_amount.as_deref());
            let (pa, amount, _) = resolve_amount(&text, like, styles).ok()?;
            fixed.push(Posting {
                account: draft.pending_account.clone()?,
                amount,
                comment,
                weight: pa.weight,
            });
            rest.extend_from_slice(tail);
        }
    }
    finish(fixed, &rest, styles)
}

/// `fixed` followed by `rest` filled in from the template, if that needs no input.
fn finish(fixed: Vec<Posting>, rest: &[TemplatePosting], styles: &Styles) -> Option<Vec<Posting>> {
    let total = fixed.len() + rest.len();
    let postings = fill_template(fixed, rest, styles);
    (postings.len() == total && is_balanced(&postings, styles)).then_some(postings)
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
    status: Status,
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
            status: Status::default(),
        }
    }

    /// The status mark (`!` or `*`) to write on every added transaction.
    pub fn status(mut self, status: Status) -> Self {
        self.status = status;
        self.draft = self.new_draft();
        self
    }

    fn new_draft(&self) -> Draft {
        Draft {
            status: self.status,
            ..Draft::default()
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
                Answer::Text(_) | Answer::Accept(_) | Answer::Amend(_) => {}
            }
        }
    }

    fn back(&mut self) {
        let d = &mut self.draft;
        d.amending = false;
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
            d.sample_amount = None;
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
        let data = &*data;
        let date = self.draft.date;
        let status = self.draft.status;
        let preview = |text: &str| {
            let (desc, comment) = split_comment(text);
            let draft = Draft {
                date,
                template: data.template(&desc),
                description: Some(desc),
                comment,
                status,
                ..Draft::default()
            };
            let postings = complete(&draft, Step::Description, &data.journal.styles)?;
            Some(draft.render(&postings))
        };
        let mut spec = Spec::new("Description");
        spec.completion = Some(&data.descriptions);
        spec.preview = Some(&preview);
        spec.amend = true;
        let answer = self.p.ask(spec)?;
        if let Answer::Text(t) | Answer::Accept(t) | Answer::Amend(t) = &answer {
            let (desc, comment) = split_comment(t);
            self.draft.template = data.template(&desc);
            self.draft.sample_amount = None;
            if self.draft.template.is_empty()
                && let Some(b) = data.borrow(&desc)
            {
                self.draft.template = b.postings;
                self.draft.sample_amount = b.sample;
            }
            self.draft.description = Some(desc);
            self.draft.comment = comment;
        }
        match &answer {
            Answer::Accept(_) => self.accept(Step::Description)?,
            Answer::Amend(_) => self.amend(Step::Description),
            _ => {}
        }
        Ok(answer)
    }

    /// Ctrl-T: take the previewed transaction's postings up to this prompt's, and
    /// ask for that one's amount; the template supplies the rest when it's given.
    fn amend(&mut self, step: Step) {
        let n = self.draft.postings.len();
        let styles = &self.data().journal.styles;
        let Some(mut postings) = complete(&self.draft, step, styles) else {
            return;
        };
        if postings.len() <= n {
            return; // finishing: there's no posting to amend
        }
        postings.truncate(n + 1);
        let p = postings.pop().expect("posting n");
        self.draft.postings = postings;
        self.draft.pending_account = Some(p.account);
        self.draft.amending = true;
    }

    /// Ctrl-Y: save the previewed transaction, if it's complete.
    fn accept(&mut self, step: Step) -> io::Result<()> {
        let styles = &self.data().journal.styles;
        let Some(postings) = complete(&self.draft, step, styles) else {
            return Ok(());
        };
        self.draft.postings = postings;
        self.draft.pending_account = None;
        self.p.say(&self.render_transaction())?;
        self.save_draft()
    }

    fn balance(&self) -> Mixed {
        sum(&self.draft.postings)
    }

    fn is_balanced(&self) -> bool {
        is_balanced(&self.draft.postings, &self.data().journal.styles)
    }

    fn ask_account(&mut self) -> io::Result<Answer> {
        let n = self.draft.postings.len();
        let balanced = self.is_balanced();
        let data = self.data.as_ref().expect("loaded");
        let draft = &self.draft;
        let entered: Vec<&str> = draft
            .postings
            .iter()
            .map(|p| strip_wrap(&p.account))
            .collect();
        let (boost, suggestion) = data.account_context(&entered);
        let default = match draft.template.get(n) {
            Some(p) => Some(p.account.clone()),
            None if !balanced => suggestion,
            None => None,
        };
        let styles = &data.journal.styles;
        let preview = |t: &str| complete(draft, Step::Account(t), styles).map(|p| draft.render(&p));
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
        spec.boost = Some(&boost);
        spec.preview = Some(&preview);
        spec.amend = true;
        let answer = self.p.ask(spec)?;
        match &answer {
            Answer::Accept(t) => self.accept(Step::Account(t))?,
            Answer::Amend(t) => self.amend(Step::Account(t)),
            Answer::Text(t) => {
                let t = t.trim();
                if t.is_empty() || t == "." {
                    if balanced {
                        self.draft.finished = true;
                        let txn = self.render_transaction();
                        self.p.say(&txn)?;
                    } else if n < 2 {
                        self.p.error = Some("A transaction needs at least two postings.".into());
                    } else {
                        let styles = &self.data().journal.styles;
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
            _ => {}
        }
        Ok(answer)
    }

    fn ask_amount(&mut self) -> io::Result<Answer> {
        let n = self.draft.postings.len();
        let data = self.data.as_ref().expect("loaded");
        let styles = &data.journal.styles;
        let draft = &self.draft;
        let default = default_amount(draft, styles);
        let preview = |t: &str| complete(draft, Step::Amount(t), styles).map(|p| draft.render(&p));
        let label = if draft.amending {
            format!("Amount {} (Enter saves)", n + 1)
        } else {
            format!("Amount {}", n + 1)
        };
        let mut spec = Spec::new(label);
        spec.default = default.clone();
        let balance = sum(&draft.postings);
        if n > 0 && !balance.is_balanced(styles) {
            spec.hint = Some(format!("  balance so far: {}", balance.display(styles)));
        }
        spec.preview = Some(&preview);
        let answer = self.p.ask(spec)?;
        let t = match &answer {
            Answer::Text(t) => t,
            Answer::Accept(t) => {
                self.accept(Step::Amount(t))?;
                return Ok(answer);
            }
            _ => return Ok(answer),
        };
        let (text, comment) = split_comment(t);
        if text.is_empty() {
            self.p
                .retry("An amount is required (Esc goes back).".into(), t);
            return Ok(answer);
        }
        let styles = &mut self.data.as_mut().expect("loaded").journal.styles;
        let like = default.as_deref().or(self.draft.sample_amount.as_deref());
        let (pa, text, restyled) = match resolve_amount(&text, like, styles) {
            Ok(r) => r,
            Err(e) => {
                self.p.retry(
                    format!("{e} (e.g. $12.50, 10 EUR, $(20+5)/2, -3 AAPL @ $150)"),
                    t,
                );
                return Ok(answer);
            }
        };
        if !restyled && pa.evaluated.is_none() {
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
        if std::mem::take(&mut self.draft.amending) {
            let n = self.draft.postings.len();
            let rest = self.draft.template.get(n..).unwrap_or_default();
            let styles = &self.data().journal.styles;
            // Otherwise (e.g. the amount left more than one commodity to balance)
            // carry on prompting.
            if let Some(postings) = finish(self.draft.postings.clone(), rest, styles) {
                self.draft.postings = postings;
                self.p.say(&self.render_transaction())?;
                self.save_draft()?;
            }
        }
        Ok(answer)
    }

    fn ask_confirm(&mut self) -> io::Result<Answer> {
        let mut spec = Spec::new("Save this transaction to the journal?");
        spec.default = Some("y".into());
        let answer = self.p.ask(spec)?;
        if let Answer::Text(t) = &answer {
            match t.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" => self.save_draft()?,
                "n" | "no" => {
                    self.p.say("Discarded.\n")?;
                    self.draft = self.new_draft();
                }
                _ => self.p.error = Some("Please answer y or n (or < to go back).".into()),
            }
        }
        Ok(answer)
    }

    fn render_transaction(&self) -> String {
        self.draft.render(&self.draft.postings)
    }

    fn save_draft(&mut self) -> io::Result<()> {
        self.save()?;
        self.p
            .say(&format!("Saved to {}.\n", self.path.display()))?;
        self.last_date = self.draft.date;
        self.draft = self.new_draft();
        Ok(())
    }

    fn save(&mut self) -> io::Result<()> {
        let txn = self.render_transaction();
        append_transaction(&self.path, &txn)?;
        self.data.as_mut().expect("loaded").record(&txn);
        Ok(())
    }
}

fn render_transaction(
    date: NaiveDate,
    status: Status,
    description: &str,
    comment: Option<&str>,
    postings: &[Posting],
) -> String {
    let mut s = format!("{date} {}{description}", status.mark());
    if let Some(c) = comment {
        s.push_str(&format!("  ; {c}"));
    }
    s.push('\n');
    let aw = postings
        .iter()
        .map(|p| p.account.width())
        .max()
        .unwrap_or(0);
    let mw = postings.iter().map(|p| p.amount.width()).max().unwrap_or(0);
    for p in postings {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tpl(postings: &[(&str, Option<&str>)]) -> Vec<TemplatePosting> {
        postings
            .iter()
            .map(|&(account, amount)| TemplatePosting {
                account: account.into(),
                amount: amount.map(Into::into),
            })
            .collect()
    }

    /// Styles as learned from a journal that writes dollars as `$1.00`.
    fn dollars() -> Styles {
        Journal::parse_str("2026-01-01 x\n    a  $1.00\n    b\n").styles
    }

    fn amounts(postings: &[Posting]) -> Vec<&str> {
        postings.iter().map(|p| p.amount.as_str()).collect()
    }

    /// A draft for "Grocer" (last time: food $42.10, checking) with `entered` postings.
    fn grocer(entered: &[(&str, &str)], pending: Option<&str>) -> Draft {
        let styles = dollars();
        Draft {
            date: NaiveDate::from_ymd_opt(2026, 10, 19),
            description: Some("Grocer".into()),
            template: tpl(&[("expenses:food", Some("$42.10")), ("assets:checking", None)]),
            postings: entered
                .iter()
                .map(|&(account, amount)| Posting {
                    account: account.into(),
                    amount: amount.into(),
                    comment: None,
                    weight: parse_posting_amount(amount, Some(&styles)).unwrap().weight,
                })
                .collect(),
            pending_account: pending.map(Into::into),
            ..Draft::default()
        }
    }

    #[test]
    fn previews_what_is_typed_for_an_amount() {
        let styles = dollars();
        let d = grocer(&[], Some("expenses:food"));
        let p = complete(&d, Step::Amount("$50 ; snacks"), &styles).unwrap();
        assert_eq!(amounts(&p), ["$50", "$-50.00"]);
        assert_eq!(p[0].comment.as_deref(), Some("snacks"));
        // A bare number takes the default's commodity; arithmetic is evaluated.
        let p = complete(&d, Step::Amount("10+2.5"), &styles).unwrap();
        assert_eq!(amounts(&p), ["$12.50", "$-12.50"]);
        assert!(complete(&d, Step::Amount("$$"), &styles).is_none());
    }

    #[test]
    fn previews_a_different_account() {
        let styles = dollars();
        let d = grocer(&[], None);
        let p = complete(&d, Step::Account("expenses:snacks"), &styles).unwrap();
        assert_eq!(p[0].account, "expenses:snacks");
        assert_eq!(amounts(&p), ["$42.10", "$-42.10"]);
        assert!(complete(&d, Step::Account("bad  name"), &styles).is_none());
    }

    #[test]
    fn last_posting_absorbs_a_changed_amount() {
        let styles = dollars();
        let mut d = grocer(&[("expenses:food", "$50")], None);
        d.template[1].amount = Some("$-42.10".into());
        let p = complete(&d, Step::Account("assets:checking"), &styles).unwrap();
        assert_eq!(amounts(&p), ["$50", "$-50.00"]);
    }

    #[test]
    fn finishing_needs_a_balanced_transaction() {
        let styles = dollars();
        let open = grocer(&[("expenses:food", "$50")], None);
        assert!(complete(&open, Step::Account(""), &styles).is_none());
        let done = grocer(
            &[("expenses:food", "$50"), ("assets:checking", "$-50")],
            None,
        );
        let p = complete(&done, Step::Account("."), &styles).unwrap();
        assert_eq!(amounts(&p), ["$50", "$-50"]);
    }

    const VARIANTS: &str = "\
2026-01-01 GROCERY STORE
    expenses:food    $10
    assets:cash

2026-01-02 grocery store
    expenses:food    $20
    assets:checking

2026-01-03 Coffee
    expenses:coffee    $3
    assets:cash
";

    fn shown(data: &Loaded) -> Vec<&str> {
        let mut v: Vec<&str> = (0..data.descriptions.len() as u32)
            .map(|i| data.descriptions.get(i).text.as_str())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn merges_descriptions_differing_in_case() {
        let data = Loaded::with_case(Journal::parse_str(VARIANTS), Case::Title);
        assert_eq!(shown(&data), ["Coffee", "Grocery Store"]);
        // The template is the most recent spelling's, whatever case is asked for.
        let accounts: Vec<String> = data
            .template("GROCERY store")
            .into_iter()
            .map(|p| p.account)
            .collect();
        assert_eq!(accounts, ["expenses:food", "assets:checking"]);
        let original = Loaded::with_case(Journal::parse_str(VARIANTS), Case::Original);
        assert_eq!(
            shown(&original),
            ["Coffee", "GROCERY STORE", "grocery store"]
        );
    }

    #[test]
    fn usual_shows_the_most_used_spelling() {
        let journal = Journal::parse_str(
            "2026-01-01 ACME/CO\n    a  $1\n    b\n\n\
             2026-01-02 ACME/CO\n    a  $1\n    b\n\n\
             2026-01-03 acme/co\n    a  $2\n    c\n",
        );
        let data = Loaded::with_case(journal, Case::Usual);
        assert_eq!(shown(&data), ["ACME/CO"]);
        let accounts: Vec<String> = data
            .template("ACME/CO")
            .into_iter()
            .map(|p| p.account)
            .collect();
        assert_eq!(accounts, ["a", "c"]);
        // On a tie the more recent spelling is shown.
        let tie = Loaded::with_case(Journal::parse_str(VARIANTS), Case::Usual);
        assert_eq!(shown(&tie), ["Coffee", "grocery store"]);
    }

    #[test]
    fn recording_updates_the_merged_entry() {
        let mut data = Loaded::with_case(Journal::parse_str(VARIANTS), Case::Lower);
        data.record("2026-01-04 Grocery Store\n    expenses:food    $5\n    assets:card\n");
        assert_eq!(shown(&data), ["coffee", "grocery store"]);
        let id = data.descriptions.id("grocery store").unwrap();
        assert_eq!(data.descriptions.get(id).text, "grocery store");
        let accounts: Vec<String> = data
            .template("grocery store")
            .into_iter()
            .map(|p| p.account)
            .collect();
        assert_eq!(accounts, ["expenses:food", "assets:card"]);
    }

    const CAFES: &str = "\
2026-01-01 Blue Bottle Coffee
    expenses:coffee    $4.00
    liabilities:amex

2026-01-02 Grocery Store
    expenses:food    $40
    assets:checking

2026-01-03 Corner Store
    expenses:food    $9
    assets:checking

2026-01-04 Sightglass
    expenses:coffee    $5
    liabilities:amex

2026-01-05 Ritual
    expenses:coffee    $4
    liabilities:amex

2026-01-06 Office Coffee
    expenses:coffee    $2
    assets:checking
";

    #[test]
    fn borrows_accounts_from_a_similar_description() {
        let data = Loaded::new(Journal::parse_str(CAFES));
        assert_eq!(
            data.borrow("Blue Bottle Roasters"),
            Some(Borrowed {
                source: "Blue Bottle Coffee".into(),
                postings: tpl(&[("expenses:coffee", None), ("liabilities:amex", None)]),
                sample: Some("$4.00".into()),
            })
        );
        // Known descriptions keep their own template; weak overlaps lend nothing.
        assert!(data.borrow("Grocery Store").is_none());
        assert!(data.borrow("Main Street Store").is_none());
        assert!(data.borrow("Joe's Pizza").is_none());
        assert!(data.borrow("").is_none());
    }

    #[test]
    fn suggests_accounts_that_go_with_those_entered() {
        let data = Loaded::new(Journal::parse_str(CAFES));
        let (boost, suggestion) = data.account_context(&["expenses:coffee"]);
        // amex went with coffee 3 times out of 4, checking once.
        assert_eq!(suggestion.as_deref(), Some("liabilities:amex"));
        let points = |name| boost.get(&data.accounts.id(name).unwrap()).copied();
        assert!(points("liabilities:amex") > points("assets:checking"));
        assert_eq!(points("expenses:coffee"), None);
        let (_, none) = data.account_context(&["expenses:food", "assets:checking"]);
        assert_eq!(none, None);
        assert_eq!(data.account_context(&[]).1, None);
    }

    #[test]
    fn new_payees_get_borrowed_and_suggested_accounts() {
        let dir = std::env::temp_dir().join(format!("ladder-app-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("j.journal");
        std::fs::write(&path, CAFES).unwrap();
        let loader = Loaded::spawn(path.clone(), Case::default());
        // Borrowed accounts: Enter takes each; then a payee with nothing to borrow,
        // where account 2 defaults to what usually goes with coffee.
        let input = "2026-10-20\nBlue Bottle Roasters\n\n4.50\n\n\n\ny\n\
                     \nSomewhere New\nexpenses:coffee\n$3\n\n\n\ny\n";
        let ui = crate::ui::LineUi::new(input.as_bytes());
        let today = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        App::new(ui, path.clone(), loader, today, Vec::new())
            .run()
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let added = &text[CAFES.len()..];
        assert_eq!(
            added,
            "\n2026-10-20 Blue Bottle Roasters\n    expenses:coffee    $4.50\n    liabilities:amex  $-4.50\n\
             \n2026-10-20 Somewhere New\n    expenses:coffee       $3\n    liabilities:amex  $-3.00\n"
        );
    }

    /// Gives scripted answers, recording each prompt's label.
    struct Script {
        answers: VecDeque<Answer>,
        labels: Vec<String>,
    }

    impl Ui for Script {
        fn ask(&mut self, spec: &Spec) -> io::Result<Answer> {
            self.labels.push(spec.label.clone());
            Ok(match self.answers.pop_front() {
                Some(Answer::Text(t)) if t.is_empty() => {
                    Answer::Text(spec.default.clone().unwrap_or_default())
                }
                Some(a) => a,
                None => Answer::Quit,
            })
        }

        fn say(&mut self, _: &str) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn ctrl_t_changes_only_the_amount() {
        let dir = std::env::temp_dir().join(format!("ladder-amend-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("j.journal");
        std::fs::write(&path, CAFES).unwrap();
        let loader = Loaded::spawn(path.clone(), Case::default());
        let text = |t: &str| Answer::Text(t.into());
        // Ctrl-T on a description, then on a different account 1, whose amount
        // defaults to the one just saved.
        let answers = [
            text("2026-10-20"),
            Answer::Amend("Blue Bottle Coffee".into()),
            text("6"),
            text(""),
            text("Blue Bottle Coffee"),
            Answer::Amend("expenses:food".into()),
            text(""),
        ];
        let ui = Script {
            answers: answers.into(),
            labels: Vec::new(),
        };
        let today = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        let mut app = App::new(ui, path.clone(), loader, today, Vec::new());
        app.run().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            app.p.ui.labels,
            [
                "Date",
                "Description",
                "Amount 1 (Enter saves)",
                "Date",
                "Description",
                "Account 1",
                "Amount 1 (Enter saves)",
                "Date",
            ]
        );
        assert_eq!(
            &text[CAFES.len()..],
            "\n2026-10-20 Blue Bottle Coffee\n    expenses:coffee    $6.00\n    liabilities:amex  $-6.00\n\
             \n2026-10-20 Blue Bottle Coffee\n    expenses:food      $6.00\n    liabilities:amex  $-6.00\n"
        );
    }

    #[test]
    fn fills_the_one_missing_amount() {
        let styles = dollars();
        let p = fill_template(
            Vec::new(),
            &tpl(&[("expenses:food", Some("$12.50")), ("assets:cash", None)]),
            &styles,
        );
        assert_eq!(amounts(&p), ["$12.50", "$-12.50"]);
        assert!(is_balanced(&p, &styles));
    }

    #[test]
    fn keeps_explicit_amounts() {
        let styles = dollars();
        let p = fill_template(
            Vec::new(),
            &tpl(&[("expenses:food", Some("$5")), ("assets:cash", Some("-$5"))]),
            &styles,
        );
        assert_eq!(amounts(&p), ["$5", "-$5"]);
        assert!(is_balanced(&p, &styles));
    }

    #[test]
    fn stops_where_input_is_needed() {
        let styles = dollars();
        let two_missing = fill_template(
            Vec::new(),
            &tpl(&[("a", Some("$5")), ("b", None), ("c", None)]),
            &styles,
        );
        assert_eq!(amounts(&two_missing), ["$5"]);
        assert!(!is_balanced(&two_missing, &styles));
        let multi = fill_template(
            Vec::new(),
            &tpl(&[
                ("a", Some("$5")),
                ("b", Some("3 EUR")),
                ("c", Some("1 GBP")),
                ("d", None),
            ]),
            &styles,
        );
        assert_eq!(amounts(&multi), ["$5", "3 EUR", "1 GBP"]);
    }

    #[test]
    fn renders_like_the_journal() {
        let styles = dollars();
        let p = fill_template(
            Vec::new(),
            &tpl(&[("expenses:food", Some("$5")), ("cash", None)]),
            &styles,
        );
        let date = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        assert_eq!(
            render_transaction(date, Status::Unmarked, "Grocer", None, &p),
            "2026-10-19 Grocer\n    expenses:food      $5\n    cash           $-5.00\n"
        );
        assert_eq!(
            render_transaction(date, Status::Cleared, "Grocer", Some("weekly"), &p),
            "2026-10-19 * Grocer  ; weekly\n    expenses:food      $5\n    cash           $-5.00\n"
        );
    }
}
