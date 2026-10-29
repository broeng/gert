//! The pure layout model of a diff: rows for the side-by-side and unified
//! modes, derived once from a [`model::Diff`]. No GPUI types, so it is
//! testable without a window, and building it is O(lines).
//!
//! Decisions:
//! - `diff_header` (the `git diff` header lines) is not shown. A rename or
//!   copy gets a one-line [`Notice`] instead.
//! - Lines are stored with tabs already expanded (see [`expand_line`]) and
//!   long lines cut at [`MAX_LINE_BYTES`], so the view never has to map
//!   offsets again.
//! - A [`model::ChangedChunk`] flagged `common` (equal apart from ignored
//!   whitespace) is shown as plain common lines and is not a navigable chunk.
//!   For folding it counts as unchanged, like any other common line.
//!
//! Folding. Gerrit 3.x ignores the `context` parameter and always sends the
//! whole file, so unchanged lines are folded here, like PolyGerrit does.
//! Every run of unchanged lines is looked at on its own:
//! - next to a change it keeps `n` lines (the context preference) on that
//!   side: `n` after the previous change, `n` before the next one; at the
//!   start or end of the file only the side facing a change keeps lines;
//! - what is left in between becomes one [`FoldRow`], unless it is fewer than
//!   [`MIN_FOLD`] lines (a fold row would save nothing) or the context is
//!   "whole file";
//! - a file without any change (a pure rename or mode change) keeps its first
//!   `n` lines and folds the rest;
//! - [`Expansion`] records what the user revealed, per run, and is applied on
//!   top of the context, so it survives context and mode changes.
//!
//! A [`DiffChunk::Skip`] (not sent by Gerrit 3.x, but by other servers) can
//! not be expanded locally: its lines were never received. It becomes a
//! [`Row::Hidden`], and unchanged runs next to one are not folded.
//!
//! Comment threads. [`Layout::build_with_threads`] takes a [`ThreadSlot`]
//! (side and location) per thread and adds one [`Row::Thread`] each:
//! - after the row whose cell number on the thread's side equals the end
//!   line of its location. Side by side, left threads come first, then right
//!   threads, after the pair row; unified, after the line row, left then
//!   right. Threads on lines the server skipped follow the [`Row::Hidden`];
//! - file-level threads, and threads whose line is outside the file
//!   (`detached`), come at the top, after the notices;
//! - a line a thread is anchored at is never folded (for a range: its last
//!   [`MAX_RANGE_LINES`] lines and its first one). It splits its run of
//!   unchanged lines into pieces whose edges facing it count as changes, so
//!   the context preference applies around it; a piece is keyed by its old
//!   start line for [`Expansion`];
//! - the characters of a range location are reported in [`Cell::comments`].
//!
//! Placement costs O(rows + threads): hash lookups by line number.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ops::Range,
    sync::Arc,
};

use model::{
    Diff, DiffChangeType, DiffChunk, DiffContext, DiffSide, DiffViewMode, Location,
    ResolvedDiffPreferences,
};

/// Lines longer than this are cut (and end in `…`). Shaping minified
/// one-liners of hundreds of kilobytes would stall the frame.
pub const MAX_LINE_BYTES: usize = 4096;

/// Folds hiding fewer lines than this are not made: the lines are shown.
pub const MIN_FOLD: u32 = 3;

/// How many lines "+N above / below" reveal.
pub const REVEAL_STEP: u32 = 10;

/// Of a thread on a range, this many lines (the last ones) are kept out of
/// folds and highlighted; so is the first one (kept visible only). A thread
/// over a whole file must not unfold it.
pub const MAX_RANGE_LINES: u32 = 200;

/// What the layout is built from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutOptions {
    pub mode: DiffViewMode,
    pub tab_size: u32,
    /// Show a marker (`→`) in the first cell of every tab.
    pub show_tabs: bool,
    /// Use the server's intraline edits.
    pub intraline: bool,
    /// How many unchanged lines to keep around changes.
    pub context: DiffContext,
    /// The diff is no file but the carrier of threads that belong to none
    /// (`DiffView::threads_only`): no "no changes" notice.
    pub threads_only: bool,
}

impl LayoutOptions {
    pub fn from_prefs(prefs: &ResolvedDiffPreferences) -> Self {
        Self {
            mode: prefs.view_mode,
            tab_size: prefs.tab_size,
            show_tabs: prefs.show_tabs,
            intraline: prefs.intraline_difference,
            context: prefs.context,
            threads_only: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Common,
    Removed,
    Added,
}

/// One line of text on one side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    /// 1-based line number in the file on this side.
    pub number: u32,
    /// Display text: tabs expanded, no line terminator.
    pub text: Arc<str>,
    /// Intraline highlights, byte ranges into `text`, sorted.
    pub marks: Vec<Range<usize>>,
    /// The tab markers inside `text` (empty unless tabs are shown).
    pub tabs: Vec<Range<usize>>,
    pub kind: LineKind,
    /// The line only differs because of a rebase.
    pub due_to_rebase: bool,
    /// Parts of the line a comment thread is anchored at (a range
    /// location), byte ranges into `text`, sorted and not overlapping.
    pub comments: Vec<Range<usize>>,
    /// URLs in `text` (byte ranges, sorted); Ctrl-click opens them. A line
    /// is never split into several cells (wrapping is only drawn), so a URL
    /// always sits in one cell.
    pub urls: Vec<Range<usize>>,
    /// The line as it is in the file, when `text` differs from it (tabs
    /// expanded, a long line cut): syntax spans are byte ranges of it.
    pub raw: Option<Arc<str>>,
}

/// What the layout needs to know of a comment thread to place it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadSlot {
    pub side: DiffSide,
    pub location: Location,
}

/// A comment thread between the lines. `slot` indexes the slice given to
/// [`Layout::build_with_threads`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadRow {
    pub slot: usize,
    pub side: DiffSide,
    /// The thread's line is not in the file (it is shown at the top).
    pub detached: bool,
}

/// A folded run of unchanged lines (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoldRow {
    /// How many lines are hidden.
    pub lines: u32,
    /// First hidden line number on the old and new side (1-based).
    pub old_start: u32,
    pub new_start: u32,
    /// Identifies the run of unchanged lines this fold is part of (the old
    /// line number of the run's first line), for [`Expansion`].
    pub run: u32,
    /// Lines can be revealed at the top / bottom edge of the fold: it borders
    /// on a change there (or, in a file without changes, on the start).
    /// Revealing from the far side of a fold at a file end would not help
    /// anyone, so those edges offer nothing.
    pub above: bool,
    pub below: bool,
}

/// What the user revealed of the folds of one file. Keyed by run, not by
/// fold, so it applies to whatever the context preference folds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Expansion {
    /// Show everything.
    all: bool,
    runs: BTreeMap<u32, Reveal>,
}

/// Lines revealed from the top / bottom edge of a run's fold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Reveal {
    top: u32,
    bottom: u32,
}

impl Expansion {
    /// Reveals `n` more lines at the top edge of the fold of `run` (the
    /// ones next to the preceding lines).
    pub fn reveal_top(&mut self, run: u32, n: u32) {
        let r = self.runs.entry(run).or_default();
        r.top = r.top.saturating_add(n);
    }

    /// Reveals `n` more lines at the bottom edge of the fold of `run`.
    pub fn reveal_bottom(&mut self, run: u32, n: u32) {
        let r = self.runs.entry(run).or_default();
        r.bottom = r.bottom.saturating_add(n);
    }

    /// Reveals the whole fold of `run`.
    pub fn reveal_run(&mut self, run: u32) {
        self.runs.insert(
            run,
            Reveal {
                top: u32::MAX,
                bottom: 0,
            },
        );
    }

    /// Reveals every fold of the file.
    pub fn reveal_everything(&mut self) {
        self.all = true;
    }

    /// Folds everything again.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Whether everything is revealed by [`Self::reveal_everything`].
    pub fn is_everything(&self) -> bool {
        self.all
    }

    fn get(&self, run: u32) -> Reveal {
        self.runs.get(&run).copied().unwrap_or_default()
    }
}

/// A one-line message in place of (or above) the lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    Binary,
    /// No content at all (identical files, an empty added file, a mode change).
    Empty,
    RenamedFrom(String),
    CopiedFrom(String),
}

