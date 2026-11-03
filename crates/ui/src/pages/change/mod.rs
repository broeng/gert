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

#[cfg(test)]
mod local_tests;
mod pending;
mod quick_vote;
mod requirements;
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
    ChangeDetail, ChangeNumber, ChangeStatus, ChangeSummary, DiffKey, DiffPair, FileCommentCounts,
    FileEntry, FilesKey, LocalApproval, LocalChange, PatchSetNumber, PendingReviewId,
    RelatedChange, RevisionId, Thread, ThreadId,
};
use store::CachedFileList;
use sync::{ChainCommit, PollPolicy};

use crate::{
    action_menu,
    actions::*,
    change_detail::{self, GitCommits, Provided},
    checkouts::Checkouts,
    comments::{ChangeComments, CommentsEvent},
    diff::{
        DiffEvent, DiffSizing, DiffStatus, DiffView, ThreadContext,
        actions::{NextFile, PrevFile},
    },
    diff_load::{DiffSource, LoadError, load_diff},
    filters::{FileMatcher, FileStyle, next_unskipped, skips},
    hint::Hint,
    local_chain,
    local_source::{
        LocalLazy, LocalSource, LocalState, Lookup, Renames, Resolved, Unserved, local_file_list,
        lookup, renames_of,
    },
    nav::{ChangeView, DiffRoute},
    outbox::{self, DraftOutbox},
    pages::list::Status,
    reply, reviewed,
    row::browser_url,
    servers::ServerBadge,
    session::{ServerKey, Session, is_unauthorized},
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
    /// Like `OpenChange`, at a patch set (a row of a relation chain read
    /// from the checkout: a local version, or an older patch set).
    OpenChangeAt(ChangeNumber, ChangeView),
    /// Like `ReplaceChange`, at a patch set.
    ReplaceChangeAt(ChangeNumber, ChangeView),
    /// A vote went through and the next change of the chain is open: like
    /// `ReplaceChange`, and the new page shows the note.
    Advance(ChangeNumber, String),
    /// A file's diff page was chosen.
    OpenDiff(DiffRoute),
    /// A refresh succeeded: the server is reachable.
    Refreshed,
    /// `a`: open the reply dialog for this change (a local review when a
    /// local version is shown).
    Reply(Box<reply::ReplyRequest>),
    /// `m`: open the change action menu for this change.
    Actions(Box<ChangeDetail>),
    /// The Submit chain button: submits the change with its ancestors, without confirmation.
    SubmitChain(Box<ChangeDetail>),
    /// `f m` or the Manage filters button: the dialog for the file filter
    /// rules, with the change's project at hand.
    ManageFilters {
        project: String,
    },
    /// `p s` or the gear next to the project: the settings of the local
    /// checkout of this project.
    ProjectSettings(String),
    /// A project or person was clicked: show the search for this query.
    Search(String),
    /// A quick vote on a local version was saved as a local review.
    LocalReviewSaved,
}

/// The file list of the current patch set pair.
/// Why `r` waits for Gerrit's file list.
const REVIEWED_WAITS: &str = "Reviewed marks come with Gerrit's file list";
/// The files notice of a local version the checkout cannot list.
const LOCAL_FILES_UNAVAILABLE: &str =
    "the project's checkout does not have this version (the files of local versions come from it)";

#[derive(Default)]
struct Files {
    key: Option<FilesKey>,
    list: Vec<FileEntry>,
    reviewed: HashSet<String>,
    /// A list (possibly empty) has arrived for `key`.
    loaded: bool,
    /// The checkout the list was made in, while it is the local list (no
    /// reviewed marks, never stored) that Gerrit's has not replaced yet.
    local: Option<std::path::PathBuf>,
    error: Option<String>,
}

#[derive(Clone)]
struct Related {
    sha: RevisionId,
    /// Gerrit's related changes; empty for a chain read from the checkout.
    changes: Vec<RelatedChange>,
    /// What the chain shows, newest first.
    rows: Vec<ChainRow>,
    /// Read from the checkout (see [`crate::local_chain`]), not Gerrit's.
    from_git: bool,
    /// Labels of the open changes in the chain, once fetched (from the
    /// cache for a chain read from the checkout).
    summaries: HashMap<ChangeNumber, ChangeSummary>,
    /// Taken over from the page of another change in the chain, and shown
    /// until this change's own list arrives.
    adopted: bool,
}

/// The relation chain as a change page hands it to the page of another
/// change in that chain (Shift-J / Shift-K, a click on a row, Back), so
/// that the chain stays on screen, scrolled where it was, while the new
/// page reads its own.
pub struct ChainHandover {
    related: Related,
    scroll: ScrollHandle,
    drawn: Option<bool>,
}

/// One row of the relation chain: a change of Gerrit's related changes,
/// or a commit of a chain read from the checkout.
#[derive(Clone)]
struct ChainRow {
    /// `#123`; for a chain from the checkout also `local`, `#7 local 2`,
    /// `#7 PS 3` or a short commit id. Never a synthetic number.
    label: String,
    subject: String,
    /// The change on show (for a chain from the checkout: its commit).
    this: bool,
    /// Where a click goes; `None` for this row and for a commit that is no
    /// change.
    open: Option<ChainOpen>,
    /// What the status pills come from: Gerrit's entry, or for a Gerrit
    /// patch set in a chain from the checkout one made from its cached
    /// summary (never outdated: the pill is about Gerrit's chain).
    related: Option<RelatedChange>,
}

/// Where a click on a relation chain row goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChainOpen {
    /// Another change, at its current patch set.
    Change(ChangeNumber),
    /// Another change at this patch set.
    At(ChangeNumber, PatchSetNumber),
    /// This change's patch set.
    Here(PatchSetNumber),
}

/// The other change a chain row leads to, if any.
fn row_change(open: Option<ChainOpen>) -> Option<ChangeNumber> {
    match open? {
        ChainOpen::Change(n) | ChainOpen::At(n, _) => Some(n),
        ChainOpen::Here(_) => None,
    }
}

