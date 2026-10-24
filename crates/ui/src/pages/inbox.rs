//! The notification inbox: every event that matched a rule, newest first
//! in day groups, across all servers (the one page that is not about a
//! single server: notifications from the others are the point of it).
//!
//! The shell hands over the rows read from the store and applies what the
//! page asks for ([`InboxOp`]); the page changes its own copy at once, so
//! a key never waits for the database.

use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use jiff::{Timestamp, tz::TimeZone};
use model::ChangeNumber;
use store::{InboxId, InboxItem, ServerId};
use sync::notify::text;

use crate::{
    actions::*,
    hint::Hint,
    inbox::InboxOp,
    servers::{ServerBadge, dot},
    session::ServerKey,
    time_fmt::{day_label, relative},
};

/// A configured server as the inbox names it.
#[derive(Clone, Debug)]
pub struct InboxServer {
    pub id: ServerId,
    pub key: ServerKey,
    /// The name pill; `None` with a single server.
    pub badge: Option<ServerBadge>,
}

/// What the page asks of the shell.
pub enum InboxEvent {
    /// Change the store; the page already shows the result.
    Op(InboxOp),
    /// Open the change on its server.
    Open {
        server: ServerKey,
        change: ChangeNumber,
    },
}

pub struct InboxPage {
    servers: Vec<InboxServer>,
    /// Newest first. Empty until the shell has read the store.
    items: Vec<InboxItem>,
    loaded: bool,
    cursor: usize,
    focus: FocusHandle,
    scroll: ScrollHandle,
    reveal: bool,
    /// Shift-D was pressed once: the next one dismisses the read ones.
    confirming_clear: bool,
    hint: Hint,
}

impl EventEmitter<InboxEvent> for InboxPage {}

impl Focusable for InboxPage {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl InboxPage {
    pub fn new(servers: Vec<InboxServer>, cx: &mut Context<Self>) -> Self {
        Self {
            servers,
            items: Vec::new(),
            loaded: false,
            cursor: 0,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            reveal: false,
            confirming_clear: false,
            hint: Hint::default(),
        }
    }

    /// The rows as the store has them now. The cursor stays on the same
    /// notification if it is still there.
    pub fn set_items(&mut self, items: Vec<InboxItem>, cx: &mut Context<Self>) {
        let at = self.items.get(self.cursor).map(|i| i.id);
        self.items = items;
        self.loaded = true;
        self.cursor = at
            .and_then(|id| self.items.iter().position(|i| i.id == id))
            .unwrap_or(self.cursor)
            .min(self.items.len().saturating_sub(1));
        cx.notify();
    }

    fn current(&self) -> Option<&InboxItem> {
        self.items.get(self.cursor)
    }

    fn server_key(&self, id: ServerId) -> Option<ServerKey> {
        self.servers
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.key.clone())
    }

    fn badge(&self, id: ServerId) -> Option<&ServerBadge> {
        self.servers.iter().find(|s| s.id == id)?.badge.as_ref()
    }

    /// A key other than the confirming one cancels the question.
    fn settle(&mut self) {
        self.confirming_clear = false;
    }

    fn moved(&mut self, to: usize, cx: &mut Context<Self>) {
        self.settle();
        self.cursor = to.min(self.items.len().saturating_sub(1));
        self.reveal = true;
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.moved(self.cursor + 1, cx);
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.moved(self.cursor.saturating_sub(1), cx);
    }

    fn open_selected(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
        self.settle();
        self.open_at(self.cursor, cx);
    }

    fn open_at(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(item) = self.items.get(index) else {
            return;
        };
        let (id, change, server) = (item.id, item.event.change, item.server);
        let unread = item.read_at.is_none();
        let Some(key) = self.server_key(server) else {
            self.hint
                .say("That server is not configured any more.", true, cx, |p| {
                    &mut p.hint
                });
            return;
        };
        if unread {
            self.set_read(id, true);
            cx.emit(InboxEvent::Op(InboxOp::Read(vec![id])));
        }
        cx.emit(InboxEvent::Open {
            server: key,
            change,
        });
        cx.notify();
    }

