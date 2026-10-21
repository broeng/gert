//! The diff widget: a virtualized, keyboard-driven view of a [`Diff`].
//!
//! All the work that depends on the diff happens in [`Layout::build`], once
//! per diff or mode change. Rendering only turns the visible slice of rows
//! into elements.

use std::{cell, rc::Rc, sync::Arc};

use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use model::{Diff, DiffViewMode, ResolvedDiffPreferences};

use super::{
    actions::*,
    layout::{
        Anchor, Cell, Expansion, FoldRow, HiddenRow, Layout, LayoutOptions, LineKind, REVEAL_STEP,
        Row,
    },
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

/// A one-line message the owner can show above the rows (loading a diff
/// again, a failed refetch).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffStatus {
    Busy(SharedString),
    Info(SharedString),
    Error(SharedString),
}

pub enum DiffEvent {
    /// The rows were rebuilt (folds opened, context or mode changed, a new
    /// diff): an inline owner has to measure the view again.
    RowsChanged,
    /// The view mode was toggled; the owner may want to remember it.
    ModeChanged(DiffViewMode),
}

pub struct DiffView {
    diff: Arc<Diff>,
    prefs: ResolvedDiffPreferences,
    font_family: Option<SharedString>,
    sizing: DiffSizing,
    layout: Arc<Layout>,
    /// Which folds the user opened. Kept across diffs, modes and contexts.
    expansion: Expansion,
    cursor: usize,
    focus: FocusHandle,
    /// Variable-height rows: code lines have one height, inline threads
    /// (and their editors) their own, so the list measures what it renders.
    list: ListState,
    /// Row height of a code line at the last render; also what the page
    /// size derives from and the height hint of rows not measured yet.
    row_height: Pixels,
    /// Window width at the last render. A change makes the list forget its
    /// height hints, which are put back afterwards.
    window_width: Pixels,
    /// Height of the list's viewport at the last render. (The list's own
    /// state is borrowed while it renders rows, so rows cannot ask it.)
    viewport: Pixels,
    /// The cursor row asks the list to scroll it into view in the next
    /// frame (cleared when that row has been painted).
    reveal: Rc<cell::Cell<Option<Reveal>>>,
    /// Styling of the current frame, shared by the row renderers.
    style: Option<Rc<Style>>,
    /// Set by the owner.
    status: Option<DiffStatus>,
    /// The view's own message ("nothing is hidden"); goes away when the
    /// cursor moves or the diff changes. Shown over an `Info` status, but
    /// not over `Busy` or `Error`.
    hint: Option<SharedString>,
}

impl EventEmitter<DiffEvent> for DiffView {}

impl Focusable for DiffView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

const DEFAULT_PAGE_ROWS: usize = 20;

/// How far above and below the viewport the list measures rows.
const OVERDRAW: Pixels = px(300.);

/// How the cursor row is brought into view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reveal {
    /// Scroll as little as possible.
    Nearest,
    /// Put the row in the middle of the viewport.
    Center,
}

impl DiffView {
    pub fn new(diff: Diff, prefs: ResolvedDiffPreferences, cx: &mut Context<Self>) -> Self {
        let expansion = Expansion::default();
        let layout = Arc::new(Layout::build(
            &diff,
            &LayoutOptions::from_prefs(&prefs),
            &expansion,
        ));
        let cursor = layout.first_selectable().unwrap_or(0);
        let row_height = super::paint::row_height(cx.theme().mono_font_size);
        let list = ListState::new(layout.len(), ListAlignment::Top, OVERDRAW);
        list.reset_with_uniform_height(layout.len(), row_height);
        Self {
            diff: Arc::new(diff),
            prefs,
            font_family: None,
            sizing: DiffSizing::Fill,
            layout,
            expansion,
            cursor,
            focus: cx.focus_handle(),
            list,
            row_height,
            window_width: px(0.),
            viewport: px(0.),
            reveal: Rc::new(cell::Cell::new(None)),
            style: None,
            status: None,
            hint: None,
        }
    }

    /// Whether some unchanged lines are folded.
    pub fn has_folds(&self) -> bool {
        self.layout.rows().iter().any(|r| matches!(r, Row::Fold(_)))
    }

    /// What the user opened, to restore it in a later view of the same diff.
    pub fn expansion(&self) -> &Expansion {
        &self.expansion
    }

