use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{AccountId, DraftInput, PatchSetNumber};

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

/// A review to post (`ReviewInput`): a message, label votes, inline
/// comments, and the drafts to publish.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewInput {
    /// The change message. `None` or empty posts none.
    pub message: Option<String>,
    /// Label name to the voted value. A label that is left out keeps the
    /// user's current vote.
    pub labels: BTreeMap<String, i32>,
    /// Inline comments posted with the review itself (`comments`, a
    /// `CommentInput` per comment, keyed by path on the wire), on the
    /// revision the review is posted to. Unlike drafts, they need not be
    /// on the server first; `drafts` does not affect them.
    pub comments: Vec<DraftInput>,
    pub drafts: DraftHandling,
    pub tag: Option<String>,
    pub notify: Option<NotifyHandling>,
    pub add_to_attention_set: Vec<AttentionSetInput>,
    pub remove_from_attention_set: Vec<AttentionSetInput>,
    /// `Some(true)` marks a work-in-progress change ready for review.
    pub ready: Option<bool>,
    /// `Some(true)` marks the change work in progress.
    pub work_in_progress: Option<bool>,
    /// The ids of the drafts to publish, instead of what `drafts` selects.
    /// `None` leaves the choice to `drafts`.
    pub draft_ids_to_publish: Option<Vec<String>>,
    /// Leaves the attention set alone: the server's automatic rules (the
    /// owner gets attention after a reply, the replier loses it) are not
    /// applied. Only the explicit `add_to_attention_set` and
    /// `remove_from_attention_set` change it.
    pub ignore_automatic_attention_set_rules: bool,
}

impl ReviewInput {
    /// The votes of this review that the server did not apply: those
    /// missing from `result.labels` or with another value. Gerrit
    /// (3.14) answers 200 and silently drops a label it does not know or a
    /// value the label does not have, so this is the only way to notice.
    /// Sorted by label name.
    pub fn unapplied_labels(&self, result: &ReviewResult) -> Vec<(String, i32)> {
        self.labels
            .iter()
            .filter(|(name, value)| result.labels.get(*name) != Some(*value))
            .map(|(name, value)| (name.clone(), *value))
            .collect()
    }

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
    /// The change's current patch set after the review, when the server
    /// sends the change along (`change_info`, Gerrit 3.14 does). Gerrit
    /// accepts votes on a patch set that is not current and keeps them
    /// there, so this is how to tell that votes posted to a patch set did
    /// not land on the current one.
    #[serde(default)]
    pub current_patch_set: Option<PatchSetNumber>,
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

    #[test]
    fn labels_the_server_dropped_are_reported() {
        let mut r = ReviewInput::default();
        r.labels.insert("Code-Review".into(), 2);
        r.labels.insert("Verified".into(), 1);
        r.labels.insert("Nope".into(), 1);
        let result = ReviewResult {
            labels: [("Code-Review".to_owned(), 2), ("Verified".to_owned(), 0)].into(),
            ..ReviewResult::default()
        };
        assert_eq!(
            r.unapplied_labels(&result),
            [("Nope".to_owned(), 1), ("Verified".to_owned(), 1)]
        );
        assert!(ReviewInput::default().unapplied_labels(&result).is_empty());
    }
}
