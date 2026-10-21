//! What the change page (inline diffs) and the diff page share when they
//! get a diff: cache first, the network only for a miss. Diffs are always
//! whole files; folding unchanged lines is the view's business.

use model::{Diff, DiffContext, DiffKey};
use sync::Syncer;

use crate::session::is_unauthorized;

/// Why a diff could not be shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    Unauthorized,
    Failed(String),
}

impl LoadError {
    fn from_sync(e: &sync::Error) -> Self {
        if is_unauthorized(e) {
            Self::Unauthorized
        } else {
            Self::Failed(e.to_string())
        }
    }
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => f.write_str("unauthorized"),
            Self::Failed(m) => f.write_str(m),
        }
    }
}

/// The diff for `key`. Diffs are keyed by revision SHA and immutable, so a
/// cached one is final and nothing is requested. Blocking (SQLite, network):
/// call from the background executor.
pub async fn load_diff(syncer: Syncer, key: DiffKey, can_sync: bool) -> Result<Diff, LoadError> {
    match syncer.cached_diff(&key) {
        Ok(Some(cached)) => return Ok(cached.diff),
        Ok(None) => {}
        Err(e) => log::warn!("reading cached diff of {}: {e}", key.path),
    }
    if !can_sync {
        return Err(LoadError::Failed(
            "not cached, and not signed in yet".into(),
        ));
    }
    syncer
        .refresh_diff(&key)
        .await
        .map_err(|e| LoadError::from_sync(&e))
}

/// How a context reads in the UI.
pub fn context_label(c: DiffContext) -> String {
    match c {
        DiffContext::Lines(n) => format!("{n} lines"),
        DiffContext::All => "whole file".to_owned(),
    }
}
