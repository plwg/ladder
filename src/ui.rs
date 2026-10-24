//! Prompting: an inline raw-mode editor with a live fuzzy dropdown, and a plain
//! line-based fallback used when stdin isn't a terminal.

use crate::fuzzy::{Index, Searcher};
use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{QueueableCommand, queue};
use std::io::{self, BufRead, BufWriter, Stdout, Write};
use std::time::Duration;
use unicode_width::UnicodeWidthChar;

pub type PreviewFn<'a> = dyn Fn(&str) -> Option<String> + 'a;

pub struct Spec<'a> {
    pub label: String,
    pub default: Option<String>,
    /// Text the input starts with (e.g. the old answer after going back).
    pub initial: String,
    pub completion: Option<&'a Index>,
    /// The complete result of the answer Enter would give, shown in a box under the
    /// prompt; Ctrl-Y accepts it as is. `None` when that answer needs more input.
    pub preview: Option<&'a PreviewFn<'a>>,
    pub hint: Option<String>,
    pub error: Option<String>,
}

impl<'a> Spec<'a> {
    pub fn new(label: impl Into<String>) -> Self {
        Spec {
            label: label.into(),
            default: None,
            initial: String::new(),
            completion: None,
            preview: None,
            hint: None,
            error: None,
        }
    }

    fn prompt(&self) -> String {
        match &self.default {
            Some(d) => format!("{} [{d}]: ", self.label),
            None => format!("{}: ", self.label),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Text(String),
    /// An answer given with Ctrl-Y: take its preview as is.
    Accept(String),
    Back,
    Quit,
}

pub trait Ui {
    fn ask(&mut self, spec: &Spec) -> io::Result<Answer>;
    fn say(&mut self, text: &str) -> io::Result<()>;
    /// Show an answer that was supplied up front (command-line args).
    fn echo(&mut self, spec: &Spec, answer: &str) -> io::Result<()> {
        self.say(&format!("{}{answer}", spec.prompt()))
    }
}

/// Resolve what the user typed into an answer (`<` goes back, empty takes the default).
fn resolve(spec: &Spec, input: &str) -> Answer {
    match input.trim() {
        "<" => Answer::Back,
        "" => Answer::Text(spec.default.clone().unwrap_or_default()),
        t => Answer::Text(t.to_owned()),
    }
}

/// Line-oriented prompting for pipes and scripts.
pub struct LineUi<R> {
    input: R,
}

impl<R: BufRead> LineUi<R> {
    pub fn new(input: R) -> Self {
        LineUi { input }
    }
}

impl<R: BufRead> Ui for LineUi<R> {
    fn ask(&mut self, spec: &Spec) -> io::Result<Answer> {
        let mut out = io::stdout().lock();
        if let Some(e) = &spec.error {
            writeln!(out, "{e}")?;
        }
        write!(out, "{}", spec.prompt())?;
        out.flush()?;
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            writeln!(out)?;
            return Ok(Answer::Quit);
        }
        writeln!(out, "{}", line.trim_end())?;
        Ok(resolve(spec, &line))
    }

    fn say(&mut self, text: &str) -> io::Result<()> {
        println!("{text}");
        Ok(())
    }
}

/// Interactive raw-mode prompting with fuzzy completion.
pub struct TermUi {
    out: BufWriter<Stdout>,
    /// Shared across prompts so per-thread matcher buffers are allocated once.
    searcher: Searcher,
}

impl TermUi {
    pub fn new() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(TermUi {
            out: BufWriter::with_capacity(1 << 16, io::stdout()),
            searcher: Searcher::new(),
        })
    }
}

impl Drop for TermUi {
    fn drop(&mut self) {
        let _ = self.out.flush();
        let _ = terminal::disable_raw_mode();
    }
}

const MAX_ROWS: usize = 10;
const MAX_PREVIEW_ROWS: usize = 8;

