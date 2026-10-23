//! The login page: server URL, optional username, HTTP password.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::{
    login::{
        LoginError, LoginOutcome, LoginRequest, credentials_page_url, login, normalize_base_url,
    },
    session::{Env, ServerKey},
};

/// Values to fill in, e.g. after the server rejected the saved password.
#[derive(Clone, Debug, Default)]
pub struct LoginPrefill {
    /// The configured server being logged in to again; `None`: a new one.
    pub server: Option<ServerKey>,
    pub base_url: String,
    pub username: Option<String>,
    pub notice: Option<String>,
}

pub struct LoginPage {
    env: Env,
    server: Option<ServerKey>,
    url: Entity<InputState>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    anonymous: bool,
    busy: bool,
    error: Option<SharedString>,
    task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<LoginOutcome> for LoginPage {}

impl Focusable for LoginPage {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.url.read(cx).focus_handle(cx).clone()
    }
}

impl LoginPage {
    pub fn new(
        env: Env,
        prefill: LoginPrefill,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let url =
            cx.new(|cx| InputState::new(window, cx).placeholder("https://gerrit.example.com"));
        let username = cx.new(|cx| InputState::new(window, cx).placeholder("Username"));
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("HTTP password or token")
                .masked(true)
        });
        url.update(cx, |s, cx| {
            s.set_value(prefill.base_url.clone(), window, cx)
        });
        if let Some(u) = &prefill.username {
            username.update(cx, |s, cx| s.set_value(u.clone(), window, cx));
        }
        let mut subs = Vec::new();
        for input in [&url, &username, &password] {
            subs.push(
                cx.subscribe_in(input, window, |this, _, ev: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        this.submit(window, cx);
                    }
                }),
            );
        }
        Self {
            env,
            server: prefill.server,
            url,
            username,
            password,
            anonymous: false,
            busy: false,
            error: prefill.notice.map(Into::into),
            task: None,
            _subs: subs,
        }
    }

    fn submit(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let username = self.username.read(cx).value().trim().to_owned();
        let password = self.password.read(cx).value().to_string();
        let (username, password) = if self.anonymous {
            (None, None)
        } else if username.is_empty() {
            self.error = Some("Enter a username, or choose Anonymous.".into());
            return cx.notify();
        } else if password.is_empty() {
            self.error = Some("Enter the HTTP password.".into());
            return cx.notify();
        } else {
            (Some(username), Some(password))
        };
        let req = LoginRequest {
            server: self.server.clone(),
            base_url: self.url.read(cx).value().to_string(),
            username,
            password,
        };
        self.busy = true;
        self.error = None;
        let env = self.env.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let bg_env = env.clone();
            let result = cx
                .background_spawn(async move { login(&bg_env, req).await })
                .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(outcome) => cx.emit(outcome),
                    Err(e) => this.error = Some(describe(&e).into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn open_credentials_page(&mut self, cx: &mut Context<Self>) {
        match normalize_base_url(self.url.read(cx).value().as_ref()) {
            Ok(base) => cx.open_url(&credentials_page_url(&base)),
            Err(e) => self.error = Some(e.to_string().into()),
        }
        cx.notify();
    }
}

fn describe(e: &LoginError) -> String {
    let s = e.to_string();
    let mut c = s.chars();
    match c.next() {
        Some(first) => format!("{}{}.", first.to_uppercase(), c.as_str()),
        None => s,
    }
}

impl Render for LoginPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let label = |t: &'static str| div().text_sm().text_color(theme.muted_foreground).child(t);
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .w(px(420.))
                    .max_w_full()
                    .gap_3()
                    .p_4()
                    .child(div().text_xl().font_semibold().child("Connect to Gerrit"))
                    .child(label("Server URL"))
                    .child(Input::new(&self.url))
                    .child(
                        Checkbox::new("anonymous")
                            .label("Anonymous (read-only, public servers)")
                            .checked(self.anonymous)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.anonymous = *checked;
                                cx.notify();
                            })),
                    )
                    .when(!self.anonymous, |d| {
                        d.child(label("Username"))
                            .child(Input::new(&self.username))
                            .child(label("HTTP password"))
                            .child(Input::new(&self.password).mask_toggle())
                            .child(
                                Button::new("credentials")
                                    .label("Open HTTP credentials page")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.open_credentials_page(cx)
                                    })),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("Single sign-on servers: create a password or token on that page, then paste it above."),
                            )
                    })
                    .children(self.error.clone().map(|e| {
                        div().text_sm().text_color(theme.danger).child(e)
                    }))
                    .child(
                        h_flex().child(
                            Button::new("login")
                                .primary()
                                .label(if self.busy { "Connecting\u{2026}" } else { "Log in" })
                                .loading(self.busy)
                                .on_click(cx.listener(|this, _, w, cx| this.submit(w, cx))),
                        ),
                    ),
            )
    }
}
