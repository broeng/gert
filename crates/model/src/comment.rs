use serde::{Deserialize, Serialize};

use crate::{Account, PatchSetNumber, RevisionId, Timestamp};

/// The virtual file that carries comments on a whole patch set rather than
/// on a file.
pub const PATCHSET_LEVEL_PATH: &str = "/PATCHSET_LEVEL";

/// Which side of a diff a comment is attached to (`Side`).
///
/// `Revision` is the patch set's own version of the file and the server's
/// default when the field is absent. `Parent` is the file as it was in the
/// parent commit (the left side when diffing against the base).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Parent,
    #[default]
    Revision,
}

impl Side {
    /// Maps Gerrit's wire value. Unknown or absent values are `Revision`,
    /// which is what the server assumes too.
    pub fn from_wire(s: Option<&str>) -> Self {
        match s {
            Some("PARENT") => Self::Parent,
            _ => Self::Revision,
        }
    }

    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Parent => "PARENT",
            Self::Revision => "REVISION",
        }
    }
}

/// A selected text range (`CommentRange`). Lines are 1-based, characters
/// 0-based; the end is exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CommentRange {
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
}

/// One line of source shown with a comment (`ContextLineInfo`), only sent
/// when context was requested.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextLine {
    pub line_number: u32,
    pub text: String,
}

/// Where in its file a comment sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Location {
    /// On the file as a whole (no `line`, no `range`).
    File,
    Line(u32),
    Range(CommentRange),
}

impl Location {
    /// A range wins over a line: the server sends `line` as the range's
    /// end line as well, so the range is the more precise anchor.
    pub fn from_parts(line: Option<u32>, range: Option<CommentRange>) -> Self {
        match (range, line) {
            (Some(r), _) => Self::Range(r),
            (None, Some(l)) => Self::Line(l),
            (None, None) => Self::File,
        }
    }

    pub fn line(&self) -> Option<u32> {
        match self {
            Self::File => None,
            Self::Line(l) => Some(*l),
            Self::Range(r) => Some(r.end_line),
        }
    }

    pub fn range(&self) -> Option<CommentRange> {
        match self {
            Self::Range(r) => Some(*r),
            _ => None,
        }
    }

    /// First line the comment touches, for sorting.
    pub fn start_line(&self) -> u32 {
        match self {
            Self::File => 0,
            Self::Line(l) => *l,
            Self::Range(r) => r.start_line,
        }
    }
}

/// A published comment (`CommentInfo`, or `RobotCommentInfo` with
/// `robot_id` set). Drafts that exist on the server are the same wire type
/// but are modelled by [`crate::Draft`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub id: String,
    /// A file path, `/COMMIT_MSG`, `/MERGE_LIST` or [`PATCHSET_LEVEL_PATH`].
    pub path: String,
    pub patch_set: PatchSetNumber,
    pub side: Side,
    /// For a comment on the parent side of a merge commit: which parent
    /// (1-based). `None` means the first parent.
    pub parent: Option<u32>,
    pub location: Location,
    pub in_reply_to: Option<String>,
    pub message: String,
    /// Absent only for comments of deleted or unknown accounts.
    pub author: Option<Account>,
    pub updated: Timestamp,
    /// Whether the comment asks for something to be done (the thread's
    /// state is that of its last comment).
    pub unresolved: bool,
    /// The commit the comment was made on.
    pub commit_id: Option<RevisionId>,
    pub tag: Option<String>,
    /// The change message that was posted together with the comment.
    pub change_message_id: Option<String>,
    /// Set for robot comments.
    pub robot_id: Option<String>,
    /// Source lines around the comment, when the request asked for them.
    #[serde(default)]
    pub context: Vec<ContextLine>,
}

impl Comment {
    pub fn is_robot(&self) -> bool {
        self.robot_id.is_some()
    }

    /// True for the commit message and patch-set level pseudo files.
    pub fn is_patch_set_level(&self) -> bool {
        self.path == PATCHSET_LEVEL_PATH
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_defaults_to_revision() {
        assert_eq!(Side::from_wire(None), Side::Revision);
        assert_eq!(Side::from_wire(Some("PARENT")), Side::Parent);
        assert_eq!(Side::from_wire(Some("FUTURE")), Side::Revision);
        assert_eq!(Side::Parent.as_wire(), "PARENT");
    }

    #[test]
    fn location_prefers_the_range() {
        let r = CommentRange {
            start_line: 3,
            start_character: 1,
            end_line: 5,
            end_character: 2,
        };
        assert_eq!(Location::from_parts(None, None), Location::File);
        assert_eq!(Location::from_parts(Some(4), None), Location::Line(4));
        let loc = Location::from_parts(Some(5), Some(r));
        assert_eq!(loc, Location::Range(r));
        assert_eq!(
            (loc.line(), loc.start_line(), loc.range()),
            (Some(5), 3, Some(r))
        );
        assert_eq!(Location::File.start_line(), 0);
    }
}
