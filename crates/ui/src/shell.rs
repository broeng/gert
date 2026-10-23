//! The root view: top bar, the current page, history, keyboard help and
//! window-focus tracking.

use gerrit_api::Auth;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::component::{IconName, TitleBar, window_border};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use model::{ChangeDetail, ChangeNumber};

use crate::{
    action_menu::{ActionDialog, ActionEvent, Done},
    actions::{self, *},
    filters_dialog::{FiltersContext, FiltersDialog, FiltersEvent},
    goto::{ChangeLink, GotoDialog, GotoEvent, LinkError, parse_link},
    login::LoginOutcome,
    nav::{ChangeView, DiffRoute, NavState, Route},
    outbox::{DraftOutbox, OutboxEvent},
    pages::{
        change::{ChangeEvent, ChangePage},
        diff::{DiffPage, DiffPageEvent},
        list::{ChangeListPage, ListEvent, ListKind},
        login::{LoginPage, LoginPrefill},
        settings::{SettingsEvent, SettingsPage},
    },
    reply::{ReplyDialog, ReplyEvent, Sent},
    row::server_label,
    servers::{ACTIVE_SERVER_STATE, Connection, Servers},
    session::{
        Env, SavedQuery, ServerKey, ServerSetup, Session, Startup, StartupPage,
        server_name_from_url,
    },
    settings::{DiffSettings, FileFilters, PageLayout},
    syntax_cache::SyntaxCache,
};

enum Page {
    Login(Entity<LoginPage>),
    List(Entity<ChangeListPage>),
    Change(Entity<ChangePage>),
    Diff(Entity<DiffPage>),
    Settings(Entity<SettingsPage>),
}

impl Page {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self {
            Page::Login(p) => p.focus_handle(cx),
            Page::List(p) => p.focus_handle(cx),
            Page::Change(p) => p.focus_handle(cx),
            Page::Diff(p) => p.focus_handle(cx),
            Page::Settings(p) => p.focus_handle(cx),
        }
    }

    fn element(&self) -> AnyElement {
        match self {
            Page::Login(p) => p.clone().into_any_element(),
            Page::List(p) => p.clone().into_any_element(),
            Page::Change(p) => p.clone().into_any_element(),
            Page::Diff(p) => p.clone().into_any_element(),
            Page::Settings(p) => p.clone().into_any_element(),
        }
    }

    fn set_window_active(&self, active: bool, cx: &mut App) {
        match self {
            Page::Login(_) => {}
            Page::List(p) => p.update(cx, |p, cx| p.set_window_active(active, cx)),
            Page::Change(p) => p.update(cx, |p, cx| p.set_window_active(active, cx)),
            Page::Diff(_) | Page::Settings(_) => {}
        }
    }
}

/// Registers key bindings. Call once after `gpui_kit::init`.
pub fn init(cx: &mut App) {
    crate::theme::set(crate::theme::ThemeName::default(), cx);
    // Always show the scroll bars: the default only shows them while
    // scrolling, which hides that a long list or diff scrolls at all.
    gpui_kit::component::Theme::set_scrollbar_mode(
        gpui_kit::component::scroll::ScrollbarMode::Always,
        cx,
    );
    actions::bind_keys(cx);
    crate::diff::bind_keys(cx);
    crate::reply::actions::bind_keys(cx);
    crate::action_menu::actions::bind_keys(cx);
    crate::filters_dialog::actions::bind_keys(cx);
    crate::goto::bind_keys(cx);
}

