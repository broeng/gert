//! Character-level highlights inside a changed region, like Gerrit's
//! `IntraLineLoader`.
//!
//! What Gerrit does (verified against 3.14): each side of a changed region
//! is its lines, every one followed by its line break, as one text; the two
//! texts are diffed per character; an insertion or deletion slides down to
//! where Gerrit puts it (see [`slide`]); changed regions that are close are
//! joined (see [`MERGE_GAP`]); a mark that spans whole lines takes the line
//! break with it. The result is close to Gerrit's, not identical: on the
//! dev Gerrit's seed data and a set of hand-made cases, about 98% of the
//! changed regions get exactly Gerrit's marks (`tests/dev_gerrit.rs`
//! reports it); the rest differ where several equally good alignments
//! exist, or where Gerrit's own cleanup marks a whole, very different, line.

use std::ops::Range;

use imara_diff::{Algorithm, Diff, InternedInput};
use model::IntralineEdit;

/// Changed regions separated by at most this many unchanged characters
/// become one mark, as long as the gap is not more than a lone line break
/// (on either side). Gerrit joins `1abcde2` / `9abcde8` and keeps
/// `1abcdef2` / `9abcdef8` apart, and keeps `1\n\nx` and `9\n\ny` apart.
const MERGE_GAP: usize = 5;

