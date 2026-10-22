//! The comments of the change page: the per-file counts in the file list,
//! the thread list with its filter and cursor, and the patch-set level
//! threads.
//!
//! The threads come from the change's shared [`ChangeComments`]; the page
//! keeps a copy ([`ChangePage::read_threads`]) and derives from it what it
//! shows: the listed threads, the counts, and what the patch-set level view
//! carries.
//!
//! Focus: the page keeps the focus. `shift-n` / `shift-p` move the thread
//! cursor (a different cursor from the file one, like the related changes'),
//! `x` or a click opens the thread: its diff page, with the cursor on it, or
//! for a patch-set level thread the card below the list, which takes the
//! focus (so `c`, `q`, `d` and the editor keys of the diff apply) until
//! `Esc` hands it back.

use gpui_kit::component::button::Button;
use gpui_kit::component::{
    ActiveTheme as _, Selectable as _, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use jiff::{Timestamp, tz::TimeZone};
use model::{DiffPair, Entry, FileCommentCounts, PATCHSET_LEVEL_PATH, ThreadId, comment_counts};

use super::{
    ChangeEvent, ChangePage,
    state::{Block, step},
    threads::{self, ThreadFilter},
    view::{pill, section_title},
};
use crate::{
    actions::*,
    diff::{
        DiffEvent, DiffSizing, DiffView,
        actions::{NextThread, PrevThread},
    },
    nav::DiffRoute,
    time_fmt::relative,
};

impl ChangePage {
    // ---- data ----

    /// Reads the shared comments again and updates what is derived from
    /// them. Call after the change, its files or the comments moved.
    pub(super) fn read_threads(&mut self, cx: &mut Context<Self>) {
        self.threads = self.comments.read(cx).threads();
        self.listed = threads::listed(&self.threads, self.filter());
        if let Some(id) = &self.thread_cursor
            && !self.listed.iter().any(|&i| &self.threads[i].id == id)
        {
            self.thread_cursor = None;
        }
        self.recount();
        self.sync_ps_view(cx);
        self.rebuild_blocks();
        cx.notify();
    }

    /// The filter in effect: the user's pick, else unresolved threads (or
    /// all if none is unresolved).
    pub(super) fn filter(&self) -> ThreadFilter {
        self.thread_filter
            .unwrap_or_else(|| ThreadFilter::default_for(&self.threads))
    }

    /// The comment numbers per file for the pair being compared.
    pub(super) fn recount(&mut self) {
        let pair = self.detail.as_ref().and_then(|d| {
            Some(DiffPair {
                base: self.sel.base_number(d),
                target: self.sel.target_number(d)?,
            })
        });
        self.counts = match pair {
            Some(pair) => comment_counts(&self.threads, pair, &self.files.list),
            None => Default::default(),
        };
    }

    /// Hands the threads to the patch-set level view, making it first.
    fn sync_ps_view(&mut self, cx: &mut Context<Self>) {
        let Some(current) = self.detail.as_ref().and_then(|d| d.current_patch_set) else {
            return;
        };
        let can_comment = self.session.can_comment();
        let threads = self.threads.clone();
        let view = match &self.ps_view {
            Some(v) => v.clone(),
            None => {
                let (prefs, font) = {
                    let s = self.settings.read(cx);
                    (s.resolved(), s.font_family())
                };
                let view = cx.new(|cx| {
                    let mut v = DiffView::threads_only(prefs, cx);
                    v.set_sizing(DiffSizing::Inline { max_rows: None }, cx);
                    v.set_font_family(font, cx);
                    v
                });
                self.ps_sub =
                    Some(cx.subscribe(&view, |this, _, ev: &DiffEvent, cx| this.ps_event(ev, cx)));
                self.ps_view = Some(view.clone());
                view
            }
        };
        view.update(cx, |v, cx| {
            v.set_can_comment(can_comment, cx);
            v.set_patchset_threads(current, threads, cx);
        });
    }

    fn ps_event(&mut self, ev: &DiffEvent, cx: &mut Context<Self>) {
        match ev {
            DiffEvent::Draft(request) => {
                let (change, request) = (self.number, request.clone());
                let detail = self.detail.clone();
                self.outbox
                    .update(cx, |o, cx| o.request(change, request, detail.as_ref(), cx));
            }
            DiffEvent::RowsChanged => {
                self.remeasure(Block::PatchSetComments);
                cx.notify();
            }
            DiffEvent::ModeChanged(_) => {}
        }
    }

    // ---- actions ----

    fn set_filter(&mut self, filter: ThreadFilter, cx: &mut Context<Self>) {
        self.thread_filter = Some(filter);
        self.read_threads(cx);
    }

    pub(super) fn threads_unresolved(
        &mut self,
        _: &ThreadsUnresolved,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_filter(ThreadFilter::Unresolved, cx);
    }

    pub(super) fn threads_drafts(
        &mut self,
        _: &ThreadsDrafts,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_filter(ThreadFilter::Drafts, cx);
    }

    pub(super) fn threads_all(&mut self, _: &ThreadsAll, _: &mut Window, cx: &mut Context<Self>) {
        self.set_filter(ThreadFilter::All, cx);
    }

    /// The position of the thread cursor in the list.
    fn thread_pos(&self) -> Option<usize> {
        let id = self.thread_cursor.as_ref()?;
        self.listed.iter().position(|&i| &self.threads[i].id == id)
    }

    fn move_thread(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.listed.is_empty() {
            return;
        }
        let n = match self.thread_pos() {
            Some(p) => step(p, self.listed.len(), delta),
            None if delta > 0 => 0,
            None => self.listed.len() - 1,
        };
        self.thread_cursor = Some(self.threads[self.listed[n]].id.clone());
        if let Some(ix) = self.block_index(Block::Thread(n)) {
            self.list.scroll_to_reveal_item(ix);
        }
        cx.notify();
    }

    pub(super) fn next_thread(&mut self, _: &NextThread, _: &mut Window, cx: &mut Context<Self>) {
        self.move_thread(1, cx);
    }

    pub(super) fn prev_thread(&mut self, _: &PrevThread, _: &mut Window, cx: &mut Context<Self>) {
        self.move_thread(-1, cx);
    }

    pub(super) fn open_thread_action(
        &mut self,
        _: &OpenThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.thread_cursor.clone() {
            self.open_thread(&id, window, cx);
        }
    }

    /// Opens a thread: its diff, or for a patch-set level thread its card.
    fn open_thread(&mut self, id: &ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.threads.iter().find(|t| &t.id == id).cloned() else {
            return;
        };
        self.thread_cursor = Some(id.clone());
        if thread.path == PATCHSET_LEVEL_PATH {
            if let Some(view) = self.ps_view.clone() {
                view.update(cx, |v, cx| v.focus_thread(id, cx));
                window.focus(&view.read(cx).focus_handle(cx), cx);
                self.reveal_block(Block::PatchSetComments);
            }
            cx.notify();
            return;
        }
        let Some(d) = &self.detail else {
            return;
        };
        let Some(target) = self.sel.target_number(d) else {
            return;
        };
        let current = DiffPair {
            base: self.sel.base_number(d),
            target,
        };
        let file = self
            .files
            .list
            .iter()
            .find(|f| f.path == thread.path)
            .or_else(|| {
                self.files
                    .list
                    .iter()
                    .find(|f| f.old_path.as_deref() == Some(&thread.path))
            });
        let (base, target, path) = threads::diff_for(&thread, current, file);
        cx.emit(ChangeEvent::OpenDiff(DiffRoute {
            change: self.number,
            base,
            target,
            path,
            thread: Some(id.clone()),
        }));
    }

    pub(super) fn new_patchset_comment_action(
        &mut self,
        _: &NewPatchSetComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.ps_view.clone() else {
            return;
        };
        self.reveal_block(Block::PatchSetComments);
        view.update(cx, |v, cx| v.new_patchset_comment(window, cx));
        cx.notify();
    }

    fn reveal_block(&mut self, block: Block) {
        if let Some(ix) = self.block_index(block) {
            self.list.scroll_to_reveal_item(ix);
        }
    }

    // ---- rendering ----

    pub(super) fn threads_header(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let filter = self.filter();
        let unresolved = self.threads.iter().filter(|t| t.is_unresolved()).count();
        let drafts = self.threads.iter().filter(|t| t.has_draft()).count();
        let count_of = |f: ThreadFilter| match f {
            ThreadFilter::Unresolved => unresolved,
            ThreadFilter::Drafts => drafts,
            ThreadFilter::All => self.threads.len(),
        };
        let button = |f: ThreadFilter, id: usize| {
            Button::new(("thread-filter", id))
                .label(format!("{} {}", f.label(), count_of(f)))
                .xsmall()
                .outline()
                .selected(filter == f)
                .on_click(cx.listener(move |this, _, _, cx| this.set_filter(f, cx)))
        };
        super::view::card_title_bar(cx)
            .flex()
            .flex_row()
            .gap_2()
            .items_center()
            .flex_wrap()
            .child(section_title("Comments", cx))
            .child(div().flex_1())
            .child(div().text_xs().text_color(muted).child("show"))
            .child(button(ThreadFilter::Unresolved, 0))
            .child(button(ThreadFilter::Drafts, 1))
            .child(button(ThreadFilter::All, 2))
            .into_any_element()
    }

    pub(super) fn threads_notice(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .py_1()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(self.filter().nothing())
            .into_any_element()
    }

    pub(super) fn thread_row(&mut self, pos: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(thread) = self.listed.get(pos).and_then(|&i| self.threads.get(i)) else {
            return div().into_any_element();
        };
        let thread = thread.clone();
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let selected = self.thread_cursor.as_ref() == Some(&thread.id);
        let (cursor_bg, cursor_bar) = if self.has_focus {
            (theme.list_active, theme.list_active_border)
        } else {
            (muted.opacity(0.10), muted)
        };
        let who = thread.entries.last().map_or_else(String::new, |e| match e {
            Entry::Published(c) => c
                .author
                .as_ref()
                .map_or_else(|| "Unknown".to_owned(), |a| a.display()),
            Entry::Draft(_) => "You".to_owned(),
        });
        let when = thread
            .last_updated()
            .map(|t| relative(Timestamp::now(), t, &TimeZone::system()))
            .unwrap_or_default();
        let id = thread.id.clone();
        h_flex()
            .id(("thread-row", pos))
            .test_support()
            .gap_2()
            .px_1()
            .py_0p5()
            .items_center()
            .text_sm()
            .border_l_2()
            .border_color(if selected {
                cursor_bar
            } else {
                theme.transparent
            })
            .when(selected, |d| d.bg(cursor_bg))
            .hover(|d| d.bg(theme.list_hover))
            .on_click(cx.listener(move |this, _, window, cx| this.open_thread(&id, window, cx)))
            .child(
                div()
                    .w(px(230.))
                    .flex_none()
                    .truncate()
                    .child(threads::location_text(&thread)),
            )
            .child(
                div()
                    .w(px(34.))
                    .flex_none()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("PS{}", thread.patch_set)),
            )
            .child(div().w(px(110.)).flex_none().truncate().child(who))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(muted)
                    .child(threads::last_line(&thread)),
            )
            .child(div().flex_none().text_xs().text_color(muted).child(when))
            .when(thread.has_draft(), |d| {
                d.child(pill("Draft", theme.primary))
            })
            .when(threads::has_failed_draft(&thread), |d| {
                d.child(pill("Upload failed", theme.danger))
            })
            .child(if thread.is_unresolved() {
                pill("Unresolved", theme.warning)
            } else {
                pill("Resolved", theme.success)
            })
            .into_any_element()
    }

    pub(super) fn patchset_comments(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let current = self.detail.as_ref().and_then(|d| d.current_patch_set);
        let can_comment = self.session.can_comment();
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(section_title("Patchset comments", cx))
                    .when_some(current, |d, n| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(format!("new ones go to patchset {n}")),
                        )
                    })
                    .child(div().flex_1())
                    .when(can_comment, |d| {
                        d.child(
                            Button::new("new-patchset-comment")
                                .label("New comment  (c)")
                                .xsmall()
                                .outline()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.new_patchset_comment_action(
                                        &NewPatchSetComment,
                                        window,
                                        cx,
                                    )
                                })),
                        )
                    }),
            )
            .children(self.ps_view.clone())
            .into_any_element()
    }
}

