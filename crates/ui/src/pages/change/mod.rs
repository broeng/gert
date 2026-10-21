//! The change page: header, submit requirements, votes, people, commit
//! message, patch set selector, file list with inline diffs, related
//! changes and the change log.
//!
//! The page is a virtualized `list` of [`Block`]s, so the heavy parts (a
//! file list of thousands of entries, many expanded inline diffs, a long
//! log) only cost what is on screen.
//!
//! Data: everything renders from the cache first (`cached_change`,
//! `cached_files`, `cached_related`), then the same calls are refreshed
//! from the server in the background and the page polls the change like
//! the list pages do. Switching the patch set pair loads the file list of
//! the new [`FilesKey`] the same way.
//!
//! Focus: the page itself keeps the focus, so `j`/`k` always move the file
//! cursor. `i` expands the diff under the cursor as a display-only
//! [`DiffView`] (as tall as its content, capped at [`INLINE_MAX_ROWS`] rows,
//! scrolling inside beyond that). Clicking into an inline diff focuses it,
//! and its own keys apply until `Esc` hands the focus back.

mod state;
#[cfg(test)]
mod tests;
mod view;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Duration,
};

use gpui_kit::*;
use jiff::Timestamp;
use model::{
    ChangeDetail, ChangeNumber, DiffContext, DiffKey, FileEntry, FilesKey, RelatedChange,
    RevisionId,
};
use store::CachedFileList;
use sync::PollPolicy;

use crate::{
    actions::*,
    diff::{
        DiffEvent, DiffSizing, DiffStatus, DiffView, Expand,
        actions::{NextFile, PrevFile},
    },
    diff_load::{LoadError, context_label, expanded_context, load_diff},
    nav::{ChangeView, DiffRoute},
    pages::list::Status,
    row::browser_url,
    session::{Session, is_unauthorized},
    settings::DiffSettings,
    tracker::Tracker,
};
use state::{Block, Content, PatchSel, step};

/// Rows an inline diff shows before it scrolls inside.
pub const INLINE_MAX_ROWS: usize = 40;
/// Inline diffs fetched at the same time; the rest wait in a queue.
const MAX_PARALLEL_DIFFS: usize = 3;

pub enum ChangeEvent {
    Unauthorized,
    /// A related change was chosen.
    OpenChange(ChangeNumber),
    /// A file's diff page was chosen.
    OpenDiff(DiffRoute),
}

/// The file list of the current patch set pair.
#[derive(Default)]
struct Files {
    key: Option<FilesKey>,
    list: Vec<FileEntry>,
    reviewed: HashSet<String>,
    /// A list (possibly empty) has arrived for `key`.
    loaded: bool,
    error: Option<String>,
}

struct Related {
    sha: RevisionId,
    changes: Vec<RelatedChange>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Queued,
    Loading,
    Idle,
}

/// One expanded file.
struct Inline {
    key: DiffKey,
    /// Set after an expansion request; replaces the preferred context.
    ctx: Option<DiffContext>,
    view: Option<Entity<DiffView>>,
    phase: Phase,
    error: Option<String>,
    task: Option<Task<()>>,
    _sub: Option<Subscription>,
}

