//! The server switcher: the chip in the title bar that names the active
//! server, and the popup behind it (`Ctrl-K`, or a click on the chip) that
//! lists the configured servers with a type-to-filter box.
//!
//! Both exist only with several servers configured. The popup is modal like
//! the Go to dialog: it takes the focus, Esc closes it, Up and Down move
//! the cursor, Enter (or a click) shows the server. `Ctrl-1` to `Ctrl-9`
//! keep working in it and close it.

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::{
    actions::Entry,
    servers::{Connection, ServerBadge, dot},
    session::ServerKey,
};

actions!(
    switcher,
    [
        /// Esc: close the popup.
        CloseSwitcher,
        /// Down: the next server in the list.
        SwitcherNext,
        /// Up: the previous server in the list.
        SwitcherPrev,
    ]
);

/// Key context of the popup.
pub const SERVER_SWITCHER: &str = "ServerSwitcher";

/// As deep as the field's own context, to win over its Esc and arrows.
const IN_FIELD: &str = "ServerSwitcher > Input";

pub fn entries() -> Vec<Entry> {
    let e = |keys, binding, description| Entry {
        binding,
        keys,
        description,
    };
    vec![
        e(
            "Esc",
            KeyBinding::new("escape", CloseSwitcher, Some(SERVER_SWITCHER)),
            "Close the switcher (Enter shows the server under the cursor)",
        ),
        e(
            "",
            KeyBinding::new("escape", CloseSwitcher, Some(IN_FIELD)),
            "",
        ),
        e(
            "Up / Down",
            KeyBinding::new("down", SwitcherNext, Some(IN_FIELD)),
            "Move the cursor (typing filters by name and address)",
        ),
        e("", KeyBinding::new("up", SwitcherPrev, Some(IN_FIELD)), ""),
    ]
}

/// Registers the popup's bindings. Call once at startup, after the
/// component library's own.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(entries().into_iter().map(|e| e.binding));
}

/// One server in the popup, as it was when the popup opened.
#[derive(Clone)]
pub(crate) struct SwitcherRow {
    pub key: ServerKey,
    pub badge: ServerBadge,
    /// 0-based place in the config: `Ctrl-<position + 1>` shows it.
    pub position: usize,
    /// The user and address, as shown under the name.
    pub detail: String,
    /// What filtering matches: name, address and user.
    pub haystack: String,
    pub connection: Connection,
    /// Drafts of this server that are not on it yet (pending and failed).
    pub drafts: usize,
    pub active: bool,
}

/// Rows whose name, address or user contain every word of `query`, ignoring
/// case. Indexes into `rows`.
pub(crate) fn filter_rows(rows: &[SwitcherRow], query: &str) -> Vec<usize> {
    let query = query.to_lowercase();
    let words: Vec<&str> = query.split_whitespace().collect();
    rows.iter()
        .enumerate()
        .filter(|(_, r)| words.iter().all(|w| r.haystack.contains(w)))
        .map(|(i, _)| i)
        .collect()
}

/// The word for a connection that needs the user's attention.
pub(crate) fn marker_text(connection: Connection) -> Option<&'static str> {
    match connection {
        Connection::Offline => Some("offline"),
        Connection::Unauthorized => Some("sign-in needed"),
        Connection::Connecting | Connection::Online => None,
    }
}

fn marker_color(connection: Connection, cx: &App) -> Hsla {
    match connection {
        Connection::Unauthorized => cx.theme().danger,
        _ => cx.theme().warning,
    }
}

/// "3 drafts" and the like.
fn drafts_text(count: usize) -> String {
    match count {
        1 => "1 draft".to_owned(),
        n => format!("{n} drafts"),
    }
}

