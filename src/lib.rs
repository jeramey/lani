//! LANI — Local Annotations, No Internet.
//!
//! A local replacement for the GitHub pull-request review loop between Claude Code sessions.
//! See `docs/design.md` for the design.

pub mod anchor;
pub mod base;
pub mod config;
pub mod daemon;
pub mod error;
pub mod executable;
pub mod git;
pub mod highlight;
pub mod listen;
pub mod markdown;
pub mod mcp;
pub mod model;
pub mod paths;
pub mod port;
pub mod prompt;
pub mod service;
pub mod settings;
pub mod store;
pub mod text;
pub mod time;
pub mod web;
