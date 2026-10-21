use std::sync::{Arc, Mutex, MutexGuard};

use futures::future::{BoxFuture, FutureExt};
use gerrit_api::{ChangeQuery, GerritClient};
use jiff::Timestamp;
use model::{ChangeDetail, ChangeNumber, ChangeSummary};
use store::{CachedChange, CachedQuery, ServerId, Store};

use crate::Error;

/// What a refresh did to the cached result of a query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryUpdate {
    /// Whether the visible result differs from before (different changes,
    /// order, `updated` times, or paging state). The UI re-renders only
    /// when this is true.
    pub changed: bool,
    /// Number of changes now cached for the query.
    pub cached_count: usize,
    /// Whether the server has results beyond the cached ones.
    pub more_changes: bool,
}

/// The seam between "something says the data is stale" and the cache.
/// [`Syncer`] is the polling implementation; a push-based source could
/// implement the same operations later.
pub trait RefreshSource: Send + Sync {
    fn refresh_query<'a>(
        &'a self,
        query: &'a str,
        page_size: u32,
    ) -> BoxFuture<'a, Result<QueryUpdate, Error>>;

    fn refresh_change(&self, number: ChangeNumber) -> BoxFuture<'_, Result<ChangeDetail, Error>>;
}

/// Fetches from one server and writes into the shared [`Store`].
#[derive(Clone)]
pub struct Syncer {
    client: GerritClient,
    store: Arc<Mutex<Store>>,
    server: ServerId,
}

impl Syncer {
    pub fn new(client: GerritClient, store: Arc<Mutex<Store>>, server: ServerId) -> Self {
        Self {
            client,
            store,
            server,
        }
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        // Every store write is a transaction, so a panic in another thread
        // cannot leave half-written data; ignore poisoning.
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The cached result of `query`, for instant start.
    pub fn cached_query(&self, query: &str) -> Result<Option<CachedQuery>, Error> {
        Ok(self.store().load_query(self.server, query)?)
    }

    /// The cached detail of a change.
    pub fn cached_change(&self, number: ChangeNumber) -> Result<Option<CachedChange>, Error> {
        Ok(self.store().load_change_detail(self.server, number)?)
    }

    /// Fetches the first page of `query` and updates the cache.
    ///
    /// If the page matches the start of the cached result (same changes,
    /// order and `updated`), the cache keeps any further pages that were
    /// loaded and only its fetch time moves. Otherwise the cached result is
    /// replaced by the new first page and later pages are dropped; the UI
    /// can load them again.
    ///
    /// On error the cache is untouched.
    pub async fn refresh_query(&self, query: &str, page_size: u32) -> Result<QueryUpdate, Error> {
        let page = self
            .client
            .query_changes(&ChangeQuery::new(query).limit(page_size))
            .await?;
        let now = Timestamp::now();

        let mut store = self.store();
        let cached = store.load_query(self.server, query)?;
        if let Some(c) = &cached
            && first_page_unchanged(c, &page.changes, page.more_changes)
        {
            store.touch_query(self.server, query, now)?;
            return Ok(QueryUpdate {
                changed: false,
                cached_count: c.changes.len(),
                more_changes: c.more_changes,
            });
        }
        store.save_query_page(self.server, query, &page.changes, page.more_changes, now)?;
        Ok(QueryUpdate {
            changed: true,
            cached_count: page.changes.len(),
            more_changes: page.more_changes,
        })
    }

    /// Fetches the page after the cached ones and appends it. With nothing
    /// cached this fetches the first page.
    pub async fn load_more(&self, query: &str, page_size: u32) -> Result<QueryUpdate, Error> {
        let have = self
            .store()
            .load_query(self.server, query)?
            .map_or(0, |c| c.changes.len());
        let start = u32::try_from(have).unwrap_or(u32::MAX);
        let page = self
            .client
            .query_changes(&ChangeQuery::new(query).limit(page_size).start(start))
            .await?;
        let now = Timestamp::now();

        let mut store = self.store();
        let before = store.load_query(self.server, query)?;
        store.append_query_page(self.server, query, &page.changes, page.more_changes, now)?;
        let after = store.load_query(self.server, query)?;
        let count = |q: &Option<CachedQuery>| q.as_ref().map_or(0, |q| q.changes.len());
        let more_changes = after.as_ref().is_some_and(|q| q.more_changes);
        Ok(QueryUpdate {
            changed: count(&before) != count(&after)
                || before.is_some_and(|b| b.more_changes) != more_changes,
            cached_count: count(&after),
            more_changes,
        })
    }

    /// Fetches a change's detail and caches it (and its list summary).
    pub async fn refresh_change(&self, number: ChangeNumber) -> Result<ChangeDetail, Error> {
        let detail = self.client.get_change(number).await?;
        let now = Timestamp::now();
        self.store().save_change_detail(self.server, &detail, now)?;
        Ok(detail)
    }
}

impl RefreshSource for Syncer {
    fn refresh_query<'a>(
        &'a self,
        query: &'a str,
        page_size: u32,
    ) -> BoxFuture<'a, Result<QueryUpdate, Error>> {
        Syncer::refresh_query(self, query, page_size).boxed()
    }

    fn refresh_change(&self, number: ChangeNumber) -> BoxFuture<'_, Result<ChangeDetail, Error>> {
        Syncer::refresh_change(self, number).boxed()
    }
}

/// True if `fresh` (the newly fetched first page) shows nothing the cache
/// does not already show.
fn first_page_unchanged(cached: &CachedQuery, fresh: &[ChangeSummary], more: bool) -> bool {
    let Some(prefix) = cached.changes.get(..fresh.len()) else {
        return false;
    };
    let same = prefix
        .iter()
        .zip(fresh)
        .all(|(a, b)| a.number == b.number && a.updated == b.updated);
    // `more_changes` of a multi-page cache describes its last page. A
    // longer cache is only consistent with a first page that has a
    // successor.
    match cached.changes.len().cmp(&fresh.len()) {
        std::cmp::Ordering::Equal => same && cached.more_changes == more,
        std::cmp::Ordering::Greater => same && more,
        std::cmp::Ordering::Less => false,
    }
}

#[cfg(test)]
mod tests;
