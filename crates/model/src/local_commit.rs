//! Commits read from a local checkout (by `local-git`), as plain data so
//! that `sync` can classify them without depending on git.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{RevisionId, Timestamp};

/// The author or committer of a commit read from the checkout. Unlike
/// Gerrit's [`GitPerson`](crate::GitPerson), every field is there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    pub name: String,
    pub email: String,
    pub time: Timestamp,
    /// Time zone offset in minutes east of UTC.
    pub tz: i32,
}

/// A commit as stored in the checkout. Not to be confused with Gerrit's
/// `CommitInfo`, which is [`Commit`](crate::Commit) here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitInfo {
    pub sha: RevisionId,
    /// As stored in the commit object, first parent first.
    pub parents: Vec<RevisionId>,
    pub author: Signature,
    pub committer: Signature,
    /// The full message, subject included, as stored (read as UTF-8,
    /// lossily). It may lack a trailing newline.
    pub message: String,
    /// The `Change-Id` trailer: `I` and 40 hex digits from the message's
    /// last paragraph, the last one if there are several.
    pub change_id: Option<String>,
}

impl CommitInfo {
    /// The first line of the message.
    pub fn subject(&self) -> &str {
        self.message.lines().next().unwrap_or("").trim_end()
    }

    /// Whether this is a merge commit (more than one parent).
    pub fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }
}

/// A commit on a named local branch that no remote-tracking branch has.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalCommit {
    pub commit: CommitInfo,
    /// The local branches (`main`, not `refs/heads/main`) whose tips reach
    /// this commit, sorted by name.
    pub branches: Vec<String>,
}

impl LocalCommit {
    pub fn is_merge(&self) -> bool {
        self.commit.is_merge()
    }
}

/// One look at a checkout for the commits of local changes, everything
/// from the same read of its refs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalScan {
    /// A hash of the refs the scan saw. A later fingerprint that is equal
    /// means a new scan would find the same. Stable within one process run
    /// only, so never store it.
    pub fingerprint: u64,
    /// Newest first, never a parent before its children.
    pub commits: Vec<LocalCommit>,
    /// There were more local commits than the limit, and the oldest were
    /// left out. A change missing from such a scan may still be on a
    /// branch, so it must not be taken as vanished.
    pub truncated: bool,
    /// The checkout has no remote-tracking branches at all, so every commit
    /// would count as local, and nothing was scanned (`commits` is empty).
    /// Not a reason to take anything as vanished either.
    pub no_remotes: bool,
    pub targets: BranchTargets,
}

impl LocalScan {
    /// Whether `commits` is every local commit, so a change missing from it
    /// is really gone from the named branches.
    pub fn is_complete(&self) -> bool {
        !self.truncated && !self.no_remotes
    }
}

/// The branch on a remote that a local branch tracks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Upstream {
    pub remote: String,
    /// The branch name on the remote (`main`, not `refs/heads/main`).
    pub branch: String,
}

/// Where the commits of a checkout's local branches are headed: each
/// branch's upstream and each remote's default branch.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchTargets {
    /// Local branch name → its upstream. Branches without an upstream, or
    /// whose upstream is another local branch, are absent.
    pub upstreams: BTreeMap<String, Upstream>,
    /// Remote name → its default branch (from `refs/remotes/<remote>/HEAD`).
    pub defaults: BTreeMap<String, String>,
}

/// The remote whose default branch is used when no branch has an upstream
/// and there is more than one remote to choose from.
const PREFERRED_REMOTE: &str = "origin";

impl BranchTargets {
    /// The target branch for a commit contained in `branches` (see
    /// [`LocalCommit::branches`]): the upstream branch of the first
    /// containing branch, by name, that has one; otherwise the default
    /// branch of `origin`, or of the only remote that has a default;
    /// otherwise `None`.
    ///
    /// Taking the first branch by name is arbitrary but stable, so a commit
    /// shared by branches with different upstreams keeps its target from
    /// one scan to the next as long as its branches stay the same.
    pub fn target_for<S: AsRef<str>>(&self, branches: &[S]) -> Option<&str> {
        let mut sorted: Vec<&str> = branches.iter().map(AsRef::as_ref).collect();
        sorted.sort_unstable();
        if let Some(up) = sorted.iter().find_map(|b| self.upstreams.get(*b)) {
            return Some(&up.branch);
        }
        if let Some(default) = self.defaults.get(PREFERRED_REMOTE) {
            return Some(default);
        }
        match self.defaults.values().collect::<Vec<_>>().as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(remote: &str, branch: &str) -> Upstream {
        Upstream {
            remote: remote.into(),
            branch: branch.into(),
        }
    }

    fn targets(ups: &[(&str, Upstream)], defaults: &[(&str, &str)]) -> BranchTargets {
        BranchTargets {
            upstreams: ups
                .iter()
                .map(|(b, u)| ((*b).to_owned(), u.clone()))
                .collect(),
            defaults: defaults
                .iter()
                .map(|(r, b)| ((*r).to_owned(), (*b).to_owned()))
                .collect(),
        }
    }

    #[test]
    fn target_is_the_upstream_of_the_first_branch_by_name_with_one() {
        let t = targets(
            &[
                ("topic", up("origin", "main")),
                ("b-fix", up("origin", "stable")),
            ],
            &[("origin", "main")],
        );
        assert_eq!(t.target_for(&["topic"]), Some("main"));
        assert_eq!(t.target_for(&["topic", "b-fix"]), Some("stable"));
        assert_eq!(t.target_for(&["a-none", "topic"]), Some("main"));
    }

    #[test]
    fn target_falls_back_to_the_remote_default() {
        let t = targets(&[], &[("origin", "master"), ("other", "dev")]);
        assert_eq!(t.target_for(&["topic"]), Some("master"));
        let t = targets(&[], &[("gerrit", "dev")]);
        assert_eq!(t.target_for(&["topic"]), Some("dev"));
        let t = targets(&[], &[("a", "dev"), ("b", "main")]);
        assert_eq!(t.target_for(&["topic"]), None);
        assert_eq!(BranchTargets::default().target_for::<&str>(&[]), None);
    }

    #[test]
    fn subject_and_merge() {
        let sig = Signature {
            name: "A".into(),
            email: "a@example.com".into(),
            time: Timestamp::UNIX_EPOCH,
            tz: 0,
        };
        let mut c = CommitInfo {
            sha: "abc".into(),
            parents: vec!["p1".into()],
            author: sig.clone(),
            committer: sig,
            message: "Subject\r\n\r\nBody".into(),
            change_id: None,
        };
        assert_eq!(c.subject(), "Subject");
        assert!(!c.is_merge());
        c.parents.push("p2".into());
        assert!(c.is_merge());
    }
}
