//! How past descriptions are shown. Spellings that differ only in case, like
//! `GROCERY STORE` and `Grocery store`, appear (and rank) as a single entry.

use std::str::FromStr;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Case {
    /// The most used spelling, as written.
    #[default]
    Usual,
    Title,
    Lower,
    Upper,
    /// As written in the journal; only exact duplicates are merged.
    Original,
}

impl Case {
    /// What spellings are grouped by: the same key means the same entry.
    pub fn key(self, s: &str) -> String {
        match self {
            Case::Original => s.to_owned(),
            _ => s.to_lowercase(),
        }
    }

    /// How a spelling is shown.
    pub fn apply(self, s: &str) -> String {
        match self {
            Case::Usual => s.to_owned(),
            Case::Title => title_case(s),
            Case::Lower => s.to_lowercase(),
            Case::Upper => s.to_uppercase(),
            Case::Original => s.to_owned(),
        }
    }
}

impl FromStr for Case {
    type Err = String;

    fn from_str(s: &str) -> Result<Case, String> {
        match s.to_ascii_lowercase().as_str() {
            "usual" => Ok(Case::Usual),
            "title" => Ok(Case::Title),
            "lower" => Ok(Case::Lower),
            "upper" => Ok(Case::Upper),
            "original" => Ok(Case::Original),
            _ => Err(format!(
                "unknown case {s:?} (expected usual, title, lower, upper or original)"
            )),
        }
    }
}

/// Capitalise the first letter of each word and lowercase the rest. A word starts
/// after whitespace or `( / - " &`, so `7-eleven` → `7-Eleven` but `joe's` → `Joe's`.
fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut start = true;
    for c in s.chars() {
        if start {
            out.extend(c.to_uppercase());
        } else {
            out.extend(c.to_lowercase());
        }
        start = c.is_whitespace() || matches!(c, '(' | '/' | '-' | '"' | '&');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_case() {
        assert_eq!(Case::Title.apply("GROCERY store"), "Grocery Store");
        assert_eq!(
            Case::Title.apply("joe's 7-eleven (main st)"),
            "Joe's 7-Eleven (Main St)"
        );
        assert_eq!(Case::Lower.apply("Coffee Shop"), "coffee shop");
        assert_eq!(Case::Upper.apply("Coffee Shop"), "COFFEE SHOP");
        assert_eq!(Case::Original.apply("CoFfee"), "CoFfee");
    }

    #[test]
    fn parses_names() {
        assert_eq!("Title".parse(), Ok(Case::Title));
        assert_eq!("usual".parse(), Ok(Case::Usual));
        assert_eq!("original".parse(), Ok(Case::Original));
        assert!("camel".parse::<Case>().is_err());
    }
}
