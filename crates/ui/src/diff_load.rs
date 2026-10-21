//! What the change page (inline diffs) and the diff page share when they
//! get a diff: cache first, the network only for a miss, and the rule for
//! turning a skip-expansion request into a larger `context`.

use model::{Diff, DiffContext, DiffKey};
use sync::Syncer;

use crate::{diff::Expand, session::is_unauthorized};

/// Beyond this many context lines a request is just "the whole file".
const MAX_CONTEXT: u32 = 1000;

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

/// The `context` to request for an expansion request, given what is shown.
///
/// The server only sends `context=N` windows around the changes. The
/// context needed to show a hidden run of `n` lines is `N + n / 2` when
/// changes are on both sides (each side contributes `N` more lines), and
/// `N + n` at the start or end of the file. Asking for that reveals this run
/// and, as a side effect, grows every other hidden run by the same amount;
/// at [`MAX_CONTEXT`] or more it is simply the whole file.
pub fn expanded_context(current: DiffContext, expand: &Expand) -> DiffContext {
    let skip = match expand {
        Expand::All => return DiffContext::All,
        Expand::Skip(skip) => skip,
    };
    let DiffContext::Lines(n) = current else {
        return DiffContext::All;
    };
    let extra = if skip.at_start || skip.at_end {
        skip.lines
    } else {
        skip.lines.div_ceil(2)
    };
    match n.saturating_add(extra) {
        c if c >= MAX_CONTEXT => DiffContext::All,
        c => DiffContext::Lines(c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::SkipRow;

    fn skip(lines: u32, at_start: bool, at_end: bool) -> Expand {
        Expand::Skip(SkipRow {
            lines,
            old_start: 1,
            new_start: 1,
            at_start,
            at_end,
        })
    }

    #[test]
    fn all_is_all() {
        assert_eq!(
            expanded_context(DiffContext::Lines(3), &Expand::All),
            DiffContext::All
        );
    }

    #[test]
    fn a_middle_run_needs_half_its_length_more_context() {
        assert_eq!(
            expanded_context(DiffContext::Lines(10), &skip(40, false, false)),
            DiffContext::Lines(30)
        );
        assert_eq!(
            expanded_context(DiffContext::Lines(10), &skip(41, false, false)),
            DiffContext::Lines(31)
        );
    }

    #[test]
    fn a_run_at_an_end_needs_its_full_length() {
        assert_eq!(
            expanded_context(DiffContext::Lines(10), &skip(40, true, false)),
            DiffContext::Lines(50)
        );
        assert_eq!(
            expanded_context(DiffContext::Lines(10), &skip(40, false, true)),
            DiffContext::Lines(50)
        );
    }

    #[test]
    fn huge_runs_and_whole_file_context_become_all() {
        assert_eq!(
            expanded_context(DiffContext::Lines(10), &skip(5000, true, false)),
            DiffContext::All
        );
        assert_eq!(
            expanded_context(DiffContext::All, &skip(5, false, false)),
            DiffContext::All
        );
    }
}
