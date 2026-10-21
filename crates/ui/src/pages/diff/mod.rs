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
//! Skip expansion: `Expand::All` re-requests with `context=ALL`;
//! `Expand::Skip` with just enough more context to reveal that run (see
//! [`crate::diff_load::expanded_context`]). The new diff goes in through
//! `DiffView::set_diff`, which keeps the cursor on its line.
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
use model::{
    ChangeDetail, DiffContext, DiffKey, DiffViewMode, FileEntry, FilesKey, ResolvedDiffPreferences,
};
use sync::Syncer;

use crate::{
    actions::*,
    diff::{
        DiffEvent, DiffSizing, DiffView, Expand,
        actions::{NextFile, PrevFile},
    },
    diff_load::{LoadError, expanded_context, load_diff},
    nav::DiffRoute,
    session::{Session, is_unauthorized},
    settings::DiffSettings,
};
use prefs::{context_label, next_context, next_tab_size, next_whitespace, whitespace_label};

pub enum DiffPageEvent {
    Unauthorized,
    /// Move to another file of the same comparison.
    OpenFile(DiffRoute),
}

/// What the first background task brings back.
struct Loaded {
    detail: Option<ChangeDetail>,
    files: Option<(FilesKey, Vec<FileEntry>)>,
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
    /// The key of the diff shown or being fetched.
    key: Option<DiffKey>,
    /// Set by an expansion request: replaces the preferred context.
    ctx_override: Option<DiffContext>,
    view: Option<Entity<DiffView>>,
    view_sub: Option<Subscription>,
    loading: bool,
    error: Option<String>,
    prefs_open: bool,
    focus: FocusHandle,
    prefs_focus: FocusHandle,
    task: Option<Task<()>>,
    prefetch: Option<Task<()>>,
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
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            session,
            settings: settings.clone(),
            route,
            detail: None,
            files_key: None,
            files: Vec::new(),
            key: None,
            ctx_override: None,
            view: None,
            view_sub: None,
            loading: true,
            error: None,
            prefs_open: false,
            focus: cx.focus_handle(),
            prefs_focus: cx.focus_handle(),
            task: None,
            prefetch: None,
            _subs: Vec::new(),
        };
        this._subs
            .push(cx.observe(&settings, |this, _, cx| this.settings_changed(cx)));
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

    fn make_key(&self, files: &FilesKey, ctx: Option<DiffContext>, cx: &App) -> DiffKey {
        let prefs = self.prefs(cx);
        DiffKey {
            change: files.change,
            base: files.base.clone(),
            target: files.target.clone(),
            path: self.route.path.clone(),
            whitespace: prefs.ignore_whitespace,
            context: ctx.unwrap_or(prefs.context),
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
        if let Some((key, files)) = l.files {
            self.files_key = Some(key);
            self.files = files;
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

    /// Fetches the diff for the current preferences (after they or the
    /// context changed). The shown diff stays until the new one is there.
    fn fetch(&mut self, cx: &mut Context<Self>) {
        let Some(files) = self.files_key.clone() else {
            return;
        };
        let key = self.make_key(&files, self.ctx_override, cx);
        if self.key.as_ref() == Some(&key) {
            return;
        }
        self.key = Some(key.clone());
        let syncer = self.session.syncer().clone();
        let can_sync = self.session.can_sync();
        self.loading = self.view.is_none();
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
                match &self.view {
                    Some(view) => view.update(cx, |v, cx| v.set_diff(diff, cx)),
                    None => self.create_view(diff, cx),
                }
                self.prefetch_next(&key, cx);
            }
            Err(LoadError::Unauthorized) => cx.emit(DiffPageEvent::Unauthorized),
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn create_view(&mut self, diff: model::Diff, cx: &mut Context<Self>) {
        let (prefs, font) = {
            let s = self.settings.read(cx);
            (s.resolved(), s.font_family())
        };
        let view = cx.new(|cx| {
            let mut v = DiffView::new(diff, prefs, cx);
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
            DiffEvent::ExpandRequested(expand) => this.expand(expand, cx),
        }));
        self.view = Some(view);
    }

    /// Fetches the next file's diff in the background so that `]` is
    /// instant. Uses the preferred context, not an expansion of this file.
    fn prefetch_next(&mut self, shown: &DiffKey, cx: &mut Context<Self>) {
        if !self.session.can_sync() || self.files.is_empty() {
            return;
        }
        let syncer: Syncer = self.session.syncer().clone();
        let files = self.files.clone();
        let key = match &self.files_key {
            Some(fk) => self.make_key(fk, None, cx),
            None => shown.clone(),
        };
        self.prefetch = Some(cx.background_spawn(async move {
            if let Err(e) = syncer.prefetch_next_diff(&key, &files).await {
                log::debug!("prefetching the next diff failed: {e}");
            }
        }));
    }

    fn expand(&mut self, expand: &Expand, cx: &mut Context<Self>) {
        let Some(current) = self.key.as_ref().map(|k| k.context) else {
            return;
        };
        self.ctx_override = Some(expanded_context(current, expand));
        self.fetch(cx);
    }

    /// Preferences changed (here or by another page): restyle, and refetch
    /// if whitespace or context changed.
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
        // A changed context starts over from the preference, not from an
        // expansion of this file.
        self.ctx_override = None;
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
        let key = self.key.as_ref();
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
                    .child(div().text_color(muted).child(pair))
                    .children(position.map(|p| div().text_color(muted).child(p)))
                    .child(div().flex_1())
                    .when_some(key, |d, k| {
                        d.child(div().text_xs().text_color(muted).child(format!(
                            "context: {}  whitespace: {}",
                            context_label(k.context),
                            whitespace_label(&k.whitespace)
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
        Ok(Some(c)) => Some(c.files),
        _ if can_sync => syncer
            .refresh_files(&files_key)
            .await
            .map(|c| c.files)
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
        context: prefs.context,
    };
    let diff = load_diff(syncer, key.clone(), can_sync).await;
    Loaded {
        detail: Some(d),
        files: files.map(|f| (files_key, f)),
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
            .relative()
            .size_full()
            .child(self.render_header(cx))
            .child(div().flex_1().min_h_0().child(body))
            .when(self.prefs_open, |d| d.child(self.render_prefs(cx)))
    }
}