/// The chip in the title bar: the dot and name of the active server, and a
/// warning glyph if it is offline or refused the credentials. The caller
/// adds the click handler.
pub(crate) fn chip(badge: &ServerBadge, connection: Connection, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    let marker = marker_text(connection).map(|_| marker_color(connection, cx));
    h_flex()
        .id("server-chip")
        .flex_none()
        .gap_1p5()
        .items_center()
        .h_6()
        .px_2()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .text_sm()
        .cursor_pointer()
        .hover(|d| d.bg(theme.list_hover))
        .child(dot(badge.color(cx)))
        .child(badge.name.clone())
        .when_some(marker, |d, color| {
            d.child(div().text_color(color).child("\u{26A0}"))
        })
}

pub enum SwitcherEvent {
    Close,
    Pick(ServerKey),
}

impl EventEmitter<SwitcherEvent> for ServerSwitcher {}

pub(crate) struct ServerSwitcher {
    rows: Vec<SwitcherRow>,
    input: Entity<InputState>,
    /// Indexes into `rows` that match the text, in order.
    visible: Vec<usize>,
    /// Index into `visible`.
    cursor: usize,
    scroll: ScrollHandle,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

impl ServerSwitcher {
    /// Opens the popup with the cursor on the active server and the focus in
    /// the filter box.
    pub fn new(rows: Vec<SwitcherRow>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Switch to a server"));
        let subs =
            vec![
                cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, _, cx| match ev {
                    InputEvent::Change => this.refilter(cx),
                    InputEvent::PressEnter { .. } => this.pick(cx),
                    _ => {}
                }),
            ];
        input.update(cx, |s, cx| s.focus(window, cx));
        let visible: Vec<usize> = (0..rows.len()).collect();
        let cursor = rows.iter().position(|r| r.active).unwrap_or(0);
        Self {
            rows,
            input,
            visible,
            cursor,
            scroll: ScrollHandle::new(),
            focus: cx.focus_handle(),
            _subs: subs,
        }
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value();
        self.visible = filter_rows(&self.rows, &text);
        self.cursor = 0;
        self.scroll.scroll_to_item(0);
        cx.notify();
    }

    fn pick(&mut self, cx: &mut Context<Self>) {
        if let Some(row) = self.visible.get(self.cursor).map(|&i| &self.rows[i]) {
            cx.emit(SwitcherEvent::Pick(row.key.clone()));
        }
    }

    fn move_cursor(&mut self, forward: bool, cx: &mut Context<Self>) {
        let n = self.visible.len();
        if n == 0 {
            return;
        }
        self.cursor = if forward {
            (self.cursor + 1) % n
        } else {
            (self.cursor + n - 1) % n
        };
        self.scroll.scroll_to_item(self.cursor);
        cx.notify();
    }

    fn next(&mut self, _: &SwitcherNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(true, cx);
    }

    fn prev(&mut self, _: &SwitcherPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(false, cx);
    }

    fn close(&mut self, _: &CloseSwitcher, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(SwitcherEvent::Close);
    }

    #[cfg(test)]
    pub(crate) fn type_for_test(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.input
            .update(cx, |s, cx| s.set_value(text.to_owned(), window, cx));
        self.refilter(cx);
    }

    #[cfg(test)]
    pub(crate) fn rows_for_test(&self) -> &[SwitcherRow] {
        &self.rows
    }

    /// The keys of the listed servers, in order, and which one the cursor
    /// is on.
    #[cfg(test)]
    pub(crate) fn listed_for_test(&self) -> (Vec<String>, Option<String>) {
        let keys = self
            .visible
            .iter()
            .map(|&i| self.rows[i].key.to_string())
            .collect();
        let at = self
            .visible
            .get(self.cursor)
            .map(|&i| self.rows[i].key.to_string());
        (keys, at)
    }

