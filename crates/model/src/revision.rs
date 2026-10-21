use serde::{Deserialize, Serialize};

use crate::{Account, PatchSetNumber, RevisionId, Timestamp};

/// How a patch set differs from its predecessor (`RevisionInfo.kind`).
/// Values this client does not know are kept in [`RevisionKind::Unknown`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RevisionKind {
    Rework,
    TrivialRebase,
    TrivialRebaseWithMessageUpdate,
    MergeFirstParentUpdate,
    NoCodeChange,
    NoChange,
    Unknown(String),
}

impl RevisionKind {
    /// Maps Gerrit's wire value (`REWORK`, `TRIVIAL_REBASE`, ...).
    pub fn from_wire(s: &str) -> Self {
        match s {
            "REWORK" => Self::Rework,
            "TRIVIAL_REBASE" => Self::TrivialRebase,
            "TRIVIAL_REBASE_WITH_MESSAGE_UPDATE" => Self::TrivialRebaseWithMessageUpdate,
            "MERGE_FIRST_PARENT_UPDATE" => Self::MergeFirstParentUpdate,
            "NO_CODE_CHANGE" => Self::NoCodeChange,
            "NO_CHANGE" => Self::NoChange,
            other => Self::Unknown(other.to_owned()),
        }
    }
}

/// The author or committer of a commit (`GitPerson`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitPerson {
    pub name: Option<String>,
    pub email: Option<String>,
    pub date: Option<Timestamp>,
    /// Time zone offset in minutes east of UTC.
    pub tz: Option<i32>,
}

/// A parent commit as listed in a commit: its SHA and, when the server
/// includes it, its subject.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitParent {
    pub sha: RevisionId,
    pub subject: Option<String>,
}

/// The git commit behind a patch set (`CommitInfo`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    /// Gerrit leaves this out for a revision's own commit (the revision key
    /// is the SHA) but sets it for parents and related changes.
    pub sha: Option<RevisionId>,
    pub parents: Vec<CommitParent>,
    pub author: Option<GitPerson>,
    pub committer: Option<GitPerson>,
    pub subject: String,
    /// The full message, subject included.
    pub message: Option<String>,
}

/// One patch set of a change (`RevisionInfo`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub number: PatchSetNumber,
    pub sha: RevisionId,
    pub kind: Option<RevisionKind>,
    pub created: Option<Timestamp>,
    pub uploader: Option<Account>,
    /// The git ref to fetch, e.g. `refs/changes/45/12345/2`.
    pub ref_name: Option<String>,
    /// The patch set description set by the uploader.
    pub description: Option<String>,
    /// Present with `ALL_COMMITS` / `CURRENT_COMMIT`.
    pub commit: Option<Commit>,
}

impl Revision {
    pub fn as_ref(&self) -> PatchSetRef {
        PatchSetRef {
            number: self.number,
            sha: self.sha.clone(),
        }
    }
}

/// A patch set by number and SHA. The number is what the REST API takes as
/// `base=`; the SHA is what caches key on.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PatchSetRef {
    pub number: PatchSetNumber,
    pub sha: RevisionId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_from_wire_keeps_unknown_values() {
        assert_eq!(
            RevisionKind::from_wire("TRIVIAL_REBASE"),
            RevisionKind::TrivialRebase
        );
        assert_eq!(
            RevisionKind::from_wire("SPLIT"),
            RevisionKind::Unknown("SPLIT".into())
        );
    }

    #[test]
    fn revision_as_ref() {
        let r = Revision {
            number: PatchSetNumber(2),
            sha: "abc".into(),
            kind: None,
            created: None,
            uploader: None,
            ref_name: None,
            description: None,
            commit: None,
        };
        assert_eq!(
            r.as_ref(),
            PatchSetRef {
                number: PatchSetNumber(2),
                sha: "abc".into()
            }
        );
    }
}
