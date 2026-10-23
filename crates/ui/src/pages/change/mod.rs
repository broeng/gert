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
//! and its own keys apply until `Esc` leaves the page.

mod quick_vote;
mod state;
#[cfg(test)]
mod tests;
mod thread_list;
pub(crate) mod threads;
mod view;

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use gpui_kit::*;
use jiff::Timestamp;
use model::{
    ChangeDetail, ChangeNumber, DiffKey, DiffPair, FileCommentCounts, FileEntry, FilesKey,
    RelatedChange, RevisionId, Thread, ThreadId,
};
use store::CachedFileList;
use sync::PollPolicy;

use crate::{
    action_menu,
    actions::*,
    comments::{ChangeComments, CommentsEvent},
    diff::{
        DiffEvent, DiffSizing, DiffStatus, DiffView, ThreadContext,
        actions::{NextFile, PrevFile},
    },
    diff_load::{LoadError, load_diff},
    filters::{FileMatcher, FileStyle, next_unskipped, skips},
    hint::Hint,
    nav::{ChangeView, DiffRoute},
    outbox::DraftOutbox,
    pages::list::Status,
    reply, reviewed,
    row::browser_url,
    session::{Session, is_unauthorized},
    settings::{DiffSettings, FileFilters, PageLayout},
    syntax_cache::{SyntaxCache, ahead_paths, diff_key},
    tracker::Tracker,
};
use state::{Block, Content, PatchSel, step};
use threads::ThreadFilter;

/// Rows an inline diff shows before it scrolls inside.
pub const INLINE_MAX_ROWS: usize = 40;
/// Inline diffs fetched at the same time; the rest wait in a queue.
const MAX_PARALLEL_DIFFS: usize = 3;

