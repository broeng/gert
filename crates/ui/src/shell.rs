//! The root view: top bar, the current page, history, keyboard help and
//! window-focus tracking.

use gerrit_api::Auth;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use model::ChangeNumber;

use crate::{
    actions::{self, *},
    login::LoginOutcome,
    nav::{ChangeView, NavState, Route},
    pages::{
        change::{ChangeEvent, ChangePage},
        list::{ChangeListPage, ListEvent, ListKind},
        login::{LoginPage, LoginPrefill},
    },
    row::server_label,
    session::{Env, SavedQuery, ServerSetup, Session, Startup},
    settings::DiffSettings,
};

enum Page {
    Login(Entity<LoginPage>),
    List(Entity<ChangeListPage>),
    Change(Entity<ChangePage>),
}

impl Page {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self {
            Page::Login(p) => p.focus_handle(cx),
            Page::List(p) => p.focus_handle(cx),
            Page::Change(p) => p.focus_handle(cx),
        }
    }

    fn element(&self) -> AnyElement {
        match self {
            Page::Login(p) => p.clone().into_any_element(),
            Page::List(p) => p.clone().into_any_element(),
            Page::Change(p) => p.clone().into_any_element(),
        }
    }

    fn set_window_active(&self, active: bool, cx: &mut App) {
        match self {
            Page::Login(_) => {}
            Page::List(p) => p.update(cx, |p, cx| p.set_window_active(active, cx)),
            Page::Change(p) => p.update(cx, |p, cx| p.set_window_active(active, cx)),
        }
    }
}

/// Registers key bindings. Call once after `gpui_kit::init`.
pub fn init(cx: &mut App) {
    actions::bind_keys(cx);
    crate::diff::bind_keys(cx);
}

pub struct Shell {
    env: Env,
    session: Option<Session>,
    queries: Vec<SavedQuery>,
    nav: NavState,
    page: Page,
    page_subs: Vec<Subscription>,
    prefill: Option<LoginPrefill>,
    search: Entity<InputState>,
    focus: FocusHandle,
    help: bool,
    window_active: bool,
    credentials_task: Option<Task<()>>,
    settings: Entity<DiffSettings>,
    prefs_task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl Shell {
    pub fn new(startup: Startup, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let Startup {
            env,
            server,
            queries,
            diff,
        } = startup;
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search changes  ( / )"));
        let mut subs = vec![
            cx.subscribe_in(
                &search,
                window,
                |this, input, ev: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        let q = input.read(cx).value().trim().to_owned();
                        this.search(q, window, cx);
                    }
                },
            ),
            cx.observe_window_activation(window, |this, window, cx| {
                this.window_active = window.is_window_active();
                this.page.set_window_active(this.window_active, cx);
            }),
        ];
        subs.push(cx.observe_window_appearance(window, |_, window, cx| {
            gpui_kit::component::Theme::sync_system_appearance(Some(window), cx);
        }));

