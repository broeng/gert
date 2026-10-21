//! The diff widget: a virtualized, keyboard-driven view of a [`Diff`].
//!
//! All the work that depends on the diff happens in [`Layout::build`], once
//! per diff or mode change. Rendering only turns the visible slice of rows
//! into elements.

use std::{ops::Range, sync::Arc};

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use model::{Diff, DiffViewMode, ResolvedDiffPreferences};

use super::{
    actions::*,
    layout::{Anchor, Cell, Layout, LayoutOptions, LineKind, Row, SkipRow},
    paint::{Style, line_highlights},
};

/// How the widget takes its height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffSizing {
    /// Fill the parent and scroll inside (the diff page).
    Fill,
    /// Be as tall as the content, at most `max_rows` rows if set, and scroll
    /// inside only beyond that (inline under a file list entry). Without a
    /// cap the widget has no scroll of its own and the parent scrolls.
    Inline { max_rows: Option<usize> },
}

/// What the owner is asked to fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expand {
    /// Show these hidden lines: request a larger context (the server only
    /// sends whole `context=N` windows, so the owner picks N from the
    /// skipped range, or just asks for everything).
    Skip(SkipRow),
    /// Show the whole file (`context=ALL`).
    All,
}

/// A one-line message the owner can show above the rows (loading more
/// lines, a failed refetch, which context is shown).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffStatus {
    Busy(SharedString),
    Info(SharedString),
    Error(SharedString),
}

pub enum DiffEvent {
    ExpandRequested(Expand),
    /// The view mode was toggled; the owner may want to remember it.
    ModeChanged(DiffViewMode),
}

pub struct DiffView {
    diff: Arc<Diff>,
    prefs: ResolvedDiffPreferences,
    font_family: Option<SharedString>,
    sizing: DiffSizing,
    layout: Arc<Layout>,
    cursor: usize,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    /// Row height at the last render; also what the page size derives from.
    row_height: Pixels,
    /// Set by the owner.
    status: Option<DiffStatus>,
    /// The view's own message ("nothing is hidden"); goes away when the
    /// cursor moves or the diff changes. The owner's status wins.
    hint: Option<SharedString>,
}

impl EventEmitter<DiffEvent> for DiffView {}

impl Focusable for DiffView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

const DEFAULT_PAGE_ROWS: usize = 20;

impl DiffView {
    pub fn new(diff: Diff, prefs: ResolvedDiffPreferences, cx: &mut Context<Self>) -> Self {
        let layout = Arc::new(Layout::build(&diff, &LayoutOptions::from_prefs(&prefs)));
        let cursor = layout.first_selectable().unwrap_or(0);
        Self {
            diff: Arc::new(diff),
            prefs,
            font_family: None,
            sizing: DiffSizing::Fill,
            layout,
            cursor,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            row_height: px(20.),
            status: None,
            hint: None,
        }
    }

    /// Whether the diff has skipped runs of common lines that could be shown.
    pub fn has_hidden_lines(&self) -> bool {
        self.layout.rows().iter().any(|r| matches!(r, Row::Skip(_)))
    }

    /// Shows or clears the owner's status line.
    pub fn set_status(&mut self, status: Option<DiffStatus>, cx: &mut Context<Self>) {
        if self.status != status {
            self.status = status;
            cx.notify();
        }
    }

    pub fn status(&self) -> Option<&DiffStatus> {
        self.status.as_ref()
    }

    /// The message the view itself shows, if any.
    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn mode(&self) -> DiffViewMode {
        self.prefs.view_mode
    }

    /// Index of the row the line cursor is on.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The line numbers under the cursor.
    pub fn cursor_anchor(&self) -> Anchor {
        self.layout.anchor(self.cursor)
    }

    /// Replaces the diff (e.g. after an expansion), keeping the cursor on
    /// the same file line and at the same place on screen where possible.
    pub fn set_diff(&mut self, diff: Diff, cx: &mut Context<Self>) {
        let keep = self.keep_position();
        self.diff = Arc::new(diff);
        self.hint = None;
        self.rebuild(keep, cx);
    }

    /// Applies new preferences. Changing the view mode keeps the cursor on
    /// the same line.
    pub fn set_preferences(&mut self, prefs: ResolvedDiffPreferences, cx: &mut Context<Self>) {
        let old = LayoutOptions::from_prefs(&self.prefs);
        let mode_changed = prefs.view_mode != self.prefs.view_mode;
        self.prefs = prefs;
        if old == LayoutOptions::from_prefs(&self.prefs) {
            cx.notify();
            return;
        }
        let keep = self.keep_position();
        self.rebuild(keep, cx);
        if mode_changed {
            cx.emit(DiffEvent::ModeChanged(self.prefs.view_mode));
        }
    }

