//! Location of LANI's data directory (`$LANI_HOME`) and the files inside it.

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