        // The first page is a placeholder; `start` replaces it.
        let login = cx.new(|cx| LoginPage::new(env.clone(), LoginPrefill::default(), window, cx));
        let mut this = Self {
            env,
            session: None,
            queries,
            nav: NavState::new(Route::Login),
            page: Page::Login(login),
            page_subs: Vec::new(),
            prefill: None,
            search,
            focus: cx.focus_handle(),
            help: false,
            window_active: window.is_window_active(),
            credentials_task: None,
            settings: cx.new(|_| DiffSettings::new(diff)),
            prefs_task: None,
            _subs: subs,
        };
        this.start(server, window, cx);
        this
    }

    /// Opens the dashboard from the cache right away and fetches the
    /// password in the background.
    fn start(&mut self, server: Option<ServerSetup>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(setup) = server else {
            return self.show(window, cx);
        };
        let username = setup.username.clone();
        let auth = if username.is_some() {
            None
        } else {
            Some(Auth::Anonymous)
        };
        match Session::new(&self.env, &setup, auth) {
            Ok(session) => self.session = Some(session),
            Err(e) => {
                self.prefill = Some(prefill_from(
                    &setup,
                    format!("Invalid server configuration: {e}"),
                ));
                return self.show(window, cx);
            }
        }
        self.nav.reset(Route::Dashboard);
        self.show(window, cx);
        self.load_diff_preferences(cx);
        let Some(username) = username else {
            return;
        };
        let backend = self.env.backend.clone();
        let (url, user) = (setup.base_url.clone(), username.clone());
        self.credentials_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { backend.load_password(&url, &user) })
                .await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(Some(password)) => {
                    let auth = Auth::Basic { username, password };
                    match Session::new(&this.env, &setup, Some(auth)) {
                        Ok(session) => {
                            this.session = Some(session);
                            this.show(window, cx);
                            this.load_diff_preferences(cx);
                        }
                        Err(e) => this.back_to_login(
                            &setup,
                            format!("Invalid server configuration: {e}"),
                            window,
                            cx,
                        ),
                    }
                }
                Ok(None) => this.back_to_login(
                    &setup,
                    "No saved password for this account. Please log in again.".into(),
                    window,
                    cx,
                ),
                Err(e) => this.back_to_login(
                    &setup,
                    format!("Could not read the password: {e}"),
                    window,
                    cx,
                ),
            })
            .ok();
        }));
    }

    /// Reads the cached diff preferences of the signed-in user right away,
    /// then refreshes them from the server in the background (a session
    /// without credentials yet reads the cache only). Anonymous sessions
    /// have none, so only the local settings apply.
    fn load_diff_preferences(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let settings = self.settings.clone();
        self.prefs_task = Some(cx.spawn(async move |_, cx| {
            let syncer = session.syncer().clone();
            let cached = {
                let syncer = syncer.clone();
                cx.background_spawn(async move { syncer.cached_diff_preferences() })
                    .await
            };
            match cached {
                Ok(c) => {
                    settings.update(cx, |s, cx| s.set_server(c.map(|c| c.preferences), cx));
                }
                Err(e) => log::warn!("reading cached diff preferences: {e}"),
            }
            if !session.can_sync() {
                return;
            }
            match cx
                .background_spawn(async move { syncer.refresh_diff_preferences().await })
                .await
            {
                Ok(Some(prefs)) => {
                    settings.update(cx, |s, cx| s.set_server(Some(prefs), cx));
                }
                Ok(None) => {}
                Err(e) => log::warn!("refreshing diff preferences: {e}"),
            }
        }));
    }

    fn back_to_login(
        &mut self,
        setup: &ServerSetup,
        notice: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session = None;
        self.prefill = Some(prefill_from(setup, notice));
        self.nav.reset(Route::Login);
        self.show(window, cx);
    }

    #[cfg(test)]
    pub(crate) fn route(&self) -> &Route {
        self.nav.current()
    }

    #[cfg(test)]
    pub(crate) fn settings(&self) -> &Entity<DiffSettings> {
        &self.settings
    }

    // ---- navigation ----

    /// Builds the page for the current route and focuses it.
    fn show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.page_subs.clear();
        let mut route = self.nav.current().clone();
        if self.session.is_none() && route != Route::Login {
            self.nav.reset(Route::Login);
            route = Route::Login;
        }
        let active = self.window_active;
        let restore = self.nav.selected();
        self.page = match (route, self.session.clone()) {
            (Route::Dashboard, Some(session)) => {
                let queries = self.queries.clone();
                self.list_page(
                    session,
                    ListKind::Dashboard,
                    queries,
                    restore,
                    active,
                    window,
                    cx,
                )
            }
            (Route::Search(q), Some(session)) => {
                let spec = vec![SavedQuery {
                    name: "Search".into(),
                    query: q,
                }];
                self.list_page(session, ListKind::Search, spec, restore, active, window, cx)
            }
            (Route::Change(n), Some(session)) => {
                let view = self.nav.change_view().unwrap_or_default();
                self.change_page(session, n, view, active, window, cx)
            }
            _ => {
                let prefill = self.prefill.take().unwrap_or_default();
                let env = self.env.clone();
                let page = cx.new(|cx| LoginPage::new(env, prefill, window, cx));
                self.page_subs.push(cx.subscribe_in(
                    &page,
                    window,
                    |this, _, outcome: &LoginOutcome, window, cx| {
                        this.logged_in(outcome, window, cx)
                    },
                ));
                Page::Login(page)
            }
        };
        let handle = self.page.focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    fn list_page(
        &mut self,
        session: Session,
        kind: ListKind,
        specs: Vec<SavedQuery>,
        restore: Option<ChangeNumber>,
        active: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Page {
        let page = cx.new(|cx| ChangeListPage::new(session, kind, specs, restore, active, cx));
        self.page_subs.push(cx.subscribe_in(
            &page,
            window,
            |this, page, ev: &ListEvent, window, cx| match ev {
                ListEvent::Open(n) => {
                    this.nav.set_selected(page.read(cx).selected_number());
                    this.navigate(Route::Change(*n), window, cx);
                }
                ListEvent::Unauthorized => this.unauthorized(window, cx),
            },
        ));
        Page::List(page)
    }

    fn change_page(
        &mut self,
        session: Session,
        number: ChangeNumber,
        view: ChangeView,
        active: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Page {
        let settings = self.settings.clone();
        let page = cx.new(|cx| ChangePage::new(session, settings, number, view, active, cx));
        self.page_subs.push(cx.subscribe_in(
            &page,
            window,
            |this, _, ev: &ChangeEvent, window, cx| match ev {
                ChangeEvent::Unauthorized => this.unauthorized(window, cx),
                ChangeEvent::OpenChange(n) => this.navigate(Route::Change(*n), window, cx),
                // The diff page arrives in the next commit.
                ChangeEvent::OpenDiff(_) => {}
            },
        ));
        Page::Change(page)
    }

    /// Remembers what is selected on the page we are about to leave.
    fn remember_selection(&mut self, cx: &App) {
        match &self.page {
            Page::List(p) => self.nav.set_selected(p.read(cx).selected_number()),
            Page::Change(p) => self.nav.set_change_view(Some(p.read(cx).view_state())),
            _ => {}
        }
    }

    fn navigate(&mut self, route: Route, window: &mut Window, cx: &mut Context<Self>) {
        self.remember_selection(cx);
        self.nav.push(route);
        self.show(window, cx);
    }

    fn search(&mut self, query: String, window: &mut Window, cx: &mut Context<Self>) {
        if !query.is_empty() && self.session.is_some() {
            self.navigate(Route::Search(query), window, cx);
        }
    }

    fn logged_in(&mut self, outcome: &LoginOutcome, window: &mut Window, cx: &mut Context<Self>) {
        let setup = ServerSetup {
            base_url: outcome.base_url.clone(),
            username: outcome.username.clone(),
            server: outcome.server,
        };
        let auth = match (&outcome.username, &outcome.password) {
            (Some(username), Some(password)) => Auth::Basic {
                username: username.clone(),
                password: password.clone(),
            },
            _ => Auth::Anonymous,
        };
        match Session::new(&self.env, &setup, Some(auth)) {
            Ok(session) => {
                self.session = Some(session);
                self.queries = outcome.queries.clone();
                self.nav.reset(Route::Dashboard);
                self.show(window, cx);
                self.load_diff_preferences(cx);
            }
            Err(e) => {
                self.prefill = Some(prefill_from(&setup, e.to_string()));
                self.show(window, cx);
            }
        }
    }

    /// The server refused our credentials: back to the login page, with the
    /// server filled in.
    fn unauthorized(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(s) = self.session.take() {
            self.prefill = Some(LoginPrefill {
                base_url: s.base_url,
                username: s.username,
                notice: Some(
                    "The server rejected the saved credentials. Please log in again.".into(),
                ),
            });
        }
        self.nav.reset(Route::Login);
        self.show(window, cx);
    }

    // ---- actions ----

    fn go_back(&mut self, _: &GoBack, window: &mut Window, cx: &mut Context<Self>) {
        self.remember_selection(cx);
        if self.nav.back() {
            self.show(window, cx);
        }
    }

    fn go_forward(&mut self, _: &GoForward, window: &mut Window, cx: &mut Context<Self>) {
        self.remember_selection(cx);
        if self.nav.forward() {
            self.show(window, cx);
        }
    }

    fn go_up(&mut self, _: &GoUp, window: &mut Window, cx: &mut Context<Self>) {
        self.remember_selection(cx);
        if self.nav.up() {
            self.show(window, cx);
        }
    }

    fn focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_some() {
            self.search.update(cx, |s, cx| s.focus(window, cx));
        }
    }

    fn blur_search(&mut self, _: &BlurSearch, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.page.focus_handle(cx);
        window.focus(&handle, cx);
    }

    fn toggle_help(&mut self, _: &ToggleHelp, _: &mut Window, cx: &mut Context<Self>) {
        self.help = !self.help;
        cx.notify();
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        if self.help {
            self.help = false;
            cx.notify();
        } else {
            cx.propagate();
        }
    }

    // ---- rendering ----

    fn render_top_bar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let session = self.session.as_ref()?;
        if *self.nav.current() == Route::Login {
            return None;
        }
        let theme = cx.theme().clone();
        let who = session
            .username
            .clone()
            .unwrap_or_else(|| "anonymous".into());
        Some(
            h_flex()
                .px_3()
                .py_2()
                .gap_2()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    Button::new("back")
                        .label("\u{2190}")
                        .small()
                        .disabled(!self.nav.can_go_back())
                        .on_click(cx.listener(|this, _, w, cx| this.go_back(&GoBack, w, cx))),
                )
                .child(
                    Button::new("forward")
                        .label("\u{2192}")
                        .small()
                        .disabled(!self.nav.can_go_forward())
                        .on_click(cx.listener(|this, _, w, cx| this.go_forward(&GoForward, w, cx))),
                )
                .child(
                    Button::new("home")
                        .label(server_label(&session.base_url))
                        .small()
                        .on_click(
                            cx.listener(|this, _, w, cx| this.navigate(Route::Dashboard, w, cx)),
                        ),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(who),
                )
                .child(
                    div()
                        .flex_1()
                        .key_context(SEARCH_BAR)
                        .on_action(cx.listener(Self::blur_search))
                        .child(Input::new(&self.search)),
                )
                .child(
                    Button::new("help").label("?").small().on_click(
                        cx.listener(|this, _, w, cx| this.toggle_help(&ToggleHelp, w, cx)),
                    ),
                ),
        )
    }

    fn render_help(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let rows = actions::entries()
            .into_iter()
            .filter(|e| !e.keys.is_empty())
            .map(|e| {
                h_flex()
                    .gap_4()
                    .child(
                        div()
                            .w(px(170.))
                            .flex_none()
                            .font_family("monospace")
                            .child(e.keys),
                    )
                    .child(div().child(e.description))
            })
            .collect::<Vec<_>>();
        div()
            .id("help-overlay")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.overlay)
            .on_click(cx.listener(|this, _, w, cx| this.toggle_help(&ToggleHelp, w, cx)))
            .child(
                v_flex()
                    .gap_2()
                    .p_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .text_color(theme.popover_foreground)
                    .text_sm()
                    .child(
                        div()
                            .text_base()
                            .font_semibold()
                            .child("Keyboard shortcuts"),
                    )
                    .children(rows)
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Press ? or Esc to close"),
                    ),
            )
    }
}

fn prefill_from(setup: &ServerSetup, notice: String) -> LoginPrefill {
    LoginPrefill {
        base_url: setup.base_url.clone(),
        username: setup.username.clone(),
        notice: Some(notice),
    }
}

impl Render for Shell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        div()
            .id("shell")
            .key_context(SHELL)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::go_back))
            .on_action(cx.listener(Self::go_forward))
            .on_action(cx.listener(Self::go_up))
            .on_action(cx.listener(Self::focus_search))
            .on_action(cx.listener(Self::toggle_help))
            .on_action(cx.listener(Self::dismiss))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground)
            .children(self.render_top_bar(cx))
            .child(div().flex_1().min_h_0().child(self.page.element()))
            .when(self.help, |d| d.child(self.render_help(cx)))
    }
}
