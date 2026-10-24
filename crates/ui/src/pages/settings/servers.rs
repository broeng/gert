//! The Servers section of the settings page: list, add, edit, reorder,
//! remove and test the connection of the configured servers.
//!
//! Removing a server purges it (config entry, cache, keyring secret; see
//! `Backend::remove_server`). Changing the URL or the user changes the
//! keyring account, so the form asks for the password again: the login is
//! checked and stored under the new account first, and only then is the old
//! secret dropped and the list written. Cancelling, or a failed check,
//! changes nothing. A new name or color needs no password.

use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

pub(crate) use crate::servers::ServerRow;
use crate::{
    actions::*,
    card,
    login::{LoginError, LoginRequest, check_credentials, login, normalize_base_url},
    servers::{Connection, ServerBadge, dot},
    session::ServerConfig,
    theme::parse_hex_color,
};

use super::{Mode, SettingsEvent, SettingsPage, SettingsTab, moved};

/// What the last "test connection" of a server found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TestState {
    Running,
    Passed,
    Failed(String),
}

/// The inputs of the server form. One set for the page: the form edits
/// one server at a time.
pub(super) struct ServerForm {
    name: Entity<InputState>,
    url: Entity<InputState>,
    username: Entity<InputState>,
    color: Entity<InputState>,
    password: Entity<InputState>,
    /// Why the form cannot be saved.
    invalid: Option<SharedString>,
}

impl ServerForm {
    pub(super) fn new(window: &mut Window, cx: &mut Context<SettingsPage>) -> Self {
        Self {
            name: cx.new(|cx| InputState::new(window, cx).placeholder("Name")),
            url: cx.new(|cx| InputState::new(window, cx).placeholder("https://gerrit.example.com")),
            username: cx
                .new(|cx| InputState::new(window, cx).placeholder("Username (empty: anonymous)")),
            color: cx.new(|cx| InputState::new(window, cx).placeholder("Automatic, or #rrggbb")),
            password: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("HTTP password (asked again when the URL or user changes)")
                    .masked(true)
            }),
            invalid: None,
        }
    }

    pub(super) fn inputs(&self) -> [&Entity<InputState>; 5] {
        [
            &self.name,
            &self.url,
            &self.username,
            &self.color,
            &self.password,
        ]
    }
}

/// The text for a connection state.
fn connection_text(c: Connection) -> &'static str {
    match c {
        Connection::Connecting => "connecting",
        Connection::Online => "online",
        Connection::Offline => "offline",
        Connection::Unauthorized => "sign-in needed",
    }
}

/// What the form made of the typed values.
#[derive(Debug, PartialEq, Eq)]
struct Edited {
    config: ServerConfig,
    /// The URL or the user differs from before, so the password is needed
    /// (for a user; an anonymous server has none).
    login_changed: bool,
}

/// Checks the typed values of the form for `old`, or says what is wrong.
fn validate_edit(
    old: &ServerRow,
    name: &str,
    url: &str,
    username: &str,
    color: &str,
    password: &str,
) -> Result<(Edited, Option<String>), SharedString> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Give the server a name.".into());
    }
    let url = normalize_base_url(url).map_err(|e| SharedString::from(e.to_string()))?;
    let username = Some(username.trim().to_owned()).filter(|u| !u.is_empty());
    let color = match color.trim() {
        "" => None,
        c => match parse_hex_color(c) {
            Some(_) => Some(format!("#{}", c.trim_start_matches('#').to_lowercase())),
            None => return Err("The color is #rrggbb, or empty for automatic.".into()),
        },
    };
    let login_changed = url != old.url || username != old.username;
    let password = Some(password.to_owned()).filter(|p| !p.is_empty());
    if login_changed && username.is_some() && password.is_none() {
        return Err(
            "The URL or user changed: enter the HTTP password, it is stored under the new account."
                .into(),
        );
    }
    Ok((
        Edited {
            config: ServerConfig {
                key: old.key.clone(),
                name: name.to_owned(),
                url,
                username,
                color,
            },
            login_changed,
        },
        password.filter(|_| login_changed),
    ))
}

