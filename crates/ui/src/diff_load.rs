//! What the change page (inline diffs) and the diff page share when they
//! get a diff: cache first, the network only for a miss. Diffs are always
//! whole files; folding unchanged lines is the view's business.

use model::{Diff, DiffContext, DiffKey};
use sync::Syncer;

use crate::{local_source::LocalLazy, session::is_unauthorized};

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

/// Where a loaded diff came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffSource {
    /// The store: what Gerrit sent earlier.
    Cache,
    /// Computed from the project's local checkout (never stored).
    Local,
    /// Just fetched from Gerrit (and stored).
    Gerrit,
}

/// The diff for `key` and where it came from, in this order: the store,
/// the local checkout (`local`, awaited only on a store miss), Gerrit.
///
/// Diffs are keyed by revision SHA and immutable, so a cached one is final
/// and nothing else is asked. A local diff is not written to the store, so
/// the cache only ever holds what Gerrit sent. The checkout never fails a
/// load: when it cannot serve, the diff comes from Gerrit. With `can_sync`
/// false (offline, not signed in) a local diff still shows. Blocking
/// (SQLite, git, network): call from the background executor.
pub async fn load_diff(
    syncer: Syncer,
    key: DiffKey,
    can_sync: bool,
    local: LocalLazy,
) -> Result<(Diff, DiffSource), LoadError> {
    match syncer.cached_diff(&key) {
        Ok(Some(cached)) => return Ok((cached.diff, DiffSource::Cache)),
        Ok(None) => {}
        Err(e) => log::warn!("reading cached diff of {}: {e}", key.path),
    }
    // Only now is the checkout needed (it may still be resolving), so a
    // stored diff never waits for git.
    if let Some(local) = local.get().await
        && let Some(diff) = local.diff(&key)
    {
        return Ok((diff, DiffSource::Local));
    }
    if !can_sync {
        return Err(LoadError::Failed(
            "not cached, and not signed in yet".into(),
        ));
    }
    syncer
        .refresh_diff(&key)
        .await
        .map(|d| (d, DiffSource::Gerrit))
        .map_err(|e| LoadError::from_sync(&e))
}

/// How a context reads in the UI.
pub fn context_label(c: DiffContext) -> String {
    match c {
        DiffContext::Lines(n) => format!("{n} lines"),
        DiffContext::All => "whole file".to_owned(),
    }
}
