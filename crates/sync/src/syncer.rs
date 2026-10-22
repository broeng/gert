use std::sync::{Arc, Mutex, MutexGuard};

use futures::future::{BoxFuture, FutureExt};
use gerrit_api::{ChangeQuery, DiffParams, GerritClient};
use jiff::Timestamp;
use model::{
    ChangeDetail, ChangeNumber, ChangeSummary, Diff, DiffKey, DiffPreferences, FileEntry, FilesKey,
    RelatedChange, RevisionId,
};
use store::{
    CachedChange, CachedDiff, CachedDiffPreferences, CachedFileList, CachedQuery, CachedRelated,
    PrunePolicy, ServerId, Store,
};

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
    /// Serializes everything that talks to the server about drafts
    /// (uploads, publishing, refreshing the draft list), so that a refresh
    /// never sees a draft that is uploaded but not yet recorded as such.
    drafts_lock: Arc<futures::lock::Mutex<()>>,
}

impl Syncer {
    pub fn new(client: GerritClient, store: Arc<Mutex<Store>>, server: ServerId) -> Self {
        Self {
            client,
            store,
            server,
            drafts_lock: Arc::default(),
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

    /// The cached file list of a comparison.
    pub fn cached_files(&self, key: &FilesKey) -> Result<Option<CachedFileList>, Error> {
        Ok(self.store().load_file_list(self.server, key)?)
    }

    /// Fetches the file list of `key` (and, for signed-in users, which
    /// files are marked reviewed) and caches it. The reviewed marks are
    /// extra: if only that request fails, the list is still stored and the
    /// previously cached marks stay.
    pub async fn refresh_files(&self, key: &FilesKey) -> Result<CachedFileList, Error> {
        let files = self
            .client
            .get_files(
                key.change,
                &key.target.sha,
                key.base.as_ref().map(|b| b.number),
            )
            .await?;
        let reviewed = if self.client.is_authenticated() {
            match self
                .client
                .get_reviewed_files(key.change, &key.target.sha)
                .await
            {
                Ok(paths) => Some(paths),
                Err(e) => {
                    log::warn!("reviewed marks of change {} unavailable: {e}", key.change);
                    None
                }
            }
        } else {
            None
        };
        let now = Timestamp::now();

        let mut store = self.store();
        store.save_file_list(self.server, key, &files, reviewed.as_deref(), now)?;
        Ok(CachedFileList {
            files,
            reviewed: match reviewed {
                Some(r) => Some(r),
                None => store
                    .load_file_list(self.server, key)?
                    .and_then(|c| c.reviewed),
            },
            fetched_at: now,
        })
    }

    /// The cached diff of a file.
    pub fn cached_diff(&self, key: &DiffKey) -> Result<Option<CachedDiff>, Error> {
        Ok(self.store().load_diff(self.server, key)?)
    }

    /// Fetches the diff of one file and caches it. Afterwards stale cache
    /// rows are pruned (see [`PrunePolicy`]); a failure to prune is logged,
    /// not returned, because the diff itself was stored.
    pub async fn refresh_diff(&self, key: &DiffKey) -> Result<Diff, Error> {
        let diff = self
            .client
            .get_diff(
                key.change,
                &key.target.sha,
                &key.path,
                &DiffParams::for_key(key),
            )
            .await?;
        let now = Timestamp::now();

        let mut store = self.store();
        store.save_diff(self.server, key, &diff, now)?;
        if let Err(e) = store.prune(&PrunePolicy::default(), now) {
            log::warn!("pruning the diff cache failed: {e}");
        }
        Ok(diff)
    }

    /// Fetches the diff of the file after `current.path` in `files` unless
    /// it is cached already, so that moving to the next file is instant.
    /// Returns the key that was fetched, or `None` if there was nothing to
    /// do: `current` is the last file or not in the list, or the next diff
    /// is cached. Errors are those of the fetch.
    pub async fn prefetch_next_diff(
        &self,
        current: &DiffKey,
        files: &[FileEntry],
    ) -> Result<Option<DiffKey>, Error> {
        let Some(position) = files.iter().position(|f| f.path == current.path) else {
            return Ok(None);
        };
        let Some(next) = files.get(position + 1) else {
            return Ok(None);
        };
        let key = current.with_path(&next.path);
        if self.store().has_diff(self.server, &key)? {
            return Ok(None);
        }
        self.refresh_diff(&key).await?;
        Ok(Some(key))
    }

    /// The cached related changes of a revision.
    pub fn cached_related(
        &self,
        change: ChangeNumber,
        revision: &RevisionId,
    ) -> Result<Option<CachedRelated>, Error> {
        Ok(self.store().load_related(self.server, change, revision)?)
    }

    /// Fetches the changes related to a revision and caches them.
    pub async fn refresh_related(
        &self,
        change: ChangeNumber,
        revision: &RevisionId,
    ) -> Result<Vec<RelatedChange>, Error> {
        let related = self.client.get_related(change, revision).await?;
        let now = Timestamp::now();
        self.store()
            .save_related(self.server, change, revision, &related, now)?;
        Ok(related)
    }

    /// The cached diff preferences of the signed-in user.
    pub fn cached_diff_preferences(&self) -> Result<Option<CachedDiffPreferences>, Error> {
        Ok(self.store().load_diff_preferences(self.server)?)
    }

    /// Fetches the user's diff preferences from the server and caches
    /// them. Anonymous sessions have none: this returns `None` without a
    /// request and leaves the cache alone.
    pub async fn refresh_diff_preferences(&self) -> Result<Option<DiffPreferences>, Error> {
        if !self.client.is_authenticated() {
            return Ok(None);
        }
        let prefs = self.client.get_diff_preferences().await?;
        let now = Timestamp::now();
        self.store()
            .save_diff_preferences(self.server, &prefs, now)?;
        Ok(Some(prefs))
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

mod actions;
mod comments;
mod edits;

pub use actions::NewChange;
pub use comments::{CommentsView, DraftSave, PublishOutcome, RetryReport};
pub use edits::ReviewerAdded;
pub use store::DraftCounts;

#[cfg(test)]
mod tests;
