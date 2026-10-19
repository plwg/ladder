//! Fixed-point decimals and hledger-style amounts (`$-10.00`, `5 EUR`, `"ACME 1" 3 @ $2`).

use rustc_hash::FxHashMap;
use std::fmt::Write as _;

const MAX_SCALE: u32 = 18;

/// A decimal number stored as `mant * 10^-scale`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Decimal {
    mant: i128,
    scale: u32,
}

impl PartialEq for Decimal {
    fn eq(&self, other: &Self) -> bool {
        let s = self.scale.max(other.scale);
        self.rescaled(s) == other.rescaled(s)
    }
}

impl Decimal {
    pub const ZERO: Decimal = Decimal { mant: 0, scale: 0 };

    pub fn new(mant: i128, scale: u32) -> Self {
        Decimal { mant, scale }
    }

    fn rescaled(self, scale: u32) -> i128 {
        self.mant * 10i128.pow(scale - self.scale)
    }

    pub fn scale(self) -> u32 {
        self.scale
    }

    pub fn is_zero(self) -> bool {
        self.mant == 0
    }

    pub fn is_negative(self) -> bool {
        self.mant < 0
    }

    pub fn abs(self) -> Self {
        Decimal {
            mant: self.mant.abs(),
            scale: self.scale,
        }
    }

    /// Drop trailing fractional zeros.
    pub fn normalize(mut self) -> Self {
        while self.scale > 0 && self.mant % 10 == 0 {
            self.mant /= 10;
            self.scale -= 1;
        }
        self
    }

    /// Round half away from zero to at most `places` fractional digits.
    pub fn round(self, places: u32) -> Self {
        if self.scale <= places {
            return self;
        }
        let div = 10i128.pow(self.scale - places);
        let (q, r) = (self.mant / div, self.mant % div);
        let q = if r.abs() * 2 >= div {
            q + self.mant.signum()
        } else {
            q
        };
        Decimal {
            mant: q,
            scale: places,
        }
    }
}

impl std::ops::Neg for Decimal {
    type Output = Decimal;
    fn neg(self) -> Decimal {
        Decimal {
            mant: -self.mant,
            scale: self.scale,
        }
    }
}

impl std::ops::Add for Decimal {
    type Output = Decimal;
    fn add(self, o: Decimal) -> Decimal {
        let s = self.scale.max(o.scale);
        Decimal {
            mant: self.rescaled(s) + o.rescaled(s),
            scale: s,
        }
    }
}

impl std::ops::Mul for Decimal {
    type Output = Decimal;
    fn mul(self, o: Decimal) -> Decimal {
        Decimal {
            mant: self.mant * o.mant,
            scale: self.scale + o.scale,
        }
        .round(MAX_SCALE)
        .normalize()
    }
}

/// How a commodity is displayed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Style {
    pub left: bool,
    pub spaced: bool,
    pub precision: u32,
    pub decimal_mark: char,
    pub thousands: Option<char>,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            left: false,
            spaced: true,
            precision: 0,
            decimal_mark: '.',
            thousands: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Amount {
    pub qty: Decimal,
    pub commodity: String,
}

/// An amount as written, along with the display style it was written in.
#[derive(Clone, Debug)]
pub struct Parsed {
    pub amount: Amount,
    pub style: Style,
}

/// Commodity styles observed in a journal; precision is the max seen.
#[derive(Default, Debug, Clone)]
pub struct Styles(FxHashMap<String, Style>);

impl Styles {
    pub fn observe(&mut self, commodity: &str, style: &Style) {
        match self.0.get_mut(commodity) {
            Some(s) => {
                s.precision = s.precision.max(style.precision);
                if s.thousands.is_none() {
                    s.thousands = style.thousands;
                }
            }
            None => {
                self.0.insert(commodity.to_owned(), style.clone());
            }
        }
    }

    pub fn get(&self, commodity: &str) -> Option<&Style> {
        self.0.get(commodity)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Style)> {
        self.0.iter().map(|(c, s)| (c.as_str(), s))
    }

