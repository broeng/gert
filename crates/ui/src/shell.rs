//! The root view: top bar, the current page, history, keyboard help and
//! window-focus tracking.

use std::{collections::HashMap, sync::Arc};

use gerrit_api::Auth;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::window_border;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use model::{ChangeDetail, ChangeNumber};
use sync::notify::{Notification, NotificationSettings};

use crate::{
    action_menu::{ActionDialog, ActionEvent, Done},
    actions::{self, *},
    checkouts::Checkouts,
    consolidation::Request,
    filters_dialog::{FiltersContext, FiltersDialog, FiltersEvent},
    goto::{ChangeLink, GotoDialog, GotoEvent, LinkError, LinkServer, resolve_link},
    inbox::InboxOp,
    login::LoginOutcome,
    nav::{ChangeView, DiffRoute, NavState, Route},
    notifier::Notifier,
    notify_rule_dialog::{NotifyRuleContext, NotifyRuleDialog, NotifyRuleEvent},
    outbox::{DraftOutbox, OutboxEvent},
    pages::{
        change::{ChainHandover, ChangeEvent, ChangePage},
        diff::{DiffPage, DiffPageEvent},
        inbox::{InboxEvent, InboxPage, InboxServer},
        list::{ChangeListPage, ListEvent, ListKind},
        login::{LoginPage, LoginPrefill},
        settings::{SettingsEvent, SettingsPage, SettingsTab},
    },
    project_dialog::{ProjectContext, ProjectDialog, ProjectEvent},
    push_dialog::{PushDialog, PushEvent, PushRequest},
    reply::{ReplyDialog, ReplyEvent, ReplyRequest, Sent},
    servers::{ACTIVE_SERVER_STATE, Connection, Servers},
    session::{
        Env, ProjectSettings, SavedQuery, ServerConfig, ServerKey, ServerSetup, Session, Startup,
        StartupPage, server_name_from_url,
    },
    settings::{DiffSettings, FileFilters, PageLayout},
    switcher::{ServerSwitcher, SwitcherEvent, SwitcherRow},
    syntax_cache::SyntaxCache,
    watching,
};

mod help;
mod local;
mod title_bar;
mod watch;

enum Page {
    Login(Entity<LoginPage>),
    List(Entity<ChangeListPage>),
    Change(Entity<ChangePage>),
    Diff(Entity<DiffPage>),
    Settings(Entity<SettingsPage>),
    Inbox(Entity<InboxPage>),
}

