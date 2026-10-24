//! The watch pass: finds what happened on the changes the user cares
//! about since the last pass, as notification events.
//!
//! One list query says which changes moved (`updated` differs from the
//! cached detail). Those are fetched one by one with [`Syncer::refresh_change`].
//! Turning the difference into events is not done here but where every
//! fresh detail is cached, so that a refresh from anywhere else (the change
//! page, an action) cannot lose them. The events wait in the store's
//! pending table until the caller acknowledges them with
//! [`Store::ack_events`](store::Store::ack_events), so they also survive a
//! restart.
//!
//! Baselines never notify: a change without a cached detail is only cached,
//! and the first complete pass on a server (see
//! [`Store::watched_since`](store::Store::watched_since)) caches
//! everything and reports nothing.

use jiff::Timestamp;
use model::{Account, Event};
use store::EVENT_RETENTION;

use super::Syncer;
use crate::Error;

/// Limits of one [`Syncer::watch_pass`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchOptions {
    /// Page size of the list query; changes beyond it are not watched.
    pub query_limit: u32,
    /// How many change details one pass fetches. The rest are seen by the
    /// next pass, so a burst of activity spreads out instead of stalling.
    pub max_fetches: usize,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            query_limit: 100,
            max_fetches: 25,
        }
    }
}

impl Syncer {
    /// Runs one watch pass over the changes matching `query`, e.g.
    /// `is:open (owner:self OR reviewer:self OR attention:self)`, and
    /// returns the pending events of the server, oldest first: those this
    /// pass found and those queued by refreshes since the last
    /// acknowledgement. They stay pending until acknowledged (by
    /// [`Event::key`]) with [`Store::ack_events`](store::Store::ack_events),
    /// so a caller that fails to deliver gets them again.
    ///
    /// Details are fetched one after the other. A change whose fetch fails
    /// is skipped with its cache untouched, so the next pass tries it again
    /// and finds its events then; the other changes are not affected. After
    /// a failure that is not a refusal (the server is unreachable) the
    /// pass stops fetching. A failure of the query itself is returned.
    ///
    /// While the server has never had a complete pass (every moved change
    /// fetched, within the bound) nothing is reported.
    pub async fn watch_pass(
        &self,
        query: &str,
        options: WatchOptions,
    ) -> Result<Vec<Event>, Error> {
        let mut list = gerrit_api::ChangeQuery::new(query).limit(options.query_limit);
        // Only `updated` is compared; the details come from the fetches.
        list.options.clear();
        let page = self.client.query_changes(&list).await?;
        let baseline = self.store().watched_since(self.server)?.is_none();

        let mut fetched = 0;
        let mut complete = true;
        for summary in &page.changes {
            let cached = self.cached_change(summary.number)?;
            if cached
                .as_ref()
                .is_some_and(|c| c.detail.summary.updated == summary.updated)
            {
                continue;
            }
            if fetched >= options.max_fetches {
                complete = false;
                continue;
            }
            fetched += 1;
            match self.refresh_change(summary.number).await {
                Ok(_) => {}
                Err(e) => {
                    log::warn!("watching change {}: {e}", summary.number);
                    complete = false;
                    if !e.is_refusal() {
                        break;
                    }
                }
            }
        }

        let now = Timestamp::now();
        if baseline {
            if complete {
                self.store().mark_watched(self.server, now)?;
            }
            return Ok(Vec::new());
        }

        let mut store = self.store();
        if let Ok(cutoff) = now.checked_sub(EVENT_RETENTION)
            && let Err(e) = store.prune_events(cutoff)
        {
            log::warn!("pruning the event ledger failed: {e}");
        }
        Ok(store.pending_events(self.server)?)
    }
}

impl Syncer {
    /// The signed-in account of the server (`GET /accounts/self/detail`).
    pub async fn account_self(&self) -> Result<Account, Error> {
        Ok(self.client.account_self().await?)
    }

    /// The events of the server waiting for acknowledgement, oldest first,
    /// without running a pass. This is what a refresh of one change (the
    /// change page, an action) queued since the last pass.
    pub fn pending_events(&self) -> Result<Vec<Event>, Error> {
        Ok(self.store().pending_events(self.server)?)
    }

    /// Acknowledges `events`: they are not returned again. Call it once
    /// they are delivered, shown or deliberately dropped.
    pub fn ack_events(&self, events: &[Event]) -> Result<(), Error> {
        let keys: Vec<String> = events.iter().map(Event::key).collect();
        Ok(self.store().ack_events(self.server, &keys)?)
    }
}

#[cfg(test)]
mod tests;