    pub fn format(&self, a: &Amount) -> String {
        let fallback = Style::default();
        format_amount(a, self.get(&a.commodity).unwrap_or(&fallback))
    }
}

fn is_commodity_char(c: char) -> bool {
    !(c.is_ascii_digit()
        || c.is_whitespace()
        || matches!(
            c,
            '-' | '+' | '.' | ',' | ';' | '@' | '=' | '"' | '(' | ')' | '*' | '/'
        ))
}

fn needs_quotes(commodity: &str) -> bool {
    commodity.chars().any(|c| !is_commodity_char(c))
}

/// Parse a leading commodity symbol; returns (commodity, bytes consumed).
fn take_commodity(s: &str) -> Option<(&str, usize)> {
    if let Some(rest) = s.strip_prefix('"') {
        let end = rest.find('"')?;
        return Some((&rest[..end], end + 2));
    }
    let end = s.find(|c| !is_commodity_char(c)).unwrap_or(s.len());
    (end > 0).then(|| (&s[..end], end))
}

struct Number {
    qty: Decimal,
    precision: u32,
    decimal_mark: Option<char>,
    thousands: Option<char>,
}

/// Parse a number made of digits and `.`/`,` separators; returns it and bytes consumed.
fn take_number(s: &str, decimal_hint: Option<char>) -> Option<(Number, usize)> {
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == ','))
        .unwrap_or(s.len());
    let raw = &s[..end];
    if !raw.bytes().any(|b| b.is_ascii_digit()) {
        return None;
    }
    let last_dot = raw.rfind('.');
    let last_comma = raw.rfind(',');
    let decimal_mark = match (last_dot, last_comma) {
        (Some(d), Some(c)) => Some(if d > c { '.' } else { ',' }),
        (None, None) => None,
        (Some(_), None) | (None, Some(_)) => {
            let mark = if last_dot.is_some() { '.' } else { ',' };
            let pos = last_dot.or(last_comma).unwrap_or(0);
            let count = raw.matches(mark).count();
            let digits_after = raw.len() - pos - 1;
            if count > 1 {
                None
            } else if decimal_hint == Some(mark) {
                Some(mark)
            } else if mark == ',' && digits_after == 3 && decimal_hint != Some(',') {
                None
            } else {
                Some(mark)
            }
        }
    };
    let thousands = raw
        .chars()
        .find(|&c| (c == '.' || c == ',') && Some(c) != decimal_mark);
    let (int_part, frac_part) = match decimal_mark.and_then(|m| raw.rfind(m)) {
        Some(p) => (&raw[..p], &raw[p + 1..]),
        None => (raw, ""),
    };
    if frac_part.len() as u32 > MAX_SCALE || int_part.len() > 20 {
        return None;
    }
    let mut mant: i128 = 0;
    for b in int_part.bytes().chain(frac_part.bytes()) {
        if b.is_ascii_digit() {
            mant = mant * 10 + (b - b'0') as i128;
        }
    }
    let precision = frac_part.len() as u32;
    let n = Number {
        qty: Decimal::new(mant, precision),
        precision,
        decimal_mark,
        thousands,
    };
    Some((n, end))
}

/// Parse one amount from the start of `s`, returning it and the unparsed remainder.
pub fn parse_amount<'a>(s: &'a str, styles: Option<&Styles>) -> Option<(Parsed, &'a str)> {
    let (qty, commodity, style, rest) = scan_amount(s, styles)?;
    Some((
        Parsed {
            amount: Amount {
                qty,
                commodity: commodity.to_owned(),
            },
            style,
        },
        rest,
    ))
}