/// The row that stands for commits a chain read from the checkout leaves
/// out.
fn more_row() -> ChainRow {
    ChainRow {
        label: "\u{2026}".to_owned(),
        subject: "more commits not shown".to_owned(),
        this: false,
        open: None,
        related: None,
    }
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
    /// Where the diff on show came from.
    source: Option<DiffSource>,
    task: Option<Task<()>>,
    _sub: Option<Subscription>,
}

pub struct ChangePage {
    session: Session,
    settings: Entity<DiffSettings>,
    number: ChangeNumber,
    /// What [`change_detail`] provides: Gerrit's detail with the local
    /// patch sets, or a local change's synthesised one. Never stored.
    detail: Option<ChangeDetail>,
    /// The store row of a local change.
    local_row: Option<LocalChange>,
    /// The commits of the local patch sets, read from the checkout, put on
    /// every detail the page takes.
    git: GitCommits,
    git_task: Option<Task<()>>,
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
    /// How the chain was last drawn: as the panel (`true`) or the box. The
    /// row bounds in `related_scroll` are from that frame.
    related_drawn: Option<bool>,
    /// Bring the current change into view the next time the chain is drawn.
    reveal_related: bool,
    /// The file filter rules and which are on, shared by all pages.
    filters: Entity<FileFilters>,
    /// The syntax worker's cache, shared by all pages.
    syntax: Entity<SyntaxCache>,
    /// What this page last asked the syntax worker to do.
    queued: Vec<DiffKey>,
    /// The server's local checkouts, when it has any.
    checkouts: Option<Entity<Checkouts>>,
    /// The local source for the file list in use (see `update_local`).
    local: LocalState,
    local_task: Option<Task<()>>,
    /// Making the file list in the checkout after a cache miss.
    local_files_task: Option<Task<()>>,
    /// The renames of the file list, for the local source.
    renames: Arc<Renames>,
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
    /// The local commits the checkout built on the Gerrit patch set on
    /// show, for its relation chain, and the patch set's commit.
    local_stack: Option<(RevisionId, local_chain::Stack)>,
    stack_task: Option<Task<()>>,
    summaries_task: Option<Task<()>>,
    /// A quick vote is on its way.
    voting: Option<Task<()>>,
    /// The change's comment threads, shown in the inline diffs.
    comments: Entity<ChangeComments>,
    outbox: Entity<DraftOutbox>,
    /// The threads as last read from `comments`.
    threads: Arc<Vec<Thread>>,
    /// The pending reviews of the change's local versions, as last read
    /// from `comments` (see [`pending`]).
    reviews: Arc<Vec<model::PendingReview>>,
    /// The review the local reviews card has selected; `None` is the newest.
    review_cursor: Option<model::PendingReviewId>,
    /// The review a first "drop" asked about.
    confirm_drop: Option<model::PendingReviewId>,
    /// Ends the drop question after a while.
    drop_timer: Option<Task<()>>,
    /// A cancel or drop on its way to the store.
    reviews_task: Option<Task<()>>,
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
    /// The server's name and color, when there are several servers.
    server_badge: Option<ServerBadge>,
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
            checkouts: None,
            local: LocalState::Unknown,
            local_task: None,
            local_files_task: None,
            renames: Arc::default(),
            styles: Vec::new(),
            visible: Vec::new(),
            number,
            detail: None,
            local_row: None,
            git: GitCommits::default(),
            git_task: None,
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
            related_drawn: None,
            reveal_related: false,
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
            local_stack: None,
            stack_task: None,
            summaries_task: None,
            voting: None,
            comments: comments.clone(),
            outbox,
            threads: Arc::default(),
            reviews: Arc::default(),
            review_cursor: None,
            confirm_drop: None,
            drop_timer: None,
            reviews_task: None,
            thread_filter: None,
            listed: Vec::new(),
            thread_cursor: None,
            counts: BTreeMap::new(),
            ps_view: None,
            ps_sub: None,
            hint: Hint::default(),
            server_badge: None,
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
                .background_spawn(async move { change_detail::cached(&syncer, number) })
                .await;
            this.update(cx, |this, cx| {
                this.loaded = true;
                match cached {
                    Ok(Some(p)) => this.take_provided(p, false, cx),
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
    pub(crate) fn local_row_for_test(&self) -> Option<&LocalChange> {
        self.local_row.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn status_for_test(&self) -> &Status {
        &self.status
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
    pub(crate) fn chain_len_for_test(&self) -> usize {
        self.related.as_ref().map_or(0, |r| r.rows.len())
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

    /// The checkout the file list was made in, while it is the local one.
    #[cfg(test)]
    pub(crate) fn files_local_for_test(&self) -> Option<&std::path::Path> {
        self.files.local.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn files_error_for_test(&self) -> Option<&str> {
        self.files.error.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn files_reviewed_count_for_test(&self) -> usize {
        self.files.reviewed.len()
    }

    #[cfg(test)]
    pub(crate) fn inline_source(&self, path: &str) -> Option<DiffSource> {
        self.inline.get(path).and_then(|i| i.source)
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

    /// Shows a failure that happened elsewhere (the shell's background
    /// saves) on the hint line.
    pub(crate) fn say_error(&mut self, text: String, cx: &mut Context<Self>) {
        self.hint.say(text, true, cx, |p| &mut p.hint);
    }

    /// An edit was saved as a new draft because its draft is being sent
    /// with a local review: say so.
    pub fn kept_as_new(&mut self, cx: &mut Context<Self>) {
        self.hint
            .say(outbox::KEPT_AS_NEW, false, cx, |p| &mut p.hint);
    }

    /// Deleting a draft was refused because a local review post is sending
    /// it: say so (the card stays).
    pub fn delete_refused(&mut self, cx: &mut Context<Self>) {
        self.hint
            .say(outbox::DELETE_BEING_SENT, false, cx, |p| &mut p.hint);
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
        // A local change is read from the store again (new versions the
        // scanner found), never from Gerrit, which does not have it.
        let can_refresh = self.session.can_sync() || self.number.is_local();
        if !can_refresh || !self.loaded || self.status.busy() {
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

    /// Fetches the change detail, with the local patch sets the store has
    /// now; the file list and related changes follow in
    /// [`Self::apply_selection`]. A local change is read from the store
    /// again instead.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.number.is_local() {
            self.reload_local(cx);
            return;
        }
        let syncer = self.session.syncer().clone();
        let number = self.number;
        self.status = Status::Refreshing;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let detail = syncer.refresh_change(number).await?;
                    let locals = change_detail::local_revisions(&syncer, number);
                    Ok::<_, sync::Error>((detail, locals))
                })
                .await;
            this.update(cx, |this, cx| {
                let now = Timestamp::now();
                match result {
                    Ok((detail, locals)) => {
                        let detail = match locals {
                            Ok(locals) => change_detail::merge(detail, locals),
                            Err(e) => {
                                log::warn!("reading the local patch sets of {number}: {e}");
                                change_detail::carry_local(detail, this.detail.as_ref())
                            }
                        };
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
                        this.set_detail(detail, cx);
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

    /// Reads a local change from the store again: its row and versions
    /// (the scanner may have found a new one); git fills in the commits
    /// that are new. No network.
    fn reload_local(&mut self, cx: &mut Context<Self>) {
        let syncer = self.session.syncer().clone();
        let number = self.number;
        self.status = Status::Refreshing;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { change_detail::cached(&syncer, number) })
                .await;
            this.update(cx, |this, cx| {
                this.status = Status::Idle;
                this.track.success(Timestamp::now());
                match result {
                    Ok(Some(p)) => this.take_provided(p, false, cx),
                    // The row is gone (pushed and consolidated, or dropped):
                    // keep what is on screen.
                    Ok(None) => {}
                    Err(e) => log::warn!("reading local change: {e}"),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Takes what the provider read from the store and loads what depends
    /// on it (from the cache; also from the server if `network`).
    fn take_provided(&mut self, p: Provided, network: bool, cx: &mut Context<Self>) {
        self.fetched_at = p.fetched_at;
        self.local_row = p.local;
        self.set_detail(p.detail, cx);
        self.apply_selection(network, cx);
    }

    /// The one place the page's detail is replaced: the commits read from
    /// the checkout go onto its local patch sets, and those not read yet
    /// are looked up (those git failed to read for the last detail once
    /// more).
    fn set_detail(&mut self, mut detail: ChangeDetail, cx: &mut Context<Self>) {
        change_detail::apply_commits(&mut detail, &self.git);
        self.git.forget_failures();
        self.detail = Some(detail);
        self.load_git(cx);
    }

    /// Reads the commits of the local patch sets that are not known yet
    /// from the checkout, unless that is under way. Every read settles its
    /// commits (found, missing or failed), so asking again after one ends.
    fn load_git(&mut self, cx: &mut Context<Self>) {
        let Some(d) = &self.detail else {
            return;
        };
        if self.git_task.is_some() || self.git.wanted(d).is_empty() {
            return;
        }
        let (detail, known, checkouts) = (d.clone(), self.git.clone(), self.checkouts.clone());
        self.git_task = Some(cx.spawn(async move |this, cx| {
            let read = change_detail::load_commits(&detail, &known, checkouts, cx).await;
            this.update(cx, |this, cx| {
                this.git_task = None;
                let Some(read) = read else {
                    return;
                };
                this.git.extend(read);
                if let Some(mut d) = this.detail.take() {
                    change_detail::apply_commits(&mut d, &this.git);
                    this.detail = Some(d);
                }
                // The header (commit message, author) grew.
                this.remeasure(Block::Header);
                cx.notify();
                this.load_git(cx);
            })
            .ok();
        }));
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
            self.local = LocalState::Unknown;
            self.local_task = None;
            self.local_files_task = None;
            self.styles.clear();
            self.visible.clear();
            self.cursor = 0;
            self.jump = true;
            // Stop the syntax worker on the old comparison now.
            self.queue_syntax(cx);
        }
        // An adopted chain stays until this change's own list replaces it.
        if self
            .related
            .as_ref()
            .is_some_and(|r| !r.adopted && Some(&r.sha) != sha.as_ref())
        {
            self.related = None;
        }
        if self.local_stack.as_ref().map(|s| &s.0) != sha.as_ref() {
            self.local_stack = None;
            self.stack_task = None;
        }
        self.read_threads(cx);
        // Gerrit knows no relation chain of a local change or version: the
        // checkout does.
        let target_local = key.as_ref().is_some_and(|k| k.target.number.is_local());
        if let Some(key) = key {
            self.load_files(key, network, cx);
        }
        if let Some(sha) = sha {
            if self.number.is_local() || target_local {
                self.load_git_chain(sha, cx);
            } else {
                self.load_related(sha.clone(), network, cx);
                self.load_stack(sha, cx);
            }
        }
    }

    fn load_files(&mut self, key: FilesKey, network: bool, cx: &mut Context<Self>) {
        let syncer = self.session.syncer().clone();
        // Gerrit cannot list a local version: the checkout does.
        let sync = network && self.session.can_sync() && !key.is_local();
        self.files_task = Some(cx.spawn(async move |this, cx| {
            let cached = {
                let (syncer, key) = (syncer.clone(), key.clone());
                cx.background_spawn(async move { syncer.cached_files(&key) })
                    .await
            };
            this.update(cx, |this, cx| match cached {
                Ok(Some(list)) => this.apply_files(&key, list, cx),
                // Nothing stored: the checkout, if it has the commits,
                // gives a list until Gerrit's arrives.
                Ok(None) => this.start_local_files(&key, cx),
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
        let reviewed = list.reviewed.unwrap_or_default().into_iter().collect();
        self.apply_list(key, list.files, reviewed, None, cx);
    }

    /// Makes the file list in the checkout when there is no stored one and
    /// none is on screen. Never stored; Gerrit's list replaces it. The
    /// shared local resolve is the one the diffs use.
    fn start_local_files(&mut self, key: &FilesKey, cx: &mut Context<Self>) {
        if self.files.key.as_ref() != Some(key) || self.files.loaded {
            return;
        }
        self.update_local(cx);
        if matches!(&self.local, LocalState::Ready(_, Err(_))) {
            // No checkout, or it cannot serve this comparison.
            self.no_local_list(key, cx);
            return;
        }
        let lazy = self.local.lazy(key, &Arc::default());
        let key = key.clone();
        self.local_files_task = Some(cx.spawn(async move |this, cx| {
            let made = local_file_list(lazy, cx).await;
            this.update(cx, |this, cx| match made {
                // Gerrit's list (or a stored one) may have come first, or
                // another pair been picked.
                Some((path, files)) => {
                    if this.files.key.as_ref() == Some(&key) && !this.files.loaded {
                        this.apply_list(&key, files, HashSet::new(), Some(path), cx);
                    }
                }
                None => this.no_local_list(&key, cx),
            })
            .ok();
        }));
    }

    /// The checkout cannot list `key`. Gerrit's list comes instead, except
    /// for a local version, which Gerrit does not have: then the notice
    /// says so (never one of Gerrit's errors).
    fn no_local_list(&mut self, key: &FilesKey, cx: &mut Context<Self>) {
        if self.parents_differ() {
            // The notice says so, in place of an error.
            self.rebuild_blocks();
            cx.notify();
            return;
        }
        if key.is_local() && self.files.key.as_ref() == Some(key) && !self.files.loaded {
            self.files.error = Some(LOCAL_FILES_UNAVAILABLE.to_owned());
            self.rebuild_blocks();
            cx.notify();
        }
    }

    /// Shows `files`. `local` is the checkout they were made in, `None` for
    /// the stored list or Gerrit's.
    fn apply_list(
        &mut self,
        key: &FilesKey,
        files: Vec<FileEntry>,
        reviewed: HashSet<String>,
        local: Option<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if self.files.key.as_ref() != Some(key) {
            return; // the user picked another pair meanwhile
        }
        if local.is_none() {
            // The local list is not needed any more, and must not land late.
            self.local_files_task = None;
        }
        let keep = self
            .want_file
            .take()
            .or_else(|| self.cursor_path().map(str::to_owned));
        self.files.list = files;
        self.files.reviewed = reviewed;
        self.files.loaded = true;
        self.files.local = local;
        self.files.error = None;
        self.renames = renames_of(&self.files.list);
        self.update_local(cx);
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

    /// Names the server's checkouts, so diffs can come from the project's
    /// local checkout. Set right after the page is made, before anything
    /// has loaded.
    pub fn set_checkouts(&mut self, checkouts: Option<Entity<Checkouts>>) {
        self.checkouts = checkouts;
    }

    /// Starts finding the local source for the file list's comparison
    /// unless that is done or under way. Nothing waits for it: stored
    /// diffs are read first, and a diff that has to be computed awaits it
    /// in `load_diff`.
    fn update_local(&mut self, cx: &mut Context<Self>) {
        let (Some(files), Some(project)) = (
            self.files.key.clone(),
            self.detail.as_ref().map(|d| d.summary.project.clone()),
        ) else {
            return;
        };
        if self.local.is_for(&files) {
            return;
        }
        self.local_task = None;
        self.local = match lookup(&self.checkouts, &project, &files, cx) {
            Lookup::Unconfigured => LocalState::Ready(files, Err(Unserved::NoCheckout)),
            Lookup::Ready(s) => LocalState::Ready(files, Ok(s)),
            Lookup::Pending(p) => {
                let (key, shared) = (files.clone(), p.clone());
                self.local_task = Some(cx.spawn(async move |this, cx| {
                    let source = shared.await;
                    this.update(cx, |this, cx| this.local_resolved(&key, source, cx))
                        .ok();
                }));
                LocalState::Resolving(files, p)
            }
        };
    }

    fn local_resolved(&mut self, files: &FilesKey, source: Resolved, cx: &mut Context<Self>) {
        if !matches!(&self.local, LocalState::Resolving(k, _) if k == files) {
            return;
        }
        self.local = LocalState::Ready(files.clone(), source);
        self.local_task = None;
        if self.parents_differ() {
            // Not the checkout's fault, whatever the list's request said.
            self.files.error = None;
            self.remeasure(Block::FilesNotice);
        }
        cx.notify(); // the "local" marker
    }

    /// Whether the file list in use is a comparison with a local version
    /// whose two sides have different parents (a rebase), which nobody can
    /// show: the checkout leaves it to Gerrit, which marks the edits a
    /// rebase brings, and Gerrit does not have the local version.
    pub(super) fn parents_differ(&self) -> bool {
        self.files.key.as_ref().is_some_and(|k| {
            k.is_local()
                && !self.files.loaded
                && self.local.unserved(k) == Some(Unserved::ParentsDiffer)
        })
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
        let rows = self.gerrit_chain_rows(sha, &changes);
        // Rows that come and go move the current one; a refresh of the
        // same rows leaves the chain where the user scrolled it.
        let moved = self.related.as_ref().is_none_or(|r| {
            !r.rows
                .iter()
                .map(|row| (&row.label, row.this))
                .eq(rows.iter().map(|row| (&row.label, row.this)))
        });
        let summaries = match self.related.take() {
            Some(r) if &r.sha == sha && !r.adopted && !r.from_git => r.summaries,
            // The previous page's votes stay for the changes still at the
            // same patch set, until this chain's own arrive.
            Some(r) if r.adopted => {
                let newest = |list: &[RelatedChange]| -> HashMap<_, _> {
                    list.iter()
                        .map(|c| (c.number, c.current_patch_set.unwrap_or(c.patch_set)))
                        .collect()
                };
                let (old, new) = (newest(&r.changes), newest(&changes));
                let mut kept = r.summaries;
                kept.retain(|n, _| old.get(n).is_some_and(|ps| new.get(n) == Some(ps)));
                kept
            }
            _ => HashMap::new(),
        };
        let open: Vec<_> = changes
            .iter()
            .filter(|c| matches!(c.status, Some(ChangeStatus::New)))
            .map(|c| c.number)
            .collect();
        self.related = Some(Related {
            sha: sha.clone(),
            changes,
            rows,
            from_git: false,
            summaries,
            adopted: false,
        });
        if moved {
            self.reveal_current_related();
        }
        self.rebuild_blocks();
        cx.notify();
        if self.session.can_sync() {
            self.load_related_summaries(sha.clone(), open, cx);
        }
    }

    /// The rows of Gerrit's relation chain `changes` of patch set `sha`,
    /// with the local commits the checkout built on it (once read, see
    /// [`ChangePage::load_stack`]) above it, or above the highest change
    /// of the chain that they are built on. Without a chain from Gerrit
    /// but with local commits, the chain is them and this change.
    ///
    /// A list cannot show a tree: when what was built on the patch set
    /// forks (one branch with a pushed change, another with a local one),
    /// the local commits are one block, in git's topological order (each
    /// line together, newest first), above the highest change they are
    /// built on.
    fn gerrit_chain_rows(&self, sha: &RevisionId, changes: &[RelatedChange]) -> Vec<ChainRow> {
        let mut rows: Vec<ChainRow> = changes
            .iter()
            .map(|c| ChainRow {
                label: change_detail::change_label(c.number),
                subject: view::related_subject(c),
                this: c.number == self.number,
                open: (c.number != self.number).then_some(ChainOpen::Change(c.number)),
                related: Some(c.clone()),
            })
            .collect();
        let Some((_, stack)) = self.local_stack.as_ref().filter(|(at, _)| at == sha) else {
            return rows;
        };
        // A commit Gerrit's chain has is Gerrit's row, though the store did
        // not know it yet (just pushed, its change not cached): not shown
        // twice, and the local commits built on it go above that row.
        let in_chain: HashMap<&RevisionId, ChangeNumber> = changes
            .iter()
            .filter_map(|c| Some((c.commit.as_ref()?.sha.as_ref()?, c.number)))
            .collect();
        let mut below = stack.on_gerrit.clone();
        let entries: Vec<&local_chain::ChainEntry> = stack
            .entries
            .iter()
            .filter(|e| match in_chain.get(&e.commit.sha) {
                Some(n) => {
                    below.push(*n);
                    false
                }
                None => true,
            })
            .collect();
        if entries.is_empty() {
            return rows;
        }
        if rows.is_empty() {
            rows.push(ChainRow {
                label: change_detail::change_label(self.number),
                subject: self
                    .detail
                    .as_ref()
                    .map(|d| d.summary.subject.clone())
                    .unwrap_or_default(),
                this: true,
                open: None,
                related: None,
            });
        }
        let at = rows
            .iter()
            .position(|r| {
                r.this
                    || r.related
                        .as_ref()
                        .is_some_and(|c| below.contains(&c.number))
            })
            .unwrap_or(0);
        let mut ignored = HashMap::new();
        let local: Vec<ChainRow> = stack
            .more
            .then(more_row)
            .into_iter()
            .chain(
                entries
                    .into_iter()
                    .map(|e| self.git_chain_row(sha, e, &mut ignored)),
            )
            .collect();
        rows.splice(at..at, local);
        rows
    }

    /// Reads the local commits the checkout built on Gerrit patch set
    /// `sha` (see [`local_chain::read_stack`]) and puts them in its
    /// relation chain; until they arrive, the ones read before stay. No
    /// network; nothing when the project does not have the
    /// `local_changes` option or no open checkout.
    fn load_stack(&mut self, sha: RevisionId, cx: &mut Context<Self>) {
        let Some(project) = self.detail.as_ref().map(|d| d.summary.project.clone()) else {
            return;
        };
        let (syncer, checkouts) = (self.session.syncer().clone(), self.checkouts.clone());
        self.stack_task = Some(cx.spawn(async move |this, cx| {
            let read = local_chain::read_stack(checkouts, project, sha.clone(), syncer, cx).await;
            if let Some(stack) = read {
                this.update(cx, |this, cx| this.apply_stack(sha, stack, cx))
                    .ok();
            }
        }));
    }

    fn apply_stack(&mut self, sha: RevisionId, stack: local_chain::Stack, cx: &mut Context<Self>) {
        let current = self
            .detail
            .as_ref()
            .and_then(|d| self.sel.target_ref(d))
            .map(|t| t.sha);
        if current.as_ref() != Some(&sha) {
            return;
        }
        // Gerrit's chain on show, if any, is drawn again without the local
        // commits that went.
        let gerrit = self
            .related
            .as_ref()
            .is_some_and(|r| r.sha == sha && !r.from_git);
        let empty = stack.entries.is_empty();
        self.local_stack = Some((sha.clone(), stack));
        // A chain handed over by the previous page stays until Gerrit's own
        // list arrives, which takes the stack in.
        let adopted = self.related.as_ref().is_some_and(|r| r.adopted);
        if !gerrit && (empty || adopted) {
            return;
        }
        let old = self.related.take();
        let old_rows: Vec<(String, bool)> = old
            .as_ref()
            .map(|r| {
                r.rows
                    .iter()
                    .map(|row| (row.label.clone(), row.this))
                    .collect()
            })
            .unwrap_or_default();
        let (changes, summaries) = match old {
            Some(r) if r.sha == sha && !r.from_git => (r.changes, r.summaries),
            _ => (Vec::new(), HashMap::new()),
        };
        let rows = self.gerrit_chain_rows(&sha, &changes);
        // The same rows again stay where the user scrolled them.
        let moved = !old_rows
            .iter()
            .map(|(label, this)| (label, *this))
            .eq(rows.iter().map(|row| (&row.label, row.this)));
        self.related = Some(Related {
            sha,
            changes,
            rows,
            from_git: false,
            summaries,
            adopted: false,
        });
        if moved {
            self.reveal_current_related();
        }
        self.rebuild_blocks();
        cx.notify();
    }

    /// Reads the relation chain of the commit `sha` from the checkout and
    /// names its commits from the store (see [`crate::local_chain`]). No
    /// network.
    fn load_git_chain(&mut self, sha: RevisionId, cx: &mut Context<Self>) {
        let Some(project) = self.detail.as_ref().map(|d| d.summary.project.clone()) else {
            return;
        };
        let (syncer, checkouts) = (self.session.syncer().clone(), self.checkouts.clone());
        self.related_task = Some(cx.spawn(async move |this, cx| {
            let read = local_chain::read(checkouts, project, sha.clone(), syncer, cx).await;
            if let Some(chain) = read {
                this.update(cx, |this, cx| this.apply_git_chain(&sha, chain, cx))
                    .ok();
            }
        }));
    }

    fn apply_git_chain(
        &mut self,
        sha: &RevisionId,
        chain: local_chain::Chain,
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
        let mut summaries = HashMap::new();
        let rows: Vec<ChainRow> = chain
            .entries
            .iter()
            .map(|e| self.git_chain_row(sha, e, &mut summaries))
            .collect();
        let rows: Vec<ChainRow> = chain
            .more_above
            .then(more_row)
            .into_iter()
            .chain(rows)
            .chain(chain.more_below.then(more_row))
            .collect();
        // As for Gerrit's chain: the same rows again stay where the user
        // scrolled them.
        let moved = self.related.as_ref().is_none_or(|r| {
            !r.rows
                .iter()
                .map(|row| (&row.label, row.this))
                .eq(rows.iter().map(|row| (&row.label, row.this)))
        });
        self.related = Some(Related {
            sha: sha.clone(),
            changes: Vec::new(),
            rows,
            from_git: true,
            summaries,
            adopted: false,
        });
        if moved {
            self.reveal_current_related();
        }
        self.rebuild_blocks();
        cx.notify();
    }

    /// The row of a commit of a chain read from the checkout, `sha` being
    /// the commit on show: where a click goes (a version of this change is
    /// picked on this page), and for a Gerrit patch set the status pills
    /// from its cached summary, which also goes into `summaries`.
    fn git_chain_row(
        &self,
        sha: &RevisionId,
        e: &local_chain::ChainEntry,
        summaries: &mut HashMap<ChangeNumber, ChangeSummary>,
    ) -> ChainRow {
        let this = e.commit.sha == *sha;
        let (open, related) = match &e.kind {
            // An older version (something may be built on it) at that
            // version; the newest is what the page shows anyway.
            ChainCommit::Local {
                change,
                patch_set,
                newest,
                ..
            } => {
                let open = if *newest {
                    ChainOpen::Change(*change)
                } else {
                    ChainOpen::At(*change, *patch_set)
                };
                (Some(open), None)
            }
            ChainCommit::LocalVersion {
                change, patch_set, ..
            } => (Some(ChainOpen::At(*change, *patch_set)), None),
            ChainCommit::Gerrit {
                change,
                patch_set,
                current,
                summary,
            } => {
                let open = if *current == Some(*patch_set) {
                    ChainOpen::Change(*change)
                } else {
                    ChainOpen::At(*change, *patch_set)
                };
                let related = summary.as_deref().map(|s| {
                    summaries.insert(*change, s.clone());
                    RelatedChange {
                        project: s.project.clone(),
                        change_id: s.change_id.clone(),
                        number: *change,
                        patch_set: *patch_set,
                        current_patch_set: None,
                        status: Some(s.status.clone()),
                        submittable: s.submittable,
                        work_in_progress: s.work_in_progress,
                        commit: None,
                    }
                });
                (Some(open), related)
            }
            ChainCommit::Unknown => (None, None),
        };
        // A version of the change on show is picked on this page.
        let open = open
            .map(|o| match o {
                ChainOpen::Change(n) | ChainOpen::At(n, _) if n == self.number => match e.kind {
                    ChainCommit::Local { patch_set, .. }
                    | ChainCommit::LocalVersion { patch_set, .. }
                    | ChainCommit::Gerrit { patch_set, .. } => ChainOpen::Here(patch_set),
                    ChainCommit::Unknown => o,
                },
                o => o,
            })
            .filter(|_| !this);
        ChainRow {
            label: local_chain::label(e),
            subject: e.commit.subject(),
            this,
            open,
            related,
        }
    }

    /// Goes where relation chain row `ix` leads: another change (a new
    /// history entry, or the current one replaced when `replace`), or
    /// another patch set of this one.
    pub(super) fn open_chain_row(&mut self, ix: usize, replace: bool, cx: &mut Context<Self>) {
        let Some(open) = self
            .related
            .as_ref()
            .and_then(|r| r.rows.get(ix))
            .and_then(|r| r.open)
        else {
            return;
        };
        let at = |target| ChangeView {
            target: Some(target),
            ..ChangeView::default()
        };
        match (open, replace) {
            (ChainOpen::Change(n), false) => cx.emit(ChangeEvent::OpenChange(n)),
            (ChainOpen::Change(n), true) => cx.emit(ChangeEvent::ReplaceChange(n)),
            (ChainOpen::At(n, ps), false) => cx.emit(ChangeEvent::OpenChangeAt(n, at(ps))),
            (ChainOpen::At(n, ps), true) => cx.emit(ChangeEvent::ReplaceChangeAt(n, at(ps))),
            (ChainOpen::Here(ps), _) => self.change_patch_sets(|s, d| s.set_target(d, ps), cx),
        }
    }

    /// The relation chain as drawn: each row's label, subject and whether
    /// it is the change on show.
    #[cfg(test)]
    pub(crate) fn chain_for_test(&self) -> Vec<(String, String, bool)> {
        self.related.as_ref().map_or_else(Vec::new, |r| {
            r.rows
                .iter()
                .map(|row| (row.label.clone(), row.subject.clone(), row.this))
                .collect()
        })
    }

    /// Fetches the votes of the open changes in the chain for their markers.
    fn load_related_summaries(
        &mut self,
        sha: RevisionId,
        open: Vec<ChangeNumber>,
        cx: &mut Context<Self>,
    ) {
        if open.is_empty() {
            return;
        }
        let syncer = self.session.syncer().clone();
        self.summaries_task = Some(cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { syncer.fetch_summaries(&open).await })
                .await;
            this.update(cx, |this, cx| match fetched {
                Ok(list) => {
                    if let Some(r) = this.related.as_mut().filter(|r| r.sha == sha) {
                        r.summaries = list.into_iter().map(|s| (s.number, s)).collect();
                        this.rebuild_blocks();
                        cx.notify();
                    }
                }
                Err(e) => log::warn!("fetching votes of related changes: {e}"),
            })
            .ok();
        }));
    }

    // ---- file filters ----

    /// Styles every file with the rules that are on for this change's
    /// project, and lists the ones that stay visible.
    fn compute_styles(&mut self, cx: &App) {
        let matcher = match &self.detail {
            Some(d) => self
                .filters
                .read(cx)
                .matcher(&self.session.key, &d.summary.project),
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
            .applicable(&self.session.key, &d.summary.project)
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

    fn project_checkout(&mut self, _: &ProjectCheckout, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(d) = &self.detail {
            cx.emit(ChangeEvent::ProjectSettings(d.summary.project.clone()));
        }
    }

    // ---- blocks ----

    fn content(&self) -> Content {
        let d = self.detail.as_ref();
        Content {
            loaded: d.is_some(),
            reviews: self.listed_reviews().len(),
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
        let local = self.local_lazy();
        self.syntax
            .update(cx, |c, cx| c.set_queue(keys, &session, local, enabled, cx));
    }

    /// The checkout the local diffs come from.
    fn local_path(&self) -> Option<std::path::PathBuf> {
        self.local_source().map(|s| s.path().to_owned())
    }

    /// The local source for the file list in use, once resolved.
    fn local_source(&self) -> Option<LocalSource> {
        let source = self.local.source_for(self.files.key.as_ref()?)?;
        Some(source.with_renames(self.renames.clone()))
    }

    /// The local source for `load_diff` and the syntax worker, resolved or
    /// not yet.
    fn local_lazy(&self) -> LocalLazy {
        match &self.files.key {
            Some(files) => self.local.lazy(files, &self.renames),
            None => LocalLazy::none(),
        }
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
                source: None,
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
        let local = self.local_lazy();
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
                .background_spawn(load_diff(syncer, task_key, can_sync, local))
                .await;
            this.update(cx, |this, cx| this.diff_arrived(&path, &key, result, cx))
                .ok();
        }));
    }

    fn diff_arrived(
        &mut self,
        path: &str,
        key: &DiffKey,
        result: Result<(model::Diff, DiffSource), LoadError>,
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
            Ok((diff, source)) => {
                inline.error = None;
                inline.source = Some(source);
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
                            source: None,
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

    /// Scrolls the relation chain to this change the next time it is drawn.
    fn reveal_current_related(&mut self) {
        self.reveal_related = true;
    }

    /// Called as the chain is drawn: brings this change into view if
    /// asked to, centred in the box and scrolled just far enough in the
    /// panel. The row bounds are from the last frame that drew the chain,
    /// so when that frame drew it the other way or with other rows, this
    /// waits for the next frame.
    fn settle_related_scroll(&mut self, panel: bool, window: &mut Window, cx: &mut Context<Self>) {
        let drawn = self.related_drawn.replace(panel);
        if !self.reveal_related {
            return;
        }
        let Some(r) = &self.related else {
            return;
        };
        let Some(ix) = r.rows.iter().position(|row| row.this) else {
            self.reveal_related = false;
            return;
        };
        let scroll = &self.related_scroll;
        let item = scroll
            .bounds_for_item(ix)
            .filter(|_| drawn == Some(panel) && scroll.children_count() == r.rows.len());
        match item {
            Some(item) => {
                let offset = scroll.offset();
                let y = view::reveal_offset(
                    !panel,
                    offset.y,
                    scroll.bounds(),
                    item,
                    scroll.max_offset().y,
                );
                scroll.set_offset(point(offset.x, y));
                self.reveal_related = false;
            }
            None => cx.on_next_frame(window, |_, _, cx| cx.notify()),
        }
    }

    /// The chain to hand to the page of `number` on `server`, if that
    /// change is in it.
    pub fn chain_for(&self, server: &ServerKey, number: ChangeNumber) -> Option<ChainHandover> {
        let r = self.related.as_ref()?;
        let has = r
            .rows
            .iter()
            .any(|row| row_change(row.open) == Some(number));
        (self.session.key == *server && has).then(|| {
            // Where this page's row leads from the next page: this change
            // at the patch set shown, unless that is the current one.
            let shown = self.detail.as_ref().and_then(|d| {
                let target = self.sel.target_number(d)?;
                (Some(target) != d.current_patch_set).then_some(target)
            });
            let from = match shown {
                Some(ps) => ChainOpen::At(self.number, ps),
                None => ChainOpen::Change(self.number),
            };
            let mut related = r.clone();
            for row in &mut related.rows {
                row.open = match row.open {
                    _ if row.this => Some(from),
                    Some(ChainOpen::Here(ps)) => Some(ChainOpen::At(self.number, ps)),
                    open => open,
                };
                row.this = false;
            }
            ChainHandover {
                related,
                scroll: self.related_scroll.clone(),
                drawn: self.related_drawn,
            }
        })
    }

    /// Shows the chain the previous page handed over until this change's
    /// own list arrives, scrolled as it was. The row of this change (at
    /// the patch set asked for, if the chain has it) becomes this page's
    /// own.
    pub fn adopt_chain(&mut self, chain: ChainHandover, cx: &mut Context<Self>) {
        let mut related = chain.related;
        let mine = |open: Option<ChainOpen>| row_change(open) == Some(self.number);
        let at = related
            .rows
            .iter()
            .position(|row| match (row.open, self.sel.target) {
                (Some(ChainOpen::At(_, ps)), Some(want)) => mine(row.open) && ps == want,
                (Some(ChainOpen::Change(_)), None) => mine(row.open),
                _ => false,
            })
            .or_else(|| related.rows.iter().position(|row| mine(row.open)));
        for (ix, row) in related.rows.iter_mut().enumerate() {
            if Some(ix) == at {
                row.this = true;
                row.open = None;
            } else if mine(row.open) {
                // Another version of this change: picked on this page once
                // its own chain is read.
                row.open = None;
            }
        }
        self.related = Some(Related {
            adopted: true,
            ..related
        });
        self.related_scroll = chain.scroll;
        self.related_drawn = chain.drawn;
        self.reveal_current_related();
        cx.notify();
    }

    /// Opens the nearest change in the direction of `delta` (1: down the
    /// relation chain, which is newest descendant first; -1: up), passing
    /// rows that open nothing (a plain commit, a "more" row), and replaces
    /// the history entry. Does nothing at the ends, or when this change is
    /// not in the chain.
    fn step_related(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(r) = &self.related else {
            return;
        };
        let Some(from) = r.rows.iter().position(|row| row.this) else {
            return;
        };
        let mut ix = from;
        let target = loop {
            match ix.checked_add_signed(delta).filter(|&i| i < r.rows.len()) {
                Some(next) if r.rows[next].open.is_some() => break Some(next),
                Some(next) => ix = next,
                None => break None,
            }
        };
        if let Some(ix) = target {
            self.open_chain_row(ix, true, cx);
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

    /// `a`: asks the shell for the reply dialog: a reply to Gerrit, or a
    /// local review of the local version shown (which edits its pending
    /// review if it has one).
    fn reply(&mut self, _: &Reply, _: &mut Window, cx: &mut Context<Self>) {
        self.open_reply(None, cx);
    }

    /// Asks the shell for the reply dialog, editing pending review `edit`
    /// if given.
    pub(super) fn open_reply(&mut self, edit: Option<PendingReviewId>, cx: &mut Context<Self>) {
        if let Some(why) = reply::unavailable(&self.session) {
            self.hint.say(why, false, cx, |p| &mut p.hint);
            return;
        }
        match &self.detail {
            Some(d) if d.current().is_some() => {
                let request = reply::ReplyRequest {
                    detail: d.clone(),
                    target: self.sel.target_number(d),
                    edit,
                    strategy: self.local_approval(cx),
                };
                cx.emit(ChangeEvent::Reply(Box::new(request)))
            }
            _ => self
                .hint
                .say("The change is not loaded yet", false, cx, |p| &mut p.hint),
        }
    }

    /// The project's `local_approval` mode (the strictest without a
    /// checkout entry).
    pub(super) fn local_approval(&self, cx: &App) -> LocalApproval {
        let project = self.detail.as_ref().map(|d| d.summary.project.as_str());
        match (&self.checkouts, project) {
            (Some(c), Some(p)) => c.read(cx).local_approval(p),
            _ => LocalApproval::Exact,
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
                let d = change_detail::carry_local(d, self.detail.as_ref());
                self.set_detail(d, cx);
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
        if self.number.is_local() {
            // Every action of the menu is a request to Gerrit.
            self.hint
                .say(action_menu::LOCAL, false, cx, |p| &mut p.hint);
            return;
        }
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

    /// Whether a reply or vote would be on a local version: a local change,
    /// or a local patch set picked as the target. Neither may go to Gerrit,
    /// which would take it for the current patch set: they become pending
    /// reviews of the version.
    pub(super) fn on_local_target(&self) -> bool {
        self.number.is_local()
            || self
                .detail
                .as_ref()
                .and_then(|d| self.sel.target_number(d))
                .is_some_and(PatchSetNumber::is_local)
    }

    /// The Submit chain button.
    pub(super) fn submit_chain(&mut self, cx: &mut Context<Self>) {
        if let Some(why) = action_menu::unavailable(&self.session) {
            self.hint.say(why, false, cx, |p| &mut p.hint);
            return;
        }
        if let Some(d) = &self.detail {
            cx.emit(ChangeEvent::SubmitChain(Box::new(d.clone())));
        }
    }

    /// An action went through: take the change as the server has it now
    /// (a new current patch set after a rebase or a submit moves the file
    /// list along) and say so.
    pub fn action_done(&mut self, done: &action_menu::Done, cx: &mut Context<Self>) {
        match &done.detail {
            Some(d) => {
                let d = change_detail::carry_local(d.clone(), self.detail.as_ref());
                self.set_detail(d, cx);
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

    /// Names the page's server in its header (several servers only).
    pub fn set_server_badge(&mut self, badge: Option<ServerBadge>, cx: &mut Context<Self>) {
        if self.server_badge != badge {
            self.server_badge = badge;
            cx.notify();
        }
    }

    #[cfg(test)]
    pub(crate) fn server_badge_for_test(&self) -> Option<&ServerBadge> {
        self.server_badge.as_ref()
    }

    /// A note under the header, for the shell after it opened the page.
    pub fn say(&mut self, note: String, cx: &mut Context<Self>) {
        self.hint.say(note, false, cx, |p| &mut p.hint);
        cx.notify();
    }

    /// The server refused an action and the change was fetched again: show
    /// it as it is now, without a note (the dialog shows the error).
    pub fn detail_refreshed(&mut self, detail: &ChangeDetail, cx: &mut Context<Self>) {
        let d = change_detail::carry_local(detail.clone(), self.detail.as_ref());
        self.set_detail(d, cx);
        self.fetched_at = Some(Timestamp::now());
        self.apply_selection(true, cx);
        cx.notify();
    }

    /// `r`: marks the file under the cursor reviewed, or clears the mark.
    /// The mark changes on screen at once; if the request fails it goes
    /// back and a note says why.
    fn toggle_reviewed(&mut self, _: &ToggleReviewed, _: &mut Window, cx: &mut Context<Self>) {
        if self.number.is_local() || self.files.key.as_ref().is_some_and(FilesKey::is_local) {
            self.hint.say(reviewed::LOCAL, false, cx, |p| &mut p.hint);
            return;
        }
        if let Some(why) = reviewed::unavailable(&self.session) {
            self.hint.say(why, false, cx, |p| &mut p.hint);
            return;
        }
        if !self.files.loaded || self.files.local.is_some() {
            // Gerrit's list, computed before the mark, would replace it.
            self.hint.say(REVIEWED_WAITS, false, cx, |p| &mut p.hint);
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
        self.reload(cx);
    }

    /// Reads the change again now (from Gerrit, or a local change from the
    /// store): its data moved elsewhere, e.g. a consolidation run sent a
    /// local review.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.track.invalidate();
        self.tick(cx);
    }

    fn open_in_browser(&mut self, _: &OpenInBrowser, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(url) = self
            .detail
            .as_ref()
            .and_then(|d| browser_url(&self.session.base_url, &d.summary.project, self.number))
        {
            cx.open_url(&url);
        }
    }
}