impl Notice {
    pub fn text(&self) -> String {
        match self {
            Self::Binary => "Binary file, no preview".to_owned(),
            Self::Empty => "No content differences".to_owned(),
            Self::RenamedFrom(p) => format!("Renamed from {p}"),
            Self::CopiedFrom(p) => format!("Copied from {p}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// Side-by-side: either side may be a blank filler.
    Side {
        left: Option<Cell>,
        right: Option<Cell>,
    },
    /// Unified: `cell.number` is the number of the side the line is on
    /// (`new` for common lines).
    Unified {
        old: Option<u32>,
        new: Option<u32>,
        cell: Cell,
    },
    Fold(FoldRow),
    /// Lines the server left out (`skip`): they were never received and
    /// cannot be shown.
    Hidden(HiddenRow),
    Notice(Notice),
    /// A comment thread (one row, whatever its height).
    Thread(ThreadRow),
}

/// See [`Row::Hidden`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HiddenRow {
    pub lines: u32,
    pub old_start: u32,
    pub new_start: u32,
}

impl Row {
    pub fn old_number(&self) -> Option<u32> {
        match self {
            Self::Side { left, .. } => left.as_ref().map(|c| c.number),
            Self::Unified { old, .. } => *old,
            Self::Fold(s) => Some(s.old_start),
            Self::Hidden(s) => Some(s.old_start),
            Self::Notice(_) | Self::Thread(_) => None,
        }
    }

    pub fn new_number(&self) -> Option<u32> {
        match self {
            Self::Side { right, .. } => right.as_ref().map(|c| c.number),
            Self::Unified { new, .. } => *new,
            Self::Fold(s) => Some(s.new_start),
            Self::Hidden(s) => Some(s.new_start),
            Self::Notice(_) | Self::Thread(_) => None,
        }
    }

    /// Whether the cursor may rest on this row.
    pub fn selectable(&self) -> bool {
        !matches!(self, Self::Notice(_) | Self::Hidden(_))
    }

    /// Whether this is a row `j` / `k` stop at: a line or a fold (the
    /// cursor can also be put on a thread row, by thread navigation or a
    /// click, but the line cursor steps over those).
    pub fn is_line(&self) -> bool {
        self.selectable() && !matches!(self, Self::Thread(_))
    }

    fn covers(&self, side: Side, n: u32) -> bool {
        let range = |start: u32, lines: u32| start..start + lines;
        match (self, side) {
            (Self::Fold(s), Side::Old) => range(s.old_start, s.lines).contains(&n),
            (Self::Fold(s), Side::New) => range(s.new_start, s.lines).contains(&n),
            (Self::Hidden(s), Side::Old) => range(s.old_start, s.lines).contains(&n),
            (Self::Hidden(s), Side::New) => range(s.new_start, s.lines).contains(&n),
            (_, Side::Old) => self.old_number() == Some(n),
            (_, Side::New) => self.new_number() == Some(n),
        }
    }
}

#[derive(Clone, Copy)]
enum Side {
    Old,
    New,
}

/// A position in the file independent of the layout mode: the line numbers
/// a row stands for. Used to keep the cursor when the mode or diff changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Anchor {
    pub old: Option<u32>,
    pub new: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct Layout {
    mode: DiffViewMode,
    rows: Vec<Row>,
    /// Row ranges of the changed chunks that count for `n` / `p`.
    chunks: Vec<Range<usize>>,
    /// Indexes of the thread rows, ascending.
    thread_rows: Vec<usize>,
    /// Highest line number on the old / new side, for the gutter width.
    max_old: u32,
    max_new: u32,
    /// The longest text in the left / right pane, in characters.
    widest: (usize, usize),
}

impl Layout {
    /// The layout without comment threads.
    pub fn build(diff: &Diff, opts: &LayoutOptions, expansion: &Expansion) -> Self {
        Self::build_with_threads(diff, opts, expansion, &[])
    }

    pub fn build_with_threads(
        diff: &Diff,
        opts: &LayoutOptions,
        expansion: &Expansion,
        threads: &[ThreadSlot],
    ) -> Self {
        let segments = if diff.binary {
            Vec::new()
        } else {
            segments(diff)
        };
        let (old, new) = segments.last().map_or((1, 1), |s| {
            (s.old_start() + s.old_len(), s.new_start() + s.new_len())
        });
        let mut b = Builder {
            opts: *opts,
            rows: Vec::new(),
            chunks: Vec::new(),
            thread_rows: Vec::new(),
            plan: ThreadPlan::new(threads, &segments, old - 1, new - 1),
        };
        if diff.binary {
            b.rows.push(Row::Notice(Notice::Binary));
        } else {
            b.rename_notice(diff);
            if diff.chunks.is_empty() && !opts.threads_only {
                b.rows.push(Row::Notice(Notice::Empty));
            }
        }
        b.place_top();
        for (i, seg) in segments.iter().enumerate() {
            match seg {
                Segment::Run(run) => {
                    let before = i.checked_sub(1).map(|j| &segments[j]);
                    let after = segments.get(i + 1);
                    b.run(run, Neighbours::of(before, after), expansion);
                }
                Segment::Changed(c, o, n) => b.changed(c, *o, *n),
                Segment::Hidden(h) => {
                    b.rows.push(Row::Hidden(h.clone()));
                    b.place_hidden(i);
                }
            }
        }
        let widest = widest(&b.rows);
        Self {
            mode: opts.mode,
            rows: b.rows,
            chunks: b.chunks,
            thread_rows: b.thread_rows,
            max_old: old - 1,
            max_new: new - 1,
            widest,
        }
    }

    pub fn mode(&self) -> DiffViewMode {
        self.mode
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Row ranges of the navigable changed chunks, in order.
    pub fn chunks(&self) -> &[Range<usize>] {
        &self.chunks
    }

    /// Index of the first row of every changed chunk.
    pub fn chunk_starts(&self) -> Vec<usize> {
        self.chunks.iter().map(|r| r.start).collect()
    }

    /// Start of the first chunk beginning after `row`.
    pub fn next_chunk(&self, row: usize) -> Option<usize> {
        let i = self.chunks.partition_point(|r| r.start <= row);
        self.chunks.get(i).map(|r| r.start)
    }

    /// Start of the last chunk lying entirely before `row` (so from inside
    /// a chunk this is the previous chunk, not the start of the current).
    pub fn prev_chunk(&self, row: usize) -> Option<usize> {
        let i = self.chunks.partition_point(|r| r.end <= row);
        i.checked_sub(1).map(|i| self.chunks[i].start)
    }

    /// The widest line number on each side, for sizing the gutters.
    pub fn max_numbers(&self) -> (u32, u32) {
        (self.max_old, self.max_new)
    }

    /// The longest line shown in the left and the right text pane, in
    /// characters (tabs expanded), for how far the text scrolls sideways.
    /// Unified rows have one pane, so both are the same.
    pub fn widest(&self) -> (usize, usize) {
        self.widest
    }

    /// Indexes of the thread rows, ascending.
    pub fn thread_rows(&self) -> &[usize] {
        &self.thread_rows
    }

    /// The first thread row after `row`.
    pub fn next_thread(&self, row: usize) -> Option<usize> {
        let i = self.thread_rows.partition_point(|&r| r <= row);
        self.thread_rows.get(i).copied()
    }

    /// The last thread row before `row`.
    pub fn prev_thread(&self, row: usize) -> Option<usize> {
        let i = self.thread_rows.partition_point(|&r| r < row);
        i.checked_sub(1).map(|i| self.thread_rows[i])
    }

    /// The row the initial cursor goes to (never a thread row).
    pub fn first_selectable(&self) -> Option<usize> {
        self.rows.iter().position(Row::is_line)
    }

    pub fn last_selectable(&self) -> Option<usize> {
        self.rows.iter().rposition(Row::is_line)
    }

    /// The file lines a row stands for. A thread row stands for the row
    /// above it, so that the cursor keeps its place when threads come
    /// and go.
    pub fn anchor(&self, row: usize) -> Anchor {
        let mut i = row;
        while matches!(self.rows.get(i), Some(Row::Thread(_))) {
            match i.checked_sub(1) {
                Some(above) => i = above,
                None => return Anchor::default(),
            }
        }
        self.rows.get(i).map_or_else(Anchor::default, |r| Anchor {
            old: r.old_number(),
            new: r.new_number(),
        })
    }

    /// The row standing for `anchor`: the one showing the new-side line
    /// if there is one, else the old-side line. Hidden lines are covered
    /// by their skip row.
    pub fn find(&self, anchor: Anchor) -> Option<usize> {
        let look = |side: Side, n: Option<u32>| {
            let n = n?;
            self.rows.iter().position(|r| r.covers(side, n))
        };
        look(Side::New, anchor.new).or_else(|| look(Side::Old, anchor.old))
    }
}

fn len32<T>(v: &[T]) -> u32 {
    u32::try_from(v.len()).unwrap_or(u32::MAX)
}

/// A line on both sides: the old and the new text (they differ only for
/// lines of a whitespace-only change).
type PairedLine<'a> = (&'a str, &'a str);

fn widest(rows: &[Row]) -> (usize, usize) {
    let width = |c: Option<&Cell>| c.map_or(0, |c| c.text.chars().count());
    rows.iter().fold((0, 0), |(l, r), row| match row {
        Row::Side { left, right } => (l.max(width(left.as_ref())), r.max(width(right.as_ref()))),
        Row::Unified { cell, .. } => {
            let w = l.max(width(Some(cell)));
            (w, w)
        }
        _ => (l, r),
    })
}

/// Consecutive unchanged lines.
struct Run<'a> {
    old: u32,
    new: u32,
    lines: Vec<PairedLine<'a>>,
}

enum Segment<'a> {
    Run(Run<'a>),
    Changed(&'a model::ChangedChunk, u32, u32),
    Hidden(HiddenRow),
}

impl Segment<'_> {
    fn old_start(&self) -> u32 {
        match self {
            Self::Run(r) => r.old,
            Self::Changed(_, old, _) => *old,
            Self::Hidden(h) => h.old_start,
        }
    }

    fn new_start(&self) -> u32 {
        match self {
            Self::Run(r) => r.new,
            Self::Changed(_, _, new) => *new,
            Self::Hidden(h) => h.new_start,
        }
    }

    fn old_len(&self) -> u32 {
        match self {
            Self::Run(r) => len32(&r.lines),
            Self::Changed(c, ..) => len32(&c.a),
            Self::Hidden(h) => h.lines,
        }
    }

    fn new_len(&self) -> u32 {
        match self {
            Self::Run(r) => len32(&r.lines),
            Self::Changed(c, ..) => len32(&c.b),
            Self::Hidden(h) => h.lines,
        }
    }
}

/// The diff as runs of unchanged lines between changes. Adjacent common
/// chunks and whitespace-only changes are merged into one run.
fn segments(diff: &Diff) -> Vec<Segment<'_>> {
    let mut out: Vec<Segment<'_>> = Vec::new();
    let (mut old, mut new) = (1_u32, 1_u32);
    fn push_lines<'a>(out: &mut Vec<Segment<'a>>, lines: Vec<PairedLine<'a>>, old: u32, new: u32) {
        if let Some(Segment::Run(r)) = out.last_mut() {
            r.lines.extend(lines);
        } else {
            out.push(Segment::Run(Run { old, new, lines }));
        }
    }
    for chunk in &diff.chunks {
        match chunk {
            DiffChunk::Common { lines } => {
                let n = len32(lines);
                let paired = lines.iter().map(|l| (l.as_str(), l.as_str())).collect();
                push_lines(&mut out, paired, old, new);
                old += n;
                new += n;
            }
            DiffChunk::Changed(c) if c.common && c.a.len() == c.b.len() => {
                let paired = c.a.iter().zip(&c.b);
                let paired = paired.map(|(a, b)| (a.as_str(), b.as_str())).collect();
                push_lines(&mut out, paired, old, new);
                old += len32(&c.a);
                new += len32(&c.b);
            }
            DiffChunk::Changed(c) => {
                out.push(Segment::Changed(c, old, new));
                old += len32(&c.a);
                new += len32(&c.b);
            }
            DiffChunk::Skip { lines } => {
                out.push(Segment::Hidden(HiddenRow {
                    lines: *lines,
                    old_start: old,
                    new_start: new,
                }));
                old += lines;
                new += lines;
            }
        }
    }
    out
}

