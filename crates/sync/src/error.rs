use model::{LocalDraftId, PatchSetNumber};

/// A failed sync operation. A failed refresh leaves the cache as it was.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("server request failed: {0}")]
    Api(#[from] gerrit_api::Error),
    #[error("cache error: {0}")]
    Store(#[from] store::Error),
    /// A review cannot be published while drafts that it would publish are
    /// not on the server. Nothing was posted.
    #[error("{} draft comment(s) are not uploaded: {}", .0.len(), describe(.0))]
    DraftsNotUploaded(Vec<PendingDraft>),
}

impl Error {
    /// True if the server refused the credentials (401): the user has to
    /// log in again. Other failures, 403 included, do not mean that.
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Api(e) if e.is_unauthorized())
    }

    /// True if the server answered and refused the request (403, 409, other
    /// HTTP errors), as opposed to not being reachable.
    pub fn is_refusal(&self) -> bool {
        matches!(self, Self::Api(e) if e.is_refusal())
    }
}

/// A draft that is not (yet) on the server, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingDraft {
    pub local_id: LocalDraftId,
    pub path: String,
    pub patch_set: PatchSetNumber,
    /// The last error, or a description of the state ("waiting to upload").
    pub reason: String,
}

fn describe(list: &[PendingDraft]) -> String {
    list.iter()
        .map(|d| format!("{} (patch set {}): {}", d.path, d.patch_set, d.reason))
        .collect::<Vec<_>>()
        .join("; ")
}
