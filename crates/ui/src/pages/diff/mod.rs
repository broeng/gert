//! The diff page: one file's diff of a change, full size, with its own
//! entry in the history (like PolyGerrit's per-file URL).
//!
//! Loading: the change detail, the file list (for the header, `[`/`]` and
//! the prefetch) and the diff come from the cache first. A diff is keyed by
//! revision SHA and immutable, so a cached one is shown without any request;
//! only a miss is fetched, on the background executor, with a "loading"
//! placeholder until it arrives. Once a diff is shown, the syntax worker
//! fetches and highlights the other files in the order `]` visits them
//! (skipping hidden, dimmed and magic files). Only where the worker does not
//! cover the file `]` goes to (highlighting is off, or it is a magic file)
//! is that file's diff prefetched instead.
//!
//! Diffs are always requested as whole files (the server ignores `context`).
//! Folding unchanged lines and opening folds happen in the view, so a
//! changed context preference never refetches; only whitespace does. The
//! new diff goes in through `DiffView::set_diff`, which keeps the cursor on
//! its line and the folds the user opened.
//!
//! Focus: the diff view owns the keyboard (`j`/`k`, `n`/`p`, ...); the page
//! forwards its focus to it as soon as it exists. `,` opens the preferences
//! panel, which takes the focus until it is closed.

mod prefs;
#[cfg(test)]
mod tests;

use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use model::{ChangeDetail, DiffKey, DiffViewMode, FileEntry, FilesKey, ResolvedDiffPreferences};
use sync::Syncer;

use crate::{
    action_menu,
    actions::*,
    change_detail::{self, patch_set_label},
    checkouts::Checkouts,
    comments::{ChangeComments, CommentsEvent},
    diff::{
        DiffEvent, DiffSizing, DiffStatus, DiffView, ThreadContext,
        actions::{NextFile, PrevFile},
    },
    diff_load::{DiffSource, LoadError, context_label, load_diff},
    file_list::{Ask, FileList, FileListEvent, Files, Seed},
    filters::{FileMatcher, FileStyle, next_unskipped, skips},
    hint::Hint,
    local_rules::{self, Op, Target},
    local_source::{LocalSource, local_marker},
    nav::DiffRoute,
    outbox::{self, DraftOutbox},
    reply, reviewed,
    servers::ServerBadge,
    session::{Session, is_unauthorized},
    settings::{DiffSettings, FileFilters},
    syntax_cache::{Highlight, SyntaxCache, ahead_paths, diff_key, is_magic},
    theme::chip_colors,
};
use prefs::{next_context, next_tab_size, next_whitespace, whitespace_label};

/// The inputs of the work done for the files after the shown one, to tell
/// when it has to be done again.
#[derive(PartialEq)]
struct AheadState {
    keys: Vec<DiffKey>,
    enabled: bool,
    /// The file `]` goes to.
    next: Option<String>,
}

pub enum DiffPageEvent {
    Unauthorized,
    /// Move to another file of the same comparison.
    OpenFile(DiffRoute),
    /// Show another comparison of the same file (a new history entry).
    Compare(DiffRoute),
    /// The breadcrumb was clicked: same as `u`.
    UpToChange,
    /// `a`: open the reply dialog for this change.
    Reply(Box<reply::ReplyRequest>),
    /// `shift-a`: open the change action menu for this change.
    Actions(Box<ChangeDetail>),
}

/// What the first background task brings back.
struct Loaded {
    detail: Option<ChangeDetail>,
    /// The comparison, also when its file list could not be had.
    files_key: Option<FilesKey>,
    /// What the store had for `files_key`: on a miss the file list gets it
    /// from the checkout and Gerrit after this.
    seed: Seed,
    error: Option<String>,
}

pub struct DiffPage {
    session: Session,
    settings: Entity<DiffSettings>,
    route: DiffRoute,
    detail: Option<ChangeDetail>,
    /// The comparison's file list, its local source and reviewed marks.
    file_list: Entity<FileList>,
    /// What `file_list` last said (its key is fixed for the page).
    files: Files,
    /// Looking for a stored diff while the list is missing.
    stored_probe: Option<Task<()>>,
    /// The server's local checkouts, when it has any (the file list has
    /// them too).
    checkouts: Option<Entity<Checkouts>>,
    /// Where the diff on screen came from.
    source: Option<DiffSource>,
    /// The file filter rules and which are on, shared by all pages.
    filters: Entity<FileFilters>,
    /// Finished syntax colors, shared by all pages: a hit is shown at once,
    /// and the view's own result for a miss is stored here.
    syntax: Entity<SyntaxCache>,
    /// The filter style of each entry of `files`.
    styles: Vec<FileStyle>,
    /// A note for the user (why a key did nothing, a failed request).
    hint: Hint,
    /// The server's name and color, when there are several servers.
    server_badge: Option<ServerBadge>,
    /// The key of the diff shown or being fetched.
    key: Option<DiffKey>,
    /// The key of the diff that is on screen (`key` runs ahead of it while a
    /// refetch is in flight or after one failed).
    shown: Option<DiffKey>,
    view: Option<Entity<DiffView>>,
    view_sub: Option<Subscription>,
    loading: bool,
    error: Option<String>,
    prefs_open: bool,
    focus: FocusHandle,
    prefs_focus: FocusHandle,
    task: Option<Task<()>>,
    prefetch: Option<Task<()>>,
    /// What the files ahead were last set up for (see `ahead`).
    ahead_state: Option<AheadState>,
    /// The change's comment threads, shown in the diff.
    comments: Entity<ChangeComments>,
    outbox: Entity<DraftOutbox>,
    /// The thread the route asked the cursor to be on, until it is found.
    want_thread: Option<model::ThreadId>,
    /// A `]` (true) or `[` pressed before the file list was loaded, done
    /// once it is.
    pending_step: Option<bool>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<DiffPageEvent> for DiffPage {}

impl Focusable for DiffPage {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.view {
            Some(v) if !self.prefs_open => v.read(cx).focus_handle(cx),
            _ => self.focus.clone(),
        }
    }
}