/// What a run of unchanged lines borders on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Edge {
    FileBoundary,
    Change,
    /// Lines the server did not send.
    Hidden,
}

struct Neighbours {
    before: Edge,
    after: Edge,
}

impl Neighbours {
    fn of(before: Option<&Segment<'_>>, after: Option<&Segment<'_>>) -> Self {
        let edge = |s: Option<&Segment<'_>>| match s {
            None => Edge::FileBoundary,
            Some(Segment::Hidden(_)) => Edge::Hidden,
            Some(_) => Edge::Change,
        };
        Self {
            before: edge(before),
            after: edge(after),
        }
    }
}

struct Builder {
    opts: LayoutOptions,
    rows: Vec<Row>,
    chunks: Vec<Range<usize>>,
    thread_rows: Vec<usize>,
    plan: ThreadPlan,
}

/// Where the threads go, worked out before the rows are built.
#[derive(Default)]
struct ThreadPlan {
    sides: Vec<DiffSide>,
    /// Slots after the row with this line number on the left / right.
    left: HashMap<u32, Vec<usize>>,
    right: HashMap<u32, Vec<usize>>,
    /// Slots after the `Hidden` segment with this index.
    hidden: HashMap<usize, Vec<usize>>,
    /// File-level and detached slots, with the `detached` flag.
    top: Vec<(usize, bool)>,
    /// Lines (old / new) that must not be folded.
    keep_old: BTreeSet<u32>,
    keep_new: BTreeSet<u32>,
    /// Character ranges (`usize::MAX`: to the end of the line) per line.
    ranges: HashMap<(DiffSide, u32), Vec<Range<usize>>>,
}

impl ThreadPlan {
    fn new(slots: &[ThreadSlot], segments: &[Segment<'_>], total_old: u32, total_new: u32) -> Self {
        let mut plan = Self {
            sides: slots.iter().map(|s| s.side).collect(),
            ..Self::default()
        };
        if slots.is_empty() {
            return plan;
        }
        // Hidden segments, ascending on both sides.
        let hidden: Vec<(usize, &HiddenRow)> = segments
            .iter()
            .enumerate()
            .filter_map(|(i, s)| match s {
                Segment::Hidden(h) => Some((i, h)),
                _ => None,
            })
            .collect();
        for (i, slot) in slots.iter().enumerate() {
            let Some(end) = slot.location.line() else {
                plan.top.push((i, false));
                continue;
            };
            let (total, start_of): (u32, fn(&HiddenRow) -> u32) = match slot.side {
                DiffSide::Left => (total_old, |h| h.old_start),
                DiffSide::Right => (total_new, |h| h.new_start),
            };
            if end == 0 || end > total {
                plan.top.push((i, true));
                continue;
            }
            let k = hidden.partition_point(|(_, h)| start_of(h) <= end);
            if let Some((seg, h)) = k.checked_sub(1).map(|k| hidden[k])
                && end < start_of(h) + h.lines
            {
                plan.hidden.entry(seg).or_default().push(i);
                continue;
            }
            let (at, keep) = match slot.side {
                DiffSide::Left => (&mut plan.left, &mut plan.keep_old),
                DiffSide::Right => (&mut plan.right, &mut plan.keep_new),
            };
            at.entry(end).or_default().push(i);
            match slot.location {
                Location::Range(r) => {
                    let first = r.start_line.min(end).max(1);
                    keep.insert(first);
                    let window = first.max(end.saturating_sub(MAX_RANGE_LINES - 1));
                    for line in window..=end {
                        keep.insert(line);
                        let chars = if first == end {
                            r.start_character as usize..r.end_character as usize
                        } else if line == first && window == first {
                            r.start_character as usize..usize::MAX
                        } else if line == end {
                            0..r.end_character as usize
                        } else {
                            0..usize::MAX
                        };
                        if chars.start < chars.end {
                            plan.ranges
                                .entry((slot.side, line))
                                .or_default()
                                .push(chars);
                        }
                    }
                }
                _ => {
                    keep.insert(end);
                }
            }
        }
        plan
    }

    fn has_keyed(&self) -> bool {
        !self.left.is_empty() || !self.right.is_empty()
    }
}

impl Builder {
    fn rename_notice(&mut self, diff: &Diff) {
        let from = diff.meta_a.as_ref().map(|m| m.name.clone());
        let to = diff.meta_b.as_ref().map(|m| m.name.as_str());
        let Some(from) = from else { return };
        if Some(from.as_str()) == to {
            return;
        }
        match diff.change_type {
            Some(DiffChangeType::Renamed) => self.rows.push(Row::Notice(Notice::RenamedFrom(from))),
            Some(DiffChangeType::Copied) => self.rows.push(Row::Notice(Notice::CopiedFrom(from))),
            _ => {}
        }
    }

    fn push_thread(&mut self, slot: usize, detached: bool) {
        let side = self.plan.sides[slot];
        self.thread_rows.push(self.rows.len());
        self.rows.push(Row::Thread(ThreadRow {
            slot,
            side,
            detached,
        }));
    }

    /// File-level and detached threads, after the notices.
    fn place_top(&mut self) {
        for k in 0..self.plan.top.len() {
            let (slot, detached) = self.plan.top[k];
            self.push_thread(slot, detached);
        }
    }

    fn place_hidden(&mut self, segment: usize) {
        let Some(slots) = self.plan.hidden.get(&segment).cloned() else {
            return;
        };
        for slot in slots {
            self.push_thread(slot, false);
        }
    }

    /// The threads after the row that shows these line numbers: first the
    /// ones on the old side, then those on the new side.
    fn place(&mut self, old: Option<u32>, new: Option<u32>) {
        if !self.plan.has_keyed() {
            return;
        }
        let at = |map: &HashMap<u32, Vec<usize>>, n: Option<u32>| {
            n.and_then(|n| map.get(&n)).cloned().unwrap_or_default()
        };
        let (left, right) = (at(&self.plan.left, old), at(&self.plan.right, new));
        for slot in left.into_iter().chain(right) {
            self.push_thread(slot, false);
        }
    }

    fn cell(
        &self,
        side: DiffSide,
        number: u32,
        line: &str,
        marks: &[Range<usize>],
        kind: LineKind,
    ) -> Cell {
        let e = expand_line(line, marks, self.opts.tab_size, self.opts.show_tabs);
        let urls = crate::links::find_urls(&e.text);
        Cell {
            number,
            urls,
            text: e.text,
            marks: e.marks,
            tabs: e.tabs,
            kind,
            due_to_rebase: false,
            comments: self.comment_marks(side, number, line),
            raw: needs_expansion(line).then(|| Arc::from(line)),
        }
    }

    /// Byte ranges of the display text of `line` that threads are anchored
    /// at.
    fn comment_marks(&self, side: DiffSide, number: u32, line: &str) -> Vec<Range<usize>> {
        let Some(chars) = self.plan.ranges.get(&(side, number)) else {
            return Vec::new();
        };
        let byte = |c: usize| line.char_indices().nth(c).map_or(line.len(), |(b, _)| b);
        let bytes: Vec<Range<usize>> = chars
            .iter()
            .map(|r| byte(r.start)..byte(r.end))
            .filter(|r| r.start < r.end)
            .collect();
        let merged = merge_ranges(bytes);
        expand_line(line, &merged, self.opts.tab_size, self.opts.show_tabs).marks
    }

    /// A run of unchanged lines. Lines threads are anchored at are shown
    /// whatever the context says; they cut the run into pieces, each folded
    /// on its own, as if the commented line were a change.
    fn run(&mut self, run: &Run<'_>, edges: Neighbours, expansion: &Expansion) {
        let total = len32(&run.lines);
        let forced: Vec<u32> = if self.plan.has_keyed() {
            let mut at: BTreeSet<u32> = BTreeSet::new();
            at.extend(
                self.plan
                    .keep_old
                    .range(run.old..run.old + total)
                    .map(|n| n - run.old),
            );
            at.extend(
                self.plan
                    .keep_new
                    .range(run.new..run.new + total)
                    .map(|n| n - run.new),
            );
            at.into_iter().collect()
        } else {
            Vec::new()
        };
        if forced.is_empty() {
            return self.piece(run, 0..total, edges, expansion);
        }
        let mut pos = 0_u32;
        let mut k = 0;
        while k < forced.len() {
            let from = forced[k];
            let mut to = from + 1;
            while forced.get(k + 1) == Some(&to) {
                k += 1;
                to += 1;
            }
            k += 1;
            if pos < from {
                let before = if pos == 0 { edges.before } else { Edge::Change };
                let piece = Neighbours {
                    before,
                    after: Edge::Change,
                };
                self.piece(run, pos..from, piece, expansion);
            }
            self.lines(run, from..to);
            pos = to;
        }
        if pos < total {
            let piece = Neighbours {
                before: Edge::Change,
                after: edges.after,
            };
            self.piece(run, pos..total, piece, expansion);
        }
    }

    /// Lines `range` (indexes) of a run, unfolded.
    fn lines(&mut self, run: &Run<'_>, range: Range<u32>) {
        for i in range {
            let (a, b) = run.lines[i as usize];
            self.common(run.old + i, run.new + i, a, b);
        }
    }