    pub fn set_mode(&mut self, mode: DiffViewMode, cx: &mut Context<Self>) {
        if mode != self.prefs.view_mode {
            let mut prefs = self.prefs.clone();
            prefs.view_mode = mode;
            self.set_preferences(prefs, cx);
        }
    }

    pub fn toggle_mode(&mut self, cx: &mut Context<Self>) {
        self.set_mode(self.prefs.view_mode.toggled(), cx);
    }

    /// Overrides the theme's monospace font; `None` goes back to it.
    pub fn set_font_family(&mut self, family: Option<SharedString>, cx: &mut Context<Self>) {
        self.font_family = family;
        cx.notify();
    }

    pub fn set_sizing(&mut self, sizing: DiffSizing, cx: &mut Context<Self>) {
        self.sizing = sizing;
        cx.notify();
    }

    /// Moves the cursor to `row` (clamped to a selectable row) and scrolls
    /// it into view.
    pub fn set_cursor(&mut self, row: usize, cx: &mut Context<Self>) {
        self.move_to(row, ScrollStrategy::Nearest, cx);
    }

    /// First row in the viewport, as of the last frame.
    #[cfg(test)]
    pub(crate) fn top_row_for_test(&self) -> usize {
        self.top_row()
    }

    #[cfg(test)]
    pub(crate) fn focus_for_test(&self) -> FocusHandle {
        self.focus.clone()
    }

    // --- internals -------------------------------------------------------

    /// Where the cursor is, in file lines and relative to the viewport top.
    fn keep_position(&self) -> (Anchor, usize) {
        let top = self.top_row();
        (
            self.layout.anchor(self.cursor),
            self.cursor.saturating_sub(top),
        )
    }

    fn rebuild(&mut self, (anchor, rel): (Anchor, usize), cx: &mut Context<Self>) {
        self.layout = Arc::new(Layout::build(
            &self.diff,
            &LayoutOptions::from_prefs(&self.prefs),
        ));
        let row = self
            .layout
            .find(anchor)
            .unwrap_or_else(|| self.cursor.min(self.layout.len().saturating_sub(1)));
        self.cursor = self.snap(row);
        let rel = rel.min(self.page_rows().saturating_sub(1));
        self.scroll
            .scroll_to_item_strict_with_offset(self.cursor, ScrollStrategy::Top, rel);
        cx.notify();
    }

    /// First row (at least partly) in the viewport.
    fn top_row(&self) -> usize {
        let y = -self.scroll.0.borrow().base_handle.offset().y;
        if y <= px(0.) || self.row_height <= px(0.) {
            return 0;
        }
        (y / self.row_height).floor() as usize
    }

    fn page_rows(&self) -> usize {
        let item = self.scroll.0.borrow().last_item_size;
        match item {
            Some(s) if self.row_height > px(0.) => {
                ((s.item.height / self.row_height).floor() as usize).max(1)
            }
            _ => DEFAULT_PAGE_ROWS,
        }
    }

    /// The nearest selectable row to `row`, looking forward first.
    fn snap(&self, row: usize) -> usize {
        let rows = self.layout.rows();
        let row = row.min(rows.len().saturating_sub(1));
        (row..rows.len())
            .chain((0..row).rev())
            .find(|&i| rows[i].selectable())
            .unwrap_or(0)
    }

    fn move_to(&mut self, row: usize, strategy: ScrollStrategy, cx: &mut Context<Self>) {
        if self.layout.is_empty() {
            return;
        }
        self.cursor = self.snap(row);
        self.hint = None;
        self.scroll.scroll_to_item(self.cursor, strategy);
        cx.notify();
    }