fn config_of(row: &ServerRow) -> ServerConfig {
    ServerConfig {
        key: row.key.clone(),
        name: row.name.clone(),
        url: row.url.clone(),
        username: row.username.clone(),
        color: row.color.clone(),
    }
}

fn row_of(config: &ServerConfig, connection: Connection) -> ServerRow {
    ServerRow {
        key: config.key.clone(),
        name: config.name.clone(),
        url: config.url.clone(),
        username: config.username.clone(),
        color: config.color.clone(),
        connection,
    }
}

impl SettingsPage {
    /// The badge of the active server, when there are several (the queries
    /// and bookmarks belong to it).
    pub(crate) fn active_badge(&self, _cx: &App) -> Option<ServerBadge> {
        if self.servers.len() < 2 {
            return None;
        }
        let key = self.active.as_ref()?;
        let position = self.servers.iter().position(|s| s.key == *key)?;
        let row = &self.servers[position];
        Some(ServerBadge::new(
            row.name.clone(),
            position,
            row.color.clone(),
        ))
    }

    fn configs(&self) -> Vec<ServerConfig> {
        self.servers.iter().map(config_of).collect()
    }

    /// The rows for a saved list, keeping each server's connection state.
    fn rows_from(&self, saved: &[ServerConfig]) -> Vec<ServerRow> {
        saved
            .iter()
            .map(|c| {
                let connection = self
                    .servers
                    .iter()
                    .find(|s| s.key == c.key)
                    .map_or(Connection::Connecting, |s| s.connection);
                row_of(c, connection)
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn servers_for_test(&self) -> &[ServerRow] {
        &self.servers
    }

    #[cfg(test)]
    pub(crate) fn server_inputs_for_test(&self) -> [Entity<InputState>; 5] {
        self.form.inputs().map(Clone::clone)
    }

    #[cfg(test)]
    pub(crate) fn server_form_error_for_test(&self) -> Option<String> {
        self.form.invalid.as_ref().map(|s| s.to_string())
    }

    #[cfg(test)]
    pub(crate) fn connection_test_for_test(&self, key: &str) -> Option<TestState> {
        self.tests
            .get(&crate::session::ServerKey::new(key))
            .cloned()
    }

    #[cfg(test)]
    pub(crate) fn confirming_server_for_test(&self) -> bool {
        matches!(self.mode, Mode::ConfirmingServer(_))
    }

    #[cfg(test)]
    pub(crate) fn select_row_for_test(&mut self, row: usize, cx: &mut Context<Self>) {
        // Shows the tab the row is in, as keys would.
        let tab = self.tab_of(row);
        self.show_tab(tab, cx);
        self.select(row, cx);
    }

    #[cfg(test)]
    pub(crate) fn servers_row_for_test(&self) -> usize {
        self.servers_row()
    }

    // ---- editing ----

    pub(super) fn start_server_edit(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.servers.get(index).cloned() else {
            return;
        };
        let set = |input: &Entity<InputState>, text: String, window: &mut Window, cx: &mut _| {
            input.update(cx, |s, cx| s.set_value(text, window, cx));
        };
        set(&self.form.name, row.name, window, cx);
        set(&self.form.url, row.url, window, cx);
        set(
            &self.form.username,
            row.username.unwrap_or_default(),
            window,
            cx,
        );
        set(&self.form.color, row.color.unwrap_or_default(), window, cx);
        set(&self.form.password, String::new(), window, cx);
        self.form.invalid = None;
        self.cursor = self.servers_row() + index;
        self.mode = Mode::EditingServer(index);
        self.form.name.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    /// Tab: the next input of the form (the first one after the last).
    pub(super) fn next_server_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let inputs = self.form.inputs();
        let at = inputs
            .iter()
            .position(|i| i.focus_handle(cx).is_focused(window));
        let next = inputs[at.map_or(0, |i| (i + 1) % inputs.len())].clone();
        next.update(cx, |s, cx| s.focus(window, cx));
    }

    pub(super) fn commit_server_edit(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(old) = self.servers.get(index).cloned() else {
            return;
        };
        let text = |i: &Entity<InputState>| i.read(cx).value().to_string();
        let checked = validate_edit(
            &old,
            &text(&self.form.name),
            &text(&self.form.url),
            &text(&self.form.username),
            &text(&self.form.color),
            &text(&self.form.password),
        );
        let (edited, password) = match checked {
            Ok(ok) => ok,
            Err(why) => {
                self.form.invalid = Some(why);
                cx.notify();
                return;
            }
        };
        self.form.invalid = None;
        let mut list = self.configs();
        list[index] = edited.config.clone();
        let env = self.env.clone();
        self.busy = true;
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let mut outcome = None;
                    if edited.login_changed {
                        // The new login is checked and stored first: if it
                        // fails nothing has changed. The old secret goes
                        // only once the new one is safe.
                        let c = &edited.config;
                        let request = LoginRequest {
                            server: Some(c.key.clone()),
                            name: None,
                            base_url: c.url.clone(),
                            username: c.username.clone(),
                            password,
                        };
                        outcome = Some(login(&env, request).await.map_err(|e| e.to_string())?);
                        if let Some(user) = &old.username
                            && let Err(e) = env.backend.forget_login(&old.url, user)
                        {
                            log::warn!("dropping the old password of {}: {e}", old.name);
                        }
                    }
                    let saved = env.backend.save_servers(&list).map_err(|e| e.to_string())?;
                    Ok::<_, String>((saved, outcome))
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok((saved, relogin)) => {
                        this.servers = this.rows_from(&saved);
                        // Whatever was tested before is about the old values.
                        this.tests.remove(&edited_key(&saved, index));
                        this.leave_edit(window, cx);
                        cx.emit(SettingsEvent::ServersSaved {
                            servers: saved,
                            relogin: relogin.map(Box::new),
                        });
                    }
                    Err(e) => {
                        this.form.invalid = Some(format!("Not saved: {e}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // ---- reorder ----

    pub(super) fn move_server(
        &mut self,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.server_under_cursor() else {
            return;
        };
        let Some((list, to)) = moved(&self.configs(), index, delta) else {
            return;
        };
        let env = self.env.clone();
        self.busy = true;
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { env.backend.save_servers(&list) })
                .await;
            this.update_in(cx, |this, _, cx| {
                this.busy = false;
                match result {
                    Ok(saved) => {
                        this.servers = this.rows_from(&saved);
                        this.cursor = this.servers_row() + to;
                        cx.emit(SettingsEvent::ServersSaved {
                            servers: saved,
                            relogin: None,
                        });
                    }
                    Err(e) => {
                        this.hint
                            .say(format!("Could not save the servers: {e}"), true, cx, |p| {
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

    // ---- remove ----

    pub(super) fn remove_server(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.servers.get(index).cloned() else {
            return;
        };
        let removed = config_of(&row);
        let remaining: Vec<ServerConfig> = self
            .configs()
            .into_iter()
            .filter(|c| c.key != row.key)
            .collect();
        let env = self.env.clone();
        self.busy = true;
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { env.backend.remove_server(&removed, &remaining) })
                .await;
            this.update_in(cx, |this, _, cx| {
                this.busy = false;
                this.mode = Mode::Browsing;
                match result {
                    Ok(saved) => {
                        this.servers = this.rows_from(&saved);
                        this.tests.remove(&row.key);
                        this.cursor = this.cursor.min(this.last_row());
                        cx.emit(SettingsEvent::ServerRemoved {
                            key: row.key.clone(),
                        });
                    }
                    Err(e) => {
                        this.hint.say(
                            format!("Could not remove {}: {e}", row.name),
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

    // ---- test connection ----

    /// `t`: asks the server under the cursor who we are, with its saved
    /// password. Nothing is sent before this.
    pub(super) fn test_connection(
        &mut self,
        _: &TestConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tab == SettingsTab::Notifications {
            return self.ask_test_notification(cx);
        }
        let Some(row) = self
            .server_under_cursor()
            .and_then(|i| self.servers.get(i))
            .cloned()
            .filter(|_| self.mode == Mode::Browsing)
        else {
            return;
        };
        if self.tests.get(&row.key) == Some(&TestState::Running) {
            return;
        }
        self.tests.insert(row.key.clone(), TestState::Running);
        let env = self.env.clone();
        let backend = env.server_backend(&row.key);
        let key = row.key.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let failed = |text: String| (TestState::Failed(text), None);
                    let password = match &row.username {
                        Some(user) => match backend.load_password(&row.url, user) {
                            Ok(Some(p)) => Some(p),
                            Ok(None) => {
                                return failed(
                                    "no saved password; log in again by editing the server".into(),
                                );
                            }
                            Err(e) => {
                                return failed(format!("could not read the password: {e}"));
                            }
                        },
                        None => None,
                    };
                    match check_credentials(
                        &env,
                        &row.url,
                        row.username.as_deref(),
                        password.as_deref(),
                    )
                    .await
                    {
                        Ok(()) => (TestState::Passed, Some(Connection::Online)),
                        Err(e) => {
                            let connection = match &e {
                                LoginError::Unauthorized | LoginError::AnonymousDenied => {
                                    Some(Connection::Unauthorized)
                                }
                                LoginError::Server(_) => Some(Connection::Offline),
                                _ => None,
                            };
                            (TestState::Failed(e.to_string()), connection)
                        }
                    }
                })
                .await;
            let (result, connection) = result;
            this.update(cx, |this, cx| {
                if let Some(connection) = connection {
                    cx.emit(SettingsEvent::ConnectionTested {
                        key: key.clone(),
                        connection,
                    });
                }
                this.tests.insert(key, result);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    // ---- rendering ----

    fn render_server_row(&self, i: usize, row: &ServerRow, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let at = self.servers_row() + i;
        let selected = at == self.cursor;
        match self.mode {
            Mode::EditingServer(e) if e == i => return self.render_server_form(i, cx),
            _ => {}
        }
        let badge = ServerBadge::new(row.name.clone(), i, row.color.clone());
        let confirming = self.mode == Mode::ConfirmingServer(i);
        let (test_text, test_ok) = match self.tests.get(&row.key) {
            Some(TestState::Running) => (Some("testing...".to_owned()), None),
            Some(TestState::Passed) => (Some("connection works".to_owned()), Some(true)),
            Some(TestState::Failed(e)) => (Some(e.clone()), Some(false)),
            None => (None, None),
        };
        let idle = !self.busy && self.mode == Mode::Browsing;
        let buttons = h_flex()
            .gap_1()
            .flex_none()
            .child(
                Button::new(("srv-up", i))
                    .label("\u{2191}")
                    .small()
                    .disabled(i == 0 || !idle)
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.cursor = at;
                        this.move_server(-1, w, cx)
                    })),
            )
            .child(
                Button::new(("srv-down", i))
                    .label("\u{2193}")
                    .small()
                    .disabled(i + 1 >= self.servers.len() || !idle)
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.cursor = at;
                        this.move_server(1, w, cx)
                    })),
            )
            .child(
                Button::new(("srv-test", i))
                    .label("Test")
                    .small()
                    .disabled(!idle)
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.cursor = at;
                        this.test_connection(&TestConnection, w, cx)
                    })),
            )
            .child(
                Button::new(("srv-edit", i))
                    .label("Edit")
                    .small()
                    .disabled(!idle)
                    .on_click(cx.listener(move |this, _, w, cx| this.start_server_edit(i, w, cx))),
            )
            .child(
                Button::new(("srv-remove", i))
                    .label("Remove")
                    .small()
                    .disabled(!idle)
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.cursor = at;
                        this.delete_query(&DeleteQuery, w, cx)
                    })),
            );
        let line = h_flex()
            .id(("server-row", i))
            .px_3()
            .py_1p5()
            .gap_3()
            .items_center()
            .when(selected, |d| d.bg(theme.list_active))
            .on_click(cx.listener(move |this, _, _, cx| this.select(at, cx)))
            .child(dot(badge.color(cx)))
            .child(
                div()
                    .w(px(160.))
                    .flex_none()
                    .truncate()
                    .font_semibold()
                    .child(row.name.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.muted_foreground)
                    .child(row.url.clone()),
            )
            .child(
                div()
                    .w(px(120.))
                    .flex_none()
                    .truncate()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(row.username.clone().unwrap_or_else(|| "anonymous".into())),
            )
            .child(
                div()
                    .w(px(90.))
                    .flex_none()
                    .text_xs()
                    .text_color(match row.connection {
                        Connection::Online => theme.success,
                        Connection::Offline | Connection::Unauthorized => theme.warning,
                        Connection::Connecting => theme.muted_foreground,
                    })
                    .child(connection_text(row.connection)),
            );
        if confirming {
            return v_flex()
                .child(
                    line.child(
                        div()
                            .flex_none()
                            .text_color(theme.danger)
                            .child("Remove, with its cache and saved password?  y / n"),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("srv-confirm-yes")
                                    .label("Yes (y)")
                                    .small()
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.confirm_delete(&ConfirmDelete, w, cx)
                                    })),
                            )
                            .child(
                                Button::new("srv-confirm-no")
                                    .label("No (n)")
                                    .small()
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.cancel_delete(&CancelDelete, w, cx)
                                    })),
                            ),
                    ),
                )
                .into_any_element();
        }
        v_flex()
            .child(line.child(buttons))
            .children(test_text.map(|t| {
                div()
                    .px_3()
                    .pb_1p5()
                    .pl(px(36.))
                    .text_xs()
                    .text_color(match test_ok {
                        Some(true) => theme.success,
                        Some(false) => theme.danger,
                        None => theme.muted_foreground,
                    })
                    .child(t)
            }))
            .into_any_element()
    }

