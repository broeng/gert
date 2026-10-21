//! The comment thread rows of the diff view: one card per thread, between
//! the code lines. Plain text only (no markdown), in the UI font.

use gpui_kit::component::button::Button;
use gpui_kit::component::{Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use jiff::{Timestamp, tz::TimeZone};
use model::{DiffSide, DiffViewMode, DraftSync, Entry, Location, Thread};

use super::{
    DiffEvent, DiffView, Reveal,
    editing::{DraftRequest, EditKey, thread_has_key},
};
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
    let card = card(view, thread, tr, (open, selected), ix, style, cx);
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
    view: &DiffView,
    thread: &Thread,
    tr: &ThreadRow,
    (open, selected): (bool, bool),
    ix: usize,
    style: &Style,
    cx: &mut Context<DiffView>,
) -> AnyElement {
    let p = &style.palette;
    let editing = view
        .editor
        .as_ref()
        .is_some_and(|e| thread_has_key(thread, &e.key));
    let open = open || editing;
    // A new comment has no entries yet and no state.
    let is_new = thread.entries.is_empty();
    let accent = if thread.is_unresolved() || is_new {
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
        .when(!is_new, |d| {
            d.child(if thread.is_unresolved() {
                "Unresolved"
            } else {
                "Resolved"
            })
        })
        .when(is_new, |d| d.child("New comment"));
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
    let header = div()
        .id(("thread-header", ix))
        .test_support()
        .flex()
        .gap_2()
        .px_2()
        .py_1()
        .items_center()
        .cursor_pointer()
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
        .child(status)
        .on_click(cx.listener(move |this, _, window, cx| {
            cx.stop_propagation();
            window.focus(&this.focus, cx);
            this.move_to(ix, Reveal::Nearest, cx);
            this.toggle_thread(ix, cx);
        }));

    let entries = open.then(|| {
        v_flex()
            .children(
                thread
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(k, e)| entry(view, e, (ix, k), now, &tz, style, cx)),
            )
            // The editor of a reply or of a new comment, below the entries.
            .when(
                matches!(
                    view.editor.as_ref().map(|e| &e.key),
                    Some(EditKey::Reply(_) | EditKey::New(..))
                ) && editing,
                |d| {
                    d.child(
                        div()
                            .px_2()
                            .pb_2()
                            .children(view.editor_element(ix, style, cx)),
                    )
                },
            )
            .when(
                view.can_comment && !editing && thread.reply_target().is_some(),
                |d| d.child(thread_buttons(ix, cx)),
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
    view: &DiffView,
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
                                cx.emit(DiffEvent::Draft(DraftRequest::Retry(id)));
                            }))
                            .into_any_element()
                    } else {
                        div().child("Edit or delete the draft.").into_any_element()
                    })
                    .into_any_element(),
            )
        }
    });
    let editing_here = matches!(
        (draft, view.editor.as_ref()),
        (Some(d), Some(ed)) if ed.key == EditKey::Draft(d.local_id)
    );
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
        .when(!editing_here, |d| {
            d.children(lines).children(sync.flatten()).children(
                draft
                    .filter(|_| view.can_comment)
                    .map(|d| draft_buttons(view, d.local_id, (ix, k), cx)),
            )
        })
        .children(
            editing_here
                .then(|| view.editor_element(ix, style, cx))
                .flatten(),
        )
}

/// A button on a card. A click on it is not a click on the row.
fn card_button(id: impl Into<ElementId>, label: &'static str) -> Button {
    Button::new(id).label(label).xsmall()
}

/// Reply, Quote, Ack and Done under a thread's entries.
fn thread_buttons(ix: usize, cx: &mut Context<DiffView>) -> Div {
    h_flex()
        .gap_1()
        .px_2()
        .pb_1()
        .child(
            card_button(("thread-reply", ix), "Reply").on_click(cx.listener(
                move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.reply_to_thread_at(ix, window, cx);
                },
            )),
        )
        .child(
            card_button(("thread-quote", ix), "Quote").on_click(cx.listener(
                move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.quote_thread_at(ix, window, cx);
                },
            )),
        )
        .child(card_button(("thread-ack", ix), "Ack").on_click(cx.listener(
            move |this, _, _, cx| {
                cx.stop_propagation();
                this.quick_reply(ix, "Ack", cx);
            },
        )))
        .child(
            card_button(("thread-done", ix), "Done").on_click(cx.listener(
                move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.quick_reply(ix, "Done", cx);
                },
            )),
        )
}

/// Edit and Delete under a draft; Delete asks again on the card.
fn draft_buttons(
    view: &DiffView,
    id: model::LocalDraftId,
    (ix, k): (usize, usize),
    cx: &mut Context<DiffView>,
) -> Div {
    let key = ix * 1000 + k;
    if view.confirm_delete == Some(id) {
        return h_flex()
            .gap_1()
            .items_center()
            .child(div().text_xs().child("Delete this draft?"))
            .child(
                card_button(("draft-delete-yes", key), "Delete").on_click(cx.listener(
                    move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.answer_delete(id, ix, true, window, cx);
                    },
                )),
            )
            .child(
                card_button(("draft-delete-no", key), "Keep").on_click(cx.listener(
                    move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.answer_delete(id, ix, false, window, cx);
                    },
                )),
            );
    }
    h_flex()
        .gap_1()
        .child(
            card_button(("draft-edit", key), "Edit").on_click(cx.listener(
                move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.edit_draft_at(id, window, cx);
                },
            )),
        )
        .child(
            card_button(("draft-delete", key), "Delete").on_click(cx.listener(
                move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.ask_delete(id, ix, cx);
                },
            )),
        )
}
