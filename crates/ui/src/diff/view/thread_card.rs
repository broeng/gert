//! The comment thread rows of the diff view: one card per thread, between
//! the code lines. Plain text only (no markdown), in the UI font.

use gpui_kit::component::button::Button;
use gpui_kit::component::{Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use jiff::{Timestamp, tz::TimeZone};
use model::{DiffSide, DiffViewMode, DraftSync, Entry, Location, Thread};

use super::{DiffEvent, DiffView};
use crate::diff::{
    layout::{LineKind, ThreadRow},
    paint::Style,
};
use crate::time_fmt::relative;

/// The element of a thread row: the card, in the pane of its side when the
/// diff is side by side (next to the gutter), full width when unified.
pub(super) fn thread_row(
    view: &DiffView,
    ix: usize,
    tr: &ThreadRow,
    style: &Style,
    cx: &mut Context<DiffView>,
) -> AnyElement {
    let Some(thread) = view.thread_at(tr.slot) else {
        return div().into_any_element();
    };
    let open = view.is_thread_open(&thread.id);
    let selected = ix == view.cursor;
    let card = card(thread, tr, (open, selected), ix, style, cx);
    let gutter = |width: Pixels| {
        div()
            .flex_none()
            .w(width)
            .bg(style.palette.gutter_bg(LineKind::Common))
            .when(style.hide_numbers, |d| d.hidden())
    };
    let slot = |card: AnyElement| div().flex_1().min_w_0().child(card);
    match view.prefs.view_mode {
        DiffViewMode::SideBySide => {
            let half = |card: Option<AnyElement>, width: Pixels| {
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .items_stretch()
                    .child(gutter(width))
                    .children(card.map(slot))
            };
            let (left, right) = match tr.side {
                DiffSide::Left => (Some(card), None),
                DiffSide::Right => (None, Some(card)),
            };
            h_flex()
                .w_full()
                .items_stretch()
                .child(half(left, style.old_gutter))
                .child(
                    half(right, style.new_gutter)
                        .border_l_1()
                        .border_color(style.palette.divider),
                )
                .into_any_element()
        }
        DiffViewMode::Unified => h_flex()
            .w_full()
            .items_stretch()
            .child(gutter(style.old_gutter + style.new_gutter))
            .child(slot(card))
            .into_any_element(),
    }
}

/// Where a thread is, when the row alone does not say: at the top of the
/// file instead of next to its line.
fn location_label(thread: &Thread, tr: &ThreadRow) -> Option<String> {
    match (thread.location, tr.detached) {
        (Location::File, _) => Some("File comment".to_owned()),
        (loc, true) => loc
            .line()
            .map(|l| format!("Line {l}, which is not in this file")),
        _ => None,
    }
}

fn card(
    thread: &Thread,
    tr: &ThreadRow,
    (open, selected): (bool, bool),
    ix: usize,
    style: &Style,
    cx: &mut Context<DiffView>,
) -> AnyElement {
    let p = &style.palette;
    let accent = if thread.is_unresolved() {
        p.unresolved
    } else {
        p.resolved
    };
    let muted = p.muted_fg;
    let now = Timestamp::now();
    let tz = TimeZone::system();
    let count = thread.entries.len();
    let location = location_label(thread, tr);

    let status = div()
        .flex_none()
        .px_1p5()
        .rounded_sm()
        .text_xs()
        .bg(accent.opacity(0.22))
        .child(if thread.is_unresolved() {
            "Unresolved"
        } else {
            "Resolved"
        });
    let pill = |label: &'static str| {
        div()
            .flex_none()
            .px_1p5()
            .rounded_sm()
            .text_xs()
            .bg(p.card_fg.opacity(0.12))
            .child(label)
    };
    let first = thread.entries.first();
    let header = h_flex()
        .id(("thread-header", ix))
        .gap_2()
        .px_2()
        .py_1()
        .items_center()
        .child(
            div()
                .flex_none()
                .w(px(10.))
                .text_color(muted)
                .child(if open { "\u{25BE}" } else { "\u{25B8}" }),
        )
        .when_some(location, |d, l| {
            d.child(div().flex_none().text_color(muted).child(l))
        })
        .when(!open, |d| {
            let author = first.map_or_else(String::new, author_name);
            let line = first
                .and_then(|e| e.message().lines().find(|l| !l.trim().is_empty()))
                .unwrap_or("")
                .to_owned();
            d.child(div().flex_none().font_semibold().child(author))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(muted)
                        .child(line),
                )
        })
        .when(open, |d| d.child(div().flex_1()))
        .when(count > 1, |d| {
            d.child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("{count} comments")),
            )
        })
        .when(thread.has_draft(), |d| d.child(pill("Draft")))
        .child(status);

    let entries = open.then(|| {
        v_flex().children(
            thread
                .entries
                .iter()
                .enumerate()
                .map(|(k, e)| entry(e, (ix, k), now, &tz, style, cx)),
        )
    });
    v_flex()
        .id(("thread", ix))
        .mx_2()
        .my_1()
        .rounded_md()
        .border_1()
        .border_color(if selected {
            p.cursor_bar
        } else {
            accent.opacity(0.55)
        })
        .bg(accent.opacity(0.09))
        .text_color(p.card_fg)
        .font_family(style.ui_family.clone())
        .text_size(style.ui_size)
        .line_height((style.ui_size * 1.45).round())
        .overflow_hidden()
        .child(header)
        .children(entries)
        .into_any_element()
}

