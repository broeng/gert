//! Changes that exist only in a local checkout so far, their versions, and
//! reviews of them that wait to be sent to Gerrit.
//!
//! A local change is the set of the user's unpushed commits with one
//! `Change-Id` (in one project, for one target branch). Each commit seen for
//! it is a local patch set. Both get numbers from reserved ranges
//! ([`ChangeNumber::LOCAL_BASE`], [`PatchSetNumber::LOCAL_BASE`]) so that
//! pages, drafts and history work on them like on server changes; those
//! numbers never go to a server. When the change shows up on Gerrit, its
//! drafts and pending reviews are moved onto the server's change and sent.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use serde::{Deserialize, Serialize};

use crate::{ChangeNumber, LocalApproval, PatchSetNumber, RevisionId, Timestamp};

/// The row id of a local change in the store. Ids are never reused, so
/// neither is the change's synthetic number ([`LocalChangeId::number`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LocalChangeId(pub i64);

impl LocalChangeId {
    /// The change's synthetic number, see [`ChangeNumber::local`].
    pub fn number(self) -> Option<ChangeNumber> {
        ChangeNumber::local(self.0)
    }
}

/// The row id of a local patch set in the store. Never reused, like
/// [`LocalChangeId`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LocalPatchSetId(pub i64);

impl LocalPatchSetId {
    /// The patch set's synthetic number, see [`PatchSetNumber::local`].
    pub fn number(self) -> Option<PatchSetNumber> {
        PatchSetNumber::local(self.0)
    }
}

/// The row id of a pending review in the store; ids grow with creation
/// order, which is the order reviews are sent in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PendingReviewId(pub i64);

impl fmt::Display for PendingReviewId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Where a local change stands relative to Gerrit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LocalChangeState {
    /// Seen in the checkout but not yet looked up on Gerrit while online,
    /// so it may be an old commit that Gerrit already has.
    Checking,
    /// Looked up: Gerrit does not have it (or, with `gerrit_number` set,
    /// these are local versions of an open Gerrit change).
    Local,
    /// The target branch is unknown and Gerrit has several changes with
    /// this `Change-Id` in the project, so it is never consolidated.
    Ambiguous,
    /// No longer on any named branch but kept because it has unsent data,
    /// until the next lookup consolidates or deletes it.
    Vanished,
    /// Found merged or abandoned on Gerrit with unsent data, which is held
    /// for the user.
    Closed,
    /// Everything was moved to the Gerrit change. The row stays as a
    /// tombstone, with its versions mapped to Gerrit patch sets, until its
    /// commits leave the branches.
    Consolidated,
}

impl LocalChangeState {
    pub const ALL: [Self; 6] = [
        Self::Checking,
        Self::Local,
        Self::Ambiguous,
        Self::Vanished,
        Self::Closed,
        Self::Consolidated,
    ];

    /// The spelling in the store.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Checking => "checking",
            Self::Local => "local",
            Self::Ambiguous => "ambiguous",
            Self::Vanished => "vanished",
            Self::Closed => "closed",
            Self::Consolidated => "consolidated",
        }
    }

    /// The state spelled `s` ([`LocalChangeState::as_str`]).
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == s)
    }
}

/// A change that exists in the checkout and not (or not yet completely) on
/// Gerrit. The summary fields are those of its newest version, so a list
/// can show it without reading git.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalChange {
    pub id: LocalChangeId,
    /// The synthetic number pages and drafts use (`LOCAL_BASE + id`).
    pub number: ChangeNumber,
    pub project: String,
    /// The target branch (`main`, not `refs/heads/main`), if known.
    pub branch: Option<String>,
    pub change_id: String,
    /// The Gerrit change these versions belong to: set for local patch sets
    /// of a Gerrit change, and once a local change was found on Gerrit.
    pub gerrit_number: Option<ChangeNumber>,
    pub subject: String,
    pub author_name: String,
    pub author_email: String,
    /// The committer time of the newest version.
    pub updated: Timestamp,
    pub state: LocalChangeState,
    pub first_seen: Timestamp,
}

impl LocalChange {
    /// Whether this is a change of its own, not versions of a change that
    /// is on Gerrit.
    pub fn is_purely_local(&self) -> bool {
        self.gerrit_number.is_none()
    }
}

/// One version (commit) of a local change, numbered in the order it was
/// first seen ("local 1", "local 2", ...).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalPatchSet {
    pub id: LocalPatchSetId,
    /// The synthetic patch set number (`LOCAL_BASE + id`).
    pub number: PatchSetNumber,
    pub local_change: LocalChangeId,
    pub sha: RevisionId,
    /// The first parent; `None` for a root commit.
    pub parent_sha: Option<RevisionId>,
    pub seen_at: Timestamp,
    /// 1 for the first version seen of the change, then counting up.
    pub ordinal: u32,
    /// Whether the commit was on a named local branch in the last complete
    /// scan of the checkout (true until a scan says otherwise). A version
    /// that is not was amended away or dropped: it will never reach Gerrit
    /// as it is.
    #[serde(default = "on_branch_default")]
    pub on_branch: bool,
    /// The Gerrit patch set with this very commit, once there is one.
    pub gerrit_patch_set: Option<PatchSetNumber>,
}

