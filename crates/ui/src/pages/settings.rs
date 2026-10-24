//! The settings page: the saved queries of the dashboard, the startup page,
//! the review flow (advance after a vote), the theme, the light/dark mode of
//! the UI and of the diff view, the file filters and the servers.
//! The page is split into tabs ([`SettingsTab`]) and shows one at a time.
//! The cursor is a row index over all of the sections: the query rows, then
//! the bookmark rows, the startup row, the advance row, the theme row and the
//! two mode rows, then the filter rows (one empty-state row when there are
//! none), then the server rows (likewise). Each tab owns a range of those
//! rows ([`SettingsPage::tab_rows`]) and the cursor stays inside it. The
//! queries and bookmarks are those of the active server; the servers section
//! is in `servers`.
//!
//! The filters are edited in the Manage filters dialog, which the shell
//! opens when the page emits [`SettingsEvent::ManageFilters`]; the page only
//! lists the rules and follows [`FileFilters`] when the dialog saves.
//!
//! Every committed change is written straight away through the backend
//! (the config file), off the main thread. The list shown is the one the
//! backend returns, so what is on screen is what is on disk.

use std::sync::Arc;

use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

pub(crate) mod servers;

use servers::ServerForm;
pub(crate) use servers::ServerRow;

use crate::{
    actions::*,
    card,
    filters::{FileFilter, FilterAction},
    hint::Hint,
    login::LoginOutcome,
    servers::Connection,
    session::{BackendError, Env, SavedQuery, ServerBackend, ServerConfig, ServerKey, StartupPage},
    settings::{FileFilters, PageLayout},
    theme::{self as app_theme, ColorMode, ColorModes, ThemeName},
};

/// What the page tells the shell.
pub enum SettingsEvent {
    /// The saved queries were written; this is the list now in effect.
    QueriesSaved(Vec<SavedQuery>),
    /// The bookmarks were written; this is the list now in effect.
    BookmarksSaved(Vec<SavedQuery>),
    /// The startup page was written.
    StartupSaved(StartupPage),
    /// Open the Manage filters dialog: on the rule `select`, or on a new
    /// rule when `add` is set.
    ManageFilters { select: Option<String>, add: bool },
    /// Open the login page to add a server.
    AddServer,
    /// The server list was written (reordered, renamed, recolored, or a
    /// URL or user changed): the list now in effect. `relogin` is the login
    /// that went with a changed URL or user, whose session replaces the old
    /// one.
    ServersSaved {
        servers: Vec<ServerConfig>,
        relogin: Option<Box<LoginOutcome>>,
    },
    /// Server `key` was removed with its cache and password.
    ServerRemoved { key: ServerKey },
    /// A connection test of server `key` reached a verdict: the server
    /// answered (`Online`), could not be reached (`Offline`) or refused the
    /// saved credentials (`Unauthorized`).
    ConnectionTested {
        key: ServerKey,
        connection: Connection,
    },
}

/// The tabs of the page, in the order they are shown. To add one: a variant
/// here (in `all` and `title`), its rows in [`SettingsPage::tab_rows`] and
/// its sections in [`SettingsPage::render_tab`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SettingsTab {
    Servers,
    /// The saved queries, the bookmarks and the startup page; the tab the
    /// page opens on the first time.
    #[default]
    Queries,
    Review,
    Appearance,
    Filters,
}

impl SettingsTab {
    pub const fn all() -> [SettingsTab; 5] {
        [
            SettingsTab::Servers,
            SettingsTab::Queries,
            SettingsTab::Review,
            SettingsTab::Appearance,
            SettingsTab::Filters,
        ]
    }

    pub fn title(self) -> &'static str {
        match self {
            SettingsTab::Servers => "Servers",
            SettingsTab::Queries => "Queries",
            SettingsTab::Review => "Review",
            SettingsTab::Appearance => "Appearance",
            SettingsTab::Filters => "Filters",
        }
    }

    /// Where the tab is in the strip.
    fn position(self) -> usize {
        Self::all().iter().position(|t| *t == self).unwrap_or(0)
    }
}

/// The two lists of name and query pairs the page edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Which {
    Queries,
    Bookmarks,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Browsing,
    /// Editing item `index` of a list; `index == len` is a new item being
    /// added.
    Editing {
        which: Which,
        index: usize,
    },
    /// Asking whether to delete item `index` of a list.
    Confirming(Which, usize),
    /// Typing the query of the startup page.
    EditingStartup,
    /// Editing server `index` in the form.
    EditingServer(usize),
    /// Asking whether to remove server `index`.
    ConfirmingServer(usize),
}

// ---- pure list logic ----

/// The query for the typed name and text, or why it cannot be saved.
pub(crate) fn validate(name: &str, query: &str) -> Result<SavedQuery, &'static str> {
    let (name, query) = (name.trim(), query.trim());
    if name.is_empty() {
        Err("Give the query a name.")
    } else if query.is_empty() {
        Err("Enter the query text.")
    } else {
        Ok(SavedQuery {
            name: name.to_owned(),
            query: query.to_owned(),
        })
    }
}

/// `list` with row `index` moved by `delta` places, and where it ended up;
/// `None` at the ends of the list.
pub(crate) fn moved<T: Clone>(list: &[T], index: usize, delta: isize) -> Option<(Vec<T>, usize)> {
    let to = index
        .checked_add_signed(delta)
        .filter(|t| *t < list.len())?;
    if index >= list.len() {
        return None;
    }
    let mut out = list.to_vec();
    let item = out.remove(index);
    out.insert(to, item);
    Some((out, to))
}

/// `list` with row `index` replaced by `q`, or `q` appended when `index` is
/// past the end.
pub(crate) fn with_query<T: Clone>(list: &[T], index: usize, q: T) -> Vec<T> {
    let mut out = list.to_vec();
    match out.get_mut(index) {
        Some(slot) => *slot = q,
        None => out.push(q),
    }
    out
}

/// `list` without row `index`.
pub(crate) fn without<T: Clone>(list: &[T], index: usize) -> Vec<T> {
    let mut out = list.to_vec();
    if index < out.len() {
        out.remove(index);
    }
    out
}

fn rule_name(rule: &FileFilter) -> String {
    rule.name.clone()
}