impl Page {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self {
            Page::Login(p) => p.focus_handle(cx),
            Page::List(p) => p.focus_handle(cx),
            Page::Change(p) => p.focus_handle(cx),
            Page::Diff(p) => p.focus_handle(cx),
            Page::Settings(p) => p.focus_handle(cx),
            Page::Inbox(p) => p.focus_handle(cx),
        }
    }

    fn element(&self) -> AnyElement {
        match self {
            Page::Login(p) => p.clone().into_any_element(),
            Page::List(p) => p.clone().into_any_element(),
            Page::Change(p) => p.clone().into_any_element(),
            Page::Diff(p) => p.clone().into_any_element(),
            Page::Settings(p) => p.clone().into_any_element(),
            Page::Inbox(p) => p.clone().into_any_element(),
        }
    }

    fn set_window_active(&self, active: bool, cx: &mut App) {
        match self {
            Page::Login(_) => {}
            Page::List(p) => p.update(cx, |p, cx| p.set_window_active(active, cx)),
            Page::Change(p) => p.update(cx, |p, cx| p.set_window_active(active, cx)),
            Page::Diff(_) | Page::Settings(_) | Page::Inbox(_) => {}
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
    crate::notify_rule_dialog::actions::bind_keys(cx);
    crate::project_dialog::actions::bind_keys(cx);
    crate::push_dialog::actions::bind_keys(cx);
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
    /// The notification rule editor while it is open (modal, like the
    /// others).
    rule_dialog: Option<Entity<NotifyRuleDialog>>,
    rule_dialog_subs: Vec<Subscription>,
    project_dialog: Option<Entity<ProjectDialog>>,
    project_dialog_subs: Vec<Subscription>,
    push_dialog: Option<Entity<PushDialog>>,
    push_dialog_subs: Vec<Subscription>,
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
    /// Hears the clicks on notifications, which arrive on other threads.
    click_task: Option<Task<()>>,
    clicks: Option<futures::channel::mpsc::UnboundedSender<Notification>>,
    notify_settings: NotificationSettings,
    /// The settings tab last shown; the page opens on it again.
    settings_tab: SettingsTab,
    /// When this run began: events from before it are catch-up.
    started: jiff::Timestamp,
    /// The unread notifications of each server, as the bell counts them.
    inbox_unread: HashMap<store::ServerId, usize>,
    /// Changes to the inbox waiting for the worker, which applies them in
    /// order on the background executor (`inbox_busy` while it runs).
    inbox_queue: Vec<InboxOp>,
    inbox_busy: bool,
    /// The page on screen should follow its local change to Gerrit, but
    /// the user was writing (an open editor with text, or a dialog): it
    /// is looked up again once they are done (see `follow_redirect`).
    redirect_pending: bool,
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
            let modal = modal.or_else(|| this.rule_dialog.as_ref().map(|d| d.focus_handle(cx)));
            let modal = modal.or_else(|| this.project_dialog.as_ref().map(|d| d.focus_handle(cx)));
            let modal = modal.or_else(|| this.push_dialog.as_ref().map(|d| d.focus_handle(cx)));
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
            rule_dialog: None,
            rule_dialog_subs: Vec::new(),
            project_dialog: None,
            project_dialog_subs: Vec::new(),
            push_dialog: None,
            push_dialog_subs: Vec::new(),
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
            click_task: None,
            clicks: None,
            notify_settings: NotificationSettings::default(),
            settings_tab: SettingsTab::default(),
            started: jiff::Timestamp::now(),
            inbox_unread: HashMap::new(),
            inbox_queue: Vec::new(),
            inbox_busy: false,
            redirect_pending: false,
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

    /// The local checkouts of server `key` (made on first use), the way the
    /// change and diff pages ask for "the repository of project X on this
    /// server" and the settings page lists the projects.
    pub(crate) fn checkouts(
        &mut self,
        key: &ServerKey,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Checkouts>> {
        let entry = self.servers.get_mut(key)?;
        let projects = &entry.setup.projects;
        Some(
            entry
                .checkouts
                .get_or_insert_with(|| cx.new(|_| Checkouts::new(projects.clone())))
                .clone(),
        )
    }

    /// Whether the user is writing something a new page would drop: a
    /// dialog is open, or a draft editor on the page holds text.
    fn busy_writing(&self, cx: &App) -> bool {
        let dialog = self.reply.is_some()
            || self.menu.is_some()
            || self.filters_dialog.is_some()
            || self.rule_dialog.is_some()
            || self.project_dialog.is_some()
            || self.push_dialog.is_some()
            || self.goto.is_some()
            || self.switcher.is_some();
        dialog
            || match &self.page {
                Page::Change(p) => p.read(cx).is_writing(cx),
                Page::Diff(p) => p.read(cx).is_writing(cx),
                _ => false,
            }
    }

    /// Looks again where the page on screen leads, if a redirect waited
    /// for the user (a dialog closed, a refresh or a run came).
    fn retry_redirect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.redirect_pending) {
            self.follow_redirect(None, window, cx);
        }
    }

    /// A note on the change or diff page on screen (red when `error`).
    fn say_on_page(&mut self, note: String, error: bool, cx: &mut Context<Self>) {
        match &self.page {
            Page::Change(p) if error => p.update(cx, |p, cx| p.say_error(note, cx)),
            Page::Change(p) => p.update(cx, |p, cx| p.say(note, cx)),
            Page::Diff(p) if error => p.update(cx, |p, cx| p.say_error(note, cx)),
            Page::Diff(p) => p.update(cx, |p, cx| p.say(note, cx)),
            _ => {}
        }
    }

    /// Asks the scanner of server `key` to look for local changes (cheap
    /// when nothing moved; see `local_scan`).
    fn scan_local(&mut self, key: &ServerKey, cx: &mut Context<Self>) {
        if let Some(scanner) = self.local_scanner(key, cx) {
            scanner.update(cx, |s, cx| s.request(cx));
        }
    }

    /// Writes the project checkouts of the active server in the background.
    /// Once the config file has them the server's setup and its
    /// [`Checkouts`] follow; on failure the old list stays.
    pub(crate) fn save_projects(&mut self, list: Vec<ProjectSettings>, cx: &mut Context<Self>) {
        let Some(entry) = self.servers.active_mut() else {
            return;
        };
        let Some(session) = &entry.session else {
            return;
        };
        let backend = session.backend().clone();
        let key = entry.setup.key.clone();
        entry.projects_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { backend.save_projects(&list) })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(saved) => {
                        if let Some(e) = this.servers.get_mut(&key) {
                            e.setup.projects = saved.clone();
                            if let Some(c) = &e.checkouts {
                                c.update(cx, |c, cx| c.set_projects(saved, cx));
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("saving the projects: {e}");
                        this.say_error(format!("Could not save the projects: {e}"), cx);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Tells the user of a failed background save on the page shown, the way
    /// the settings tabs do for their own saves. Pages without a hint line
    /// (the others) leave it to the log.
    fn say_error(&mut self, text: String, cx: &mut Context<Self>) {
        match &self.page {
            Page::Settings(p) => p.update(cx, |p, cx| p.say_error(text, cx)),
            Page::Change(p) => p.update(cx, |p, cx| p.say_error(text, cx)),
            Page::Diff(p) => p.update(cx, |p, cx| p.say_error(text, cx)),
            _ => {}
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
        self.inbox_op(InboxOp::Reload, cx);
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
                move |this, _, ev: &OutboxEvent, window, cx| match ev {
                    OutboxEvent::Unauthorized => this.unauthorized_for(&owner, window, cx),
                    OutboxEvent::KeptAsNew(change) => {
                        if this.current_change() == Some((owner.clone(), *change)) {
                            match &this.page {
                                Page::Change(p) => p.update(cx, |p, cx| p.kept_as_new(cx)),
                                Page::Diff(p) => p.update(cx, |p, cx| p.kept_as_new(cx)),
                                _ => {}
                            }
                        }
                    }
                    OutboxEvent::DeleteBeingSent(change) => {
                        if this.current_change() == Some((owner.clone(), *change)) {
                            match &this.page {
                                Page::Change(p) => p.update(cx, |p, cx| p.delete_refused(cx)),
                                Page::Diff(p) => p.update(cx, |p, cx| p.delete_refused(cx)),
                                _ => {}
                            }
                        }
                    }
                    OutboxEvent::HeldPublished {
                        change,
                        item,
                        result,
                    } => this.held_published(&owner, *change, item, result, cx),
                    OutboxEvent::Changed(_) => {}
                },
            ));
            // The title bar shows the counts.
            subs.push(cx.observe(&outbox, |_, _, cx| cx.notify()));
            outbox.update(cx, |o, cx| o.resume(cx));
            outbox
        });
        if let Some(entry) = self.servers.get_mut(key) {
            let old_user = entry.session.as_ref().and_then(|s| s.username.clone());
            let new_user = session.as_ref().and_then(|s| s.username.clone());
            if new_user.is_some() && new_user != old_user && entry.me.is_some() {
                // Another account signed in: what we knew is not theirs.
                entry.me = None;
            }
            // A new session may try at once.
            entry.account_task = None;
            entry.account_failures = 0;
            entry.account_retry = None;
            entry.session = session;
            // Unreachable as far as the shell knows; a refusal of the old
            // session's credentials says nothing about the new one.
            if let Some(s) = &entry.session {
                s.set_unreachable(entry.connection == Connection::Offline);
            }
            entry.outbox = outbox;
            entry.outbox_subs = subs;
            if let Some(scanner) = &entry.local_scan {
                scanner.update(cx, |s, _| s.set_session(entry.session.as_ref()));
            }
            // The poll holds the old session's client.
            entry.attention_task = None;
            entry.watch_task = None;
        }
        self.sync_consolidator(key, window, cx);
        self.sync_attention_polls(window, cx);
    }

    /// A refresh of server `key` worked, so it is reachable: upload its
    /// drafts that waited, read the account if it is not known yet, and
    /// send what local changes have for Gerrit.
    fn server_reachable(&mut self, key: &ServerKey, window: &mut Window, cx: &mut Context<Self>) {
        self.servers.set_connection(key, Connection::Online);
        if let Some(outbox) = self.servers.get(key).and_then(|e| e.outbox.clone()) {
            outbox.update(cx, |o, cx| o.retry_all(cx));
        }
        self.fetch_account(key, window, cx);
        self.consolidate(key, Request::Poll, cx);
        if self.nav.current_server() == Some(key) {
            self.retry_redirect(window, cx);
        }
    }

    /// Fetches the signed-in account of server `key` (`me`) unless it is
    /// known: once per session, only while the server answers, and after a
    /// failure no sooner than the poll interval, doubled per failure like
    /// the polls. The account's email lands in the cache on the way
    /// (`Syncer::account_self`), and the local change scanner looks again
    /// with it. Refused credentials go the usual way.
    fn fetch_account(&mut self, key: &ServerKey, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.servers.get(key) else {
            return;
        };
        let due = entry
            .account_retry
            .is_none_or(|t| std::time::Instant::now() >= t);
        let Some(syncer) = entry
            .session
            .as_ref()
            .filter(|s| s.can_sync() && s.username.is_some())
            .filter(|_| entry.me.is_none() && entry.account_task.is_none() && due)
            .filter(|_| {
                !matches!(
                    entry.connection,
                    Connection::Offline | Connection::Unauthorized
                )
            })
            .map(|s| s.syncer().clone())
        else {
            return;
        };
        let owner = key.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let account = cx
                .background_spawn(async move { syncer.account_self().await })
                .await;
            this.update_in(cx, |this, window, cx| {
                let focused = this.window_active;
                let Some(entry) = this.servers.get_mut(&owner) else {
                    return;
                };
                entry.account_task = None;
                match account {
                    Ok(account) => {
                        entry.me = Some(watching::Me::of(&account));
                        entry.account_failures = 0;
                        entry.account_retry = None;
                        this.scan_local(&owner, cx);
                        this.account_known(&owner, cx);
                    }
                    Err(e) if e.is_unauthorized() => this.unauthorized_for(&owner, window, cx),
                    Err(e) => {
                        log::debug!("reading the account of {owner}: {e}");
                        entry.account_failures = entry.account_failures.saturating_add(1);
                        let state = sync::PollState {
                            last_attempt: None,
                            window_focused: focused,
                            visible: true,
                            consecutive_failures: entry.account_failures,
                        };
                        let wait = sync::PollPolicy::default().interval(&state);
                        entry.account_retry = Some(std::time::Instant::now() + wait);
                    }
                }
            })
            .ok();
        });
        if let Some(entry) = self.servers.get_mut(key) {
            entry.account_task = Some(task);
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
    pub(crate) fn local_scan_for_test(
        &self,
        key: &str,
    ) -> Option<Entity<crate::local_scan::LocalScanner>> {
        self.servers.get(&ServerKey::new(key))?.local_scan.clone()
    }

    #[cfg(test)]
    pub(crate) fn nav_for_test(&mut self) -> &mut NavState {
        &mut self.nav
    }

    #[cfg(test)]
    pub(crate) fn show_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show(window, cx);
    }

    #[cfg(test)]
    pub(crate) fn consolidation_for_test(
        &self,
        key: &str,
    ) -> Option<Entity<crate::consolidation::Consolidator>> {
        self.servers
            .get(&ServerKey::new(key))?
            .consolidation
            .clone()
    }

    /// Asks for a consolidation run of server `key`.
    #[cfg(test)]
    pub(crate) fn consolidate_for_test(
        &mut self,
        key: &str,
        kind: Request,
        cx: &mut Context<Self>,
    ) {
        self.consolidate(&ServerKey::new(key), kind, cx);
    }

    /// Signs server `key` in as `username` (the password `pw`).
    #[cfg(test)]
    pub(crate) fn sign_in_for_test(
        &mut self,
        key: &str,
        username: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = ServerKey::new(key);
        let Some(entry) = self.servers.get_mut(&key) else {
            return;
        };
        entry.setup.username = Some(username.to_owned());
        let setup = entry.setup.clone();
        let auth = Auth::Basic {
            username: username.to_owned(),
            password: "pw".into(),
        };
        let session = Session::new(&self.env, &setup, Some(auth)).expect("session");
        self.install_session(&key, Some(session), window, cx);
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
    pub(crate) fn inbox_page_for_test(&self) -> Option<Entity<InboxPage>> {
        match &self.page {
            Page::Inbox(p) => Some(p.clone()),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn open_inbox_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_inbox(&OpenInbox, window, cx);
    }

    /// Asks the inbox worker to re-read the store, as a delivered event does.
    #[cfg(test)]
    pub(crate) fn inbox_reload_for_test(&mut self, cx: &mut Context<Self>) {
        self.inbox_op(InboxOp::Reload, cx);
    }

    /// What the bell counts.
    #[cfg(test)]
    pub(crate) fn inbox_unread_for_test(&self) -> usize {
        crate::inbox::total(&self.inbox_unread)
    }

    #[cfg(test)]
    pub(crate) fn notify_settings_for_test(&self) -> &NotificationSettings {
        &self.notify_settings
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

    /// The server and change of the change or diff page on screen.
    fn current_change(&self) -> Option<(ServerKey, ChangeNumber)> {
        let key = self.nav.current_server()?.clone();
        match self.nav.current() {
            Route::Change(n) => Some((key, *n)),
            Route::Diff(d) => Some((key, d.change)),
            _ => None,
        }
    }

    /// Builds the page for the current route and focuses it.
    fn show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.adopt_nav_server(cx);
        self.page_subs.clear();
        self.drop_reply(cx);
        self.drop_menu();
        self.drop_filters_dialog();
        self.drop_rule_dialog();
        self.drop_project_dialog();
        self.drop_push_dialog();
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
            (Route::Inbox, Some(_)) => self.inbox_page(window, cx),
            (Route::Diff(route), Some(session)) => self.diff_page(session, route, window, cx),
            (Route::Change(n), Some(session)) => {
                let view = self.nav.change_view().unwrap_or_default();
                // Within a relation chain the new page takes the chain over,
                // so that it does not blink or jump back to the top.
                let chain = match &self.page {
                    Page::Change(p) => p.read(cx).chain_for(&session.key, n),
                    _ => None,
                };
                self.change_page(session, n, view, chain, active, window, cx)
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
        // A local change or version in the route may be on Gerrit by now
        // (a history entry from before).
        self.redirect_pending = false;
        if matches!(self.page, Page::Change(_) | Page::Diff(_)) {
            self.follow_redirect(None, window, cx);
        }
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
        self.scan_local(&key, cx);
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
        if let Some(scanner) = self.local_scanner(&owner, cx) {
            page.update(cx, |p, cx| p.set_local_scanner(scanner, cx));
        }
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
                ListEvent::Refreshed => this.server_reachable(&owner, window, cx),
            },
        ));
        Page::List(page)
    }

    fn inbox_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Page {
        let servers = self
            .servers
            .keys()
            .into_iter()
            .filter_map(|key| {
                let id = self.servers.get(&key)?.setup.server;
                let badge = self.servers.badge(&key);
                Some(InboxServer { id, key, badge })
            })
            .collect();
        let page = cx.new(|cx| InboxPage::new(servers, cx));
        self.page_subs.push(cx.subscribe_in(
            &page,
            window,
            |this, _, ev: &InboxEvent, window, cx| match ev {
                InboxEvent::Op(op) => this.inbox_op(op.clone(), cx),
                InboxEvent::Open { server, change } => {
                    this.go_to(server, Route::Change(*change), None, window, cx)
                }
            },
        ));
        // The rows come from the store.
        self.inbox_op(InboxOp::Reload, cx);
        // Opening the page is seeing what is in it: the bell resets.
        self.inbox_op(InboxOp::Seen, cx);
        Page::Inbox(page)
    }

    /// Queues a change to the inbox. The worker applies the queue in order
    /// off the main thread, then reads the unread counts for the bell (and
    /// the rows, for the page when it is open and was asked to reload or
    /// something failed).
    fn inbox_op(&mut self, op: InboxOp, cx: &mut Context<Self>) {
        self.inbox_queue.push(op);
        if self.inbox_busy {
            return;
        }
        self.inbox_busy = true;
        let store = self.env.store.clone();
        cx.spawn(async move |this, cx| {
            loop {
                let next = this.update(cx, |this, _| {
                    let ops = std::mem::take(&mut this.inbox_queue);
                    if ops.is_empty() {
                        this.inbox_busy = false;
                    }
                    (
                        ops,
                        this.servers.server_ids(),
                        matches!(this.page, Page::Inbox(_)),
                    )
                });
                let Ok((ops, servers, page_open)) = next else {
                    return;
                };
                if ops.is_empty() {
                    return;
                }
                let store = store.clone();
                let loaded = cx
                    .background_spawn(
                        async move { run_inbox_ops(&store, &ops, &servers, page_open) },
                    )
                    .await;
                let applied = this.update(cx, |this, cx| {
                    if let Some(counts) = loaded.counts {
                        this.inbox_unread = counts;
                    }
                    if let (Some(items), Page::Inbox(page)) = (loaded.items, &this.page) {
                        page.update(cx, |p, cx| p.set_items(items, cx));
                    }
                    cx.notify();
                });
                if applied.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    fn open_inbox(&mut self, _: &OpenInbox, window: &mut Window, cx: &mut Context<Self>) {
        if self.session().is_some() {
            self.navigate(Route::Inbox, window, cx);
        }
    }

    fn settings_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Page {
        let env = self.env.clone();
        let queries = self.queries();
        let bookmarks = self.bookmarks();
        let (start, layout) = (self.start_page.clone(), self.layout.clone());
        let filters = self.filters.clone();
        let server = self.session().map(|s| s.backend().clone());
        let (servers, active) = (self.servers.rows(), self.servers.active_key().cloned());
        let tab = self.settings_tab;
        let notify = self.notify_settings.clone();
        let checkouts = active.clone().and_then(|key| self.checkouts(&key, cx));
        let page = cx.new(|cx| {
            SettingsPage::new(
                env, server, servers, active, queries, bookmarks, start, layout, filters, notify,
                checkouts, tab, window, cx,
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
                SettingsEvent::TabShown(tab) => this.settings_tab = *tab,
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
                SettingsEvent::NotificationsSaved(settings) => {
                    this.set_notification_settings(settings.clone(), window, cx)
                }
                SettingsEvent::EditNotificationRule { index } => {
                    this.open_rule_dialog(*index, window, cx)
                }
                SettingsEvent::TestNotification => {
                    let sent = this.test_notification();
                    if let Page::Settings(page) = &this.page {
                        page.update(cx, |p, cx| p.test_sent(sent, cx));
                    }
                }
                SettingsEvent::EditProject { name } => {
                    this.open_project_dialog(name.clone(), None, window, cx)
                }
                SettingsEvent::SaveProjects(list) => this.save_projects(list.clone(), cx),
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

    #[allow(clippy::too_many_arguments)]
    fn change_page(
        &mut self,
        session: Session,
        number: ChangeNumber,
        view: ChangeView,
        chain: Option<ChainHandover>,
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
        let checkouts = self.checkouts(&owner, cx);
        let page = cx.new(|cx| {
            let mut page = ChangePage::new(
                session, settings, layout, filters, syntax, number, view, active, outbox, cx,
            );
            page.set_checkouts(checkouts, cx);
            page.set_server_badge(badge, cx);
            if let Some(chain) = chain {
                page.adopt_chain(chain, cx);
            }
            page
        });
        self.page_subs.push(cx.subscribe_in(
            &page,
            window,
            move |this, _, ev: &ChangeEvent, window, cx| match ev {
                ChangeEvent::Unauthorized => this.unauthorized_for(&owner, window, cx),
                ChangeEvent::Refreshed => {
                    this.server_reachable(&owner, window, cx);
                    this.drain_notifications(&owner, cx);
                }
                ChangeEvent::Reply(request) => this.open_reply((**request).clone(), window, cx),
                ChangeEvent::Actions(detail) => this.open_menu((**detail).clone(), window, cx),
                ChangeEvent::SubmitChain(detail) => {
                    this.open_menu_at((**detail).clone(), true, window, cx)
                }
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
                ChangeEvent::ProjectSettings(project) => {
                    this.open_project_settings(project, window, cx)
                }
                ChangeEvent::Search(q) => this.search(q.clone(), window, cx),
                ChangeEvent::LocalReviewSaved => this.consolidate(&owner, Request::Now, cx),
                ChangeEvent::Push(request) => {
                    this.open_push_dialog((**request).clone(), window, cx)
                }
                ChangeEvent::OpenChange(n) => this.navigate(Route::Change(*n), window, cx),
                ChangeEvent::OpenChangeAt(n, view) => {
                    this.remember_selection(cx);
                    this.nav.push(Route::Change(*n));
                    this.nav.set_change_view(Some(view.clone()));
                    this.show(window, cx)
                }
                ChangeEvent::ReplaceChangeAt(n, view) => {
                    this.nav.replace(Route::Change(*n));
                    this.nav.set_change_view(Some(view.clone()));
                    this.show(window, cx)
                }
                // Like stepping through files: Back leaves the walk.
                ChangeEvent::ReplaceChange(n) => {
                    this.nav.replace(Route::Change(*n));
                    this.show(window, cx)
                }
                // A vote moved on to the next change of the chain.
                ChangeEvent::Advance(n, at, note) => {
                    this.nav.replace(Route::Change(*n));
                    if let Some(ps) = at {
                        this.nav.set_change_view(Some(ChangeView {
                            target: Some(*ps),
                            ..ChangeView::default()
                        }));
                    }
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
        let checkouts = self.checkouts(&owner, cx);
        let page = cx.new(|cx| {
            let mut page = DiffPage::new(session, settings, filters, syntax, route, outbox, cx);
            page.set_checkouts(checkouts, cx);
            page.set_server_badge(badge, cx);
            page
        });
        self.page_subs.push(cx.subscribe_in(
            &page,
            window,
            move |this, _, ev: &DiffPageEvent, window, cx| match ev {
                DiffPageEvent::Unauthorized => this.unauthorized_for(&owner, window, cx),
                DiffPageEvent::Reply(request) => this.open_reply((**request).clone(), window, cx),
                DiffPageEvent::Actions(detail) => this.open_menu((**detail).clone(), window, cx),
                // Like PolyGerrit, stepping through files does not stack
                // history entries: Back from any file leads to the change.
                DiffPageEvent::UpToChange => this.go_up(&GoUp, window, cx),
                DiffPageEvent::Compare(route) => {
                    this.navigate(Route::Diff(route.clone()), window, cx)
                }
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
            bookmarks: known
                .as_ref()
                .map(|k| k.bookmarks.clone())
                .unwrap_or_default(),
            projects: known.map(|k| k.projects).unwrap_or_default(),
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
        // The store dropped its notifications with the server.
        self.inbox_op(InboxOp::Reload, cx);
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
        self.drop_rule_dialog();
        self.drop_project_dialog();
        self.drop_push_dialog();
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
            || self.rule_dialog.is_some()
            || self.project_dialog.is_some()
            || self.push_dialog.is_some()
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
            self.drop_rule_dialog();
            self.drop_project_dialog();
            self.drop_push_dialog();
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

    /// Opens the reply dialog for `request` on top of the current page.
    fn open_reply(&mut self, request: ReplyRequest, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(session), Some(outbox)) =
            (self.session().cloned(), self.active_outbox().cloned())
        else {
            return;
        };
        if self.reply.is_some()
            || self.menu.is_some()
            || self.filters_dialog.is_some()
            || self.rule_dialog.is_some()
            || self.project_dialog.is_some()
            || self.push_dialog.is_some()
        {
            return;
        }
        let owner = session.key.clone();
        let badge = self.servers.badge(&owner);
        let dialog = cx.new(|cx| {
            let mut dialog = ReplyDialog::open(session, outbox, request, window, cx);
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
                ReplyEvent::Saved { note, voted } => {
                    this.reply_saved(note.clone(), voted, window, cx)
                }
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
        self.retry_redirect(window, cx);
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

    /// A local review was saved: close the dialog, let the page say so,
    /// and send it if its version is on Gerrit by now.
    fn reply_saved(
        &mut self,
        note: String,
        voted: &[(String, i32)],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_reply(window, cx);
        match &self.page {
            Page::Change(p) => p.update(cx, |p, cx| p.local_review_saved(note, voted, cx)),
            Page::Diff(p) => p.update(cx, |p, cx| p.say(note, cx)),
            _ => {}
        }
        if let Some(key) = self.servers.active_key().cloned() {
            self.consolidate(&key, Request::Now, cx);
        }
    }

    // ---- change action menu ----

    /// Opens the action menu for `detail` on top of the current page.
    fn open_menu(&mut self, detail: ChangeDetail, window: &mut Window, cx: &mut Context<Self>) {
        self.open_menu_at(detail, false, window, cx);
    }

    /// Opens the action menu and submits at once if `submit` (the change
    /// page's Submit chain button).
    fn open_menu_at(
        &mut self,
        detail: ChangeDetail,
        submit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session().cloned() else {
            return;
        };
        if self.menu.is_some()
            || self.reply.is_some()
            || self.filters_dialog.is_some()
            || self.rule_dialog.is_some()
            || self.project_dialog.is_some()
            || self.push_dialog.is_some()
        {
            return;
        }
        let owner = session.key.clone();
        let badge = self.servers.badge(&owner);
        // The Submit warning asks the checkout, where the project is
        // scanned for local changes.
        let project = detail.summary.project.clone();
        let scanned = self.checkouts(&owner, cx).is_some_and(|c| {
            c.read(cx)
                .local_change_projects()
                .any(|p| p.name == project)
        });
        let scanner = scanned.then(|| self.local_scanner(&owner, cx)).flatten();
        let dialog = cx.new(|cx| {
            let mut dialog = ActionDialog::new(session, detail, window, cx);
            dialog.set_server_badge(badge, cx);
            if let Some(scanner) = scanner {
                dialog.set_local_scanner(scanner);
            }
            if submit {
                dialog.submit_now(window, cx);
            }
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
        self.retry_redirect(window, cx);
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
        if self.reply.is_some()
            || self.menu.is_some()
            || self.filters_dialog.is_some()
            || self.rule_dialog.is_some()
            || self.project_dialog.is_some()
            || self.push_dialog.is_some()
        {
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
        self.retry_redirect(window, cx);
        cx.notify();
    }

    // ---- notification rule editor ----

    /// Opens the notification rule editor on rule `index` of the settings
    /// page's list (one past the end: a new rule).
    fn open_rule_dialog(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.reply.is_some()
            || self.menu.is_some()
            || self.filters_dialog.is_some()
            || self.rule_dialog.is_some()
            || self.project_dialog.is_some()
            || self.push_dialog.is_some()
        {
            return;
        }
        let Page::Settings(page) = &self.page else {
            return;
        };
        let context = NotifyRuleContext {
            settings: page.read(cx).notification_settings().clone(),
            index,
            servers: self
                .servers
                .rows()
                .into_iter()
                .map(|r| (r.key, r.name))
                .collect(),
            server_ids: self.servers.server_ids(),
            queries: self.servers.query_names(),
        };
        let env = self.env.clone();
        let dialog = cx.new(|cx| NotifyRuleDialog::new(env, context, window, cx));
        self.rule_dialog_subs.push(cx.subscribe_in(
            &dialog,
            window,
            |this, _, ev: &NotifyRuleEvent, window, cx| match ev {
                NotifyRuleEvent::Close => this.close_rule_dialog(window, cx),
                NotifyRuleEvent::Saved { settings, index } => {
                    this.close_rule_dialog(window, cx);
                    this.set_notification_settings(settings.clone(), window, cx);
                    if let Page::Settings(page) = &this.page {
                        page.update(cx, |p, cx| {
                            p.notifications_saved(settings.clone(), *index, cx)
                        });
                    }
                }
            },
        ));
        self.rule_dialog = Some(dialog);
        cx.notify();
    }

    /// Takes the editor away for everything that replaces the page under it.
    fn drop_rule_dialog(&mut self) {
        self.rule_dialog_subs.clear();
        self.rule_dialog = None;
    }

    fn close_rule_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drop_rule_dialog();
        let handle = self.page.focus_handle(cx);
        window.focus(&handle, cx);
        self.retry_redirect(window, cx);
        cx.notify();
    }

    // ---- project checkout dialog ----

    /// Opens the project dialog on the checkout of project `edit` of the
    /// active server, or on a new one starting with the name `name`.
    fn open_project_dialog(
        &mut self,
        edit: Option<String>,
        name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.reply.is_some()
            || self.menu.is_some()
            || self.filters_dialog.is_some()
            || self.rule_dialog.is_some()
            || self.project_dialog.is_some()
            || self.push_dialog.is_some()
        {
            return;
        }
        let Some(entry) = self.servers.active() else {
            return;
        };
        let context = ProjectContext {
            projects: entry.setup.projects.clone(),
            edit,
            name,
            anonymous: entry.setup.username.is_none(),
        };
        let dialog = cx.new(|cx| ProjectDialog::new(context, window, cx));
        self.project_dialog_subs.push(cx.subscribe_in(
            &dialog,
            window,
            |this, _, ev: &ProjectEvent, window, cx| match ev {
                ProjectEvent::Close => this.close_project_dialog(window, cx),
                ProjectEvent::Save(list) => {
                    this.save_projects(list.clone(), cx);
                    this.close_project_dialog(window, cx);
                }
            },
        ));
        self.project_dialog = Some(dialog);
        cx.notify();
    }

    /// Takes the dialog away for everything that replaces the page under it.
    fn drop_project_dialog(&mut self) {
        self.project_dialog_subs.clear();
        self.project_dialog = None;
    }

    fn close_project_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drop_project_dialog();
        let handle = self.page.focus_handle(cx);
        window.focus(&handle, cx);
        self.retry_redirect(window, cx);
        cx.notify();
    }

    // ---- push dialog ----

    /// Opens the push dialog for a local version of a change on the active
    /// server.
    fn open_push_dialog(
        &mut self,
        request: PushRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.reply.is_some()
            || self.menu.is_some()
            || self.filters_dialog.is_some()
            || self.rule_dialog.is_some()
            || self.project_dialog.is_some()
            || self.push_dialog.is_some()
        {
            return;
        }
        let Some(session) = self.session().cloned() else {
            return;
        };
        let owner = session.key.clone();
        let Some(checkouts) = self.checkouts(&owner, cx) else {
            return;
        };
        let dialog =
            cx.new(|cx| PushDialog::new(request, checkouts, session.base_url.clone(), window, cx));
        self.push_dialog_subs.push(cx.subscribe_in(
            &dialog,
            window,
            move |this, _, ev: &PushEvent, window, cx| match ev {
                PushEvent::Close => this.close_push_dialog(window, cx),
                PushEvent::Pushed(note) => {
                    this.close_push_dialog(window, cx);
                    if let Page::Change(p) = &this.page {
                        p.update(cx, |p, cx| p.say(note.clone(), cx));
                    }
                    // Finds the commits on Gerrit and sends the reviews.
                    this.consolidate(&owner, Request::Now, cx);
                }
            },
        ));
        self.push_dialog = Some(dialog);
        cx.notify();
    }

    /// Takes the dialog away for everything that replaces the page under it.
    fn drop_push_dialog(&mut self) {
        self.push_dialog_subs.clear();
        self.push_dialog = None;
    }

    fn close_push_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drop_push_dialog();
        let handle = self.page.focus_handle(cx);
        window.focus(&handle, cx);
        self.retry_redirect(window, cx);
        cx.notify();
    }

    /// The gear next to a change's project: the settings page on the
    /// Projects tab with the project's checkout selected, or, when the
    /// project has none, the dialog for a new one with the name filled in.
    fn open_project_settings(
        &mut self,
        project: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.servers.active() else {
            return;
        };
        let configured = entry.setup.projects.iter().any(|p| p.name == project);
        self.settings_tab = SettingsTab::Projects;
        self.navigate(Route::Settings, window, cx);
        let Page::Settings(page) = &self.page else {
            return;
        };
        if configured {
            page.update(cx, |p, cx| p.select_project(project, cx));
        } else {
            self.open_project_dialog(None, Some(project.to_owned()), window, cx);
        }
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
            || self.rule_dialog.is_some()
            || self.project_dialog.is_some()
            || self.push_dialog.is_some()
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
        self.retry_redirect(window, cx);
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
            || self.rule_dialog.is_some()
            || self.project_dialog.is_some()
            || self.push_dialog.is_some()
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
        self.retry_redirect(window, cx);
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
            self.open_reply(ReplyRequest::gerrit(detail), window, cx);
        }
    }

    #[cfg(test)]
    pub(crate) fn open_reply_for_test(
        &mut self,
        request: ReplyRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_reply(request, window, cx);
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
    pub(crate) fn rule_dialog_for_test(&self) -> Option<Entity<NotifyRuleDialog>> {
        self.rule_dialog.clone()
    }

    #[cfg(test)]
    pub(crate) fn open_project_settings_for_test(
        &mut self,
        project: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_project_settings(project, window, cx);
    }

    #[cfg(test)]
    pub(crate) fn project_dialog_for_test(&self) -> Option<Entity<ProjectDialog>> {
        self.project_dialog.clone()
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
}

/// What a batch of inbox changes left: the unread counts for the bell, and
/// the rows if the page needs them.
struct InboxLoaded {
    counts: Option<HashMap<store::ServerId, usize>>,
    items: Option<Vec<store::InboxItem>>,
}

/// Applies `ops` to the store, then reads the unread counts, and the rows of
/// `servers` if the page is open and wants them (a reload was asked for, or
/// a change failed, so that it shows what the store has).
fn run_inbox_ops(
    store: &std::sync::Mutex<store::Store>,
    ops: &[InboxOp],
    servers: &[store::ServerId],
    page_open: bool,
) -> InboxLoaded {
    let Ok(mut store) = store.lock() else {
        log::warn!("the cache is unavailable: no inbox update");
        return InboxLoaded {
            counts: None,
            items: None,
        };
    };
    let now = jiff::Timestamp::now();
    let mut failed = false;
    let reload_asked = ops.contains(&InboxOp::Reload);
    // While the page is open, rows it is about to show count as seen, so a
    // new arrival leaves no badge behind.
    let seen = (page_open && reload_asked).then_some(&InboxOp::Seen);
    for op in ops.iter().chain(seen) {
        if let Err(e) = op.apply(&mut store, now) {
            log::warn!("changing the notification inbox failed: {e}");
            failed = true;
        }
    }
    let counts = store
        .inbox_unread_counts()
        .map_err(|e| log::warn!("counting unread notifications failed: {e}"))
        .ok();
    let reload = reload_asked || failed;
    let items = (page_open && reload)
        .then(|| {
            let limit = store::INBOX_KEEP * servers.len().max(1);
            store
                .inbox_items(Some(servers), limit)
                .map_err(|e| log::warn!("reading the notification inbox failed: {e}"))
                .ok()
        })
        .flatten();
    InboxLoaded { counts, items }
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
                    .on_action(cx.listener(Self::open_inbox))
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
                    .children(self.rule_dialog.clone())
                    .children(self.project_dialog.clone())
                    .children(self.push_dialog.clone())
                    .children(self.goto.clone())
                    .children(self.switcher.clone()),
            )
    }
}