fn author_name(e: &Entry) -> String {
    match e {
        Entry::Published(c) => c
            .author
            .as_ref()
            .map_or_else(|| "Unknown".to_owned(), |a| a.display()),
        // Drafts are the signed-in user's own.
        Entry::Draft(_) => "You".to_owned(),
    }
}

/// One comment or draft: who and when, the text, and for a draft where it
/// stands with the server.
fn entry(
    e: &Entry,
    (ix, k): (usize, usize),
    now: Timestamp,
    tz: &TimeZone,
    style: &Style,
    cx: &mut Context<DiffView>,
) -> Div {
    let p = &style.palette;
    let muted = p.muted_fg;
    let draft = match e {
        Entry::Draft(d) => Some(d),
        Entry::Published(_) => None,
    };
    let sync = draft.map(|d| match &d.sync {
        DraftSync::Synced | DraftSync::PendingDelete => None,
        DraftSync::PendingCreate | DraftSync::PendingUpdate => {
            let text = match &d.last_error {
                Some(err) => format!("Not uploaded yet ({err})"),
                None => "Not uploaded yet".to_owned(),
            };
            Some(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(text)
                    .into_any_element(),
            )
        }
        DraftSync::Failed {
            error, retryable, ..
        } => {
            let id = d.local_id;
            Some(
                h_flex()
                    .gap_2()
                    .items_center()
                    .text_xs()
                    .text_color(p.danger)
                    .child(div().min_w_0().child(format!("Upload failed: {error}")))
                    .child(if *retryable {
                        Button::new(("retry-draft", ix * 1000 + k))
                            .label("Retry")
                            .xsmall()
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.stop_propagation();
                                cx.emit(DiffEvent::RetryDraft(id));
                            }))
                            .into_any_element()
                    } else {
                        div().child("Edit or delete the draft.").into_any_element()
                    })
                    .into_any_element(),
            )
        }
    });
    let lines = e.message().lines().map(|line| {
        if line.trim().is_empty() {
            div().h(px(6.))
        } else {
            div().child(line.to_owned())
        }
    });
    v_flex()
        .px_2()
        .py_1()
        .gap_0p5()
        .when(k > 0, |d| {
            d.border_t_1().border_color(p.card_fg.opacity(0.12))
        })
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(div().font_semibold().child(author_name(e)))
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(relative(now, e.updated(), tz)),
                )
                .when(draft.is_some(), |d| {
                    d.child(
                        div()
                            .px_1p5()
                            .rounded_sm()
                            .text_xs()
                            .bg(p.card_fg.opacity(0.12))
                            .child("Draft"),
                    )
                }),
        )
        .children(lines)
        .children(sync.flatten())
}