/// Where a rule applies, for its row: its projects, and its servers if it
/// is limited to some. `servers` names the configured ones.
fn scope_text(rule: &FileFilter, servers: &[ServerRow]) -> String {
    let projects = if rule.projects.is_empty() {
        "all projects".to_owned()
    } else {
        rule.projects.join(", ")
    };
    if rule.servers.is_empty() {
        return projects;
    }
    let names: Vec<String> = rule
        .servers
        .iter()
        .map(|k| {
            servers
                .iter()
                .find(|s| s.key == *k)
                .map_or_else(|| k.to_string(), |s| s.name.clone())
        })
        .collect();
    format!("{projects} on {}", names.join(", "))
}

pub struct SettingsPage {
    env: Env,
    /// The backend of the server whose queries and bookmarks are edited;
    /// `None` when no server is set up.
    server: Option<Arc<dyn ServerBackend>>,
    queries: Vec<SavedQuery>,
    bookmarks: Vec<SavedQuery>,
    /// The tab on show; the cursor is always on one of its rows.
    tab: SettingsTab,
    cursor: usize,
    mode: Mode,
    /// The configured servers, in config order, and the one whose queries
    /// and bookmarks the page edits.
    servers: Vec<ServerRow>,
    active: Option<ServerKey>,
    form: ServerForm,
    tests: std::collections::HashMap<ServerKey, servers::TestState>,
    name: Entity<InputState>,
    query: Entity<InputState>,
    startup_input: Entity<InputState>,
    start_page: StartupPage,
    layout: Entity<PageLayout>,
    filters: Entity<FileFilters>,
    /// The names of the rules on screen, to keep the cursor on its rule when
    /// the list changes under it.
    filter_names: Vec<String>,
    /// Why the row being edited cannot be saved.
    invalid: Option<&'static str>,
    /// A save is in flight: keys that change the list wait.
    busy: bool,
    hint: Hint,
    focus: FocusHandle,
    scroll: ScrollHandle,
    /// Where each tab was scrolled to when it was left.
    scrolled: std::collections::HashMap<SettingsTab, Point<Pixels>>,
    task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<SettingsEvent> for SettingsPage {}

impl Focusable for SettingsPage {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl SettingsPage {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        env: Env,
        server: Option<Arc<dyn ServerBackend>>,
        servers: Vec<ServerRow>,
        active: Option<ServerKey>,
        queries: Vec<SavedQuery>,
        bookmarks: Vec<SavedQuery>,
        start_page: StartupPage,
        layout: Entity<PageLayout>,
        filters: Entity<FileFilters>,
        tab: SettingsTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Name"));
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Query, e.g. is:open"));
        let startup_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Query, e.g. is:open owner:self"));
        let form = ServerForm::new(window, cx);
        let mut subs: Vec<Subscription> = [&name, &query, &startup_input]
            .into_iter()
            .chain(form.inputs())
            .map(|input| {
                cx.subscribe_in(input, window, |this, _, ev: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        this.commit_edit(window, cx);
                    }
                })
            })
            .collect();
        // The dialog saves into the shared rules; the section follows.
        subs.push(cx.observe(&filters, |this, filters, cx| {
            this.follow_filters(&filters, cx)
        }));
        let filter_names = filters.read(cx).rules().iter().map(rule_name).collect();
        let mut page = Self {
            env,
            server,
            servers,
            active,
            form,
            tests: Default::default(),
            queries,
            bookmarks,
            tab,
            cursor: 0,
            mode: Mode::Browsing,
            name,
            query,
            startup_input,
            start_page,
            layout,
            filters,
            filter_names,
            invalid: None,
            busy: false,
            hint: Hint::default(),
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            scrolled: Default::default(),
            task: None,
            _subs: subs,
        };
        page.cursor = *page.tab_rows(tab).start();
        page
    }

    #[cfg(test)]
    pub(crate) fn queries_for_test(&self) -> &[SavedQuery] {
        &self.queries
    }

    #[cfg(test)]
    pub(crate) fn bookmarks_for_test(&self) -> &[SavedQuery] {
        &self.bookmarks
    }

    #[cfg(test)]
    pub(crate) fn inputs_for_test(&self) -> (Entity<InputState>, Entity<InputState>) {
        (self.name.clone(), self.query.clone())
    }

    #[cfg(test)]
    pub(crate) fn startup_input_for_test(&self) -> Entity<InputState> {
        self.startup_input.clone()
    }

    #[cfg(test)]
    pub(crate) fn start_page_for_test(&self) -> &StartupPage {
        &self.start_page
    }

    #[cfg(test)]
    pub(crate) fn cursor_for_test(&self) -> usize {
        self.cursor
    }

    /// The names of the rules listed, in order.
    #[cfg(test)]
    pub(crate) fn filter_names_for_test(&self) -> &[String] {
        &self.filter_names
    }

    #[cfg(test)]
    pub(crate) fn tab_for_test(&self) -> SettingsTab {
        self.tab
    }

    /// Shows `tab` as a key or click would.
    #[cfg(test)]
    pub(crate) fn show_tab_for_test(&mut self, tab: SettingsTab, cx: &mut Context<Self>) {
        self.show_tab(tab, cx);
    }

    #[cfg(test)]
    pub(crate) fn hint_for_test(&self) -> Option<(String, bool)> {
        self.hint.current().map(|(t, e)| (t.to_owned(), e))
    }

    #[cfg(test)]
    pub(crate) fn editing_for_test(&self) -> bool {
        matches!(
            self.mode,
            Mode::Editing { .. } | Mode::EditingStartup | Mode::EditingServer(_)
        )
    }

    /// The rows of the bookmarks: one per bookmark, or one empty-state row
    /// the cursor can be on to add the first.
    fn bookmark_rows(&self) -> usize {
        self.bookmarks.len().max(1)
    }

    /// Cursor positions after the query and bookmark rows.
    fn startup_row(&self) -> usize {
        self.queries.len() + self.bookmark_rows()
    }

    fn list(&self, which: Which) -> &Vec<SavedQuery> {
        match which {
            Which::Queries => &self.queries,
            Which::Bookmarks => &self.bookmarks,
        }
    }

    /// The cursor row of item `index` of a list.
    fn row_of(&self, which: Which, index: usize) -> usize {
        match which {
            Which::Queries => index,
            Which::Bookmarks => self.queries.len() + index,
        }
    }

    /// The list the cursor is in and the item it is on. For the empty
    /// bookmarks row that is item 0 of an empty list, a new item.
    fn item_under_cursor(&self) -> Option<(Which, usize)> {
        if self.cursor < self.queries.len() {
            Some((Which::Queries, self.cursor))
        } else if self.cursor < self.startup_row() {
            Some((Which::Bookmarks, self.cursor - self.queries.len()))
        } else {
            None
        }
    }

    fn advance_row(&self) -> usize {
        self.startup_row() + 1
    }

    fn theme_row(&self) -> usize {
        self.startup_row() + 2
    }

    fn mode_row(&self) -> usize {
        self.startup_row() + 3
    }

    fn diff_mode_row(&self) -> usize {
        self.startup_row() + 4
    }

    /// The first filter row (the empty-state row when there are no rules).
    fn filters_row(&self) -> usize {
        self.startup_row() + 5
    }

    /// Switches the mode of the row under the cursor to the next one.
    fn cycle_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let modes = app_theme::color_modes(cx);
        if self.cursor == self.mode_row() {
            self.save_modes(
                ColorModes {
                    ui: modes.ui.next(),
                    ..modes
                },
                window,
                cx,
            );
        } else {
            self.save_modes(
                ColorModes {
                    diff: modes.diff.next(),
                    ..modes
                },
                window,
                cx,
            );
        }
    }

    /// The first server row (the empty-state row when there are none).
    fn servers_row(&self) -> usize {
        self.filters_row() + self.filter_names.len().max(1)
    }

    /// The last row the cursor can be on.
    fn last_row(&self) -> usize {
        self.servers_row() + self.servers.len().saturating_sub(1)
    }

    /// The cursor rows of `tab`. Never empty: a list without items has one
    /// empty-state row.
    fn tab_rows(&self, tab: SettingsTab) -> std::ops::RangeInclusive<usize> {
        match tab {
            SettingsTab::Servers => self.servers_row()..=self.last_row(),
            SettingsTab::Queries => 0..=self.startup_row(),
            SettingsTab::Review => self.advance_row()..=self.advance_row(),
            SettingsTab::Appearance => self.theme_row()..=self.diff_mode_row(),
            SettingsTab::Filters => self.filters_row()..=self.servers_row() - 1,
        }
    }

    /// `row` moved into the rows of the tab on show.
    fn in_tab(&self, row: usize) -> usize {
        let rows = self.tab_rows(self.tab);
        row.clamp(*rows.start(), *rows.end())
    }

    /// The tab that has cursor row `row`.
    #[cfg(test)]
    fn tab_of(&self, row: usize) -> SettingsTab {
        SettingsTab::all()
            .into_iter()
            .find(|t| self.tab_rows(*t).contains(&row))
            .unwrap_or_default()
    }

    /// Shows `tab`, with the cursor on its first row and the scroll position
    /// it had when it was left. Keys and clicks that would leave a pending
    /// edit, question or save behind are ignored.
    fn show_tab(&mut self, tab: SettingsTab, cx: &mut Context<Self>) {
        if tab == self.tab || self.mode != Mode::Browsing || !self.can_change() {
            return;
        }
        self.scrolled.insert(self.tab, self.scroll.offset());
        self.tab = tab;
        self.cursor = *self.tab_rows(tab).start();
        let offset = self.scrolled.get(&tab).copied().unwrap_or_default();
        self.scroll.set_offset(offset);
        cx.notify();
    }

    /// The rule the cursor is on, if it is on one.
    fn filter_under_cursor(&self) -> Option<usize> {
        self.cursor
            .checked_sub(self.filters_row())
            .filter(|i| *i < self.filter_names.len())
    }

    fn in_filters(&self) -> bool {
        (self.filters_row()..self.servers_row()).contains(&self.cursor)
    }

    fn in_servers(&self) -> bool {
        self.cursor >= self.servers_row()
    }

    /// The server the cursor is on, if it is on one.
    fn server_under_cursor(&self) -> Option<usize> {
        self.cursor
            .checked_sub(self.servers_row())
            .filter(|i| *i < self.servers.len())
    }

    /// Whether a key may change the list now.
    fn can_change(&self) -> bool {
        !self.busy
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.cursor + 1, cx);
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.cursor.saturating_sub(1), cx);
    }

    fn select(&mut self, to: usize, cx: &mut Context<Self>) {
        if self.mode == Mode::Browsing {
            self.cursor = self.in_tab(to);
            cx.notify();
        }
    }

    // ---- editing ----

    fn edit_query(&mut self, _: &EditQuery, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_change() {
            return;
        }
        if self.in_filters() {
            self.manage_filters(self.filter_under_cursor(), self.filter_names.is_empty(), cx);
        } else if self.in_servers() {
            if let Some(i) = self.server_under_cursor() {
                self.start_server_edit(i, window, cx);
            } else {
                cx.emit(SettingsEvent::AddServer);
            }
        } else if let Some((which, index)) = self.item_under_cursor() {
            self.start_edit(which, index, window, cx);
        } else if self.cursor == self.startup_row() {
            self.start_startup_edit(window, cx);
        } else if self.cursor == self.theme_row() {
            self.save_theme(app_theme::current(cx).next(), window, cx);
        } else if self.cursor == self.mode_row() || self.cursor == self.diff_mode_row() {
            self.cycle_mode(window, cx);
        } else {
            self.save_advance(!self.layout.read(cx).advance_after_vote(), window, cx);
        }
    }

    /// `space`: flip the setting under the cursor.
    fn toggle_setting(&mut self, _: &ToggleSetting, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_change() || self.mode != Mode::Browsing {
            return;
        }
        if self.cursor == self.startup_row() {
            match self.start_page {
                StartupPage::Dashboard => self.start_startup_edit(window, cx),
                StartupPage::Search(_) => self.save_startup(StartupPage::Dashboard, window, cx),
            }
        } else if self.cursor == self.advance_row() {
            self.save_advance(!self.layout.read(cx).advance_after_vote(), window, cx);
        } else if self.cursor == self.theme_row() {
            self.save_theme(app_theme::current(cx).next(), window, cx);
        } else if self.cursor == self.mode_row() || self.cursor == self.diff_mode_row() {
            self.cycle_mode(window, cx);
        }
    }

    fn start_startup_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = match &self.start_page {
            StartupPage::Search(q) => q.clone(),
            StartupPage::Dashboard => String::new(),
        };
        self.startup_input
            .update(cx, |s, cx| s.set_value(text, window, cx));
        self.cursor = self.startup_row();
        self.mode = Mode::EditingStartup;
        self.invalid = None;
        self.startup_input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn add_query(&mut self, _: &AddQuery, window: &mut Window, cx: &mut Context<Self>) {
        if self.in_filters() {
            self.manage_filters(None, true, cx);
        } else if self.in_servers() {
            if self.mode == Mode::Browsing && self.can_change() {
                cx.emit(SettingsEvent::AddServer);
            }
        } else if self.tab == SettingsTab::Queries && self.can_change() {
            let which = self.item_under_cursor().map_or(Which::Queries, |(w, _)| w);
            self.start_edit(which, self.list(which).len(), window, cx);
        }
    }

    // ---- file filters ----

    /// Asks the shell for the dialog, on rule `index` or on a new rule.
    fn manage_filters(&mut self, index: Option<usize>, add: bool, cx: &mut Context<Self>) {
        if self.mode != Mode::Browsing {
            return;
        }
        cx.emit(SettingsEvent::ManageFilters {
            select: index.and_then(|i| self.filter_names.get(i).cloned()),
            add,
        });
    }

    /// The rules changed (the dialog saved): list them again and keep the
    /// cursor on its rule, or clamp it when that rule is gone.
    fn follow_filters(&mut self, filters: &Entity<FileFilters>, cx: &mut Context<Self>) {
        let names: Vec<String> = filters.read(cx).rules().iter().map(rule_name).collect();
        // The server rows come after the filters and move with them.
        let in_servers_at = self.in_servers().then(|| self.cursor - self.servers_row());
        if let Some(old) = self.filter_under_cursor()
            && let Some(at) = names.iter().position(|n| *n == self.filter_names[old])
        {
            self.cursor = self.filters_row() + at;
        }
        self.filter_names = names;
        if let Some(at) = in_servers_at {
            self.cursor = self.servers_row() + at;
        }
        self.cursor = self.in_tab(self.cursor);
        cx.notify();
    }

    fn start_edit(
        &mut self,
        which: Which,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (name, query) = match self.list(which).get(index) {
            Some(q) => (q.name.clone(), q.query.clone()),
            None => (String::new(), String::new()),
        };
        self.name.update(cx, |s, cx| s.set_value(name, window, cx));
        self.query
            .update(cx, |s, cx| s.set_value(query, window, cx));
        self.cursor = self.row_of(which, index);
        self.mode = Mode::Editing { which, index };
        self.invalid = None;
        self.name.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn cancel_edit(&mut self, _: &CancelEdit, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(
            self.mode,
            Mode::Editing { .. } | Mode::EditingStartup | Mode::EditingServer(_)
        ) && !self.busy
        {
            let new_row = match self.mode {
                Mode::Editing { which, index } if index >= self.list(which).len() => Some(which),
                _ => None,
            };
            self.leave_edit(window, cx);
            if let Some(which) = new_row {
                // Back to the last item (the "add" row is gone).
                let last = self.list(which).len().saturating_sub(1);
                self.cursor = self.row_of(which, last);
            }
        }
    }

    fn leave_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = Mode::Browsing;
        self.invalid = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Tab / Shift-Tab between the two fields.
    fn other_field(&mut self, _: &NextField, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.mode, Mode::EditingServer(_)) {
            return self.next_server_field(window, cx);
        }
        if !matches!(self.mode, Mode::Editing { .. }) {
            return;
        }
        let on_name = self.name.focus_handle(cx).is_focused(window);
        let target = if on_name { &self.query } else { &self.name };
        target.update(cx, |s, cx| s.focus(window, cx));
    }

    fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if let Mode::EditingServer(i) = self.mode {
            return self.commit_server_edit(i, window, cx);
        }
        if self.mode == Mode::EditingStartup {
            let text = self.startup_input.read(cx).value().trim().to_owned();
            if text.is_empty() {
                self.invalid = Some("Enter the query text.");
                cx.notify();
            } else {
                self.invalid = None;
                self.save_startup(StartupPage::Search(text), window, cx);
            }
            return;
        }
        let Mode::Editing { which, index } = self.mode else {
            return;
        };
        let name = self.name.read(cx).value().to_string();
        let text = self.query.read(cx).value().to_string();
        match validate(&name, &text) {
            Ok(q) => {
                self.invalid = None;
                let list = with_query(self.list(which), index, q);
                self.save(which, list, index, window, cx);
            }
            Err(why) => {
                self.invalid = Some(why);
                cx.notify();
            }
        }
    }

    // ---- delete ----

    fn delete_query(&mut self, _: &DeleteQuery, _: &mut Window, cx: &mut Context<Self>) {
        // The dialog asks before it deletes a rule; the page has no path of
        // its own for that.
        if let Some(i) = self.filter_under_cursor() {
            self.manage_filters(Some(i), false, cx);
        } else if self.in_servers() {
            if self.can_change()
                && self.mode == Mode::Browsing
                && let Some(i) = self.server_under_cursor()
            {
                self.mode = Mode::ConfirmingServer(i);
                cx.notify();
            }
        } else if self.can_change()
            && self.mode == Mode::Browsing
            && let Some((which, index)) = self.item_under_cursor()
            && index < self.list(which).len()
        {
            self.mode = Mode::Confirming(which, index);
            cx.notify();
        }
    }

    fn confirm_delete(&mut self, _: &ConfirmDelete, window: &mut Window, cx: &mut Context<Self>) {
        if let Mode::ConfirmingServer(i) = self.mode
            && self.can_change()
        {
            return self.remove_server(i, window, cx);
        }
        if let Mode::Confirming(which, i) = self.mode
            && self.can_change()
        {
            let list = without(self.list(which), i);
            self.save(which, list, i, window, cx);
        }
    }

    fn cancel_delete(&mut self, _: &CancelDelete, _: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.mode, Mode::Confirming(..) | Mode::ConfirmingServer(_)) && !self.busy {
            self.mode = Mode::Browsing;
            cx.notify();
        }
    }

    // ---- reorder ----

    fn move_down(&mut self, _: &MoveQueryDown, window: &mut Window, cx: &mut Context<Self>) {
        self.move_by(1, window, cx);
    }

    fn move_up(&mut self, _: &MoveQueryUp, window: &mut Window, cx: &mut Context<Self>) {
        self.move_by(-1, window, cx);
    }

    fn move_by(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_change() || self.mode != Mode::Browsing {
            return;
        }
        if self.in_servers() {
            return self.move_server(delta, window, cx);
        }
        if let Some((which, index)) = self.item_under_cursor()
            && let Some((list, to)) = moved(self.list(which), index, delta)
        {
            self.save(which, list, to, window, cx);
        }
    }

    // ---- saving ----

    /// Writes `list` in the background. On success it becomes the list on
    /// screen, the cursor goes to its item `cursor` and the shell is told;
    /// on failure nothing changes but a hint.
    fn save(
        &mut self,
        which: Which,
        list: Vec<SavedQuery>,
        cursor: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy = true;
        let backend = self.server.clone();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let Some(backend) = backend else {
                        return Err(BackendError("no server is set up".into()));
                    };
                    match which {
                        Which::Queries => backend.save_queries(&list),
                        Which::Bookmarks => backend.save_bookmarks(&list),
                    }
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(saved) => {
                        let at = cursor.min(saved.len().saturating_sub(1));
                        match which {
                            Which::Queries => this.queries = saved.clone(),
                            Which::Bookmarks => this.bookmarks = saved.clone(),
                        }
                        this.cursor = this.row_of(which, at);
                        this.leave_edit(window, cx);
                        cx.emit(match which {
                            Which::Queries => SettingsEvent::QueriesSaved(saved),
                            Which::Bookmarks => SettingsEvent::BookmarksSaved(saved),
                        });
                    }
                    Err(e) => {
                        if matches!(this.mode, Mode::Confirming(..)) {
                            this.mode = Mode::Browsing;
                        }
                        let what = match which {
                            Which::Queries => "queries",
                            Which::Bookmarks => "bookmarks",
                        };
                        this.hint
                            .say(format!("Could not save the {what}: {e}"), true, cx, |p| {
                                &mut p.hint
                            });
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Writes the startup page in the background; on failure the old one
    /// stays and a hint says why.
    fn save_startup(&mut self, page: StartupPage, window: &mut Window, cx: &mut Context<Self>) {
        self.busy = true;
        let backend = self.env.backend.clone();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { backend.save_startup_page(&page) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(saved) => {
                        this.start_page = saved.clone();
                        this.leave_edit(window, cx);
                        cx.emit(SettingsEvent::StartupSaved(saved));
                    }
                    Err(e) => {
                        this.hint.say(
                            format!("Could not save the startup page: {e}"),
                            true,
                            cx,
                            |p| &mut p.hint,
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Writes `advance_after_vote`; on success the shared layout follows at
    /// once, so the next vote uses it.
    fn save_advance(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.busy = true;
        let backend = self.env.backend.clone();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { backend.save_advance_after_vote(on) })
                .await;
            this.update_in(cx, |this, _, cx| {
                this.busy = false;
                match result {
                    Ok(saved) => this
                        .layout
                        .update(cx, |l, cx| l.set_advance_after_vote(saved, cx)),
                    Err(e) => {
                        this.hint
                            .say(format!("Could not save the setting: {e}"), true, cx, |p| {
                                &mut p.hint
                            });
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Writes the theme; on success it applies at once.
    fn save_theme(&mut self, name: ThemeName, window: &mut Window, cx: &mut Context<Self>) {
        self.busy = true;
        let backend = self.env.backend.clone();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { backend.save_theme(name) })
                .await;
            this.update_in(cx, |this, _, cx| {
                this.busy = false;
                match result {
                    Ok(saved) => app_theme::set(saved, cx),
                    Err(e) => {
                        this.hint
                            .say(format!("Could not save the theme: {e}"), true, cx, |p| {
                                &mut p.hint
                            });
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Writes the light/dark modes; on success they apply at once.
    fn save_modes(&mut self, modes: ColorModes, window: &mut Window, cx: &mut Context<Self>) {
        self.busy = true;
        let backend = self.env.backend.clone();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { backend.save_color_modes(modes) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(saved) => app_theme::set_color_modes(saved, window, cx),
                    Err(e) => {
                        this.hint
                            .say(format!("Could not save the mode: {e}"), true, cx, |p| {
                                &mut p.hint
                            });
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // ---- rendering ----

    fn key_context(&self) -> &'static str {
        match self.mode {
            Mode::Browsing => "SettingsPage Browsing",
            Mode::Editing { .. } | Mode::EditingStartup | Mode::EditingServer(_) => {
                "SettingsPage Editing"
            }
            Mode::Confirming(..) | Mode::ConfirmingServer(_) => "SettingsPage Confirming",
        }
    }

    fn render_row(
        &self,
        which: Which,
        i: usize,
        q: &SavedQuery,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let row_no = self.row_of(which, i);
        let selected = row_no == self.cursor;
        // Element ids of the list's buttons.
        let [up, down, edit, delete, row_id] = match which {
            Which::Queries => ["q-up", "q-down", "q-edit", "q-delete", "query-row"],
            Which::Bookmarks => ["b-up", "b-down", "b-edit", "b-delete", "bookmark-row"],
        };
        let len = self.list(which).len();
        let buttons = h_flex()
            .gap_1()
            .flex_none()
            .child(
                Button::new((up, i))
                    .label("\u{2191}")
                    .small()
                    .disabled(i == 0 || self.busy)
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.cursor = row_no;
                        this.move_up(&MoveQueryUp, w, cx)
                    })),
            )
            .child(
                Button::new((down, i))
                    .label("\u{2193}")
                    .small()
                    .disabled(i + 1 >= len || self.busy)
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.cursor = row_no;
                        this.move_down(&MoveQueryDown, w, cx)
                    })),
            )
            .child(
                Button::new((edit, i))
                    .label("Edit")
                    .small()
                    .disabled(self.busy)
                    .on_click(cx.listener(move |this, _, w, cx| {
                        if this.mode == Mode::Browsing && this.can_change() {
                            this.start_edit(which, i, w, cx);
                        }
                    })),
            )
            .child(
                Button::new((delete, i))
                    .label("Delete")
                    .small()
                    .disabled(self.busy)
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.cursor = row_no;
                        this.delete_query(&DeleteQuery, w, cx)
                    })),
            );
        let row = h_flex()
            .id((row_id, i))
            .px_3()
            .py_1p5()
            .gap_3()
            .items_center()
            .when(selected, |d| d.bg(theme.list_active))
            .on_click(cx.listener(move |this, _, _, cx| this.select(row_no, cx)));
        match self.mode {
            Mode::Editing { which: w, index } if w == which && index == i => {
                self.render_editor(row, cx)
            }
            Mode::Confirming(w, index) if w == which && index == i => row
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(theme.danger)
                        .child(format!("Delete \u{201c}{}\u{201d}?  y / n", q.name)),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("confirm-yes")
                                .label("Yes (y)")
                                .small()
                                .on_click(cx.listener(|this, _, w, cx| {
                                    this.confirm_delete(&ConfirmDelete, w, cx)
                                })),
                        )
                        .child(Button::new("confirm-no").label("No (n)").small().on_click(
                            cx.listener(|this, _, w, cx| this.cancel_delete(&CancelDelete, w, cx)),
                        )),
                )
                .into_any_element(),
            _ => row
                .child(
                    div()
                        .w(px(200.))
                        .flex_none()
                        .truncate()
                        .child(q.name.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .child(q.query.clone()),
                )
                .child(buttons)
                .into_any_element(),
        }
    }

    /// The row in edit mode: two inputs, and why it cannot be saved yet.
    fn render_editor(&self, row: Stateful<Div>, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        row.child(
            div()
                .w(px(200.))
                .flex_none()
                .child(Input::new(&self.name).small()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(Input::new(&self.query).small()),
        )
        .child(
            h_flex()
                .gap_1()
                .flex_none()
                .child(
                    Button::new("edit-save")
                        .label("Save")
                        .small()
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _, w, cx| this.commit_edit(w, cx))),
                )
                .child(
                    Button::new("edit-cancel").label("Cancel").small().on_click(
                        cx.listener(|this, _, w, cx| this.cancel_edit(&CancelEdit, w, cx)),
                    ),
                ),
        )
        .children(
            self.invalid
                .map(|why| div().text_sm().text_color(theme.danger).child(why)),
        )
        .into_any_element()
    }

    fn render_queries(&self, cx: &mut Context<Self>) -> AnyElement {
        self.render_list(Which::Queries, cx)
    }

    fn render_bookmarks(&self, cx: &mut Context<Self>) -> AnyElement {
        self.render_list(Which::Bookmarks, cx)
    }

    /// The card of one list: its rows, the row being added, and the Add button.
    fn render_list(&self, which: Which, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let items = self.list(which).clone();
        let n = items.len();
        let adding =
            matches!(self.mode, Mode::Editing { which: w, index } if w == which && index >= n);
        let (title, note, empty, add_label, add_id, new_id) = match which {
            Which::Queries => (
                "Saved queries",
                "Each one is a section of the dashboard, in this order.",
                "No saved queries. Press a to add one.",
                "Add a query (a)",
                "add-query",
                "query-row-new",
            ),
            Which::Bookmarks => (
                "Bookmarks",
                "Buttons above the change lists, in this order; B 1 to B 9 open the first nine.",
                "No bookmarks. Press a to add one.",
                "Add a bookmark (a)",
                "add-bookmark",
                "bookmark-row-new",
            ),
        };
        let mut body = v_flex().w_full().child(card::frame(
            card::title_bar(cx)
                .child(div().font_semibold().child(title))
                .children(self.active_badge(cx).map(|b| b.pill(cx)))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(note),
                ),
            true,
            n == 0 && !adding,
            cx,
        ));
        if n == 0 && !adding {
            // The cursor can rest on the empty bookmarks row, to add the first.
            let selected = which == Which::Bookmarks && self.cursor == self.row_of(which, 0);
            body = body.child(card::frame(
                card::row_separator(cx)
                    .px_3()
                    .py_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .when(selected, |d| d.bg(theme.list_active))
                    .child(empty),
                false,
                true,
                cx,
            ));
        }
        for (i, q) in items.iter().enumerate() {
            let row = self.render_row(which, i, q, cx);
            body = body.child(card::frame(
                card::row_separator(cx).child(row),
                false,
                i + 1 == n && !adding,
                cx,
            ));
        }
        if adding {
            let row = h_flex()
                .id(new_id)
                .px_3()
                .py_1p5()
                .gap_3()
                .items_center()
                .bg(theme.list_active);
            let row = self.render_editor(row, cx);
            body = body.child(card::frame(
                card::row_separator(cx).child(row),
                false,
                true,
                cx,
            ));
        }
        v_flex()
            .gap_2()
            .child(body)
            .child(
                div().px_4().child(
                    Button::new(add_id)
                        .label(add_label)
                        .small()
                        .disabled(self.busy || self.mode != Mode::Browsing)
                        .on_click(cx.listener(move |this, _, w, cx| {
                            this.cursor = this.row_of(which, 0);
                            this.add_query(&AddQuery, w, cx)
                        })),
                ),
            )
            .into_any_element()
    }

    /// A titled card with one row, highlighted when the cursor is on it.
    fn section(
        &self,
        title: &'static str,
        note: &'static str,
        row: impl IntoElement,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.section_rows(title, note, vec![(row.into_any_element(), selected)], cx)
    }

    /// A titled card with one or more rows, each of which the cursor can be
    /// on (the `bool` says whether it is).
    fn section_rows(
        &self,
        title: &'static str,
        note: &'static str,
        rows: Vec<(AnyElement, bool)>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let count = rows.len();
        v_flex()
            .w_full()
            .child(card::frame(
                card::title_bar(cx)
                    .child(div().font_semibold().child(title))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(note),
                    ),
                true,
                false,
                cx,
            ))
            .children(rows.into_iter().enumerate().map(|(i, (row, selected))| {
                card::frame(
                    card::row_separator(cx).child(
                        h_flex()
                            .id((title, i))
                            .px_3()
                            .py_1p5()
                            .gap_3()
                            .items_center()
                            .when(selected, |d| d.bg(theme.list_active))
                            .child(row),
                    ),
                    false,
                    i + 1 == count,
                    cx,
                )
            }))
            .into_any_element()
    }

    fn render_startup(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let editing = self.mode == Mode::EditingStartup;
        let search = matches!(self.start_page, StartupPage::Search(_));
        let busy = self.busy;
        let choose = h_flex()
            .gap_1()
            .flex_none()
            .child(
                Button::new("start-dashboard")
                    .label("Dashboard")
                    .small()
                    .when(!search && !editing, |b| b.primary())
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, w, cx| {
                        this.cursor = this.startup_row();
                        if this.can_change() && this.start_page != StartupPage::Dashboard {
                            this.leave_edit(w, cx);
                            this.save_startup(StartupPage::Dashboard, w, cx);
                        }
                    })),
            )
            .child(
                Button::new("start-search")
                    .label("Search")
                    .small()
                    .when(search || editing, |b| b.primary())
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, w, cx| {
                        if this.can_change() && this.mode == Mode::Browsing {
                            this.start_startup_edit(w, cx);
                        }
                    })),
            );
        let detail = if editing {
            h_flex()
                .flex_1()
                .min_w_0()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(&self.startup_input).small()),
                )
                .child(
                    Button::new("startup-save")
                        .label("Save")
                        .small()
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, w, cx| this.commit_edit(w, cx))),
                )
                .child(
                    Button::new("startup-cancel")
                        .label("Cancel")
                        .small()
                        .on_click(
                            cx.listener(|this, _, w, cx| this.cancel_edit(&CancelEdit, w, cx)),
                        ),
                )
                .children(
                    self.invalid
                        .map(|why| div().text_sm().text_color(theme.danger).child(why)),
                )
                .into_any_element()
        } else if let StartupPage::Search(q) = &self.start_page {
            h_flex()
                .flex_1()
                .min_w_0()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .child(q.clone()),
                )
                .child(
                    Button::new("startup-edit")
                        .label("Edit")
                        .small()
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, w, cx| {
                            if this.can_change() && this.mode == Mode::Browsing {
                                this.start_startup_edit(w, cx);
                            }
                        })),
                )
                .into_any_element()
        } else {
            div()
                .flex_1()
                .text_color(theme.muted_foreground)
                .child("The saved queries")
                .into_any_element()
        };
        let row = h_flex()
            .w_full()
            .gap_3()
            .items_center()
            .child(div().w(px(200.)).flex_none().child("Open on start"))
            .child(choose)
            .child(detail);
        self.section(
            "Startup page",
            "What opens when the app starts. Space switches, Enter edits the query.",
            row,
            self.cursor == self.startup_row() && self.mode == Mode::Browsing,
            cx,
        )
    }

    fn render_review(&self, cx: &mut Context<Self>) -> AnyElement {
        let on = self.layout.read(cx).advance_after_vote();
        let busy = self.busy;
        let row = Checkbox::new("advance-after-vote")
            .label("Advance after a vote: go to the next open change of the relation chain")
            .checked(on)
            .disabled(busy)
            .on_click(cx.listener(|this, checked: &bool, w, cx| {
                this.cursor = this.advance_row();
                if this.can_change() {
                    this.save_advance(*checked, w, cx);
                }
            }));
        self.section(
            "Review",
            "Applies to quick votes and to the reply dialog. Space toggles.",
            row,
            self.cursor == self.advance_row() && self.mode == Mode::Browsing,
            cx,
        )
    }

    fn render_appearance(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = app_theme::current(cx);
        let modes = app_theme::color_modes(cx);
        let busy = self.busy;
        let browsing = self.mode == Mode::Browsing;
        let theme_row = self.theme_row();
        let choose = h_flex()
            .gap_1()
            .flex_none()
            .children(ThemeName::ALL.into_iter().map(|name| {
                Button::new(("theme", name as usize))
                    .label(name.label())
                    .small()
                    .when(name == current, |b| b.primary())
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.cursor = this.theme_row();
                        if this.can_change() && app_theme::current(cx) != name {
                            this.save_theme(name, w, cx);
                        }
                    }))
            }));
        let theme_line = h_flex()
            .w_full()
            .gap_3()
            .items_center()
            .child(div().w(px(200.)).flex_none().child("Theme"))
            .child(choose);
        let mode_line =
            |label: &'static str, id: &'static str, diff: bool, cx: &mut Context<Self>| {
                let now = if diff { modes.diff } else { modes.ui };
                h_flex()
                    .w_full()
                    .gap_3()
                    .items_center()
                    .child(div().w(px(200.)).flex_none().child(label))
                    .child(
                        h_flex()
                            .gap_1()
                            .flex_none()
                            .children(ColorMode::ALL.into_iter().map(|mode| {
                                Button::new((id, mode as usize))
                                    .label(mode.label())
                                    .small()
                                    .when(mode == now, |b| b.primary())
                                    .disabled(busy)
                                    .on_click(cx.listener(move |this, _, w, cx| {
                                        this.cursor = if diff {
                                            this.diff_mode_row()
                                        } else {
                                            this.mode_row()
                                        };
                                        let modes = app_theme::color_modes(cx);
                                        let next = if diff {
                                            ColorModes {
                                                diff: mode,
                                                ..modes
                                            }
                                        } else {
                                            ColorModes { ui: mode, ..modes }
                                        };
                                        if this.can_change() && next != modes {
                                            this.save_modes(next, w, cx);
                                        }
                                    }))
                            })),
                    )
            };
        let ui_line = mode_line("Light or dark", "ui-mode", false, cx);
        let diff_line = mode_line("Light or dark in diffs", "diff-mode", true, cx);
        self.section_rows(
            "Appearance",
            "Space switches the theme or mode under the cursor.",
            vec![
                (
                    theme_line.into_any_element(),
                    self.cursor == theme_row && browsing,
                ),
                (
                    ui_line.into_any_element(),
                    self.cursor == self.mode_row() && browsing,
                ),
                (
                    diff_line.into_any_element(),
                    self.cursor == self.diff_mode_row() && browsing,
                ),
            ],
            cx,
        )
    }

    /// What the rule does: the word, with a swatch and "bold" for a highlight.
    fn render_effect(&self, action: &FilterAction, cx: &App) -> AnyElement {
        let theme = cx.theme();
        match action {
            FilterAction::Hide => div().child("hide").into_any_element(),
            FilterAction::Dim => div().child("dim").into_any_element(),
            FilterAction::Highlight { color, bold } => h_flex()
                .gap_1p5()
                .items_center()
                .child(
                    div()
                        .size_3()
                        .flex_none()
                        .rounded_full()
                        .border_1()
                        .border_color(theme.border)
                        .bg(color.map_or(theme.foreground, Hsla::from)),
                )
                .child(div().when(*bold, |d| d.font_bold()).child("highlight"))
                .when(*bold, |d| {
                    d.child(div().text_color(theme.muted_foreground).child("bold"))
                })
                .into_any_element(),
        }
    }

    fn render_filter_row(&self, i: usize, rule: &FileFilter, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let selected = self.filters_row() + i == self.cursor && self.mode == Mode::Browsing;
        let globs = rule.globs.join(", ");
        h_flex()
            .id(("filter-row", i))
            .px_3()
            .py_1p5()
            .gap_3()
            .items_center()
            .when(selected, |d| d.bg(theme.list_active))
            .on_click(cx.listener(move |this, _, _, cx| {
                let at = this.filters_row() + i;
                this.select(at, cx)
            }))
            .child(
                div()
                    .w(px(200.))
                    .flex_none()
                    .truncate()
                    .child(rule.name.clone()),
            )
            .child(
                div()
                    .w(px(150.))
                    .flex_none()
                    .child(self.render_effect(&rule.action, cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.muted_foreground)
                    .child(globs),
            )
            .child(
                div()
                    .w(px(160.))
                    .flex_none()
                    .truncate()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(scope_text(rule, &self.servers)),
            )
            .child(
                div()
                    .w(px(90.))
                    .flex_none()
                    .text_xs()
                    .text_color(theme.warning)
                    .when(!rule.enabled, |d| d.child("off by default")),
            )
            .child(
                Button::new(("filter-edit", i))
                    .label("Edit")
                    .small()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.cursor = this.filters_row() + i;
                        this.manage_filters(Some(i), false, cx)
                    })),
            )
            .into_any_element()
    }

    fn render_filters(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let rules = self.filters.read(cx).rules().to_vec();
        let n = rules.len();
        let mut body = v_flex().w_full().child(card::frame(
            card::title_bar(cx)
                .child(div().font_semibold().child("File filters"))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Hide, dim or highlight files in the file lists and diffs. Enter edits, a adds."),
                ),
            true,
            n == 0,
            cx,
        ));
        if n == 0 {
            let selected = self.cursor == self.filters_row() && self.mode == Mode::Browsing;
            body = body.child(card::frame(
                card::row_separator(cx).child(
                    div()
                        .id("filter-row-empty")
                        .px_3()
                        .py_2()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .when(selected, |d| d.bg(theme.list_active))
                        .child("No file filters. Press a to add one."),
                ),
                false,
                true,
                cx,
            ));
        }
        for (i, rule) in rules.iter().enumerate() {
            let row = self.render_filter_row(i, rule, cx);
            body = body.child(card::frame(
                card::row_separator(cx).child(row),
                false,
                i + 1 == n,
                cx,
            ));
        }
        v_flex()
            .gap_2()
            .child(body)
            .child(
                div().px_4().child(
                    Button::new("add-filter")
                        .label("Add a filter (a)")
                        .small()
                        .disabled(self.mode != Mode::Browsing)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cursor = this.filters_row();
                            this.manage_filters(None, true, cx)
                        })),
                ),
            )
            .into_any_element()
    }

    fn render_hint(&self, cx: &App) -> Option<impl IntoElement> {
        let (text, error) = self.hint.current()?;
        let theme = cx.theme();
        Some(
            div()
                .px_4()
                .py_1()
                .text_xs()
                .text_color(if error {
                    theme.danger
                } else {
                    theme.foreground
                })
                .child(text.to_owned()),
        )
    }
}

