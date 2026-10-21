//! Dashboard and search results: lists of changes, cache first, then
//! refreshed by polling.

use std::time::Duration;

use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use jiff::{Timestamp, tz::TimeZone};
use model::{ChangeNumber, ChangeSummary};
use store::CachedQuery;
use sync::PollPolicy;

use crate::{
    actions::*,
    labels::{label_columns, mark, short_name},
    list_model::{ListModel, PAGE_SIZE, Pager},
    row::{browser_url, chips, size_text},
    session::{SavedQuery, Session, is_unauthorized},
    theme::{chip_colors, mark_color},
    time_fmt::relative,
    tracker::Tracker,
};

/// What the page asks of the shell.
pub enum ListEvent {
    Open(ChangeNumber),
    Unauthorized,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListKind {
    /// One section per saved query.
    Dashboard,
    /// One section, paged.
    Search,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Idle,
    Refreshing,
    LoadingMore,
    Failed(String),
}

impl Status {
    pub fn busy(&self) -> bool {
        matches!(self, Status::Refreshing | Status::LoadingMore)
    }
}

struct Section {
    spec: SavedQuery,
    /// Every cached row of the query, in server order.
    rows: Vec<ChangeSummary>,
    /// The cache has been read (an empty cache counts).
    loaded: bool,
    more_on_server: bool,
    fetched_at: Option<Timestamp>,
    status: Status,
    track: Tracker,
    task: Option<Task<()>>,
}

impl Section {
    fn new(spec: SavedQuery) -> Self {
        Self {
            spec,
            rows: Vec::new(),
            loaded: false,
            more_on_server: false,
            fetched_at: None,
            status: Status::Idle,
            track: Tracker::default(),
            task: None,
        }
    }

