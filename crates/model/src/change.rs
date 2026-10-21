use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Serialize};

use crate::{Account, AccountId, ChangeNumber, Timestamp};

/// Lifecycle state of a change. Values this client does not know are kept
/// verbatim in [`ChangeStatus::Unknown`] instead of failing.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChangeStatus {
    New,
    Merged,
    Abandoned,
    Unknown(String),
}

impl ChangeStatus {
    /// Maps Gerrit's wire value (`NEW`, `MERGED`, `ABANDONED`).
    pub fn from_wire(s: &str) -> Self {
        match s {
            "NEW" => Self::New,
            "MERGED" => Self::Merged,
            "ABANDONED" => Self::Abandoned,
            other => Self::Unknown(other.to_owned()),
        }
    }
}

impl fmt::Display for ChangeStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::New => f.write_str("NEW"),
            Self::Merged => f.write_str("MERGED"),
            Self::Abandoned => f.write_str("ABANDONED"),
            Self::Unknown(s) => f.write_str(s),
        }
    }
}

/// One label's state as shown in a change list (Gerrit's `LABELS` option).
///
/// The four flags say whether Gerrit reported an account in the corresponding
/// slot: `approved` is the max vote, `rejected` the min vote, `recommended`
/// a positive and `disliked` a negative non-extreme vote. `value` is the
/// numeric vote when the server includes one (usually only for the caller's
/// own vote or with detailed labels).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelSummary {
    pub approved: bool,
    pub rejected: bool,
    pub recommended: bool,
    pub disliked: bool,
    pub value: Option<i32>,
}

/// What a search-result row needs to render.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSummary {
    pub number: ChangeNumber,
    /// The `Change-Id` footer (`I...`), if reported.
    pub change_id: Option<String>,
    pub project: String,
    pub branch: String,
    pub subject: String,
    pub status: ChangeStatus,
    pub owner: Account,
    pub created: Timestamp,
    pub updated: Timestamp,
    pub insertions: u32,
    pub deletions: u32,
    pub labels: BTreeMap<String, LabelSummary>,
    pub work_in_progress: bool,
    pub is_private: bool,
    pub unresolved_comment_count: u32,
    pub total_comment_count: u32,
    pub topic: Option<String>,
    pub hashtags: Vec<String>,
    /// Accounts whose turn it is, sorted by id.
    pub attention_set: Vec<AccountId>,
    pub submittable: Option<bool>,
}

/// Change metadata for the M1 placeholder change page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeDetail {
    pub summary: ChangeSummary,
    /// Commit SHA of the current patch set.
    pub current_revision: Option<String>,
    /// Patch set number of the current patch set.
    pub current_patch_set: Option<u32>,
    /// Full commit message of the current patch set.
    pub commit_message: Option<String>,
    pub reviewers: Vec<Account>,
    pub cc: Vec<Account>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_from_wire() {
        assert_eq!(ChangeStatus::from_wire("NEW"), ChangeStatus::New);
        assert_eq!(ChangeStatus::from_wire("MERGED"), ChangeStatus::Merged);
        assert_eq!(
            ChangeStatus::from_wire("ABANDONED"),
            ChangeStatus::Abandoned
        );
        let u = ChangeStatus::from_wire("DRAFT");
        assert_eq!(u, ChangeStatus::Unknown("DRAFT".into()));
        assert_eq!(u.to_string(), "DRAFT");
    }
}
