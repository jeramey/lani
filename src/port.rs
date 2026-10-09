//! Choosing the web server's port (docs/design.md §13.1): a base port from the configuration file
//! (or the default) and the ports above it, or exactly the port given with `--port`.

use std::num::NonZeroU16;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::paths;

/// The base port when the configuration file sets none: "LANI" on a phone keypad.
pub const DEFAULT_PORT: NonZeroU16 = NonZeroU16::new(5264).unwrap();

/// How many ports the server tries, counting up from the base port. Jupyter tries as many.
pub const PORT_TRIES: usize = 50;

/// Ports that browsers refuse to connect to: the "bad ports" of the Fetch Standard
/// (<https://fetch.spec.whatwg.org/#port-blocking>), which Safari, Chrome and Firefox follow.
const BLOCKED_BY_BROWSERS: &[u16] = &[
    1, 7, 9, 11, 13, 15, 17, 19, 20, 21, 22, 23, 25, 37, 42, 43, 53, 69, 77, 79, 87, 95, 101, 102,
    103, 104, 109, 110, 111, 113, 115, 117, 119, 123, 135, 137, 139, 143, 161, 179, 389, 427, 465,
    512, 513, 514, 515, 526, 530, 531, 532, 540, 548, 554, 556, 563, 587, 601, 636, 989, 990, 993,
    995, 1719, 1720, 1723, 2049, 3659, 4045, 4190, 5060, 5061, 6000, 6566, 6665, 6666, 6667, 6668,
    6669, 6679, 6697, 10080,
];

#[must_use]
pub fn blocked_by_browsers(port: u16) -> bool {
    BLOCKED_BY_BROWSERS.contains(&port)
}

/// Which port the web server listens on, on 127.0.0.1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortChoice {
    /// Any free port, picked by the OS: `--port 0`, or `port = 0` in the configuration file.
    Any,
    /// Exactly this port, from `--port`. Starting fails if it is taken.
    Exact(NonZeroU16),
    /// The first free port from this base port up, from the configuration file or the default,
    /// trying up to [`PORT_TRIES`] ports and skipping those browsers refuse.
    From(NonZeroU16),
}

impl PortChoice {
    /// The choice for `--port <port>`.
    pub fn flag(port: u16) -> Result<Self> {
        match NonZeroU16::new(port) {
            None => Ok(Self::Any),
            Some(port) => {
                refuse_blocked(port.get(), "pass another --port")?;
                Ok(Self::Exact(port))
            }
        }
    }

    /// The choice when `--port` is not given: the configuration file's `port`, or
    /// [`DEFAULT_PORT`].
    pub fn configured(config: &Config) -> Result<Self> {
        match config.port.map(NonZeroU16::new) {
            None => Ok(Self::From(DEFAULT_PORT)),
            Some(None) => Ok(Self::Any),
            Some(Some(port)) => {
                let path = paths::config_path()?;
                refuse_blocked(
                    port.get(),
                    &format!("set another port in {}", path.display()),
                )?;
                Ok(Self::From(port))
            }
        }
    }

    /// The choice when `--port` is not given, from the configuration file on disk.
    pub fn from_config_file() -> Result<Self> {
        Self::configured(&Config::load()?)
    }

    /// The ports to try, in order; 0 asks the OS for any free port.
    #[must_use]
    pub fn candidates(self) -> Vec<u16> {
        match self {
            Self::Any => vec![0],
            Self::Exact(port) => vec![port.get()],
            Self::From(base) => (base.get()..=u16::MAX)
                .filter(|port| !blocked_by_browsers(*port))
                .take(PORT_TRIES)
                .collect(),
        }
    }
}

fn refuse_blocked(port: u16, instead: &str) -> Result<()> {
    if blocked_by_browsers(port) {
        return Err(Error::Invalid(format!(
            "browsers refuse to connect to port {port} (it is one of the Fetch Standard's \
             \"bad ports\"), so LANI cannot use it; {instead}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(port: u16) -> NonZeroU16 {
        NonZeroU16::new(port).unwrap()
    }

    #[test]
    fn counting_up_skips_ports_browsers_refuse_and_stops_at_the_last_port() {
        let from = PortChoice::From(port(6660)).candidates();
        assert_eq!(from.len(), PORT_TRIES);
        assert_eq!(&from[..6], [6660, 6661, 6662, 6663, 6664, 6670]);
        assert!(!from.contains(&6679) && !from.contains(&6697));
        assert_eq!(
            PortChoice::From(port(65534)).candidates(),
            [65534, 65535],
            "no wrapping around"
        );
        assert_eq!(PortChoice::Exact(port(7000)).candidates(), [7000]);
        assert_eq!(PortChoice::Any.candidates(), [0]);
    }

    #[test]
    fn the_flag_names_an_exact_port_or_any() {
        assert_eq!(PortChoice::flag(0).unwrap(), PortChoice::Any);
        assert_eq!(
            PortChoice::flag(7000).unwrap(),
            PortChoice::Exact(port(7000))
        );
        match PortChoice::flag(6667) {
            Err(Error::Invalid(message)) => assert!(
                message.contains("browsers refuse to connect to port 6667")
                    && message.ends_with("pass another --port"),
                "{message}"
            ),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn the_configured_port_is_a_base_or_any() {
        assert_eq!(
            PortChoice::configured(&Config::default()).unwrap(),
            PortChoice::From(DEFAULT_PORT)
        );
        assert!(!blocked_by_browsers(DEFAULT_PORT.get()));
        assert_eq!(
            PortChoice::configured(&Config { port: Some(0) }).unwrap(),
            PortChoice::Any
        );
        assert_eq!(
            PortChoice::configured(&Config { port: Some(7000) }).unwrap(),
            PortChoice::From(port(7000))
        );
        match PortChoice::configured(&Config { port: Some(6667) }) {
            Err(Error::Invalid(message)) => assert!(
                message.contains("browsers refuse to connect to port 6667")
                    && message.contains("set another port in /")
                    && message.ends_with("config.toml"),
                "{message}"
            ),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }
}
