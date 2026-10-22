//! Reviewers, CCs, topic and hashtags.
//!
//! Like the change actions (see `actions`): send the write, and if the
//! server accepted it fetch the change again into the cache. A refused
//! write (401, 403, 409, ...) is an error and leaves the cache alone, and
//! so does an add that Gerrit answered with 200 but did not do (see
//! [`ReviewerAdded`]).
//!
//! [`Syncer::suggest_reviewers`] is an autocomplete: read-only, and not
//! cached.

use model::{AddReviewerResult, ChangeDetail, ChangeNumber, ReviewerState, SuggestedReviewer};

use super::Syncer;
use crate::Error;

/// How many suggestions to ask for.
const SUGGEST_LIMIT: u32 = 10;

/// The outcome of adding a reviewer or CC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewerAdded {
    /// What the server said. Check [`AddReviewerResult::is_added`]: when it
    /// is false (`error`, or `confirm` for a large group) nothing changed
    /// and the cache was not touched.
    pub result: AddReviewerResult,
    /// The refreshed change. `None` if nobody was added, or if the
    /// re-fetch failed afterwards (the add itself went through).
    pub detail: Option<ChangeDetail>,
}

impl Syncer {
    /// Autocomplete for reviewers: accounts and groups matching `query`.
    /// Nothing is cached; an empty query gives an empty list without a
    /// request.
    pub async fn suggest_reviewers(
        &self,
        change: ChangeNumber,
        query: &str,
    ) -> Result<Vec<SuggestedReviewer>, Error> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self
            .client
            .suggest_reviewers(change, query, SUGGEST_LIMIT)
            .await?)
    }

    /// Adds `reviewer` (see [`SuggestedReviewer::reviewer_input`]) as a
    /// reviewer or CC, or moves an existing one to `state`. Pass
    /// `confirmed` after the server asked to confirm a large group.
    pub async fn add_reviewer(
        &self,
        change: ChangeNumber,
        reviewer: &str,
        state: ReviewerState,
        confirmed: bool,
    ) -> Result<ReviewerAdded, Error> {
        let result = self
            .client
            .add_reviewer(change, reviewer, state, confirmed)
            .await?;
        let detail = if result.is_added() {
            self.refresh_after_action(change, "add reviewer").await
        } else {
            None
        };
        Ok(ReviewerAdded { result, detail })
    }

    /// Removes a reviewer or CC (account id, email or username).
    pub async fn remove_reviewer(
        &self,
        change: ChangeNumber,
        reviewer: &str,
    ) -> Result<Option<ChangeDetail>, Error> {
        self.client.remove_reviewer(change, reviewer).await?;
        Ok(self.refresh_after_action(change, "remove reviewer").await)
    }

    /// Sets the topic, or clears it for `None` or a blank one.
    pub async fn set_topic(
        &self,
        change: ChangeNumber,
        topic: Option<&str>,
    ) -> Result<Option<ChangeDetail>, Error> {
        self.client.set_topic(change, topic).await?;
        Ok(self.refresh_after_action(change, "topic").await)
    }

    /// Adds and removes hashtags.
    pub async fn edit_hashtags(
        &self,
        change: ChangeNumber,
        add: &[String],
        remove: &[String],
    ) -> Result<Option<ChangeDetail>, Error> {
        self.client.edit_hashtags(change, add, remove).await?;
        Ok(self.refresh_after_action(change, "hashtags").await)
    }
}

#[cfg(test)]
mod tests;
