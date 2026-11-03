use crate::{ChangeNumber, PatchSetRef, Whitespace};

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
/// (`intraline` is always requested, so it is not part of the key. Nor is
/// the context: the server ignores it and always sends the whole file, so
/// unchanged lines are folded on the client.)
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DiffKey {
    pub change: ChangeNumber,
    pub base: Option<PatchSetRef>,
    pub target: PatchSetRef,
    pub path: String,
    pub whitespace: Whitespace,
}

impl FilesKey {
    /// Whether the comparison involves a local change or a local patch set:
    /// only the checkout can answer it, never Gerrit.
    pub fn is_local(&self) -> bool {
        self.change.is_local()
            || self.target.number.is_local()
            || self.base.as_ref().is_some_and(|b| b.number.is_local())
    }
}

impl DiffKey {
    /// Whether the diff involves a local change or a local patch set (see
    /// [`FilesKey::is_local`]).
    pub fn is_local(&self) -> bool {
        self.files_key().is_local()
    }

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
        };
        let next = key.with_path("b.rs");
        assert_eq!(next.path, "b.rs");
        assert_eq!(next.files_key(), key.files_key());
        assert_eq!(next.whitespace, Whitespace::IgnoreAll);
    }

    #[test]
    fn a_local_number_anywhere_makes_the_comparison_local() {
        let ps = |n: u32| PatchSetRef {
            number: PatchSetNumber(n),
            sha: "s".into(),
        };
        let local = PatchSetNumber::LOCAL_BASE + 1;
        let key = |change: u64, base: Option<u32>, target: u32| FilesKey {
            change: ChangeNumber(change),
            base: base.map(ps),
            target: ps(target),
        };
        assert!(!key(5, Some(1), 2).is_local());
        assert!(key(5, None, local).is_local());
        assert!(key(5, Some(local), 2).is_local());
        assert!(key(ChangeNumber::LOCAL_BASE + 3, None, local).is_local());
        let diff = DiffKey {
            change: ChangeNumber(5),
            base: None,
            target: ps(local),
            path: "a".into(),
            whitespace: Whitespace::IgnoreNone,
        };
        assert!(diff.is_local());
    }
}
