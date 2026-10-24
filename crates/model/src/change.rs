use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Serialize};

use crate::{
    Account, AccountId, Actions, ChangeMessage, ChangeNumber, LabelValue, PatchSetNumber, Revision,
    RevisionId, SubmitRequirement, Timestamp,
};

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
    /// Whether the change merges cleanly into its branch (Gerrit computes it
    /// for a single change unless `SKIP_MERGEABLE` is given). `None` for
    /// merged changes, for changes not tested yet, and in caches from before
    /// this field.
    #[serde(default)]
    pub mergeable: Option<bool>,
}

/// One reviewer's vote on a label (`ApprovalInfo`, `DETAILED_LABELS`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelVote {
    pub account: Account,
    pub value: i32,
    pub date: Option<Timestamp>,
}

/// Everything the change page shows beyond a list row.
///
/// The collections added after the first release are `#[serde(default)]` so
/// details cached by an older build still load.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeDetail {
    pub summary: ChangeSummary,
    /// Commit SHA of the current patch set.
    pub current_revision: Option<RevisionId>,
    /// Patch set number of the current patch set.
    pub current_patch_set: Option<PatchSetNumber>,
    /// Full commit message of the current patch set.
    pub commit_message: Option<String>,
    pub reviewers: Vec<Account>,
    pub cc: Vec<Account>,
    /// All patch sets, ascending by number (`ALL_REVISIONS`).
    #[serde(default)]
    pub revisions: Vec<Revision>,
    /// The change log, oldest first (`MESSAGES`).
    #[serde(default)]
    pub messages: Vec<ChangeMessage>,
    #[serde(default)]
    pub submit_requirements: Vec<SubmitRequirement>,
    /// Votes per label name (`DETAILED_LABELS`); labels nobody voted on
    /// have an empty list.
    #[serde(default)]
    pub votes: BTreeMap<String, Vec<LabelVote>>,
    /// Who is in the attention set and why (`DETAILED_ACCOUNTS`). The
    /// summary lists the same accounts by id only.
    #[serde(default)]
    pub attention: Vec<AttentionEntry>,
    /// The vote values the signed-in user may set per label, ascending
    /// (`permitted_labels`, `DETAILED_LABELS`). Empty for anonymous
    /// sessions and for labels the user may not vote on.
    #[serde(default)]
    pub permitted_labels: BTreeMap<String, Vec<i32>>,
    /// What each value of a label means, ascending by value
    /// (`labels[].values`), for the reply dialog.
    #[serde(default)]
    pub label_values: BTreeMap<String, Vec<LabelValue>>,
    /// What the user may do with the change (`CHANGE_ACTIONS`): abandon,
    /// restore, wip, ready, private, revert, ... Empty for anonymous
    /// sessions.
    #[serde(default)]
    pub change_actions: Actions,
    /// What the user may do with the current patch set (`CURRENT_ACTIONS`):
    /// submit, rebase, cherrypick, ... Empty for anonymous sessions.
    #[serde(default)]
    pub revision_actions: Actions,
}

/// One account in a change's attention set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionEntry {
    pub account: Account,
    pub reason: Option<String>,
    pub last_update: Option<Timestamp>,
}

impl ChangeDetail {
    /// The patch set with this number.
    pub fn revision(&self, number: PatchSetNumber) -> Option<&Revision> {
        self.revisions.iter().find(|r| r.number == number)
    }

    /// The current patch set, if the revisions are present.
    pub fn current(&self) -> Option<&Revision> {
        let sha = self.current_revision.as_ref()?;
        self.revisions.iter().find(|r| &r.sha == sha)
    }

