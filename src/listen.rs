//! Where the web server listens (docs/design.md §13.1–§13.2): a TCP port on 127.0.0.1, or, in
//! socket mode, a Unix socket in the private run directory that you reach through an SSH forward.

use std::num::NonZeroU16;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::Config;
use crate::daemon;
use crate::error::{Error, Result};
use crate::paths;
use crate::port::PortChoice;

const SOCKET_FILE: &str = "web.sock";

/// How the server listens: `listen` in the configuration file, or `lani serve --listen`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Listen {
    /// A TCP port on 127.0.0.1, the default.
    Tcp,
    /// A Unix socket, for a LANI you reach over SSH (§13.2).
    Socket,
}

impl Listen {
    /// The value as the configuration file and `--listen` spell it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Socket => "socket",
        }
    }
}

/// The web server's socket in socket mode: `run/web.sock`.
#[must_use]
pub fn socket_path(home: &Path) -> PathBuf {
    daemon::run_dir(home).join(SOCKET_FILE)
}

/// Where a server is to listen, from the flags and the configuration file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Binding {
    /// A TCP port on 127.0.0.1.
    Tcp(PortChoice),
    /// The Unix socket at `path`. Nothing is bound at `browser_port`: it is the local port that
    /// the printed command and links suggest you forward from.
    Socket {
        path: PathBuf,
        browser_port: NonZeroU16,
    },
}

impl Binding {
    /// Where a server started now would listen. The configuration file is read only for what the
    /// flags leave open.
    pub fn choose(home: &Path, port: Option<PortChoice>, listen: Option<Listen>) -> Result<Self> {
        let config = match (port, listen) {
            // Nothing is left to the file, so a mistake in it doesn't matter here.
            (Some(_), Some(_)) => Config::default(),
            _ => Config::load()?,
        };
        Self::decide(home, port, listen, &config)
    }

    /// Where a server would listen, given the flags and the configuration.
    pub fn decide(
        home: &Path,
        port: Option<PortChoice>,
        listen: Option<Listen>,
        config: &Config,
    ) -> Result<Self> {
        let port = match port {
            Some(choice) => choice,
            None => PortChoice::configured(config)?,
        };
        match listen.or(config.listen).unwrap_or(Listen::Tcp) {
            Listen::Tcp => Ok(Self::Tcp(port)),
            Listen::Socket => {
                let browser_port = match port {
                    PortChoice::Exact(port) | PortChoice::From(port) => port,
                    PortChoice::Any => {
                        return Err(Error::Invalid(format!(
                            "in socket mode the port is the one your browser uses through an SSH \
                             forward, so it cannot be 0; set another port in {}, or pass --port",
                            paths::shown_config_path()
                        )));
                    }
                };
                let path = socket_path(home);
                check_socket_path(&path)?;
                Ok(Self::Socket { path, browser_port })
            }
        }
    }
}