struct Editor<'s, 'a> {
    spec: &'s Spec<'a>,
    input: Vec<char>,
    cursor: usize,
    searcher: &'s mut Searcher,
    selected: Option<usize>,
    scroll: usize,
}

enum Outcome {
    Continue { edited: bool },
    Done(Answer),
}

impl Outcome {
    const EDITED: Outcome = Outcome::Continue { edited: true };
    const MOVED: Outcome = Outcome::Continue { edited: false };
}

fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

impl<'s, 'a> Editor<'s, 'a> {
    fn text(&self) -> String {
        self.input.iter().collect()
    }

    fn refresh(&mut self) {
        if let Some(index) = self.spec.completion {
            let q = self.text();
            self.searcher.search(index, &q);
            let has = !q.trim().is_empty() && !self.searcher.results.is_empty();
            self.selected = has.then_some(0);
            self.scroll = 0;
        }
    }

    fn set_input(&mut self, s: &str) {
        self.input = s.chars().collect();
        self.cursor = self.input.len();
    }

    fn candidate(&self, i: usize) -> Option<&'a str> {
        let index = self.spec.completion?;
        self.searcher
            .results
            .get(i)
            .map(|&id| index.get(id).text.as_str())
    }

    /// The answer Enter would give, if it's text.
    fn pending(&self) -> Option<String> {
        match self.selected.and_then(|i| self.candidate(i)) {
            Some(c) => Some(c.to_owned()),
            None => match resolve(self.spec, &self.text()) {
                Answer::Text(t) => Some(t),
                _ => None,
            },
        }
    }

    fn preview(&self) -> Option<String> {
        let f = self.spec.preview?;
        f(&self.pending()?)
    }

    fn select(&mut self, delta: isize) {
        let n = self.searcher.results.len();
        if n == 0 {
            return;
        }
        self.selected = match (self.selected, delta) {
            (None, d) if d > 0 => Some((d as usize - 1).min(n - 1)),
            (None, _) => None,
            (Some(i), d) if d < 0 && i < d.unsigned_abs() => None,
            (Some(i), d) => Some((i as isize + d).clamp(0, n as isize - 1) as usize),
        };
        self.scroll_to_selected();
    }

    /// How many match rows fit: everything else drawn below the prompt keeps
    /// its place, so the list shrinks first on a short terminal.
    fn visible_rows(&self) -> usize {
        let (_, h) = terminal::size().unwrap_or((80, 24));
        let mut fixed = 1; // the prompt
        fixed += usize::from(self.spec.error.is_some());
        fixed += usize::from(self.spec.hint.is_some());
        fixed += usize::from(self.spec.completion.is_some()); // status line
        if let Some(p) = self.preview() {
            // A blank line, the box's two edges and its body.
            fixed += 3 + p.lines().take(MAX_PREVIEW_ROWS).count();
        }
        (h as usize).saturating_sub(fixed).clamp(1, MAX_ROWS)
    }

    fn scroll_to_selected(&mut self) {
        let rows = self.visible_rows();
        if let Some(i) = self.selected {
            if i < self.scroll {
                self.scroll = i;
            } else if i >= self.scroll + rows {
                self.scroll = i + 1 - rows;
            }
        }
    }

    fn delete_word_back(&mut self) {
        let mut i = self.cursor;
        while i > 0 && self.input[i - 1] == ' ' {
            i -= 1;
        }
        while i > 0 && !matches!(self.input[i - 1], ' ' | ':') {
            i -= 1;
        }
        if i == self.cursor && i > 0 {
            i -= 1; // lone separator
        }
        self.input.drain(i..self.cursor);
        self.cursor = i;
    }

    fn key(&mut self, k: KeyEvent) -> Outcome {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        match k.code {
            KeyCode::Enter if alt => Outcome::Done(resolve(self.spec, &self.text())),
            // `.` (finish) and `<` (back) are commands, never fuzzy queries.
            KeyCode::Enter if matches!(self.text().trim(), "." | "<") => {
                Outcome::Done(resolve(self.spec, &self.text()))
            }
            KeyCode::Enter => match self.selected.and_then(|i| self.candidate(i)) {
                Some(c) => Outcome::Done(Answer::Text(c.to_owned())),
                None => Outcome::Done(resolve(self.spec, &self.text())),
            },
            KeyCode::Char('y') if ctrl => match self.pending() {
                Some(t) if self.preview().is_some() => Outcome::Done(Answer::Accept(t)),
                _ => Outcome::MOVED,
            },
            KeyCode::Char('c') if ctrl => Outcome::Done(Answer::Quit),
            KeyCode::Char('d') if ctrl && self.input.is_empty() => Outcome::Done(Answer::Quit),
            KeyCode::Esc if self.input.is_empty() => Outcome::Done(Answer::Back),
            KeyCode::Esc => {
                self.input.clear();
                self.cursor = 0;
                Outcome::EDITED
            }
            KeyCode::Tab => {
                match self.candidate(self.selected.unwrap_or(0)) {
                    Some(c) => self.set_input(c),
                    None => return Outcome::MOVED,
                }
                Outcome::EDITED
            }
            KeyCode::Down => {
                self.select(1);
                Outcome::MOVED
            }
            KeyCode::Char('n') if ctrl => {
                self.select(1);
                Outcome::MOVED
            }
            KeyCode::Up => {
                self.select(-1);
                Outcome::MOVED
            }
            KeyCode::Char('p') if ctrl => {
                self.select(-1);
                Outcome::MOVED
            }
            KeyCode::PageDown => {
                self.select(self.visible_rows() as isize);
                Outcome::MOVED
            }
            KeyCode::PageUp => {
                self.select(-(self.visible_rows() as isize));
                Outcome::MOVED
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                Outcome::MOVED
            }
            KeyCode::Char('b') if ctrl => {
                self.cursor = self.cursor.saturating_sub(1);
                Outcome::MOVED
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.input.len());
                Outcome::MOVED
            }
            KeyCode::Char('f') if ctrl => {
                self.cursor = (self.cursor + 1).min(self.input.len());
                Outcome::MOVED
            }
            KeyCode::Home => {
                self.cursor = 0;
                Outcome::MOVED
            }
            KeyCode::Char('a') if ctrl => {
                self.cursor = 0;
                Outcome::MOVED
            }
            KeyCode::End => {
                self.cursor = self.input.len();
                Outcome::MOVED
            }
            KeyCode::Char('e') if ctrl => {
                self.cursor = self.input.len();
                Outcome::MOVED
            }
            KeyCode::Backspace if alt => {
                self.delete_word_back();
                Outcome::EDITED
            }
            KeyCode::Char('w') if ctrl => {
                self.delete_word_back();
                Outcome::EDITED
            }
            KeyCode::Char('h') if ctrl => {
                self.backspace();
                Outcome::EDITED
            }
            KeyCode::Backspace => {
                self.backspace();
                Outcome::EDITED
            }
            KeyCode::Delete => {
                self.delete();
                Outcome::EDITED
            }
            KeyCode::Char('d') if ctrl => {
                self.delete();
                Outcome::EDITED
            }
            KeyCode::Char('u') if ctrl => {
                self.input.drain(..self.cursor);
                self.cursor = 0;
                Outcome::EDITED
            }
            KeyCode::Char('k') if ctrl => {
                self.input.truncate(self.cursor);
                Outcome::EDITED
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.input.insert(self.cursor, c);
                self.cursor += 1;
                Outcome::EDITED
            }
            _ => Outcome::MOVED,
        }
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.input.remove(self.cursor);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.input.len() {
            self.input.remove(self.cursor);
        }
    }

    fn render(&mut self, out: &mut impl Write) -> io::Result<()> {
        let (w, _) = terminal::size().unwrap_or((80, 24));
        let width = (w as usize).max(20) - 1;
        self.scroll_to_selected();
        let prompt = self.spec.prompt();
        let pw: usize = prompt.chars().map(char_width).sum();
        queue!(out, MoveToColumn(0), Clear(ClearType::FromCursorDown))?;
        queue!(
            out,
            SetAttribute(Attribute::Bold),
            Print(&prompt),
            SetAttribute(Attribute::Reset)
        )?;

        // Scroll the input horizontally so the cursor stays visible.
        let avail = width.saturating_sub(pw).max(8);
        let widths: Vec<usize> = self.input.iter().map(|&c| char_width(c)).collect();
        let mut start = 0;
        while widths[start..self.cursor].iter().sum::<usize>() >= avail {
            start += 1;
        }
        let mut shown = 0;
        let mut cursor_col = pw;
        for (i, &c) in self.input.iter().enumerate().skip(start) {
            if shown + widths[i] > avail {
                break;
            }
            if i == self.cursor {
                cursor_col = pw + shown;
            }
            out.queue(Print(c))?;
            shown += widths[i];
        }
        if self.cursor >= self.input.len() {
            cursor_col = pw + shown;
        }

        let mut lines = 0;
        if let Some(e) = &self.spec.error {
            queue!(
                out,
                Print("\r\n"),
                SetForegroundColor(Color::Red),
                Print(truncate(e, width)),
                ResetColor
            )?;
            lines += 1;
        }
        if let Some(index) = self.spec.completion {
            // The list always takes all its rows, so the status line and the box
            // below it stay put however many matches there are.
            let rows = self.visible_rows();
            for row in self.scroll..self.scroll + rows {
                out.queue(Print("\r\n"))?;
                if let Some(&id) = self.searcher.results.get(row) {
                    let item = index.get(id);
                    let hl = self.searcher.highlight(index, id);
                    let selected = self.selected == Some(row);
                    render_row(out, &item.text, &hl, selected, width)?;
                }
                lines += 1;
            }
            let pos = self
                .selected
                .map_or(String::new(), |i| format!("{}/", i + 1));
            let status = format!(
                "  {pos}{} matches · ↑↓ select · Tab insert · Alt-Enter as typed",
                self.searcher.total
            );
            queue!(
                out,
                Print("\r\n"),
                SetForegroundColor(Color::DarkGrey),
                Print(truncate(&status, width)),
                ResetColor
            )?;
            lines += 1;
        }
        if let Some(h) = &self.spec.hint {
            queue!(
                out,
                Print("\r\n"),
                SetForegroundColor(Color::DarkGrey),
                Print(truncate(h, width)),
                ResetColor
            )?;
            lines += 1;
        }
        // A blank line keeps the box apart from the completions and hint.
        if let Some(p) = self.preview() {
            if lines > 0 {
                out.queue(Print("\r\n"))?;
                lines += 1;
            }
            lines += render_preview(out, &p, width)?;
        }
        if lines > 0 {
            out.queue(MoveUp(lines))?;
        }
        out.queue(MoveToColumn(cursor_col as u16))?;
        out.flush()
    }
}