    fn render_row(&self, ix: usize, row: &SwitcherRow, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let selected = ix == self.cursor;
        let key = row.key.clone();
        let hover = theme.list_hover;
        let marker = marker_text(row.connection).map(|t| (t, marker_color(row.connection, cx)));
        h_flex()
            .id(("server-row", ix))
            .gap_3()
            .px_3()
            .py_1p5()
            .items_center()
            .rounded_md()
            .cursor_pointer()
            .when(selected, |d| d.bg(theme.list_active))
            .hover(move |d| d.bg(hover))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(SwitcherEvent::Pick(key.clone()))))
            .child(dot(row.badge.color(cx)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_baseline()
                            .child(
                                div()
                                    .font_semibold()
                                    .truncate()
                                    .child(row.badge.name.clone()),
                            )
                            .when(row.active, |d| {
                                d.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("current"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(row.detail.clone()),
                    ),
            )
            .when_some(marker, |d, (text, color)| {
                d.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(color)
                        .child(format!("\u{26A0} {text}")),
                )
            })
            .when(row.drafts > 0, |d| {
                d.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(drafts_text(row.drafts)),
                )
            })
            .when(row.position < 9, |d| {
                d.child(
                    div()
                        .flex_none()
                        .w_12()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("Ctrl-{}", row.position + 1)),
                )
            })
            .into_any_element()
    }
}

impl Render for ServerSwitcher {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut rows = Vec::new();
        for (ix, &i) in self.visible.iter().enumerate() {
            rows.push(self.render_row(ix, &self.rows[i], cx));
        }
        div()
            .id("switcher-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(120.))
            .bg(crate::theme::modal_backdrop(cx))
            .child(
                v_flex()
                    .id("switcher")
                    .key_context(SERVER_SWITCHER)
                    .track_focus(&self.focus)
                    .on_action(cx.listener(Self::close))
                    .on_action(cx.listener(Self::next))
                    .on_action(cx.listener(Self::prev))
                    .w(px(560.))
                    .max_w_full()
                    .gap_2()
                    .p_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .text_color(theme.popover_foreground)
                    .text_sm()
                    .child(Input::new(&self.input))
                    .child(
                        v_flex()
                            .id("switcher-list")
                            .max_h(px(360.))
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .gap_0p5()
                            .children(rows)
                            .when(self.visible.is_empty(), |d| {
                                d.child(
                                    div()
                                        .p_2()
                                        .text_color(theme.muted_foreground)
                                        .child("No server matches."),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Enter: show, Esc: close"),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{Connection, ServerKey, SwitcherRow, filter_rows, marker_text};

    fn row(key: &str, name: &str, url: &str, user: Option<&str>) -> SwitcherRow {
        use crate::servers::Servers;
        use crate::session::ServerSetup;
        let servers = Servers::new(
            vec![
                ServerSetup {
                    key: ServerKey::new(key),
                    name: name.into(),
                    base_url: url.into(),
                    username: user.map(str::to_owned),
                    ..crate::tests::setup()
                },
                ServerSetup {
                    key: ServerKey::new("other"),
                    ..crate::tests::setup()
                },
            ],
            None,
        );
        SwitcherRow {
            key: ServerKey::new(key),
            badge: servers.badge(&ServerKey::new(key)).expect("badge"),
            position: 0,
            detail: String::new(),
            haystack: format!("{name} {url} {}", user.unwrap_or_default()).to_lowercase(),
            connection: Connection::Online,
            drafts: 0,
            active: false,
        }
    }

    #[test]
    fn filtering_matches_name_address_and_user_by_words() {
        let rows = vec![
            row("a", "Work", "https://gerrit.corp.example", Some("alice")),
            row("b", "Android", "https://android-review.example.org", None),
        ];
        assert_eq!(filter_rows(&rows, ""), [0, 1]);
        assert_eq!(filter_rows(&rows, "wor"), [0]);
        assert_eq!(filter_rows(&rows, "REVIEW"), [1]);
        assert_eq!(filter_rows(&rows, "alice corp"), [0]);
        assert_eq!(filter_rows(&rows, "example"), [0, 1]);
        assert!(filter_rows(&rows, "nothing").is_empty());
    }

    #[test]
    fn only_trouble_gets_a_marker() {
        assert_eq!(marker_text(Connection::Online), None);
        assert_eq!(marker_text(Connection::Connecting), None);
        assert!(marker_text(Connection::Offline).is_some());
        assert!(marker_text(Connection::Unauthorized).is_some());
    }
}
