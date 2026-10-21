//! The diff page: one file's diff of a change, full size, with its own
//! entry in the history (like PolyGerrit's per-file URL).
//!
//! Loading: the change detail, the file list (for the header, `[`/`]` and
//! the prefetch) and the diff come from the cache first. A diff is keyed by
//! revision SHA and immutable, so a cached one is shown without any request;
//! only a miss is fetched, on the background executor, with a "loading"
//! placeholder until it arrives. Once a diff is shown, the next file's diff
//! is prefetched in the background.
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

use std::collections::HashSet;

use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use model::{ChangeDetail, DiffKey, DiffViewMode, FileEntry, FilesKey, ResolvedDiffPreferences};
use sync::Syncer;

use crate::{
    actions::*,
    comments::{ChangeComments, CommentsEvent},
    diff::{
        DiffEvent, DiffSizing, DiffStatus, DiffView, ThreadContext,
        actions::{NextFile, PrevFile},
    },
    diff_load::{LoadError, context_label, load_diff},
    hint::Hint,
    nav::DiffRoute,
    outbox::DraftOutbox,
    reviewed,
    session::{Session, is_unauthorized},
    settings::DiffSettings,
};
use prefs::{next_context, next_tab_size, next_whitespace, whitespace_label};

pub enum DiffPageEvent {
    Unauthorized,
    /// Move to another file of the same comparison.
    OpenFile(DiffRoute),
}

/// What the first background task brings back.
struct Loaded {
    detail: Option<ChangeDetail>,
    files: Option<(FilesKey, Vec<FileEntry>, HashSet<String>)>,
    diff: Option<(DiffKey, Result<model::Diff, LoadError>)>,
    error: Option<String>,
}