pub struct ChangePage {
    session: Session,
    settings: Entity<DiffSettings>,
    number: ChangeNumber,
    detail: Option<ChangeDetail>,
    fetched_at: Option<Timestamp>,
    /// The cache has been read (an empty cache counts).
    loaded: bool,
    status: Status,
    track: Tracker,
    sel: PatchSel,
    files: Files,
    related: Option<Related>,
    related_cursor: Option<usize>,
    /// Index into `files.list`.
    cursor: usize,
    /// Put the cursor on this file once the list is there.
    want_file: Option<String>,
    /// Scroll to the cursor at the next opportunity (list restored).
    jump: bool,
    inline: HashMap<String, Inline>,
    commit_open: bool,
    list: ListState,
    blocks: Vec<Block>,
    focus: FocusHandle,
    window_active: bool,
    /// The page itself (not an inline diff) had the focus at the last frame.
    has_focus: bool,
    policy: PollPolicy,
    ticks: u32,
    task: Option<Task<()>>,
    files_task: Option<Task<()>>,
    related_task: Option<Task<()>>,
    _ticker: Task<()>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<ChangeEvent> for ChangePage {}

impl Focusable for ChangePage {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ChangePage {
    pub fn new(
        session: Session,
        settings: Entity<DiffSettings>,
        number: ChangeNumber,
        view: ChangeView,
        window_active: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            session,
            settings: settings.clone(),
            number,
            detail: None,
            fetched_at: None,
            loaded: false,
            status: Status::Idle,
            track: Tracker::default(),
            sel: PatchSel {
                base: view.base,
                target: view.target,
            },
            files: Files::default(),
            related: None,
            related_cursor: None,
            cursor: 0,
            want_file: view.file,
            jump: true,
            inline: HashMap::new(),
            commit_open: true,
            list: ListState::new(1, ListAlignment::Top, px(300.)),
            blocks: vec![Block::Header],
            focus: cx.focus_handle(),
            window_active,
            has_focus: true,
            policy: PollPolicy::default(),
            ticks: 0,
            task: None,
            files_task: None,
            related_task: None,
            _ticker: Task::ready(()),
            _subs: Vec::new(),
        };
        this._subs
            .push(cx.observe(&settings, |this, _, cx| this.settings_changed(cx)));
        let syncer = this.session.syncer().clone();
        this.task = Some(cx.spawn(async move |this, cx| {
            let cached = cx
                .background_spawn(async move { syncer.cached_change(number) })
                .await;
            this.update(cx, |this, cx| {
                this.loaded = true;
                match cached {
                    Ok(Some(c)) => {
                        this.detail = Some(c.detail);
                        this.fetched_at = Some(c.fetched_at);
                        this.apply_selection(false, cx);
                    }
                    Ok(None) => this.rebuild_blocks(),
                    Err(e) => {
                        log::warn!("reading cached change {number}: {e}");
                        this.rebuild_blocks();
                    }
                }
                this.tick(cx);
                cx.notify();
            })
            .ok();
        }));
        this._ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                    break;
                }
            }
        });
        this
    }

    #[cfg(test)]
    pub fn detail(&self) -> Option<&ChangeDetail> {
        self.detail.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn focus_handle_for_test(&self) -> FocusHandle {
        self.focus.clone()
    }

    #[cfg(test)]
    pub(crate) fn scroll_to_end_for_test(&mut self) {
        let last = self.blocks.len().saturating_sub(1);
        self.list.scroll_to(ListOffset {
            item_ix: last,
            offset_in_item: px(0.),
        });
    }

    /// The path under the file cursor.
    pub fn cursor_path(&self) -> Option<&str> {
        self.files.list.get(self.cursor).map(|f| f.path.as_str())
    }

    #[cfg(test)]
    pub(crate) fn files_key(&self) -> Option<&FilesKey> {
        self.files.key.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn file_paths(&self) -> Vec<&str> {
        self.files.list.iter().map(|f| f.path.as_str()).collect()
    }

    #[cfg(test)]
    pub(crate) fn expanded_paths(&self) -> Vec<&str> {
        let mut v: Vec<_> = self.inline.keys().map(String::as_str).collect();
        v.sort_unstable();
        v
    }

    #[cfg(test)]
    pub(crate) fn inline_ready(&self, path: &str) -> bool {
        self.inline.get(path).is_some_and(|i| i.view.is_some())
    }

    #[cfg(test)]
    pub(crate) fn inline_context(&self, path: &str) -> Option<DiffContext> {
        self.inline.get(path).map(|i| i.key.context)
    }

    #[cfg(test)]
    pub(crate) fn inline_view(&self, path: &str) -> Option<Entity<DiffView>> {
        self.inline.get(path).and_then(|i| i.view.clone())
    }

    #[cfg(test)]
    pub(crate) fn blocks_for_test(&self) -> &[Block] {
        &self.blocks
    }

    #[cfg(test)]
    pub(crate) fn selection_for_test(
        &self,
    ) -> (Option<model::PatchSetNumber>, Option<model::PatchSetNumber>) {
        self.detail
            .as_ref()
            .map(|d| (self.sel.base_number(d), self.sel.target_number(d)))
            .unwrap_or((None, None))
    }

    /// What to restore when the user comes back to this change.
    pub fn view_state(&self) -> ChangeView {
        ChangeView {
            base: self.sel.base,
            target: self.sel.target,
            file: self.cursor_path().map(str::to_owned),
        }
    }

    // ---- data ----

    fn tick(&mut self, cx: &mut Context<Self>) {
        self.ticks = self.ticks.wrapping_add(1);
        if self.ticks.is_multiple_of(10) {
            cx.notify(); // keeps "updated 3m ago" current
        }
        if !self.session.can_sync() || !self.loaded || self.status.busy() {
            return;
        }
        let now = Timestamp::now();
        if self
            .policy
            .is_due(now, &self.track.state(self.window_active, true))
        {
            self.refresh(cx);
        }
    }

    pub fn set_window_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.window_active != active {
            self.window_active = active;
            self.tick(cx);
        }
    }

    /// Fetches the change detail; the file list and related changes follow
    /// in [`Self::apply_selection`].
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let syncer = self.session.syncer().clone();
        let number = self.number;
        self.status = Status::Refreshing;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { syncer.refresh_change(number).await })
                .await;
            this.update(cx, |this, cx| {
                let now = Timestamp::now();
                match result {
                    Ok(detail) => {
                        this.track.success(now);
                        this.detail = Some(detail);
                        this.fetched_at = Some(now);
                        this.status = Status::Idle;
                        this.apply_selection(true, cx);
                    }
                    Err(e) if is_unauthorized(&e) => {
                        this.status = Status::Failed("unauthorized".into());
                        cx.emit(ChangeEvent::Unauthorized);
                    }
                    Err(e) => {
                        log::warn!("refreshing change {}: {e}", this.number);
                        this.track.failure(now);
                        this.status = Status::Failed(e.to_string());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Works out which file list and related changes the current detail and
    /// patch set pick call for, resets what depended on the old ones, and
    /// loads them (from the cache; also from the server if `network`).
    fn apply_selection(&mut self, network: bool, cx: &mut Context<Self>) {
        let Some(d) = &self.detail else {
            return;
        };
        let key = self.sel.files_key(d);
        let sha = self.sel.target_ref(d).map(|t| t.sha);
        if key != self.files.key {
            if self.want_file.is_none() {
                self.want_file = self.cursor_path().map(str::to_owned);
            }
            self.files = Files {
                key: key.clone(),
                ..Default::default()
            };
            self.inline.clear();
            self.cursor = 0;
            self.jump = true;
        }
        if self.related.as_ref().map(|r| &r.sha) != sha.as_ref() {
            self.related = None;
            self.related_cursor = None;
        }
        self.rebuild_blocks();
        if let Some(key) = key {
            self.load_files(key, network, cx);
        }
        if let Some(sha) = sha {
            self.load_related(sha, network, cx);
        }
    }

    fn load_files(&mut self, key: FilesKey, network: bool, cx: &mut Context<Self>) {
        let syncer = self.session.syncer().clone();
        let sync = network && self.session.can_sync();
        self.files_task = Some(cx.spawn(async move |this, cx| {
            let cached = {
                let (syncer, key) = (syncer.clone(), key.clone());
                cx.background_spawn(async move { syncer.cached_files(&key) })
                    .await
            };
            this.update(cx, |this, cx| match cached {
                Ok(Some(list)) => this.apply_files(&key, list, cx),
                Ok(None) => {}
                Err(e) => log::warn!("reading cached files: {e}"),
            })
            .ok();
            if !sync {
                return;
            }
            let fetched = {
                let key = key.clone();
                cx.background_spawn(async move { syncer.refresh_files(&key).await })
                    .await
            };
            this.update(cx, |this, cx| match fetched {
                Ok(list) => this.apply_files(&key, list, cx),
                Err(e) if is_unauthorized(&e) => cx.emit(ChangeEvent::Unauthorized),
                Err(e) => {
                    log::warn!("fetching files of change {}: {e}", key.change);
                    if this.files.key.as_ref() == Some(&key) && !this.files.loaded {
                        this.files.error = Some(e.to_string());
                        this.rebuild_blocks();
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
    }

    fn apply_files(&mut self, key: &FilesKey, list: CachedFileList, cx: &mut Context<Self>) {
        if self.files.key.as_ref() != Some(key) {
            return; // the user picked another pair meanwhile
        }
        let keep = self
            .want_file
            .take()
            .or_else(|| self.cursor_path().map(str::to_owned));
        self.files.list = list.files;
        self.files.reviewed = list.reviewed.unwrap_or_default().into_iter().collect();
        self.files.loaded = true;
        self.files.error = None;
        self.cursor = keep
            .and_then(|p| self.files.list.iter().position(|f| f.path == p))
            .unwrap_or_else(|| self.cursor.min(self.files.list.len().saturating_sub(1)));
        let paths: HashSet<_> = self.files.list.iter().map(|f| f.path.as_str()).collect();
        self.inline.retain(|p, _| paths.contains(p.as_str()));
        self.rebuild_blocks();
        self.restore_scroll();
        cx.notify();
    }

    fn load_related(&mut self, sha: RevisionId, network: bool, cx: &mut Context<Self>) {
        let syncer = self.session.syncer().clone();
        let sync = network && self.session.can_sync();
        let number = self.number;
        self.related_task = Some(cx.spawn(async move |this, cx| {
            let cached = {
                let (syncer, sha) = (syncer.clone(), sha.clone());
                cx.background_spawn(async move { syncer.cached_related(number, &sha) })
                    .await
            };
            this.update(cx, |this, cx| match cached {
                Ok(Some(c)) => this.apply_related(&sha, c.changes, cx),
                Ok(None) => {}
                Err(e) => log::warn!("reading cached related changes: {e}"),
            })
            .ok();
            if !sync {
                return;
            }
            let fetched = {
                let sha = sha.clone();
                cx.background_spawn(async move { syncer.refresh_related(number, &sha).await })
                    .await
            };
            this.update(cx, |this, cx| match fetched {
                Ok(changes) => this.apply_related(&sha, changes, cx),
                Err(e) if is_unauthorized(&e) => cx.emit(ChangeEvent::Unauthorized),
                Err(e) => log::warn!("fetching related changes of {number}: {e}"),
            })
            .ok();
        }));
    }

    fn apply_related(
        &mut self,
        sha: &RevisionId,
        changes: Vec<RelatedChange>,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .detail
            .as_ref()
            .and_then(|d| self.sel.target_ref(d))
            .map(|t| t.sha);
        if current.as_ref() != Some(sha) {
            return;
        }
        self.related = Some(Related {
            sha: sha.clone(),
            changes,
        });
        self.rebuild_blocks();
        cx.notify();
    }

    // ---- blocks ----

    fn content(&self) -> Content {
        let d = self.detail.as_ref();
        Content {
            loaded: d.is_some(),
            requirements: d.map_or(0, |d| d.submit_requirements.len()),
            labels: d.map_or(0, |d| d.votes.len()),
            files: self.files.list.len(),
            related: self.related.as_ref().map_or(0, |r| r.changes.len()),
            messages: d.map_or(0, |d| d.messages.len()),
        }
    }

    /// Recomputes the block list after the data changed and tells the list
    /// what to re-measure. Blocks that did not move keep their scroll
    /// position; only the changed middle is replaced.
    fn rebuild_blocks(&mut self) {
        let new = state::blocks(self.content());
        let old = &self.blocks;
        let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(new[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        self.list
            .splice(prefix..old.len() - suffix, new.len() - prefix - suffix);
        self.list.remeasure_items(0..new.len());
        self.blocks = new;
    }

    fn block_index(&self, block: Block) -> Option<usize> {
        self.blocks.iter().position(|b| *b == block)
    }

    fn remeasure(&self, block: Block) {
        if let Some(ix) = self.block_index(block) {
            self.list.remeasure_items(ix..ix + 1);
        }
    }

    /// Scrolls so that the file under the cursor is visible.
    fn reveal_cursor(&mut self) {
        if let Some(ix) = self.block_index(Block::File(self.cursor)) {
            self.list.scroll_to_reveal_item(ix);
        }
    }

    /// When the page comes back with the cursor on a far-away file, put the
    /// list at it once the files are there. Items that were never measured
    /// have no height yet, so this positions by index, not by pixels.
    fn restore_scroll(&mut self) {
        if !std::mem::take(&mut self.jump) || self.cursor == 0 {
            return;
        }
        if let Some(ix) = self.block_index(Block::File(self.cursor)) {
            self.list.scroll_to(ListOffset {
                item_ix: ix.saturating_sub(2),
                offset_in_item: px(0.),
            });
        }
    }

    // ---- inline diffs ----

    fn diff_key(&self, path: &str, ctx: Option<DiffContext>, cx: &App) -> Option<DiffKey> {
        let files = self.files.key.as_ref()?;
        let prefs = self.settings.read(cx).resolved();
        Some(DiffKey {
            change: files.change,
            base: files.base.clone(),
            target: files.target.clone(),
            path: path.to_owned(),
            whitespace: prefs.ignore_whitespace,
            context: ctx.unwrap_or(prefs.context),
        })
    }

    fn expand_inline(&mut self, path: &str, cx: &mut Context<Self>) {
        if self.inline.contains_key(path) {
            return;
        }
        let Some(key) = self.diff_key(path, None, cx) else {
            return;
        };
        self.inline.insert(
            path.to_owned(),
            Inline {
                key,
                ctx: None,
                view: None,
                phase: Phase::Queued,
                error: None,
                task: None,
                _sub: None,
            },
        );
        self.remeasure(Block::File(self.index_of(path)));
        self.pump(cx);
    }

    fn collapse_inline(&mut self, path: &str) {
        if self.inline.remove(path).is_some() {
            self.remeasure(Block::File(self.index_of(path)));
        }
    }

    fn index_of(&self, path: &str) -> usize {
        self.files
            .list
            .iter()
            .position(|f| f.path == path)
            .unwrap_or(usize::MAX)
    }

    /// Starts queued fetches while fewer than [`MAX_PARALLEL_DIFFS`] run.
    /// Files are served in list order.
    fn pump(&mut self, cx: &mut Context<Self>) {
        let mut running = self
            .inline
            .values()
            .filter(|i| i.phase == Phase::Loading)
            .count();
        if running >= MAX_PARALLEL_DIFFS {
            return;
        }
        let mut queued: VecDeque<String> = self
            .files
            .list
            .iter()
            .filter(|f| {
                self.inline
                    .get(&f.path)
                    .is_some_and(|i| i.phase == Phase::Queued)
            })
            .map(|f| f.path.clone())
            .collect();
        while running < MAX_PARALLEL_DIFFS {
            let Some(path) = queued.pop_front() else {
                break;
            };
            self.start_fetch(&path, cx);
            running += 1;
        }
    }

    fn start_fetch(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some(inline) = self.inline.get_mut(path) else {
            return;
        };
        inline.phase = Phase::Loading;
        let key = inline.key.clone();
        let syncer = self.session.syncer().clone();
        let can_sync = self.session.can_sync();
        let path = path.to_owned();
        let task_key = key.clone();
        inline.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(load_diff(syncer, task_key, can_sync))
                .await;
            this.update(cx, |this, cx| this.diff_arrived(&path, &key, result, cx))
                .ok();
        }));
    }

    fn diff_arrived(
        &mut self,
        path: &str,
        key: &DiffKey,
        result: Result<model::Diff, LoadError>,
        cx: &mut Context<Self>,
    ) {
        let (prefs, font) = {
            let s = self.settings.read(cx);
            (s.resolved(), s.font_family())
        };
        let Some(inline) = self.inline.get_mut(path) else {
            return;
        };
        if &inline.key != key {
            return; // superseded by a newer request
        }
        inline.phase = Phase::Idle;
        inline.task = None;
        match result {
            Ok(diff) => {
                inline.error = None;
                // Say which context an expanded diff shows; the default
                // context needs no label.
                let status = inline
                    .ctx
                    .map(|c| DiffStatus::Info(format!("Context: {}", context_label(c)).into()));
                match &inline.view {
                    Some(view) => view.update(cx, |v, cx| {
                        v.set_diff(diff, cx);
                        v.set_status(status, cx);
                    }),
                    None => {
                        let view = cx.new(|cx| {
                            let mut v = DiffView::new(diff, prefs, cx);
                            v.set_sizing(
                                DiffSizing::Inline {
                                    max_rows: Some(INLINE_MAX_ROWS),
                                },
                                cx,
                            );
                            v.set_font_family(font, cx);
                            v.set_status(status, cx);
                            v
                        });
                        let owned = path.to_owned();
                        inline._sub =
                            Some(cx.subscribe(&view, move |this, _, ev: &DiffEvent, cx| {
                                this.inline_event(&owned, ev, cx)
                            }));
                        inline.view = Some(view);
                    }
                }
            }
            Err(LoadError::Unauthorized) => cx.emit(ChangeEvent::Unauthorized),
            Err(e) => {
                // A refetch (more context) that fails must not pass
                // silently behind the diff that is still shown.
                if let Some(view) = &inline.view {
                    let msg = format!("Could not load more lines: {e}");
                    view.update(cx, |v, cx| {
                        v.set_status(Some(DiffStatus::Error(msg.into())), cx)
                    });
                }
                inline.error = Some(e.to_string());
            }
        }
        self.remeasure(Block::File(self.index_of(path)));
        if self.cursor_path() == Some(path) {
            self.reveal_cursor(); // the diff grew below the row
        }
        self.pump(cx);
        cx.notify();
    }

    fn inline_event(&mut self, path: &str, ev: &DiffEvent, cx: &mut Context<Self>) {
        match ev {
            DiffEvent::ModeChanged(mode) => {
                let mode = *mode;
                self.settings.update(cx, |s, cx| {
                    s.update_session(|p| p.view_mode = Some(mode), cx)
                });
            }
            DiffEvent::ExpandRequested(expand) => self.expand_context(path, expand, cx),
        }
    }

    /// An inline diff asked for more lines: refetch with a larger context
    /// (see [`expanded_context`]); the view keeps its cursor in `set_diff`.
    fn expand_context(&mut self, path: &str, expand: &Expand, cx: &mut Context<Self>) {
        let Some(current) = self.inline.get(path).map(|i| i.key.context) else {
            return;
        };
        let ctx = expanded_context(current, expand);
        let Some(key) = self.diff_key(path, Some(ctx), cx) else {
            return;
        };
        if let Some(inline) = self.inline.get_mut(path) {
            inline.ctx = Some(ctx);
            inline.key = key;
            inline.phase = Phase::Queued;
            if let Some(view) = &inline.view {
                let msg = format!("Loading context: {}\u{2026}", context_label(ctx));
                view.update(cx, |v, cx| {
                    v.set_status(Some(DiffStatus::Busy(msg.into())), cx)
                });
            }
        }
        self.remeasure(Block::File(self.index_of(path)));
        self.pump(cx);
        cx.notify();
    }

    /// Whitespace, context, mode, font ...: apply to the open inline diffs
    /// and refetch those whose key changed.
    fn settings_changed(&mut self, cx: &mut Context<Self>) {
        let (prefs, font) = {
            let s = self.settings.read(cx);
            (s.resolved(), s.font_family())
        };
        let paths: Vec<String> = self.inline.keys().cloned().collect();
        for path in paths {
            let key = self.diff_key(&path, self.inline.get(&path).and_then(|i| i.ctx), cx);
            let Some(inline) = self.inline.get_mut(&path) else {
                continue;
            };
            if let Some(view) = &inline.view {
                view.update(cx, |v, cx| {
                    v.set_preferences(prefs.clone(), cx);
                    v.set_font_family(font.clone(), cx);
                });
            }
            if let Some(key) = key
                && key != inline.key
            {
                inline.key = key;
                inline.phase = Phase::Queued;
            }
        }
        self.pump(cx);
        cx.notify();
    }

    // ---- actions ----

    fn move_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = step(self.cursor, self.files.list.len(), delta);
        if n != self.cursor {
            self.cursor = n;
            self.reveal_cursor();
            cx.notify();
        }
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(1, cx);
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(-1, cx);
    }

    fn next_file(&mut self, _: &NextFile, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(1, cx);
    }

    fn prev_file(&mut self, _: &PrevFile, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(-1, cx);
    }

    fn open_selected(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
        self.open_file(self.cursor, cx);
    }

    fn open_file(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(d) = &self.detail else {
            return;
        };
        let (Some(file), Some(target)) = (self.files.list.get(ix), self.sel.target_number(d))
        else {
            return;
        };
        self.cursor = ix;
        cx.emit(ChangeEvent::OpenDiff(DiffRoute {
            change: self.number,
            base: self.sel.base_number(d),
            target,
            path: file.path.clone(),
        }));
    }

    fn toggle_inline_action(&mut self, _: &ToggleInline, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_inline_at(self.cursor, cx);
    }

    fn toggle_inline_at(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(path) = self.files.list.get(ix).map(|f| f.path.clone()) else {
            return;
        };
        if self.inline.contains_key(&path) {
            self.collapse_inline(&path);
        } else {
            self.expand_inline(&path, cx);
        }
        cx.notify();
    }

    fn toggle_all_inline(&mut self, _: &ToggleAllInline, _: &mut Window, cx: &mut Context<Self>) {
        let paths: Vec<String> = self.files.list.iter().map(|f| f.path.clone()).collect();
        if paths.is_empty() {
            return;
        }
        if paths.iter().all(|p| self.inline.contains_key(p)) {
            self.inline.clear();
        } else {
            for p in &paths {
                if !self.inline.contains_key(p)
                    && let Some(key) = self.diff_key(p, None, cx)
                {
                    self.inline.insert(
                        p.clone(),
                        Inline {
                            key,
                            ctx: None,
                            view: None,
                            phase: Phase::Queued,
                            error: None,
                            task: None,
                            _sub: None,
                        },
                    );
                }
            }
            self.pump(cx);
        }
        self.rebuild_blocks();
        cx.notify();
    }

    fn change_patch_sets(
        &mut self,
        f: impl FnOnce(&mut PatchSel, &ChangeDetail),
        cx: &mut Context<Self>,
    ) {
        let Some(d) = &self.detail else {
            return;
        };
        f(&mut self.sel, d);
        self.want_file = None;
        self.apply_selection(true, cx);
        cx.notify();
    }

    fn next_target(&mut self, _: &NextTarget, _: &mut Window, cx: &mut Context<Self>) {
        self.change_patch_sets(|s, d| s.cycle_target(d, true), cx);
    }

    fn prev_target(&mut self, _: &PrevTarget, _: &mut Window, cx: &mut Context<Self>) {
        self.change_patch_sets(|s, d| s.cycle_target(d, false), cx);
    }

    fn next_base(&mut self, _: &NextBase, _: &mut Window, cx: &mut Context<Self>) {
        self.change_patch_sets(|s, d| s.cycle_base(d, true), cx);
    }

    fn prev_base(&mut self, _: &PrevBase, _: &mut Window, cx: &mut Context<Self>) {
        self.change_patch_sets(|s, d| s.cycle_base(d, false), cx);
    }

    fn toggle_commit(&mut self, _: &ToggleCommit, _: &mut Window, cx: &mut Context<Self>) {
        self.commit_open = !self.commit_open;
        self.remeasure(Block::Commit);
        cx.notify();
    }

    fn move_related(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(r) = &self.related else {
            return;
        };
        let start = self
            .related_cursor
            .or_else(|| r.changes.iter().position(|c| c.number == self.number));
        let n = match start {
            Some(s) => step(s, r.changes.len(), delta),
            None => 0,
        };
        self.related_cursor = Some(n);
        if let Some(ix) = self.block_index(Block::Related) {
            self.list.scroll_to_reveal_item(ix);
        }
        self.remeasure(Block::Related);
        cx.notify();
    }

    fn next_related(&mut self, _: &NextRelated, _: &mut Window, cx: &mut Context<Self>) {
        self.move_related(1, cx);
    }

    fn prev_related(&mut self, _: &PrevRelated, _: &mut Window, cx: &mut Context<Self>) {
        self.move_related(-1, cx);
    }

    fn open_related(&mut self, _: &OpenRelated, _: &mut Window, cx: &mut Context<Self>) {
        let number = self
            .related_cursor
            .and_then(|i| self.related.as_ref()?.changes.get(i))
            .map(|c| c.number);
        if let Some(n) = number.filter(|n| *n != self.number) {
            cx.emit(ChangeEvent::OpenChange(n));
        }
    }

    fn focus_files(&mut self, _: &FocusFiles, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus.is_focused(window) {
            cx.propagate(); // nothing to hand back: let Esc close the help
        } else {
            window.focus(&self.focus, cx);
        }
    }

    fn scroll_page(&mut self, down: bool) {
        let h = self.list.viewport_bounds().size.height * 0.9;
        self.list.scroll_by(if down { h } else { -h });
    }

    fn page_down(&mut self, _: &ScrollPageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_page(true);
        cx.notify();
    }

    fn page_up(&mut self, _: &ScrollPageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_page(false);
        cx.notify();
    }

    fn refresh_now(&mut self, _: &Refresh, _: &mut Window, cx: &mut Context<Self>) {
        self.track.invalidate();
        self.tick(cx);
    }

    fn open_in_browser(&mut self, _: &OpenInBrowser, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(d) = &self.detail {
            cx.open_url(&browser_url(
                &self.session.base_url,
                &d.summary.project,
                self.number,
            ));
        }
    }
}