impl SettingsPage {
    /// The sections of `tab`.
    fn render_tab(&self, tab: SettingsTab, cx: &mut Context<Self>) -> Vec<AnyElement> {
        match tab {
            SettingsTab::Servers => vec![self.render_servers(cx)],
            SettingsTab::Queries => vec![
                self.render_queries(cx),
                self.render_bookmarks(cx),
                self.render_startup(cx),
            ],
            SettingsTab::Review => vec![self.render_review(cx)],
            SettingsTab::Appearance => vec![self.render_appearance(cx)],
            SettingsTab::Filters => vec![self.render_filters(cx)],
        }
    }

    /// The title and the strip of tabs, which stay put while a tab scrolls.
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_none()
            .px_4()
            .pt_4()
            .gap_3()
            .child(div().text_xl().font_semibold().child("Settings"))
            .child(
                TabBar::new("settings-tabs")
                    .underline()
                    .selected_index(self.tab.position())
                    .on_click(cx.listener(|this, ix: &usize, _, cx| {
                        if let Some(tab) = SettingsTab::all().get(*ix) {
                            this.show_tab(*tab, cx);
                        }
                    }))
                    .children(
                        SettingsTab::all()
                            .into_iter()
                            .map(|t| Tab::new().label(t.title())),
                    ),
            )
    }
}

