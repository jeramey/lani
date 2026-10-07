//! Error type shared by every LANI module.
//!
//! The user-facing variants (`Invalid`, `NotFound`, `Forbidden`, `Conflict`) carry complete,
//! actionable messages: they are shown verbatim to Claude through MCP and to the human in the web
//! UI.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    /// The request cannot be carried out as given; the message says how to fix it.
    #[error("{0}")]
    Invalid(String),
    /// A referenced review, issue, comment, file or ref does not exist.
    #[error("{0}")]
    NotFound(String),
    /// The caller's role is not allowed to perform this action.
    #[error("{0}")]
    Forbidden(String),
    /// The text was changed by someone else since the caller read it.
    #[error("{0}")]
    Conflict(String),
    /// Something LANI depends on is unavailable: the web server could not be started, reached or
    /// stopped, or the `git` on PATH is missing or too old.
    #[error("{0}")]
    Unavailable(String),
    /// An internal failure that is not the caller's fault, such as a crashed background task.
    #[error("internal error: {0}")]
    Internal(String),
    /// A git command failed unexpectedly.
    #[error("git failed: {0}")]
    Git(String),
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
