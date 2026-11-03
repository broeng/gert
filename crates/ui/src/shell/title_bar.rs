//! The shell's title bar: navigation, the server chip, the inbox bell,
//! the draft indicator, search and help, and the window controls.

use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::Input;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex};
use gpui_kit::component::{IconName, TitleBar};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::{actions::*, inbox, nav::Route, row::server_label};

use super::Shell;

impl Shell {
    /// The title bar: it holds the navigation, the server and user, the search
    /// box and the help button, so that no extra row is spent on them, and
    /// carries the window buttons when the window is client-decorated. What
    /// is not a control is a drag handle for moving the window.
    pub(super) fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                            .child(self.render_bell(cx))
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

    /// The bell: opens the inbox, with the number of unread notifications
    /// of all servers on it (nothing when there are none).
    fn render_bell(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let unread = inbox::total(&self.inbox_unread);
        div()
            .relative()
            .child(
                Button::new("inbox")
                    .icon(IconName::Bell)
                    .small()
                    .tooltip("Notifications (Ctrl-Shift-N)")
                    .on_click(cx.listener(|this, _, w, cx| this.open_inbox(&OpenInbox, w, cx))),
            )
            .when(unread > 0, |d| {
                d.child(
                    div()
                        .absolute()
                        .top(px(-2.))
                        .right(px(-2.))
                        .px_1()
                        .min_w(px(14.))
                        .rounded_full()
                        .bg(theme.danger)
                        .text_color(theme.danger_foreground)
                        .text_size(px(10.))
                        .text_center()
                        .child(if unread > 99 {
                            "99+".to_owned()
                        } else {
                            unread.to_string()
                        }),
                )
            })
    }

    /// The active server's chip, with several servers: its dot, name and
    /// a warning if it cannot be used. A click opens the switcher.
    pub(super) fn render_server_chip(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
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
    pub(super) fn retry_drafts(&mut self, cx: &mut Context<Self>) {
        if let Some(outbox) = self.active_outbox().cloned() {
            outbox.update(cx, |o, cx| o.retry_everything(cx));
        }
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