    fn apply(&mut self, cached: CachedQuery) {
        self.rows = cached.changes;
        self.more_on_server = cached.more_changes;
        self.fetched_at = Some(cached.fetched_at);
    }
}

// Column widths in pixels. Owner, project and branch shrink first when the
// window is narrow; the subject keeps a minimum.
const W_NUM: f32 = 56.;
const W_SUBJECT_MIN: f32 = 260.;
const W_OWNER: f32 = 120.;
const W_PROJECT: f32 = 140.;
const W_BRANCH: f32 = 84.;
const W_UPDATED: f32 = 72.;
const W_SIZE: f32 = 76.;
const W_LABEL: f32 = 28.;
const W_CMTS: f32 = 36.;

type Fetched = Result<(sync::QueryUpdate, Option<CachedQuery>), sync::Error>;

pub struct ChangeListPage {
    session: Session,
    kind: ListKind,
    sections: Vec<Section>,
    model: ListModel,
    pager: Pager,
    focus: FocusHandle,
    scroll: ScrollHandle,
    window_active: bool,
    policy: PollPolicy,
    /// Select this change once the rows are known (coming back to a page).
    restore: Option<ChangeNumber>,
    reveal: bool,
    ticks: u32,
    _ticker: Task<()>,
}

impl EventEmitter<ListEvent> for ChangeListPage {}

impl Focusable for ChangeListPage {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ChangeListPage {
    pub fn new(
        session: Session,
        kind: ListKind,
        specs: Vec<SavedQuery>,
        restore: Option<ChangeNumber>,
        window_active: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let count = specs.len();
        let mut this = Self {
            session,
            kind,
            sections: specs.into_iter().map(Section::new).collect(),
            model: ListModel::new(count),
            pager: Pager::default(),
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            window_active,
            policy: PollPolicy::default(),
            restore,
            reveal: false,
            ticks: 0,
            _ticker: Task::ready(()),
        };
        for i in 0..count {
            this.load_cache(i, cx);
        }
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
    pub(crate) fn focus_handle_for_test(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn selected_number(&self) -> Option<ChangeNumber> {
        self.model.selected_number()
    }

    /// Rows currently shown in section `i`.
    pub fn visible_rows(&self, i: usize) -> &[ChangeSummary] {
        let Some(s) = self.sections.get(i) else {
            return &[];
        };
        match self.kind {
            ListKind::Dashboard => &s.rows[..s.rows.len().min(PAGE_SIZE)],
            ListKind::Search => &s.rows[self.pager.range(s.rows.len())],
        }
    }

    // ---- data ----

    fn load_cache(&mut self, i: usize, cx: &mut Context<Self>) {
        let syncer = self.session.syncer().clone();
        let query = self.sections[i].spec.query.clone();
        self.sections[i].task = Some(cx.spawn(async move |this, cx| {
            let cached = cx
                .background_spawn(async move { syncer.cached_query(&query) })
                .await;
            this.update(cx, |this, cx| this.cache_loaded(i, cached, cx))
                .ok();
        }));
    }

    fn cache_loaded(
        &mut self,
        i: usize,
        cached: Result<Option<CachedQuery>, sync::Error>,
        cx: &mut Context<Self>,
    ) {
        let s = &mut self.sections[i];
        s.loaded = true;
        match cached {
            Ok(Some(c)) => s.apply(c),
            Ok(None) => {}
            Err(e) => log::warn!("reading cached query {:?}: {e}", s.spec.query),
        }
        self.sync_model();
        self.model.ensure_selection();
        self.tick(cx);
        cx.notify();
    }

    /// Pushes the visible rows into the selection model.
    fn sync_model(&mut self) {
        if self.kind == ListKind::Search
            && let Some(s) = self.sections.first()
        {
            self.pager.clamp(s.rows.len());
        }
        for i in 0..self.sections.len() {
            let numbers = self.visible_rows(i).iter().map(|c| c.number).collect();
            self.model.set_section(i, numbers);
        }
        if let Some(n) = self.restore
            && self.model.select_number(n)
        {
            self.restore = None;
            self.reveal = true;
        }
    }

    fn refresh(&mut self, i: usize, cx: &mut Context<Self>) {
        let syncer = self.session.syncer().clone();
        let query = self.sections[i].spec.query.clone();
        self.sections[i].status = Status::Refreshing;
        self.sections[i].task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let update = syncer.refresh_query(&query, PAGE_SIZE as u32).await?;
                    let cached = if update.changed {
                        syncer.cached_query(&query)?
                    } else {
                        None
                    };
                    Ok::<_, sync::Error>((update, cached))
                })
                .await;
            this.update(cx, |this, cx| this.fetched(i, false, result, cx))
                .ok();
        }));
        cx.notify();
    }

    fn load_more(&mut self, i: usize, cx: &mut Context<Self>) {
        let syncer = self.session.syncer().clone();
        let query = self.sections[i].spec.query.clone();
        self.sections[i].status = Status::LoadingMore;
        self.sections[i].task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let update = syncer.load_more(&query, PAGE_SIZE as u32).await?;
                    let cached = syncer.cached_query(&query)?;
                    Ok::<_, sync::Error>((update, cached))
                })
                .await;
            this.update(cx, |this, cx| this.fetched(i, true, result, cx))
                .ok();
        }));
        cx.notify();
    }

    fn fetched(&mut self, i: usize, was_load_more: bool, result: Fetched, cx: &mut Context<Self>) {
        let now = Timestamp::now();
        let s = &mut self.sections[i];
        match result {
            Ok((update, cached)) => {
                s.track.success(now);
                s.status = Status::Idle;
                let before = s.rows.len();
                match cached {
                    Some(c) => s.apply(c),
                    None => {
                        s.more_on_server = update.more_changes;
                        s.fetched_at = Some(now);
                    }
                }
                if was_load_more && s.rows.len() > before {
                    self.pager.page = before / PAGE_SIZE;
                    self.sync_model();
                    self.model.select_first_in(0);
                    self.reveal = true;
                } else {
                    self.sync_model();
                }
            }
            Err(e) if is_unauthorized(&e) => {
                s.status = Status::Failed("unauthorized".into());
                cx.emit(ListEvent::Unauthorized);
            }
            Err(e) => {
                log::warn!("refreshing {:?}: {e}", s.spec.query);
                s.track.failure(now);
                s.status = Status::Failed(e.to_string());
            }
        }
        cx.notify();
    }

    // ---- polling ----

    /// Starts a refresh for every section the policy says is due.
    fn tick(&mut self, cx: &mut Context<Self>) {
        self.ticks = self.ticks.wrapping_add(1);
        if self.ticks.is_multiple_of(10) {
            cx.notify(); // keeps "updated 3m ago" current
        }
        if !self.session.can_sync() {
            return;
        }
        let now = Timestamp::now();
        for i in 0..self.sections.len() {
            let s = &self.sections[i];
            if s.loaded
                && !s.status.busy()
                && self
                    .policy
                    .is_due(now, &s.track.state(self.window_active, true))
            {
                self.refresh(i, cx);
            }
        }
    }

    /// Called by the shell when the window gains or loses focus. Regaining
    /// focus shortens the polling interval, so overdue sections refresh now.
    pub fn set_window_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.window_active != active {
            self.window_active = active;
            self.tick(cx);
        }
    }

    /// `shift-r`
    fn refresh_now(&mut self, cx: &mut Context<Self>) {
        for s in &mut self.sections {
            s.track.invalidate();
            if !s.status.busy() {
                s.status = Status::Idle;
            }
        }
        self.tick(cx);
    }

    // ---- actions ----

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.model.select_next();
        self.reveal = true;
        cx.notify();
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.model.select_prev();
        self.reveal = true;
        cx.notify();
    }

    fn open_selected(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(n) = self.model.selected_number() {
            cx.emit(ListEvent::Open(n));
        }
    }

    fn open_in_browser(&mut self, _: &OpenInBrowser, _: &mut Window, cx: &mut Context<Self>) {
        let Some(n) = self.model.selected_number() else {
            return;
        };
        let project = (0..self.sections.len())
            .flat_map(|i| self.visible_rows(i))
            .find(|c| c.number == n)
            .map(|c| c.project.clone());
        if let Some(project) = project {
            cx.open_url(&browser_url(&self.session.base_url, &project, n));
        }
    }

    fn next_page(&mut self, _: &NextPage, _: &mut Window, cx: &mut Context<Self>) {
        if self.kind != ListKind::Search {
            return;
        }
        let Some(s) = self.sections.first() else {
            return;
        };
        if self.pager.has_cached_next(s.rows.len()) {
            self.pager.page += 1;
            self.sync_model();
            self.model.select_first_in(0);
            self.reveal = true;
            cx.notify();
        } else if s.more_on_server && !s.status.busy() && self.session.can_sync() {
            self.load_more(0, cx);
        }
    }

    fn prev_page(&mut self, _: &PrevPage, _: &mut Window, cx: &mut Context<Self>) {
        if self.kind == ListKind::Search && self.pager.page > 0 {
            self.pager.page -= 1;
            self.sync_model();
            self.model.select_first_in(0);
            self.reveal = true;
            cx.notify();
        }
    }

    fn refresh_action(&mut self, _: &Refresh, _: &mut Window, cx: &mut Context<Self>) {
        self.refresh_now(cx);
    }

    // ---- rendering ----

    /// Index of a row among the scroll container's children: every section
    /// contributes a header and then its rows (or one placeholder).
    fn child_index(&self, section: usize, row: usize) -> usize {
        let before: usize = (0..section)
            .map(|s| 1 + self.visible_rows(s).len().max(1))
            .sum();
        before + 1 + row
    }

    fn render_header(&self, cols: &[String], cx: &App) -> impl IntoElement {
        let th = |w: f32, t: &str| {
            div()
                .w(px(w))
                .flex_none()
                .truncate()
                .child(SharedString::from(t.to_owned()))
        };
        h_flex()
            .px_3()
            .py_1()
            .gap_2()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(th(W_NUM, "#"))
            .child(div().flex_1().min_w(px(W_SUBJECT_MIN)).child("Subject"))
            .child(th(W_OWNER, "Owner").flex_shrink(1.).min_w_0())
            .child(th(W_PROJECT, "Project").flex_shrink(1.).min_w_0())
            .child(th(W_BRANCH, "Branch").flex_shrink(1.).min_w_0())
            .child(th(W_UPDATED, "Updated"))
            .child(th(W_SIZE, "Size"))
            .children(
                cols.iter()
                    .map(|c| th(W_LABEL, &short_name(c)).text_center()),
            )
            .child(th(W_CMTS, "Cmts").text_center())
    }

    fn render_row(
        &self,
        (section, row): (usize, usize),
        c: &ChangeSummary,
        cols: &[String],
        now: Timestamp,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let number = c.number;
        let (ins, del) = size_text(c);
        let tz = TimeZone::system();
        let cell = |w: f32| div().w(px(w)).flex_none().truncate();
        let shrinking = |w: f32| div().w(px(w)).flex_shrink(1.).min_w_0().truncate();
        let muted = theme.muted_foreground;
        h_flex()
            .id(("change-row", section * 10_000 + row))
            .px_3()
            .py_1()
            .gap_2()
            .text_sm()
            .border_l_2()
            .border_color(if selected {
                theme.list_active_border
            } else {
                theme.transparent
            })
            .when(selected, |d| d.bg(theme.list_active))
            .hover(|d| d.bg(theme.list_hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.model.select_number(number);
                cx.emit(ListEvent::Open(number));
            }))
            .child(cell(W_NUM).text_color(muted).child(c.number.to_string()))
            .child(
                h_flex()
                    .flex_1()
                    .min_w(px(W_SUBJECT_MIN))
                    .gap_1()
                    .overflow_hidden()
                    .children(chips(c).into_iter().map(|chip| {
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
                    .child(div().min_w_0().truncate().child(c.subject.clone())),
            )
            .child(shrinking(W_OWNER).child(c.owner.display()))
            .child(
                shrinking(W_PROJECT)
                    .text_color(muted)
                    .child(c.project.clone()),
            )
            .child(
                shrinking(W_BRANCH)
                    .text_color(muted)
                    .child(c.branch.clone()),
            )
            .child(
                cell(W_UPDATED)
                    .text_color(muted)
                    .child(relative(now, c.updated, &tz)),
            )
            .child(
                h_flex()
                    .w(px(W_SIZE))
                    .flex_none()
                    .gap_1()
                    .text_xs()
                    .child(div().text_color(theme.success).child(ins))
                    .child(div().text_color(theme.danger).child(del)),
            )
            .children(cols.iter().map(|name| {
                let m = c.labels.get(name).map(mark);
                let (text, color) = match &m {
                    Some(m) => (m.text(), mark_color(m, cx)),
                    None => (String::new(), muted),
                };
                div()
                    .w(px(W_LABEL))
                    .flex_none()
                    .text_center()
                    .text_color(color)
                    .child(text)
            }))
            .child(
                div()
                    .w(px(W_CMTS))
                    .flex_none()
                    .text_center()
                    .text_xs()
                    .text_color(theme.warning)
                    .when(c.unresolved_comment_count > 0, |d| {
                        d.child(c.unresolved_comment_count.to_string())
                    }),
            )
    }

    fn render_section_header(
        &self,
        i: usize,
        now: Timestamp,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let s = &self.sections[i];
        let theme = cx.theme().clone();
        let count = if s.loaded {
            let n = s.rows.len().min(if self.kind == ListKind::Dashboard {
                PAGE_SIZE
            } else {
                usize::MAX
            });
            format!("{n}{}", if s.more_on_server { "+" } else { "" })
        } else {
            String::new()
        };
        let status: AnyElement = match &s.status {
            Status::Refreshing => caption_text("refreshing\u{2026}", theme.muted_foreground),
            Status::LoadingMore => caption_text("loading more\u{2026}", theme.muted_foreground),
            Status::Failed(msg) => h_flex()
                .gap_2()
                .child(caption_text(&format!("error: {msg}"), theme.danger))
                .child(
                    Button::new(("retry", i))
                        .label("Retry")
                        .xsmall()
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_now(cx))),
                )
                .into_any_element(),
            Status::Idle => match s.fetched_at {
                Some(t) => caption_text(
                    &format!("updated {}", relative(now, t, &TimeZone::system())),
                    theme.muted_foreground,
                ),
                None => div().into_any_element(),
            },
        };
        h_flex()
            .px_3()
            .pt_3()
            .pb_1()
            .gap_2()
            .child(div().font_semibold().child(s.spec.name.clone()))
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(count),
            )
            .when(self.kind == ListKind::Search, |d| {
                d.child(
                    div()
                        .text_sm()
                        .font_family("monospace")
                        .text_color(theme.muted_foreground)
                        .child(s.spec.query.clone()),
                )
            })
            .child(div().flex_1())
            .child(status)
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.kind != ListKind::Search {
            return None;
        }
        let s = self.sections.first()?;
        let has_next = self.pager.has_next(s.rows.len(), s.more_on_server);
        Some(
            h_flex()
                .px_3()
                .py_1()
                .gap_2()
                .border_t_1()
                .border_color(cx.theme().border)
                .text_sm()
                .child(
                    Button::new("prev-page")
                        .label("Previous (p)")
                        .small()
                        .disabled(self.pager.page == 0)
                        .on_click(cx.listener(|this, _, w, cx| this.prev_page(&PrevPage, w, cx))),
                )
                .child(format!("Page {}", self.pager.page + 1))
                .child(
                    Button::new("next-page")
                        .label(if self.pager.has_cached_next(s.rows.len()) {
                            "Next (n)"
                        } else {
                            "Load more (n)"
                        })
                        .small()
                        .disabled(!has_next || s.status.busy())
                        .on_click(cx.listener(|this, _, w, cx| this.next_page(&NextPage, w, cx))),
                ),
        )
    }
}

