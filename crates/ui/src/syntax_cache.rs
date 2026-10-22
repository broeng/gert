//! The session-wide cache of syntax highlighting, and the background worker
//! that fills it ahead of the user.
//!
//! A diff is immutable for its [`DiffKey`] (revisions are keyed by SHA), so a
//! cached result is always right. Highlighting a big file takes up to a
//! second, so the pages ask the worker to do the files the user is likely to
//! open next ([`SyntaxCache::set_queue`]) and the diff view takes a finished
//! result from the cache instead of computing it ([`SyntaxCache::get`]).
//!
//! The cache is one GPUI entity owned by the shell and shared by the pages,
//! like [`crate::settings::FileFilters`]. The UI thread only does the
//! bookkeeping; fetching and highlighting run on the background executor.

use std::collections::VecDeque;

use gpui_kit::{AppContext as _, Context, Task};
use model::DiffKey;
use sync::Syncer;

use crate::{
    diff::syntax::{DiffSyntax, highlight_diff},
    diff_load::load_diff,
    session::Session,
};

/// How many results the cache keeps (and so the longest queue that is
/// worked on: the worker must not evict what it just made).
pub const MAX_ENTRIES: usize = 64;
/// The most lines (both sides of all entries) the cache keeps. A highlighted
/// line is a small span list, so this holds the memory to a few tens of MB.
pub const MAX_LINES: usize = 300_000;

/// What highlighting a diff gave: `None` when the file has nothing to color
/// (unknown language, a magic file, over the limits). Cached too, so that
/// known-plain files are not tried again.
pub type Highlight = Option<DiffSyntax>;

fn lines_of(h: &Highlight) -> usize {
    h.as_ref().map_or(0, |s| {
        s.old.as_ref().map_or(0, |side| side.len()) + s.new.as_ref().map_or(0, |side| side.len())
    })
}

/// A least-recently-used map bounded by entry count and total lines. Small
/// enough (64 entries) that a vector with move-to-back is the right thing.
struct Lru {
    /// Least recently used first.
    entries: Vec<(DiffKey, Highlight)>,
    lines: usize,
    max_entries: usize,
    max_lines: usize,
}

impl Lru {
    fn new(max_entries: usize, max_lines: usize) -> Self {
        Self {
            entries: Vec::new(),
            lines: 0,
            max_entries,
            max_lines,
        }
    }

    fn position(&self, key: &DiffKey) -> Option<usize> {
        self.entries.iter().position(|(k, _)| k == key)
    }

    #[allow(dead_code, reason = "used by the queue worker, wired in by 8d")]
    fn contains(&self, key: &DiffKey) -> bool {
        self.position(key).is_some()
    }

    /// Marks the entry as most recently used.
    fn get(&mut self, key: &DiffKey) -> Option<Highlight> {
        let i = self.position(key)?;
        let entry = self.entries.remove(i);
        let out = entry.1.clone();
        self.entries.push(entry);
        Some(out)
    }

    fn insert(&mut self, key: DiffKey, value: Highlight) {
        if let Some(i) = self.position(&key) {
            let (_, old) = self.entries.remove(i);
            self.lines -= lines_of(&old);
        }
        self.lines += lines_of(&value);
        self.entries.push((key, value));
        // The newest entry stays even if it alone is over the line bound.
        while self.entries.len() > 1
            && (self.entries.len() > self.max_entries || self.lines > self.max_lines)
        {
            let (_, evicted) = self.entries.remove(0);
            self.lines -= lines_of(&evicted);
        }
    }
}

#[allow(
    dead_code,
    reason = "the queue is fed by the pages in 8d; until then only tests use it"
)]
pub struct SyntaxCache {
    lru: Lru,
    queue: VecDeque<DiffKey>,
    /// The worker; dropping it cancels the item in flight.
    worker: Option<Task<()>>,
}

impl Default for SyntaxCache {
    fn default() -> Self {
        Self::new()
    }
}

impl SyntaxCache {
    pub fn new() -> Self {
        Self::with_limits(MAX_ENTRIES, MAX_LINES)
    }

    fn with_limits(max_entries: usize, max_lines: usize) -> Self {
        Self {
            lru: Lru::new(max_entries, max_lines),
            queue: VecDeque::new(),
            worker: None,
        }
    }

    /// The cached result for `key` (`Some(None)`: known to be plain), and
    /// marks it as recently used.
    pub fn get(&mut self, key: &DiffKey) -> Option<Highlight> {
        self.lru.get(key)
    }

    /// Stores a result, e.g. one a diff view computed itself.
    pub fn insert(&mut self, key: DiffKey, syntax: Highlight) {
        self.lru.insert(key, syntax);
    }

    /// Replaces the work queue: the worker highlights `keys` in order, one at
    /// a time, and drops what it was doing. Keys already cached are skipped;
    /// diffs not in the cache are fetched through the session's syncer (only
    /// when it can sync). The queue is cut at [`MAX_ENTRIES`]. With `enabled`
    /// false (the `syntax_highlighting` preference is off) the queue is just
    /// cleared. Callers pass the resolved preference, and call again when it
    /// turns on.
    #[allow(dead_code, reason = "queued by the pages in 8d")]
    pub fn set_queue(
        &mut self,
        keys: Vec<DiffKey>,
        session: &Session,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        self.worker = None;
        self.queue.clear();
        if !enabled || keys.is_empty() {
            return;
        }
        self.queue
            .extend(keys.into_iter().take(self.lru.max_entries));
        let (syncer, can_sync) = (session.syncer().clone(), session.can_sync());
        self.worker = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(Some(key)) = this.update(cx, |this, _| this.next_key()) else {
                    return;
                };
                let syncer = syncer.clone();
                let job = key.clone();
                let result = cx
                    .background_spawn(async move { highlight_key(syncer, job, can_sync).await })
                    .await;
                let stored = this.update(cx, |this, cx| {
                    match result {
                        Ok(h) => this.lru.insert(key, h),
                        Err(e) => log::debug!("highlighting {} ahead failed: {e}", key.path),
                    }
                    cx.notify();
                });
                if stored.is_err() {
                    return;
                }
            }
        }));
    }

    /// The next queued key that is not cached yet.
    #[allow(dead_code, reason = "part of set_queue")]
    fn next_key(&mut self) -> Option<DiffKey> {
        while let Some(key) = self.queue.pop_front() {
            if !self.lru.contains(&key) {
                return Some(key);
            }
        }
        None
    }

    #[cfg(test)]
    pub(crate) fn queued_for_test(&self) -> usize {
        self.queue.len()
    }
}

/// Gets the diff (cache first, the network for a miss) and highlights it.
/// Blocking: call from the background executor.
#[allow(dead_code, reason = "part of set_queue")]
async fn highlight_key(
    syncer: Syncer,
    key: DiffKey,
    can_sync: bool,
) -> Result<Highlight, crate::diff_load::LoadError> {
    let diff = load_diff(syncer, key.clone(), can_sync).await?;
    Ok(highlight_diff(&key.path, &diff))
}

#[cfg(test)]
mod tests;