/// The comment numbers of a file in the file list: unresolved threads
/// prominent, then the comments and drafts the diff shows. A fixed-width
/// cell, so the columns after it line up.
pub(super) const COUNTS_W: f32 = 210.;

pub(super) fn counts_cell(counts: Option<FileCommentCounts>, cx: &App) -> Div {
    let theme = cx.theme();
    let cell = h_flex()
        .flex_none()
        .w(px(COUNTS_W))
        .gap_1()
        .justify_end()
        .items_center()
        .text_xs();
    let Some(c) = counts else {
        return cell;
    };
    cell.when(c.unresolved > 0, |d| {
        d.child(
            div()
                .flex_none()
                .px_1()
                .rounded_sm()
                .font_semibold()
                .text_color(theme.warning)
                .bg(theme.warning.opacity(0.18))
                .child(format!("{} unresolved", c.unresolved)),
        )
    })
    .when(c.comments > 0, |d| {
        d.child(
            div()
                .flex_none()
                .text_color(theme.muted_foreground)
                .child(if c.comments == 1 {
                    "1 comment".to_owned()
                } else {
                    format!("{} comments", c.comments)
                }),
        )
    })
    .when(c.drafts > 0, |d| {
        d.child(
            div()
                .flex_none()
                .px_1()
                .rounded_sm()
                .text_color(theme.primary)
                .bg(theme.primary.opacity(0.15))
                .child(if c.drafts == 1 {
                    "1 draft".to_owned()
                } else {
                    format!("{} drafts", c.drafts)
                }),
        )
    })
}