fn caption_text(text: &str, color: Hsla) -> AnyElement {
    div()
        .text_xs()
        .text_color(color)
        .child(text.to_owned())
        .into_any_element()
}

impl Render for ChangeListPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.reveal {
            self.reveal = false;
            if let Some((s, r)) = self.model.selected() {
                self.scroll.scroll_to_item(self.child_index(s, r));
            }
        }
        let now = Timestamp::now();
        let cols = label_columns((0..self.sections.len()).flat_map(|i| self.visible_rows(i)));
        let selected = self.model.selected();

        let mut body = v_flex()
            .id("change-list-scroll")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll);
        for i in 0..self.sections.len() {
            body = body.child(self.render_section_header(i, now, cx));
            let rows: Vec<ChangeSummary> = self.visible_rows(i).to_vec();
            if rows.is_empty() {
                let s = &self.sections[i];
                let text = if s.loaded
                    && (s.fetched_at.is_some() || matches!(s.status, Status::Failed(_)))
                {
                    "No changes"
                } else {
                    "Loading\u{2026}"
                };
                body = body.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(text),
                );
            }
            for (r, c) in rows.iter().enumerate() {
                body = body.child(self.render_row(
                    (i, r),
                    c,
                    &cols,
                    now,
                    selected == Some((i, r)),
                    cx,
                ));
            }
        }

        v_flex()
            .id("change-list")
            .key_context(CHANGE_LIST)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::open_in_browser))
            .on_action(cx.listener(Self::next_page))
            .on_action(cx.listener(Self::prev_page))
            .on_action(cx.listener(Self::refresh_action))
            .size_full()
            .child(self.render_header(&cols, cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(body)
                    .vertical_scrollbar(&self.scroll),
            )
            .children(self.render_footer(cx))
    }
}
