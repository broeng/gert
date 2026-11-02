//! Where the lines of a file went between two commits, for carrying
//! comments over to another version of a change ("porting").

use crate::{CommentRange, Location};

/// How the lines of one file in one commit (`from`) appear in the same
/// file in another commit (`to`), following a rename. Made by `local-git`
/// (`Repo::line_map`).
///
/// Lines are 1-based, as in Gerrit's comments, and counted the way Gerrit's
/// diff shows the file: the newline at the end of a file adds an empty last
/// line. A line maps only when it is unchanged; a changed or deleted line,
/// also one that differs only in whitespace, maps to nothing, so a comment
/// on it cannot be carried over.
///
/// Comments on the parent side (`Side::Parent`) are mapped with the map
/// between the two parent commits; whether to carry them over at all (only
/// when the parent is unchanged) is the caller's decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineMap {
    target: Target,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    /// The file is not in `to` (deleted, and not renamed).
    Gone,
    File {
        path: String,
        lines: Lines,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Lines {
    /// The same content: line n is line n, for the `count` lines there are.
    Identity { count: u32 },
    /// `map[n - 1]` is where line n went.
    Mapped(Vec<Option<u32>>),
    /// Changed content without lines to map (a binary file).
    Opaque,
}

impl LineMap {
    /// The file is unchanged (maybe renamed to `path`) and has `lines`
    /// lines.
    pub fn identity(path: impl Into<String>, lines: u32) -> Self {
        Self::file(path, Lines::Identity { count: lines })
    }

    /// The file is at `path` in `to`, and line n of `from` is `map[n - 1]`
    /// there (`None`: changed or deleted). `map` has an entry for every
    /// line of `from`.
    pub fn mapped(path: impl Into<String>, map: Vec<Option<u32>>) -> Self {
        Self::file(path, Lines::Mapped(map))
    }

    /// The file is at `path` in `to`, changed, but its lines cannot be
    /// mapped (a binary file): only comments on the file as a whole carry
    /// over.
    pub fn opaque(path: impl Into<String>) -> Self {
        Self::file(path, Lines::Opaque)
    }

    /// The file is not in `to`.
    pub fn file_gone() -> Self {
        Self {
            target: Target::Gone,
        }
    }

    fn file(path: impl Into<String>, lines: Lines) -> Self {
        Self {
            target: Target::File {
                path: path.into(),
                lines,
            },
        }
    }

    /// The file's path in `to`, after any rename; `None` when it is gone.
    pub fn target_path(&self) -> Option<&str> {
        match &self.target {
            Target::Gone => None,
            Target::File { path, .. } => Some(path),
        }
    }

    pub fn is_file_gone(&self) -> bool {
        matches!(self.target, Target::Gone)
    }

    /// Whether the content is the same in both commits, so every line maps
    /// to itself.
    pub fn is_identity(&self) -> bool {
        matches!(
            self.target,
            Target::File {
                lines: Lines::Identity { .. },
                ..
            }
        )
    }

    /// Where line `line` of `from` is in `to`, if it is there unchanged.
    /// `None` for line 0 and lines past the end of the file.
    pub fn map_line(&self, line: u32) -> Option<u32> {
        let Target::File { lines, .. } = &self.target else {
            return None;
        };
        let index = usize::try_from(line.checked_sub(1)?).ok()?;
        match lines {
            Lines::Identity { count } => (line <= *count).then_some(line),
            Lines::Mapped(map) => map.get(index).copied().flatten(),
            Lines::Opaque => None,
        }
    }

    /// The range in `to`, if every line it touches is unchanged and nothing
    /// was inserted between them; the characters stay as they are. The end
    /// line counts as touched even when the range ends at its first
    /// character.
    pub fn map_range(&self, range: CommentRange) -> Option<CommentRange> {
        if range.start_line > range.end_line {
            return None;
        }
        let start = self.map_line(range.start_line)?;
        let end = self.map_line(range.end_line)?;
        // Unchanged lines keep their order, so when both ends moved by the
        // same amount and every line between them maps, nothing was
        // inserted or deleted inside.
        if end.checked_sub(start) != Some(range.end_line - range.start_line) {
            return None;
        }
        if !(range.start_line..=range.end_line).all(|l| self.map_line(l).is_some()) {
            return None;
        }
        Some(CommentRange {
            start_line: start,
            end_line: end,
            ..range
        })
    }

    /// A comment's location in `to`: a comment on the file as a whole
    /// carries over while the file exists, a line or a range as
    /// [`map_line`](Self::map_line) and [`map_range`](Self::map_range) say.
    pub fn map_location(&self, location: Location) -> Option<Location> {
        match location {
            Location::File => self.target_path().map(|_| Location::File),
            Location::Line(l) => self.map_line(l).map(Location::Line),
            Location::Range(r) => self.map_range(r).map(Location::Range),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start_line: u32, end_line: u32) -> CommentRange {
        CommentRange {
            start_line,
            start_character: 2,
            end_line,
            end_character: 5,
        }
    }

    #[test]
    fn identity_maps_every_line_of_the_file_to_itself() {
        let m = LineMap::identity("new.rs", 3);
        assert!(m.is_identity());
        assert_eq!(m.target_path(), Some("new.rs"));
        assert_eq!(
            (1..=4).map(|l| m.map_line(l)).collect::<Vec<_>>(),
            [Some(1), Some(2), Some(3), None]
        );
        assert_eq!(m.map_line(0), None);
        assert_eq!(m.map_range(range(1, 3)), Some(range(1, 3)));
    }

    #[test]
    fn mapped_lines_and_ranges() {
        // Line 2 changed, a line was inserted after line 3.
        let m = LineMap::mapped("a.rs", vec![Some(1), None, Some(3), Some(5), Some(6)]);
        assert!(!m.is_identity());
        assert_eq!(m.map_line(1), Some(1));
        assert_eq!(m.map_line(2), None);
        assert_eq!(m.map_line(4), Some(5));
        assert_eq!(m.map_line(6), None);
        // Inside unchanged lines, shifted.
        assert_eq!(m.map_range(range(4, 5)), Some(range(5, 6)));
        // Across the changed line, and across the insertion.
        assert_eq!(m.map_range(range(1, 3)), None);
        assert_eq!(m.map_range(range(3, 4)), None);
        // Backwards.
        assert_eq!(m.map_range(range(5, 4)), None);
        assert_eq!(
            m.map_location(Location::Range(range(5, 5))),
            Some(Location::Range(range(6, 6)))
        );
        assert_eq!(m.map_location(Location::Line(2)), None);
        assert_eq!(m.map_location(Location::File), Some(Location::File));
    }

    #[test]
    fn a_gone_file_and_an_opaque_one() {
        let gone = LineMap::file_gone();
        assert!(gone.is_file_gone());
        assert_eq!(gone.target_path(), None);
        assert_eq!(gone.map_line(1), None);
        assert_eq!(gone.map_location(Location::File), None);

        let bin = LineMap::opaque("logo.png");
        assert!(!bin.is_file_gone());
        assert_eq!(bin.map_line(1), None);
        assert_eq!(bin.map_location(Location::File), Some(Location::File));
    }
}