fn on_branch_default() -> bool {
    true
}

/// Where a pending review stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PendingReviewState {
    /// Waiting for its patch set to reach Gerrit; can still be edited and
    /// cancelled.
    Pending,
    /// Being posted. Written before the request, so a crash leaves a trace
    /// to check against the change's messages (by the nonce tag).
    Sending,
    /// Not sent automatically; waits for the user to publish or drop it
    /// (see [`PendingReview::held_reason`]).
    Held,
    /// Posted.
    Sent,
    /// The server refused (some of) it (see [`PendingReview::error`]).
    Failed,
}

impl PendingReviewState {
    pub const ALL: [Self; 5] = [
        Self::Pending,
        Self::Sending,
        Self::Held,
        Self::Sent,
        Self::Failed,
    ];

    /// The spelling in the store.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Sending => "sending",
            Self::Held => "held",
            Self::Sent => "sent",
            Self::Failed => "failed",
        }
    }

    /// The state spelled `s` ([`PendingReviewState::as_str`]).
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == s)
    }
}

/// A reply (votes and a message) given on a local patch set, kept until
/// that patch set, or what it became, is on Gerrit. Its inline comments
/// are ordinary drafts on the local numbers; its message is a sealed
/// patch-set-level draft linked to it ([`crate::Draft::pending_review`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingReview {
    pub id: PendingReviewId,
    /// The local change it belongs to.
    pub local_change: LocalChangeId,
    /// Where it will be posted: the local numbers until consolidation moves
    /// it onto the Gerrit change and patch set.
    pub change: ChangeNumber,
    pub patch_set: PatchSetNumber,
    /// The commit of `patch_set`: the local version until consolidation
    /// moves the review, then the Gerrit patch set's commit.
    pub revision: RevisionId,
    /// The commit the votes were given on; never changes, so the approval
    /// rules can tell an exact match from a moved vote.
    pub given_on_sha: RevisionId,
    /// Label name → vote.
    pub labels: BTreeMap<String, i32>,
    /// The project's `local_approval` mode when the votes were given.
    pub strategy: LocalApproval,
    pub state: PendingReviewState,
    /// Why the last send failed (in `Failed`).
    pub error: Option<String>,
    /// Why it is held (in `Held`).
    pub held_reason: Option<String>,
    /// The labels of a `Held` review whose votes may only be dropped, never
    /// published or moved: approvals held under the strict rule (a local
    /// patch set of a Gerrit change) or the `exact` strategy, and approvals
    /// already cast on a patch set that was no longer current. Empty in
    /// every other state.
    #[serde(default)]
    pub drop_only: BTreeSet<String>,
    pub created: Timestamp,
    /// When the votes were last given or changed (at creation, and by an
    /// edit that changes the labels). The "purely local" test compares
    /// this with the Gerrit change's creation.
    pub voted_at: Timestamp,
    /// Identifies the send attempt (part of the review's tag); set when the
    /// review is marked as sending.
    pub nonce: Option<String>,
    /// The text of its message draft, if it has one: a read-only copy
    /// joined in when the review is read. The sealed draft is the source of
    /// truth; change it through the pending review API.
    pub message: Option<String>,
    /// Whether its message asks for something to be done (the message
    /// draft's `unresolved`); false without a message.
    #[serde(default)]
    pub unresolved: bool,
    /// Its message draft, if it has one.
    pub message_draft: Option<crate::LocalDraftId>,
}

impl PendingReview {
    /// Whether it still has to reach the server (anything but `Sent`).
    pub fn is_unsent(&self) -> bool {
        self.state != PendingReviewState::Sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spellings_round_trip() {
        for s in LocalChangeState::ALL {
            assert_eq!(LocalChangeState::parse(s.as_str()), Some(s));
        }
        for s in PendingReviewState::ALL {
            assert_eq!(PendingReviewState::parse(s.as_str()), Some(s));
        }
        assert_eq!(LocalChangeState::parse("Local"), None);
        assert_eq!(PendingReviewState::parse(""), None);
    }

    #[test]
    fn row_ids_give_synthetic_numbers() {
        let c = LocalChangeId(3).number().unwrap();
        assert!(c.is_local());
        assert_eq!(c.local_id(), Some(3));
        let p = LocalPatchSetId(4).number().unwrap();
        assert!(p.is_local());
        assert_eq!(p.local_id(), Some(4));
    }
}
