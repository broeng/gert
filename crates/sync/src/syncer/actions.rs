//! Change actions: submit, rebase, abandon, restore, WIP / ready, private.
//!
//! Every method sends the request, and only if the server accepted it
//! fetches the change again (detail, and with it the actions the user may
//! now run) into the cache, so the UI re-renders from the store. A refused
//! action (401, 403, 409, ...) is returned as an error and leaves the cache
//! as it was: there is no optimistic update and no outbox, an action needs
//! a connection.
//!
//! The result is the refreshed detail, or `None` when the action went
//! through but fetching the change failed afterwards (logged; the next
//! refresh catches up). Callers must not treat that as a failed action.

use model::{ChangeDetail, ChangeNumber};

use super::Syncer;
use crate::Error;

impl Syncer {
    /// Submits the change.
    pub async fn submit(&self, change: ChangeNumber) -> Result<Option<ChangeDetail>, Error> {
        self.client.submit(change).await?;
        Ok(self.refresh_after_action(change, "submit").await)
    }

    /// Rebases the change onto `base` (a commit SHA or change number), or
    /// onto the tip of its branch.
    pub async fn rebase(
        &self,
        change: ChangeNumber,
        base: Option<&str>,
    ) -> Result<Option<ChangeDetail>, Error> {
        self.client.rebase(change, base).await?;
        Ok(self.refresh_after_action(change, "rebase").await)
    }

    pub async fn abandon(
        &self,
        change: ChangeNumber,
        message: Option<&str>,
    ) -> Result<Option<ChangeDetail>, Error> {
        self.client.abandon(change, message).await?;
        Ok(self.refresh_after_action(change, "abandon").await)
    }

    pub async fn restore(
        &self,
        change: ChangeNumber,
        message: Option<&str>,
    ) -> Result<Option<ChangeDetail>, Error> {
        self.client.restore(change, message).await?;
        Ok(self.refresh_after_action(change, "restore").await)
    }

    /// Marks the change work in progress, or ready for review.
    pub async fn set_work_in_progress(
        &self,
        change: ChangeNumber,
        work_in_progress: bool,
        message: Option<&str>,
    ) -> Result<Option<ChangeDetail>, Error> {
        self.client
            .set_work_in_progress(change, work_in_progress, message)
            .await?;
        Ok(self.refresh_after_action(change, "wip/ready").await)
    }

    /// Marks the change private, or not.
    pub async fn set_private(
        &self,
        change: ChangeNumber,
        private: bool,
    ) -> Result<Option<ChangeDetail>, Error> {
        self.client.set_private(change, private).await?;
        Ok(self.refresh_after_action(change, "private").await)
    }

    async fn refresh_after_action(&self, change: ChangeNumber, what: &str) -> Option<ChangeDetail> {
        match self.refresh_change(change).await {
            Ok(d) => Some(d),
            Err(e) => {
                log::warn!("refreshing change {change} after {what}: {e}");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests;