/// Check that a socket can be made at `path` and forwarded to: the path fits in a Unix socket
/// address, the printed `ssh -L` command can name it, and nothing but a socket left over from an
/// earlier server is there.
fn check_socket_path(path: &Path) -> Result<()> {
    // The command is printed as text, and ssh expands `${…}` in socket paths with no escape for it
    // (§13.2).
    match path.to_str() {
        None => {
            return Err(Error::Invalid(format!(
                "the socket path {} is not UTF-8, so the ssh command to reach it can't be printed; \
                 set LANI_HOME to a directory whose path is",
                path.display()
            )));
        }
        Some(text) if text.contains("${") => {
            return Err(Error::Invalid(format!(
                "the socket path {text} contains \"${{\", which ssh would expand, so the server \
                 couldn't be reached through a forward; set LANI_HOME to a directory without it"
            )));
        }
        Some(_) => {}
    }
    if std::os::unix::net::SocketAddr::from_pathname(path).is_err() {
        return Err(Error::Invalid(format!(
            "the socket path {} is {} bytes, too long for a Unix socket, which allows 103 on macOS \
             and 107 on Linux; set LANI_HOME to a shorter directory",
            path.display(),
            path.as_os_str().len()
        )));
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_socket() => Err(Error::Unavailable(format!(
            "cannot listen on {}: something other than a socket is there; move it away",
            path.display()
        ))),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;

    use super::*;

    fn port(port: u16) -> NonZeroU16 {
        NonZeroU16::new(port).unwrap()
    }

    fn config(listen: Option<Listen>, port: Option<u16>) -> Config {
        Config { port, listen }
    }

    #[test]
    fn tcp_is_the_default_and_a_flag_overrides_the_file_either_way() {
        let home = Path::new("/home/you/.local/share/lani");
        let socket = Binding::Socket {
            path: home.join("run/web.sock"),
            browser_port: crate::port::DEFAULT_PORT,
        };
        let tcp = Binding::Tcp(PortChoice::From(crate::port::DEFAULT_PORT));
        let decide = |listen, config: &Config| Binding::decide(home, None, listen, config).unwrap();
        assert_eq!(decide(None, &Config::default()), tcp);
        assert_eq!(decide(None, &config(Some(Listen::Socket), None)), socket);
        assert_eq!(
            decide(Some(Listen::Tcp), &config(Some(Listen::Socket), None)),
            tcp
        );
        assert_eq!(decide(Some(Listen::Socket), &Config::default()), socket);
    }

    #[test]
    fn in_socket_mode_the_port_is_the_browsers_and_cannot_be_zero() {
        let home = Path::new("/home/you/.local/share/lani");
        let browser_port = |port_flag, file_port| match Binding::decide(
            home,
            port_flag,
            None,
            &config(Some(Listen::Socket), file_port),
        ) {
            Ok(Binding::Socket { browser_port, .. }) => Ok(browser_port.get()),
            Ok(other) => panic!("expected a socket, got {other:?}"),
            Err(error) => Err(error.to_string()),
        };
        assert_eq!(browser_port(None, Some(7000)), Ok(7000));
        assert_eq!(
            browser_port(Some(PortChoice::Exact(port(7100))), Some(7000)),
            Ok(7100)
        );
        for (flag, file) in [(None, Some(0)), (Some(PortChoice::Any), None)] {
            let refused = browser_port(flag, file).unwrap_err();
            assert!(
                refused.contains("in socket mode the port is the one your browser uses")
                    && refused.contains("cannot be 0"),
                "{refused}"
            );
        }
    }

    #[test]
    fn a_socket_path_must_fit_and_hold_nothing_but_a_leftover_socket() {
        let directory = tempfile::tempdir().unwrap();
        let long = directory.path().join("x".repeat(110));
        let too_long = check_socket_path(&long).unwrap_err().to_string();
        assert!(
            too_long.contains("too long for a Unix socket")
                && too_long.contains(&format!("is {} bytes", long.as_os_str().len()))
                && too_long.ends_with("set LANI_HOME to a shorter directory"),
            "{too_long}"
        );

        let expanded = directory.path().join("${HOME}").join("web.sock");
        let refused = check_socket_path(&expanded).unwrap_err().to_string();
        assert!(refused.contains("which ssh would expand"), "{refused}");
        let not_utf8 = directory
            .path()
            .join(std::ffi::OsStr::from_bytes(b"caf\xe9"))
            .join("web.sock");
        let refused = check_socket_path(&not_utf8).unwrap_err().to_string();
        assert!(refused.contains("is not UTF-8"), "{refused}");

        let path = directory.path().join("web.sock");
        check_socket_path(&path).unwrap();
        let leftover = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(leftover);
        assert!(path.exists(), "dropping a listener leaves its socket file");
        check_socket_path(&path).unwrap();

        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "not a socket").unwrap();
        let occupied = check_socket_path(&path).unwrap_err().to_string();
        assert!(
            occupied.contains("something other than a socket is there"),
            "{occupied}"
        );
    }
}