/// The edits for the two sides of a changed region that has lines on both
/// sides: `(edits_a, edits_b)`. A side is not `terminated` when its last
/// line is the last line of a file without a final newline: that line
/// has no line break.
pub(crate) fn edits(
    a: &[String],
    b: &[String],
    a_terminated: bool,
    b_terminated: bool,
) -> (Vec<IntralineEdit>, Vec<IntralineEdit>) {
    let (ta, tb) = (Side::new(a, a_terminated), Side::new(b, b_terminated));
    // Like JGit, strip the common start and then the common end first, so
    // an insertion into a run of equal characters lands after the run's
    // start.
    let start = ta
        .chars
        .iter()
        .zip(&tb.chars)
        .take_while(|(x, y)| x == y)
        .count();
    let end = ta.chars[start..]
        .iter()
        .rev()
        .zip(tb.chars[start..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (mid_a, mid_b) = (
        &ta.chars[start..ta.chars.len() - end],
        &tb.chars[start..tb.chars.len() - end],
    );
    let mut input = InternedInput::default();
    input.update_before(mid_a.iter().copied());
    input.update_after(mid_b.iter().copied());
    let diff = Diff::compute(Algorithm::Histogram, &input);

    let hunks: Vec<(Range<usize>, Range<usize>)> = diff
        .hunks()
        .map(|h| {
            (
                start + h.before.start as usize..start + h.before.end as usize,
                start + h.after.start as usize..start + h.after.end as usize,
            )
        })
        .collect();
    let mut regions: Vec<(Range<usize>, Range<usize>)> = Vec::new();
    for (i, (mut ra, mut rb)) in hunks.iter().cloned().enumerate() {
        let (next_a, next_b) = hunks
            .get(i + 1)
            .map_or((ta.chars.len(), tb.chars.len()), |(a, b)| {
                (a.start, b.start)
            });
        slide(&mut ra, &mut rb, &ta.chars, &tb.chars, next_a, next_b);
        if let Some((pa, pb)) = regions.last_mut()
            && ta.gap_joins(pa.end..ra.start)
            && tb.gap_joins(pb.end..rb.start)
        {
            pa.end = ra.end;
            pb.end = rb.end;
            continue;
        }
        regions.push((ra, rb));
    }
    let ea = regions.iter().map(|(r, _)| r.clone()).collect::<Vec<_>>();
    let eb = regions.iter().map(|(_, r)| r.clone()).collect::<Vec<_>>();
    (ta.encode(&ea), tb.encode(&eb))
}

/// Moves a pure insertion or deletion down while the text stays the same
/// (an insertion into a run of equal characters ends up after the run's
/// start), until it covers whole lines (`z\n`, not `\nz`) or cannot move.
/// This is where Gerrit puts them. `next_a` and `next_b` are the starts of
/// the next changed region.
fn slide(
    ra: &mut Range<usize>,
    rb: &mut Range<usize>,
    a: &[char],
    b: &[char],
    next_a: usize,
    next_b: usize,
) {
    let (side, chars, next, other) = if ra.start == ra.end && rb.start != rb.end {
        (rb, b, next_b, ra)
    } else if rb.start == rb.end && ra.start != ra.end {
        (ra, a, next_a, rb)
    } else {
        return;
    };
    let aligned =
        |r: &Range<usize>| (r.start == 0 || chars[r.start - 1] == '\n') && chars[r.end - 1] == '\n';
    while !aligned(side)
        && side.end < next
        && side.end < chars.len()
        && chars[side.start] == chars[side.end]
    {
        side.start += 1;
        side.end += 1;
        other.start += 1;
        other.end += 1;
    }
}

/// One side's text as characters.
struct Side {
    chars: Vec<char>,
}

impl Side {
    fn new(lines: &[String], terminated: bool) -> Self {
        let mut chars = Vec::new();
        for l in lines {
            chars.extend(l.chars());
            chars.push('\n');
        }
        if !terminated {
            chars.pop();
        }
        Self { chars }
    }

    fn gap_joins(&self, gap: Range<usize>) -> bool {
        gap.len() <= MERGE_GAP
            && (!self.chars[gap.clone()].contains(&'\n') || self.chars[gap] == ['\n'])
    }

    /// Character ranges to `skip`/`mark` pairs in UTF-16 units. A range
    /// that covers whole lines, or the empty line, takes the line break
    /// after it too.
    fn encode(&self, ranges: &[Range<usize>]) -> Vec<IntralineEdit> {
        let units = |r: Range<usize>| -> u32 {
            self.chars[r]
                .iter()
                .map(|c| c.len_utf16() as u32)
                .sum::<u32>()
        };
        let mut out = Vec::new();
        let mut cursor = 0;
        for r in ranges {
            let (start, mut end) = (r.start, r.end);
            let at_line_start = start == 0 || self.chars[start - 1] == '\n';
            if at_line_start && end < self.chars.len() && self.chars[end] == '\n' {
                end += 1;
            }
            if end == start {
                continue;
            }
            out.push(IntralineEdit {
                skip: units(cursor..start),
                mark: units(start..end),
            });
            cursor = end;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| (*s).to_owned()).collect()
    }

    fn e(skip: u32, mark: u32) -> IntralineEdit {
        IntralineEdit { skip, mark }
    }

    fn run(a: &[&str], b: &[&str]) -> (Vec<IntralineEdit>, Vec<IntralineEdit>) {
        edits(&lines(a), &lines(b), true, true)
    }

    #[test]
    fn marks_the_changed_characters_of_a_word() {
        // Gerrit marks `eh` / `he`, not the whole word.
        let (a, b) = run(&["Teh queue"], &["The queue"]);
        assert_eq!((a, b), (vec![e(1, 2)], vec![e(1, 2)]));
        let (a, b) = run(&["x1"], &["y1"]);
        assert_eq!((a, b), (vec![e(0, 1)], vec![e(0, 1)]));
    }

    #[test]
    fn a_pure_insertion_marks_only_the_inserting_side() {
        let (a, b) = run(&["foo(bar)"], &["foo(1, bar)"]);
        assert!(a.is_empty());
        assert_eq!(b, [e(4, 3)]);
    }

    #[test]
    fn close_edits_join_and_distant_ones_do_not() {
        // Five unchanged characters between them: one mark.
        let (a, _) = run(&["1abcde2"], &["9abcde8"]);
        assert_eq!(a, [e(0, 8)]);
        // Six: two marks (the first starts at the line start, but does
        // not end at the line end, so no line break is taken).
        let (a, b) = run(&["1abcdef2"], &["9abcdef8"]);
        assert_eq!(a, [e(0, 1), e(6, 1)]);
        assert_eq!(b, [e(0, 1), e(6, 1)]);
    }

    #[test]
    fn a_mark_over_whole_lines_includes_the_line_break() {
        let (a, b) = run(&["b"], &["B"]);
        assert_eq!((a, b), (vec![e(0, 2)], vec![e(0, 2)]));
        // An inserted whole line sits with its own line break.
        let (a, b) = run(&["x1", "}"], &["y1", "}", "z"]);
        assert!(a.iter().all(|m| m.skip + m.mark <= 3));
        assert_eq!(b.last(), Some(&e(4, 2)));
    }

    #[test]
    fn offsets_count_utf16_units() {
        // The emoji is two units.
        let (a, b) = run(&["héllo 😀 smile"], &["héllo 😀 happy"]);
        assert_eq!((a, b), (vec![e(9, 5)], vec![e(9, 5)]));
    }

    #[test]
    fn a_last_line_without_a_final_newline_has_no_line_break() {
        let a = lines(&["three"]);
        let b = lines(&["three", "four"]);
        let (ea, eb) = edits(&a, &b, false, false);
        assert!(ea.is_empty());
        // `\nfour`, five characters, ending with the text.
        assert_eq!(eb, [e(5, 5)]);
    }
}
