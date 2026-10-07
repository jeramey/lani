//! Persisted settings (docs/design.md §13.1). The only one is the web server's idle timeout.

use std::fmt;
use std::time::Duration;

use crate::error::{Error, Result};

/// The `setting` table key for the idle timeout.
pub const IDLE_TIMEOUT_KEY: &str = "idle_timeout";

/// The shortest idle timeout accepted, so a typo cannot stop the server almost immediately.
pub const MIN_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// How long the web server may sit without activity before stopping itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdleTimeout {
    Never,
    After(Duration),
}

impl IdleTimeout {
    /// Parse `never` or a whole number of minutes, hours or days: `30m`, `3h`, `2d`.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("never") {
            return Ok(Self::Never);
        }
        let invalid = || {
            Error::Invalid(format!(
                "invalid idle timeout {text:?}: use \"never\" or a whole number followed by m, h or d, like 30m, 3h or 2d"
            ))
        };
        let unit_start = text
            .find(|c: char| !c.is_ascii_digit())
            .ok_or_else(invalid)?;
        let (number, unit) = text.split_at(unit_start);
        let number: u64 = number.parse().map_err(|_| invalid())?;
        let seconds_per_unit = match unit {
            "m" => MINUTE,
            "h" => HOUR,
            "d" => DAY,
            _ => return Err(invalid()),
        };
        let duration =
            Duration::from_secs(number.checked_mul(seconds_per_unit).ok_or_else(invalid)?);
        if duration < MIN_IDLE_TIMEOUT {
            return Err(Error::Invalid(format!(
                "idle timeout {text:?} is too short; the minimum is 5m (or use \"never\")"
            )));
        }
        Ok(Self::After(duration))
    }
}

/// The canonical form, in the largest whole unit: `never`, `90m`, `3h`, `2d`.
impl fmt::Display for IdleTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Never => f.write_str("never"),
            Self::After(duration) => {
                let seconds = duration.as_secs();
                if seconds % DAY == 0 {
                    write!(f, "{}d", seconds / DAY)
                } else if seconds % HOUR == 0 {
                    write!(f, "{}h", seconds / HOUR)
                } else {
                    write!(f, "{}m", seconds / MINUTE)
                }
            }
        }
    }
}

/// A duration as hours and minutes for display, e.g. `2h 59m`.
#[must_use]
pub fn hours_minutes(duration: Duration) -> String {
    let minutes = duration.as_secs() / MINUTE;
    match (minutes / 60, minutes % 60) {
        (0, minutes) => format!("{minutes}m"),
        (hours, 0) => format!("{hours}h"),
        (hours, minutes) => format!("{hours}h {minutes}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units_and_never() {
        assert_eq!(IdleTimeout::parse("never").unwrap(), IdleTimeout::Never);
        assert_eq!(IdleTimeout::parse(" NEVER ").unwrap(), IdleTimeout::Never);
        assert_eq!(
            IdleTimeout::parse("30m").unwrap(),
            IdleTimeout::After(Duration::from_secs(30 * 60))
        );
        assert_eq!(
            IdleTimeout::parse("3h").unwrap(),
            IdleTimeout::After(Duration::from_secs(3 * 3600))
        );
        assert_eq!(
            IdleTimeout::parse("2d").unwrap(),
            IdleTimeout::After(Duration::from_secs(2 * 86400))
        );
    }

    #[test]
    fn rejects_bad_and_short_values() {
        for bad in [
            "",
            "3",
            "h",
            "3x",
            "3 h",
            "-3h",
            "1.5h",
            "4m",
            "99999999999999999999d",
        ] {
            assert!(
                IdleTimeout::parse(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn displays_in_the_largest_whole_unit() {
        for text in ["never", "90m", "3h", "2d", "5m"] {
            assert_eq!(IdleTimeout::parse(text).unwrap().to_string(), text);
        }
        assert_eq!(IdleTimeout::parse("180m").unwrap().to_string(), "3h");
        assert_eq!(IdleTimeout::parse("48h").unwrap().to_string(), "2d");
    }

    #[test]
    fn hours_and_minutes() {
        assert_eq!(hours_minutes(Duration::from_secs(59)), "0m");
        assert_eq!(hours_minutes(Duration::from_secs(45 * 60)), "45m");
        assert_eq!(hours_minutes(Duration::from_secs(3 * 3600)), "3h");
        assert_eq!(hours_minutes(Duration::from_secs(3 * 3600 - 60)), "2h 59m");
    }
}