pub struct DiffPage {
    session: Session,
    settings: Entity<DiffSettings>,
    route: DiffRoute,
    detail: Option<ChangeDetail>,
    files_key: Option<FilesKey>,
    files: Vec<FileEntry>,
    /// The files of this patch set the user marked reviewed.
    reviewed: HashSet<String>,
    /// A note for the user (why a key did nothing, a failed request).
    hint: Hint,
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
    /// The change's comment threads, shown in the diff.
    comments: Entity<ChangeComments>,
    outbox: Entity<DraftOutbox>,
    /// The thread the route asked the cursor to be on, until it is found.
    want_thread: Option<model::ThreadId>,
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
        route: DiffRoute,
        outbox: Entity<DraftOutbox>,
        cx: &mut Context<Self>,
    ) -> Self {
        let comments = outbox.update(cx, |o, cx| o.comments(route.change, cx));
        let want_thread = route.thread.clone();
        let mut this = Self {
            session,
            settings: settings.clone(),
            route,
            detail: None,
            files_key: None,
            files: Vec::new(),
            reviewed: HashSet::new(),
            hint: Hint::default(),
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
            comments: comments.clone(),
            outbox,
            want_thread,
            _subs: Vec::new(),
        };
        this._subs
            .push(cx.observe(&settings, |this, _, cx| this.settings_changed(cx)));
        this._subs
            .push(cx.subscribe(&comments, |this, _, ev, cx| match ev {
                CommentsEvent::Changed => this.apply_threads(cx),
                CommentsEvent::Unauthorized => cx.emit(DiffPageEvent::Unauthorized),
            }));
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
        let prefs = self.prefs(cx);
        DiffKey {
            change: files.change,
            base: files.base.clone(),
            target: files.target.clone(),
            path: self.route.path.clone(),
            whitespace: prefs.ignore_whitespace,
        }
    }

    /// Everything, from the cache where possible.
    fn load(&mut self, cx: &mut Context<Self>) {
        let syncer = self.session.syncer().clone();
        let can_sync = self.session.can_sync();
        let route = self.route.clone();
        let prefs = self.prefs(cx);
        self.loading = true;
        self.task = Some(cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(load_everything(syncer, can_sync, route, prefs))
                .await;
            this.update(cx, |this, cx| this.loaded(loaded, cx)).ok();
        }));
    }

    fn loaded(&mut self, l: Loaded, cx: &mut Context<Self>) {
        self.detail = l.detail;
        if let Some((key, files, reviewed)) = l.files {
            self.files_key = Some(key);
            self.files = files;
            self.reviewed = reviewed;
        }
        if let Some(e) = l.error {
            self.loading = false;
            self.error = Some(e);
            cx.notify();
            return;
        }
        match l.diff {
            Some((key, result)) => {
                self.diff_arrived(key, result, cx);
                self.fetch(cx); // preferences may have changed meanwhile
            }
            None => {
                self.loading = false;
                self.error = Some("the diff could not be requested".into());
            }
        }
        cx.notify();
    }

    /// Fetches the diff for the current preferences (after the whitespace
    /// option changed). The shown diff stays until the new one is there.
    fn fetch(&mut self, cx: &mut Context<Self>) {
        let Some(files) = self.files_key.clone() else {
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
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(load_diff(syncer, key.clone(), can_sync))
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
        result: Result<model::Diff, LoadError>,
        cx: &mut Context<Self>,
    ) {
        if self.key.as_ref().is_some_and(|k| *k != key) {
            return; // superseded
        }
        self.loading = false;
        match result {
            Ok(diff) => {
                self.error = None;
                self.key = Some(key.clone());
                self.shown = Some(key.clone());
                match &self.view {
                    Some(view) => view.update(cx, |v, cx| {
                        v.set_diff(diff, cx);
                        v.set_status(None, cx);
                    }),
                    None => self.create_view(diff, cx),
                }
                self.prefetch_next(&key, cx);
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

    fn create_view(&mut self, diff: model::Diff, cx: &mut Context<Self>) {
        let (prefs, font) = {
            let s = self.settings.read(cx);
            (s.resolved(), s.font_family())
        };
        let can_comment = self.session.can_comment();
        let view = cx.new(|cx| {
            let mut v = DiffView::new(diff, prefs, cx);
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

    /// Fetches the next file's diff in the background so that `]` is
    /// instant.
    fn prefetch_next(&mut self, shown: &DiffKey, cx: &mut Context<Self>) {
        if !self.session.can_sync() || self.files.is_empty() {
            return;
        }
        let syncer: Syncer = self.session.syncer().clone();
        let files = self.files.clone();
        let key = match &self.files_key {
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
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn is_reviewed_for_test(&self) -> bool {
        self.reviewed.contains(&self.route.path)
    }

    #[cfg(test)]
    pub(crate) fn hint_for_test(&self) -> Option<(String, bool)> {
        self.hint.current().map(|(t, e)| (t.to_owned(), e))
    }

    // ---- reviewed mark ----

    /// `r`: marks the shown file reviewed, or clears the mark. The mark
    /// changes on screen at once; if the request fails it goes back and a
    /// note says why.
    fn toggle_reviewed(&mut self, _: &ToggleReviewed, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(why) = reviewed::unavailable(&self.session) {
            self.hint.say(why, false, cx, |p| &mut p.hint);
            return;
        }
        let Some(key) = self.files_key.clone() else {
            return;
        };
        let path = self.route.path.clone();
        let reviewed = !self.reviewed.contains(&path);
        self.mark_reviewed(&path, reviewed);
        cx.notify();
        let syncer = self.session.syncer().clone();
        let (change, sha, sent) = (self.route.change, key.target.sha.clone(), path.clone());
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
            self.reviewed.insert(path.to_owned());
        } else {
            self.reviewed.remove(path);
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
        if self.files_key.as_ref() == Some(key) && self.reviewed.contains(path) == reviewed {
            self.mark_reviewed(path, !reviewed);
        }
        if let Some(e) = error {
            let note = reviewed::failure_note(path, reviewed, &e);
            self.hint.say(note, true, cx, |p| &mut p.hint);
        }
        if login {
            cx.emit(DiffPageEvent::Unauthorized);
        }
        cx.notify();
    }

    // ---- navigation between files ----

    fn position(&self) -> Option<usize> {
        self.files.iter().position(|f| f.path == self.route.path)
    }

    fn go_to(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(file) = self.files.get(ix) else {
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

    fn next_file(&mut self, _: &NextFile, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(i) = self.position().filter(|i| i + 1 < self.files.len()) {
            self.go_to(i + 1, cx);
        }
    }

    fn prev_file(&mut self, _: &PrevFile, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(i) = self.position().filter(|i| *i > 0) {
            self.go_to(i - 1, cx);
        }
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

    // ---- rendering ----

    fn path_label(&self) -> String {
        let old = self
            .files
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

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let prefs = self.prefs(cx);
        let subject = self
            .detail
            .as_ref()
            .map_or_else(String::new, |d| d.summary.subject.clone());
        let pair = match self.route.base {
            Some(b) => format!("Patchset {b} \u{2192} {}", self.route.target),
            None => format!("Base \u{2192} Patchset {}", self.route.target),
        };
        let position = self
            .position()
            .map(|i| format!("{} of {}", i + 1, self.files.len()));
        let mode = match prefs.view_mode {
            DiffViewMode::SideBySide => "side-by-side",
            DiffViewMode::Unified => "unified",
        };
        // What is on screen, not what was asked for: a failed or pending
        // request must not pass for the current state.
        let key = self.shown.as_ref().or(self.key.as_ref());
        let pending = self.key != self.shown && self.shown.is_some();
        let context = self.prefs(cx).context;
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
                    .child(
                        div()
                            .text_color(muted)
                            .child(format!("#{}", self.route.change)),
                    )
                    .child(div().font_semibold().min_w_0().truncate().child(subject)),
            )
            .child(
                h_flex()
                    .gap_3()
                    .items_baseline()
                    .flex_wrap()
                    .text_sm()
                    .child(
                        div()
                            .font_family(theme.mono_font_family.clone())
                            .child(self.path_label()),
                    )
                    .when(self.reviewed.contains(&self.route.path), |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(theme.success)
                                .child("\u{2713} reviewed"),
                        )
                    })
                    .child(div().text_color(muted).child(pair))
                    .children(position.map(|p| div().text_color(muted).child(p)))
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

async fn load_everything(
    syncer: Syncer,
    can_sync: bool,
    route: DiffRoute,
    prefs: ResolvedDiffPreferences,
) -> Loaded {
    let fail = |detail: Option<ChangeDetail>, error: String| Loaded {
        detail,
        files: None,
        diff: None,
        error: Some(error),
    };
    let mut detail = match syncer.cached_change(route.change) {
        Ok(c) => c.map(|c| c.detail),
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
    if !has_revisions(&detail) && can_sync {
        match syncer.refresh_change(route.change).await {
            Ok(d) => detail = Some(d),
            Err(e) if is_unauthorized(&e) => {
                return fail(detail, "unauthorized".into());
            }
            Err(e) => log::warn!("refreshing change {}: {e}", route.change),
        }
    }
    let Some(d) = detail.clone().filter(|_| has_revisions(&detail)) else {
        return fail(
            detail,
            format!(
                "Change {} with patch set {} is not available{}.",
                route.change,
                route.target,
                if can_sync { "" } else { " offline" }
            ),
        );
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
    let files = match syncer.cached_files(&files_key) {
        Ok(Some(c)) => Some((c.files, c.reviewed)),
        _ if can_sync => syncer
            .refresh_files(&files_key)
            .await
            .map(|c| (c.files, c.reviewed))
            .map_err(|e| log::warn!("fetching files of change {}: {e}", route.change))
            .ok(),
        _ => None,
    };
    let key = DiffKey {
        change: files_key.change,
        base: files_key.base.clone(),
        target: files_key.target.clone(),
        path: route.path.clone(),
        whitespace: prefs.ignore_whitespace,
    };
    let diff = load_diff(syncer, key.clone(), can_sync).await;
    Loaded {
        detail: Some(d),
        files: files.map(|(f, r)| (files_key, f, r.unwrap_or_default().into_iter().collect())),
        diff: Some((key, diff)),
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
            .on_action(cx.listener(Self::toggle_reviewed))
            .relative()
            .size_full()
            .child(self.render_header(cx))
            .child(div().flex_1().min_h_0().child(body))
            .when(self.prefs_open, |d| d.child(self.render_prefs(cx)))
    }
}
