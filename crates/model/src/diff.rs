use std::ops::Range;

use serde::{Deserialize, Serialize};

/// How a file changed between the two sides of a diff (`DiffInfo.change_type`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DiffChangeType {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Rewrite,
    Unknown(String),
}

impl DiffChangeType {
    /// Maps Gerrit's wire value (`ADDED`, `MODIFIED`, ..., `REWRITE`).
    pub fn from_wire(s: &str) -> Self {
        match s {
            "ADDED" => Self::Added,
            "MODIFIED" => Self::Modified,
            "DELETED" => Self::Deleted,
            "RENAMED" => Self::Renamed,
            "COPIED" => Self::Copied,
            "REWRITE" => Self::Rewrite,
            other => Self::Unknown(other.to_owned()),
        }
    }
}

/// Whether the server computed intraline edits (`DiffInfo.intraline_status`).
/// Anything but `Ok` means the chunks carry no (or incomplete) edits.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IntralineStatus {
    Ok,
    Timeout,
    Failure,
    Unknown(String),
}

impl IntralineStatus {
    /// Maps Gerrit's wire value (`OK`, `TIMEOUT`, `FAILURE`).
    pub fn from_wire(s: &str) -> Self {
        match s {
            "OK" => Self::Ok,
            "TIMEOUT" => Self::Timeout,
            "FAILURE" => Self::Failure,
            other => Self::Unknown(other.to_owned()),
        }
    }
}

/// Metadata of one side of a diff (`FileMeta`). Absent for the side that
/// does not exist (`meta_a` of an added file, `meta_b` of a deleted one).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffFileMeta {
    pub name: String,
    pub content_type: Option<String>,
    /// Number of lines in the whole file on this side.
    pub lines: u32,
}

/// An intraline highlight, as Gerrit encodes it: `skip` characters that are
/// not highlighted, then `mark` characters that are. Each edit starts
/// where the previous one's mark ended (the first one at the chunk start).
///
/// Offsets are counted in UTF-16 code units (Java `char`s, like JavaScript
/// string indices) over the chunk's lines **joined with `\n`**, so the
/// line break between two lines counts as one character and a mark may run
/// across it. [`line_marks`] turns edits into per-line byte ranges.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntralineEdit {
    pub skip: u32,
    pub mark: u32,
}

/// A region where the two sides differ (the `a` / `b` entries of a
/// `DiffInfo.content` item). Either side may be empty (pure insertion or
/// deletion).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedChunk {
    /// Lines of the old side (`a`), without line terminators.
    pub a: Vec<String>,
    /// Lines of the new side (`b`).
    pub b: Vec<String>,
    /// Intraline highlights over `a` (`edit_a`); empty if none.
    pub edits_a: Vec<IntralineEdit>,
    pub edits_b: Vec<IntralineEdit>,
    /// The change is only there because the change was rebased: the lines
    /// differ between the two patch sets but not against the parent.
    pub due_to_rebase: bool,
    /// The server's `common` flag: the two sides are the same apart from
    /// ignored whitespace. Only set when a whitespace option is active.
    pub common: bool,
}

impl ChangedChunk {
    /// Highlighted byte ranges per line of `a`.
    pub fn marks_a(&self) -> Vec<Vec<Range<usize>>> {
        line_marks(&self.a, &self.edits_a)
    }

    /// Highlighted byte ranges per line of `b`.
    pub fn marks_b(&self) -> Vec<Vec<Range<usize>>> {
        line_marks(&self.b, &self.edits_b)
    }
}

/// One item of `DiffInfo.content`, in file order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffChunk {
    /// Lines present on both sides (`ab`). With a whitespace option the
    /// server sends the text of the new side.
    Common {
        lines: Vec<String>,
    },
    Changed(ChangedChunk),
    /// Lines omitted from the response (`skip`), identical on both sides.
    /// They count towards the line numbers of both sides.
    Skip {
        lines: u32,
    },
}