fn truncate(s: &str, width: usize) -> String {
    let mut w = 0;
    s.chars()
        .take_while(|&c| {
            w += char_width(c);
            w <= width
        })
        .collect()
}

/// Draw `text` in a box whose bottom edge is titled with how to accept it; returns the lines used.
fn render_preview(out: &mut impl Write, text: &str, width: usize) -> io::Result<u16> {
    const TITLE: &str = " Ctrl-Y saves this ";
    let body: Vec<&str> = text.lines().take(MAX_PREVIEW_ROWS).collect();
    let text_width = |s: &str| s.chars().map(char_width).sum::<usize>();
    // Inner width: the widest line plus a space either side, within the terminal.
    let inner = body
        .iter()
        .map(|l| text_width(l) + 2)
        .chain([TITLE.len() + 2])
        .max()
        .unwrap_or(0)
        .min(width.saturating_sub(2));
    fn dim(out: &mut impl Write, s: &str) -> io::Result<()> {
        queue!(
            out,
            SetForegroundColor(Color::DarkGrey),
            Print(s),
            ResetColor
        )
    }
    dim(out, &format!("\r\n╭{}╮", "─".repeat(inner)))?;
    for line in &body {
        let shown = truncate(line, inner.saturating_sub(2));
        let pad = " ".repeat(inner.saturating_sub(text_width(&shown) + 1));
        dim(out, "\r\n│")?;
        queue!(out, Print(" "), Print(&shown), Print(pad))?;
        dim(out, "│")?;
    }
    let fill = inner.saturating_sub(TITLE.len() + 1);
    dim(out, &format!("\r\n╰─{TITLE}{}╯", "─".repeat(fill)))?;
    Ok(body.len() as u16 + 2)
}

