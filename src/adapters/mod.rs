pub mod claude;
pub mod codex;
pub mod cursor;
pub mod devin;
pub mod opencode;

use crate::model::{Agent, SessionRef, Turn};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// A discovery request: which directory, and an optional "modified since" cutoff.
/// The cutoff is enforced with a cheap filesystem stat *before* any file is opened,
/// so stale sessions are never read.
pub struct Query<'a> {
    pub cwd: &'a Path,
    pub since: Option<SystemTime>,
}

impl<'a> Query<'a> {
    /// True if `modified` passes the `since` cutoff (or no cutoff is set).
    pub fn recent_enough(&self, modified: SystemTime) -> bool {
        match self.since {
            Some(cut) => modified >= cut,
            None => true,
        }
    }
}

/// An adapter knows how to find and read one agent's local session logs.
pub trait Adapter {
    fn agent(&self) -> Agent;
    /// Sessions whose recorded cwd matches the query, with a cheap fingerprint scan.
    fn discover(&self, q: &Query) -> Result<Vec<SessionRef>>;
    /// Full transcript for a previously-discovered session.
    fn read(&self, session: &SessionRef, include_tools: bool) -> Result<Vec<Turn>>;
}

/// All adapters, optionally filtered to a single agent.
pub fn registry(only: Option<Agent>) -> Vec<Box<dyn Adapter>> {
    let all: Vec<Box<dyn Adapter>> = vec![
        Box::new(claude::ClaudeAdapter),
        Box::new(codex::CodexAdapter),
        Box::new(cursor::CursorAdapter),
        Box::new(devin::DevinAdapter),
        Box::new(opencode::OpenCodeAdapter),
    ];
    match only {
        Some(a) => all.into_iter().filter(|x| x.agent() == a).collect(),
        None => all,
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}