    fn render_server_form(&self, i: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let field = |label: &'static str, input: &Entity<InputState>| {
            h_flex()
                .gap_3()
                .items_center()
                .child(
                    div()
                        .w(px(90.))
                        .flex_none()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .child(div().flex_1().min_w_0().child(Input::new(input).small()))
        };
        let typed_color = parse_hex_color(self.form.color.read(cx).value().as_ref());
        let preview = typed_color.unwrap_or_else(|| ServerBadge::new("", i, None).color(cx));
        let old = self.servers.get(i).cloned();
        let changed = old.is_some_and(|old| {
            self.form.url.read(cx).value().trim().trim_end_matches('/') != old.url
                || Some(self.form.username.read(cx).value().trim().to_owned())
                    .filter(|u| !u.is_empty())
                    != old.username
        });
        v_flex()
            .id(("server-form", i))
            .px_3()
            .py_2()
            .gap_2()
            .bg(theme.list_active)
            .child(field("Name", &self.form.name))
            .child(field("URL", &self.form.url))
            .child(field("Username", &self.form.username))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().child(field("Color", &self.form.color)))
                    .child(dot(preview)),
            )
            .child(field("Password", &self.form.password))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(if changed {
                        "The URL or user changed: the password is stored under the new account and the old one is removed."
                    } else {
                        "Name and color need no password. Empty color: automatic."
                    }),
            )
            .children(
                self.form
                    .invalid
                    .clone()
                    .map(|why| div().text_sm().text_color(theme.danger).child(why)),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("srv-save")
                            .label("Save")
                            .small()
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, w, cx| this.commit_edit(w, cx))),
                    )
                    .child(
                        Button::new("srv-cancel").label("Cancel").small().on_click(
                            cx.listener(|this, _, w, cx| this.cancel_edit(&CancelEdit, w, cx)),
                        ),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn render_servers(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let n = self.servers.len();
        let mut body = v_flex().w_full().child(card::frame(
            card::title_bar(cx)
                .child(div().font_semibold().child("Servers"))
                .child(div().text_xs().text_color(theme.muted_foreground).child(
                    "Enter edits, a adds, d removes, t tests the connection. \
                     The order is that of Ctrl-1 to Ctrl-9.",
                )),
            true,
            n == 0,
            cx,
        ));
        if n == 0 {
            let selected = self.cursor == self.servers_row() && self.mode == Mode::Browsing;
            body = body.child(card::frame(
                card::row_separator(cx).child(
                    div()
                        .id("server-row-empty")
                        .px_3()
                        .py_2()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .when(selected, |d| d.bg(theme.list_active))
                        .child("No servers. Press a to add one."),
                ),
                false,
                true,
                cx,
            ));
        }
        for (i, row) in self.servers.iter().enumerate() {
            let element = self.render_server_row(i, row, cx);
            body = body.child(card::frame(
                card::row_separator(cx).child(element),
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
                    Button::new("add-server")
                        .label("Add a server (a)")
                        .small()
                        .disabled(self.busy || self.mode != Mode::Browsing)
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.emit(SettingsEvent::AddServer);
                            let _ = this;
                        })),
                ),
            )
            .into_any_element()
    }
}

