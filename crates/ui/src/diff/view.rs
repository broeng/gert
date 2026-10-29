//! The diff widget: a virtualized, keyboard-driven view of a [`Diff`].
//!
//! All the work that depends on the diff happens in [`Layout::build`], once
//! per diff or mode change. Rendering only turns the visible slice of rows
//! into elements.

mod editing;
mod thread_card;

use std::{cell, collections::HashMap, rc::Rc, sync::Arc};

use gpui_kit::component::scroll::{ScrollableElement as _, ScrollbarHandle};
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use model::{
    Diff, DiffPair, DiffSide, DiffViewMode, PATCHSET_LEVEL_PATH, PatchSetNumber,
    ResolvedDiffPreferences, Thread, ThreadId,
};

use super::{
    actions::*,
    layout::{
        Anchor, Cell, Expansion, FoldRow, HiddenRow, Layout, LayoutOptions, LineKind, REVEAL_STEP,
        Row, ThreadSlot,
    },
    paint::{Palette, Style, SyntaxPaint, TEXT_INSET, line_highlights},
    syntax::{DiffSyntax, highlight_diff},
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

pub use editing::DraftRequest;
use editing::{DraftEditor, EditKey, UnsavedBuffers};

pub enum DiffEvent {
    /// The rows were rebuilt (folds opened, context or mode changed, a new
    /// diff): an inline owner has to measure the view again.
    RowsChanged,
    /// The view mode was toggled; the owner may want to remember it.
    ModeChanged(DiffViewMode),
    /// The user wrote, changed, deleted or retried a draft. The owner does
    /// the work (the view does no I/O); the drafts come back as threads.
    Draft(DraftRequest),
    /// The view's own background job finished the syntax colors of the
    /// current diff (`None`: nothing to color). The owner may cache them.
    Highlighted(Option<DiffSyntax>),
}

/// Which file of which patch set pair the view shows: what decides where
/// (and whether) a comment thread appears in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadContext {
    pub pair: DiffPair,
    pub path: String,
    /// The file's name in the base, for a renamed file.
    pub old_path: Option<String>,
}

/// Syntax highlighting of the current diff.
enum Syntax {
    /// Not computed (off, no path yet, or a carrier of threads).
    Idle,
    /// Computing in the background; dropping the task cancels it.
    Pending(#[expect(dead_code, reason = "dropping the task cancels the job")] Task<()>),
    /// Done; `None` when the file has nothing to color.
    Ready(Option<DiffSyntax>),
}

pub struct DiffView {
    diff: Arc<Diff>,
    /// The file the diff is of, for its language. Set by the owner.
    path: Option<String>,
    syntax: Syntax,
    /// Bumped for every new highlight job, so a result that arrives for an
    /// older diff or path is ignored.
    syntax_gen: u64,
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
    /// How far the code text is scrolled sideways (lines not wrapped).
    side_scroll: SideScroll,
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
    /// All the change's threads, as last given by the owner.
    threads: Arc<Vec<Thread>>,
    /// Indexes into `threads` of those shown in this diff; the position is
    /// the slot of the layout's thread row.
    shown: Vec<usize>,
    slots: Vec<ThreadSlot>,
    thread_context: Option<ThreadContext>,
    /// Threads the user opened or closed by hand, whatever their state
    /// says. Kept across [`Self::set_threads`].
    thread_open: HashMap<ThreadId, bool>,
    /// Whether the user may write comments (signed in).
    can_comment: bool,
    /// The open draft editor, if any.
    editor: Option<DraftEditor>,
    /// Text of closed editors that was not saved; see `editing`.
    unsaved: UnsavedBuffers,
    /// The thread of a new comment while its editor is open (it has no
    /// entries and occupies the last slot).
    new_thread: Option<Thread>,
    /// The draft whose "delete?" question is on screen.
    confirm_delete: Option<model::LocalDraftId>,
    /// The view carries the patch-set level threads of a change and no
    /// file: see [`DiffView::threads_only`].
    threads_only: bool,
}

impl EventEmitter<DiffEvent> for DiffView {}

impl Focusable for DiffView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// What a rebuild keeps: see [`DiffView::keep_position`].
type Keep = (Anchor, usize, Option<ThreadId>);

const DEFAULT_PAGE_ROWS: usize = 20;

/// Unresolved threads and those with a draft of the user's start open.
fn default_open(t: &Thread) -> bool {
    t.is_unresolved() || t.has_draft()
}

/// Columns `h` / `l` scroll the text sideways.
const SIDE_STEP: f32 = 8.;

/// The sideways scroll of the code text when lines are not wrapped. One
/// offset for every text pane, so both sides of a row move together, and
/// the gutters, folds and comment threads stay where they are. Shared with
/// the horizontal scrollbar, which drags it.
#[derive(Clone, Default)]
struct SideScroll(Rc<cell::Cell<SideScrollState>>);

#[derive(Clone, Copy, Default)]
struct SideScrollState {
    /// How far the text is scrolled, `0..=max`.
    x: Pixels,
    max: Pixels,
    /// The list's bounds, which the scrollbar runs along.
    viewport: Bounds<Pixels>,
}

impl SideScroll {
    fn x(&self) -> Pixels {
        self.0.get().x
    }

