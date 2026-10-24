//! The watch pass: finds what happened on the changes the user cares
//! about since the last pass, as notification events.
//!
//! One list query says which changes moved (`updated` differs from the
//! cached detail). Those are fetched one by one with [`Syncer::refresh_change`],
//! after loading the previous cached detail, and the two are compared with
//! [`diff_change`]. Events then go through the ledger so each is reported
//! once, across restarts.
//!
//! Baselines never notify: a change without a cached detail is only cached,
//! and the first complete pass on a server (see
//! [`Store::watched_since`](store::Store::watched_since)) caches
//! everything and reports nothing.
//!
//! Caveat: the comparison is against whatever the cache holds. A refresh
//! of the change page in between advances the cache without going through
//! here.

use jiff::Timestamp;
use model::Event;
use store::EVENT_RETENTION;

use super::Syncer;
use crate::{Error, diff_change};

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
    /// returns the events nobody was told about yet, oldest first.
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

        let mut events = Vec::new();
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
                Ok(new) => {
                    if !baseline && let Some(old) = cached {
                        events.extend(diff_change(self.server, &old.detail, &new));
                    }
                }
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

        events.sort_by_key(|e| e.at);
        let keys: Vec<String> = events.iter().map(Event::key).collect();
        let mut store = self.store();
        let fresh = store.record_events(self.server, &keys, now)?;
        if let Ok(cutoff) = now.checked_sub(EVENT_RETENTION)
            && let Err(e) = store.prune_events(cutoff)
        {
            log::warn!("pruning the event ledger failed: {e}");
        }
        Ok(events
            .into_iter()
            .zip(fresh)
            .filter_map(|(e, new)| new.then_some(e))
            .collect())
    }
}

#[cfg(test)]
mod tests;
