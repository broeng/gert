//! The root view: top bar, the current page, history, keyboard help and
//! window-focus tracking.

use std::sync::Arc;

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
use sync::notify::{NotificationSettings, Screen};

use crate::{
    action_menu::{ActionDialog, ActionEvent, Done},
    actions::{self, *},
    attention::{self, Polled},
    filters_dialog::{FiltersContext, FiltersDialog, FiltersEvent},
    goto::{ChangeLink, GotoDialog, GotoEvent, LinkError, LinkServer, resolve_link},
    login::LoginOutcome,
    nav::{ChangeView, DiffRoute, NavState, Route},
    notifier::Notifier,
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
        Env, SavedQuery, ServerConfig, ServerKey, ServerSetup, Session, Startup, StartupPage,
        server_name_from_url,
    },
    settings::{DiffSettings, FileFilters, PageLayout},
    switcher::{ServerSwitcher, SwitcherEvent, SwitcherRow},
    syntax_cache::SyntaxCache,
    watching::{self, Inputs, Ran},
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
    crate::switcher::bind_keys(cx);
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
    /// A link whose server is being added on the login page: opened there
    /// once the login worked.
    pending_link: Option<String>,
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
    /// The server switcher popup while it is open (modal, like the others).
    switcher: Option<Entity<ServerSwitcher>>,
    switcher_subs: Vec<Subscription>,
    /// Shown in the title bar when nobody is signed in.
    app_name: SharedString,
    window_active: bool,
    settings: Entity<DiffSettings>,
    layout: Entity<PageLayout>,
    /// The file filter rules and their session toggles; the pages get it
    /// with the change and diff pages.
    filters: Entity<FileFilters>,
    prefs_task: Option<Task<()>>,
    /// Where desktop notifications go; `None` (the default) means none are
    /// watched for. Set with [`Shell::set_notifications`].
    notifier: Option<Arc<dyn Notifier>>,
    notify_settings: NotificationSettings,
    /// When this run began: events from before it are catch-up.
    started: jiff::Timestamp,
    _subs: Vec<Subscription>,
}

/// What the notification watch does after a pass.
enum WatchStep {
    Done,
    Failed,
    /// The credentials are refused; the watch ends.
    Stop,
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
            pending_link: None,
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
            switcher: None,
            switcher_subs: Vec::new(),
            app_name: SharedString::default(),
            window_active: window.is_window_active(),
            settings: cx.new(|_| DiffSettings::new(diff)),
            layout: cx.new(|_| PageLayout::default()),
            filters: cx.new(|_| FileFilters::new(filters)),
            prefs_task: None,
            notifier: None,
            notify_settings: NotificationSettings::default(),
            started: jiff::Timestamp::now(),
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

    /// Turns on desktop notifications: every signed-in server is watched
    /// for the events `settings` asks for, and `notifier` shows them.
    pub fn set_notifications(
        &mut self,
        notifier: Arc<dyn Notifier>,
        settings: NotificationSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.notifier = Some(notifier);
        self.set_notification_settings(settings, window, cx);
    }

    /// Takes new rules: the watches follow the master switch, and the
    /// running ones read the rules again at their next pass.
    pub(crate) fn set_notification_settings(
        &mut self,
        settings: NotificationSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.notify_settings = settings;
        self.sync_notification_watches(window, cx);
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
        self.nav.reset_for(Some(active), self.start_route());
        self.show(window, cx);
        self.load_diff_preferences(cx);
    }