    /// Moves by `delta` selectable rows (rows that cannot hold the cursor
    /// are stepped over), stopping at the ends.
    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows = self.layout.rows();
        let mut i = self.cursor;
        let mut left = delta.unsigned_abs();
        while left > 0 {
            let next = if delta > 0 {
                i.checked_add(1).filter(|&n| n < rows.len())
            } else {
                i.checked_sub(1)
            };
            let Some(n) = next else { break };
            i = n;
            if rows[i].selectable() {
                left -= 1;
            }
        }
        self.move_to(i, ScrollStrategy::Nearest, cx);
    }

    fn expand_at(&mut self, row: usize, cx: &mut Context<Self>) {
        if let Some(Row::Skip(skip)) = self.layout.rows().get(row) {
            cx.emit(DiffEvent::ExpandRequested(Expand::Skip(skip.clone())));
        }
    }

    // --- actions ---------------------------------------------------------

    fn line_down(&mut self, _: &LineDown, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
    }

    fn line_up(&mut self, _: &LineUp, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-1, cx);
    }

    fn next_chunk(&mut self, _: &NextChunk, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.layout.next_chunk(self.cursor) {
            self.move_to(row, ScrollStrategy::Center, cx);
        }
    }

    fn prev_chunk(&mut self, _: &PrevChunk, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.layout.prev_chunk(self.cursor) {
            self.move_to(row, ScrollStrategy::Center, cx);
        }
    }

    fn page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.step(self.page_rows() as isize, cx);
    }

    fn page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-(self.page_rows() as isize), cx);
    }

    fn half_page_down(&mut self, _: &HalfPageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.step((self.page_rows() / 2).max(1) as isize, cx);
    }

    fn half_page_up(&mut self, _: &HalfPageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-((self.page_rows() / 2).max(1) as isize), cx);
    }

    fn cursor_top(&mut self, _: &CursorTop, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, ScrollStrategy::Nearest, cx);
    }

    fn cursor_bottom(&mut self, _: &CursorBottom, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(usize::MAX, ScrollStrategy::Nearest, cx);
    }

    fn toggle_mode_action(&mut self, _: &ToggleMode, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_mode(cx);
    }

    fn expand_skip(&mut self, _: &ExpandSkip, _: &mut Window, cx: &mut Context<Self>) {
        self.expand_at(self.cursor, cx);
    }

    fn show_full_file(&mut self, _: &ShowFullFile, _: &mut Window, cx: &mut Context<Self>) {
        // Without skipped runs a refetch would change nothing, which looks
        // like a dead key: say so instead.
        if self.has_hidden_lines() {
            self.hint = None;
            cx.emit(DiffEvent::ExpandRequested(Expand::All));
        } else {
            self.hint = Some("The whole file is already shown".into());
            cx.notify();
        }
    }

    fn click_row(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        self.move_to(row, ScrollStrategy::Nearest, cx);
        self.expand_at(row, cx);
    }

    // --- rendering -------------------------------------------------------

    fn render_rows(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let layout = self.layout.clone();
        let (max_old, max_new) = layout.max_numbers();
        let style = Style::new(
            self.font_family.clone(),
            self.prefs.line_length,
            self.prefs.hide_line_numbers,
            self.focus.is_focused(window),
            (max_old, max_new),
            window,
            cx,
        );
        let end = range.end.min(layout.len());
        let start = range.start.min(end);
        layout.rows()[start..end]
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let ix = start + i;
                let body = match row {
                    Row::Side { left, right } => h_flex()
                        .size_full()
                        .child(cell_element(left.as_ref(), style.old_gutter, &style))
                        .child(
                            cell_element(right.as_ref(), style.new_gutter, &style)
                                .border_l_1()
                                .border_color(style.palette.divider),
                        )
                        .into_any_element(),
                    Row::Unified { old, new, cell } => h_flex()
                        .size_full()
                        .child(number_element(*old, style.old_gutter, cell.kind, &style))
                        .child(number_element(*new, style.new_gutter, cell.kind, &style))
                        .child(text_pane(cell, &style))
                        .into_any_element(),
                    Row::Skip(skip) => skip_element(skip, &style),
                    Row::Notice(n) => h_flex()
                        .size_full()
                        .items_center()
                        .justify_center()
                        .text_color(style.palette.muted_fg)
                        .child(n.text())
                        .into_any_element(),
                };
                let selected = ix == self.cursor && row.selectable();
                div()
                    .id(("diff-row", ix))
                    .relative()
                    .w_full()
                    .h(style.row_height)
                    .child(body)
                    .when(selected, |d| {
                        d.child(
                            div()
                                .absolute()
                                .inset_0()
                                .bg(style.palette.cursor_bg)
                                .border_l_2()
                                .border_color(style.palette.cursor_bar),
                        )
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.click_row(ix, window, cx);
                    }))
                    .into_any_element()
            })
            .collect()
    }
}

fn number_element(n: Option<u32>, width: Pixels, kind: LineKind, style: &Style) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_end()
        .h_full()
        .w(width)
        .pr(px(8.))
        .text_color(style.palette.gutter_fg)
        .bg(style.palette.gutter_bg(kind))
        .when(style.hide_numbers, |d| d.hidden())
        .children(n.map(|n| n.to_string()))
}