    /// The action with this id, from the revision actions first, then the
    /// change actions. `None` if the server does not offer it at all; check
    /// [`ActionInfo::enabled`](crate::ActionInfo::enabled) before running it.
    pub fn action(&self, id: &str) -> Option<&crate::ActionInfo> {
        self.revision_actions
            .get(id)
            .or_else(|| self.change_actions.get(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn revision(n: u32, sha: &str) -> Revision {
        Revision {
            number: PatchSetNumber(n),
            sha: sha.into(),
            kind: None,
            created: None,
            uploader: None,
            ref_name: None,
            description: None,
            commit: None,
        }
    }

    fn detail() -> ChangeDetail {
        ChangeDetail {
            summary: ChangeSummary {
                number: ChangeNumber(1),
                change_id: None,
                project: "p".into(),
                branch: "b".into(),
                subject: "s".into(),
                status: ChangeStatus::New,
                owner: Account {
                    id: AccountId(1),
                    name: None,
                    email: None,
                    username: None,
                    display_name: None,
                },
                created: "2024-01-01T00:00:00Z".parse().unwrap(),
                updated: "2024-01-01T00:00:00Z".parse().unwrap(),
                insertions: 0,
                deletions: 0,
                labels: BTreeMap::new(),
                work_in_progress: false,
                is_private: false,
                unresolved_comment_count: 0,
                total_comment_count: 0,
                topic: None,
                hashtags: vec![],
                attention_set: vec![],
                submittable: None,
                mergeable: None,
            },
            current_revision: None,
            current_patch_set: None,
            commit_message: None,
            reviewers: vec![],
            cc: vec![],
            revisions: vec![],
            messages: vec![],
            submit_requirements: vec![],
            votes: BTreeMap::new(),
            attention: vec![],
            permitted_labels: BTreeMap::new(),
            label_values: BTreeMap::new(),
            change_actions: BTreeMap::new(),
            revision_actions: BTreeMap::new(),
        }
    }

    #[test]
    fn old_cached_detail_without_new_fields_still_loads() {
        let mut v = serde_json::to_value(detail()).unwrap();
        v["current_patch_set"] = 2.into();
        let obj = v.as_object_mut().unwrap();
        for key in [
            "revisions",
            "messages",
            "submit_requirements",
            "votes",
            "attention",
            "permitted_labels",
            "label_values",
            "change_actions",
            "revision_actions",
        ] {
            obj.remove(key);
        }
        let d: ChangeDetail = serde_json::from_value(v).unwrap();
        assert_eq!(d.current_patch_set, Some(PatchSetNumber(2)));
        assert!(d.revisions.is_empty() && d.messages.is_empty() && d.votes.is_empty());
        assert!(d.attention.is_empty() && d.permitted_labels.is_empty());
    }

    #[test]
    fn action_lookup_prefers_the_revision_and_keeps_unknown_ids() {
        use crate::ActionInfo;
        let act = |label: &str| ActionInfo {
            label: Some(label.into()),
            enabled: true,
            ..Default::default()
        };
        let mut d = detail();
        d.change_actions.insert("abandon".into(), act("Abandon"));
        d.change_actions.insert("x".into(), act("change"));
        d.revision_actions.insert("x".into(), act("revision"));
        d.revision_actions.insert("plugin~thing".into(), act("P"));
        assert_eq!(
            d.action("abandon").unwrap().label.as_deref(),
            Some("Abandon")
        );
        assert_eq!(d.action("x").unwrap().label.as_deref(), Some("revision"));
        assert!(d.action("plugin~thing").is_some() && d.action("submit").is_none());
        // `enabled` is absent when false.
        let a: ActionInfo = serde_json::from_str(r#"{"method":"POST"}"#).unwrap();
        assert!(!a.enabled && a.enabled_options.is_empty());
    }

    #[test]
    fn revision_lookup() {
        let mut d = detail();
        d.revisions = vec![revision(1, "aaa"), revision(2, "bbb")];
        d.current_revision = Some("bbb".into());
        assert_eq!(
            d.revision(PatchSetNumber(1)).map(|r| r.sha.as_str()),
            Some("aaa")
        );
        assert_eq!(d.current().map(|r| r.number), Some(PatchSetNumber(2)));
        assert!(d.revision(PatchSetNumber(9)).is_none());
    }

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
