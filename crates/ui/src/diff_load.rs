//! What the change page (inline diffs) and the diff page share when they
//! get a diff: cache first, the network only for a miss. Diffs are always
//! whole files; folding unchanged lines is the view's business.

use model::{Diff, DiffContext, DiffKey, PATCHSET_LEVEL_PATH};
use sync::Syncer;

use crate::{
    local_source::LocalLazy,
    session::is_unauthorized,
    timing::{self, Steps},
};

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
/// load: when it cannot serve, the diff comes from Gerrit, except for a
/// local change or patch set, which only the checkout has. With `can_sync`
/// false (offline, not signed in) a local diff still shows. Blocking
/// (SQLite, git, network): call from the background executor. Logs its
/// steps' timing (see [`timing`]).
pub async fn load_diff(
    syncer: Syncer,
    key: DiffKey,
    can_sync: bool,
    local: LocalLazy,
) -> Result<(Diff, DiffSource), LoadError> {
    let mut steps = Steps::start();
    let result = load(syncer, &key, can_sync, local, &mut steps).await;
    log::debug!(
        target: timing::TARGET,
        "diff of {} in change {}: {} in {steps}",
        key.path,
        key.change,
        outcome(&result)
    );
    result
}

/// How a load ended, for the timing log.
pub fn outcome(result: &Result<(Diff, DiffSource), LoadError>) -> String {
    match result {
        Ok((_, source)) => format!("{source:?}"),
        Err(e) => format!("failed ({e})"),
    }
}

async fn load(
    syncer: Syncer,
    key: &DiffKey,
    can_sync: bool,
    local: LocalLazy,
    steps: &mut Steps,
) -> Result<(Diff, DiffSource), LoadError> {
    if key.path == PATCHSET_LEVEL_PATH {
        // Where patch set level comments live; not a file.
        return Err(LoadError::Failed(
            "patch set level comments have no diff".into(),
        ));
    }
    let cached = syncer.cached_diff(key);
    steps.lap("store");
    match cached {
        Ok(Some(cached)) => return Ok((cached.diff, DiffSource::Cache)),
        Ok(None) => {}
        Err(e) => log::warn!("reading cached diff of {}: {e}", key.path),
    }
    // Only now is the checkout needed (it may still be resolving), so a
    // stored diff never waits for git.
    let local = local.get().await;
    steps.lap("waiting for the checkout");
    if let Some(local) = local {
        let diff = local.diff(key);
        steps.lap("git");
        if let Some(diff) = diff {
            return Ok((diff, DiffSource::Local));
        }
    }
    if key.is_local() {
        // Gerrit does not have it, and must not be asked.
        return Err(LoadError::Failed(
            "the local checkout cannot show this diff".into(),
        ));
    }
    if !can_sync {
        return Err(LoadError::Failed(
            "not cached, and not signed in yet".into(),
        ));
    }
    let fetched = syncer
        .refresh_diff(key)
        .await
        .map(|d| (d, DiffSource::Gerrit))
        .map_err(|e| LoadError::from_sync(&e));
    steps.lap("gerrit");
    fetched
}

/// How a context reads in the UI.
pub fn context_label(c: DiffContext) -> String {
    match c {
        DiffContext::Lines(n) => format!("{n} lines"),
        DiffContext::All => "whole file".to_owned(),
    }
}
