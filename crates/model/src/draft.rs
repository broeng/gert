use serde::{Deserialize, Serialize};

use crate::{ChangeNumber, Location, PatchSetNumber, RevisionId, Side, Timestamp};

/// The id of a draft in the local database. Stable for the draft's whole
/// life, unlike the server's draft id, which only exists once the draft was
/// uploaded (and which Gerrit never changes afterwards). Ids grow with
/// creation order, so they also order uploads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LocalDraftId(pub i64);

/// What still has to be done to a draft on the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DraftOp {
    Create,
    Update,
    Delete,
}

/// Where a local draft stands relative to the server. See `store` for the
/// transitions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DraftSync {
    /// The server has exactly this draft.
    Synced,
    /// New locally, never uploaded.
    PendingCreate,
    /// Uploaded before, edited locally since.
    PendingUpdate,
    /// Deleted locally; kept until the server confirms, then removed.
    /// Hidden from the UI and from threads.
    PendingDelete,
    /// The upload was refused or gave up. Not retried automatically.
    Failed {
        /// The operation that failed.
        op: DraftOp,
        /// The server's message or the transport error.
        error: String,
        /// Whether trying again by hand may help (for example after a 500).
        /// False for definite rejections (403, 409, 400): the draft has to
        /// be edited or deleted first.
        retryable: bool,
    },
}

impl DraftSync {
    /// True while the server does not have the draft's current content
    /// (anything but `Synced`).
    pub fn is_pending(&self) -> bool {
        !matches!(self, Self::Synced)
    }

    /// True for the states the automatic retry works on.
    pub fn is_uploadable(&self) -> bool {
        matches!(
            self,
            Self::PendingCreate | Self::PendingUpdate | Self::PendingDelete
        )
    }

    /// The operation an upload of this state performs.
    pub fn op(&self) -> Option<DraftOp> {
        match self {
            Self::Synced => None,
            Self::PendingCreate => Some(DraftOp::Create),
            Self::PendingUpdate => Some(DraftOp::Update),
            Self::PendingDelete => Some(DraftOp::Delete),
            Self::Failed { op, .. } => Some(*op),
        }
    }
}

/// The fields of a draft the user writes, and what the server takes
/// (`DraftInput`). The patch set is not part of it: it is the revision in
/// the request URL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftInput {
    pub path: String,
    pub side: Side,
    pub parent: Option<u32>,
    pub location: Location,
    /// The id of the published comment this replies to.
    pub in_reply_to: Option<String>,
    pub message: String,
    pub unresolved: bool,
}

/// A draft comment of the signed-in user, kept in the local database and
/// mirrored to the server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    pub local_id: LocalDraftId,
    /// Gerrit's id for the draft, once it exists there.
    pub server_id: Option<String>,
    pub change: ChangeNumber,
    pub patch_set: PatchSetNumber,
    /// The commit of `patch_set`; drafts are uploaded to a revision.
    pub revision: RevisionId,
    pub path: String,
    pub side: Side,
    pub parent: Option<u32>,
    pub location: Location,
    pub in_reply_to: Option<String>,
    pub message: String,
    pub unresolved: bool,
    /// The last local edit, or the server's time for synced drafts.
    pub updated: Timestamp,
    pub sync: DraftSync,
    /// Upload attempts since the last change of the draft.
    pub attempts: u32,
    /// The error of the last failed attempt, kept while the draft stays
    /// pending (for example "offline").
    pub last_error: Option<String>,
}

impl Draft {
    /// The writable part, as sent to the server.
    pub fn input(&self) -> DraftInput {
        DraftInput {
            path: self.path.clone(),
            side: self.side,
            parent: self.parent,
            location: self.location,
            in_reply_to: self.in_reply_to.clone(),
            message: self.message.clone(),
            unresolved: self.unresolved,
        }
    }

    /// Whether the user should see this draft: deleted ones are only
    /// waiting for the server's confirmation.
    pub fn is_visible(&self) -> bool {
        self.sync != DraftSync::PendingDelete
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_state_helpers() {
        let failed = DraftSync::Failed {
            op: DraftOp::Update,
            error: "gone".into(),
            retryable: false,
        };
        assert!(!DraftSync::Synced.is_pending());
        assert!(failed.is_pending() && !failed.is_uploadable());
        assert!(DraftSync::PendingDelete.is_uploadable());
        assert_eq!(failed.op(), Some(DraftOp::Update));
        assert_eq!(DraftSync::PendingCreate.op(), Some(DraftOp::Create));
        assert_eq!(DraftSync::Synced.op(), None);
    }
}