fn render_row(
    out: &mut impl Write,
    text: &str,
    hl: &[u32],
    selected: bool,
    width: usize,
) -> io::Result<()> {
    let marker = if selected { "› " } else { "  " };
    if selected {
        queue!(
            out,
            SetForegroundColor(Color::Cyan),
            SetAttribute(Attribute::Bold)
        )?;
    }
    out.queue(Print(marker))?;
    let mut used = 2;
    let mut hl = hl.iter().peekable();
    for (i, c) in text.chars().enumerate() {
        let cw = char_width(c);
        if used + cw > width {
            break;
        }
        let on = hl.next_if_eq(&&(i as u32)).is_some();
        if on {
            queue!(out, SetForegroundColor(Color::Yellow), Print(c))?;
            out.queue(if selected {
                SetForegroundColor(Color::Cyan)
            } else {
                SetForegroundColor(Color::Reset)
            })?;
        } else {
            out.queue(Print(c))?;
        }
        used += cw;
    }
    queue!(out, SetAttribute(Attribute::Reset), ResetColor)?;
    Ok(())
}

impl Ui for TermUi {
    fn ask(&mut self, spec: &Spec) -> io::Result<Answer> {
        let mut ed = Editor {
            spec,
            input: Vec::new(),
            cursor: 0,
            searcher: &mut self.searcher,
            selected: None,
            scroll: 0,
        };
        ed.set_input(&spec.initial);
        ed.refresh();
        ed.render(&mut self.out)?;
        loop {
            let mut edited = false;
            let mut ev = Some(event::read()?);
            // Drain everything already queued (fast typing, pastes) before searching/redrawing.
            while let Some(e) = ev {
                if let Event::Key(k) = e
                    && k.kind != KeyEventKind::Release
                {
                    match ed.key(k) {
                        Outcome::Done(answer) => {
                            let shown = match &answer {
                                Answer::Text(t) | Answer::Accept(t) => t.clone(),
                                _ => ed.text(),
                            };
                            queue!(self.out, MoveToColumn(0), Clear(ClearType::FromCursorDown))?;
                            queue!(
                                self.out,
                                SetAttribute(Attribute::Bold),
                                Print(spec.prompt()),
                                SetAttribute(Attribute::Reset)
                            )?;
                            queue!(self.out, Print(shown), Print("\r\n"))?;
                            self.out.flush()?;
                            return Ok(answer);
                        }
                        Outcome::Continue { edited: e } => edited |= e,
                    }
                }
                ev = if event::poll(Duration::ZERO)? {
                    Some(event::read()?)
                } else {
                    None
                };
            }
            if edited {
                ed.refresh();
            }
            ed.render(&mut self.out)?;
        }
    }

    fn say(&mut self, text: &str) -> io::Result<()> {
        for line in text.lines() {
            queue!(self.out, MoveToColumn(0), Print(line), Print("\r\n"))?;
        }
        self.out.flush()
    }
}
