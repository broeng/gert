use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::AccountId;

/// What to do with the user's drafts when a review is posted
/// (`DraftHandling`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DraftHandling {
    /// Leave all drafts alone.
    Keep,
    /// Publish the drafts of the revision the review is posted to.
    Publish,
    /// Publish the drafts of every patch set. This client's default: the
    /// reply dialog lists them all.
    #[default]
    PublishAllRevisions,
}

impl DraftHandling {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Keep => "KEEP",
            Self::Publish => "PUBLISH",
            Self::PublishAllRevisions => "PUBLISH_ALL_REVISIONS",
        }
    }
}

/// Who gets an email about a review (`NotifyHandling`). Mail is off on the
/// dev Gerrit; on real servers `None` keeps a vote quiet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NotifyHandling {
    None,
    Owner,
    OwnerReviewers,
    All,
}

impl NotifyHandling {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::None => "NONE",
            Self::Owner => "OWNER",
            Self::OwnerReviewers => "OWNER_REVIEWERS",
            Self::All => "ALL",
        }
    }
}

/// An attention set change (`AttentionSetInput`). The server adds
/// the reason to the change log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionSetInput {
    pub user: AccountId,
    pub reason: String,
}

/// A review to post (`ReviewInput`): a message, label votes, and the drafts
/// to publish.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewInput {
    /// The change message. `None` or empty posts none.
    pub message: Option<String>,
    /// Label name to the voted value. A label that is left out keeps the
    /// user's current vote.
    pub labels: BTreeMap<String, i32>,
    pub drafts: DraftHandling,
    pub tag: Option<String>,
    pub notify: Option<NotifyHandling>,
    pub add_to_attention_set: Vec<AttentionSetInput>,
    pub remove_from_attention_set: Vec<AttentionSetInput>,
    /// `Some(true)` marks a work-in-progress change ready for review.
    pub ready: Option<bool>,
    /// `Some(true)` marks the change work in progress.
    pub work_in_progress: Option<bool>,
}

impl ReviewInput {
    /// A review with only a message, publishing all drafts.
    pub fn with_message(message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
            ..Self::default()
        }
    }
}

/// The server's answer to a posted review (`ReviewResult`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewResult {
    /// The user's votes after the review, as the server applied them.
    pub labels: BTreeMap<String, i32>,
    /// Whether a work-in-progress change was marked ready.
    pub ready: Option<bool>,
    /// Set when part of the request failed although the review itself was
    /// accepted (for example a reviewer that could not be added).
    pub error: Option<String>,
}

/// One value a label can be given, with its meaning (`labels[].values`):
/// `-1`, "This shall not be merged".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelValue {
    pub value: i32,
    pub description: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_publish_every_draft() {
        let r = ReviewInput::with_message("LGTM");
        assert_eq!(r.drafts, DraftHandling::PublishAllRevisions);
        assert_eq!(r.drafts.as_wire(), "PUBLISH_ALL_REVISIONS");
        assert_eq!(r.message.as_deref(), Some("LGTM"));
        assert!(r.labels.is_empty());
        assert_eq!(NotifyHandling::OwnerReviewers.as_wire(), "OWNER_REVIEWERS");
    }
}