/// The key of the server at `index` of a saved list.
fn edited_key(saved: &[ServerConfig], index: usize) -> crate::session::ServerKey {
    saved
        .get(index)
        .map(|c| c.key.clone())
        .unwrap_or_else(|| crate::session::ServerKey::new(""))
}

#[cfg(test)]
mod tests {
    use super::{Connection, ServerRow, validate_edit};
    use crate::session::ServerKey;

    fn row() -> ServerRow {
        ServerRow {
            key: ServerKey::new("work"),
            name: "Work".into(),
            url: "https://g.example".into(),
            username: Some("jdoe".into()),
            color: None,
            connection: Connection::Online,
        }
    }

    #[test]
    fn a_rename_or_color_needs_no_password() {
        let (e, pw) = validate_edit(
            &row(),
            " Day job ",
            "https://g.example/",
            "jdoe",
            "FF8800",
            "",
        )
        .unwrap();
        assert!(!e.login_changed);
        assert_eq!(pw, None);
        assert_eq!(e.config.name, "Day job");
        assert_eq!(e.config.color.as_deref(), Some("#ff8800"));
        assert_eq!(e.config.url, "https://g.example");
        // Empty color: automatic again.
        let (e, _) = validate_edit(&row(), "Work", "https://g.example", "jdoe", " ", "").unwrap();
        assert_eq!(e.config.color, None);
    }

