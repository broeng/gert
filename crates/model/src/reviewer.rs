use serde::{Deserialize, Serialize};

use crate::Account;

/// Whether an added reviewer votes (`REVIEWER`) or is only copied (`CC`).
/// `REMOVED` is not a state one can ask for; removing is its own call.
///
/// [`ChangeDetail`](crate::ChangeDetail) keeps the two lists apart
/// (`reviewers` and `cc`), so the UI can tell them from each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReviewerState {
    Reviewer,
    Cc,
}

impl ReviewerState {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Reviewer => "REVIEWER",
            Self::Cc => "CC",
        }
    }
}

/// A group suggested as a reviewer (`GroupBaseInfo`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuggestedGroup {
    /// The group's UUID; it is what `add_reviewer` takes for a group.
    pub id: String,
    pub name: Option<String>,
}

/// One entry of the reviewer autocomplete (`SuggestedReviewerInfo`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SuggestedReviewer {
    Account(Account),
    Group {
        group: SuggestedGroup,
        /// How many accounts the group has, when the server says.
        count: Option<u32>,
        /// Adding the group needs a confirmation (it is large): pass
        /// `confirmed` to `add_reviewer` once the user agreed.
        confirm: bool,
    },
}

impl SuggestedReviewer {
    /// What to send as the `reviewer` of an add: the account id, or the
    /// group UUID.
    pub fn reviewer_input(&self) -> String {
        match self {
            Self::Account(a) => a.id.to_string(),
            Self::Group { group, .. } => group.id.clone(),
        }
    }
}

/// The answer to adding a reviewer (`ReviewerResult`).
///
/// Gerrit answers 200 even when it did not add anybody: `error` then says
/// why (unknown account, not allowed, ...), and `confirm` means a large
/// group needs the user's confirmation first. Use [`Self::is_added`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddReviewerResult {
    /// The `reviewer` that was sent.
    pub input: String,
    pub error: Option<String>,
    /// Repeat the request with `confirmed` set to add a large group.
    pub confirm: bool,
    /// Accounts now added as reviewers (more than one for a group).
    pub reviewers: Vec<Account>,
    /// Accounts now added as CCs.
    pub ccs: Vec<Account>,
}

impl AddReviewerResult {
    /// Whether the server added (or moved) anybody.
    pub fn is_added(&self) -> bool {
        self.error.is_none() && !self.confirm
    }
}