pub struct Shell {
    env: Env,
    /// The configured servers, each with its session, outbox, connection
    /// state, queries and bookmarks; and which one is active.
    servers: Servers,
    /// What `start` opens (the `[startup]` setting).
    start_page: StartupPage,
    nav: NavState,
    page: Page,
    page_subs: Vec<Subscription>,
    prefill: Option<LoginPrefill>,
    search: Entity<InputState>,
    focus: FocusHandle,
    help: bool,
    help_scroll: ScrollHandle,
    /// The reply dialog while it is open (modal; the pages keep their state).
    reply: Option<Entity<ReplyDialog>>,
    reply_subs: Vec<Subscription>,
    /// The change action menu while it is open (modal, like the reply).
    menu: Option<Entity<ActionDialog>>,
    menu_subs: Vec<Subscription>,
    /// The Manage filters dialog while it is open (modal, like the others).
    filters_dialog: Option<Entity<FiltersDialog>>,
    filters_dialog_subs: Vec<Subscription>,
    /// The Go to dialog while it is open (modal, like the others).
    goto: Option<Entity<GotoDialog>>,
    goto_subs: Vec<Subscription>,
    /// Shown in the title bar when nobody is signed in.
    app_name: SharedString,
    window_active: bool,
    settings: Entity<DiffSettings>,
    layout: Entity<PageLayout>,
    /// The file filter rules and their session toggles; the pages get it
    /// with the change and diff pages.
    filters: Entity<FileFilters>,
    prefs_task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl Shell {
    pub fn new(startup: Startup, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let Startup {
            env,
            servers,
            active,
            filters,
            diff,
            start_page,
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
        // The page lists are virtualized: a focused element (an inline diff
        // after a click) that scrolls out of the list is no longer in the
        // frame, and GPUI then dispatches keys from the window root, where
        // none of our bindings match. Hand the focus to the closest ancestor
        // that is still there, or to the page.
        subs.push(cx.on_focus_lost(window, |this, window, cx| {
            // The dialog is modal: the focus stays in it.
            let modal = this.reply.as_ref().map(|d| d.focus_handle(cx));
            let modal = modal.or_else(|| this.menu.as_ref().map(|d| d.focus_handle(cx)));
            let modal = modal.or_else(|| this.filters_dialog.as_ref().map(|d| d.focus_handle(cx)));
            let modal = modal.or_else(|| this.goto.as_ref().map(|d| d.focus_handle(cx)));
            if let Some(handle) = modal {
                window.focus(&handle, cx);
                return;
            }
            let target = window
                .focus_lost_restore_target(cx)
                .unwrap_or_else(|| this.page.focus_handle(cx));
            window.focus(&target, cx);
        }));
        subs.push(cx.observe_window_appearance(window, |_, window, cx| {
            crate::theme::sync_appearance(window, cx);
        }));

        // The first page is a placeholder; `start` replaces it.
        let requested = active.clone();
        let servers = Servers::new(servers, active);
        let login = cx.new(|cx| LoginPage::new(env.clone(), LoginPrefill::default(), window, cx));
        let mut this = Self {
            env,
            servers,
            start_page,
            nav: NavState::new(Route::Login),
            page: Page::Login(login),
            page_subs: Vec::new(),
            prefill: None,
            search,
            focus: cx.focus_handle(),
            help: false,
            help_scroll: ScrollHandle::new(),
            reply: None,
            reply_subs: Vec::new(),
            menu: None,
            menu_subs: Vec::new(),
            filters_dialog: None,
            filters_dialog_subs: Vec::new(),
            goto: None,
            goto_subs: Vec::new(),
            app_name: SharedString::default(),
            window_active: window.is_window_active(),
            settings: cx.new(|_| DiffSettings::new(diff)),
            layout: cx.new(|_| PageLayout::default()),
            filters: cx.new(|_| FileFilters::new(filters)),
            prefs_task: None,
            _subs: subs,
        };
        this.start(window, cx);
        // Remember the fallback when the stored server is gone from the
        // config, so that the next start does not look it up again.
        if this.servers.active_key() != requested.as_ref() {
            this.persist_active(cx);
        }
        this
    }

    /// Sets the application name shown in the title bar of the login page.
    pub fn with_app_name(mut self, name: impl Into<SharedString>) -> Self {
        self.app_name = name.into();
        self
    }

    /// Sets whether a vote moves on to the next open change of the chain
    /// (the `[review] advance_after_vote` setting).
    pub fn with_advance_after_vote(self, on: bool, cx: &mut Context<Self>) -> Self {
        self.layout
            .update(cx, |layout, cx| layout.set_advance_after_vote(on, cx));
        self
    }

    /// The session of the active server.
    fn session(&self) -> Option<&Session> {
        self.servers.active()?.session.as_ref()
    }

    /// The syntax colors computed so far for the active server's diffs, and
    /// the worker that makes more; the pages share it.
    fn syntax_cache(&mut self, cx: &mut Context<Self>) -> Entity<SyntaxCache> {
        match self.servers.active_mut() {
            Some(entry) => entry
                .syntax
                .get_or_insert_with(|| cx.new(|_| SyntaxCache::new()))
                .clone(),
            None => cx.new(|_| SyntaxCache::new()),
        }
    }

    /// The outbox of the active server.
    fn active_outbox(&self) -> Option<&Entity<DraftOutbox>> {
        self.servers.active()?.outbox.as_ref()
    }

    /// The saved queries of the active server.
    fn queries(&self) -> Vec<SavedQuery> {
        self.servers
            .active()
            .map(|e| e.setup.queries.clone())
            .unwrap_or_default()
    }

    /// The bookmarks of the active server.
    fn bookmarks(&self) -> Vec<SavedQuery> {
        self.servers
            .active()
            .map(|e| e.setup.bookmarks.clone())
            .unwrap_or_default()
    }

    /// Stores the active server as the one to start on next time.
    fn persist_active(&self, cx: &mut Context<Self>) {
        let Some(key) = self.servers.active_key() else {
            return;
        };
        let (store, key) = (self.env.store.clone(), key.to_string());
        cx.background_spawn(async move {
            let result = match store.lock() {
                Ok(mut store) => store
                    .set_app_state(ACTIVE_SERVER_STATE, &key)
                    .map_err(|e| e.to_string()),
                Err(_) => Err("the cache lock is poisoned".to_owned()),
            };
            if let Err(e) = result {
                log::warn!("remembering the active server: {e}");
            }
        })
        .detach();
    }

    /// Opens the start page of the active server from the cache right away
    /// and fetches the passwords in the background. Every configured server
    /// gets its session and outbox now, so that the drafts of a server that
    /// is not shown are uploaded too.
    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = self.servers.active_key().cloned() else {
            return self.show(window, cx);
        };
        let mut active_error = None;
        for key in self.servers.keys() {
            if let Err(e) = self.open_server(&key, window, cx) {
                if key == active {
                    active_error = Some(e);
                } else {
                    log::warn!("server {key}: invalid configuration: {e}");
                    self.servers.set_connection(&key, Connection::Offline);
                }
            }
        }
        if let Some(e) = active_error {
            if let Some(entry) = self.servers.active() {
                self.prefill = Some(prefill_from(
                    &entry.setup,
                    format!("Invalid server configuration: {e}"),
                ));
            }
            return self.show(window, cx);
        }
        self.nav.reset(match &self.start_page {
            StartupPage::Dashboard => Route::Dashboard,
            StartupPage::Search(q) => Route::Search(q.clone()),
        });
        self.show(window, cx);
        self.load_diff_preferences(cx);
    }

    /// Gives server `key` a session that reads the cache, and starts
    /// fetching its password if it has a user.
    fn open_server(
        &mut self,
        key: &ServerKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), gerrit_api::Error> {
        let Some(entry) = self.servers.get(key) else {
            return Ok(());
        };
        let setup = entry.setup.clone();
        let auth = if setup.username.is_some() {
            None
        } else {
            Some(Auth::Anonymous)
        };
        let session = Session::new(&self.env, &setup, auth)?;
        let backend = session.backend().clone();
        self.install_session(key, Some(session), window, cx);
        let Some(username) = setup.username.clone() else {
            return Ok(());
        };
        let (task_key, key) = (key.clone(), key.clone());
        let (url, user) = (setup.base_url.clone(), username.clone());
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { backend.load_password(&url, &user) })
                .await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(Some(password)) => {
                    let auth = Auth::Basic { username, password };
                    match Session::new(&this.env, &setup, Some(auth)) {
                        Ok(session) => {
                            this.install_session(&key, Some(session), window, cx);
                            if this.servers.active_key() == Some(&key) {
                                this.show(window, cx);
                                this.load_diff_preferences(cx);
                            }
                        }
                        Err(e) => this.credentials_failed(
                            &key,
                            format!("Invalid server configuration: {e}"),
                            window,
                            cx,
                        ),
                    }
                }
                Ok(None) => this.credentials_failed(
                    &key,
                    "No saved password for this account. Please log in again.".into(),
                    window,
                    cx,
                ),
                Err(e) => this.credentials_failed(
                    &key,
                    format!("Could not read the password: {e}"),
                    window,
                    cx,
                ),
            })
            .ok();
        });
        if let Some(entry) = self.servers.get_mut(&task_key) {
            entry.credentials_task = Some(task);
        }
        Ok(())
    }

    /// Replaces the session of server `key`, and with it its outbox of
    /// drafts. A new session counts its drafts and uploads those that
    /// waited.
    fn install_session(
        &mut self,
        key: &ServerKey,
        session: Option<Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut subs = Vec::new();
        let outbox = session.as_ref().map(|s| {
            let outbox = cx.new(|_| DraftOutbox::new(s.clone()));
            let owner = key.clone();
            subs.push(cx.subscribe_in(
                &outbox,
                window,
                move |this, _, ev: &OutboxEvent, window, cx| {
                    if matches!(ev, OutboxEvent::Unauthorized) {
                        this.unauthorized_for(&owner, window, cx);
                    }
                },
            ));
            // The title bar shows the counts.
            subs.push(cx.observe(&outbox, |_, _, cx| cx.notify()));
            outbox.update(cx, |o, cx| o.resume(cx));
            outbox
        });
        if let Some(entry) = self.servers.get_mut(key) {
            entry.session = session;
            entry.outbox = outbox;
            entry.outbox_subs = subs;
        }
    }

    /// A refresh worked, so the active server is reachable: upload the
    /// drafts that waited.
    fn server_reachable(&mut self, cx: &mut Context<Self>) {
        if let Some(key) = self.servers.active_key().cloned() {
            self.servers.set_connection(&key, Connection::Online);
        }
        if let Some(outbox) = self.active_outbox().cloned() {
            outbox.update(cx, |o, cx| o.retry_all(cx));
        }
    }

    /// The outbox of the active server.
    fn outbox_for(&mut self, session: &Session, cx: &mut Context<Self>) -> Entity<DraftOutbox> {
        match self.active_outbox() {
            Some(outbox) => outbox.clone(),
            None => cx.new(|_| DraftOutbox::new(session.clone())),
        }
    }

    /// Reads the cached diff preferences of the signed-in user right away,
    /// then refreshes them from the server in the background (a session
    /// without credentials yet reads the cache only). Anonymous sessions
    /// have none, so only the local settings apply.
    fn load_diff_preferences(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session().cloned() else {
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

    /// The password of server `key` could not be had. The active server
    /// goes to the login page; another one is only marked, and keeps
    /// serving its cache.
    fn credentials_failed(
        &mut self,
        key: &ServerKey,
        notice: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.servers.set_connection(key, Connection::Unauthorized);
        if self.servers.active_key() == Some(key) {
            if let Some(setup) = self.servers.get(key).map(|e| e.setup.clone()) {
                self.back_to_login(&setup, notice, window, cx);
            }
        } else {
            log::warn!("server {key}: {notice}");
        }
    }

    fn back_to_login(
        &mut self,
        setup: &ServerSetup,
        notice: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.install_session(&setup.key, None, window, cx);
        self.prefill = Some(prefill_from(setup, notice));
        self.nav.reset(Route::Login);
        self.show(window, cx);
    }

    #[cfg(test)]
    pub(crate) fn route(&self) -> &Route {
        self.nav.current()
    }

    #[cfg(test)]
    pub(crate) fn change_page_for_test(&self) -> Option<Entity<ChangePage>> {
        match &self.page {
            Page::Change(p) => Some(p.clone()),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn diff_page_for_test(&self) -> Option<Entity<DiffPage>> {
        match &self.page {
            Page::Diff(p) => Some(p.clone()),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn outbox_for_test(&self) -> Option<Entity<DraftOutbox>> {
        self.active_outbox().cloned()
    }

    #[cfg(test)]
    pub(crate) fn outbox_of_for_test(&self, key: &ServerKey) -> Option<Entity<DraftOutbox>> {
        self.servers.get(key)?.outbox.clone()
    }

    #[cfg(test)]
    pub(crate) fn syntax_of_for_test(
        &mut self,
        key: &ServerKey,
        cx: &mut Context<Self>,
    ) -> Option<Entity<SyntaxCache>> {
        self.servers.get(key)?;
        self.servers.set_active(key);
        Some(self.syntax_cache(cx))
    }

    #[cfg(test)]
    pub(crate) fn connection_for_test(&self, key: &str) -> Option<Connection> {
        self.servers.connection(&ServerKey::new(key))
    }

    #[cfg(test)]
    pub(crate) fn list_page_for_test(&self) -> Option<Entity<ChangeListPage>> {
        match &self.page {
            Page::List(p) => Some(p.clone()),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn retry_drafts_for_test(&mut self, cx: &mut Context<Self>) {
        self.retry_drafts(cx);
    }

    #[cfg(test)]
    pub(crate) fn queries_for_test(&self) -> &[SavedQuery] {
        &self.servers.active().expect("a server").setup.queries
    }

    #[cfg(test)]
    pub(crate) fn settings_page_for_test(&self) -> Option<Entity<SettingsPage>> {
        match &self.page {
            Page::Settings(p) => Some(p.clone()),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn layout_for_test(&self) -> &Entity<PageLayout> {
        &self.layout
    }

    #[cfg(test)]
    pub(crate) fn settings(&self) -> &Entity<DiffSettings> {
        &self.settings
    }

    // ---- navigation ----

    /// Builds the page for the current route and focuses it.
    fn show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.page_subs.clear();
        self.drop_reply(cx);
        self.drop_menu();
        self.drop_filters_dialog();
        self.drop_goto();
        let mut route = self.nav.current().clone();
        if self.session().is_none() && route != Route::Login {
            self.nav.reset(Route::Login);
            route = Route::Login;
        }
        let active = self.window_active;
        let restore = self.nav.selected();
        self.page = match (route, self.session().cloned()) {
            (Route::Dashboard, Some(session)) => {
                let queries = self.queries();
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
            (Route::Settings, Some(_)) => self.settings_page(window, cx),
            (Route::Diff(route), Some(session)) => self.diff_page(session, route, window, cx),
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
        let bookmarks = self.bookmarks();
        page.update(cx, |p, cx| p.set_bookmarks(bookmarks, cx));
        self.page_subs.push(cx.subscribe_in(
            &page,
            window,
            |this, page, ev: &ListEvent, window, cx| match ev {
                ListEvent::Open(n) => {
                    this.nav.set_selected(page.read(cx).selected_number());
                    this.navigate(Route::Change(*n), window, cx);
                }
                ListEvent::Search(q) => this.search(q.clone(), window, cx),
                ListEvent::SaveBookmarks(list) => this.save_bookmarks(list.clone(), cx),
                ListEvent::Unauthorized => this.unauthorized(window, cx),
                ListEvent::Refreshed => this.server_reachable(cx),
            },
        ));
        Page::List(page)
    }

    fn settings_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Page {
        let env = self.env.clone();
        let queries = self.queries();
        let bookmarks = self.bookmarks();
        let (start, layout) = (self.start_page.clone(), self.layout.clone());
        let filters = self.filters.clone();
        let server = self.session().map(|s| s.backend().clone());
        let page = cx.new(|cx| {
            SettingsPage::new(
                env, server, queries, bookmarks, start, layout, filters, window, cx,
            )
        });
        self.page_subs.push(cx.subscribe_in(
            &page,
            window,
            |this, _, ev: &SettingsEvent, window, cx| match ev {
                // The dashboard is built from this list when it is shown
                // again, from the cache for the queries it already knew.
                SettingsEvent::QueriesSaved(queries) => {
                    if let Some(e) = this.servers.active_mut() {
                        e.setup.queries = queries.clone();
                    }
                }
                SettingsEvent::BookmarksSaved(list) => {
                    if let Some(e) = this.servers.active_mut() {
                        e.setup.bookmarks = list.clone();
                    }
                }
                SettingsEvent::StartupSaved(page) => this.start_page = page.clone(),
                SettingsEvent::ManageFilters { select, add } => {
                    let context = FiltersContext {
                        select: select.clone(),
                        start_adding: *add,
                        ..Default::default()
                    };
                    this.open_filters_dialog(context, window, cx)
                }
            },
        ));
        Page::Settings(page)
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
        let layout = self.layout.clone();
        let filters = self.filters.clone();
        let syntax = self.syntax_cache(cx);
        let outbox = self.outbox_for(&session, cx);
        let page = cx.new(|cx| {
            ChangePage::new(
                session, settings, layout, filters, syntax, number, view, active, outbox, cx,
            )
        });
        self.page_subs.push(cx.subscribe_in(
            &page,
            window,
            |this, _, ev: &ChangeEvent, window, cx| match ev {
                ChangeEvent::Unauthorized => this.unauthorized(window, cx),
                ChangeEvent::Refreshed => this.server_reachable(cx),
                ChangeEvent::Reply(detail) => this.open_reply((**detail).clone(), window, cx),
                ChangeEvent::Actions(detail) => this.open_menu((**detail).clone(), window, cx),
                ChangeEvent::ManageFilters { project } => {
                    let paths = match &this.page {
                        Page::Change(p) => Some(p.read(cx).changed_paths()),
                        _ => None,
                    };
                    let context = FiltersContext {
                        project: Some(project.clone()),
                        paths,
                        select: None,
                        start_adding: false,
                    };
                    this.open_filters_dialog(context, window, cx)
                }
                ChangeEvent::OpenChange(n) => this.navigate(Route::Change(*n), window, cx),
                // Like stepping through files: Back leaves the walk.
                ChangeEvent::ReplaceChange(n) => {
                    this.nav.replace(Route::Change(*n));
                    this.show(window, cx)
                }
                // A vote moved on to the next change of the chain.
                ChangeEvent::Advance(n, note) => {
                    this.nav.replace(Route::Change(*n));
                    this.show(window, cx);
                    if let Page::Change(p) = &this.page {
                        let note = note.clone();
                        p.update(cx, |p, cx| p.say(note, cx));
                    }
                }
                ChangeEvent::OpenDiff(route) => {
                    this.navigate(Route::Diff(route.clone()), window, cx)
                }
            },
        ));
        Page::Change(page)
    }

    fn diff_page(
        &mut self,
        session: Session,
        route: DiffRoute,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Page {
        let settings = self.settings.clone();
        let outbox = self.outbox_for(&session, cx);
        let filters = self.filters.clone();
        let syntax = self.syntax_cache(cx);
        let page =
            cx.new(|cx| DiffPage::new(session, settings, filters, syntax, route, outbox, cx));
        self.page_subs.push(cx.subscribe_in(
            &page,
            window,
            |this, _, ev: &DiffPageEvent, window, cx| match ev {
                DiffPageEvent::Unauthorized => this.unauthorized(window, cx),
                DiffPageEvent::Reply(detail) => this.open_reply((**detail).clone(), window, cx),
                DiffPageEvent::Actions(detail) => this.open_menu((**detail).clone(), window, cx),
                // Like PolyGerrit, stepping through files does not stack
                // history entries: Back from any file leads to the change.
                DiffPageEvent::UpToChange => this.go_up(&GoUp, window, cx),
                DiffPageEvent::OpenFile(route) => {
                    this.nav.replace(Route::Diff(route.clone()));
                    this.show(window, cx)
                }
            },
        ));
        Page::Diff(page)
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

    /// Writes the bookmarks of the active server in the background; the
    /// buttons follow once the config file has them. On failure the old
    /// list stays.
    fn save_bookmarks(&mut self, list: Vec<SavedQuery>, cx: &mut Context<Self>) {
        let Some(entry) = self.servers.active_mut() else {
            return;
        };
        let Some(session) = &entry.session else {
            return;
        };
        let backend = session.backend().clone();
        let key = entry.setup.key.clone();
        entry.bookmarks_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { backend.save_bookmarks(&list) })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(saved) => {
                        if let Some(e) = this.servers.get_mut(&key) {
                            e.setup.bookmarks = saved.clone();
                        }
                        if this.servers.active_key() == Some(&key)
                            && let Page::List(page) = &this.page
                        {
                            page.update(cx, |p, cx| p.set_bookmarks(saved, cx));
                        }
                    }
                    Err(e) => log::warn!("saving the bookmarks: {e}"),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn search(&mut self, query: String, window: &mut Window, cx: &mut Context<Self>) {
        if !query.is_empty() && self.session().is_some() {
            self.navigate(Route::Search(query), window, cx);
        }
    }

    fn logged_in(&mut self, outcome: &LoginOutcome, window: &mut Window, cx: &mut Context<Self>) {
        // A server that is known keeps its bookmarks (a new login after the
        // credentials were refused is the usual case).
        let bookmarks = self
            .servers
            .get(&outcome.key)
            .map(|e| e.setup.bookmarks.clone())
            .unwrap_or_default();
        let setup = ServerSetup {
            key: outcome.key.clone(),
            name: server_name_from_url(&outcome.base_url),
            base_url: outcome.base_url.clone(),
            username: outcome.username.clone(),
            color: None,
            server: outcome.server,
            queries: outcome.queries.clone(),
            bookmarks,
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
                self.servers.upsert(setup);
                let key = outcome.key.clone();
                self.install_session(&key, Some(session), window, cx);
                self.servers.set_connection(&key, Connection::Online);
                if self.servers.set_active(&key) == Some(true) {
                    self.persist_active(cx);
                }
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

    /// The active server refused our credentials: back to the login page,
    /// with the server filled in.
    fn unauthorized(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(key) = self.servers.active_key().cloned() {
            self.unauthorized_for(&key, window, cx);
        }
    }

    /// Server `key` refused our credentials. It loses its session (so
    /// nothing more is sent with them) and is marked; if it is the active
    /// one, the login page opens with the server filled in.
    fn unauthorized_for(&mut self, key: &ServerKey, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.servers.active_key() == Some(key);
        if active {
            self.drop_reply(cx);
            self.drop_menu();
            self.drop_filters_dialog();
        }
        let session = self.servers.get(key).and_then(|e| e.session.clone());
        self.servers.set_connection(key, Connection::Unauthorized);
        if let Some(s) = session {
            self.install_session(key, None, window, cx);
            if active {
                self.prefill = Some(LoginPrefill {
                    server: Some(s.key),
                    base_url: s.base_url,
                    username: s.username,
                    notice: Some(
                        "The server rejected the saved credentials. Please log in again.".into(),
                    ),
                });
            }
        }
        if active {
            self.nav.reset(Route::Login);
            self.show(window, cx);
        }
    }

    // ---- reply dialog ----

    /// Opens the reply dialog for `detail` on top of the current page.
    fn open_reply(&mut self, detail: ChangeDetail, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(session), Some(outbox)) =
            (self.session().cloned(), self.active_outbox().cloned())
        else {
            return;
        };
        if self.reply.is_some() || self.menu.is_some() || self.filters_dialog.is_some() {
            return;
        }
        let dialog = cx.new(|cx| ReplyDialog::new(session, outbox, detail, window, cx));
        self.reply_subs.push(cx.subscribe_in(
            &dialog,
            window,
            |this, _, ev: &ReplyEvent, window, cx| match ev {
                ReplyEvent::Close => this.close_reply(window, cx),
                ReplyEvent::Unauthorized => this.unauthorized(window, cx),
                ReplyEvent::Sent(sent) => this.reply_sent(sent, window, cx),
            },
        ));
        self.reply = Some(dialog);
        cx.notify();
    }

    /// Takes the dialog away, keeping what was typed as a draft. For
    /// everything that replaces the page under it.
    fn drop_reply(&mut self, cx: &mut Context<Self>) {
        self.reply_subs.clear();
        if let Some(dialog) = self.reply.take() {
            dialog.update(cx, |d, cx| d.keep_text(cx));
        }
    }

    fn close_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reply_subs.clear();
        self.reply = None;
        let handle = self.page.focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    /// The review went out: close the dialog and let the page show the
    /// result.
    fn reply_sent(&mut self, sent: &Sent, window: &mut Window, cx: &mut Context<Self>) {
        self.close_reply(window, cx);
        match &self.page {
            Page::Change(p) => p.update(cx, |p, cx| p.reply_sent(sent, cx)),
            Page::Diff(p) => p.update(cx, |p, cx| p.reply_sent(sent, cx)),
            _ => {}
        }
    }

    // ---- change action menu ----

    /// Opens the action menu for `detail` on top of the current page.
    fn open_menu(&mut self, detail: ChangeDetail, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session().cloned() else {
            return;
        };
        if self.menu.is_some() || self.reply.is_some() || self.filters_dialog.is_some() {
            return;
        }
        let dialog = cx.new(|cx| ActionDialog::new(session, detail, window, cx));
        self.menu_subs.push(cx.subscribe_in(
            &dialog,
            window,
            |this, _, ev: &ActionEvent, window, cx| match ev {
                ActionEvent::Close => this.close_menu(window, cx),
                ActionEvent::Unauthorized => this.unauthorized(window, cx),
                ActionEvent::Done(done) => this.action_done(done, window, cx),
                ActionEvent::Refreshed(detail) => match &this.page {
                    Page::Change(p) => p.update(cx, |p, cx| p.detail_refreshed(detail, cx)),
                    Page::Diff(p) => p.update(cx, |p, cx| p.detail_refreshed(detail, cx)),
                    _ => {}
                },
            },
        ));
        self.menu = Some(dialog);
        cx.notify();
    }

    /// Takes the menu away for everything that replaces the page under it.
    fn drop_menu(&mut self) {
        self.menu_subs.clear();
        self.menu = None;
    }

    fn close_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drop_menu();
        let handle = self.page.focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    /// The server did the action: close the menu and let the page show the
    /// result.
    fn action_done(&mut self, done: &Done, window: &mut Window, cx: &mut Context<Self>) {
        self.close_menu(window, cx);
        if let Some(new) = done.opened {
            // A revert or cherry-pick made another change: go there (Back
            // returns to this one). Its detail is in the cache already.
            self.navigate(Route::Change(new), window, cx);
            if let Page::Change(p) = &self.page {
                let note = done.note();
                p.update(cx, |p, cx| p.say(note, cx));
            }
            return;
        }
        match &self.page {
            Page::Change(p) => p.update(cx, |p, cx| p.action_done(done, cx)),
            Page::Diff(p) => {
                if let Some(route) = p.update(cx, |p, cx| p.action_done(done, cx)) {
                    // A new current patch set: the same file there. The page
                    // is built again from the cache the action refreshed.
                    self.nav.replace(Route::Diff(route));
                    self.show(window, cx);
                    if let Page::Diff(p) = &self.page {
                        let note = format!("{}; showing the new patchset", done.note());
                        p.update(cx, |p, cx| p.say(note, cx));
                    }
                }
            }
            _ => {}
        }
    }

    // ---- manage filters dialog ----

    /// Opens the Manage filters dialog on top of the current page. The
    /// change page passes its project and files (for the match preview); the
    /// settings page passes neither, and may name the rule to start on.
    pub(crate) fn open_filters_dialog(
        &mut self,
        context: FiltersContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.reply.is_some() || self.menu.is_some() || self.filters_dialog.is_some() {
            return;
        }
        let env = self.env.clone();
        let filters = self.filters.clone();
        let dialog = cx.new(|cx| FiltersDialog::new(env, filters, context, window, cx));
        self.filters_dialog_subs.push(cx.subscribe_in(
            &dialog,
            window,
            |this, _, ev: &FiltersEvent, window, cx| match ev {
                FiltersEvent::Close => this.close_filters_dialog(window, cx),
            },
        ));
        self.filters_dialog = Some(dialog);
        cx.notify();
    }

    /// Takes the dialog away for everything that replaces the page under it.
    fn drop_filters_dialog(&mut self) {
        self.filters_dialog_subs.clear();
        self.filters_dialog = None;
    }

    fn close_filters_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drop_filters_dialog();
        let handle = self.page.focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    // ---- go to a change ----

    /// Opens the change a link from the web UI points at, like the browser
    /// would. The current page stays behind it in the history. A link that
    /// does not parse opens the Go to dialog on it, which says why. Does
    /// nothing before a server is set up.
    pub fn open_link(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session() else {
            log::warn!("not opening {text}: no server is set up");
            return;
        };
        match parse_link(text, &session.base_url) {
            Ok(link) => self.open_change_link(&link, window, cx),
            Err(e) => self.show_goto(text.trim().to_owned(), Some(e), window, cx),
        }
    }

    fn open_change_link(&mut self, link: &ChangeLink, window: &mut Window, cx: &mut Context<Self>) {
        let (route, view) = link.route();
        self.remember_selection(cx);
        self.nav.push(route);
        if view.is_some() {
            self.nav.set_change_view(view);
        }
        self.show(window, cx);
    }

    /// `g`: the dialog, with the first line of the clipboard in it.
    fn open_goto(&mut self, _: &OpenGoto, window: &mut Window, cx: &mut Context<Self>) {
        let text = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .and_then(|t| {
                t.lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .map(str::to_owned)
            })
            .unwrap_or_default();
        self.show_goto(text, None, window, cx);
    }

    fn show_goto(
        &mut self,
        text: String,
        error: Option<LinkError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let modal_open = self.reply.is_some()
            || self.menu.is_some()
            || self.filters_dialog.is_some()
            || self.goto.is_some();
        let Some(base_url) = self.session().map(|s| s.base_url.clone()) else {
            return;
        };
        if modal_open {
            return;
        }
        if self.help {
            self.toggle_help(&ToggleHelp, window, cx);
        }
        let dialog = cx.new(|cx| GotoDialog::new(base_url, text, error, window, cx));
        self.goto_subs.push(cx.subscribe_in(
            &dialog,
            window,
            |this, _, ev: &GotoEvent, window, cx| match ev {
                GotoEvent::Close => this.close_goto(window, cx),
                GotoEvent::Open(link) => {
                    this.drop_goto();
                    this.open_change_link(link, window, cx);
                }
            },
        ));
        self.goto = Some(dialog);
        cx.notify();
    }

    fn drop_goto(&mut self) {
        self.goto_subs.clear();
        self.goto = None;
    }

    fn close_goto(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drop_goto();
        let handle = self.page.focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn goto_for_test(&self) -> Option<Entity<GotoDialog>> {
        self.goto.clone()
    }

    #[cfg(test)]
    pub(crate) fn filters_for_test(&self) -> &Entity<FileFilters> {
        &self.filters
    }

    #[cfg(test)]
    pub(crate) fn filters_dialog_for_test(&self) -> Option<Entity<FiltersDialog>> {
        self.filters_dialog.clone()
    }

    #[cfg(test)]
    pub(crate) fn menu_for_test(&self) -> Option<Entity<ActionDialog>> {
        self.menu.clone()
    }

    #[cfg(test)]
    pub(crate) fn reply_for_test(&self) -> Option<Entity<ReplyDialog>> {
        self.reply.clone()
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
        // Up from a diff lands on its change with the cursor on that file
        // and the same patch sets, whether the change page is behind us in
        // the history or has to be opened.
        let from_diff = match &self.page {
            Page::Diff(p) => Some(p.read(cx).route().change_view()),
            _ => None,
        };
        if self.nav.up() {
            if from_diff.is_some() {
                self.nav.set_change_view(from_diff);
            }
            self.show(window, cx);
        }
    }

    fn open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        if self.session().is_some() {
            self.navigate(Route::Settings, window, cx);
        }
    }

    fn focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        if self.session().is_some() {
            self.search.update(cx, |s, cx| s.focus(window, cx));
        }
    }

    fn blur_search(&mut self, _: &BlurSearch, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.page.focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// The overlay is modal: it takes the focus (so that the page's keys,
    /// including its own Esc, do not fire behind it and Esc reaches
    /// [`Self::dismiss`]), and the page gets it back when it closes.
    fn toggle_help(&mut self, _: &ToggleHelp, window: &mut Window, cx: &mut Context<Self>) {
        self.help = !self.help;
        let target = if self.help {
            self.focus.clone()
        } else {
            self.page.focus_handle(cx)
        };
        window.focus(&target, cx);
        cx.notify();
    }

    fn dismiss(&mut self, _: &Dismiss, window: &mut Window, cx: &mut Context<Self>) {
        if self.help {
            self.toggle_help(&ToggleHelp, window, cx);
        } else {
            // Esc leaves the page: diff to change, change to its list,
            // settings to the dashboard.
            self.go_up(&GoUp, window, cx);
        }
    }

    #[cfg(test)]
    pub(crate) fn help_open_for_test(&self) -> bool {
        self.help
    }

    // ---- rendering ----

    /// The title bar: it holds the navigation, the server and user, the search
    /// box and the help button, so that no extra row is spent on them, and
    /// carries the window buttons when the window is client-decorated. What
    /// is not a control is a drag handle for moving the window.
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        // Controls must not start a window move.
        let no_drag = |d: Div| d.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        let signed_in = self
            .session()
            .filter(|_| *self.nav.current() != Route::Login);
        let content = match signed_in {
            None => h_flex()
                .w_full()
                .px_2()
                .text_sm()
                .font_semibold()
                .child(self.app_name.clone()),
            Some(session) => {
                let who = session
                    .username
                    .clone()
                    .unwrap_or_else(|| "anonymous".into());
                h_flex()
                    .w_full()
                    .h_full()
                    .px_2()
                    .gap_2()
                    .items_center()
                    .child(no_drag(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                Button::new("back")
                                    .label("\u{2190}")
                                    .small()
                                    .disabled(!self.nav.can_go_back())
                                    .on_click(
                                        cx.listener(|this, _, w, cx| this.go_back(&GoBack, w, cx)),
                                    ),
                            )
                            .child(
                                Button::new("forward")
                                    .label("\u{2192}")
                                    .small()
                                    .disabled(!self.nav.can_go_forward())
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.go_forward(&GoForward, w, cx)
                                    })),
                            )
                            .child(
                                Button::new("home")
                                    .label(server_label(&session.base_url))
                                    .small()
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.navigate(Route::Dashboard, w, cx)
                                    })),
                            ),
                    ))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(who),
                    )
                    .children(self.render_draft_indicator(cx))
                    // The free space between the cluster and the search box.
                    .child(div().flex_1().h_full())
                    .child(no_drag(
                        div()
                            .w(px(380.))
                            .flex_none()
                            .key_context(SEARCH_BAR)
                            .on_action(cx.listener(Self::blur_search))
                            .child(Input::new(&self.search).small()),
                    ))
                    .child(div().flex_1().h_full())
                    .child(no_drag(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("settings")
                                    .icon(IconName::Settings)
                                    .small()
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.open_settings(&OpenSettings, w, cx)
                                    })),
                            )
                            .child(Button::new("help").label("?").small().on_click(
                                cx.listener(|this, _, w, cx| this.toggle_help(&ToggleHelp, w, cx)),
                            )),
                    ))
            }
        };
        TitleBar::new().child(content)
    }

    /// The drafts that are not on the server, as the title bar says it;
    /// nothing when there are none.
    fn render_draft_indicator(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let counts = self.active_outbox()?.read(cx).counts();
        let label = draft_label(counts)?;
        let theme = cx.theme().clone();
        let color = if counts.failed > 0 {
            theme.danger
        } else {
            theme.muted_foreground
        };
        Some(
            div()
                .id("drafts-indicator")
                .flex_none()
                .px_1p5()
                .rounded_sm()
                .text_xs()
                .text_color(color)
                .border_1()
                .border_color(color.opacity(0.5))
                .cursor_pointer()
                .hover(|d| d.bg(color.opacity(0.12)))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|this, _, _, cx| this.retry_drafts(cx)))
                .child(label),
        )
    }

    /// The indicator's click: try the drafts that are not on the server
    /// again.
    fn retry_drafts(&mut self, cx: &mut Context<Self>) {
        if let Some(outbox) = self.active_outbox().cloned() {
            outbox.update(cx, |o, cx| o.retry_everything(cx));
        }
    }

    fn render_help(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        // Two columns: the first three groups on the left, the rest on the
        // right, so that the whole overlay fits a normal window.
        let mut columns: [Vec<AnyElement>; 2] = [Vec::new(), Vec::new()];
        for (i, g) in actions::help_groups().into_iter().enumerate() {
            let rows = g
                .entries
                .into_iter()
                .filter(|e| !e.keys.is_empty())
                .map(|e| {
                    h_flex()
                        .gap_3()
                        .child(
                            div()
                                .w(px(190.))
                                .flex_none()
                                .font_family(theme.mono_font_family.clone())
                                .child(e.keys),
                        )
                        .child(div().child(e.description))
                })
                .collect::<Vec<_>>();
            columns[usize::from(i >= 3)].push(
                v_flex()
                    .gap_0p5()
                    .pb_3()
                    .child(
                        div()
                            .font_semibold()
                            .text_color(theme.muted_foreground)
                            .child(g.title),
                    )
                    .children(rows)
                    .into_any_element(),
            );
        }
        let [left, right] = columns;
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
                    .max_h_full()
                    .max_w_full()
                    .my_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .text_color(theme.popover_foreground)
                    .text_sm()
                    .child(
                        div()
                            .px_4()
                            .pt_3()
                            .pb_1()
                            .text_base()
                            .font_semibold()
                            .child("Keyboard shortcuts"),
                    )
                    .child(
                        // Two columns of groups; the whole panel scrolls if
                        // the window is too short.
                        v_flex()
                            .relative()
                            .min_h_0()
                            .child(
                                div()
                                    .id("help-scroll")
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .track_scroll(&self.help_scroll)
                                    .px_4()
                                    .py_2()
                                    .child(
                                        h_flex()
                                            .items_start()
                                            .gap_8()
                                            .child(v_flex().children(left))
                                            .child(v_flex().children(right)),
                                    ),
                            )
                            .vertical_scrollbar(&self.help_scroll),
                    )
                    .child(
                        div()
                            .px_4()
                            .pb_3()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Press ? or Esc to close"),
                    ),
            )
    }
}

