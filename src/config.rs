//! The configuration file (docs/design.md §13.1), found by [`paths::config_path`]. A missing file
//! means the defaults, and every key is optional. Settings the web UI changes are kept in the
//! database instead (§7.3).

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::listen::Listen;
use crate::paths;

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The port the web server tries first; 0 means any free port (§13.1). In socket mode, the
    /// local port the printed command and links suggest you forward from (§13.2).
    pub port: Option<u16>,
    /// How the web server listens: on a TCP port (the default) or a Unix socket (§13.2).
    pub listen: Option<Listen>,
}

impl Config {
    /// Read the configuration file, or the defaults if there is none or it cannot be located.
    pub fn load() -> Result<Self> {
        let Some(path) = paths::config_path() else {
            return Ok(Self::default());
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text).map_err(|message| {
                Error::Invalid(format!("error in {}: {message}", path.display()))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(Error::Invalid(format!(
                "cannot read {}: {error}",
                path.display()
            ))),
        }
    }

    /// Parse the text of a configuration file; the error says what is wrong and where.
    pub fn parse(text: &str) -> std::result::Result<Self, String> {
        toml::from_str(text).map_err(|error| error.to_string().trim_end().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_is_optional() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        assert_eq!(
            Config::parse("# comment\nport = 7000\n").unwrap(),
            Config {
                port: Some(7000),
                listen: None
            }
        );
        assert_eq!(
            Config::parse("listen = \"socket\"\n").unwrap().listen,
            Some(Listen::Socket)
        );
        assert_eq!(
            Config::parse("listen = \"tcp\"\n").unwrap().listen,
            Some(Listen::Tcp)
        );
    }

    #[test]
    fn mistakes_are_reported_not_ignored() {
        let unknown = Config::parse("prot = 7000").unwrap_err();
        assert!(unknown.contains("unknown field `prot`"), "{unknown}");
        let out_of_range = Config::parse("port = 70000").unwrap_err();
        assert!(out_of_range.contains("line 1"), "{out_of_range}");
        let not_a_number = Config::parse("port = \"7000\"").unwrap_err();
        assert!(not_a_number.contains("invalid type"), "{not_a_number}");
        let unknown_mode = Config::parse("listen = \"unix\"").unwrap_err();
        assert!(
            unknown_mode.contains("unknown variant `unix`, expected `tcp` or `socket`"),
            "{unknown_mode}"
        );
    }
}
