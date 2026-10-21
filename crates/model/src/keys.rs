use crate::{ChangeNumber, DiffContext, PatchSetRef, Whitespace};

/// Identifies a file list: which patch set, compared against what.
/// `base: None` compares against the parent commit.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FilesKey {
    pub change: ChangeNumber,
    pub base: Option<PatchSetRef>,
    pub target: PatchSetRef,
}

/// Identifies one file's diff. Together with the server it is the cache
/// key: it contains the revision SHAs, not just the patch set numbers.
/// (`intraline` is always requested, so it is not part of the key.)
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DiffKey {
    pub change: ChangeNumber,
    pub base: Option<PatchSetRef>,
    pub target: PatchSetRef,
    pub path: String,
    pub whitespace: Whitespace,
    pub context: DiffContext,
}

impl DiffKey {
    /// The file list this diff belongs to.
    pub fn files_key(&self) -> FilesKey {
        FilesKey {
            change: self.change,
            base: self.base.clone(),
            target: self.target.clone(),
        }
    }

    /// The same comparison for another file.
    pub fn with_path(&self, path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            ..self.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PatchSetNumber;

    #[test]
    fn with_path_keeps_the_comparison() {
        let key = DiffKey {
            change: ChangeNumber(5),
            base: None,
            target: PatchSetRef {
                number: PatchSetNumber(2),
                sha: "bbb".into(),
            },
            path: "a.rs".into(),
            whitespace: Whitespace::IgnoreAll,
            context: DiffContext::All,
        };
        let next = key.with_path("b.rs");
        assert_eq!(next.path, "b.rs");
        assert_eq!(next.files_key(), key.files_key());
        assert_eq!(next.whitespace, Whitespace::IgnoreAll);
    }
}