    /// The page a server opens on when it is shown for the first time (the
    /// `[startup]` setting).
    fn start_route(&self) -> Route {
        match &self.start_page {
            StartupPage::Dashboard => Route::Dashboard,
            StartupPage::Search(q) => Route::Search(q.clone()),
        }
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
            // The poll holds the old session's client.
            entry.attention_task = None;
            entry.watch_task = None;
        }
        self.sync_attention_polls(window, cx);
    }

    /// Starts and stops the background polls so that exactly the servers
    /// that are not shown, are signed in and can talk to the network are
    /// polled. Cheap and idempotent: called whenever one of these changes.
    fn sync_attention_polls(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.servers.active_key().cloned();
        for key in self.servers.keys() {
            let Some(entry) = self.servers.get(&key) else {
                continue;
            };
            let syncer = entry
                .session
                .as_ref()
                .filter(|s| s.can_sync() && s.username.is_some())
                .filter(|_| active.as_ref() != Some(&key))
                .filter(|_| entry.connection != Connection::Unauthorized)
                .map(|s| s.syncer().clone());
            let running = entry.attention_task.is_some();
            let task = match syncer {
                Some(_) if running => continue,
                Some(syncer) => Some(self.attention_poll(key.clone(), syncer, window, cx)),
                None => None,
            };
            if let Some(entry) = self.servers.get_mut(&key) {
                entry.attention_task = task;
            }
        }
        self.sync_notification_watches(window, cx);
    }

    /// Starts and stops the notification watches so that exactly the
    /// servers that are signed in and can talk to the network are watched,
    /// and only while notifications are on. Unlike the attention count this
    /// includes the active server. Cheap and idempotent, like
    /// `sync_attention_polls`, which calls it.
    fn sync_notification_watches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let on = self.notifier.is_some() && self.notify_settings.enabled;
        for key in self.servers.keys() {
            let Some(entry) = self.servers.get(&key) else {
                continue;
            };
            let syncer = entry
                .session
                .as_ref()
                .filter(|s| s.can_sync() && s.username.is_some())
                .filter(|_| on && entry.connection != Connection::Unauthorized)
                .map(|s| s.syncer().clone());
            let task = match syncer {
                Some(_) if entry.watch_task.is_some() => continue,
                Some(syncer) => Some(self.notification_watch(key.clone(), syncer, window, cx)),
                None => None,
            };
            if let Some(entry) = self.servers.get_mut(&key) {
                entry.watch_task = task;
            }
        }
    }

    /// What a pass of server `key` needs from the shell; `None` while
    /// notifications are off or the server is gone.
    fn watch_inputs(&self, key: &ServerKey) -> Option<Inputs> {
        let entry = self.servers.get(key)?;
        self.notify_settings.enabled.then(|| Inputs {
            server: key.as_str().to_owned(),
            settings: self.notify_settings.clone(),
            saved: entry
                .setup
                .queries
                .iter()
                .map(|q| (q.name.clone(), q.query.clone()))
                .collect(),
            me: entry.me.clone(),
        })
    }

    /// The task that watches server `key` for notification events until it
    /// is dropped: one pass now, then every `PollPolicy` interval (the
    /// focused one while the window has the focus, longer after failures).
    /// The rules and saved queries are read again at every pass, so the
    /// task only idles while notifications are off.
    fn notification_watch(
        &mut self,
        key: ServerKey,
        syncer: sync::Syncer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let policy = sync::PollPolicy::default();
        cx.spawn_in(window, async move |this, cx| {
            let mut failures = 0u32;
            loop {
                let Ok(inputs) = this.update(cx, |this, _| this.watch_inputs(&key)) else {
                    return;
                };
                let ran = match inputs {
                    Some(inputs) => {
                        let syncer = syncer.clone();
                        Some(
                            cx.background_spawn(async move {
                                watching::run(&syncer, &inputs, true).await
                            })
                            .await,
                        )
                    }
                    None => None,
                };
                let step = this.update_in(cx, |this, window, cx| {
                    let step = match ran {
                        Some(ran) => this.watched(&key, ran, window, cx),
                        None => WatchStep::Done,
                    };
                    (step, this.window_active)
                });
                let Ok((step, focused)) = step else { return };
                failures = match step {
                    WatchStep::Done => 0,
                    WatchStep::Failed => failures.saturating_add(1),
                    WatchStep::Stop => return,
                };
                let state = sync::PollState {
                    last_attempt: None,
                    window_focused: focused,
                    visible: true,
                    consecutive_failures: failures,
                };
                cx.background_executor()
                    .timer(policy.interval(&state))
                    .await;
            }
        })
    }

    /// The result of a pass over server `key`.
    fn watched(
        &mut self,
        key: &ServerKey,
        ran: Result<Option<Ran>, sync::Error>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> WatchStep {
        match ran {
            Ok(Some(ran)) => {
                self.found_events(key, ran, cx);
                WatchStep::Done
            }
            Ok(None) => WatchStep::Done,
            Err(e) if e.is_unauthorized() => {
                self.unauthorized_for(key, window, cx);
                WatchStep::Stop
            }
            Err(e) => {
                log::debug!("watching {key} for notifications: {e}");
                WatchStep::Failed
            }
        }
    }

    /// Shows what a refresh of one change of server `key` (the change page,
    /// an action) queued, without waiting for the next pass.
    fn drain_notifications(&mut self, key: &ServerKey, cx: &mut Context<Self>) {
        let Some(entry) = self.servers.get(key).filter(|e| e.watch_task.is_some()) else {
            return;
        };
        let (Some(session), Some(inputs)) = (entry.session.as_ref(), self.watch_inputs(key)) else {
            return;
        };
        let syncer = session.syncer().clone();
        let key = key.clone();
        cx.spawn(async move |this, cx| {
            let ran = cx
                .background_spawn(async move { watching::run(&syncer, &inputs, false).await })
                .await;
            this.update(cx, |this, cx| match ran {
                Ok(Some(ran)) => this.found_events(&key, ran, cx),
                Ok(None) => {}
                Err(e) => log::debug!("collecting notification events of {key}: {e}"),
            })
            .ok();
        })
        .detach();
    }

    /// Decides what to show for the events a pass of server `key` found,
    /// shows it and acknowledges every event the pass saw.
    fn found_events(&mut self, key: &ServerKey, ran: Ran, cx: &mut Context<Self>) {
        let Ran { me, found } = ran;
        let current = self.current_change();
        let Some(entry) = self.servers.get_mut(key) else {
            return;
        };
        entry.me = Some(me);
        let Some(syncer) = entry.session.as_ref().map(|s| s.syncer().clone()) else {
            return;
        };
        if found.seen.is_empty() {
            return;
        }
        // Acknowledged below; one that is still in flight is not shown again.
        if entry.notified.len() > 5_000 {
            entry.notified.clear();
        }
        let matched: Vec<_> = found
            .matched
            .into_iter()
            .filter(|(event, _)| entry.notified.insert(event.key()))
            .collect();
        let screen = Screen {
            focused: self.window_active,
            current: current.as_ref().map(|(k, n)| (k.as_str(), *n)),
        };
        // The zone is read once by jiff and cached.
        let now = jiff::Zoned::now().time();
        let plan = sync::notify::plan(
            &self.notify_settings,
            key.as_str(),
            matched,
            &screen,
            now,
            self.started,
        );
        if let Some(notifier) = &self.notifier {
            for notification in &plan.notifications {
                notifier.show(notification, Box::new(|| {}));
            }
        }
        // The inbox will keep `plan.delivered`, suppressed ones included.
        log::debug!(
            "{key}: {} notification events, {} shown",
            plan.delivered.len(),
            plan.notifications.len()
        );
        let seen = found.seen;
        cx.background_spawn(async move {
            if let Err(e) = syncer.ack_events(&seen) {
                log::warn!("acknowledging notification events failed: {e}");
            }
        })
        .detach();
    }

    /// The server and change of the change or diff page on screen.
    fn current_change(&self) -> Option<(ServerKey, ChangeNumber)> {
        let key = self.nav.current_server()?.clone();
        match self.nav.current() {
            Route::Change(n) => Some((key, *n)),
            Route::Diff(d) => Some((key, d.change)),
            _ => None,
        }
    }

    /// The task that polls the attention count of server `key` until it is
    /// dropped: once now, then every `PollPolicy::background` interval
    /// (longer after failures). It starts from the count in the cache.
    fn attention_poll(
        &mut self,
        key: ServerKey,
        syncer: sync::Syncer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let policy = sync::PollPolicy::background();
        cx.spawn_in(window, async move |this, cx| {
            let known = {
                let syncer = syncer.clone();
                cx.background_spawn(async move { attention::cached_count(&syncer) })
                    .await
            };
            let stored = this.update(cx, |this, cx| {
                if let Some(e) = this.servers.get_mut(&key) {
                    e.attention_count = e.attention_count.or(known);
                    cx.notify();
                }
            });
            if stored.is_err() {
                return;
            }
            let mut failures = 0u32;
            loop {
                let polled = {
                    let syncer = syncer.clone();
                    cx.background_spawn(async move { attention::poll(&syncer).await })
                        .await
                };
                failures = if polled.failed() {
                    failures.saturating_add(1)
                } else {
                    0
                };
                let stop = this.update_in(cx, |this, window, cx| {
                    this.attention_polled(&key, polled, window, cx)
                });
                if stop.unwrap_or(true) {
                    return;
                }
                let state = sync::PollState {
                    last_attempt: None,
                    window_focused: true,
                    visible: true,
                    consecutive_failures: failures,
                };
                cx.background_executor()
                    .timer(policy.interval(&state))
                    .await;
            }
        })
    }

    /// The result of a background poll of server `key`. True when polling
    /// should stop (the credentials are refused).
    fn attention_polled(
        &mut self,
        key: &ServerKey,
        polled: Polled,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match polled {
            Polled::Count(n) => {
                if let Some(e) = self.servers.get_mut(key) {
                    e.attention_count = Some(n);
                }
                self.server_reachable(key, cx);
            }
            // It answered, so it is reachable even without a result.
            Polled::Refused => self.server_reachable(key, cx),
            Polled::Offline => self.servers.set_connection(key, Connection::Offline),
            Polled::Unauthorized => {
                self.unauthorized_for(key, window, cx);
                return true;
            }
        }
        cx.notify();
        false
    }

    /// A refresh of server `key` worked, so it is reachable: upload its
    /// drafts that waited.
    fn server_reachable(&mut self, key: &ServerKey, cx: &mut Context<Self>) {
        self.servers.set_connection(key, Connection::Online);
        if let Some(outbox) = self.servers.get(key).and_then(|e| e.outbox.clone()) {
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

    /// Reads the cached diff preferences of the signed-in user of the
    /// active server right away, then refreshes them from the server in the
    /// background (a session without credentials yet reads the cache only).
    /// Anonymous sessions have none, so only the local settings apply.
    fn load_diff_preferences(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session().cloned() else {
            return;
        };
        let key = session.key.clone();
        self.prefs_task = Some(cx.spawn(async move |this, cx| {
            let syncer = session.syncer().clone();
            let cached = {
                let syncer = syncer.clone();
                cx.background_spawn(async move { syncer.cached_diff_preferences() })
                    .await
            };
            match cached {
                Ok(c) => {
                    this.update(cx, |this, cx| {
                        this.set_server_prefs(&key, c.map(|c| c.preferences), cx)
                    })
                    .ok();
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
                    this.update(cx, |this, cx| {
                        this.set_server_prefs(&session.key, Some(prefs), cx)
                    })
                    .ok();
                }
                Ok(None) => {}
                Err(e) => log::warn!("refreshing diff preferences: {e}"),
            }
        }));
    }

    /// The diff preferences of server `key` arrived. They are remembered
    /// with the server, and shown by the shared settings if it is the
    /// active one (a late answer for a server we switched away from must
    /// not change what the pages of the new one use).
    fn set_server_prefs(
        &mut self,
        key: &ServerKey,
        prefs: Option<model::DiffPreferences>,
        cx: &mut Context<Self>,
    ) {
        if let Some(entry) = self.servers.get_mut(key) {
            entry.diff_prefs = prefs.clone();
        }
        if self.servers.active_key() == Some(key) {
            self.settings.update(cx, |s, cx| s.set_server(prefs, cx));
        }
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
        self.nav.push_to(Some(setup.key.clone()), Route::Login);
        self.show(window, cx);
    }

    #[cfg(test)]
    pub(crate) fn active_for_test(&self) -> Option<&ServerKey> {
        self.servers.active_key()
    }

    #[cfg(test)]
    pub(crate) fn trail_for_test(&self) -> (Vec<(Option<String>, Route)>, usize) {
        (self.nav.trail(), self.nav.position())
    }

    #[cfg(test)]
    pub(crate) fn login_page_for_test(&self) -> Option<Entity<LoginPage>> {
        match &self.page {
            Page::Login(p) => Some(p.clone()),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn server_keys_for_test(&self) -> Vec<String> {
        self.servers
            .keys()
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn server_url_for_test(&self, key: &str) -> Option<(String, Option<String>)> {
        self.servers
            .get(&ServerKey::new(key))
            .map(|e| (e.setup.base_url.clone(), e.setup.username.clone()))
    }

    #[cfg(test)]
    pub(crate) fn server_name_for_test(&self, key: &str) -> Option<String> {
        self.servers
            .get(&ServerKey::new(key))
            .map(|e| e.setup.name.clone())
    }

    #[cfg(test)]
    pub(crate) fn unauthorized_for_test(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.unauthorized_for(&ServerKey::new(key), window, cx);
    }

    #[cfg(test)]
    pub(crate) fn navigate_for_test(
        &mut self,
        route: Route,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate(route, window, cx);
    }

    #[cfg(test)]
    pub(crate) fn back_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.go_back(&GoBack, window, cx);
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
        self.adopt_nav_server(cx);
        self.page_subs.clear();
        self.drop_reply(cx);
        self.drop_menu();
        self.drop_filters_dialog();
        self.drop_goto();
        self.drop_switcher();
        let mut route = self.nav.current().clone();
        if self.session().is_none() && route != Route::Login {
            // The login page goes on top of the history instead of
            // replacing it, so that the other servers' pages stay.
            self.nav.push(Route::Login);
            route = Route::Login;
        }
        if route != Route::Login {
            // A link waiting for its server to be added is forgotten once
            // the user leaves the login page without adding it.
            self.pending_link = None;
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
        // The shown server may have changed.
        self.sync_attention_polls(window, cx);
        cx.notify();
    }

    /// Makes the server of the page the history is on the active one. Back,
    /// forward, up and the jump actions all end in `show`, so the server
    /// follows the page whichever way we got there.
    fn adopt_nav_server(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.nav.current_server().cloned() else {
            return;
        };
        if self.servers.set_active(&key) != Some(true) {
            return;
        }
        self.persist_active(cx);
        // The preferences of the server we left must not linger.
        let prefs = self.servers.get(&key).and_then(|e| e.diff_prefs.clone());
        self.settings.update(cx, |s, cx| s.set_server(prefs, cx));
        self.load_diff_preferences(cx);
        let setup = self
            .servers
            .get(&key)
            .filter(|e| e.session.is_none())
            .map(|e| e.setup.clone());
        if let Some(setup) = setup {
            self.prefill = Some(prefill_from(
                &setup,
                "The saved credentials of this server are not available. Please log in again."
                    .into(),
            ));
        }
    }

    /// Shows server `key`: its own last page, or its start page on the
    /// first visit in this session. The page we leave stays behind in the
    /// one shared history, so Back returns to it (and to its server).
    /// Does nothing for an unknown server or the active one.
    pub(crate) fn switch_to(
        &mut self,
        key: &ServerKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.servers.get(key).is_none() || self.servers.active_key() == Some(key) {
            return;
        }
        self.remember_selection(cx);
        let start = self.start_route();
        self.nav.visit(key, start);
        self.show(window, cx);
    }

    /// Opens `route` on server `key`, switching to it if it is not the
    /// active one. (`navigate` stays on the active server.) `view` is the
    /// state to give a change page.
    pub(crate) fn go_to(
        &mut self,
        key: &ServerKey,
        route: Route,
        view: Option<ChangeView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.servers.get(key).is_none() {
            return;
        }
        self.remember_selection(cx);
        self.nav.push_to(Some(key.clone()), route);
        if view.is_some() {
            self.nav.set_change_view(view);
        }
        self.show(window, cx);
    }

    /// `ctrl-1` .. `ctrl-9`: the nth server in config order.
    fn switch_server(
        &mut self,
        action: &SwitchServer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(key) = self.servers.keys().get(action.position).cloned() {
            self.switch_to(&key, window, cx);
        }
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
        let owner = session.key.clone();
        let page = cx.new(|cx| ChangeListPage::new(session, kind, specs, restore, active, cx));
        let bookmarks = self.bookmarks();
        page.update(cx, |p, cx| p.set_bookmarks(bookmarks, cx));
        self.page_subs.push(cx.subscribe_in(
            &page,
            window,
            move |this, page, ev: &ListEvent, window, cx| match ev {
                ListEvent::Open(n) => {
                    this.nav.set_selected(page.read(cx).selected_number());
                    this.navigate(Route::Change(*n), window, cx);
                }
                ListEvent::Search(q) => this.search(q.clone(), window, cx),
                ListEvent::SaveBookmarks(list) => this.save_bookmarks(list.clone(), cx),
                ListEvent::Unauthorized => this.unauthorized_for(&owner, window, cx),
                ListEvent::Refreshed => this.server_reachable(&owner, cx),
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
        let (servers, active) = (self.servers.rows(), self.servers.active_key().cloned());
        let page = cx.new(|cx| {
            SettingsPage::new(
                env, server, servers, active, queries, bookmarks, start, layout, filters, window,
                cx,
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
                SettingsEvent::AddServer => {
                    this.open_add_server(String::new(), None, None, window, cx)
                }
                SettingsEvent::ServersSaved { servers, relogin } => {
                    this.servers_saved(servers, relogin.as_deref(), window, cx)
                }
                SettingsEvent::ServerRemoved { key } => this.server_removed(key, window, cx),
                // What a test found is the freshest word on the server.
                SettingsEvent::ConnectionTested { key, connection } => {
                    this.servers.set_connection(key, *connection);
                    cx.notify();
                }
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
        let owner = session.key.clone();
        let badge = self.servers.badge(&owner);
        let page = cx.new(|cx| {
            let mut page = ChangePage::new(
                session, settings, layout, filters, syntax, number, view, active, outbox, cx,
            );
            page.set_server_badge(badge, cx);
            page
        });
        self.page_subs.push(cx.subscribe_in(
            &page,
            window,
            move |this, _, ev: &ChangeEvent, window, cx| match ev {
                ChangeEvent::Unauthorized => this.unauthorized_for(&owner, window, cx),
                ChangeEvent::Refreshed => {
                    this.server_reachable(&owner, cx);
                    this.drain_notifications(&owner, cx);
                }
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
                        ..Default::default()
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
        let owner = session.key.clone();
        let badge = self.servers.badge(&owner);
        let page = cx.new(|cx| {
            let mut page = DiffPage::new(session, settings, filters, syntax, route, outbox, cx);
            page.set_server_badge(badge, cx);
            page
        });
        self.page_subs.push(cx.subscribe_in(
            &page,
            window,
            move |this, _, ev: &DiffPageEvent, window, cx| match ev {
                DiffPageEvent::Unauthorized => this.unauthorized_for(&owner, window, cx),
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
        // A server that is known keeps its name, color and bookmarks (a new
        // login after the credentials were refused is the usual case).
        let known = self.servers.get(&outcome.key).map(|e| e.setup.clone());
        let is_new = known.is_none();
        let setup = ServerSetup {
            key: outcome.key.clone(),
            name: known
                .as_ref()
                .map_or_else(|| outcome.name.clone(), |k| k.name.clone()),
            base_url: outcome.base_url.clone(),
            username: outcome.username.clone(),
            color: known.as_ref().and_then(|k| k.color.clone()),
            server: outcome.server,
            queries: outcome.queries.clone(),
            bookmarks: known.map(|k| k.bookmarks).unwrap_or_default(),
        };
        let auth = match (&outcome.username, &outcome.password) {
            (Some(username), Some(password)) => Auth::Basic {
                username: username.clone(),
                password: password.clone(),
            },
            _ => Auth::Anonymous,
        };
        let session = match Session::new(&self.env, &setup, Some(auth)) {
            Ok(session) => session,
            Err(e) => {
                self.prefill = Some(LoginPrefill {
                    server: (!is_new).then_some(setup.key.clone()),
                    ..prefill_from(&setup, e.to_string())
                });
                return self.show(window, cx);
            }
        };
        let key = outcome.key.clone();
        self.servers.upsert(setup);
        self.install_session(&key, Some(session), window, cx);
        self.servers.set_connection(&key, Connection::Online);
        if self.servers.set_active(&key) == Some(true) {
            self.persist_active(cx);
            // The preferences of the server we left must not linger.
            let prefs = self.servers.get(&key).and_then(|e| e.diff_prefs.clone());
            self.settings.update(cx, |s, cx| s.set_server(prefs, cx));
        }
        if is_new {
            self.save_server_names(&outcome.base_url, &outcome.name, cx);
        }
        // The link that led to adding the server opens there, else the
        // dashboard. The login page was a history entry of its own: it
        // turns into that page, and the history before it stays.
        let (route, view, key) = self
            .pending_link
            .take()
            .and_then(|text| {
                resolve_link(&text, &self.link_servers(), Some(&key))
                    .ok()
                    .map(|(k, link)| {
                        let (route, view) = link.route();
                        (route, view, k)
                    })
            })
            .unwrap_or((Route::Dashboard, None, key));
        self.nav.replace_to(Some(key), route);
        if view.is_some() {
            self.nav.set_change_view(view);
        }
        self.show(window, cx);
        self.load_diff_preferences(cx);
    }

    /// The settings page wrote the server list: take over the order, the
    /// names and the colors, and the new session if the URL or user changed
    /// (the page already stored the password under the new account).
    fn servers_saved(
        &mut self,
        list: &[ServerConfig],
        relogin: Option<&LoginOutcome>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.servers.apply_list(list);
        if let Some(outcome) = relogin {
            self.relogin(outcome, window, cx);
        }
        cx.notify();
    }

    /// A configured server logged in with a new URL or user (settings): it
    /// keeps its place, name, color and lists, and gets a session for the
    /// new login. The cache row is the same one, now at the new URL.
    fn relogin(&mut self, outcome: &LoginOutcome, window: &mut Window, cx: &mut Context<Self>) {
        let Some(known) = self.servers.get(&outcome.key).map(|e| e.setup.clone()) else {
            return;
        };
        let setup = ServerSetup {
            base_url: outcome.base_url.clone(),
            username: outcome.username.clone(),
            server: outcome.server,
            ..known
        };
        let auth = match (&outcome.username, &outcome.password) {
            (Some(username), Some(password)) => Auth::Basic {
                username: username.clone(),
                password: password.clone(),
            },
            _ => Auth::Anonymous,
        };
        let key = outcome.key.clone();
        let session = match Session::new(&self.env, &setup, Some(auth)) {
            Ok(session) => Some(session),
            Err(e) => {
                log::warn!("server {key}: invalid configuration: {e}");
                None
            }
        };
        let ok = session.is_some();
        self.servers.upsert(setup);
        self.install_session(&key, session, window, cx);
        self.servers.set_connection(
            &key,
            if ok {
                Connection::Online
            } else {
                Connection::Offline
            },
        );
        if self.servers.active_key() == Some(&key) {
            self.load_diff_preferences(cx);
        }
    }

    /// The settings page removed server `key` (its cache, password and
    /// config entry are gone): forget it here too. If it was the active
    /// one, the first remaining server takes over, or, with none left, the
    /// login page opens to add one.
    fn server_removed(&mut self, key: &ServerKey, window: &mut Window, cx: &mut Context<Self>) {
        let was_active = self.servers.active_key() == Some(key);
        self.servers.remove(key);
        self.nav.forget_server(key);
        if self
            .prefill
            .as_ref()
            .is_some_and(|p| p.server.as_ref() == Some(key))
        {
            self.prefill = None;
        }
        if !was_active {
            return cx.notify();
        }
        self.drop_reply(cx);
        self.drop_menu();
        self.drop_filters_dialog();
        if let Some(first) = self.servers.keys().first().cloned() {
            let start = self.start_route();
            self.nav.visit(&first, start);
        } else {
            self.pending_link = None;
        }
        self.show(window, cx);
    }

    /// Writes the servers to the config with the name the user gave the one
    /// just added. The login saved it under its host name, so nothing is
    /// needed when they are the same.
    fn save_server_names(&mut self, base_url: &str, name: &str, cx: &mut Context<Self>) {
        if name == server_name_from_url(base_url) {
            return;
        }
        let configs: Vec<ServerConfig> = self
            .servers
            .keys()
            .into_iter()
            .filter_map(|key| {
                let setup = &self.servers.get(&key)?.setup;
                Some(ServerConfig {
                    key,
                    name: setup.name.clone(),
                    url: setup.base_url.clone(),
                    username: setup.username.clone(),
                    color: setup.color.clone(),
                })
            })
            .collect();
        let backend = self.env.backend.clone();
        cx.background_spawn(async move {
            if let Err(e) = backend.save_servers(&configs) {
                log::warn!("saving the server name: {e}");
            }
        })
        .detach();
    }

    // ---- adding a server ----

    /// Opens the login page for a server that is not configured yet, with
    /// `url` filled in. Nothing is sent to that address until the user logs
    /// in. A successful login makes the new server the active one and then
    /// opens `link` (a Go to text) there, if there is one. The page is
    /// pushed on the history, so Back returns to where the user was.
    pub(crate) fn open_add_server(
        &mut self,
        url: String,
        link: Option<String>,
        notice: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let notice = notice.or_else(|| {
            link.as_ref().map(|_| {
                format!(
                    "None of your servers is at {url}. Log in to add it; nothing has been \
                     sent to it yet."
                )
            })
        });
        self.remember_selection(cx);
        self.prefill = Some(LoginPrefill {
            server: None,
            base_url: url,
            name: None,
            username: None,
            notice,
        });
        self.pending_link = link;
        let server = self.servers.active_key().cloned();
        self.nav.push_to(server, Route::Login);
        self.show(window, cx);
    }

    /// The add-server action: the login page for a new server, empty.
    fn add_server(&mut self, _: &AddServer, window: &mut Window, cx: &mut Context<Self>) {
        if self.reply.is_some()
            || self.menu.is_some()
            || self.filters_dialog.is_some()
            || self.goto.is_some()
        {
            return;
        }
        self.open_add_server(String::new(), None, None, window, cx);
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
                    name: None,
                    username: s.username,
                    notice: Some(
                        "The server rejected the saved credentials. Please log in again.".into(),
                    ),
                });
            }
        }
        if active {
            self.nav.push_to(Some(key.clone()), Route::Login);
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
        let owner = session.key.clone();
        let badge = self.servers.badge(&owner);
        let dialog = cx.new(|cx| {
            let mut dialog = ReplyDialog::new(session, outbox, detail, window, cx);
            dialog.set_server_badge(badge, cx);
            dialog
        });
        self.reply_subs.push(cx.subscribe_in(
            &dialog,
            window,
            move |this, _, ev: &ReplyEvent, window, cx| match ev {
                ReplyEvent::Close => this.close_reply(window, cx),
                ReplyEvent::Unauthorized => this.unauthorized_for(&owner, window, cx),
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
        let owner = session.key.clone();
        let badge = self.servers.badge(&owner);
        let dialog = cx.new(|cx| {
            let mut dialog = ActionDialog::new(session, detail, window, cx);
            dialog.set_server_badge(badge, cx);
            dialog
        });
        self.menu_subs.push(cx.subscribe_in(
            &dialog,
            window,
            move |this, _, ev: &ActionEvent, window, cx| match ev {
                ActionEvent::Close => this.close_menu(window, cx),
                ActionEvent::Unauthorized => this.unauthorized_for(&owner, window, cx),
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
        if let Some(key) = self.servers.active_key().cloned() {
            self.drain_notifications(&key, cx);
        }
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
        let context = FiltersContext {
            servers: self
                .servers
                .rows()
                .into_iter()
                .map(|r| (r.key, r.name))
                .collect(),
            server: self.servers.active_key().cloned(),
            ..context
        };
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
    /// would, on whichever configured server the link is for (switching to
    /// it). The current page stays behind it in the history. A link that
    /// does not parse opens the Go to dialog on it, which says why; one for
    /// a server that is not configured offers to add it there. With no
    /// server at all the offer is the login page.
    pub fn open_link(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let servers = self.link_servers();
        match resolve_link(text, &servers, self.servers.active_key()) {
            Ok((key, link)) => self.open_change_link(&key, &link, window, cx),
            Err(LinkError::UnknownServer(url)) if servers.is_empty() => {
                self.open_add_server(url, Some(text.trim().to_owned()), None, window, cx)
            }
            Err(e) if servers.is_empty() => log::warn!("not opening {text}: {e}"),
            Err(e) => self.show_goto(text.trim().to_owned(), Some(e), window, cx),
        }
    }

    /// The configured servers, as links see them.
    fn link_servers(&self) -> Vec<LinkServer> {
        self.servers
            .keys()
            .into_iter()
            .filter_map(|key| {
                let base_url = self.servers.get(&key)?.setup.base_url.clone();
                Some(LinkServer { key, base_url })
            })
            .collect()
    }

    fn open_change_link(
        &mut self,
        key: &ServerKey,
        link: &ChangeLink,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (route, view) = link.route();
        self.go_to(key, route, view, window, cx);
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
            || self.goto.is_some()
            || self.switcher.is_some();
        let servers = self.link_servers();
        let active = self.servers.active_key().cloned();
        if modal_open || servers.is_empty() {
            return;
        }
        if self.help {
            self.toggle_help(&ToggleHelp, window, cx);
        }
        let dialog = cx.new(|cx| GotoDialog::new(servers, active, text, error, window, cx));
        self.goto_subs.push(cx.subscribe_in(
            &dialog,
            window,
            |this, _, ev: &GotoEvent, window, cx| match ev {
                GotoEvent::Close => this.close_goto(window, cx),
                GotoEvent::Open(key, link) => {
                    this.drop_goto();
                    this.open_change_link(key, link, window, cx);
                }
                GotoEvent::AddServer { url, text } => {
                    this.drop_goto();
                    this.open_add_server(url.clone(), Some(text.clone()), None, window, cx);
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

    // ---- server switcher ----

    /// `ctrl-k`, or a click on the chip: opens the switcher, or closes it if
    /// it is open. Nothing with a single server, or under another dialog.
    fn toggle_server_switcher(
        &mut self,
        _: &ToggleServerSwitcher,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.switcher.is_some() {
            self.close_switcher(window, cx);
            return;
        }
        if !self.servers.several()
            || self.reply.is_some()
            || self.menu.is_some()
            || self.filters_dialog.is_some()
            || self.goto.is_some()
        {
            return;
        }
        if self.help {
            self.toggle_help(&ToggleHelp, window, cx);
        }
        let rows = self.switcher_rows(cx);
        let switcher = cx.new(|cx| ServerSwitcher::new(rows, window, cx));
        self.switcher_subs.push(cx.subscribe_in(
            &switcher,
            window,
            |this, _, ev: &SwitcherEvent, window, cx| match ev {
                SwitcherEvent::Close => this.close_switcher(window, cx),
                SwitcherEvent::Pick(key) => {
                    this.close_switcher(window, cx);
                    this.switch_to(key, window, cx);
                }
                SwitcherEvent::AddServer => this.add_server(&AddServer, window, cx),
            },
        ));
        self.switcher = Some(switcher);
        cx.notify();
    }

    /// The popup's rows: every server in config order with what is known
    /// about it now.
    fn switcher_rows(&self, cx: &App) -> Vec<SwitcherRow> {
        let active = self.servers.active_key();
        self.servers
            .keys()
            .into_iter()
            .enumerate()
            .filter_map(|(position, key)| {
                let entry = self.servers.get(&key)?;
                let badge = self.servers.badge_of(&key)?;
                let setup = &entry.setup;
                let who = setup.username.as_deref().unwrap_or("anonymous");
                let drafts = entry.outbox.as_ref().map_or(0, |o| {
                    let c = o.read(cx).counts();
                    c.pending + c.failed
                });
                Some(SwitcherRow {
                    active: active == Some(&key),
                    detail: format!("{who} \u{00B7} {}", setup.base_url),
                    haystack: format!("{} {} {who}", setup.name, setup.base_url).to_lowercase(),
                    connection: entry.connection,
                    attention: if active == Some(&key) {
                        None
                    } else {
                        entry.attention_count
                    },
                    key,
                    badge,
                    position,
                    drafts,
                })
            })
            .collect()
    }

    fn drop_switcher(&mut self) {
        self.switcher_subs.clear();
        self.switcher = None;
    }

    fn close_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drop_switcher();
        let handle = self.page.focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn chip_shown_for_test(&self, cx: &mut Context<Self>) -> bool {
        self.render_server_chip(cx).is_some()
    }

    /// The number on the chip, if it shows one.
    #[cfg(test)]
    pub(crate) fn chip_badge_for_test(&self) -> Option<String> {
        self.servers.several().then_some(())?;
        crate::switcher::badge_text(self.servers.others_attention())
    }

    #[cfg(test)]
    pub(crate) fn attention_for_test(&self, key: &str) -> Option<usize> {
        self.servers.get(&ServerKey::new(key))?.attention_count
    }

    #[cfg(test)]
    pub(crate) fn remove_server_for_test(
        &mut self,
        key: &ServerKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.server_removed(key, window, cx);
    }

    #[cfg(test)]
    pub(crate) fn set_connection_for_test(&mut self, key: &str, connection: Connection) {
        self.servers
            .set_connection(&ServerKey::new(key), connection);
    }

    #[cfg(test)]
    pub(crate) fn set_window_active_for_test(&mut self, active: bool) {
        self.window_active = active;
    }

    #[cfg(test)]
    pub(crate) fn session_for_test(&self, key: &str) -> Option<&Session> {
        self.servers.get(&ServerKey::new(key))?.session.as_ref()
    }

    /// Whether the notification watch of server `key` runs.
    #[cfg(test)]
    pub(crate) fn watching_for_test(&self, key: &str) -> bool {
        self.servers
            .get(&ServerKey::new(key))
            .is_some_and(|e| e.watch_task.is_some())
    }

    #[cfg(test)]
    pub(crate) fn open_dialogs_for_test(
        &mut self,
        detail: ChangeDetail,
        menu: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if menu {
            self.open_menu(detail, window, cx);
        } else {
            self.open_reply(detail, window, cx);
        }
    }

    #[cfg(test)]
    pub(crate) fn switcher_for_test(&self) -> Option<Entity<ServerSwitcher>> {
        self.switcher.clone()
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
                                Button::new("add-server")
                                    .label("+")
                                    .small()
                                    .tooltip("Add server (Ctrl-Shift-A)")
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.add_server(&AddServer, w, cx)
                                    })),
                            )
                            .child(
                                Button::new("home")
                                    .label(if self.servers.several() {
                                        // The chip names the server already.
                                        "Dashboard".to_string()
                                    } else {
                                        server_label(&session.base_url)
                                    })
                                    .small()
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.navigate(Route::Dashboard, w, cx)
                                    })),
                            )
                            .children(self.render_server_chip(cx)),
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

    /// The active server's chip, with several servers: its dot, name and
    /// a warning if it cannot be used. A click opens the switcher.
    fn render_server_chip(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let key = self.servers.active_key()?;
        let badge = self.servers.badge(key)?;
        let connection = self.servers.get(key)?.connection;
        Some(
            crate::switcher::chip(&badge, connection, self.servers.others_attention(), cx)
                .on_click(cx.listener(|this, _, w, cx| {
                    this.toggle_server_switcher(&ToggleServerSwitcher, w, cx)
                })),
        )
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
        name: None,
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
                    .on_action(cx.listener(Self::switch_server))
                    .on_action(cx.listener(Self::toggle_server_switcher))
                    .on_action(cx.listener(Self::add_server))
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
                    .children(self.goto.clone())
                    .children(self.switcher.clone()),
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
