//! Pending reviews: the reply dialog's and quick votes' writes on a local
//! patch set, kept in the store until the version is on Gerrit (see
//! `store::pending_reviews`). No network: nothing here talks to the
//! server; consolidation sends them later.

use model::{ChangeNumber, LocalApproval, PendingReview, PendingReviewId, Timestamp};
use store::{NewPendingReview, PendingReviewEdit};

use super::Syncer;
use crate::Error;

impl Syncer {
    /// The pending reviews of `change` (a local change's synthetic number,
    /// or a Gerrit change with local patch sets), in creation order, sent
    /// ones included. Blocking (SQLite).
    pub fn pending_reviews(&self, change: ChangeNumber) -> Result<Vec<PendingReview>, Error> {
        Ok(self.store().pending_reviews(self.server, change)?)
    }

    /// Saves a review on a local patch set. Fails with
    /// `store::Error::NotLocal` for a patch set that is not a local one and
    /// `store::Error::NotReviewable` when its change is consolidated or
    /// closed (reply on the Gerrit change then). Blocking (SQLite).
    pub fn create_pending_review(&self, input: &NewPendingReview) -> Result<PendingReview, Error> {
        Ok(self.store().create_pending_review(self.server, input)?)
    }

    /// Replaces the votes and message of a review that is still pending;
    /// `None` when it is gone or no longer pending (being sent, sent,
    /// held). Blocking (SQLite).
    pub fn update_pending_review(
        &self,
        id: PendingReviewId,
        edit: &PendingReviewEdit,
    ) -> Result<Option<PendingReview>, Error> {
        Ok(self
            .store()
            .update_pending_review(self.server, id, edit, Timestamp::now())?)
    }

    /// Gives one label's vote in a review that is still pending, leaving
    /// the rest of it alone (a quick vote). `None` when it is gone or no
    /// longer pending. Blocking (SQLite).
    pub fn set_pending_review_label(
        &self,
        id: PendingReviewId,
        label: &str,
        value: i32,
        strategy: LocalApproval,
    ) -> Result<Option<PendingReview>, Error> {
        Ok(self.store().set_review_label(
            self.server,
            id,
            label,
            value,
            strategy,
            false,
            Timestamp::now(),
        )?)
    }

    /// Withdraws a pending review; its message stays as an ordinary draft.
    /// False when it is not pending (any more). Blocking (SQLite).
    pub fn cancel_pending_review(&self, id: PendingReviewId) -> Result<bool, Error> {
        Ok(self.store().cancel_pending_review(self.server, id)?)
    }

    /// Discards a held or failed review with its message. False in any
    /// other state. Blocking (SQLite).
    pub fn drop_pending_review(&self, id: PendingReviewId) -> Result<bool, Error> {
        Ok(self.store().drop_pending_review(self.server, id)?)
    }
}

#[cfg(test)]
mod tests;