/// "2 drafts not uploaded", "1 failed" or both.
fn draft_label(counts: sync::DraftCounts) -> Option<String> {
    let mut parts = Vec::new();
    if counts.pending > 0 {
        let plural = if counts.pending == 1 { "" } else { "s" };
        parts.push(format!("{} draft{plural} not uploaded", counts.pending));
    }
    if counts.failed > 0 {
        parts.push(format!("{} failed", counts.failed));
    }
    (!parts.is_empty()).then(|| parts.join(" \u{00B7} "))
}

fn prefill_from(setup: &ServerSetup, notice: String) -> LoginPrefill {
    LoginPrefill {
        server: Some(setup.key.clone()),
        base_url: setup.base_url.clone(),
        username: setup.username.clone(),
        notice: Some(notice),
    }
}

/// Half-width of the resize hit band around the window edge. The band is
/// centered on the edge, so this many pixels of it are inside the window.
const RESIZE_HIT_SIZE: Pixels = px(6.);

impl Render for Shell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        // The border draws the frame and resize edges of a client-decorated
        // window and is transparent otherwise. There is no shadow inset on
        // Linux (see the `window` module docs), so the 1px frame is the
        // window edge and half of the resize hit band lies outside the
        // window: `RESIZE_HIT_SIZE` is the width of the usable half.
        window_border()
            .shadow_size(px(0.))
            .resize_hit_size(RESIZE_HIT_SIZE)
            .child(
                div()
                    .id("shell")
                    .key_context(SHELL)
                    .track_focus(&self.focus)
                    .on_action(cx.listener(Self::go_back))
                    .on_action(cx.listener(Self::go_forward))
                    .on_action(cx.listener(Self::go_up))
                    .on_action(cx.listener(Self::open_settings))
                    .on_action(cx.listener(Self::focus_search))
                    .on_action(cx.listener(Self::open_goto))
                    .on_action(cx.listener(Self::toggle_help))
                    .on_action(cx.listener(Self::dismiss))
                    .relative()
                    .size_full()
                    .flex()
                    .flex_col()
                    .bg(theme.background)
                    .text_color(theme.foreground)
                    .child(self.render_title_bar(cx))
                    .child(div().flex_1().min_h_0().child(self.page.element()))
                    .when(self.help, |d| d.child(self.render_help(cx)))
                    .children(self.reply.clone())
                    .children(self.menu.clone())
                    .children(self.filters_dialog.clone())
                    .children(self.goto.clone()),
            )
    }
}

#[cfg(test)]
mod label_tests {
    use super::draft_label;
    use sync::DraftCounts;

    #[test]
    fn the_indicator_says_nothing_when_all_drafts_are_on_the_server() {
        let label = |pending, failed| draft_label(DraftCounts { pending, failed });
        assert_eq!(label(0, 0), None);
        assert_eq!(label(1, 0).as_deref(), Some("1 draft not uploaded"));
        assert_eq!(label(3, 0).as_deref(), Some("3 drafts not uploaded"));
        assert_eq!(label(0, 2).as_deref(), Some("2 failed"));
        assert_eq!(
            label(1, 2).as_deref(),
            Some("1 draft not uploaded \u{00B7} 2 failed")
        );
    }
}