fn text_pane(cell: &Cell, style: &Style) -> Div {
    let p = &style.palette;
    div()
        .relative()
        .flex_1()
        .min_w_0()
        .h_full()
        .overflow_hidden()
        .whitespace_nowrap()
        .pl(px(6.))
        .bg(p.line_bg(cell.kind, cell.due_to_rebase))
        .when(cell.due_to_rebase, |d| d.text_color(p.muted_fg))
        .children(style.ruler.map(|x| {
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(x + px(6.))
                .w(px(1.))
                .bg(p.ruler)
        }))
        .when(!cell.text.is_empty(), |d| {
            d.child(
                StyledText::new(SharedString::from(cell.text.clone()))
                    .with_highlights(line_highlights(cell, p)),
            )
        })
}

/// One half of a side-by-side row; a blank filler without a cell.
fn cell_element(cell: Option<&Cell>, gutter: Pixels, style: &Style) -> Div {
    let p = &style.palette;
    match cell {
        Some(cell) => h_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .child(number_element(Some(cell.number), gutter, cell.kind, style))
            .child(text_pane(cell, style)),
        None => h_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(p.filler_bg)
            .child(number_element(None, gutter, LineKind::Common, style)),
    }
}

fn skip_element(skip: &SkipRow, style: &Style) -> AnyElement {
    let text = if skip.lines == 1 {
        "1 common line hidden".to_owned()
    } else {
        format!("{} common lines hidden", skip.lines)
    };
    h_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .bg(style.palette.skip_bg)
        .text_color(style.palette.muted_fg)
        .cursor_pointer()
        .child(text)
        .child(
            div()
                .text_color(style.palette.gutter_fg)
                .child("(Enter to show)"),
        )
        .into_any_element()
}

impl Render for DiffView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (family, size, row_height, bg, fg, strip) = {
            let t = cx.theme();
            let strip = match (&self.status, &self.hint) {
                (Some(DiffStatus::Busy(m)), _) => Some((m.clone(), t.muted_foreground, t.muted)),
                (Some(DiffStatus::Info(m)), _) => Some((m.clone(), t.muted_foreground, t.muted)),
                (Some(DiffStatus::Error(m)), _) => {
                    Some((m.clone(), t.danger, t.danger.opacity(0.1)))
                }
                (None, Some(m)) => Some((m.clone(), t.muted_foreground, t.muted)),
                (None, None) => None,
            };
            let family = self
                .font_family
                .clone()
                .unwrap_or_else(|| t.mono_font_family.clone());
            (
                family,
                t.mono_font_size,
                super::paint::row_height(t.mono_font_size),
                t.background,
                t.foreground,
                strip,
            )
        };
        self.row_height = row_height;
        let count = self.layout.len();
        let list = uniform_list(
            "diff-rows",
            count,
            cx.processor(|this, range: Range<usize>, window, cx| {
                this.render_rows(range, window, cx)
            }),
        )
        .track_scroll(&self.scroll);
        let list = match self.sizing {
            DiffSizing::Fill => list.w_full().flex_1().min_h_0(),
            DiffSizing::Inline { max_rows } => list
                .with_sizing_behavior(ListSizingBehavior::Infer)
                .w_full()
                .when_some(max_rows, |l, n| l.max_h(row_height * n as f32)),
        };
        v_flex()
            .id("diff-view")
            .key_context(DIFF_VIEW)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::line_down))
            .on_action(cx.listener(Self::line_up))
            .on_action(cx.listener(Self::next_chunk))
            .on_action(cx.listener(Self::prev_chunk))
            .on_action(cx.listener(Self::page_down))
            .on_action(cx.listener(Self::page_up))
            .on_action(cx.listener(Self::half_page_down))
            .on_action(cx.listener(Self::half_page_up))
            .on_action(cx.listener(Self::cursor_top))
            .on_action(cx.listener(Self::cursor_bottom))
            .on_action(cx.listener(Self::toggle_mode_action))
            .on_action(cx.listener(Self::expand_skip))
            .on_action(cx.listener(Self::show_full_file))
            .when(self.sizing == DiffSizing::Fill, |d| d.size_full())
            .when(self.sizing != DiffSizing::Fill, |d| d.w_full())
            .bg(bg)
            .text_color(fg)
            .font_family(family)
            .text_size(size)
            .line_height(row_height)
            .children(strip.map(|(text, fg, bg)| {
                div()
                    .id("diff-status")
                    .flex_none()
                    .px_2()
                    .text_xs()
                    .bg(bg)
                    .text_color(fg)
                    .child(text)
            }))
            .child(list)
    }
}
