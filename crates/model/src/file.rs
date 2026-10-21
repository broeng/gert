use serde::{Deserialize, Serialize};

/// The virtual file holding the commit message.
pub const COMMIT_MSG_PATH: &str = "/COMMIT_MSG";
/// The virtual file listing the commits merged by a merge commit.
pub const MERGE_LIST_PATH: &str = "/MERGE_LIST";

/// How a file changed (`FileInfo.status`). Values this client does not know
/// are kept in [`FileStatus::Unknown`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FileStatus {
    Added,
    Deleted,
    Renamed,
    Copied,
    Rewritten,
    Modified,
    Unknown(String),
}

impl FileStatus {
    /// Maps Gerrit's wire value (`A`, `D`, `R`, `C`, `W`, `M`). Gerrit
    /// leaves the field out for modified files, so `None` is `Modified`.
    pub fn from_wire(s: Option<&str>) -> Self {
        match s {
            None | Some("M") => Self::Modified,
            Some("A") => Self::Added,
            Some("D") => Self::Deleted,
            Some("R") => Self::Renamed,
            Some("C") => Self::Copied,
            Some("W") => Self::Rewritten,
            Some(other) => Self::Unknown(other.to_owned()),
        }
    }

    /// The one-letter code PolyGerrit shows in the file list.
    pub fn letter(&self) -> &str {
        match self {
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Copied => "C",
            Self::Rewritten => "W",
            Self::Modified => "M",
            Self::Unknown(s) => s,
        }
    }
}

/// One row of a change's file list (`FileInfo`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// The file's path in the target patch set. The server key of the entry.
    pub path: String,
    pub status: FileStatus,
    /// Where a renamed or copied file came from.
    pub old_path: Option<String>,
    pub lines_inserted: u32,
    pub lines_deleted: u32,
    pub size_delta: i64,
    pub size: u64,
    pub binary: bool,
}

impl FileEntry {
    /// `/COMMIT_MSG` and `/MERGE_LIST` are not files in the tree.
    pub fn is_special(&self) -> bool {
        self.path == COMMIT_MSG_PATH || self.path == MERGE_LIST_PATH
    }
}

/// Orders file paths like the server's file list: the special files
/// (`/COMMIT_MSG`, then `/MERGE_LIST`) first, the rest by path.
pub fn file_order(a: &str, b: &str) -> std::cmp::Ordering {
    let rank = |p: &str| match p {
        COMMIT_MSG_PATH => 0,
        MERGE_LIST_PATH => 1,
        _ => 2,
    };
    rank(a).cmp(&rank(b)).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_from_wire() {
        assert_eq!(FileStatus::from_wire(None), FileStatus::Modified);
        assert_eq!(FileStatus::from_wire(Some("A")), FileStatus::Added);
        assert_eq!(FileStatus::from_wire(Some("W")), FileStatus::Rewritten);
        let u = FileStatus::from_wire(Some("X"));
        assert_eq!(u, FileStatus::Unknown("X".into()));
        assert_eq!(u.letter(), "X");
        assert_eq!(FileStatus::Renamed.letter(), "R");
    }

    #[test]
    fn special_files_sort_first() {
        let mut paths = vec![
            "src/b.rs",
            ".gitignore",
            "/MERGE_LIST",
            "src/a.rs",
            "/COMMIT_MSG",
        ];
        paths.sort_by(|a, b| file_order(a, b));
        assert_eq!(
            paths,
            [
                "/COMMIT_MSG",
                "/MERGE_LIST",
                ".gitignore",
                "src/a.rs",
                "src/b.rs"
            ]
        );
    }
}
