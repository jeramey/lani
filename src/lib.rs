//! LANI — Local Annotations, No Internet.
//!
//! A local replacement for the GitHub pull-request review loop between Claude Code sessions.
//! See `docs/design.md` for the design.

pub mod anchor;
pub mod base;
pub mod error;
pub mod git;
pub mod mcp;
pub mod model;
pub mod paths;
pub mod service;
pub mod store;
pub mod time;