    /// Lines `range` (indexes) of a run, folded as far as the context and
    /// the user's reveals allow. The fold is keyed by the old line number of
    /// the piece's first line.
    fn piece(
        &mut self,
        run: &Run<'_>,
        range: Range<u32>,
        edges: Neighbours,
        expansion: &Expansion,
    ) {
        let total = range.end - range.start;
        let key = run.old + range.start;
        let DiffContext::Lines(n) = self.opts.context else {
            return self.lines(run, range);
        };
        if expansion.all || edges.before == Edge::Hidden || edges.after == Edge::Hidden {
            return self.lines(run, range);
        }
        let (keep_top, keep_bottom) = match (edges.before, edges.after) {
            (Edge::Change, Edge::Change) => (n, n),
            (Edge::Change, _) => (n, 0),
            (_, Edge::Change) => (0, n),
            // No change in the whole file: show how it starts.
            _ => (n, 0),
        };
        let reveal = expansion.get(key);
        let top = keep_top.saturating_add(reveal.top);
        let bottom = keep_bottom.saturating_add(reveal.bottom);
        let hidden = total.saturating_sub(top.saturating_add(bottom));
        if hidden < MIN_FOLD || top.saturating_add(bottom) >= total {
            return self.lines(run, range);
        }
        self.lines(run, range.start..range.start + top);
        self.rows.push(Row::Fold(FoldRow {
            lines: hidden,
            old_start: key + top,
            new_start: run.new + range.start + top,
            run: key,
            above: keep_top > 0 || edges.before == Edge::Change,
            below: edges.after == Edge::Change,
        }));
        self.lines(run, range.end - bottom..range.end);
    }

    /// A line on both sides. `a` and `b` differ only for whitespace-only
    /// changes; otherwise they are the same text.
    fn common(&mut self, old: u32, new: u32, a: &str, b: &str) {
        let mut right = self.cell(DiffSide::Right, new, b, &[], LineKind::Common);
        if self.opts.mode == DiffViewMode::Unified && !self.plan.ranges.is_empty() {
            // One cell stands for both sides here.
            let old_side = self.comment_marks(DiffSide::Left, old, a);
            right.comments.extend(old_side);
            right.comments = merge_ranges(std::mem::take(&mut right.comments));
        }
        match self.opts.mode {
            DiffViewMode::SideBySide => {
                let left = if a == b && self.plan.ranges.is_empty() {
                    Cell {
                        number: old,
                        ..right.clone()
                    }
                } else {
                    self.cell(DiffSide::Left, old, a, &[], LineKind::Common)
                };
                self.rows.push(Row::Side {
                    left: Some(left),
                    right: Some(right),
                });
            }
            DiffViewMode::Unified => self.rows.push(Row::Unified {
                old: Some(old),
                new: Some(new),
                cell: right,
            }),
        }
        self.place(Some(old), Some(new));
    }

    fn changed(&mut self, c: &model::ChangedChunk, old: u32, new: u32) {
        let empty = |n: usize| vec![Vec::new(); n];
        let (marks_a, marks_b) = if self.opts.intraline {
            (c.marks_a(), c.marks_b())
        } else {
            (empty(c.a.len()), empty(c.b.len()))
        };
        let cells =
            |this: &Self, lines: &[String], marks: &[Vec<Range<usize>>], start, kind, side| {
                (start..)
                    .zip(lines.iter().zip(marks))
                    .map(|(n, (line, m))| {
                        let mut cell = this.cell(side, n, line, m, kind);
                        cell.due_to_rebase = c.due_to_rebase;
                        cell
                    })
                    .collect::<Vec<_>>()
            };
        let a = cells(self, &c.a, &marks_a, old, LineKind::Removed, DiffSide::Left);
        let b = cells(self, &c.b, &marks_b, new, LineKind::Added, DiffSide::Right);

        let first = self.rows.len();
        match self.opts.mode {
            DiffViewMode::SideBySide => {
                let (mut a, mut b) = (a.into_iter(), b.into_iter());
                loop {
                    let (left, right) = (a.next(), b.next());
                    if left.is_none() && right.is_none() {
                        break;
                    }
                    let numbers = (
                        left.as_ref().map(|c| c.number),
                        right.as_ref().map(|c| c.number),
                    );
                    self.rows.push(Row::Side { left, right });
                    self.place(numbers.0, numbers.1);
                }
            }
            DiffViewMode::Unified => {
                for cell in a {
                    let old = Some(cell.number);
                    self.rows.push(Row::Unified {
                        old,
                        new: None,
                        cell,
                    });
                    self.place(old, None);
                }
                for cell in b {
                    let new = Some(cell.number);
                    self.rows.push(Row::Unified {
                        old: None,
                        new,
                        cell,
                    });
                    self.place(None, new);
                }
            }
        }
        if self.rows.len() > first {
            self.chunks.push(first..self.rows.len());
        }
    }
}

/// Sorts ranges and joins those that touch or overlap.
fn merge_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => merged.push(r),
        }
    }
    merged
}

/// A line ready for display.
#[derive(Debug, PartialEq, Eq)]
pub struct Expanded {
    pub text: Arc<str>,
    pub marks: Vec<Range<usize>>,
    pub tabs: Vec<Range<usize>>,
}

/// Expands tabs to the next multiple of `tab_size` columns (each character
/// counts as one column) and cuts the line at [`MAX_LINE_BYTES`]. `marks`
/// (sorted, non-overlapping byte ranges of `line`) are translated to byte
/// ranges of the result. With `show_tabs` the first cell of each tab is a
/// `→`, reported in `tabs`.
pub fn expand_line(line: &str, marks: &[Range<usize>], tab_size: u32, show_tabs: bool) -> Expanded {
    if !needs_expansion(line) {
        return Expanded {
            text: Arc::from(line),
            marks: marks.to_vec(),
            tabs: Vec::new(),
        };
    }
    let bounds: Vec<usize> = marks.iter().flat_map(|r| [r.start, r.end]).collect();
    let (out, mapped, tabs) = expand_bounds(line, &bounds, tab_size, show_tabs);
    let marks = mapped
        .iter()
        .step_by(2)
        .zip(mapped.iter().skip(1).step_by(2))
        .filter(|(from, to)| to > from)
        .map(|(from, to)| *from..*to)
        .collect();
    Expanded {
        text: Arc::from(out),
        marks,
        tabs,
    }
}

/// Translates sorted, non-overlapping spans (byte ranges of the raw `line`,
/// each with a payload) to byte ranges of the display text [`expand_line`]
/// makes of it. Spans that fall into the cut-off tail of a long line are
/// dropped.
pub fn map_spans<T: Copy>(
    line: &str,
    spans: &[(Range<usize>, T)],
    tab_size: u32,
    show_tabs: bool,
) -> Vec<(Range<usize>, T)> {
    if !needs_expansion(line) {
        return spans.to_vec();
    }
    let bounds: Vec<usize> = spans.iter().flat_map(|(r, _)| [r.start, r.end]).collect();
    let (_, mapped, _) = expand_bounds(line, &bounds, tab_size, show_tabs);
    spans
        .iter()
        .zip(mapped.as_chunks::<2>().0)
        .filter(|(_, [from, to])| to > from)
        .map(|((_, v), [from, to])| (*from..*to, *v))
        .collect()
}

fn needs_expansion(line: &str) -> bool {
    line.len() > MAX_LINE_BYTES || line.contains('\t')
}

/// The shared core of [`expand_line`] and [`map_spans`]: the display text,
/// the display offsets of `bounds` (sorted byte offsets of `line`) and the
/// tab markers.
fn expand_bounds(
    line: &str,
    bounds: &[usize],
    tab_size: u32,
    show_tabs: bool,
) -> (String, Vec<usize>, Vec<Range<usize>>) {
    let tab = tab_size.max(1) as usize;
    let mut mapped = Vec::with_capacity(bounds.len());
    let mut out = String::with_capacity(line.len() + 16);
    let mut tabs = Vec::new();
    let mut col = 0_usize;
    let mut k = 0;
    let mut truncated = false;
    for (i, ch) in line.char_indices() {
        while k < bounds.len() && bounds[k] <= i {
            mapped.push(out.len());
            k += 1;
        }
        if out.len() >= MAX_LINE_BYTES {
            truncated = true;
            break;
        }
        if ch == '\t' {
            let width = tab - col % tab;
            if show_tabs {
                let start = out.len();
                out.push('\u{2192}');
                tabs.push(start..out.len());
                out.extend(std::iter::repeat_n(' ', width - 1));
            } else {
                out.extend(std::iter::repeat_n(' ', width));
            }
            col += width;
        } else {
            out.push(ch);
            col += 1;
        }
    }
    // Boundaries at or past the end of the (possibly cut) line.
    while k < bounds.len() {
        mapped.push(out.len());
        k += 1;
    }
    if truncated {
        out.push('\u{2026}');
    }
    (out, mapped, tabs)
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)] // expected mark lists
mod tests {
    use model::{ChangedChunk, DiffFileMeta, IntralineEdit};

    use super::*;

