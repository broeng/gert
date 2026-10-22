//! Change actions: submit, rebase, abandon, restore, WIP / ready, private.
//!
//! Every method sends the request, and only if the server accepted it
//! fetches the change again (detail, and with it the actions the user may
//! now run) into the cache, so the UI re-renders from the store. A refused
//! action (401, 403, 409, ...) is returned as an error: there is no
//! optimistic update and no outbox, an action needs a connection. A caller
//! that gets a refusal asks [`Syncer::refresh_after_refusal`] for the
//! change as the server has it, since the refusal usually means the cached
//! one is stale (the change was merged, rebased or abandoned meanwhile).
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
        options: model::RebaseOptions,
    ) -> Result<Option<ChangeDetail>, Error> {
        self.client.rebase(change, base, options).await?;
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

    /// After `error` refused a write to `change`: fetches the change into
    /// the cache and returns it, so the UI can show what the server has.
    /// Does nothing (`None`) for errors that are not a refusal: a transport
    /// failure says nothing about the change, and 401 means logging in. If
    /// fetching fails too, that is logged and the cache stays as it was.
    pub async fn refresh_after_refusal(
        &self,
        change: ChangeNumber,
        error: &Error,
    ) -> Option<ChangeDetail> {
        if !error.is_refusal() {
            return None;
        }
        self.refresh_after_action(change, "a refused write").await
    }

    pub(super) async fn refresh_after_action(
        &self,
        change: ChangeNumber,
        what: &str,
    ) -> Option<ChangeDetail> {
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
