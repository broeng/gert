use serde::{Deserialize, Serialize};

use crate::{ChangeNumber, ChangeStatus, Commit, PatchSetNumber};

/// One entry of the related-changes list
/// (`RelatedChangeAndCommitInfo`): a change in the same series, newest
/// descendant first, as the server orders them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelatedChange {
    pub project: String,
    pub change_id: Option<String>,
    pub number: ChangeNumber,
    /// The patch set that is related to the one asked about.
    pub patch_set: PatchSetNumber,
    /// The newest patch set of that change; differs from `patch_set` when
    /// the series is based on an outdated patch set.
    pub current_patch_set: Option<PatchSetNumber>,
    pub status: Option<ChangeStatus>,
    pub submittable: Option<bool>,
    pub work_in_progress: bool,
    /// The commit (SHA and subject; parents when the server includes them).
    pub commit: Option<Commit>,
}

impl RelatedChange {
    /// True if a newer patch set of the related change exists.
    pub fn is_outdated(&self) -> bool {
        self.current_patch_set.is_some_and(|c| c > self.patch_set)
    }
}