    pub fn set_expansion(&mut self, expansion: Expansion, cx: &mut Context<Self>) {
        if self.expansion != expansion {
            let keep = self.keep_position();
            self.expansion = expansion;
            self.rebuild(keep, cx);
        }
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

    /// Replaces the diff (e.g. after a whitespace change), keeping the folds
    /// the user opened and the cursor on the same file line and at the same
    /// place on screen where possible.
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
        self.move_to(row, Reveal::Nearest, cx);
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

    fn rebuild(&mut self, keep: (Anchor, usize), cx: &mut Context<Self>) {
        self.rebuild_with(keep, None);
        cx.emit(DiffEvent::RowsChanged);
        cx.notify();
    }

    fn rebuild_with(&mut self, (anchor, rel): (Anchor, usize), follow: Option<u32>) {
        self.layout = Arc::new(Layout::build(
            &self.diff,
            &LayoutOptions::from_prefs(&self.prefs),
            &self.expansion,
        ));
        let fold = follow.and_then(|run| {
            self.layout
                .rows()
                .iter()
                .position(|r| matches!(r, Row::Fold(f) if f.run == run))
        });
        let row = fold
            .or_else(|| self.layout.find(anchor))
            .unwrap_or_else(|| self.cursor.min(self.layout.len().saturating_sub(1)));
        self.cursor = self.snap(row);
        self.reset_list();
        let rel = rel.min(self.page_rows().saturating_sub(1));
        self.list.scroll_to(ListOffset {
            item_ix: self.cursor.saturating_sub(rel),
            offset_in_item: px(0.),
        });
        self.reveal.set(Some(Reveal::Nearest));
    }

    /// Forgets the measured row heights (the rows changed) and hints that
    /// every row is as tall as a code line until it has been rendered.
    fn reset_list(&self) {
        self.list
            .reset_with_uniform_height(self.layout.len(), self.row_height);
    }

    /// First row (at least partly) in the viewport.
    fn top_row(&self) -> usize {
        self.list.logical_scroll_top().item_ix
    }

    fn page_rows(&self) -> usize {
        let h = self.list.viewport_bounds().size.height;
        if h > px(0.) && self.row_height > px(0.) {
            ((h / self.row_height).floor() as usize).max(1)
        } else {
            DEFAULT_PAGE_ROWS
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

    fn move_to(&mut self, row: usize, how: Reveal, cx: &mut Context<Self>) {
        if self.layout.is_empty() {
            return;
        }
        self.cursor = self.snap(row);
        self.hint = None;
        self.scroll_to_cursor(how);
        cx.notify();
    }

    /// Brings the cursor row into view. A row near the viewport is placed
    /// by the heights measured so far; a far one by its index, since rows
    /// that were never rendered only have a guessed height. Either way the
    /// row asks for the exact position when it is painted (see
    /// [`autoscroll_probe`]), so neither guess needs to be right.
    fn scroll_to_cursor(&mut self, how: Reveal) {
        let (top, page) = (self.top_row(), self.page_rows());
        let near = self.cursor + 3 * page >= top && self.cursor <= top + 3 * page;
        match how {
            Reveal::Nearest if near => self.list.scroll_to_reveal_item(self.cursor),
            _ => self.list.scroll_to(ListOffset {
                item_ix: self.cursor.saturating_sub(page / 4),
                offset_in_item: px(0.),
            }),
        }
        self.reveal.set(Some(how));
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
        self.move_to(i, Reveal::Nearest, cx);
    }

    /// Changes what is folded and rebuilds. The cursor stays on its file
    /// line; with `follow` (the cursor was on a fold the user is opening
    /// step by step) it stays on that fold's remainder instead, so the
    /// same key can be pressed again.
    fn reveal(
        &mut self,
        follow: Option<u32>,
        change: impl FnOnce(&mut Expansion),
        cx: &mut Context<Self>,
    ) {
        change(&mut self.expansion);
        self.hint = None;
        let keep = self.keep_position();
        self.rebuild_with(keep, follow);
        cx.emit(DiffEvent::RowsChanged);
        cx.notify();
    }

    fn fold_at(&self, row: usize) -> Option<FoldRow> {
        match self.layout.rows().get(row) {
            Some(Row::Fold(f)) => Some(f.clone()),
            _ => None,
        }
    }

    /// Opens the whole fold under the cursor / at `row`.
    fn expand_fold(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(fold) = self.fold_at(row) else {
            return;
        };
        self.reveal(None, |e| e.reveal_run(fold.run), cx);
    }

    /// Reveals `REVEAL_STEP` more lines at the top or bottom of the fold
    /// at `row`.
    fn reveal_step(&mut self, row: usize, top: bool, cx: &mut Context<Self>) {
        let Some(fold) = self.fold_at(row) else {
            self.hint = Some("Not on a folded range".into());
            cx.notify();
            return;
        };
        let (allowed, label) = if top {
            (fold.above, "above")
        } else {
            (fold.below, "below")
        };
        if !allowed {
            self.hint = Some(format!("Nothing to show {label} this range").into());
            cx.notify();
            return;
        }
        self.reveal(
            Some(fold.run),
            |e| {
                if top {
                    e.reveal_top(fold.run, REVEAL_STEP);
                } else {
                    e.reveal_bottom(fold.run, REVEAL_STEP);
                }
            },
            cx,
        );
    }

    /// The list forgets the height hints of its rows when its width
    /// changes (every row may wrap differently), and the scroll bar then
    /// only knows the rows rendered so far. After a change of the window
    /// width or of the code row height the hints are put back once the
    /// frame is done, keeping the scroll position.
    fn sync_list_hints(&mut self, row_height: Pixels, window: &Window, cx: &mut Context<Self>) {
        let width = window.viewport_size().width;
        if width == self.window_width && row_height == self.row_height {
            return;
        }
        let first = self.window_width == px(0.);
        self.window_width = width;
        self.row_height = row_height;
        if first {
            return; // the hints from `new` are in place
        }
        cx.defer_in(window, |this, _, cx| {
            let top = this.list.logical_scroll_top();
            this.reset_list();
            this.list.scroll_to(top);
            cx.notify();
        });
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
            self.move_to(row, Reveal::Center, cx);
        }
    }

    fn prev_chunk(&mut self, _: &PrevChunk, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.layout.prev_chunk(self.cursor) {
            self.move_to(row, Reveal::Center, cx);
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
        self.move_to(0, Reveal::Nearest, cx);
    }

    fn cursor_bottom(&mut self, _: &CursorBottom, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(usize::MAX, Reveal::Nearest, cx);
    }

    fn toggle_mode_action(&mut self, _: &ToggleMode, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_mode(cx);
    }

    fn expand_fold_action(&mut self, _: &ExpandFold, _: &mut Window, cx: &mut Context<Self>) {
        self.expand_fold(self.cursor, cx);
    }

    fn expand_above(&mut self, _: &ExpandAbove, _: &mut Window, cx: &mut Context<Self>) {
        self.reveal_step(self.cursor, true, cx);
    }

    fn expand_below(&mut self, _: &ExpandBelow, _: &mut Window, cx: &mut Context<Self>) {
        self.reveal_step(self.cursor, false, cx);
    }

    /// `shift-x`: opens every fold; pressed again, folds them again.
    fn toggle_all_folds(&mut self, _: &ShowFullFile, _: &mut Window, cx: &mut Context<Self>) {
        self.set_all_expanded(!self.all_expanded(), cx);
    }

    /// Whether `shift-x` has opened everything (and would fold again).
    pub fn all_expanded(&self) -> bool {
        self.expansion.is_everything()
    }

    /// Opens every fold of the file, or folds them all again.
    pub fn set_all_expanded(&mut self, expand: bool, cx: &mut Context<Self>) {
        if expand && !self.has_folds() {
            self.hint = Some("The whole file is already shown".into());
            cx.notify();
            return;
        }
        self.reveal(
            None,
            |e| {
                if expand {
                    e.reveal_everything();
                } else {
                    e.clear();
                }
            },
            cx,
        );
    }

    fn click_row(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        self.move_to(row, Reveal::Nearest, cx);
    }

    // --- rendering -------------------------------------------------------

    /// The element of one row. Called by the list for the rows it lays out,
    /// so it must not depend on anything that is expensive per row.
    fn render_row(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let layout = self.layout.clone();
        let (Some(row), Some(style)) = (layout.rows().get(ix), self.style.clone()) else {
            return div().into_any_element();
        };
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
            Row::Fold(fold) => fold_element(fold, ix, &style, cx),
            Row::Hidden(hidden) => hidden_element(hidden, &style),
            Row::Notice(n) => h_flex()
                .size_full()
                .items_center()
                .justify_center()
                .text_color(style.palette.muted_fg)
                .child(n.text())
                .into_any_element(),
        };
        let selected = ix == self.cursor && row.selectable();
        let reveal = self.reveal.get().filter(|_| selected);
        let viewport = self.viewport;
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
            .when_some(reveal, |d, how| {
                d.child(autoscroll_probe(self.reveal.clone(), how, viewport))
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.click_row(ix, window, cx);
            }))
            .into_any_element()
    }
}

/// An invisible child of the cursor row. While a scroll to the cursor is
/// pending it asks the list (via the window's autoscroll request) to make
/// this row's real bounds visible, which stays exact when rows above it
/// turn out taller than guessed.
fn autoscroll_probe(
    pending: Rc<cell::Cell<Option<Reveal>>>,
    how: Reveal,
    viewport: Pixels,
) -> impl IntoElement {
    canvas(
        move |bounds, window, _| {
            let target = match how {
                Reveal::Nearest => bounds,
                // A request as tall as the viewport, centered on the row,
                // ends with the row in the middle.
                Reveal::Center => Bounds {
                    origin: point(bounds.left(), bounds.center().y - viewport / 2.),
                    size: size(bounds.size.width, viewport.max(bounds.size.height)),
                },
            };
            window.request_autoscroll(target);
        },
        move |_, (), _, _| pending.set(None),
    )
    .absolute()
    .inset_0()
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

fn fold_element(
    fold: &FoldRow,
    ix: usize,
    style: &Style,
    cx: &mut Context<DiffView>,
) -> AnyElement {
    let text = if fold.lines == 1 {
        "1 unchanged line".to_owned()
    } else {
        format!("{} unchanged lines", fold.lines)
    };
    let muted = style.palette.gutter_fg;
    let button = |id: &'static str, label: String, enabled: bool, top: Option<bool>| {
        // `top`: reveal at the top / bottom edge; `None`: the whole fold.
        div()
            .id((id, ix))
            .px_1()
            .rounded_sm()
            .text_color(muted)
            .when(!enabled, |d| d.hidden())
            .cursor_pointer()
            .hover(|d| d.text_color(style.palette.muted_fg).underline())
            .child(label)
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                window.focus(&this.focus, cx);
                this.move_to(ix, Reveal::Nearest, cx);
                match top {
                    Some(top) => this.reveal_step(ix, top, cx),
                    None => this.expand_fold(ix, cx),
                }
            }))
    };
    let step = fold.lines.min(REVEAL_STEP);
    h_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .bg(style.palette.skip_bg)
        .text_color(style.palette.muted_fg)
        .child(text)
        .child(button(
            "fold-above",
            format!("+{step} above"),
            fold.above && fold.lines > REVEAL_STEP,
            Some(true),
        ))
        .child(button(
            "fold-below",
            format!("+{step} below"),
            fold.below && fold.lines > REVEAL_STEP,
            Some(false),
        ))
        .child(button("fold-all", "show all".to_owned(), true, None))
        .into_any_element()
}

