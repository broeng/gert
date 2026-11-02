//! Reads a local git checkout through the `git` CLI and produces the same
//! [`model::FileEntry`] lists and [`model::Diff`]s that `gerrit-api` gets
//! from a Gerrit server, so a change's files can be shown without asking
//! the server when the checkout has the commits.
//!
//! It also finds the user's unpushed commits on local branches
//! ([`Repo::local_commits`]) for local changes.
//!
//! The repository is only ever read: no fetch, no checkout, no gc (see
//! [`git`] for the flags every invocation carries). Everything here is
//! **blocking** `std::process` work; callers run it on a background executor.
//!
//! A comparison that Gerrit computes in ways we cannot reproduce (merge
//! commits against their auto-merge, patch sets with different parents,
//! the synthetic `/COMMIT_MSG` file, ...) is not an error: it yields
//! [`Local::NotLocal`] and the caller asks Gerrit instead.

mod commits;
mod compare;
mod diff;
mod error;
mod files;
mod git;
mod intraline;

pub use compare::{BaseKind, CommitData, Comparison};
pub use error::{Error, NotLocal};

use std::path::Path;

use model::RevisionId;

use crate::git::Git;

/// The result of asking the checkout for something: the data, or the reason
/// it has to come from Gerrit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Local<T> {
    Available(T),
    NotLocal(NotLocal),
}

impl<T> Local<T> {
    /// The data, if the checkout could produce it.
    pub fn available(self) -> Option<T> {
        match self {
            Self::Available(v) => Some(v),
            Self::NotLocal(_) => None,
        }
    }
}

/// A git repository (work tree, linked worktree or bare) on disk.
#[derive(Clone, Debug)]
pub struct Repo {
    git: Git,
}

impl Repo {
    /// Checks with `git rev-parse` that `path` is inside a git repository
    /// and remembers it. Fails with [`Error::GitNotFound`] when there is no
    /// `git` binary, [`Error::NotARepository`] when the path is not a
    /// repository (or does not exist), and [`Error::Git`] for anything else
    /// (for example git refusing a repository owned by someone else).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        if !path.is_dir() {
            return Err(Error::NotARepository(path.to_owned()));
        }
        let git = Git::new(path.to_owned());
        let out = git.run_raw(&["rev-parse", "--git-dir"], None)?;
        if out.status.success() {
            return Ok(Self { git });
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("not a git repository") {
            return Err(Error::NotARepository(path.to_owned()));
        }
        Err(git::failure(&["rev-parse", "--git-dir"], &out))
    }

    /// The path the repository was opened with.
    pub fn path(&self) -> &Path {
        self.git.dir()
    }

    /// Whether the repository has this commit. Anything that is not a full
    /// hexadecimal object id is reported as absent.
    pub fn has_commit(&self, sha: &RevisionId) -> Result<bool, Error> {
        if !is_object_id(sha.as_str()) {
            return Ok(false);
        }
        let spec = format!("{}^{{commit}}", sha.as_str());
        let args = ["cat-file", "-e", spec.as_str()];
        let out = self.git.run_raw(&args, None)?;
        match out.status.code() {
            Some(0) => Ok(true),
            // 1: silently absent, 128: "Not a valid object name".
            Some(1 | 128) if !String::from_utf8_lossy(&out.stderr).contains("not a git repo") => {
                Ok(false)
            }
            _ => Err(git::failure(&args, &out)),
        }
    }
}

/// A full SHA-1 or SHA-256 object id. Checking this keeps anything that
/// looks like an option or a revision expression away from git.
pub(crate) fn is_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests;
#[cfg(any(test, feature = "test-support"))]
pub mod testutil;