pub enum ChangeEvent {
    Unauthorized,
    /// A related change was clicked: a new history entry.
    OpenChange(ChangeNumber),
    /// Shift-J / Shift-K stepped to a related change: replaces the current
    /// history entry, so that Back leaves the walk.
    ReplaceChange(ChangeNumber),
    /// A vote went through and the next change of the chain is open: like
    /// `ReplaceChange`, and the new page shows the note.
    Advance(ChangeNumber, String),
    /// A file's diff page was chosen.
    OpenDiff(DiffRoute),
    /// A refresh succeeded: the server is reachable.
    Refreshed,
    /// `a`: open the reply dialog for this change.
    Reply(Box<ChangeDetail>),
    /// `m`: open the change action menu for this change.
    Actions(Box<ChangeDetail>),
    /// `f m` or the Manage filters button: the dialog for the file filter
    /// rules, with the change's project at hand.
    ManageFilters {
        project: String,
    },
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
    /// Session layout shared by the change pages.
    layout: Entity<PageLayout>,
    /// The relation chain is drawn as the side panel (wanted, wide enough
    /// and there is a chain), as of the last render.
    panel_shown: bool,
    /// Scroll position of the relation chain box or panel.
    related_scroll: ScrollHandle,
    /// The file filter rules and which are on, shared by all pages.
    filters: Entity<FileFilters>,
    /// The syntax worker's cache, shared by all pages.
    syntax: Entity<SyntaxCache>,
    /// What this page last asked the syntax worker to do.
    queued: Vec<DiffKey>,
    /// The filter style of each entry of `files.list`.
    styles: Vec<FileStyle>,
    /// Indexes into `files.list` of the files the filters do not hide. The
    /// file blocks, the cursor and the row clicks all index this list.
    visible: Vec<usize>,
    /// Index into `visible`.
    cursor: usize,
    /// Put the cursor on this file once the list is there.
    want_file: Option<String>,
    /// Scroll to the cursor at the next opportunity (list restored).
    jump: bool,
    inline: HashMap<String, Inline>,
    commit_open: bool,
    /// Change log entries the user unfolded, by message id; the others show
    /// as one line.
    open_messages: HashSet<String>,
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
    /// A quick vote is on its way.
    voting: Option<Task<()>>,
    /// The change's comment threads, shown in the inline diffs.
    comments: Entity<ChangeComments>,
    outbox: Entity<DraftOutbox>,
    /// The threads as last read from `comments`.
    threads: Arc<Vec<Thread>>,
    /// The filter the user picked; `None` is the default for the threads.
    thread_filter: Option<ThreadFilter>,
    /// Indexes into `threads` of those the list shows.
    listed: Vec<usize>,
    /// The thread under the thread cursor (`shift-n` / `shift-p`).
    thread_cursor: Option<ThreadId>,
    /// Comment numbers per file for the pair being compared.
    counts: BTreeMap<String, FileCommentCounts>,
    /// The patch-set level threads, as cards (made once the change is
    /// there).
    ps_view: Option<Entity<DiffView>>,
    ps_sub: Option<Subscription>,
    /// A note for the user (why a key did nothing, a failed request).
    hint: Hint,
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
    // The shared session entities (settings, layout, outbox) are separate
    // arguments, as everywhere else in the shell.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session: Session,
        settings: Entity<DiffSettings>,
        layout: Entity<PageLayout>,
        filters: Entity<FileFilters>,
        syntax: Entity<SyntaxCache>,
        number: ChangeNumber,
        view: ChangeView,
        window_active: bool,
        outbox: Entity<DraftOutbox>,
        cx: &mut Context<Self>,
    ) -> Self {
        let comments = outbox.update(cx, |o, cx| o.comments(number, cx));
        let mut this = Self {
            session,
            settings: settings.clone(),
            layout: layout.clone(),
            filters: filters.clone(),
            syntax,
            queued: Vec::new(),
            styles: Vec::new(),
            visible: Vec::new(),
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
            panel_shown: false,
            related_scroll: ScrollHandle::new(),
            cursor: 0,
            want_file: view.file,
            jump: true,
            inline: HashMap::new(),
            commit_open: true,
            open_messages: HashSet::new(),
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
            voting: None,
            comments: comments.clone(),
            outbox,
            threads: Arc::default(),
            thread_filter: None,
            listed: Vec::new(),
            thread_cursor: None,
            counts: BTreeMap::new(),
            ps_view: None,
            ps_sub: None,
            hint: Hint::default(),
            _ticker: Task::ready(()),
            _subs: Vec::new(),
        };
        this._subs
            .push(cx.observe(&settings, |this, _, cx| this.settings_changed(cx)));
        this._subs.push(cx.observe(&layout, |_, _, cx| cx.notify()));
        this._subs
            .push(cx.observe(&filters, |this, _, cx| this.filters_changed(cx)));
        this._subs
            .push(cx.subscribe(&comments, |this, _, ev, cx| match ev {
                CommentsEvent::Changed => this.apply_threads(cx),
                CommentsEvent::Unauthorized => cx.emit(ChangeEvent::Unauthorized),
            }));
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
    pub(crate) fn panel_shown_for_test(&self) -> bool {
        self.panel_shown
    }

    #[cfg(test)]
    pub(crate) fn related_scroll_for_test(&self) -> ScrollHandle {
        self.related_scroll.clone()
    }

    #[cfg(test)]
    pub(crate) fn list_for_test(&self) -> ListState {
        self.list.clone()
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
        let &ix = self.visible.get(self.cursor)?;
        self.files.list.get(ix).map(|f| f.path.as_str())
    }

    /// The paths of all the files of the change, hidden ones too.
    pub(crate) fn changed_paths(&self) -> Vec<String> {
        self.files.list.iter().map(|f| f.path.clone()).collect()
    }

    #[cfg(test)]
    pub(crate) fn files_key(&self) -> Option<&FilesKey> {
        self.files.key.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn file_paths(&self) -> Vec<&str> {
        self.files.list.iter().map(|f| f.path.as_str()).collect()
    }

    /// The paths of the files the list shows (the filters hide the rest).
    #[cfg(test)]
    pub(crate) fn visible_paths(&self) -> Vec<&str> {
        self.visible
            .iter()
            .map(|&i| self.files.list[i].path.as_str())
            .collect()
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
    pub(crate) fn inline_view(&self, path: &str) -> Option<Entity<DiffView>> {
        self.inline.get(path).and_then(|i| i.view.clone())
    }

    #[cfg(test)]
    pub(crate) fn counts_for_test(&self, path: &str) -> Option<FileCommentCounts> {
        self.counts.get(path).copied()
    }

    /// The ids of the threads the list shows, in order.
    #[cfg(test)]
    pub(crate) fn listed_for_test(&self) -> Vec<String> {
        self.listed
            .iter()
            .map(|&i| match &self.threads[i].id {
                ThreadId::Comment(id) => id.clone(),
                ThreadId::Draft(id) => format!("draft{}", id.0),
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn threads_for_test(&self) -> Arc<Vec<Thread>> {
        self.threads.clone()
    }

    #[cfg(test)]
    pub(crate) fn thread_cursor_for_test(&self) -> Option<ThreadId> {
        self.thread_cursor.clone()
    }

    #[cfg(test)]
    pub(crate) fn ps_view_for_test(&self) -> Option<Entity<DiffView>> {
        self.ps_view.clone()
    }

    #[cfg(test)]
    pub(crate) fn is_reviewed_for_test(&self, path: &str) -> bool {
        self.files.reviewed.contains(path)
    }

    #[cfg(test)]
    pub(crate) fn quick_buttons_for_test(&self, cx: &App) -> Vec<crate::reply::plan::QuickButton> {
        self.quick_buttons(cx)
    }

    #[cfg(test)]
    pub(crate) fn hint_for_test(&self) -> Option<(String, bool)> {
        self.hint.current().map(|(t, e)| (t.to_owned(), e))
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
                        cx.emit(ChangeEvent::Refreshed);
                        this.track.success(now);
                        // A change that moved may have new comments.
                        let moved = this
                            .detail
                            .as_ref()
                            .is_some_and(|d| d.summary.updated != detail.summary.updated);
                        if moved {
                            this.comments.update(cx, |c, cx| c.change_updated(cx));
                        }
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
            self.styles.clear();
            self.visible.clear();
            self.cursor = 0;
            self.jump = true;
        }
        if self.related.as_ref().map(|r| &r.sha) != sha.as_ref() {
            self.related = None;
        }
        self.read_threads(cx);
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
        self.compute_styles(cx);
        self.queue_syntax(cx);
        self.cursor = self.cursor_on(keep.as_deref());
        let paths: HashSet<_> = self.files.list.iter().map(|f| f.path.as_str()).collect();
        self.inline.retain(|p, _| paths.contains(p.as_str()));
        self.recount();
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
        self.reveal_current_related();
        self.rebuild_blocks();
        cx.notify();
    }

    // ---- file filters ----

    /// Styles every file with the rules that are on for this change's
    /// project, and lists the ones that stay visible.
    fn compute_styles(&mut self, cx: &App) {
        let matcher = match &self.detail {
            Some(d) => self.filters.read(cx).matcher(&d.summary.project),
            None => FileMatcher::default(),
        };
        self.styles = self
            .files
            .list
            .iter()
            .map(|f| matcher.style(&f.path))
            .collect();
        self.visible = (0..self.styles.len())
            .filter(|&i| !self.styles[i].hidden)
            .collect();
    }

    /// The cursor (an index into `visible`) for the file at `path`, else the
    /// current one kept in range.
    fn cursor_on(&self, path: Option<&str>) -> usize {
        path.and_then(|p| {
            self.visible
                .iter()
                .position(|&i| self.files.list[i].path == p)
        })
        .unwrap_or_else(|| self.cursor.min(self.visible.len().saturating_sub(1)))
    }

    /// A rule was toggled or the rules were replaced: restyle, and keep the
    /// cursor on its file if that is still shown.
    fn filters_changed(&mut self, cx: &mut Context<Self>) {
        let keep = self.cursor_path().map(str::to_owned);
        self.compute_styles(cx);
        self.queue_syntax(cx);
        self.cursor = self.cursor_on(keep.as_deref());
        self.rebuild_blocks();
        cx.notify();
    }

    /// `f 1` .. `f 9`: toggles the filter button at `position`.
    fn toggle_filter(&mut self, action: &ToggleFilter, _: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = &self.detail else {
            return;
        };
        let name = self
            .filters
            .read(cx)
            .applicable(&d.summary.project)
            .get(action.position)
            .map(|r| r.name.clone());
        match name {
            Some(name) => self.filters.update(cx, |f, cx| f.toggle(&name, cx)),
            None => self
                .hint
                .say("No such file filter", false, cx, |p| &mut p.hint),
        }
    }

    fn manage_filters(&mut self, _: &ManageFilters, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(d) = &self.detail {
            cx.emit(ChangeEvent::ManageFilters {
                project: d.summary.project.clone(),
            });
        }
    }

    // ---- blocks ----

    fn content(&self) -> Content {
        let d = self.detail.as_ref();
        Content {
            loaded: d.is_some(),
            files: self.visible.len(),
            threads: self.listed.len(),
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

    fn diff_key(&self, path: &str, cx: &App) -> Option<DiffKey> {
        let files = self.files.key.as_ref()?;
        let prefs = self.settings.read(cx).resolved();
        Some(diff_key(files, path, prefs.ignore_whitespace))
    }

    /// Has the syntax worker highlight the file `]` would open first (the
    /// first one that is not hidden, dimmed or a magic file), so that its
    /// colors are there by the time the diff page opens. Called whenever
    /// the list, the filters, the whitespace option or the preference may
    /// have changed; does nothing when the queue would be the same.
    fn queue_syntax(&mut self, cx: &mut Context<Self>) {
        let enabled = self.settings.read(cx).resolved().syntax_highlighting;
        let keys: Vec<DiffKey> = match &self.files.key {
            Some(files) if enabled && self.files.loaded => {
                let whitespace = self.settings.read(cx).resolved().ignore_whitespace;
                ahead_paths(&self.files.list, &self.styles, None)
                    .into_iter()
                    .take(1)
                    .map(|p| diff_key(files, p, whitespace.clone()))
                    .collect()
            }
            _ => Vec::new(),
        };
        if keys == self.queued {
            return;
        }
        self.queued = keys.clone();
        let session = self.session.clone();
        self.syntax
            .update(cx, |c, cx| c.set_queue(keys, &session, enabled, cx));
    }

    fn expand_inline(&mut self, path: &str, cx: &mut Context<Self>) {
        if self.inline.contains_key(path) {
            return;
        }
        let Some(key) = self.diff_key(path, cx) else {
            return;
        };
        self.inline.insert(
            path.to_owned(),
            Inline {
                key,
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
        self.visible
            .iter()
            .position(|&i| self.files.list[i].path == path)
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
        let context = self.thread_context(path);
        let threads = self.comments.read(cx).threads();
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
                match &inline.view {
                    Some(view) => view.update(cx, |v, cx| {
                        v.set_diff(diff, cx);
                        v.set_status(None, cx);
                    }),
                    None => {
                        let can_comment = self.session.can_comment();
                        let view = cx.new(|cx| {
                            let mut v = DiffView::new(diff, prefs, cx);
                            v.set_can_comment(can_comment, cx);
                            v.set_sizing(
                                DiffSizing::Inline {
                                    max_rows: Some(INLINE_MAX_ROWS),
                                },
                                cx,
                            );
                            v.set_font_family(font, cx);
                            if let Some(context) = context {
                                v.set_threads(context, threads, cx);
                            }
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
                // A refetch (other whitespace option) that fails must not
                // pass silently behind the diff that is still shown.
                if let Some(view) = &inline.view {
                    let msg = format!("Could not load the diff: {e}");
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

    /// Which file of which patch set pair an inline diff shows.
    fn thread_context(&self, path: &str) -> Option<ThreadContext> {
        let d = self.detail.as_ref()?;
        Some(ThreadContext {
            pair: DiffPair {
                base: self.sel.base_number(d),
                target: self.sel.target_number(d)?,
            },
            path: path.to_owned(),
            old_path: self
                .files
                .list
                .iter()
                .find(|f| f.path == path)
                .and_then(|f| f.old_path.clone()),
        })
    }

    /// Hands the change's threads to every inline diff.
    fn apply_threads(&mut self, cx: &mut Context<Self>) {
        self.read_threads(cx);
        let threads = self.threads.clone();
        let views: Vec<_> = self
            .inline
            .iter()
            .filter_map(|(p, i)| Some((p.clone(), i.view.clone()?)))
            .collect();
        for (path, view) in views {
            if let Some(context) = self.thread_context(&path) {
                let threads = threads.clone();
                view.update(cx, |v, cx| v.set_threads(context, threads, cx));
            }
        }
    }

    fn inline_event(&mut self, path: &str, ev: &DiffEvent, cx: &mut Context<Self>) {
        match ev {
            DiffEvent::Draft(request) => {
                let (change, request) = (self.number, request.clone());
                let detail = self.detail.clone();
                self.outbox
                    .update(cx, |o, cx| o.request(change, request, detail.as_ref(), cx));
            }
            DiffEvent::ModeChanged(mode) => {
                let mode = *mode;
                self.settings.update(cx, |s, cx| {
                    s.update_session(|p| p.view_mode = Some(mode), cx)
                });
            }
            DiffEvent::RowsChanged => {
                // Folds were opened or the layout changed: the row is
                // taller or shorter now.
                self.remeasure(Block::File(self.index_of(path)));
                if self.cursor_path() == Some(path) {
                    self.reveal_cursor();
                }
                cx.notify();
            }
            DiffEvent::Highlighted(_) => {}
        }
    }

    /// Whitespace, context, mode, font ...: apply to the open inline diffs
    /// and refetch those whose key changed (only whitespace does).
    fn settings_changed(&mut self, cx: &mut Context<Self>) {
        let (prefs, font) = {
            let s = self.settings.read(cx);
            (s.resolved(), s.font_family())
        };
        if let Some(view) = &self.ps_view {
            view.update(cx, |v, cx| {
                v.set_preferences(prefs.clone(), cx);
                v.set_font_family(font.clone(), cx);
            });
        }
        self.queue_syntax(cx);
        let paths: Vec<String> = self.inline.keys().cloned().collect();
        for path in paths {
            let key = self.diff_key(&path, cx);
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
        let n = step(self.cursor, self.visible.len(), delta);
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

    /// `]` / `[`: the next / previous file that is not dimmed.
    fn jump_file(&mut self, forward: bool, cx: &mut Context<Self>) {
        let skipped = |i: usize| skips(&self.styles[self.visible[i]]);
        if let Some(n) = next_unskipped(self.visible.len(), self.cursor, forward, skipped) {
            self.cursor = n;
            self.reveal_cursor();
            cx.notify();
        }
    }

    fn next_file(&mut self, _: &NextFile, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_file(true, cx);
    }

    fn prev_file(&mut self, _: &PrevFile, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_file(false, cx);
    }

    fn open_selected(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
        self.open_file(self.cursor, cx);
    }

    fn open_file(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(d) = &self.detail else {
            return;
        };
        let file = self.visible.get(ix).and_then(|&i| self.files.list.get(i));
        let (Some(file), Some(target)) = (file, self.sel.target_number(d)) else {
            return;
        };
        self.cursor = ix;
        cx.emit(ChangeEvent::OpenDiff(DiffRoute {
            change: self.number,
            base: self.sel.base_number(d),
            target,
            path: file.path.clone(),
            thread: None,
        }));
    }

    fn toggle_inline_action(&mut self, _: &ToggleInline, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_inline_at(self.cursor, cx);
    }

    fn toggle_inline_at(&mut self, ix: usize, cx: &mut Context<Self>) {
        let file = self.visible.get(ix).and_then(|&i| self.files.list.get(i));
        let Some(path) = file.map(|f| f.path.clone()) else {
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
        let paths: Vec<String> = self
            .visible
            .iter()
            .map(|&i| self.files.list[i].path.clone())
            .collect();
        if paths.is_empty() {
            return;
        }
        if paths.iter().all(|p| self.inline.contains_key(p)) {
            self.inline.clear();
        } else {
            for p in &paths {
                if !self.inline.contains_key(p)
                    && let Some(key) = self.diff_key(p, cx)
                {
                    self.inline.insert(
                        p.clone(),
                        Inline {
                            key,
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
        self.remeasure(Block::Header);
        cx.notify();
    }

    /// Folds the change log entry `ix` out to all its lines, or back to one.
    pub(super) fn toggle_message(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(id) = self
            .detail
            .as_ref()
            .and_then(|d| d.messages.get(ix))
            .map(|m| m.id.clone())
        else {
            return;
        };
        if !self.open_messages.remove(&id) {
            self.open_messages.insert(id);
        }
        self.remeasure(Block::Message(ix));
        cx.notify();
    }

    /// Scrolls the relation chain (box or panel) to this change.
    fn reveal_current_related(&self) {
        let at = self
            .related
            .as_ref()
            .and_then(|r| r.changes.iter().position(|c| c.number == self.number));
        if let Some(ix) = at {
            self.related_scroll.scroll_to_item(ix);
        }
    }

    /// Opens the change `delta` rows further down the relation chain (the
    /// list is newest descendant first), replacing the history entry. Does
    /// nothing at the ends, or when this change is not in the chain.
    fn step_related(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(r) = &self.related else {
            return;
        };
        let target = r
            .changes
            .iter()
            .position(|c| c.number == self.number)
            .and_then(|i| r.changes.get(i.checked_add_signed(delta)?))
            .map(|c| c.number);
        if let Some(n) = target {
            cx.emit(ChangeEvent::ReplaceChange(n));
        }
    }

    fn next_related(&mut self, _: &NextRelated, _: &mut Window, cx: &mut Context<Self>) {
        self.step_related(1, cx);
    }

    fn prev_related(&mut self, _: &PrevRelated, _: &mut Window, cx: &mut Context<Self>) {
        self.step_related(-1, cx);
    }

    fn toggle_related_panel_action(
        &mut self,
        _: &ToggleRelatedPanel,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_related_panel(cx);
    }

    fn toggle_related_panel(&mut self, cx: &mut Context<Self>) {
        self.layout.update(cx, |l, cx| l.toggle_related_panel(cx));
        self.reveal_current_related();
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

    /// `a`: asks the shell for the reply dialog.
    fn reply(&mut self, _: &Reply, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(why) = reply::unavailable(&self.session) {
            self.hint.say(why, false, cx, |p| &mut p.hint);
            return;
        }
        match &self.detail {
            Some(d) if d.current().is_some() => cx.emit(ChangeEvent::Reply(Box::new(d.clone()))),
            _ => self
                .hint
                .say("The change is not loaded yet", false, cx, |p| &mut p.hint),
        }
    }

    /// A review was posted from the dialog: take the change as the server
    /// has it now (votes, attention set, log) and say so.
    pub fn reply_sent(&mut self, sent: &reply::Sent, cx: &mut Context<Self>) {
        self.take_detail(sent.detail.clone(), cx);
        let (note, warning) = sent.note();
        if !warning && self.advance_target(&sent.voted, cx).is_some() {
            self.after_vote(&sent.voted, reply::plan::voted_text(&sent.voted), cx);
            return;
        }
        self.hint.say(note, warning, cx, |p| &mut p.hint);
    }

    /// Takes the change as the server has it after a write, or fetches it
    /// again if that failed.
    fn take_detail(&mut self, detail: Option<ChangeDetail>, cx: &mut Context<Self>) {
        match detail {
            Some(d) => {
                self.detail = Some(d);
                self.fetched_at = Some(Timestamp::now());
                self.apply_selection(true, cx);
            }
            None => {
                self.track.invalidate();
                self.tick(cx);
            }
        }
    }

    /// `m`: asks the shell for the change action menu.
    fn open_actions(&mut self, _: &ChangeMenu, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(why) = action_menu::unavailable(&self.session) {
            self.hint.say(why, false, cx, |p| &mut p.hint);
            return;
        }
        match &self.detail {
            Some(d) => cx.emit(ChangeEvent::Actions(Box::new(d.clone()))),
            None => self
                .hint
                .say("The change is not loaded yet", false, cx, |p| &mut p.hint),
        }
    }

    /// An action went through: take the change as the server has it now
    /// (a new current patch set after a rebase or a submit moves the file
    /// list along) and say so.
    pub fn action_done(&mut self, done: &action_menu::Done, cx: &mut Context<Self>) {
        match &done.detail {
            Some(d) => {
                self.detail = Some(d.clone());
                self.fetched_at = Some(Timestamp::now());
                self.apply_selection(true, cx);
            }
            None => {
                self.track.invalidate();
                self.tick(cx);
            }
        }
        self.hint.say(done.note(), false, cx, |p| &mut p.hint);
    }

    /// A note under the header, for the shell after it opened the page.
    pub fn say(&mut self, note: String, cx: &mut Context<Self>) {
        self.hint.say(note, false, cx, |p| &mut p.hint);
        cx.notify();
    }

    /// The server refused an action and the change was fetched again: show
    /// it as it is now, without a note (the dialog shows the error).
    pub fn detail_refreshed(&mut self, detail: &ChangeDetail, cx: &mut Context<Self>) {
        self.detail = Some(detail.clone());
        self.fetched_at = Some(Timestamp::now());
        self.apply_selection(true, cx);
        cx.notify();
    }

    /// `r`: marks the file under the cursor reviewed, or clears the mark.
    /// The mark changes on screen at once; if the request fails it goes
    /// back and a note says why.
    fn toggle_reviewed(&mut self, _: &ToggleReviewed, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(why) = reviewed::unavailable(&self.session) {
            self.hint.say(why, false, cx, |p| &mut p.hint);
            return;
        }
        let (Some(key), Some(path)) = (
            self.files.key.clone(),
            self.cursor_path().map(str::to_owned),
        ) else {
            return;
        };
        let reviewed = !self.files.reviewed.contains(&path);
        self.mark_reviewed(&path, reviewed);
        cx.notify();
        let syncer = self.session.syncer().clone();
        let (change, sha, sent) = (self.number, key.target.sha.clone(), path.clone());
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(reviewed::send(syncer, change, sha, sent, reviewed))
                .await;
            this.update(cx, |this, cx| {
                this.reviewed_result(&key, &path, reviewed, outcome, cx)
            })
            .ok();
        })
        .detach();
    }

    fn mark_reviewed(&mut self, path: &str, reviewed: bool) {
        if reviewed {
            self.files.reviewed.insert(path.to_owned());
        } else {
            self.files.reviewed.remove(path);
        }
    }

    fn reviewed_result(
        &mut self,
        key: &FilesKey,
        path: &str,
        reviewed: bool,
        outcome: reviewed::Outcome,
        cx: &mut Context<Self>,
    ) {
        let (error, login) = match outcome {
            reviewed::Outcome::Done => return,
            reviewed::Outcome::Unauthorized => (None, true),
            reviewed::Outcome::Failed(e) => (Some(e), false),
        };
        // Back to what the server has, unless another pair is shown now.
        if self.files.key.as_ref() == Some(key) && self.files.reviewed.contains(path) == reviewed {
            self.mark_reviewed(path, !reviewed);
        }
        if let Some(e) = error {
            let note = reviewed::failure_note(path, reviewed, &e);
            self.hint.say(note, true, cx, |p| &mut p.hint);
        }
        if login {
            cx.emit(ChangeEvent::Unauthorized);
        }
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