    fn lines(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| (*s).to_owned()).collect()
    }

    fn diff(chunks: Vec<DiffChunk>) -> Diff {
        Diff {
            meta_a: None,
            meta_b: None,
            change_type: Some(DiffChangeType::Modified),
            intraline_status: None,
            diff_header: Vec::new(),
            chunks,
            binary: false,
        }
    }

    fn common(l: &[&str]) -> DiffChunk {
        DiffChunk::Common { lines: lines(l) }
    }

    fn changed(a: &[&str], b: &[&str]) -> DiffChunk {
        DiffChunk::Changed(ChangedChunk {
            a: lines(a),
            b: lines(b),
            ..Default::default()
        })
    }

    fn opts(mode: DiffViewMode) -> LayoutOptions {
        LayoutOptions {
            mode,
            tab_size: 4,
            show_tabs: false,
            intraline: true,
            context: DiffContext::All,
            threads_only: false,
        }
    }

    /// Context, a skip, a 1:2 change, context, a trailing skip.
    fn sample() -> Diff {
        diff(vec![
            common(&["a", "b"]),
            DiffChunk::Skip { lines: 10 },
            changed(&["x"], &["y", "z"]),
            common(&["c"]),
            DiffChunk::Skip { lines: 5 },
        ])
    }

    fn build(diff: &Diff, opts: &LayoutOptions) -> Layout {
        Layout::build(diff, opts, &Expansion::default())
    }

    type Half<'a> = Option<(u32, &'a str)>;

    fn side(row: &Row) -> (Half<'_>, Half<'_>) {
        let Row::Side { left, right } = row else {
            panic!("not a side row: {row:?}");
        };
        fn f(c: &Option<Cell>) -> Option<(u32, &str)> {
            c.as_ref().map(|c| (c.number, &*c.text))
        }
        (f(left), f(right))
    }

    #[test]
    fn the_widest_line_is_counted_per_pane_with_tabs_expanded() {
        let d = diff(vec![
            common(&["ab"]),
            changed(&["\tx", "a longer old line"], &["new"]),
            common(&["é"]),
        ]);
        // "\tx" is 5 columns with a tab width of 4; "é" is one character.
        let l = build(&d, &opts(DiffViewMode::SideBySide));
        assert_eq!(l.widest(), (17, 3));
        let l = build(&d, &opts(DiffViewMode::Unified));
        assert_eq!(l.widest(), (17, 17));
        assert_eq!(
            build(&diff(Vec::new()), &opts(DiffViewMode::Unified)).widest(),
            (0, 0)
        );
    }

    #[test]
    fn server_skips_keep_the_line_numbers_going() {
        let l = build(&sample(), &opts(DiffViewMode::SideBySide));
        let r = l.rows();
        assert_eq!(r.len(), 2 + 1 + 2 + 1 + 1);
        assert_eq!(side(&r[0]), (Some((1, "a")), Some((1, "a"))));
        assert_eq!(side(&r[1]), (Some((2, "b")), Some((2, "b"))));
        assert_eq!(
            r[2],
            Row::Hidden(HiddenRow {
                lines: 10,
                old_start: 3,
                new_start: 3,
            })
        );
        // The change begins after the 10 hidden lines, at line 13 on both sides.
        assert_eq!(side(&r[3]), (Some((13, "x")), Some((13, "y"))));
        // The shorter side is padded with a blank, the longer one goes on.
        assert_eq!(side(&r[4]), (None, Some((14, "z"))));
        // One old line but two new ones: the numbers have drifted by one.
        assert_eq!(side(&r[5]), (Some((14, "c")), Some((15, "c"))));
        assert_eq!(
            r[6],
            Row::Hidden(HiddenRow {
                lines: 5,
                old_start: 15,
                new_start: 16,
            })
        );
        assert_eq!(l.max_numbers(), (19, 20));
    }

    #[test]
    fn unified_puts_deletions_before_insertions() {
        let l = build(&sample(), &opts(DiffViewMode::Unified));
        let r = l.rows();
        assert_eq!(r.len(), 2 + 1 + 3 + 1 + 1);
        let unified = |row: &Row| match row {
            Row::Unified { old, new, cell } => (*old, *new, cell.text.to_string(), cell.kind),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            unified(&r[0]),
            (Some(1), Some(1), "a".into(), LineKind::Common)
        );
        assert_eq!(
            unified(&r[3]),
            (Some(13), None, "x".into(), LineKind::Removed)
        );
        assert_eq!(
            unified(&r[4]),
            (None, Some(13), "y".into(), LineKind::Added)
        );
        assert_eq!(
            unified(&r[5]),
            (None, Some(14), "z".into(), LineKind::Added)
        );
        assert_eq!(
            unified(&r[6]),
            (Some(14), Some(15), "c".into(), LineKind::Common)
        );
    }

    // --- folding ---------------------------------------------------------

    fn numbered(from: usize, to: usize) -> Vec<String> {
        (from..=to).map(|i| format!("l{i}")).collect()
    }

    fn common_n(from: usize, to: usize) -> DiffChunk {
        DiffChunk::Common {
            lines: numbered(from, to),
        }
    }

    fn fold_opts(mode: DiffViewMode, n: u32) -> LayoutOptions {
        LayoutOptions {
            context: DiffContext::Lines(n),
            ..opts(mode)
        }
    }

    /// Unified rows as `text`, `-text`, `+text` or `~N` for a fold.
    fn shape(l: &Layout) -> Vec<String> {
        l.rows()
            .iter()
            .map(|r| match r {
                Row::Unified { cell, .. } => match cell.kind {
                    LineKind::Common => cell.text.to_string(),
                    LineKind::Removed => format!("-{}", cell.text),
                    LineKind::Added => format!("+{}", cell.text),
                },
                Row::Fold(f) => format!("~{}", f.lines),
                Row::Hidden(h) => format!("?{}", h.lines),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    fn folded(chunks: Vec<DiffChunk>, n: u32) -> Vec<String> {
        shape(&build(&diff(chunks), &fold_opts(DiffViewMode::Unified, n)))
    }

    #[test]
    fn a_run_between_changes_keeps_context_on_both_sides() {
        let d = vec![
            changed(&["x"], &["y"]),
            common_n(1, 30),
            changed(&["p"], &["q"]),
        ];
        assert_eq!(
            folded(d, 3),
            [
                "-x", "+y", "l1", "l2", "l3", "~24", "l28", "l29", "l30", "-p", "+q"
            ]
        );
    }

    #[test]
    fn the_start_keeps_context_only_before_the_first_change() {
        let d = vec![common_n(1, 20), changed(&["x"], &["y"])];
        assert_eq!(folded(d, 3), ["~17", "l18", "l19", "l20", "-x", "+y"]);
    }

    #[test]
    fn the_end_keeps_context_only_after_the_last_change() {
        let d = vec![changed(&["x"], &["y"]), common_n(1, 20)];
        assert_eq!(folded(d, 3), ["-x", "+y", "l1", "l2", "l3", "~17"]);
    }

    #[test]
    fn small_gaps_are_not_folded() {
        let run = |len| {
            vec![
                changed(&["x"], &["y"]),
                common_n(1, len),
                changed(&["p"], &["q"]),
            ]
        };
        // 8 lines with 3 of context on each side would hide 2: shown.
        assert_eq!(folded(run(8), 3).len(), 2 + 8 + 2);
        // 9 lines would hide 3 (MIN_FOLD): folded.
        let rows = folded(run(9), 3);
        assert_eq!(rows.len(), 2 + 3 + 1 + 3 + 2);
        assert!(rows.contains(&"~3".to_owned()));
        // Changes closer than twice the context merge their contexts.
        assert_eq!(folded(run(5), 3).len(), 2 + 5 + 2);
    }

    #[test]
    fn zero_context_folds_everything_unchanged() {
        let d = vec![
            common_n(1, 5),
            changed(&["x"], &["y"]),
            common_n(6, 9),
            changed(&["p"], &["q"]),
        ];
        assert_eq!(folded(d, 0), ["~5", "-x", "+y", "~4", "-p", "+q"]);
    }

    #[test]
    fn whole_file_context_folds_nothing() {
        let d = diff(vec![common_n(1, 50), changed(&["x"], &["y"])]);
        let l = build(&d, &opts(DiffViewMode::Unified));
        assert_eq!(l.len(), 52);
    }

    #[test]
    fn a_file_without_changes_shows_how_it_starts() {
        let d = vec![common_n(1, 20)];
        assert_eq!(folded(d, 3), ["l1", "l2", "l3", "~17"]);
        // And all of it when the context says so, or when it is short.
        assert_eq!(folded(vec![common_n(1, 5)], 3).len(), 5);
    }

    #[test]
    fn fold_rows_know_their_lines_and_edges() {
        let d = diff(vec![
            common_n(1, 20),
            changed(&["x"], &["y", "z"]),
            common_n(21, 60),
            changed(&["p"], &["q"]),
            common_n(61, 80),
        ]);
        let l = build(&d, &fold_opts(DiffViewMode::SideBySide, 3));
        let folds: Vec<_> = l
            .rows()
            .iter()
            .filter_map(|r| match r {
                Row::Fold(f) => Some(f.clone()),
                _ => None,
            })
            .collect();
        let want = |lines, old_start, new_start, run, above, below| FoldRow {
            lines,
            old_start,
            new_start,
            run,
            above,
            below,
        };
        assert_eq!(
            folds,
            [
                want(17, 1, 1, 1, false, true),
                // One old line became two: the new side is one ahead.
                want(34, 25, 26, 22, true, true),
                want(17, 66, 67, 63, true, false),
            ]
        );
        // The numbers continue correctly after a fold.
        let Row::Side { left, right } = &l.rows()[1] else {
            panic!()
        };
        assert_eq!(left.as_ref().map(|c| c.number), Some(18));
        assert_eq!(right.as_ref().map(|c| c.number), Some(18));
        assert_eq!(l.max_numbers(), (82, 83));
    }

    #[test]
    fn whitespace_only_chunks_count_as_unchanged() {
        let ws = DiffChunk::Changed(ChangedChunk {
            a: lines(&["a  b"]),
            b: lines(&["a b"]),
            common: true,
            ..Default::default()
        });
        let d = vec![
            common_n(1, 10),
            ws,
            common_n(12, 21),
            changed(&["x"], &["y"]),
        ];
        let l = build(&diff(d), &fold_opts(DiffViewMode::Unified, 2));
        // 21 unchanged lines in one run, folded as one: 19 hidden.
        assert_eq!(shape(&l), ["~19", "l20", "l21", "-x", "+y"]);
        assert_eq!(l.chunks().len(), 1);
    }

    #[test]
    fn server_skips_stay_hidden_and_stop_folding_next_to_them() {
        let d = vec![
            common_n(1, 30),
            DiffChunk::Skip { lines: 40 },
            common_n(71, 100),
            changed(&["x"], &["y"]),
        ];
        let rows = folded(d, 3);
        assert_eq!(rows.iter().filter(|r| r.starts_with('?')).count(), 1);
        assert!(rows.contains(&"?40".to_owned()));
        // The run before the skip is shown in full; the one after it is
        // next to a skip as well.
        assert_eq!(rows.len(), 30 + 1 + 30 + 2);
        let l = build(
            &diff(vec![DiffChunk::Skip { lines: 4 }]),
            &opts(DiffViewMode::Unified),
        );
        assert_eq!(l.first_selectable(), None);
    }

    fn big() -> Diff {
        diff(vec![
            changed(&["x"], &["y"]),
            common_n(1, 100),
            changed(&["p"], &["q"]),
        ])
    }

    fn with(ex: &Expansion, mode: DiffViewMode) -> Layout {
        Layout::build(&big(), &fold_opts(mode, 3), ex)
    }

    #[test]
    fn partial_reveals_shrink_the_fold_from_either_edge() {
        let mut ex = Expansion::default();
        let first = |l: &Layout| {
            l.rows()
                .iter()
                .find_map(|r| match r {
                    Row::Fold(f) => Some(f.clone()),
                    _ => None,
                })
                .expect("a fold")
        };
        let l = with(&ex, DiffViewMode::Unified);
        assert_eq!(first(&l).lines, 94);

        ex.reveal_top(2, 10);
        let f = first(&with(&ex, DiffViewMode::Unified));
        assert_eq!((f.lines, f.old_start), (84, 15));

        ex.reveal_bottom(2, 10);
        let l = with(&ex, DiffViewMode::Unified);
        assert_eq!(first(&l).lines, 74);
        // 3 + 10 lines above, 10 + 3 below.
        assert_eq!(l.len(), 2 + 13 + 1 + 13 + 2);

        // The side-by-side layout folds the same lines.
        let sbs = with(&ex, DiffViewMode::SideBySide);
        assert!(
            sbs.rows()
                .iter()
                .any(|r| matches!(r, Row::Fold(f) if f.lines == 74))
        );
    }

    #[test]
    fn revealing_nearly_everything_shows_the_rest_instead_of_a_tiny_fold() {
        let mut ex = Expansion::default();
        ex.reveal_top(2, 89); // 94 - 89 = 5 left: still folded
        assert!(
            with(&ex, DiffViewMode::Unified)
                .rows()
                .iter()
                .any(|r| matches!(r, Row::Fold(f) if f.lines == 5))
        );
        ex.reveal_top(2, 4); // 1 left: shown
        let l = with(&ex, DiffViewMode::Unified);
        assert!(!l.rows().iter().any(|r| matches!(r, Row::Fold(_))));
        assert_eq!(l.len(), 2 + 100 + 2);
    }

    #[test]
    fn revealing_a_run_or_everything_removes_the_folds() {
        let mut ex = Expansion::default();
        ex.reveal_run(2);
        assert_eq!(with(&ex, DiffViewMode::Unified).len(), 2 + 100 + 2);

        let mut all = Expansion::default();
        all.reveal_everything();
        assert!(all.is_everything());
        assert_eq!(with(&all, DiffViewMode::SideBySide).len(), 100 + 2);
        all.clear();
        assert!(!all.is_everything());
        assert!(
            with(&all, DiffViewMode::SideBySide)
                .rows()
                .iter()
                .any(|r| matches!(r, Row::Fold(_)))
        );
    }

    #[test]
    fn reveals_apply_on_top_of_any_context() {
        let mut ex = Expansion::default();
        ex.reveal_top(2, 10);
        let l = Layout::build(&big(), &fold_opts(DiffViewMode::Unified, 10), &ex);
        // 10 + 10 above, 10 below, 70 hidden.
        assert!(
            l.rows()
                .iter()
                .any(|r| matches!(r, Row::Fold(f) if f.lines == 70))
        );
    }

    #[test]
    fn anchors_find_folded_lines_at_the_fold_and_revealed_ones_directly() {
        let ex = Expansion::default();
        let l = with(&ex, DiffViewMode::Unified);
        let anchor = Anchor {
            old: Some(50),
            new: Some(50),
        };
        let at = l.find(anchor).expect("fold");
        assert!(matches!(l.rows()[at], Row::Fold(_)));
        let mut all = Expansion::default();
        all.reveal_everything();
        let l = with(&all, DiffViewMode::Unified);
        let at = l.find(anchor).expect("line");
        assert_eq!(l.anchor(at), anchor);
    }

    #[test]
    fn chunk_boundaries_and_navigation() {
        let d = diff(vec![
            common(&["a"]),
            changed(&["1", "2"], &["3"]),
            common(&["b"]),
            changed(&[], &["new"]),
            changed(&["gone"], &[]),
            common(&["c"]),
        ]);
        let l = build(&d, &opts(DiffViewMode::SideBySide));
        // rows: a | 1/3, 2/- | b | -/new | gone/- | c
        assert_eq!(l.len(), 7);
        assert_eq!(l.chunk_starts(), [1, 4, 5]);
        assert_eq!(l.chunks(), [1..3, 4..5, 5..6]);
        assert_eq!(l.next_chunk(0), Some(1));
        assert_eq!(l.next_chunk(1), Some(4));
        assert_eq!(l.next_chunk(2), Some(4));
        assert_eq!(l.next_chunk(5), None);
        assert_eq!(l.prev_chunk(0), None);
        assert_eq!(l.prev_chunk(1), None);
        // From inside a chunk, `p` goes to the one before, not its own start.
        assert_eq!(l.prev_chunk(2), None);
        assert_eq!(l.prev_chunk(3), Some(1));
        assert_eq!(l.prev_chunk(5), Some(4));
        assert_eq!(l.prev_chunk(6), Some(5));

        let u = build(&d, &opts(DiffViewMode::Unified));
        assert_eq!(u.chunk_starts(), [1, 5, 6]);
    }

    #[test]
    fn rebase_chunks_are_marked() {
        let d = diff(vec![DiffChunk::Changed(ChangedChunk {
            a: lines(&["x"]),
            b: lines(&["y"]),
            due_to_rebase: true,
            ..Default::default()
        })]);
        let l = build(&d, &opts(DiffViewMode::SideBySide));
        let Row::Side { left, right } = &l.rows()[0] else {
            panic!()
        };
        assert!(left.as_ref().is_some_and(|c| c.due_to_rebase));
        assert!(right.as_ref().is_some_and(|c| c.due_to_rebase));
    }

    #[test]
    fn whitespace_only_chunks_are_plain_lines() {
        let d = diff(vec![DiffChunk::Changed(ChangedChunk {
            a: lines(&["a  b"]),
            b: lines(&["a b"]),
            common: true,
            ..Default::default()
        })]);
        let l = build(&d, &opts(DiffViewMode::SideBySide));
        let Row::Side { left, right } = &l.rows()[0] else {
            panic!()
        };
        assert_eq!(
            left.as_ref().map(|c| (&*c.text, c.kind)),
            Some(("a  b", LineKind::Common))
        );
        assert_eq!(
            right.as_ref().map(|c| (&*c.text, c.kind)),
            Some(("a b", LineKind::Common))
        );
        assert!(l.chunks().is_empty());
    }

    #[test]
    fn spans_follow_tab_expansion() {
        let spans = [(1..4, 'a'), (5..8, 'b')];
        // "\tfoo bar": the tab becomes four spaces.
        assert_eq!(
            map_spans("\tfoo bar", &spans, 4, false),
            [(4..7, 'a'), (8..11, 'b')]
        );
        // A line without tabs is unchanged.
        assert_eq!(map_spans("xfoo bar", &spans, 4, false), spans);
        // Spans in the cut-off tail of a long line are dropped.
        let long = format!("\t{}", "x".repeat(MAX_LINE_BYTES + 10));
        let tail = [(1..3, 'a'), (MAX_LINE_BYTES + 5..MAX_LINE_BYTES + 8, 'b')];
        assert_eq!(map_spans(&long, &tail, 4, false), [(4..6, 'a')]);
    }

    #[test]
    fn intraline_marks_follow_tab_expansion() {
        // "\tfoo bar" with "bar" marked: bytes 5..8.
        let e = expand_line("\tfoo bar", &[5..8], 4, false);
        assert_eq!(&*e.text, "    foo bar");
        assert_eq!(e.marks, [8..11]);
        assert_eq!(&e.text[8..11], "bar");
        assert!(e.tabs.is_empty());

        // With visible tabs the marker is 3 bytes wide.
        let e = expand_line("\tfoo bar", &[5..8], 4, true);
        assert_eq!(&*e.text, "\u{2192}   foo bar");
        assert_eq!(&e.text[e.marks[0].clone()], "bar");
        assert_eq!(e.tabs, [0..3]);

        // A mark that covers the tab covers all of its cells.
        let e = expand_line("a\tb", &[1..3], 4, false);
        assert_eq!(&*e.text, "a   b");
        assert_eq!(&e.text[e.marks[0].clone()], "   b");
    }

    #[test]
    fn tabs_align_to_the_next_stop() {
        for (line, want) in [
            ("a\tb", "a   b"),
            ("abc\td", "abc d"),
            ("abcd\te", "abcd    e"),
            ("\t\tx", "        x"),
        ] {
            assert_eq!(&*expand_line(line, &[], 4, false).text, want, "{line:?}");
        }
        // A tab size of 0 must not divide by zero.
        assert_eq!(&*expand_line("\tx", &[], 0, false).text, " x");
    }

    #[test]
    fn marks_survive_multibyte_text() {
        let e = expand_line("é\tÄ\u{1F600}x", &[3..5, 5..10], 4, false);
        assert_eq!(&*e.text, "é   Ä\u{1F600}x");
        assert_eq!(e.marks.len(), 2);
        assert_eq!(&e.text[e.marks[0].clone()], "Ä");
        assert_eq!(&e.text[e.marks[1].clone()], "\u{1F600}x");
    }

    #[test]
    fn long_lines_are_cut() {
        let long = "x".repeat(MAX_LINE_BYTES * 3);
        let e = expand_line(
            &long,
            &[10..20, MAX_LINE_BYTES * 2..MAX_LINE_BYTES * 2 + 5],
            4,
            false,
        );
        assert!(e.text.len() <= MAX_LINE_BYTES + '\u{2026}'.len_utf8());
        assert!(e.text.ends_with('\u{2026}'));
        assert_eq!(e.marks[0], 10..20);
        // The mark that was cut away is empty and dropped.
        assert_eq!(e.marks.len(), 1);
    }

    #[test]
    fn intraline_edits_become_cell_marks() {
        // b = "xbar", the second line "foo"; edits cover "bar" (offset 1..4).
        let d = diff(vec![DiffChunk::Changed(ChangedChunk {
            a: lines(&["foo"]),
            b: lines(&["\tfoo"]),
            edits_b: vec![IntralineEdit { skip: 1, mark: 3 }],
            ..Default::default()
        })]);
        let l = build(&d, &opts(DiffViewMode::SideBySide));
        let Row::Side { right: Some(c), .. } = &l.rows()[0] else {
            panic!()
        };
        assert_eq!(&*c.text, "    foo");
        assert_eq!(c.marks, [4..7]);

        let off = LayoutOptions {
            intraline: false,
            ..opts(DiffViewMode::SideBySide)
        };
        let l = build(&d, &off);
        let Row::Side { right: Some(c), .. } = &l.rows()[0] else {
            panic!()
        };
        assert!(c.marks.is_empty());
    }

    #[test]
    fn binary_empty_and_rename_notices() {
        let mut d = diff(Vec::new());
        d.binary = true;
        let l = build(&d, &opts(DiffViewMode::SideBySide));
        assert_eq!(l.rows(), [Row::Notice(Notice::Binary)]);
        assert_eq!(l.first_selectable(), None);
        assert!(l.chunks().is_empty());

        let l = build(&diff(Vec::new()), &opts(DiffViewMode::Unified));
        assert_eq!(l.rows(), [Row::Notice(Notice::Empty)]);

        let meta = |name: &str| {
            Some(DiffFileMeta {
                name: name.into(),
                content_type: None,
                lines: 1,
            })
        };
        let mut d = diff(vec![common(&["a"])]);
        d.change_type = Some(DiffChangeType::Renamed);
        d.meta_a = meta("old.rs");
        d.meta_b = meta("new.rs");
        let l = build(&d, &opts(DiffViewMode::SideBySide));
        assert_eq!(
            l.rows()[0],
            Row::Notice(Notice::RenamedFrom("old.rs".into()))
        );
        assert_eq!(l.first_selectable(), Some(1));
        // Same name on both sides: no notice.
        d.meta_b = meta("old.rs");
        assert_eq!(build(&d, &opts(DiffViewMode::SideBySide)).len(), 1);
    }

    #[test]
    fn anchors_map_between_modes() {
        let side = build(&sample(), &opts(DiffViewMode::SideBySide));
        let uni = build(&sample(), &opts(DiffViewMode::Unified));
        // Every side-by-side row finds a unified row showing one of its lines.
        for i in 0..side.len() {
            let a = side.anchor(i);
            let j = uni.find(a).unwrap_or_else(|| panic!("row {i} {a:?}"));
            let back = uni.anchor(j);
            assert!(
                back.old == a.old || back.new == a.new,
                "row {i} {a:?} -> {j} {back:?}"
            );
        }
        // A deleted line stays on the deleted line.
        let del = side
            .find(Anchor {
                old: Some(13),
                new: None,
            })
            .expect("old line");
        assert_eq!(side.anchor(del).old, Some(13));
        // A line the server left out is found at its hidden row.
        assert_eq!(
            side.find(Anchor {
                old: Some(7),
                new: Some(7)
            }),
            Some(2)
        );
        assert_eq!(
            side.find(Anchor {
                old: Some(900),
                new: Some(900)
            }),
            None
        );
    }

    // --- comment threads -------------------------------------------------

    use model::CommentRange;

    fn slot(side: DiffSide, line: u32) -> ThreadSlot {
        ThreadSlot {
            side,
            location: Location::Line(line),
        }
    }

    fn with_threads(d: &Diff, o: &LayoutOptions, slots: &[ThreadSlot]) -> Layout {
        Layout::build_with_threads(d, o, &Expansion::default(), slots)
    }

    /// Every row as text: `l/r` numbers side by side (`-` for a blank),
    /// `o/n` unified, `~N` a fold, `?N` hidden, `#N` the notice, `T<slot>`
    /// (with `!` when detached) a thread.
    fn describe(l: &Layout) -> Vec<String> {
        let num = |n: Option<u32>| n.map_or("-".to_owned(), |n| n.to_string());
        l.rows()
            .iter()
            .map(|r| match r {
                Row::Side { left, right } => format!(
                    "{}|{}",
                    num(left.as_ref().map(|c| c.number)),
                    num(right.as_ref().map(|c| c.number))
                ),
                Row::Unified { old, new, .. } => format!("{}/{}", num(*old), num(*new)),
                Row::Fold(f) => format!("~{}", f.lines),
                Row::Hidden(h) => format!("?{}", h.lines),
                Row::Notice(_) => "#".to_owned(),
                Row::Thread(t) => format!("T{}{}", t.slot, if t.detached { "!" } else { "" }),
            })
            .collect()
    }

    /// 5 common lines, a 2:1 change (old 6-7, new 6), 3 common lines.
    fn threaded() -> Diff {
        diff(vec![
            common_n(1, 5),
            changed(&["x", "w"], &["y"]),
            common_n(8, 10),
        ])
    }

    #[test]
    fn threads_follow_their_line_side_by_side() {
        let slots = [
            slot(DiffSide::Right, 6),
            slot(DiffSide::Left, 7),
            slot(DiffSide::Left, 6),
            slot(DiffSide::Right, 8),
        ];
        let l = with_threads(&threaded(), &opts(DiffViewMode::SideBySide), &slots);
        assert_eq!(
            describe(&l),
            [
                "1|1", "2|2", "3|3", "4|4", "5|5", "6|6",
                // Left threads first, then right ones, in slot order.
                "T2", "T0", "7|-", "T1", "8|7", "9|8", "T3", "10|9"
            ]
        );
        assert_eq!(l.thread_rows(), [6, 7, 9, 12]);
        assert_eq!(l.next_thread(0), Some(6));
        assert_eq!(l.next_thread(6), Some(7));
        assert_eq!(l.next_thread(12), None);
        assert_eq!(l.prev_thread(6), None);
        assert_eq!(l.prev_thread(9), Some(7));
        // A thread row stands for the code row above it.
        assert_eq!(l.anchor(7), l.anchor(5));
        assert_eq!(l.anchor(9).old, Some(7));
    }

    #[test]
    fn threads_follow_their_line_unified() {
        let slots = [
            slot(DiffSide::Right, 3),
            slot(DiffSide::Left, 3),
            slot(DiffSide::Left, 7),
            slot(DiffSide::Right, 6),
        ];
        let l = with_threads(&threaded(), &opts(DiffViewMode::Unified), &slots);
        assert_eq!(
            describe(&l),
            [
                "1/1", "2/2", "3/3", "T1", "T0", "4/4", "5/5", "6/-", "7/-", "T2", "-/6", "T3",
                "8/7", "9/8", "10/9"
            ]
        );
    }

    #[test]
    fn threads_on_skipped_lines_follow_the_hidden_row() {
        let d = diff(vec![
            common(&["a", "b"]),
            DiffChunk::Skip { lines: 10 },
            changed(&["x"], &["y"]),
        ]);
        let slots = [slot(DiffSide::Right, 5), slot(DiffSide::Left, 4)];
        for mode in [DiffViewMode::SideBySide, DiffViewMode::Unified] {
            let l = with_threads(&d, &opts(mode), &slots);
            let rows = describe(&l);
            assert_eq!(rows[2], "?10", "{mode:?}");
            assert_eq!(rows[3..5], ["T0", "T1"], "{mode:?}");
            assert_eq!(l.thread_rows().len(), 2);
        }
    }

    #[test]
    fn file_level_and_detached_threads_come_first_after_the_notices() {
        let meta = |name: &str| {
            Some(DiffFileMeta {
                name: name.into(),
                content_type: None,
                lines: 1,
            })
        };
        let mut d = diff(vec![common(&["a"]), changed(&["b"], &["c"])]);
        d.change_type = Some(DiffChangeType::Renamed);
        d.meta_a = meta("old.rs");
        d.meta_b = meta("new.rs");
        let slots = [
            slot(DiffSide::Right, 2),
            ThreadSlot {
                side: DiffSide::Right,
                location: Location::File,
            },
            slot(DiffSide::Right, 99),
            slot(DiffSide::Left, 0),
        ];
        let l = with_threads(&d, &opts(DiffViewMode::SideBySide), &slots);
        assert_eq!(describe(&l), ["#", "T1", "T2!", "T3!", "1|1", "2|2", "T0"]);
        let rows: Vec<_> = l
            .rows()
            .iter()
            .filter_map(|r| match r {
                Row::Thread(t) => Some((t.slot, t.detached)),
                _ => None,
            })
            .collect();
        assert_eq!(rows, [(1, false), (2, true), (3, true), (0, false)]);
        // The initial cursor skips them all.
        assert_eq!(l.first_selectable(), Some(4));
        assert_eq!(l.last_selectable(), Some(5));
        // A thread row at the very top has no code row above it.
        assert_eq!(l.anchor(1), Anchor::default());

        // A binary file still shows its file comments.
        let mut b = diff(Vec::new());
        b.binary = true;
        let l = with_threads(&b, &opts(DiffViewMode::SideBySide), &slots[1..2]);
        assert_eq!(describe(&l), ["#", "T0"]);
        assert_eq!(l.first_selectable(), None);
    }

    fn big_run() -> Diff {
        diff(vec![
            changed(&["x"], &["y"]),
            common_n(1, 100),
            changed(&["p"], &["q"]),
        ])
    }

    #[test]
    fn a_commented_line_is_never_folded_and_splits_its_run() {
        // The run starts at line 2 on both sides, so `l50` is line 51.
        let slots = [slot(DiffSide::Right, 51)];
        let o = fold_opts(DiffViewMode::Unified, 3);
        let l = with_threads(&big_run(), &o, &slots);
        let d = describe(&l);
        let at = d.iter().position(|r| r == "T0").expect("thread");
        assert_eq!(d[at - 1], "51/51");
        // Context of 3 on both sides of the commented line.
        assert_eq!(
            d,
            [
                "1/-", "-/1", "2/2", "3/3", "4/4", "~43", "48/48", "49/49", "50/50", "51/51", "T0",
                "52/52", "53/53", "54/54", "~44", "99/99", "100/100", "101/101", "102/-", "-/102"
            ]
        );
        let folds: Vec<_> = l
            .rows()
            .iter()
            .filter_map(|r| match r {
                Row::Fold(f) => Some((f.lines, f.run, f.above, f.below)),
                _ => None,
            })
            .collect();
        // Both pieces border on a change on both edges.
        assert_eq!(folds, [(43, 2, true, true), (44, 52, true, true)]);

        // Without the context pref nothing else is shown around it.
        let none = with_threads(&big_run(), &fold_opts(DiffViewMode::Unified, 0), &slots);
        let d = describe(&none);
        assert_eq!(
            d.iter().filter(|r| r.contains('/')).count(),
            2 + 1 + 2,
            "{d:?}"
        );
    }

    #[test]
    fn reveals_apply_to_one_piece_of_a_split_run() {
        let slots = [slot(DiffSide::Right, 51)];
        let o = fold_opts(DiffViewMode::SideBySide, 3);
        let mut ex = Expansion::default();
        ex.reveal_run(52); // the piece after the commented line
        let l = Layout::build_with_threads(&big_run(), &o, &ex, &slots);
        let folds: Vec<u32> = l
            .rows()
            .iter()
            .filter_map(|r| match r {
                Row::Fold(f) => Some(f.lines),
                _ => None,
            })
            .collect();
        assert_eq!(folds, [43]);
        // The first piece keeps its key, the run's old start.
        let mut ex = Expansion::default();
        ex.reveal_top(2, 10);
        let l = Layout::build_with_threads(&big_run(), &o, &ex, &slots);
        assert!(
            l.rows()
                .iter()
                .any(|r| matches!(r, Row::Fold(f) if f.lines == 33 && f.run == 2))
        );
    }

    #[test]
    fn a_range_keeps_its_end_lines_unfolded_and_is_capped() {
        let range = |start_line, end_line| ThreadSlot {
            side: DiffSide::Right,
            location: Location::Range(CommentRange {
                start_line,
                start_character: 0,
                end_line,
                end_character: 1,
            }),
        };
        let o = fold_opts(DiffViewMode::Unified, 0);
        // Lines 20 to 24: all five are shown, with the thread after 24.
        let l = with_threads(&big_run(), &o, &[range(20, 24)]);
        let shown = describe(&l)
            .into_iter()
            .filter(|r| r.contains('/'))
            .collect::<Vec<_>>();
        assert_eq!(shown.len(), 2 + 5 + 2, "{shown:?}");
        // A huge range does not unfold the file: the first line and the
        // last MAX_RANGE_LINES stay, the rest is folded.
        let d = diff(vec![
            changed(&["x"], &["y"]),
            common_n(1, 1000),
            changed(&["p"], &["q"]),
        ]);
        let l = with_threads(&d, &o, &[range(10, 900)]);
        let shown = l.rows().iter().filter(|r| r.new_number().is_some()).count();
        // 2 changes (+ the first fold row counts as it has a number).
        assert!(shown < 2 + 1 + 1 + MAX_RANGE_LINES as usize + 5, "{shown}");
        assert!(l.rows().iter().any(|r| matches!(r, Row::Fold(_))));
    }

    #[test]
    fn range_threads_mark_their_characters() {
        let d = diff(vec![common(&[
            "zero", "abcdef", "\tghij", "klmnop", "tail",
        ])]);
        let slots = [ThreadSlot {
            side: DiffSide::Right,
            location: Location::Range(CommentRange {
                start_line: 2,
                start_character: 2,
                end_line: 4,
                end_character: 3,
            }),
        }];
        let l = with_threads(&d, &opts(DiffViewMode::Unified), &slots);
        let marks = |ix: usize| match &l.rows()[ix] {
            Row::Unified { cell, .. } => (cell.text.to_string(), cell.comments.clone()),
            other => panic!("{other:?}"),
        };
        assert_eq!(marks(0).1, Vec::<Range<usize>>::new());
        // From character 2 to the end of the line.
        assert_eq!(marks(1), ("abcdef".to_owned(), vec![2..6]));
        // The whole line, with its tab expanded to four spaces.
        let (text, m) = marks(2);
        assert_eq!(text, "    ghij");
        assert_eq!(m, [0..8]);
        // The last line up to its third character.
        assert_eq!(marks(3), ("klmnop".to_owned(), vec![0..3]));
        // The thread sits after the end line.
        assert_eq!(describe(&l)[4], "T0");
        assert_eq!(marks(5).1, Vec::<Range<usize>>::new());
    }

    #[test]
    fn range_marks_work_on_the_left_and_in_one_line() {
        let d = diff(vec![changed(&["héllo wörld"], &["x"])]);
        let slots = [ThreadSlot {
            side: DiffSide::Left,
            location: Location::Range(CommentRange {
                start_line: 1,
                start_character: 6,
                end_line: 1,
                end_character: 11,
            }),
        }];
        let l = with_threads(&d, &opts(DiffViewMode::SideBySide), &slots);
        let Row::Side {
            left: Some(left),
            right: Some(right),
        } = &l.rows()[0]
        else {
            panic!()
        };
        assert_eq!(&left.text[left.comments[0].clone()], "wörld");
        assert!(right.comments.is_empty());
        // A range that selects nothing marks nothing.
        let empty = [ThreadSlot {
            side: DiffSide::Left,
            location: Location::Range(CommentRange {
                start_line: 1,
                start_character: 3,
                end_line: 1,
                end_character: 3,
            }),
        }];
        let l = with_threads(&d, &opts(DiffViewMode::SideBySide), &empty);
        let Row::Side {
            left: Some(left), ..
        } = &l.rows()[0]
        else {
            panic!()
        };
        assert!(left.comments.is_empty());
    }

    #[test]
    fn every_thread_is_placed_exactly_once() {
        let d = diff(vec![
            common_n(1, 40),
            DiffChunk::Skip { lines: 10 },
            changed(&["x"], &["y", "z"]),
            common_n(1, 30),
        ]);
        let mut slots = Vec::new();
        for side in [DiffSide::Left, DiffSide::Right] {
            for line in [0, 1, 20, 40, 41, 50, 51, 52, 53, 54, 80, 81, 82, 500] {
                slots.push(slot(side, line));
            }
            slots.push(ThreadSlot {
                side,
                location: Location::File,
            });
        }
        for mode in [DiffViewMode::SideBySide, DiffViewMode::Unified] {
            for context in [
                DiffContext::All,
                DiffContext::Lines(0),
                DiffContext::Lines(3),
            ] {
                let o = LayoutOptions {
                    context,
                    ..opts(mode)
                };
                let l = with_threads(&d, &o, &slots);
                let mut seen: Vec<usize> = l
                    .rows()
                    .iter()
                    .filter_map(|r| match r {
                        Row::Thread(t) => Some(t.slot),
                        _ => None,
                    })
                    .collect();
                seen.sort_unstable();
                assert_eq!(
                    seen,
                    (0..slots.len()).collect::<Vec<_>>(),
                    "{mode:?} {context:?}"
                );
                assert_eq!(l.thread_rows().len(), slots.len());
            }
        }
    }

    #[test]
    fn no_threads_is_the_plain_layout() {
        let o = fold_opts(DiffViewMode::SideBySide, 3);
        let a = Layout::build(&big_run(), &o, &Expansion::default());
        let b = with_threads(&big_run(), &o, &[]);
        assert_eq!(a.rows(), b.rows());
        assert!(b.thread_rows().is_empty());
    }

    #[test]
    fn cells_know_their_urls_in_both_modes() {
        let url = "https://example.org/a/very/long/path?q=1&r=2#frag";
        let line = format!("\t// see {url}.");
        let long = format!("{url}{}", "x".repeat(500));
        let d = diff(vec![common(&[&line]), changed(&["no link"], &[&long])]);
        for mode in [DiffViewMode::SideBySide, DiffViewMode::Unified] {
            let l = build(&d, &opts(mode));
            let cells: Vec<&Cell> = l
                .rows()
                .iter()
                .flat_map(|r| match r {
                    Row::Side { left, right } => vec![left.as_ref(), right.as_ref()],
                    Row::Unified { cell, .. } => vec![Some(cell)],
                    _ => vec![],
                })
                .flatten()
                .collect();
            let with: Vec<Vec<&str>> = cells
                .iter()
                .filter(|c| !c.urls.is_empty())
                .map(|c| c.urls.iter().map(|r| &c.text[r.clone()]).collect())
                .collect();
            // The tab is expanded before the ranges are taken, and a long
            // line is not split.
            assert!(with.contains(&vec![url]), "{mode:?}: {with:?}");
            assert!(with.contains(&vec![long.as_str()]), "{mode:?}: {with:?}");
            assert!(
                cells
                    .iter()
                    .any(|c| &*c.text == "no link" && c.urls.is_empty())
            );
        }
    }
}