impl DiffPage {
    pub fn new(
        session: Session,
        settings: Entity<DiffSettings>,
        filters: Entity<FileFilters>,
        syntax: Entity<SyntaxCache>,
        route: DiffRoute,
        outbox: Entity<DraftOutbox>,
        cx: &mut Context<Self>,
    ) -> Self {
        let comments = outbox.update(cx, |o, cx| o.comments(route.change, cx));
        let file_list = cx.new(|_| FileList::new(session.clone()));
        let want_thread = route.thread.clone();
        let mut this = Self {
            session,
            settings: settings.clone(),
            route,
            detail: None,
            file_list: file_list.clone(),
            files: Files::default(),
            stored_probe: None,
            checkouts: None,
            source: None,
            filters: filters.clone(),
            syntax,
            styles: Vec::new(),
            hint: Hint::default(),
            server_badge: None,
            key: None,
            shown: None,
            view: None,
            view_sub: None,
            loading: true,
            error: None,
            prefs_open: false,
            focus: cx.focus_handle(),
            prefs_focus: cx.focus_handle(),
            task: None,
            prefetch: None,
            ahead_state: None,
            comments: comments.clone(),
            outbox,
            want_thread,
            pending_step: None,
            _subs: Vec::new(),
        };
        this._subs
            .push(cx.observe(&settings, |this, _, cx| this.settings_changed(cx)));
        this._subs.push(cx.observe(&filters, |this, _, cx| {
            this.restyle(cx);
            this.ahead(cx);
            cx.notify();
        }));
        this._subs
            .push(cx.subscribe(&comments, |this, _, ev, cx| match ev {
                CommentsEvent::Changed => this.apply_threads(cx),
                CommentsEvent::Unauthorized => cx.emit(DiffPageEvent::Unauthorized),
            }));
        this._subs
            .push(cx.subscribe(&file_list, |this, _, ev, cx| this.files_changed(ev, cx)));
        this.load(cx);
        this
    }

    pub fn route(&self) -> &DiffRoute {
        &self.route
    }

    #[cfg(test)]
    pub(crate) fn view_for_test(&self) -> Option<Entity<DiffView>> {
        self.view.clone()
    }

    #[cfg(test)]
    pub(crate) fn shown_for_test(&self) -> Option<&DiffKey> {
        self.shown.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn key_for_test(&self) -> Option<&DiffKey> {
        self.key.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn prefs_open_for_test(&self) -> bool {
        self.prefs_open
    }

    #[cfg(test)]
    pub(crate) fn error_for_test(&self) -> Option<&str> {
        self.error.as_deref()
    }

    // ---- loading ----

    fn prefs(&self, cx: &App) -> ResolvedDiffPreferences {
        self.settings.read(cx).resolved()
    }

    fn make_key(&self, files: &FilesKey, cx: &App) -> DiffKey {
        diff_key(files, &self.route.path, self.prefs(cx).ignore_whitespace)
    }

    /// Everything, from the cache where possible.
    fn load(&mut self, cx: &mut Context<Self>) {
        let syncer = self.session.syncer().clone();
        let can_sync = self.session.can_sync();
        let route = self.route.clone();
        self.loading = true;
        self.task = Some(cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(load_everything(syncer, can_sync, route))
                .await;
            this.update(cx, |this, cx| this.loaded(loaded, cx)).ok();
        }));
    }

    /// A stored diff is shown without waiting for a list: the diff is
    /// fetched at once if the store has it (`load_diff` then reads the
    /// store and never touches git).
    fn probe_stored_diff(&mut self, files: &FilesKey, cx: &mut Context<Self>) {
        let key = self.make_key(files, cx);
        let syncer = self.session.syncer().clone();
        self.stored_probe = Some(cx.spawn(async move |this, cx| {
            let hit = cx
                .background_spawn(async move { matches!(syncer.cached_diff(&key), Ok(Some(_))) })
                .await;
            if hit {
                this.update(cx, |this, cx| this.fetch(cx)).ok();
            }
        }));
    }

    /// Fetches the diff and does the step asked for meanwhile, once a list
    /// has arrived or nothing is left that could bring one.
    fn files_settled(&mut self, cx: &mut Context<Self>) {
        if !self.files.settled {
            return;
        }
        self.fetch(cx);
        if let Some(forward) = self.pending_step.take() {
            self.step_file(forward, cx);
        }
    }

    /// The file list said something changed: take its snapshot, and redo
    /// what depends on it.
    fn files_changed(&mut self, ev: &FileListEvent, cx: &mut Context<Self>) {
        self.files = self.file_list.read(cx).files();
        match ev {
            FileListEvent::Listed => {
                // What depends on the list: the styles, the old path of a
                // rename, the files ahead (and the prefetch decision), the
                // position; then the diff, if it waited for the list.
                self.restyle(cx);
                self.apply_threads(cx);
                self.ahead_state = None;
                self.ahead(cx);
                self.files_settled(cx);
            }
            FileListEvent::Changed => self.files_settled(cx),
            FileListEvent::MarksChanged => {}
            FileListEvent::LocalResolved => {
                // The prefetch decision and the marker depend on it.
                self.ahead_state = None;
                self.ahead(cx);
            }
            FileListEvent::ReviewedFailed(note) => {
                self.hint.say(note.clone(), true, cx, |p| &mut p.hint);
            }
            FileListEvent::Unauthorized => cx.emit(DiffPageEvent::Unauthorized),
        }
        cx.notify();
    }

    fn loaded(&mut self, l: Loaded, cx: &mut Context<Self>) {
        self.detail = l.detail;
        let step = self.pending_step.take();
        if let Some(e) = l.error {
            self.loading = false;
            self.error = Some(e);
            cx.notify();
            return;
        }
        let Some(key) = l.files_key else {
            self.loading = false;
            self.error = Some("the diff could not be requested".into());
            if let Some(forward) = step {
                self.step_file(forward, cx);
            }
            cx.notify();
            return;
        };
        let project = self
            .detail
            .as_ref()
            .map(|d| d.summary.project.clone())
            .unwrap_or_default();
        let miss = matches!(l.seed, Seed::Miss);
        self.file_list.update(cx, |f, cx| {
            f.load(Some(key.clone()), &project, l.seed, Ask::OnMiss, cx)
        });
        self.files = self.file_list.read(cx).files();
        self.restyle(cx);
        if miss {
            // The diff is fetched, and the step done, once a list is there
            // (or nothing is left to bring one); a stored diff at once.
            self.pending_step = step;
            self.probe_stored_diff(&key, cx);
            self.files_settled(cx);
            cx.notify(); // the header, from the detail
            return;
        }
        self.fetch(cx);
        if let Some(forward) = step {
            self.step_file(forward, cx);
        }
        cx.notify();
    }