    fn set_read(&mut self, id: InboxId, read: bool) {
        if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
            item.read_at = read.then(Timestamp::now);
        }
    }

    fn toggle_read(&mut self, _: &ToggleInboxRead, _: &mut Window, cx: &mut Context<Self>) {
        self.settle();
        let Some(item) = self.current() else {
            return;
        };
        let (id, was_read) = (item.id, item.read_at.is_some());
        self.set_read(id, !was_read);
        cx.emit(InboxEvent::Op(if was_read {
            InboxOp::Unread(vec![id])
        } else {
            InboxOp::Read(vec![id])
        }));
        cx.notify();
    }

    fn mark_all_read(&mut self, _: &MarkAllInboxRead, _: &mut Window, cx: &mut Context<Self>) {
        self.settle();
        let now = Timestamp::now();
        for item in self.items.iter_mut().filter(|i| i.read_at.is_none()) {
            item.read_at = Some(now);
        }
        cx.emit(InboxEvent::Op(InboxOp::ReadAll));
        cx.notify();
    }

    fn dismiss(&mut self, _: &DismissInboxItem, _: &mut Window, cx: &mut Context<Self>) {
        self.settle();
        if self.cursor >= self.items.len() {
            return;
        }
        let item = self.items.remove(self.cursor);
        self.cursor = self.cursor.min(self.items.len().saturating_sub(1));
        cx.emit(InboxEvent::Op(InboxOp::Dismiss(vec![item.id])));
        cx.notify();
    }

    fn clear_read(&mut self, _: &ClearReadInbox, _: &mut Window, cx: &mut Context<Self>) {
        let read = self.items.iter().filter(|i| i.read_at.is_some()).count();
        if read == 0 {
            self.settle();
            self.hint.say(
                "There are no read notifications to dismiss.",
                false,
                cx,
                |p| &mut p.hint,
            );
            return;
        }
        if !self.confirming_clear {
            self.confirming_clear = true;
            let plural = if read == 1 { "" } else { "s" };
            self.hint.say(
                format!("Press Shift-D again to dismiss {read} read notification{plural}."),
                false,
                cx,
                |p| &mut p.hint,
            );
            return;
        }
        self.settle();
        let at = self.current().map(|i| i.id);
        self.items.retain(|i| i.read_at.is_none());
        self.cursor = at
            .and_then(|id| self.items.iter().position(|i| i.id == id))
            .unwrap_or(0)
            .min(self.items.len().saturating_sub(1));
        cx.emit(InboxEvent::Op(InboxOp::ClearRead));
        cx.notify();
    }

    fn unread(&self) -> usize {
        self.items.iter().filter(|i| i.read_at.is_none()).count()
    }

    #[cfg(test)]
    pub(crate) fn items_for_test(&self) -> &[InboxItem] {
        &self.items
    }

    #[cfg(test)]
    pub(crate) fn loaded_for_test(&self) -> bool {
        self.loaded
    }

    #[cfg(test)]
    pub(crate) fn cursor_for_test(&self) -> usize {
        self.cursor
    }

    #[cfg(test)]
    pub(crate) fn groups_for_test(&self) -> Vec<(String, usize)> {
        let (now, tz) = (Timestamp::now(), TimeZone::system());
        groups(&self.items, now, &tz)
            .into_iter()
            .map(|g| (g.label, g.len))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn hint_for_test(&self) -> Option<(&str, bool)> {
        self.hint.current()
    }
}

/// Consecutive items that fall on the same day.
struct Group {
    label: String,
    len: usize,
}

fn groups(items: &[InboxItem], now: Timestamp, tz: &TimeZone) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for item in items {
        let label = day_label(now, item.created_at, tz);
        match groups.last_mut() {
            Some(g) if g.label == label => g.len += 1,
            _ => groups.push(Group { label, len: 1 }),
        }
    }
    groups
}

impl InboxPage {
    fn render_row(
        &self,
        index: usize,
        item: &InboxItem,
        now: Timestamp,
        tz: &TimeZone,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let selected = index == self.cursor;
        let unread = item.read_at.is_none();
        let muted = theme.muted_foreground;
        let body = text::body(&item.event);
        let (line, extra) = match body.split_once('\n') {
            Some((line, extra)) => (line.to_owned(), Some(extra.to_owned())),
            None => (body, None),
        };
        h_flex()
            .id(("inbox-row", index))
            .px_3()
            .py_1p5()
            .gap_2()
            .items_start()
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
                this.settle();
                this.cursor = index;
                this.open_at(index, cx);
            }))
            .child(div().pt(px(5.)).child(dot(if unread {
                theme.primary
            } else {
                theme.transparent
            })))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .when(unread, |d| d.font_semibold())
                            .child(text::title(&item.event, item.mention)),
                    )
                    .child(div().truncate().text_color(muted).child(line))
                    .children(extra.map(|e| div().truncate().text_xs().text_color(muted).child(e))),
            )
            .child(
                v_flex()
                    .flex_none()
                    .items_end()
                    .text_xs()
                    .text_color(muted)
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .children(self.badge(item.server).map(|b| b.pill(cx)))
                            .child(relative(now, item.created_at, tz)),
                    )
                    .child(
                        div()
                            .max_w(px(260.))
                            .truncate()
                            .child(item.rule_title.clone()),
                    ),
            )
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_1()
            .child(div().text_lg().child("No notifications yet"))
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child("Choose what you are told about in Settings, Notifications."),
            )
    }
}

