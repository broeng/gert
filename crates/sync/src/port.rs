//! What consolidation asks the local checkout when it ports the comments of
//! a local version that never reached Gerrit onto a Gerrit patch set.
//!
//! `sync` does not read git itself: [`PortSource`] is implemented by `ui`
//! over `local-git`, which keeps `sync`'s dependencies as they are. Its
//! calls are blocking (they start `git`); consolidation makes them on the
//! background executor it runs on, like its store calls.

use std::collections::BTreeMap;

use model::{LineMap, RevisionId};

/// A commit as the checkout has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortCommit {
    /// Its first parent; `None` for a root commit.
    pub parent: Option<RevisionId>,
}

/// What the checkout says about one path between two commits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathMap {
    /// Where the file's lines went, following a rename, or that it is gone
    /// ([`LineMap`]).
    Lines(LineMap),
    /// The checkout cannot map the path; comments on it are held.
    NotLocal(NotLocalPath),
}

/// Why the checkout cannot map a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotLocalPath {
    /// Not a regular file in `from`: deleted by `from` (a comment on it can
    /// only be on the file as a whole), or a submodule.
    NotAFile,
    /// The file is in both commits, but its content is not in the checkout
    /// (a partial clone).
    MissingContent,
    /// Anything else, as a sentence for the user.
    Other(String),
}

/// One [`PathMap`] per path asked about.
pub type PortMaps = BTreeMap<String, PathMap>;

/// Why a [`PortSource`] call has no answer at all.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PortError {
    /// No checkout of the project is configured or open (yet): nothing is
    /// ported now, and nothing is held for it.
    #[error("no checkout of the project is open")]
    NoCheckout,
    /// A commit is not in the checkout (never fetched, or removed by a
    /// `git gc` after an amend).
    #[error("commit {} is not in the checkout", .0.short(12))]
    MissingCommit(RevisionId),
    /// `git` failed; the next run tries again.
    #[error("{0}")]
    Git(String),
}

/// The local checkouts, as far as porting needs them. Blocking.
pub trait PortSource: Send + Sync {
    /// Commit `sha` in the checkout of `project`; `None` when the checkout
    /// does not have it.
    fn commit(&self, project: &str, sha: &RevisionId) -> Result<Option<PortCommit>, PortError>;

    /// How the lines of each of `paths` in commit `from` appear in commit
    /// `to`, with an entry for every path: one call per pair of commits.
    /// [`PortError::MissingCommit`] when either commit is not in the
    /// checkout.
    fn line_maps(
        &self,
        project: &str,
        from: &RevisionId,
        to: &RevisionId,
        paths: &[String],
    ) -> Result<PortMaps, PortError>;
}