    /// Scrolls to `x`, within the limits; whether the text moved.
    fn scroll_to(&self, x: Pixels) -> bool {
        let mut s = self.0.get();
        let x = x.clamp(px(0.), s.max);
        let moved = x != s.x;
        s.x = x;
        self.0.set(s);
        moved
    }

    fn scroll_by(&self, dx: Pixels) -> bool {
        self.scroll_to(self.x() + dx)
    }

    fn set_limits(&self, viewport: Bounds<Pixels>, max: Pixels) {
        let mut s = self.0.get();
        s.viewport = viewport;
        s.max = max;
        s.x = s.x.min(max);
        self.0.set(s);
    }
}

impl ScrollbarHandle for SideScroll {
    fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.0.get().viewport
    }

    fn offset(&self) -> Point<Pixels> {
        point(-self.x(), px(0.))
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        self.scroll_to(-offset.x);
    }

    fn content_size(&self) -> Size<Pixels> {
        let s = self.0.get();
        size(s.viewport.size.width + s.max, s.viewport.size.height)
    }
}

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
            path: None,
            syntax: Syntax::Idle,
            syntax_gen: 0,
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
            side_scroll: SideScroll::default(),
            reveal: Rc::new(cell::Cell::new(None)),
            style: None,
            status: None,
            hint: None,
            threads: Arc::default(),
            shown: Vec::new(),
            slots: Vec::new(),
            thread_context: None,
            thread_open: HashMap::new(),
            can_comment: false,
            editor: None,
            unsaved: HashMap::new(),
            new_thread: None,
            confirm_delete: None,
            threads_only: false,
        }
    }

    /// A view without a file that shows the change's patch-set level
    /// threads (and only those) as cards, full width, whatever patch set
    /// they are on; the change page uses it for its "Patchset comments".
    /// Give it threads with [`Self::set_patchset_threads`]. `c` (or
    /// [`Self::new_patchset_comment`]) starts a new thread.
    pub fn threads_only(prefs: ResolvedDiffPreferences, cx: &mut Context<Self>) -> Self {
        // A diff without chunks; the layout leaves out the "no changes"
        // notice for it, so that only the threads remain.
        let carrier = Diff {
            meta_a: None,
            meta_b: None,
            change_type: None,
            intraline_status: None,
            diff_header: Vec::new(),
            chunks: Vec::new(),
            binary: false,
        };
        let mut this = Self::new(carrier, prefs, cx);
        this.threads_only = true;
        this.rebuild_with(this.keep_position(), None);
        this
    }

    /// Gives a [`Self::threads_only`] view the change's threads. New
    /// comments go to `current`, the change's current patch set.
    pub fn set_patchset_threads(
        &mut self,
        current: PatchSetNumber,
        threads: Arc<Vec<Thread>>,
        cx: &mut Context<Self>,
    ) {
        let context = ThreadContext {
            pair: DiffPair {
                base: None,
                target: current,
            },
            path: PATCHSET_LEVEL_PATH.to_owned(),
            old_path: None,
        };
        self.set_threads(context, threads, cx);
    }

    /// Opens the thread (its messages), puts the cursor on it and brings
    /// it into view. `false` if the view does not show that thread.
    pub fn focus_thread(&mut self, id: &ThreadId, cx: &mut Context<Self>) -> bool {
        let Some(row) = self.thread_row_of(id) else {
            return false;
        };
        self.thread_open.insert(id.clone(), true);
        self.list.remeasure_items(row..row + 1);
        self.move_to(row, Reveal::Center, cx);
        cx.emit(DiffEvent::RowsChanged);
        true
    }

    /// The thread the cursor is on, if it is on a thread row.
    pub fn cursor_thread(&self) -> Option<ThreadId> {
        self.thread_id_at(self.cursor)
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

    /// Gives the view the change's comment threads. Those that show in this
    /// file for the patch set pair in `context` appear between the lines.
    /// If none moved (the same places, maybe other contents), only the
    /// affected rows are measured again; otherwise the rows are rebuilt,
    /// keeping the cursor and the opened folds.
    pub fn set_threads(
        &mut self,
        context: ThreadContext,
        threads: Arc<Vec<Thread>>,
        cx: &mut Context<Self>,
    ) {
        let keep = self.keep_position();
        let old_threads = std::mem::replace(&mut self.threads, threads);
        let old_shown = self.shown.clone();
        self.thread_context = Some(context);
        self.reslot(keep, old_threads, old_shown, cx);
    }

    /// The thread indexes and layout slots for the current threads, context
    /// and the editor's new thread, if any.
    fn compute_slots(&self) -> (Vec<usize>, Vec<ThreadSlot>) {
        let (mut shown, mut slots) = (Vec::new(), Vec::new());
        if let Some(context) = &self.thread_context {
            for (i, t) in self.threads.iter().enumerate() {
                let side = if self.threads_only {
                    (t.path == PATCHSET_LEVEL_PATH).then_some(DiffSide::Right)
                } else {
                    t.side_in(context.pair, &context.path, context.old_path.as_deref())
                };
                if let Some(side) = side {
                    shown.push(i);
                    slots.push(ThreadSlot {
                        side,
                        location: t.location,
                    });
                }
            }
        }
        if let (Some(t), Some(EditKey::New(side, _))) =
            (&self.new_thread, self.editor.as_ref().map(|e| &e.key))
        {
            slots.push(ThreadSlot {
                side: *side,
                location: t.location,
            });
        }
        (shown, slots)
    }

    /// Takes the threads, the context and the new-comment thread as they
    /// are now. If no thread moved (the same places, maybe other
    /// contents), only the affected rows are measured again; otherwise the
    /// rows are rebuilt, keeping the cursor and the opened folds.
    fn reslot(
        &mut self,
        keep: Keep,
        old_threads: Arc<Vec<Thread>>,
        old_shown: Vec<usize>,
        cx: &mut Context<Self>,
    ) {
        let (shown, slots) = self.compute_slots();
        // In a file the threads keep their places; a threads-only view orders
        // them by age, so a reply can move a thread, and the rows (and the
        // cursor, which follows its thread) have to be rebuilt.
        let reordered = self.threads_only && {
            let ids = |threads: &[Thread], shown: &[usize]| -> Vec<ThreadId> {
                shown
                    .iter()
                    .filter_map(|&i| threads.get(i).map(|t| t.id.clone()))
                    .collect()
            };
            ids(&old_threads, &old_shown) != ids(&self.threads, &shown)
        };
        let unchanged = slots == self.slots && !reordered;
        self.shown = shown;
        self.slots = slots;
        // An editor whose thread is gone (the draft was deleted
        // elsewhere) has nothing to show in.
        if self.editor.is_some() && self.editor_slot().is_none() {
            self.editor = None;
        }
        if unchanged {
            let mut changed = false;
            for &ix in self.layout.thread_rows() {
                let Some(Row::Thread(tr)) = self.layout.rows().get(ix) else {
                    continue;
                };
                let was = old_shown.get(tr.slot).and_then(|&i| old_threads.get(i));
                if was != self.thread_at(tr.slot) {
                    self.list.remeasure_items(ix..ix + 1);
                    changed = true;
                }
            }
            if changed {
                cx.emit(DiffEvent::RowsChanged);
                cx.notify();
            }
            return;
        }
        let at_top = {
            let top = self.list.logical_scroll_top();
            top.item_ix == 0 && top.offset_in_item == px(0.)
        };
        self.rebuild_with(keep, None);
        if at_top {
            // Threads at the top of the file (file comments) must not end
            // up above the viewport just because the cursor is below them.
            self.list.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: px(0.),
            });
        }
        cx.emit(DiffEvent::RowsChanged);
        cx.notify();
    }

    /// Places the threads again after the editor's new thread came or went.
    fn reslot_now(&mut self, cx: &mut Context<Self>) {
        let keep = self.keep_position();
        let (threads, shown) = (self.threads.clone(), self.shown.clone());
        self.reslot(keep, threads, shown, cx);
    }

    /// The threads shown in this diff, in the layout's slot order.
    pub fn shown_threads(&self) -> impl Iterator<Item = &Thread> {
        self.shown.iter().filter_map(|&i| self.threads.get(i))
    }

    /// Whether the thread is shown open (its messages) rather than as a
    /// one-line summary: by default when it is unresolved or has a draft.
    pub fn is_thread_open(&self, id: &ThreadId) -> bool {
        match self.thread_open.get(id) {
            Some(open) => *open,
            None => self
                .threads
                .iter()
                .find(|t| &t.id == id)
                .is_some_and(default_open),
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
        self.start_syntax(cx);
    }

    /// Like [`Self::set_diff_for`] with colors the owner already has (`None`:
    /// not known, compute them): a known result is used at once, with no job
    /// and no plain frame first.
    pub fn set_diff_for_cached(
        &mut self,
        path: &str,
        diff: Diff,
        cached: Option<Option<DiffSyntax>>,
        cx: &mut Context<Self>,
    ) {
        self.path = Some(path.to_owned());
        let keep = self.keep_position();
        self.diff = Arc::new(diff);
        self.hint = None;
        self.rebuild(keep, cx);
        self.install_or_start_syntax(cached, cx);
    }

    /// Like [`Self::set_path`] with colors the owner already has.
    pub fn set_path_cached(
        &mut self,
        path: &str,
        cached: Option<Option<DiffSyntax>>,
        cx: &mut Context<Self>,
    ) {
        self.path = Some(path.to_owned());
        self.install_or_start_syntax(cached, cx);
    }

    fn install_or_start_syntax(
        &mut self,
        cached: Option<Option<DiffSyntax>>,
        cx: &mut Context<Self>,
    ) {
        match cached {
            Some(result) if !self.threads_only => {
                // Also cancels a job for the previous diff.
                self.syntax_gen += 1;
                self.syntax = Syntax::Ready(result);
                cx.notify();
            }
            _ => self.start_syntax(cx),
        }
    }

    /// Like [`Self::set_diff`] for another file (or the first diff of a
    /// view): `path` decides the language of the syntax colors.
    pub fn set_diff_for(&mut self, path: &str, diff: Diff, cx: &mut Context<Self>) {
        self.path = Some(path.to_owned());
        self.set_diff(diff, cx);
    }

    /// Tells the view which file its diff is of, if it did not get the diff
    /// with [`Self::set_diff_for`]. Colors the diff when the path is new.
    pub fn set_path(&mut self, path: &str, cx: &mut Context<Self>) {
        if self.path.as_deref() != Some(path) {
            self.path = Some(path.to_owned());
            self.start_syntax(cx);
        }
    }

    /// The syntax colors to paint: computed, and the preference is on.
    fn active_syntax(&self) -> Option<&DiffSyntax> {
        match &self.syntax {
            Syntax::Ready(Some(s)) if self.prefs.syntax_highlighting => Some(s),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn syntax_for_test(&self) -> Option<&DiffSyntax> {
        self.active_syntax()
    }

    /// Starts highlighting the current diff in the background, replacing
    /// (and so cancelling) any earlier job. The diff stays plain until the
    /// result arrives.
    fn start_syntax(&mut self, cx: &mut Context<Self>) {
        self.syntax_gen += 1;
        self.syntax = Syntax::Idle;
        if !self.prefs.syntax_highlighting || self.threads_only {
            return;
        }
        let Some(path) = self.path.clone() else {
            return;
        };
        let (diff, generation) = (self.diff.clone(), self.syntax_gen);
        self.syntax = Syntax::Pending(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { highlight_diff(&path, &diff) })
                .await;
            this.update(cx, |this, cx| {
                if this.syntax_gen == generation {
                    this.syntax = Syntax::Ready(result.clone());
                    cx.emit(DiffEvent::Highlighted(result));
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// Applies new preferences. Changing the view mode keeps the cursor on
    /// the same line.
    pub fn set_preferences(&mut self, prefs: ResolvedDiffPreferences, cx: &mut Context<Self>) {
        let old = LayoutOptions::from_prefs(&self.prefs);
        let mode_changed = prefs.view_mode != self.prefs.view_mode;
        let syntax_toggled = prefs.syntax_highlighting != self.prefs.syntax_highlighting;
        // Wrapping changes the height of the code rows, which the list has
        // measured; it has to start over like for a new layout.
        let wrap_toggled = prefs.line_wrapping != self.prefs.line_wrapping;
        self.prefs = prefs;
        if syntax_toggled {
            if !self.prefs.syntax_highlighting {
                // Stop a running job; a finished result is kept for when
                // the colors come back.
                if matches!(self.syntax, Syntax::Pending(_)) {
                    self.syntax_gen += 1;
                    self.syntax = Syntax::Idle;
                }
            } else if matches!(self.syntax, Syntax::Idle) {
                self.start_syntax(cx);
            }
        }
        if old == LayoutOptions::from_prefs(&self.prefs) && !wrap_toggled {
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

    #[cfg(test)]
    pub(crate) fn side_scroll_for_test(&self) -> Pixels {
        self.side_scroll.x()
    }

    /// Height of a row as the list measured it (it must have been drawn).
    #[cfg(test)]
    pub(crate) fn row_height_for_test(&self, row: usize) -> Option<Pixels> {
        self.list.bounds_for_item(row).map(|b| b.size.height)
    }

    // --- internals -------------------------------------------------------

    /// Where the cursor is, in file lines and relative to the viewport top,
    /// and the thread it is on, if it is on a thread row.
    fn keep_position(&self) -> Keep {
        let top = self.top_row();
        (
            self.layout.anchor(self.cursor),
            self.cursor.saturating_sub(top),
            self.thread_id_at(self.cursor),
        )
    }

    fn rebuild(&mut self, keep: Keep, cx: &mut Context<Self>) {
        self.rebuild_with(keep, None);
        cx.emit(DiffEvent::RowsChanged);
        cx.notify();
    }

    fn rebuild_with(&mut self, (anchor, rel, thread): Keep, follow: Option<u32>) {
        self.layout = Arc::new(Layout::build_with_threads(
            &self.diff,
            &LayoutOptions {
                threads_only: self.threads_only,
                ..LayoutOptions::from_prefs(&self.prefs)
            },
            &self.expansion,
            &self.slots,
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
        if fold.is_none()
            && let Some(row) = thread.and_then(|id| self.thread_row_of(&id))
        {
            self.cursor = row;
        }
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
        self.register_editor_focus();
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

    /// The nearest row the cursor may rest on: `row` itself if it is one
    /// (a thread row too), else the nearest line, looking forward first.
    fn snap(&self, row: usize) -> usize {
        let rows = self.layout.rows();
        let row = row.min(rows.len().saturating_sub(1));
        if rows.get(row).is_some_and(Row::selectable) {
            return row;
        }
        (row..rows.len())
            .chain((0..row).rev())
            .find(|&i| rows[i].is_line())
            .or_else(|| rows.iter().position(Row::selectable))
            .unwrap_or(0)
    }

    fn thread_at(&self, slot: usize) -> Option<&Thread> {
        match self.shown.get(slot) {
            Some(&i) => self.threads.get(i),
            None if slot == self.shown.len() => self.new_thread.as_ref(),
            None => None,
        }
    }

    /// The thread on `row`, if it is a thread row.
    fn thread_id_at(&self, row: usize) -> Option<ThreadId> {
        match self.layout.rows().get(row) {
            Some(Row::Thread(tr)) => self.thread_at(tr.slot).map(|t| t.id.clone()),
            _ => None,
        }
    }

    fn thread_row_of(&self, id: &ThreadId) -> Option<usize> {
        self.layout
            .thread_rows()
            .iter()
            .copied()
            .find(|&ix| self.thread_id_at(ix).as_ref() == Some(id))
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

    /// Moves by `delta` lines (rows that are not lines, thread rows
    /// included, are stepped over), stopping at the ends.
    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows = self.layout.rows();
        let (mut i, mut at) = (self.cursor, self.cursor);
        let mut left = delta.unsigned_abs();
        while left > 0 {
            let next = if delta > 0 {
                i.checked_add(1).filter(|&n| n < rows.len())
            } else {
                i.checked_sub(1)
            };
            let Some(n) = next else { break };
            i = n;
            if rows[i].is_line() {
                left -= 1;
                at = i;
            }
        }
        self.move_to(at, Reveal::Nearest, cx);
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

    fn scroll_left(&mut self, _: &ScrollLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_sideways(-SIDE_STEP, cx);
    }

    fn scroll_right(&mut self, _: &ScrollRight, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_sideways(SIDE_STEP, cx);
    }

    fn scroll_sideways(&mut self, columns: f32, cx: &mut Context<Self>) {
        if self.prefs.line_wrapping {
            self.say("Lines are wrapped", cx);
            return;
        }
        let Some(char_w) = self.style.as_ref().map(|s| s.char_w) else {
            return;
        };
        if self.side_scroll.scroll_by(char_w * columns) {
            cx.notify();
        }
    }

    /// How far the code text can scroll sideways: until the end of the
    /// longest line, and two columns more, shows in its pane.
    fn side_scroll_max(&self, style: &Style, width: Pixels) -> Pixels {
        let gutter = |g: Pixels| if style.hide_numbers { px(0.) } else { g };
        let over = |chars: usize, pane: Pixels| {
            (style.char_w * (chars + 2) as f32 + TEXT_INSET - pane).max(px(0.))
        };
        let (left, right) = self.layout.widest();
        match self.layout.mode() {
            DiffViewMode::SideBySide => {
                let half = width / 2.;
                over(left, half - gutter(style.old_gutter))
                    .max(over(right, half - gutter(style.new_gutter)))
            }
            DiffViewMode::Unified => over(
                left.max(right),
                width - gutter(style.old_gutter) - gutter(style.new_gutter),
            ),
        }
    }

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
        let first = self.layout.first_selectable().unwrap_or(0);
        self.move_to(first, Reveal::Nearest, cx);
    }

    fn cursor_bottom(&mut self, _: &CursorBottom, _: &mut Window, cx: &mut Context<Self>) {
        let last = self.layout.last_selectable().unwrap_or(usize::MAX);
        self.move_to(last, Reveal::Nearest, cx);
    }

    fn toggle_mode_action(&mut self, _: &ToggleMode, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_mode(cx);
    }

    /// Enter / `x`: opens the fold under the cursor, or opens / closes the
    /// thread under it.
    fn expand_fold_action(&mut self, _: &ExpandFold, _: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.layout.rows().get(self.cursor), Some(Row::Thread(_))) {
            self.toggle_thread(self.cursor, cx);
        } else {
            self.expand_fold(self.cursor, cx);
        }
    }

    fn next_thread(&mut self, _: &NextThread, _: &mut Window, cx: &mut Context<Self>) {
        match self.layout.next_thread(self.cursor) {
            Some(row) => self.move_to(row, Reveal::Center, cx),
            None => self.say("No next comment thread", cx),
        }
    }

    fn prev_thread(&mut self, _: &PrevThread, _: &mut Window, cx: &mut Context<Self>) {
        match self.layout.prev_thread(self.cursor) {
            Some(row) => self.move_to(row, Reveal::Center, cx),
            None => self.say("No previous comment thread", cx),
        }
    }

    fn expand_threads(&mut self, _: &ExpandThreads, _: &mut Window, cx: &mut Context<Self>) {
        self.set_all_threads_open(true, cx);
    }

    fn collapse_threads(&mut self, _: &CollapseThreads, _: &mut Window, cx: &mut Context<Self>) {
        self.set_all_threads_open(false, cx);
    }

    fn say(&mut self, message: &'static str, cx: &mut Context<Self>) {
        self.hint = Some(message.into());
        cx.notify();
    }

    /// Opens or closes the thread on `row`; its height changes.
    fn toggle_thread(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(id) = self.thread_id_at(row) else {
            return;
        };
        let open = !self.is_thread_open(&id);
        self.thread_open.insert(id, open);
        self.list.remeasure_items(row..row + 1);
        self.hint = None;
        cx.emit(DiffEvent::RowsChanged);
        cx.notify();
    }

    /// Opens (or closes) every thread shown in the file.
    pub fn set_all_threads_open(&mut self, open: bool, cx: &mut Context<Self>) {
        let ids: Vec<ThreadId> = self.shown_threads().map(|t| t.id.clone()).collect();
        if ids.is_empty() {
            self.say("No comment threads in this file", cx);
            return;
        }
        for id in ids {
            self.thread_open.insert(id, open);
        }
        for ix in self.layout.thread_rows().to_vec() {
            self.list.remeasure_items(ix..ix + 1);
        }
        self.hint = None;
        cx.emit(DiffEvent::RowsChanged);
        cx.notify();
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
        let wrap = style.wrap;
        let body = match row {
            Row::Side { left, right } => code_row(wrap)
                .child(cell_element(
                    left.as_ref(),
                    DiffSide::Left,
                    style.old_gutter,
                    &style,
                ))
                .child(
                    cell_element(right.as_ref(), DiffSide::Right, style.new_gutter, &style)
                        .border_l_1()
                        .border_color(style.palette.divider),
                )
                .into_any_element(),
            Row::Unified { old, new, cell } => code_row(wrap)
                .child(number_element(*old, style.old_gutter, cell.kind, &style))
                .child(number_element(*new, style.new_gutter, cell.kind, &style))
                .child(text_pane(
                    cell,
                    // One cell stands for both sides; common lines show the
                    // new one.
                    if cell.kind == LineKind::Removed {
                        DiffSide::Left
                    } else {
                        DiffSide::Right
                    },
                    &style,
                ))
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
            Row::Thread(tr) => thread_card::thread_row(self, ix, tr, &style, cx),
        };
        let is_thread = matches!(row, Row::Thread(_));
        // A wrapped line is as tall as its text.
        let grows = wrap && matches!(row, Row::Side { .. } | Row::Unified { .. });
        let selected = ix == self.cursor && row.selectable();
        let reveal = self.reveal.get().filter(|_| selected);
        let viewport = self.viewport;
        div()
            .id(("diff-row", ix))
            .relative()
            .w_full()
            .when(grows, |d| d.min_h(style.row_height))
            .when(!is_thread && !grows, |d| d.h(style.row_height))
            .child(body)
            .when(selected, |d| {
                d.child(
                    div()
                        .absolute()
                        .inset_0()
                        // A thread card keeps its own colours; the cursor
                        // shows as the bar and the card's border.
                        .when(!is_thread, |d| d.bg(style.palette.cursor_bg))
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

/// The row of a code line: its cells side by side. A wrapped line makes
/// the row taller, and every cell stretches to it, so that the backgrounds
/// meet.
fn code_row(wrap: bool) -> Div {
    let row = h_flex().w_full();
    if wrap {
        row.items_stretch()
    } else {
        row.h_full()
    }
}

fn number_element(n: Option<u32>, width: Pixels, kind: LineKind, style: &Style) -> Div {
    div()
        .flex()
        .flex_none()
        // Next to a wrapped line the number stays on its first row.
        .when(style.wrap, |d| d.items_start())
        .when(!style.wrap, |d| d.items_center().h_full())
        .justify_end()
        .w(width)
        .pr(px(8.))
        .text_color(style.palette.gutter_fg)
        .bg(style.palette.gutter_bg(kind))
        .when(style.hide_numbers, |d| d.hidden())
        .children(n.map(|n| n.to_string()))
}

fn text_pane(cell: &Cell, side: DiffSide, style: &Style) -> Div {
    let p = &style.palette;
    div()
        .relative()
        .flex_1()
        .min_w_0()
        .when(!style.wrap, |d| d.h_full().whitespace_nowrap())
        .when(style.wrap, |d| d.whitespace_normal())
        .overflow_hidden()
        .pl(TEXT_INSET)
        .bg(p.line_bg(cell.kind, cell.due_to_rebase))
        .when(cell.due_to_rebase, |d| d.text_color(p.muted_fg))
        .children(style.ruler.map(|x| {
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(x + TEXT_INSET - style.scroll_x)
                .w(px(1.))
                .bg(p.ruler)
        }))
        .when(!cell.text.is_empty(), |d| {
            let syntax = style
                .syntax
                .as_ref()
                .map(|s| s.runs(cell, side))
                .unwrap_or_default();
            let text = StyledText::new(SharedString::from(cell.text.clone()))
                .with_highlights(line_highlights(cell, p, &syntax));
            let shifted = |text| div().relative().left(-style.scroll_x).child(text);
            if cell.urls.is_empty() {
                return d.child(shifted(text));
            }
            // Ctrl-click (Cmd on macOS) on a URL opens it. The click still
            // reaches the row, so it moves the cursor there as well.
            let layout = text.layout().clone();
            let urls = cell.urls.clone();
            let line = cell.text.clone();
            d.child(shifted(text))
                .on_mouse_up(MouseButton::Left, move |e, _, cx| {
                    if !e.modifiers.secondary() {
                        return;
                    }
                    if let Ok(ix) = layout.index_for_position(e.position)
                        && let Some(r) = urls.iter().find(|r| r.contains(&ix))
                    {
                        cx.open_url(&line[r.clone()]);
                    }
                })
        })
}

/// One half of a side-by-side row; a blank filler without a cell.
fn cell_element(cell: Option<&Cell>, side: DiffSide, gutter: Pixels, style: &Style) -> Div {
    let p = &style.palette;
    let half = || {
        let d = h_flex().flex_1().min_w_0();
        if style.wrap {
            d.items_stretch()
        } else {
            d.h_full()
        }
    };
    match cell {
        Some(cell) => half()
            .overflow_hidden()
            .child(number_element(Some(cell.number), gutter, cell.kind, style))
            .child(text_pane(cell, side, style)),
        None => half()
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
        let t = crate::theme::diff_theme(window, cx);
        let (family, size, row_height, bg, fg, strip) = {
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
        let mut style = Style::new(
            self.font_family.clone(),
            self.prefs.line_length,
            self.prefs.hide_line_numbers,
            Palette::new(self.focus.is_focused(window), &t, cx),
            self.layout.max_numbers(),
            &t,
            window,
        );
        style.wrap = self.prefs.line_wrapping;
        let bounds = self.list.viewport_bounds();
        if bounds.size.width > px(0.) {
            let max = if style.wrap {
                px(0.)
            } else {
                self.side_scroll_max(&style, bounds.size.width)
            };
            self.side_scroll.set_limits(bounds, max);
        }
        style.scroll_x = self.side_scroll.x();
        if let Some(syntax) = self.active_syntax() {
            style.syntax = Some(SyntaxPaint {
                syntax: syntax.clone(),
                theme: t.highlight_theme.clone(),
                tab_size: self.prefs.tab_size,
                show_tabs: self.prefs.show_tabs,
            });
        }
        self.style = Some(Rc::new(style));
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
            .on_action(cx.listener(Self::scroll_left))
            .on_action(cx.listener(Self::scroll_right))
            .on_action(cx.listener(Self::toggle_mode_action))
            .on_action(cx.listener(Self::expand_fold_action))
            .on_action(cx.listener(Self::expand_above))
            .on_action(cx.listener(Self::expand_below))
            .on_action(cx.listener(Self::toggle_all_folds))
            .on_action(cx.listener(Self::next_thread))
            .on_action(cx.listener(Self::prev_thread))
            .on_action(cx.listener(Self::expand_threads))
            .on_action(cx.listener(Self::collapse_threads))
            .on_action(cx.listener(Self::new_comment))
            .on_action(cx.listener(Self::quote_reply))
            .on_action(cx.listener(Self::thread_done))
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
                    .on_scroll_wheel(cx.listener(|this, e: &ScrollWheelEvent, _, cx| {
                        // The list scrolls up and down; sideways is ours.
                        let dx = e.delta.pixel_delta(this.row_height).x;
                        if dx != px(0.) && this.side_scroll.scroll_by(-dx) {
                            cx.notify();
                        }
                    }))
                    .vertical_scrollbar(&self.list)
                    .when(!self.prefs.line_wrapping, |d| {
                        d.horizontal_scrollbar(&self.side_scroll)
                    }),
            )
    }
}