impl Render for InboxPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let (now, tz) = (Timestamp::now(), TimeZone::system());
        let groups = groups(&self.items, now, &tz);

        let mut body = v_flex()
            .id("inbox-scroll")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .pb_4();
        let (mut index, mut child) = (0, 0);
        let mut scroll_to = None;
        for g in &groups {
            let title = div()
                .child(
                    crate::card::title_bar(cx)
                        .text_sm()
                        .font_semibold()
                        .child(g.label.clone()),
                )
                .into_any_element();
            body = body.child(crate::card::frame(title, true, false, cx));
            child += 1;
            for r in 0..g.len {
                if index == self.cursor {
                    scroll_to = Some(child);
                }
                let row = self
                    .render_row(index, &self.items[index], now, &tz, cx)
                    .into_any_element();
                body = body.child(crate::card::frame(
                    crate::card::row_separator(cx).child(row),
                    false,
                    r + 1 == g.len,
                    cx,
                ));
                index += 1;
                child += 1;
            }
        }
        if self.reveal {
            self.reveal = false;
            if let Some(child) = scroll_to {
                self.scroll.scroll_to_item(child);
            }
        }

        let unread = self.unread();
        let header = v_flex().flex_none().px_4().pt_4().pb_2().child(
            h_flex()
                .gap_2()
                .items_baseline()
                .child(div().text_xl().font_semibold().child("Notifications"))
                .when(unread > 0, |d| {
                    d.child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(format!("{unread} unread")),
                    )
                }),
        );
        v_flex()
            .id("inbox")
            .key_context(INBOX_PAGE)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::toggle_read))
            .on_action(cx.listener(Self::mark_all_read))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::clear_read))
            .size_full()
            .child(header)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .when(self.loaded && self.items.is_empty(), |d| {
                        d.child(self.render_empty(cx))
                    })
                    .when(!self.items.is_empty(), |d| {
                        d.child(body).vertical_scrollbar(&self.scroll)
                    }),
            )
            .children(self.hint.current().map(|(text, error)| {
                div()
                    .px_4()
                    .py_1()
                    .text_xs()
                    .text_color(if error {
                        theme.danger
                    } else {
                        theme.foreground
                    })
                    .child(text.to_owned())
            }))
            .child(
                div()
                    .px_4()
                    .py_1()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(
                        "j / k move, Enter open, r read / unread, Shift-R all read, \
                         d dismiss, Shift-D dismiss read, Esc close",
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use jiff::{Timestamp, tz::TimeZone};
    use model::{ChangeNumber, Event, EventKind};
    use store::{InboxId, InboxItem, ServerId};

    use super::groups;

    fn item(id: i64, at: &str) -> InboxItem {
        let at: Timestamp = at.parse().expect("ts");
        InboxItem {
            id: InboxId(id),
            server: ServerId(1),
            event: Event {
                server: ServerId(1),
                change: ChangeNumber(1),
                project: "p".into(),
                subject: "s".into(),
                owner: None,
                actor: None,
                at,
                kind: EventKind::Merged,
                subject_account: None,
            },
            rule_title: "r".into(),
            mention: false,
            shown_on_os: false,
            created_at: at,
            read_at: None,
        }
    }

    #[test]
    fn items_group_by_local_day_in_order() {
        let now: Timestamp = "2024-06-15T12:00:00Z".parse().expect("ts");
        let items = [
            item(5, "2024-06-15T11:00:00Z"),
            item(4, "2024-06-15T01:00:00Z"),
            item(3, "2024-06-14T23:00:00Z"),
            item(2, "2024-06-14T01:00:00Z"),
            item(1, "2024-06-01T01:00:00Z"),
        ];
        let got: Vec<_> = groups(&items, now, &TimeZone::UTC)
            .into_iter()
            .map(|g| (g.label, g.len))
            .collect();
        assert_eq!(
            got,
            [
                ("Today".to_owned(), 2),
                ("Yesterday".to_owned(), 2),
                ("Jun 1".to_owned(), 1)
            ]
        );
        assert!(groups(&[], now, &TimeZone::UTC).is_empty());
    }
}