    /// Styles every file with the rules that are on for the change's
    /// project.
    fn restyle(&mut self, cx: &App) {
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
    }

    /// The filter style of the file shown.
    fn current_style(&self) -> Option<&FileStyle> {
        self.styles.get(self.position()?)
    }

    /// Fetches the diff for the current preferences (after the whitespace
    /// option changed). The shown diff stays until the new one is there.
    fn fetch(&mut self, cx: &mut Context<Self>) {
        let Some(files) = self.files.key.clone() else {
            return;
        };
        let key = self.make_key(&files, cx);
        if self.key.as_ref() == Some(&key) {
            return;
        }
        self.key = Some(key.clone());
        let syncer = self.session.syncer().clone();
        let can_sync = self.session.can_sync();
        self.loading = self.view.is_none();
        if let Some(view) = &self.view {
            let msg = format!(
                "Loading\u{2026} (whitespace: {})",
                whitespace_label(&key.whitespace)
            );
            view.update(cx, |v, cx| {
                v.set_status(Some(DiffStatus::Busy(msg.into())), cx)
            });
        }
        // A stored diff is shown without waiting for git: load_diff awaits
        // the local source only after the store missed.
        let local = self.files.local_lazy();
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(load_diff(syncer, key.clone(), can_sync, local))
                .await;
            this.update(cx, |this, cx| {
                this.diff_arrived(key, result, cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn diff_arrived(
        &mut self,
        key: DiffKey,
        result: Result<(model::Diff, DiffSource), LoadError>,
        cx: &mut Context<Self>,
    ) {
        if self.key.as_ref().is_some_and(|k| *k != key) {
            return; // superseded
        }
        self.loading = false;
        match result {
            Ok((diff, source)) => {
                self.error = None;
                self.source = Some(source);
                self.key = Some(key.clone());
                self.shown = Some(key.clone());
                let path = self.route.path.clone();
                let cached = self.syntax.update(cx, |c, _| c.get(&key));
                match &self.view {
                    Some(view) => view.update(cx, |v, cx| {
                        v.set_diff_for_cached(&path, diff, cached, cx);
                        v.set_status(None, cx);
                    }),
                    None => self.create_view(diff, cached, cx),
                }
                self.ahead(cx);
            }
            Err(LoadError::Unauthorized) => cx.emit(DiffPageEvent::Unauthorized),
            Err(e) => {
                // With a diff on screen the page keeps it, so the failure
                // has to show on the view or it passes unseen.
                if let Some(view) = &self.view {
                    let msg = format!("Could not load the diff: {e}");
                    view.update(cx, |v, cx| {
                        v.set_status(Some(DiffStatus::Error(msg.into())), cx)
                    });
                }
                self.error = Some(e.to_string());
                // Back to what is on screen, so that the header tells the
                // truth and the same request can be tried again.
                if self.shown.is_some() {
                    self.key = self.shown.clone();
                }
            }
        }
    }

    fn create_view(
        &mut self,
        diff: model::Diff,
        cached: Option<Highlight>,
        cx: &mut Context<Self>,
    ) {
        let (prefs, font) = {
            let s = self.settings.read(cx);
            (s.resolved(), s.font_family())
        };
        let can_comment = self.session.can_comment();
        let path = self.route.path.clone();
        let view = cx.new(|cx| {
            let mut v = DiffView::new(diff, prefs, cx);
            v.set_path_cached(&path, cached, cx);
            v.set_can_comment(can_comment, cx);
            v.set_sizing(DiffSizing::Fill, cx);
            v.set_font_family(font, cx);
            v
        });
        self.view_sub = Some(cx.subscribe(&view, |this, _, ev: &DiffEvent, cx| match ev {
            DiffEvent::ModeChanged(mode) => {
                let mode = *mode;
                this.settings.update(cx, |s, cx| {
                    s.update_session(|p| p.view_mode = Some(mode), cx)
                });
            }
            DiffEvent::RowsChanged => {}
            DiffEvent::Highlighted(result) => {
                // The view's job is for the diff on screen.
                if let Some(key) = this.shown.clone() {
                    let result = result.clone();
                    this.syntax.update(cx, |c, _| c.insert(key, result));
                }
            }
            DiffEvent::Draft(request) => {
                let (change, request) = (this.route.change, request.clone());
                let detail = this.detail.clone();
                this.outbox
                    .update(cx, |o, cx| o.request(change, request, detail.as_ref(), cx));
            }
        }));
        self.view = Some(view);
        self.apply_threads(cx);
    }

    /// Hands the change's threads to the diff view, which shows those that
    /// belong to this file and patch set pair.
    fn apply_threads(&mut self, cx: &mut Context<Self>) {
        let Some(view) = &self.view else {
            return;
        };
        let old_path = self
            .files
            .list
            .iter()
            .find(|f| f.path == self.route.path)
            .and_then(|f| f.old_path.clone());
        let context = ThreadContext {
            pair: model::DiffPair {
                base: self.route.base,
                target: self.route.target,
            },
            path: self.route.path.clone(),
            old_path,
        };
        let threads = self.comments.read(cx).threads();
        let any = !threads.is_empty();
        let want = self.want_thread.clone();
        let found = view.update(cx, |v, cx| {
            v.set_threads(context, threads, cx);
            want.is_some_and(|id| v.focus_thread(&id, cx))
        });
        // Wait for the comments if they are not there yet; once they are
        // and the thread is not among them (deleted), forget the wish.
        if found || any {
            self.want_thread = None;
        }
    }

    /// Gets the files after the shown one ready, in the order `]` visits
    /// them: the syntax worker fetches and highlights the next unskipped
    /// files, then the earlier ones. Where the worker does not cover the
    /// file `]` goes to (syntax highlighting is off, or that is a magic
    /// file, which is never highlighted), its diff is just prefetched. Does
    /// nothing when nothing it depends on changed since the last call.
    fn ahead(&mut self, cx: &mut Context<Self>) {
        let (Some(files), Some(shown)) = (self.files.key.clone(), self.shown.clone()) else {
            return;
        };
        let prefs = self.prefs(cx);
        let enabled = prefs.syntax_highlighting;
        let next = self.position().and_then(|from| {
            let skipped = |i: usize| self.styles.get(i).is_some_and(skips);
            next_unskipped(self.files.list.len(), from, true, skipped)
        });
        let next = next
            .and_then(|i| self.files.list.get(i))
            .map(|f| f.path.clone());
        let keys: Vec<DiffKey> = if enabled {
            ahead_paths(&self.files.list, &self.styles, Some(&self.route.path))
                .into_iter()
                .map(|p| diff_key(&files, p, prefs.ignore_whitespace.clone()))
                .collect()
        } else {
            Vec::new()
        };
        let state = AheadState {
            keys,
            enabled,
            next,
        };
        if self.ahead_state.as_ref() == Some(&state) {
            return;
        }
        let session = self.session.clone();
        let keys = state.keys.clone();
        let local = self.files.local_lazy();
        self.syntax
            .update(cx, |c, cx| c.set_queue(keys, &session, local, enabled, cx));
        let covered = enabled && state.next.as_deref().is_some_and(|p| !is_magic(p));
        if !covered && let Some(next) = &state.next {
            self.prefetch_next(&shown, next, cx);
        }
        self.ahead_state = Some(state);
    }

    /// The local source for the comparison on show, once resolved.
    fn local_source(&self) -> Option<LocalSource> {
        self.files.local_source()
    }

    /// Whether the comparison involves a local version whose two sides
    /// have different parents (see [`Files::parents_differ`]): nobody can
    /// show it.
    fn parents_differ(&self) -> bool {
        self.files.parents_differ()
    }

    /// The same file against the parent, which the checkout can show.
    fn compare_with_parent(&mut self, cx: &mut Context<Self>) {
        cx.emit(DiffPageEvent::Compare(DiffRoute {
            base: None,
            thread: None,
            ..self.route.clone()
        }));
    }

    /// Names the server's checkouts, so diffs can come from the project's
    /// local checkout. Set right after the page is made, before anything
    /// has loaded.
    pub fn set_checkouts(&mut self, checkouts: Option<Entity<Checkouts>>, cx: &mut App) {
        self.checkouts = checkouts.clone();
        self.file_list.update(cx, |f, _| f.set_checkouts(checkouts));
    }

    /// The checkout the file list was made in, while it is the local one.
    #[cfg(test)]
    pub(crate) fn files_local_for_test(&self) -> Option<&std::path::Path> {
        self.files.checkout()
    }

    #[cfg(test)]
    pub(crate) fn file_paths_for_test(&self) -> Vec<&str> {
        self.files.list.iter().map(|f| f.path.as_str()).collect()
    }

    #[cfg(test)]
    pub(crate) fn source_for_test(&self) -> Option<DiffSource> {
        self.source
    }

    /// Fetches the next file's diff in the background so that `]` is
    /// instant.
    fn prefetch_next(&mut self, shown: &DiffKey, next: &str, cx: &mut Context<Self>) {
        // Gerrit has nothing to prefetch for a local version.
        if !self.session.can_sync() || self.files.list.is_empty() || shown.is_local() {
            return;
        }
        let syncer: Syncer = self.session.syncer().clone();
        // The checkout can serve the next file when it is needed: no
        // diffing ahead for a diff that costs a git call.
        if let Some(local) = self.local_source()
            && local.serves(&shown.with_path(next))
        {
            return;
        }
        // The file after the shown one that `]` would go to: the shown file
        // and the ones that are not skipped, since the prefetch takes the
        // next entry of the list it is given.
        let files: Vec<FileEntry> = self
            .files
            .list
            .iter()
            .zip(&self.styles)
            .filter(|(f, style)| f.path == self.route.path || !skips(style))
            .map(|(f, _)| f.clone())
            .collect();
        let key = match &self.files.key {
            Some(fk) => self.make_key(fk, cx),
            None => shown.clone(),
        };
        self.prefetch = Some(cx.background_spawn(async move {
            if let Err(e) = syncer.prefetch_next_diff(&key, &files).await {
                log::debug!("prefetching the next diff failed: {e}");
            }
        }));
    }

    /// Preferences changed (here or by another page): restyle, and refetch
    /// if the whitespace option changed.
    fn settings_changed(&mut self, cx: &mut Context<Self>) {
        let (prefs, font) = {
            let s = self.settings.read(cx);
            (s.resolved(), s.font_family())
        };
        if let Some(view) = &self.view {
            view.update(cx, |v, cx| {
                v.set_preferences(prefs, cx);
                v.set_font_family(font, cx);
            });
        }
        self.fetch(cx);
        self.ahead(cx);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn is_reviewed_for_test(&self) -> bool {
        self.files.is_reviewed(&self.route.path)
    }

    #[cfg(test)]
    pub(crate) fn detail_for_test(&self) -> Option<&ChangeDetail> {
        self.detail.as_ref()
    }

    /// Shows a failure that happened elsewhere (the shell's background
    /// saves) on the hint line.
    pub(crate) fn say_error(&mut self, text: String, cx: &mut Context<Self>) {
        self.hint.say(text, true, cx, |p| &mut p.hint);
    }

    /// Whether the diff's draft editor holds text the user wrote.
    pub fn is_writing(&self, cx: &App) -> bool {
        self.view
            .as_ref()
            .is_some_and(|v| v.read(cx).has_written_text(cx))
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

    // ---- reply ----

    /// `a`: asks the shell for the reply dialog: a reply to Gerrit, or for
    /// a local version shown a local review of it (see [`reply`]).
    fn reply(&mut self, _: &Reply, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(why) = local_rules::refusal(Op::Reply, &self.rule_target(), &self.session) {
            self.hint.say(why, false, cx, |p| &mut p.hint);
            return;
        }
        match &self.detail {
            Some(d) if d.current().is_some() => {
                let strategy = self
                    .checkouts
                    .as_ref()
                    .map_or(model::LocalApproval::Exact, |c| {
                        c.read(cx).local_approval(&d.summary.project)
                    });
                let request = reply::ReplyRequest {
                    detail: d.clone(),
                    target: Some(self.route.target),
                    edit: None,
                    strategy,
                };
                cx.emit(DiffPageEvent::Reply(Box::new(request)))
            }
            _ => self
                .hint
                .say("The change is not loaded yet", false, cx, |p| &mut p.hint),
        }
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

    /// A review was posted from the dialog: take the change as the server
    /// has it now and say so.
    pub fn reply_sent(&mut self, sent: &reply::Sent, cx: &mut Context<Self>) {
        if let Some(d) = &sent.detail {
            self.detail = Some(change_detail::carry_local(d.clone(), self.detail.as_ref()));
        }
        let (note, warning) = sent.note();
        self.hint.say(note, warning, cx, |p| &mut p.hint);
        cx.notify();
    }

    // ---- change actions ----

    /// What the page shows, for [`local_rules`]: known from the route,
    /// also before the detail is loaded and when signed out.
    fn rule_target(&self) -> Target {
        Target {
            change: self.route.change,
            target: Some(self.route.target),
            base: self.route.base,
        }
    }

    /// `shift-a`: asks the shell for the change action menu (`m` is the
    /// unified / side-by-side toggle in the diff).
    fn open_actions(&mut self, _: &ChangeMenu, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(why) = local_rules::refusal(Op::Actions, &self.rule_target(), &self.session) {
            self.hint.say(why, false, cx, |p| &mut p.hint);
            return;
        }
        match &self.detail {
            Some(d) => cx.emit(DiffPageEvent::Actions(Box::new(d.clone()))),
            None => self
                .hint
                .say("The change is not loaded yet", false, cx, |p| &mut p.hint),
        }
    }

    /// An action went through: take the change as the server has it now
    /// (the status chips follow) and say so.
    ///
    /// When the diff showed the current patch set and a new one has become
    /// current (a rebase), the answer is the route of the same file in the
    /// new patch set, which the shell opens: the page would otherwise keep
    /// reviewing a patch set the change has moved on from, and the change
    /// page does the same switch. A diff of an older patch set stays, with
    /// a note about the new current one.
    pub fn action_done(
        &mut self,
        done: &action_menu::Done,
        cx: &mut Context<Self>,
    ) -> Option<DiffRoute> {
        let old_current = self.detail.as_ref().and_then(|d| d.current_patch_set);
        let mut note = done.note();
        let mut moved = None;
        match &done.detail {
            Some(d) => {
                if let Some(new) = d.current_patch_set.filter(|n| Some(*n) != old_current) {
                    if old_current == Some(self.route.target) {
                        moved = Some(DiffRoute {
                            target: new,
                            thread: None,
                            ..self.route.clone()
                        });
                    } else {
                        let label = patch_set_label(&d.revisions, new);
                        note = format!("{note}; {label} is now current");
                    }
                }
                self.detail = Some(change_detail::carry_local(d.clone(), self.detail.as_ref()));
            }
            None => note = format!("{note}; could not refresh the change"),
        }
        if moved.is_none() {
            self.hint.say(note, false, cx, |p| &mut p.hint);
            cx.notify();
        }
        moved
    }

    /// A note under the header, for the shell after it rebuilt the page.
    pub fn say(&mut self, note: String, cx: &mut Context<Self>) {
        self.hint.say(note, false, cx, |p| &mut p.hint);
        cx.notify();
    }

    /// The server refused an action and the change was fetched again: show
    /// the chips as they are now (the dialog shows the error).
    pub fn detail_refreshed(&mut self, detail: &ChangeDetail, cx: &mut Context<Self>) {
        self.detail = Some(change_detail::carry_local(
            detail.clone(),
            self.detail.as_ref(),
        ));
        self.restyle(cx);
        cx.notify();
    }

    // ---- reviewed mark ----

    /// `r`: marks the shown file reviewed, or clears the mark. The mark
    /// changes on screen at once; if the request fails it goes back and a
    /// note says why.
    fn toggle_reviewed(&mut self, _: &ToggleReviewed, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(why) = local_rules::refusal(Op::Reviewed, &self.rule_target(), &self.session) {
            self.hint.say(why, false, cx, |p| &mut p.hint);
            return;
        }
        if self.files.key.is_none() {
            return;
        }
        if self.files.marks_wait() {
            // Gerrit's list, computed before the mark, would replace it.
            self.hint.say(reviewed::WAITS, false, cx, |p| &mut p.hint);
            return;
        }
        let path = self.route.path.clone();
        let reviewed = !self.files.is_reviewed(&path);
        self.file_list
            .update(cx, |f, cx| f.set_reviewed(&path, reviewed, cx));
        self.files = self.file_list.read(cx).files();
        cx.notify();
    }

    // ---- navigation between files ----

    fn position(&self) -> Option<usize> {
        self.files
            .list
            .iter()
            .position(|f| f.path == self.route.path)
    }

    fn go_to(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(file) = self.files.list.get(ix) else {
            return;
        };
        if file.path != self.route.path {
            cx.emit(DiffPageEvent::OpenFile(DiffRoute {
                path: file.path.clone(),
                thread: None,
                ..self.route.clone()
            }));
        }
    }

    /// The next / previous file that is neither hidden nor dimmed. `]` on
    /// the last one goes back up to the change (`[` on the first does
    /// nothing). Each file is a new page, so a quick second `]` often
    /// comes while the file list is still loading; it waits for it.
    fn step_file(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(from) = self.position() else {
            if (self.loading && self.files.key.is_none()) || !self.files.settled {
                self.pending_step = Some(forward);
            }
            return;
        };
        let skipped = |i: usize| self.styles.get(i).is_some_and(skips);
        match next_unskipped(self.files.list.len(), from, forward, skipped) {
            Some(i) => self.go_to(i, cx),
            None if forward => cx.emit(DiffPageEvent::UpToChange),
            None => {}
        }
    }

    fn next_file(&mut self, _: &NextFile, _: &mut Window, cx: &mut Context<Self>) {
        self.step_file(true, cx);
    }

    fn prev_file(&mut self, _: &PrevFile, _: &mut Window, cx: &mut Context<Self>) {
        self.step_file(false, cx);
    }

    // ---- preferences panel ----

    fn toggle_prefs(&mut self, _: &TogglePrefs, window: &mut Window, cx: &mut Context<Self>) {
        self.prefs_open = !self.prefs_open;
        let target = if self.prefs_open {
            self.prefs_focus.clone()
        } else {
            self.focus_handle(cx)
        };
        window.focus(&target, cx);
        cx.notify();
    }

    fn close_prefs(&mut self, _: &ClosePrefs, window: &mut Window, cx: &mut Context<Self>) {
        if self.prefs_open {
            self.toggle_prefs(&TogglePrefs, window, cx);
        }
    }

    fn set_session(
        &mut self,
        change: impl FnOnce(&mut model::DiffPreferences, &ResolvedDiffPreferences),
        cx: &mut Context<Self>,
    ) {
        let resolved = self.prefs(cx);
        self.settings
            .update(cx, |s, cx| s.update_session(|p| change(p, &resolved), cx));
    }

    fn prefs_mode(&mut self, _: &PrefsMode, _: &mut Window, cx: &mut Context<Self>) {
        self.set_session(|p, r| p.view_mode = Some(r.view_mode.toggled()), cx);
    }

    fn prefs_context(&mut self, _: &PrefsContext, _: &mut Window, cx: &mut Context<Self>) {
        self.set_session(|p, r| p.context = Some(next_context(r.context)), cx);
    }

    fn prefs_whitespace(&mut self, _: &PrefsWhitespace, _: &mut Window, cx: &mut Context<Self>) {
        self.set_session(
            |p, r| p.ignore_whitespace = Some(next_whitespace(&r.ignore_whitespace)),
            cx,
        );
    }

    fn prefs_tab_size(&mut self, _: &PrefsTabSize, _: &mut Window, cx: &mut Context<Self>) {
        self.set_session(|p, r| p.tab_size = Some(next_tab_size(r.tab_size)), cx);
    }

    fn prefs_show_tabs(&mut self, _: &PrefsShowTabs, _: &mut Window, cx: &mut Context<Self>) {
        self.set_session(|p, r| p.show_tabs = Some(!r.show_tabs), cx);
    }

    fn prefs_intraline(&mut self, _: &PrefsIntraline, _: &mut Window, cx: &mut Context<Self>) {
        self.set_session(
            |p, r| p.intraline_difference = Some(!r.intraline_difference),
            cx,
        );
    }

    fn prefs_wrap(&mut self, _: &PrefsWrap, _: &mut Window, cx: &mut Context<Self>) {
        self.set_session(|p, r| p.line_wrapping = Some(!r.line_wrapping), cx);
    }

    fn prefs_syntax(&mut self, _: &PrefsSyntax, _: &mut Window, cx: &mut Context<Self>) {
        self.set_session(
            |p, r| p.syntax_highlighting = Some(!r.syntax_highlighting),
            cx,
        );
    }

    // ---- rendering ----

    fn path_label(&self) -> String {
        let old = self
            .files
            .list
            .iter()
            .find(|f| f.path == self.route.path)
            .and_then(|f| f.old_path.clone());
        let name = match self.route.path.as_str() {
            model::COMMIT_MSG_PATH => "Commit message",
            model::MERGE_LIST_PATH => "Merge list",
            p => p,
        };
        match old {
            Some(old) => format!("{old} \u{2192} {name}"),
            None => name.to_owned(),
        }
    }

    /// "N of M" over the files that are not hidden. When the shown file is
    /// one of the hidden ones there is no number for it.
    fn position_label(&self) -> Option<String> {
        let shown = self.styles.iter().filter(|s| !s.hidden).count();
        let i = self.position()?;
        Some(if self.styles.get(i).is_some_and(|s| s.hidden) {
            format!("\u{2013} of {shown}")
        } else {
            let rank = self.styles.iter().take(i + 1).filter(|s| !s.hidden).count();
            format!("{rank} of {shown}")
        })
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let prefs = self.prefs(cx);
        let subject = self
            .detail
            .as_ref()
            .map_or_else(String::new, |d| d.summary.subject.clone());
        let chips = self
            .detail
            .as_ref()
            .map_or_else(Vec::new, |d| crate::row::chips(&d.summary));
        let revisions = self.detail.as_ref().map_or(&[][..], |d| &d.revisions);
        let label = |n| patch_set_label(revisions, n);
        let pair = match self.route.base {
            Some(b) => format!("{} \u{2192} {}", label(b), label(self.route.target)),
            None => format!("Base \u{2192} {}", label(self.route.target)),
        };
        let heading = change_heading(self.route.change);
        let position = self.position_label();
        let style = self.current_style().cloned().unwrap_or_default();
        let mode = match prefs.view_mode {
            DiffViewMode::SideBySide => "side-by-side",
            DiffViewMode::Unified => "unified",
        };
        // What is on screen, not what was asked for: a failed or pending
        // request must not pass for the current state.
        let key = self.shown.as_ref().or(self.key.as_ref());
        let pending = self.key != self.shown && self.shown.is_some();
        let context = self.prefs(cx).context;
        let local_path = (self.source == Some(DiffSource::Local))
            .then(|| self.local_source().map(|s| s.path().to_owned()))
            .flatten();
        // The list the position and `]` go by comes from the checkout too;
        // the marker of a local diff already says it.
        let list_path = self.files.checkout.clone().filter(|_| local_path.is_none());
        v_flex()
            .px_3()
            .py_2()
            .gap_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .items_baseline()
                    .min_w_0()
                    .children(self.server_badge.as_ref().map(|b| b.pill(cx)))
                    .child(
                        h_flex()
                            .id("diff-up-to-change")
                            .gap_2()
                            .items_baseline()
                            .min_w_0()
                            .cursor_pointer()
                            .rounded_sm()
                            .hover(|d| d.bg(theme.list_hover))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DiffPageEvent::UpToChange)))
                            .child(
                                div()
                                    .text_color(theme.link)
                                    .flex_none()
                                    .child(heading.clone())
                                    .debug_selector(move || format!("diff-change: {heading}")),
                            )
                            .child(div().font_semibold().min_w_0().truncate().child(subject)),
                    )
                    .children(chips.into_iter().map(|chip| {
                        let (fg, bg) = chip_colors(chip, cx);
                        div()
                            .flex_none()
                            .px_1()
                            .rounded_sm()
                            .text_xs()
                            .text_color(fg)
                            .bg(bg)
                            .child(chip.text())
                    }))
                    .child(div().text_color(muted).flex_none().child("\u{203A}"))
                    .child(
                        div()
                            .flex_none()
                            .font_family(theme.mono_font_family.clone())
                            .when(style.dimmed, |d| {
                                d.text_color(theme.foreground.opacity(0.55))
                            })
                            .when_some(style.color, |d, c| d.text_color(Hsla::from(c)))
                            .when(style.bold, |d| d.font_weight(FontWeight::BOLD))
                            .child(self.path_label()),
                    )
                    .when(style.hidden || style.dimmed, |d| {
                        d.child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(muted)
                                .child(format!("filtered: {}", style.rules.join(", "))),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_3()
                    .items_baseline()
                    .flex_wrap()
                    .text_sm()
                    .when(self.files.is_reviewed(&self.route.path), |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(theme.success)
                                .child("\u{2713} reviewed"),
                        )
                    })
                    .child(
                        div()
                            .text_color(muted)
                            .child(pair.clone())
                            .debug_selector(move || format!("diff-pair: {pair}")),
                    )
                    .children(position.map(|p| div().text_color(muted).child(p)))
                    .children(list_path.map(|p| local_marker("files-local", &p, cx)))
                    .children(self.hint.current().map(|(text, error)| {
                        div()
                            .text_xs()
                            .text_color(if error {
                                theme.danger
                            } else {
                                theme.foreground
                            })
                            .child(text.to_owned())
                    }))
                    .child(div().flex_1())
                    .when_some(key, |d, k| {
                        d.child(div().text_xs().text_color(muted).child(format!(
                            "context: {}  whitespace: {}{}",
                            context_label(context),
                            whitespace_label(&k.whitespace),
                            if pending { "  (updating\u{2026})" } else { "" }
                        )))
                    })
                    .children(local_path.map(|p| local_marker("diff-local", &p, cx)))
                    .child(
                        div()
                            .px_1()
                            .rounded_sm()
                            .text_xs()
                            .bg(theme.primary.opacity(0.15))
                            .text_color(theme.primary)
                            .child(mode),
                    )
                    .child(
                        Button::new("diff-prefs")
                            .label("Preferences  ( , )")
                            .xsmall()
                            .on_click(
                                cx.listener(|this, _, w, cx| {
                                    this.toggle_prefs(&TogglePrefs, w, cx)
                                }),
                            ),
                    ),
            )
    }

    /// In place of the diff when [`Self::parents_differ`]: why, and the
    /// way to a diff that can be shown.
    fn render_parents_differ(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let revisions = self.detail.as_ref().map_or(&[][..], |d| &d.revisions);
        let target = patch_set_label(revisions, self.route.target);
        v_flex()
            .p_4()
            .gap_2()
            .items_start()
            .child(
                div()
                    .child(PARENTS_DIFFER)
                    .debug_selector(|| "diff-parents-differ".to_owned()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(PARENTS_DIFFER_HINT),
            )
            .child(
                Button::new("diff-against-parent")
                    .label(format!("Compare Base \u{2192} {target}"))
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| this.compare_with_parent(cx))),
            )
            .into_any_element()
    }

    fn render_prefs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let p = self.prefs(cx);
        let row = |id: &'static str,
                   key: &'static str,
                   name: &'static str,
                   value: String,
                   on: fn(&mut Self, &mut Window, &mut Context<Self>)| {
            h_flex()
                .id(id)
                .gap_3()
                .px_2()
                .py_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(|d| d.bg(theme.list_hover))
                .on_click(cx.listener(move |this, _, w, cx| on(this, w, cx)))
                .child(
                    div()
                        .w(px(22.))
                        .flex_none()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .child(key),
                )
                .child(div().w(px(150.)).flex_none().child(name))
                .child(div().font_semibold().child(value))
        };
        let on_off = |b: bool| if b { "on" } else { "off" }.to_owned();
        div().absolute().top(px(70.)).right(px(16.)).child(
            v_flex()
                .id("diff-prefs-panel")
                .key_context(DIFF_PREFS)
                .track_focus(&self.prefs_focus)
                .on_action(cx.listener(Self::close_prefs))
                .on_action(cx.listener(Self::prefs_mode))
                .on_action(cx.listener(Self::prefs_context))
                .on_action(cx.listener(Self::prefs_whitespace))
                .on_action(cx.listener(Self::prefs_tab_size))
                .on_action(cx.listener(Self::prefs_show_tabs))
                .on_action(cx.listener(Self::prefs_intraline))
                .on_action(cx.listener(Self::prefs_syntax))
                .on_action(cx.listener(Self::prefs_wrap))
                .gap_0p5()
                .p_3()
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .bg(theme.popover)
                .text_color(theme.popover_foreground)
                .text_sm()
                .shadow_lg()
                .child(div().font_semibold().pb_1().child("Diff preferences"))
                .child(row(
                    "pref-mode",
                    "m",
                    "View",
                    match p.view_mode {
                        DiffViewMode::SideBySide => "side-by-side",
                        DiffViewMode::Unified => "unified",
                    }
                    .into(),
                    |t, w, cx| t.prefs_mode(&PrefsMode, w, cx),
                ))
                .child(row(
                    "pref-context",
                    "c",
                    "Context",
                    context_label(p.context),
                    |t, w, cx| t.prefs_context(&PrefsContext, w, cx),
                ))
                .child(row(
                    "pref-ws",
                    "w",
                    "Whitespace",
                    whitespace_label(&p.ignore_whitespace).into(),
                    |t, w, cx| t.prefs_whitespace(&PrefsWhitespace, w, cx),
                ))
                .child(row(
                    "pref-tab",
                    "t",
                    "Tab width",
                    p.tab_size.to_string(),
                    |t, w, cx| t.prefs_tab_size(&PrefsTabSize, w, cx),
                ))
                .child(row(
                    "pref-show-tabs",
                    "s",
                    "Show tabs",
                    on_off(p.show_tabs),
                    |t, w, cx| t.prefs_show_tabs(&PrefsShowTabs, w, cx),
                ))
                .child(row(
                    "pref-intraline",
                    "i",
                    "Intraline changes",
                    on_off(p.intraline_difference),
                    |t, w, cx| t.prefs_intraline(&PrefsIntraline, w, cx),
                ))
                .child(row(
                    "pref-syntax",
                    "y",
                    "Syntax highlighting",
                    on_off(p.syntax_highlighting),
                    |t, w, cx| t.prefs_syntax(&PrefsSyntax, w, cx),
                ))
                .child(row(
                    "pref-wrap",
                    "l",
                    "Line wrapping",
                    on_off(p.line_wrapping),
                    |t, w, cx| t.prefs_wrap(&PrefsWrap, w, cx),
                ))
                .child(
                    div()
                        .pt_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Changes last until you quit. Esc or , closes."),
                ),
        )
    }
}

/// Why a comparison of a local version with another patch set cannot be
/// shown (see [`DiffPage::parents_differ`]).
pub(crate) const PARENTS_DIFFER: &str =
    "Not available locally: the two versions have different parents (rebased).";
/// What to do instead.
pub(crate) const PARENTS_DIFFER_HINT: &str = "Gerrit marks the edits a rebase brings, which the checkout cannot, and Gerrit does not have \
     the local version. Compare against the parent (Base) instead.";

/// The breadcrumb's name of the change: `Change 123:`, or `Local change:`
/// (its number is synthetic).
fn change_heading(number: model::ChangeNumber) -> String {
    if number.is_local() {
        "Local change:".to_owned()
    } else {
        format!("Change {number}:")
    }
}

/// The detail (from [`change_detail`], so with the local patch sets), the
/// comparison and its stored file list. Gerrit is asked for the detail
/// only when the cache lacks the patch sets of the route, never for a
/// local change.
async fn load_everything(syncer: Syncer, can_sync: bool, route: DiffRoute) -> Loaded {
    let fail = |detail: Option<ChangeDetail>, error: String| Loaded {
        detail,
        files_key: None,
        seed: Seed::Unread,
        error: Some(error),
    };
    let mut detail = match change_detail::cached(&syncer, route.change) {
        Ok(p) => p.map(|p| p.detail),
        Err(e) => {
            log::warn!("reading cached change {}: {e}", route.change);
            None
        }
    };
    let has_revisions = |d: &Option<ChangeDetail>| {
        d.as_ref().is_some_and(|d| {
            d.revision(route.target).is_some() && route.base.is_none_or(|b| d.revision(b).is_some())
        })
    };
    if !has_revisions(&detail) && can_sync && !route.change.is_local() {
        let shown = change_detail::shown_locals(detail.as_ref(), route.change);
        match change_detail::refreshed(&syncer, route.change, shown).await {
            Ok(d) => detail = Some(d),
            Err(e) if is_unauthorized(&e) => {
                return fail(detail, "unauthorized".into());
            }
            Err(e) => log::warn!("refreshing change {}: {e}", route.change),
        }
    }
    let Some(d) = detail.clone().filter(|_| has_revisions(&detail)) else {
        let revisions = detail.as_ref().map_or(&[][..], |d| &d.revisions);
        let offline = if can_sync || route.change.is_local() {
            ""
        } else {
            " offline"
        };
        let error = format!(
            "{} with {} is not available{offline}.",
            change_heading(route.change).trim_end_matches(':'),
            patch_set_label(revisions, route.target),
        );
        return fail(detail, error);
    };
    let (Some(target), base) = (
        d.revision(route.target).map(|r| r.as_ref()),
        route.base.and_then(|b| d.revision(b)).map(|r| r.as_ref()),
    ) else {
        return fail(detail, "patch set not found".into());
    };
    let files_key = FilesKey {
        change: route.change,
        base,
        target,
    };
    // Read here, with the detail, so that the first diff fetch has the
    // list's renames. A miss is the file list's to fill: from the checkout,
    // and from Gerrit.
    let seed = match syncer.cached_files(&files_key) {
        Ok(Some(c)) => Seed::Hit(c),
        Ok(None) => Seed::Miss,
        Err(e) => {
            log::warn!("reading cached files: {e}");
            Seed::Miss
        }
    };
    Loaded {
        detail: Some(d),
        files_key: Some(files_key),
        seed,
        error: None,
    }
}

impl Render for DiffPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The shell focuses the page before its diff exists; hand the
        // keyboard to the diff view as soon as it is there.
        if let Some(view) = &self.view
            && !self.prefs_open
            && self.focus.is_focused(window)
        {
            let handle = view.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
        let theme = cx.theme().clone();
        let body: AnyElement = match (&self.view, &self.error) {
            (Some(view), _) => div().size_full().child(view.clone()).into_any_element(),
            (None, _) if self.parents_differ() => self.render_parents_differ(cx),
            (None, Some(e)) => div()
                .p_4()
                .text_color(theme.danger)
                .child(format!("Could not load the diff: {e}"))
                .into_any_element(),
            (None, None) => div()
                .p_4()
                .text_color(theme.muted_foreground)
                .child(if self.loading {
                    "Loading diff\u{2026}"
                } else {
                    ""
                })
                .into_any_element(),
        };
        v_flex()
            .id("diff-page")
            .key_context(DIFF_PAGE)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::next_file))
            .on_action(cx.listener(Self::prev_file))
            .on_action(cx.listener(Self::toggle_prefs))
            .on_action(cx.listener(Self::prefs_syntax))
            .on_action(cx.listener(Self::toggle_reviewed))
            .on_action(cx.listener(Self::reply))
            .on_action(cx.listener(Self::open_actions))
            .relative()
            .size_full()
            .children(self.server_badge.as_ref().map(|b| b.accent_line(cx)))
            .child(self.render_header(cx))
            .child(div().flex_1().min_h_0().child(body))
            .when(self.prefs_open, |d| d.child(self.render_prefs(cx)))
    }
}