/// Allocation-free amount parse: (quantity, commodity, style, remainder).
pub fn scan_amount<'a>(
    s: &'a str,
    styles: Option<&Styles>,
) -> Option<(Decimal, &'a str, Style, &'a str)> {
    let mut rest = s.trim_start();
    let mut negative = false;
    let mut take_sign = |rest: &mut &str| {
        if let Some(r) = rest.strip_prefix('-') {
            negative = !negative;
            *rest = r.trim_start();
        } else if let Some(r) = rest.strip_prefix('+') {
            *rest = r.trim_start();
        }
    };
    take_sign(&mut rest);
    let mut style = Style::default();
    let mut commodity = "";
    if !rest.starts_with(|c: char| c.is_ascii_digit() || c == '.' || c == ',') {
        let (c, n) = take_commodity(rest)?;
        commodity = c;
        style.left = true;
        let after = &rest[n..];
        rest = after.trim_start();
        style.spaced = rest.len() != after.len();
        take_sign(&mut rest);
    }
    let hint = styles
        .and_then(|st| st.get(commodity))
        .map(|s| s.decimal_mark);
    let (num, n) = take_number(rest, hint)?;
    rest = &rest[n..];
    if !style.left {
        let trimmed = rest.trim_start();
        if let Some((c, n)) = take_commodity(trimmed) {
            style.spaced = trimmed.len() != rest.len();
            commodity = c;
            rest = &trimmed[n..];
        }
    }
    style.precision = num.precision;
    style.decimal_mark = num.decimal_mark.unwrap_or('.');
    style.thousands = num.thousands;
    let qty = if negative { -num.qty } else { num.qty };
    Some((qty, commodity, style, rest))
}

/// A posting amount: `AMOUNT [@ UNITPRICE | @@ TOTALPRICE] [= ASSERTION]`.
#[derive(Clone, Debug)]
pub struct PostingAmount {
    pub parsed: Parsed,
    /// What this posting contributes to the transaction balance (its cost, if priced).
    pub weight: Amount,
}

pub fn parse_posting_amount(s: &str, styles: Option<&Styles>) -> Result<PostingAmount, String> {
    let (parsed, rest) =
        parse_amount(s, styles).ok_or_else(|| format!("can't parse amount: {s}"))?;
    let rest = rest.trim_start();
    let mut weight = parsed.amount.clone();
    let rest = if let Some(r) = rest.strip_prefix('@') {
        let (total, r) = match r.strip_prefix('@') {
            Some(r) => (true, r),
            None => (false, r),
        };
        let (price, r) =
            parse_amount(r, styles).ok_or_else(|| format!("can't parse price: {s}"))?;
        let p = price.amount;
        weight = if total {
            let q = p.qty.abs();
            Amount {
                qty: if parsed.amount.qty.is_negative() {
                    -q
                } else {
                    q
                },
                commodity: p.commodity,
            }
        } else {
            Amount {
                qty: parsed.amount.qty * p.qty,
                commodity: p.commodity,
            }
        };
        r.trim_start()
    } else {
        rest
    };
    if !(rest.is_empty() || rest.starts_with('=')) {
        return Err(format!("unexpected text after amount: {rest}"));
    }
    Ok(PostingAmount { parsed, weight })
}

pub fn format_amount(a: &Amount, style: &Style) -> String {
    let precision = style.precision.max(a.qty.normalize().scale());
    let q = a.qty.round(precision);
    let digits = q.mant.unsigned_abs().to_string();
    let p = q.scale as usize;
    let digits = if digits.len() <= p {
        format!("{}{digits}", "0".repeat(p + 1 - digits.len()))
    } else {
        digits
    };
    let (int_part, frac_part) = digits.split_at(digits.len() - p);
    let mut num = String::with_capacity(digits.len() + 8);
    if q.mant < 0 {
        num.push('-');
    }
    match style.thousands {
        Some(sep) => {
            for (i, ch) in int_part.chars().enumerate() {
                if i > 0 && (int_part.len() - i) % 3 == 0 {
                    num.push(sep);
                }
                num.push(ch);
            }
        }
        None => num.push_str(int_part),
    }
    // pad to the style's precision (q may have fewer digits than style wants)
    let mut frac = frac_part.to_owned();
    while (frac.len() as u32) < precision {
        frac.push('0');
    }
    if !frac.is_empty() {
        num.push(style.decimal_mark);
        num.push_str(&frac);
    }
    let sym = if needs_quotes(&a.commodity) && !a.commodity.is_empty() {
        format!("\"{}\"", a.commodity)
    } else {
        a.commodity.clone()
    };
    let sp = if style.spaced && !sym.is_empty() {
        " "
    } else {
        ""
    };
    let mut out = String::new();
    if sym.is_empty() {
        out.push_str(&num);
    } else if style.left {
        let _ = write!(out, "{sym}{sp}{num}");
    } else {
        let _ = write!(out, "{num}{sp}{sym}");
    }
    out
}