fn hidden_element(hidden: &HiddenRow, style: &Style) -> AnyElement {
    let text = if hidden.lines == 1 {
        "1 line not sent by the server".to_owned()
    } else {
        format!("{} lines not sent by the server", hidden.lines)
    };
    h_flex()
        .size_full()
        .items_center()
        .justify_center()
        .bg(style.palette.skip_bg)
        .text_color(style.palette.muted_fg)
        .child(text)
        .into_any_element()
}

impl Render for DiffView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (family, size, row_height, bg, fg, strip) = {
            let t = cx.theme();
            let strip = match (&self.status, &self.hint) {
                (Some(DiffStatus::Busy(m)), _) => Some((m.clone(), t.muted_foreground, t.muted)),
                // A transient hint (the key was pressed with nothing to do)
                // beats the owner's standing info.
                (Some(DiffStatus::Info(_)), Some(m)) => {
                    Some((m.clone(), t.muted_foreground, t.muted))
                }
                (Some(DiffStatus::Info(m)), None) => Some((m.clone(), t.muted_foreground, t.muted)),
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
        self.sync_list_hints(row_height, window, cx);
        self.viewport = self.list.viewport_bounds().size.height;
        self.style = Some(Rc::new(Style::new(
            self.font_family.clone(),
            self.prefs.line_length,
            self.prefs.hide_line_numbers,
            self.focus.is_focused(window),
            self.layout.max_numbers(),
            window,
            cx,
        )));
        let list = list(
            self.list.clone(),
            cx.processor(|this, ix, _window, cx| this.render_row(ix, cx)),
        );
        let list = match self.sizing {
            DiffSizing::Fill => list.size_full(),
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
            .on_action(cx.listener(Self::expand_fold_action))
            .on_action(cx.listener(Self::expand_above))
            .on_action(cx.listener(Self::expand_below))
            .on_action(cx.listener(Self::toggle_all_folds))
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
            .child(
                div()
                    .relative()
                    .w_full()
                    .when(self.sizing == DiffSizing::Fill, |d| d.flex_1().min_h_0())
                    .child(list)
                    .vertical_scrollbar(&self.list),
            )
    }
}
