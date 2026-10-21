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

use std::{ops::Range, sync::Arc};

use model::{Diff, DiffChangeType, DiffChunk, DiffViewMode, ResolvedDiffPreferences};

/// Lines longer than this are cut (and end in `…`). Shaping minified
/// one-liners of hundreds of kilobytes would stall the frame.
pub const MAX_LINE_BYTES: usize = 4096;

/// What the layout is built from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutOptions {
    pub mode: DiffViewMode,
    pub tab_size: u32,
    /// Show a marker (`→`) in the first cell of every tab.
    pub show_tabs: bool,
    /// Use the server's intraline edits.
    pub intraline: bool,
}

impl LayoutOptions {
    pub fn from_prefs(prefs: &ResolvedDiffPreferences) -> Self {
        Self {
            mode: prefs.view_mode,
            tab_size: prefs.tab_size,
            show_tabs: prefs.show_tabs,
            intraline: prefs.intraline_difference,
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
}

/// A run of unchanged lines the server did not send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkipRow {
    pub lines: u32,
    /// First hidden line number on the old and new side (1-based).
    pub old_start: u32,
    pub new_start: u32,
    /// Nothing but hidden lines precedes / follows it in the diff.
    pub at_start: bool,
    pub at_end: bool,
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
    Skip(SkipRow),
    Notice(Notice),
}

impl Row {
    pub fn old_number(&self) -> Option<u32> {
        match self {
            Self::Side { left, .. } => left.as_ref().map(|c| c.number),
            Self::Unified { old, .. } => *old,
            Self::Skip(s) => Some(s.old_start),
            Self::Notice(_) => None,
        }
    }

    pub fn new_number(&self) -> Option<u32> {
        match self {
            Self::Side { right, .. } => right.as_ref().map(|c| c.number),
            Self::Unified { new, .. } => *new,
            Self::Skip(s) => Some(s.new_start),
            Self::Notice(_) => None,
        }
    }

    /// Whether the line cursor may rest on this row.
    pub fn selectable(&self) -> bool {
        !matches!(self, Self::Notice(_))
    }