impl DiffChunk {
    /// How many lines of the old file this chunk covers.
    pub fn a_len(&self) -> u32 {
        match self {
            Self::Common { lines } => count(lines),
            Self::Changed(c) => count(&c.a),
            Self::Skip { lines } => *lines,
        }
    }

    /// How many lines of the new file this chunk covers.
    pub fn b_len(&self) -> u32 {
        match self {
            Self::Common { lines } => count(lines),
            Self::Changed(c) => count(&c.b),
            Self::Skip { lines } => *lines,
        }
    }
}

fn count(lines: &[String]) -> u32 {
    u32::try_from(lines.len()).unwrap_or(u32::MAX)
}

/// A file diff as the server computes it (`DiffInfo`).
///
/// This is a faithful model, not a layout: side-by-side rows and unified
/// lines are derived from `chunks` by the view. Line numbers are implicit:
/// walk the chunks and add [`DiffChunk::a_len`] / [`DiffChunk::b_len`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    pub meta_a: Option<DiffFileMeta>,
    pub meta_b: Option<DiffFileMeta>,
    pub change_type: Option<DiffChangeType>,
    pub intraline_status: Option<IntralineStatus>,
    /// The `git diff` header lines (`diff --git ...`, `rename from ...`).
    pub diff_header: Vec<String>,
    pub chunks: Vec<DiffChunk>,
    /// Binary files have no chunks.
    pub binary: bool,
}

impl Diff {
    /// Lines of the old file covered by the chunks.
    pub fn a_len(&self) -> u32 {
        self.chunks.iter().map(DiffChunk::a_len).sum()
    }

    /// Lines of the new file covered by the chunks.
    pub fn b_len(&self) -> u32 {
        self.chunks.iter().map(DiffChunk::b_len).sum()
    }
}

/// Converts Gerrit intraline edits over `lines` into highlighted byte
/// ranges, one `Vec` per line. See [`IntralineEdit`] for how offsets are
/// counted. Marks are clipped to the line they fall on (a mark that spans
/// a line break yields a range on each line, without the break itself);
/// empty and out-of-range parts are dropped.
pub fn line_marks(lines: &[String], edits: &[IntralineEdit]) -> Vec<Vec<Range<usize>>> {
    let mut out = vec![Vec::new(); lines.len()];
    if edits.is_empty() {
        return out;
    }
    // Absolute [start, end) of every mark in UTF-16 units.
    let mut marks = Vec::with_capacity(edits.len());
    let mut pos = 0_u64;
    for e in edits {
        let start = pos + u64::from(e.skip);
        let end = start + u64::from(e.mark);
        if end > start {
            marks.push((start, end));
        }
        pos = end;
    }
    let mut line_start = 0_u64;
    let mut first = 0; // marks are sorted, so earlier ones never matter again
    for (line, ranges) in lines.iter().zip(&mut out) {
        let len = line.encode_utf16().count() as u64;
        let line_end = line_start + len;
        while first < marks.len() && marks[first].1 <= line_start {
            first += 1;
        }
        for &(s, e) in &marks[first..] {
            if s >= line_end {
                break;
            }
            let (s, e) = (s.max(line_start), e.min(line_end));
            if e > s {
                let from = utf16_to_byte(line, s - line_start);
                let to = utf16_to_byte(line, e - line_start);
                ranges.push(from..to);
            }
        }
        line_start = line_end + 1; // the joining `\n`
    }
    out
}

