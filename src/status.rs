//! The status mark written on added transactions.

use std::str::FromStr;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    Unmarked,
    /// `!`
    Pending,
    /// `*`
    Cleared,
}

impl Status {
    /// What goes between the date and the description.
    pub fn mark(self) -> &'static str {
        match self {
            Status::Unmarked => "",
            Status::Pending => "! ",
            Status::Cleared => "* ",
        }
    }
}

impl FromStr for Status {
    type Err = String;

    fn from_str(s: &str) -> Result<Status, String> {
        match s.to_ascii_lowercase().as_str() {
            "unmarked" | "none" => Ok(Status::Unmarked),
            "pending" | "!" => Ok(Status::Pending),
            "cleared" | "*" => Ok(Status::Cleared),
            _ => Err(format!(
                "unknown status {s:?} (expected unmarked, pending or cleared)"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_and_marks() {
        assert_eq!("Cleared".parse(), Ok(Status::Cleared));
        assert_eq!("*".parse(), Ok(Status::Cleared));
        assert_eq!("!".parse(), Ok(Status::Pending));
        assert_eq!("unmarked".parse(), Ok(Status::Unmarked));
        assert!("done".parse::<Status>().is_err());
    }
}