    fn covers(&self, side: Side, n: u32) -> bool {
        match (self, side) {
            (Self::Skip(s), Side::Old) => (s.old_start..s.old_start + s.lines).contains(&n),
            (Self::Skip(s), Side::New) => (s.new_start..s.new_start + s.lines).contains(&n),
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
    /// Highest line number on the old / new side, for the gutter width.
    max_old: u32,
    max_new: u32,
}

impl Layout {
    pub fn build(diff: &Diff, opts: &LayoutOptions) -> Self {
        let mut b = Builder {
            opts: *opts,
            rows: Vec::new(),
            chunks: Vec::new(),
        };
        let (mut old, mut new) = (1_u32, 1_u32);
        if diff.binary {
            b.rows.push(Row::Notice(Notice::Binary));
        } else {
            b.rename_notice(diff);
            if diff.chunks.is_empty() {
                b.rows.push(Row::Notice(Notice::Empty));
            }
            let last = diff.chunks.len().saturating_sub(1);
            for (i, chunk) in diff.chunks.iter().enumerate() {
                match chunk {
                    DiffChunk::Common { lines } => {
                        for line in lines {
                            b.common(old, new, line, line);
                            old += 1;
                            new += 1;
                        }
                    }
                    DiffChunk::Changed(c) => {
                        b.changed(c, old, new);
                        old += len32(&c.a);
                        new += len32(&c.b);
                    }
                    DiffChunk::Skip { lines } => {
                        b.rows.push(Row::Skip(SkipRow {
                            lines: *lines,
                            old_start: old,
                            new_start: new,
                            at_start: i == 0,
                            at_end: i == last,
                        }));
                        old += lines;
                        new += lines;
                    }
                }
            }
        }
        Self {
            mode: opts.mode,
            rows: b.rows,
            chunks: b.chunks,
            max_old: old - 1,
            max_new: new - 1,
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

    pub fn first_selectable(&self) -> Option<usize> {
        self.rows.iter().position(Row::selectable)
    }

    pub fn last_selectable(&self) -> Option<usize> {
        self.rows.iter().rposition(Row::selectable)
    }

    pub fn anchor(&self, row: usize) -> Anchor {
        self.rows.get(row).map_or_else(Anchor::default, |r| Anchor {
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

struct Builder {
    opts: LayoutOptions,
    rows: Vec<Row>,
    chunks: Vec<Range<usize>>,
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

    fn cell(&self, number: u32, line: &str, marks: &[Range<usize>], kind: LineKind) -> Cell {
        let e = expand_line(line, marks, self.opts.tab_size, self.opts.show_tabs);
        Cell {
            number,
            text: e.text,
            marks: e.marks,
            tabs: e.tabs,
            kind,
            due_to_rebase: false,
        }
    }

    /// A line on both sides. `a` and `b` differ only for whitespace-only
    /// changes; otherwise they are the same text.
    fn common(&mut self, old: u32, new: u32, a: &str, b: &str) {
        let right = self.cell(new, b, &[], LineKind::Common);
        match self.opts.mode {
            DiffViewMode::SideBySide => {
                let left = if a == b {
                    Cell {
                        number: old,
                        ..right.clone()
                    }
                } else {
                    self.cell(old, a, &[], LineKind::Common)
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
    }

    fn changed(&mut self, c: &model::ChangedChunk, old: u32, new: u32) {
        if c.common && c.a.len() == c.b.len() {
            for (i, (a, b)) in (0_u32..).zip(c.a.iter().zip(&c.b)) {
                self.common(old + i, new + i, a, b);
            }
            return;
        }
        let empty = |n: usize| vec![Vec::new(); n];
        let (marks_a, marks_b) = if self.opts.intraline {
            (c.marks_a(), c.marks_b())
        } else {
            (empty(c.a.len()), empty(c.b.len()))
        };
        let cells = |this: &Self, lines: &[String], marks: &[Vec<Range<usize>>], start, kind| {
            (start..)
                .zip(lines.iter().zip(marks))
                .map(|(n, (line, m))| {
                    let mut cell = this.cell(n, line, m, kind);
                    cell.due_to_rebase = c.due_to_rebase;
                    cell
                })
                .collect::<Vec<_>>()
        };
        let a = cells(self, &c.a, &marks_a, old, LineKind::Removed);
        let b = cells(self, &c.b, &marks_b, new, LineKind::Added);

        let first = self.rows.len();
        match self.opts.mode {
            DiffViewMode::SideBySide => {
                let (mut a, mut b) = (a.into_iter(), b.into_iter());
                loop {
                    let (left, right) = (a.next(), b.next());
                    if left.is_none() && right.is_none() {
                        break;
                    }
                    self.rows.push(Row::Side { left, right });
                }
            }
            DiffViewMode::Unified => {
                for cell in a {
                    self.rows.push(Row::Unified {
                        old: Some(cell.number),
                        new: None,
                        cell,
                    });
                }
                for cell in b {
                    self.rows.push(Row::Unified {
                        old: None,
                        new: Some(cell.number),
                        cell,
                    });
                }
            }
        }
        if self.rows.len() > first {
            self.chunks.push(first..self.rows.len());
        }
    }
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
    if line.len() <= MAX_LINE_BYTES && !line.contains('\t') {
        return Expanded {
            text: Arc::from(line),
            marks: marks.to_vec(),
            tabs: Vec::new(),
        };
    }
    let tab = tab_size.max(1) as usize;
    let bounds: Vec<usize> = marks.iter().flat_map(|r| [r.start, r.end]).collect();
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
    fn side_by_side_numbers_survive_skips() {
        let l = Layout::build(&sample(), &opts(DiffViewMode::SideBySide));
        let r = l.rows();
        assert_eq!(r.len(), 2 + 1 + 2 + 1 + 1);
        assert_eq!(side(&r[0]), (Some((1, "a")), Some((1, "a"))));
        assert_eq!(side(&r[1]), (Some((2, "b")), Some((2, "b"))));
        assert_eq!(
            r[2],
            Row::Skip(SkipRow {
                lines: 10,
                old_start: 3,
                new_start: 3,
                at_start: false,
                at_end: false
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
            Row::Skip(SkipRow {
                lines: 5,
                old_start: 15,
                new_start: 16,
                at_start: false,
                at_end: true
            })
        );
        assert_eq!(l.max_numbers(), (19, 20));
    }

    #[test]
    fn unified_puts_deletions_before_insertions() {
        let l = Layout::build(&sample(), &opts(DiffViewMode::Unified));
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

    #[test]
    fn skip_at_start_and_end_are_flagged() {
        let d = diff(vec![
            DiffChunk::Skip { lines: 3 },
            changed(&["x"], &["y"]),
            DiffChunk::Skip { lines: 4 },
        ]);
        let l = Layout::build(&d, &opts(DiffViewMode::SideBySide));
        let flags: Vec<_> = l
            .rows()
            .iter()
            .filter_map(|r| match r {
                Row::Skip(s) => Some((s.at_start, s.at_end, s.old_start)),
                _ => None,
            })
            .collect();
        assert_eq!(flags, [(true, false, 1), (false, true, 5)]);
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
        let l = Layout::build(&d, &opts(DiffViewMode::SideBySide));
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

        let u = Layout::build(&d, &opts(DiffViewMode::Unified));
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
        let l = Layout::build(&d, &opts(DiffViewMode::SideBySide));
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
        let l = Layout::build(&d, &opts(DiffViewMode::SideBySide));
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
        let l = Layout::build(&d, &opts(DiffViewMode::SideBySide));
        let Row::Side { right: Some(c), .. } = &l.rows()[0] else {
            panic!()
        };
        assert_eq!(&*c.text, "    foo");
        assert_eq!(c.marks, [4..7]);

        let off = LayoutOptions {
            intraline: false,
            ..opts(DiffViewMode::SideBySide)
        };
        let l = Layout::build(&d, &off);
        let Row::Side { right: Some(c), .. } = &l.rows()[0] else {
            panic!()
        };
        assert!(c.marks.is_empty());
    }

    #[test]
    fn binary_empty_and_rename_notices() {
        let mut d = diff(Vec::new());
        d.binary = true;
        let l = Layout::build(&d, &opts(DiffViewMode::SideBySide));
        assert_eq!(l.rows(), [Row::Notice(Notice::Binary)]);
        assert_eq!(l.first_selectable(), None);
        assert!(l.chunks().is_empty());

        let l = Layout::build(&diff(Vec::new()), &opts(DiffViewMode::Unified));
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
        let l = Layout::build(&d, &opts(DiffViewMode::SideBySide));
        assert_eq!(
            l.rows()[0],
            Row::Notice(Notice::RenamedFrom("old.rs".into()))
        );
        assert_eq!(l.first_selectable(), Some(1));
        // Same name on both sides: no notice.
        d.meta_b = meta("old.rs");
        assert_eq!(Layout::build(&d, &opts(DiffViewMode::SideBySide)).len(), 1);
    }

    #[test]
    fn anchors_map_between_modes() {
        let side = Layout::build(&sample(), &opts(DiffViewMode::SideBySide));
        let uni = Layout::build(&sample(), &opts(DiffViewMode::Unified));
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
        // A line hidden by a skip is found at the skip row.
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
}
