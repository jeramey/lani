//! Location of LANI's data directory (`$LANI_HOME`) and the files inside it, and of the
//! configuration file.

use std::fs::DirBuilder;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Resolve the data directory: `$LANI_HOME`, else `$XDG_DATA_HOME/lani`, else
/// `~/.local/share/lani`. Every candidate must be an absolute path.
pub fn lani_home() -> Result<PathBuf> {
    if let Some(home) = non_empty_env("LANI_HOME") {
        return require_absolute("LANI_HOME", PathBuf::from(home));
    }
    if let Some(xdg) = non_empty_env("XDG_DATA_HOME") {
        return require_absolute("XDG_DATA_HOME", PathBuf::from(xdg).join("lani"));
    }
    match non_empty_env("HOME") {
        Some(home) => require_absolute("HOME", PathBuf::from(home).join(".local/share/lani")),
        None => Err(Error::Invalid(
            "cannot locate the LANI data directory: set LANI_HOME or HOME".to_string(),
        )),
    }
}

/// Resolve the configuration file: `$XDG_CONFIG_HOME/lani/config.toml`, else
/// `~/.config/lani/config.toml`. `None` when neither variable is an absolute path: then there is
/// no configuration file, and the defaults apply. A relative path is ignored, as the XDG base
/// directory specification says.
#[must_use]
pub fn config_path() -> Option<PathBuf> {
    config_path_from(non_empty_env("XDG_CONFIG_HOME"), non_empty_env("HOME"))
}

fn config_path_from(xdg_config_home: Option<String>, home: Option<String>) -> Option<PathBuf> {
    let absolute =
        |value: Option<String>| value.map(PathBuf::from).filter(|path| path.is_absolute());
    absolute(xdg_config_home)
        .map(|xdg| xdg.join("lani/config.toml"))
        .or_else(|| absolute(home).map(|home| home.join(".config/lani/config.toml")))
}

/// The configuration file's path as messages show it, which tell the human where to change
/// something.
#[must_use]
pub fn shown_config_path() -> String {
    config_path().map_or_else(
        || "the configuration file (set HOME or XDG_CONFIG_HOME to use one)".to_string(),
        |path| path.display().to_string(),
    )
}

/// Create the data directory with mode 0700 if it does not exist.
///
/// An existing directory that other users can access is refused rather than silently changed,
/// because the database inside it holds your review text.
pub fn ensure_home(dir: &Path) -> Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let mode = std::fs::metadata(dir)?.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(Error::Invalid(format!(
            "{} is accessible by other users (mode {:o}); run `chmod 700 {}`",
            dir.display(),
            mode & 0o777,
            dir.display()
        )));
    }
    Ok(())
}

#[must_use]
pub fn db_path(home: &Path) -> PathBuf {
    home.join("lani.db")
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn require_absolute(variable: &str, path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(Error::Invalid(format!(
            "{variable} must be an absolute path, got {}",
            path.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configuration_file_follows_xdg_and_ignores_relative_paths() {
        let path = |xdg: Option<&str>, home: Option<&str>| {
            config_path_from(xdg.map(str::to_string), home.map(str::to_string))
        };
        assert_eq!(
            path(Some("/x"), Some("/h")),
            Some(PathBuf::from("/x/lani/config.toml"))
        );
        assert_eq!(
            path(None, Some("/h")),
            Some(PathBuf::from("/h/.config/lani/config.toml"))
        );
        assert_eq!(
            path(Some("relative"), Some("/h")),
            Some(PathBuf::from("/h/.config/lani/config.toml")),
            "a relative XDG_CONFIG_HOME is ignored"
        );
        assert_eq!(path(None, None), None);
        assert_eq!(path(Some("relative"), Some("also-relative")), None);
    }
}