/// A sum of amounts in possibly several commodities.
#[derive(Clone, Debug, Default)]
pub struct Mixed(pub Vec<Amount>);

impl Mixed {
    pub fn add(&mut self, a: &Amount) {
        match self.0.iter_mut().find(|x| x.commodity == a.commodity) {
            Some(x) => x.qty = x.qty + a.qty,
            None => self.0.push(a.clone()),
        }
    }

    /// Non-zero components, judged at display precision.
    pub fn nonzero<'a>(&'a self, styles: &'a Styles) -> impl Iterator<Item = &'a Amount> + 'a {
        self.0.iter().filter(move |a| {
            let p = styles
                .get(&a.commodity)
                .map_or(a.qty.scale(), |s| s.precision);
            !a.qty.round(p).is_zero()
        })
    }

    /// Balanced if zero, or exactly two commodities of opposite sign (implicit conversion).
    pub fn is_balanced(&self, styles: &Styles) -> bool {
        let nz: Vec<&Amount> = self.nonzero(styles).collect();
        match nz.as_slice() {
            [] => true,
            [a, b] => a.qty.is_negative() != b.qty.is_negative(),
            _ => false,
        }
    }

    pub fn display(&self, styles: &Styles) -> String {
        let parts: Vec<String> = self.nonzero(styles).map(|a| styles.format(a)).collect();
        if parts.is_empty() {
            "0".into()
        } else {
            parts.join(", ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Parsed {
        parse_amount(s, None).unwrap().0
    }

    fn roundtrip(s: &str) -> String {
        let x = p(s);
        format_amount(&x.amount, &x.style)
    }

    #[test]
    fn parses_and_formats() {
        for s in [
            "$10.00",
            "$-10.00",
            "10 EUR",
            "-3.5 EUR",
            "€1.234,56",
            "1,000.00 USD",
            "\"ACME 1\" 3",
            "42",
        ] {
            assert_eq!(roundtrip(s), s, "{s}");
        }
        assert_eq!(roundtrip("-$10.00"), "$-10.00");
        assert_eq!(roundtrip("$ 5"), "$ 5");
        assert_eq!(p("1,000").amount.qty, Decimal::new(1000, 0));
        assert_eq!(p("1,5").amount.qty, Decimal::new(15, 1));
    }

    #[test]
    fn prices() {
        let pa = parse_posting_amount("10 AAPL @ $2.50", None).unwrap();
        assert_eq!(
            pa.weight,
            Amount {
                qty: Decimal::new(25, 0),
                commodity: "$".into()
            }
        );
        let pa = parse_posting_amount("-10 AAPL @@ $30", None).unwrap();
        assert_eq!(
            pa.weight,
            Amount {
                qty: Decimal::new(-30, 0),
                commodity: "$".into()
            }
        );
        assert!(parse_posting_amount("$10 = $100", None).is_ok());
        assert!(parse_posting_amount("$10 junk", None).is_err());
    }

    #[test]
    fn balancing() {
        let st = Styles::default();
        let mut m = Mixed::default();
        m.add(&p("$10.00").amount);
        assert!(!m.is_balanced(&st));
        m.add(&p("$-10").amount);
        assert!(m.is_balanced(&st));
        m.add(&p("5 EUR").amount);
        m.add(&p("$-6").amount);
        assert!(m.is_balanced(&st));
    }

    #[test]
    fn rounding() {
        assert_eq!(Decimal::new(125, 2).round(1), Decimal::new(13, 1));
        assert_eq!(Decimal::new(-125, 2).round(1), Decimal::new(-13, 1));
        assert_eq!(Decimal::new(124, 2).round(1), Decimal::new(12, 1));
    }
}