    #[test]
    fn a_new_url_or_user_needs_the_password_again() {
        let err =
            validate_edit(&row(), "Work", "https://other.example", "jdoe", "", "").unwrap_err();
        assert!(err.contains("password"), "{err}");
        let err = validate_edit(&row(), "Work", "https://g.example", "alice", "", "").unwrap_err();
        assert!(err.contains("password"), "{err}");
        let (e, pw) =
            validate_edit(&row(), "Work", "https://g.example", "alice", "", "s3cret").unwrap();
        assert!(e.login_changed);
        assert_eq!(pw.as_deref(), Some("s3cret"));
        // The password is not kept when nothing about the login changed.
        let (_, pw) =
            validate_edit(&row(), "Work", "https://g.example", "jdoe", "", "typed").unwrap();
        assert_eq!(pw, None);
    }

    #[test]
    fn going_anonymous_needs_no_password() {
        let (e, pw) = validate_edit(&row(), "Work", "https://g.example", " ", "", "").unwrap();
        assert!(e.login_changed);
        assert_eq!(e.config.username, None);
        assert_eq!(pw, None);
    }

    #[test]
    fn bad_values_are_refused_with_a_reason() {
        for (name, url, color) in [
            ("", "https://g.example", ""),
            ("W", "ftp://g.example", ""),
            ("W", "", ""),
            ("W", "https://g.example", "red"),
        ] {
            assert!(
                validate_edit(&row(), name, url, "jdoe", color, "").is_err(),
                "{name} {url} {color}"
            );
        }
    }
}