impl Render for SettingsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let sections = self.render_tab(self.tab, cx);
        v_flex()
            .id("settings")
            .key_context(self.key_context())
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::edit_query))
            .on_action(cx.listener(Self::toggle_setting))
            .on_action(cx.listener(Self::add_query))
            .on_action(cx.listener(Self::delete_query))
            .on_action(cx.listener(Self::confirm_delete))
            .on_action(cx.listener(Self::cancel_delete))
            .on_action(cx.listener(Self::cancel_edit))
            .on_action(cx.listener(Self::other_field))
            .on_action(
                cx.listener(|this, _: &PrevField, w, cx| this.other_field(&NextField, w, cx)),
            )
            .on_action(cx.listener(Self::move_up))
            .on_action(cx.listener(Self::move_down))
            .on_action(cx.listener(Self::test_connection))
            .size_full()
            .child(self.render_header(cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .id("settings-scroll")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .py_4()
                            .gap_4()
                            .children(sections),
                    )
                    .vertical_scrollbar(&self.scroll),
            )
            .children(self.render_hint(cx))
            .child(
                div()
                    .px_4()
                    .py_1()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(
                        "j / k: select    Enter / e: edit    a: add    d: delete    \
                         Shift-J / Shift-K: move    Space: toggle    u: back to the dashboard    \
                         Filters: Enter / e / d: dialog    Servers: t: test connection",
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{moved, validate, with_query, without};
    use crate::session::SavedQuery;

    fn q(n: &str) -> SavedQuery {
        SavedQuery {
            name: n.into(),
            query: format!("q:{n}"),
        }
    }

    fn names(l: &[SavedQuery]) -> Vec<&str> {
        l.iter().map(|q| q.name.as_str()).collect()
    }

    #[test]
    fn a_query_needs_a_name_and_text() {
        assert_eq!(validate("", "is:open"), Err("Give the query a name."));
        assert_eq!(validate("  ", "is:open"), Err("Give the query a name."));
        assert_eq!(validate("Open", " "), Err("Enter the query text."));
        assert_eq!(
            validate(" Open ", " is:open "),
            Ok(SavedQuery {
                name: "Open".into(),
                query: "is:open".into()
            })
        );
    }

    #[test]
    fn moving_swaps_with_the_neighbour_and_stops_at_the_ends() {
        let l = vec![q("a"), q("b"), q("c")];
        let (down, at) = moved(&l, 0, 1).unwrap();
        assert_eq!((names(&down), at), (vec!["b", "a", "c"], 1));
        let (up, at) = moved(&l, 2, -1).unwrap();
        assert_eq!((names(&up), at), (vec!["a", "c", "b"], 1));
        assert!(moved(&l, 0, -1).is_none());
        assert!(moved(&l, 2, 1).is_none());
        assert!(moved::<SavedQuery>(&[], 0, 1).is_none());
    }

    #[test]
    fn replace_add_and_delete() {
        let l = vec![q("a"), q("b")];
        assert_eq!(names(&with_query(&l, 1, q("x"))), ["a", "x"]);
        assert_eq!(names(&with_query(&l, 2, q("x"))), ["a", "b", "x"]);
        assert_eq!(names(&without(&l, 0)), ["b"]);
        assert_eq!(names(&without(&l, 5)), ["a", "b"]);
        assert!(without(&without(&l, 0), 0).is_empty());
    }
}