/// Byte offset of the `units`-th UTF-16 code unit of `s` (rounded up to
/// the next character boundary, clamped to the end).
fn utf16_to_byte(s: &str, units: u64) -> usize {
    let mut seen = 0_u64;
    for (i, c) in s.char_indices() {
        if seen >= units {
            return i;
        }
        seen += c.len_utf16() as u64;
    }
    s.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| (*s).to_owned()).collect()
    }

    fn edit(skip: u32, mark: u32) -> IntralineEdit {
        IntralineEdit { skip, mark }
    }

    #[test]
    fn wire_values_keep_unknowns() {
        assert_eq!(
            DiffChangeType::from_wire("REWRITE"),
            DiffChangeType::Rewrite
        );
        assert_eq!(
            DiffChangeType::from_wire("SPLIT"),
            DiffChangeType::Unknown("SPLIT".into())
        );
        assert_eq!(
            IntralineStatus::from_wire("TIMEOUT"),
            IntralineStatus::Timeout
        );
        assert_eq!(
            IntralineStatus::from_wire("?"),
            IntralineStatus::Unknown("?".into())
        );
    }

    #[test]
    fn marks_within_one_line() {
        // "let foo = 1;" with "foo" marked, then "1" marked.
        let l = lines(&["let foo = 1;"]);
        let m = line_marks(&l, &[edit(4, 3), edit(4, 1)]);
        assert_eq!(m, vec![vec![4..7, 11..12]]);
    }

    #[test]
    fn mark_across_a_line_break_counts_the_newline() {
        // Joined text: "abc\ndef\nghi". Mark offsets 2..9 covers "c", the
        // newline, "def", the newline and "g".
        let l = lines(&["abc", "def", "ghi"]);
        let m = line_marks(&l, &[edit(2, 7)]);
        assert_eq!(m, vec![vec![2..3], vec![0..3], vec![0..1]]);
    }

    #[test]
    fn skip_across_lines_and_trailing_lines_without_marks() {
        let l = lines(&["ab", "cd", "ef"]);
        // Skip "ab\nc" (4), mark "d" (1).
        let m = line_marks(&l, &[edit(4, 1)]);
        assert_eq!(m, vec![vec![], vec![1..2], vec![]]);
    }

    #[test]
    fn offsets_are_utf16_units_but_ranges_are_bytes() {
        // "é" is 2 bytes / 1 unit; the emoji is 4 bytes / 2 units.
        let l = lines(&["é😀x"]);
        let m = line_marks(&l, &[edit(1, 2), edit(0, 1)]);
        assert_eq!(m, vec![vec![2..6, 6..7]]);
    }

    #[test]
    fn out_of_range_and_empty_marks_are_dropped() {
        let l = lines(&["abc"]);
        assert_eq!(
            line_marks(&l, &[edit(0, 0)]),
            vec![Vec::<Range<usize>>::new()]
        );
        assert_eq!(line_marks(&l, &[edit(2, 10)]), vec![vec![2..3]]);
        assert_eq!(
            line_marks(&l, &[edit(50, 3)]),
            vec![Vec::<Range<usize>>::new()]
        );
        assert!(line_marks(&[], &[edit(0, 1)]).is_empty());
    }

    #[test]
    fn chunk_lengths_and_totals() {
        let diff = Diff {
            meta_a: None,
            meta_b: None,
            change_type: None,
            intraline_status: None,
            diff_header: vec![],
            chunks: vec![
                DiffChunk::Common {
                    lines: lines(&["x", "y"]),
                },
                DiffChunk::Changed(ChangedChunk {
                    a: lines(&["old"]),
                    b: lines(&["new", "newer"]),
                    ..Default::default()
                }),
                DiffChunk::Skip { lines: 10 },
            ],
            binary: false,
        };
        assert_eq!((diff.a_len(), diff.b_len()), (13, 14));
    }

    #[test]
    fn chunk_marks_use_each_sides_own_edits() {
        let c = ChangedChunk {
            a: lines(&["foo bar"]),
            b: lines(&["foo baz"]),
            edits_a: vec![edit(4, 3)],
            edits_b: vec![edit(6, 1)],
            ..Default::default()
        };
        assert_eq!(c.marks_a(), vec![vec![4..7]]);
        assert_eq!(c.marks_b(), vec![vec![6..7]]);
    }
}
