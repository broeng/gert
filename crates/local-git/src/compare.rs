//! Resolving which two commits a Gerrit comparison means.

use model::{CommitInfo, RevisionId};

use crate::commits::parse_commit;
use crate::git::Object;
use crate::{Error, Local, NotLocal, Repo, is_object_id};

/// What the base side of a [`Comparison`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BaseKind {
    /// The target's first (only) parent, or the empty tree for a root
    /// commit: a change's default view.
    Parent,
    /// Another patch set of the same change (with the same parents).
    PatchSet,
}

/// Two sides to compare, both present in the checkout. Made by
/// [`Repo::resolve`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comparison {
    /// The base: a commit, or the empty tree id for a root commit.
    pub(crate) base: String,
    pub(crate) base_kind: BaseKind,
    /// The base commit for [`BaseKind::PatchSet`].
    pub(crate) base_commit: Option<CommitInfo>,
    pub(crate) target: CommitInfo,
    /// The subjects of the target's parents (which are the base's too),
    /// which Gerrit prints in the header of `/COMMIT_MSG`; see
    /// [`Repo::parent_subjects`].
    pub(crate) parent_subjects: Vec<String>,
}

impl Comparison {
    pub fn base_kind(&self) -> BaseKind {
        self.base_kind
    }

    /// The target commit.
    pub fn target(&self) -> &CommitInfo {
        &self.target
    }
}

impl Repo {
    /// Works out what to compare for `target` against `base`.
    ///
    /// With no base, that is the target's parent (the empty tree for a root
    /// commit). With a base, the two commits themselves. It is
    /// [`Local::NotLocal`] when a commit is missing, when the target is a
    /// merge commit without a base (Gerrit uses an auto-merge), when the
    /// parent of a shallow checkout is missing, or when the two patch sets
    /// have different parents (Gerrit then marks edits as due to rebase).
    pub fn resolve(
        &self,
        target: &RevisionId,
        base: Option<&RevisionId>,
    ) -> Result<Local<Comparison>, Error> {
        let Some(target) = self.commit_data(target.as_str())? else {
            return Ok(Local::NotLocal(NotLocal::MissingCommit(
                target.as_str().to_owned(),
            )));
        };
        match base {
            None => match target.parents.as_slice() {
                [] => {
                    let base = self.empty_tree()?;
                    Ok(Local::Available(Comparison {
                        base,
                        base_kind: BaseKind::Parent,
                        base_commit: None,
                        target,
                        parent_subjects: Vec::new(),
                    }))
                }
                [parent] => {
                    let Some(parent_data) = self.commit_data(parent.as_str())? else {
                        return Ok(Local::NotLocal(NotLocal::MissingCommit(
                            parent.as_str().to_owned(),
                        )));
                    };
                    Ok(Local::Available(Comparison {
                        base: parent.as_str().to_owned(),
                        base_kind: BaseKind::Parent,
                        base_commit: None,
                        parent_subjects: vec![parent_data.subject()],
                        target,
                    }))
                }
                _ => Ok(Local::NotLocal(NotLocal::MergeCommit)),
            },
            Some(base) => {
                let Some(base) = self.commit_data(base.as_str())? else {
                    return Ok(Local::NotLocal(NotLocal::MissingCommit(
                        base.as_str().to_owned(),
                    )));
                };
                if base.parents != target.parents {
                    return Ok(Local::NotLocal(NotLocal::ParentsDiffer));
                }
                let parent_subjects = self.parent_subjects(&target)?;
                Ok(Local::Available(Comparison {
                    base: base.sha.as_str().to_owned(),
                    base_kind: BaseKind::PatchSet,
                    base_commit: Some(base),
                    target,
                    parent_subjects,
                }))
            }
        }
    }

    /// The subject of each of the commit's parents, as Gerrit shows it
    /// ([`CommitInfo::subject`]), empty for a parent that is not in the
    /// checkout.
    pub(crate) fn parent_subjects(&self, commit: &CommitInfo) -> Result<Vec<String>, Error> {
        commit
            .parents
            .iter()
            .map(|p| {
                Ok(self
                    .commit_data(p.as_str())?
                    .map(|c| c.subject())
                    .unwrap_or_default())
            })
            .collect()
    }

    /// The commit's parents and message, or `None` when it is not here.
    /// Read from the raw object, so shallow and graft files do not hide
    /// parents.
    pub(crate) fn commit_data(&self, sha: &str) -> Result<Option<CommitInfo>, Error> {
        if !is_object_id(sha) || !self.has_commit(&RevisionId(sha.to_owned()))? {
            return Ok(None);
        }
        let objects = self.git.read_objects(&[format!("{sha}^{{commit}}")])?;
        // `cat-file --batch` prints the commit's raw object for a commit.
        let Some(Object::Found { kind, data, .. }) = objects.into_iter().next() else {
            return Ok(None);
        };
        if kind != "commit" {
            return Ok(None);
        }
        Ok(Some(parse_commit(sha, &data)))
    }

    /// The id of the empty tree in this repository's hash format.
    fn empty_tree(&self) -> Result<String, Error> {
        let out = self
            .git
            .run(&["hash-object", "-t", "tree", "--stdin"], Some(b""))?;
        Ok(String::from_utf8_lossy(&out).trim().to_owned())
    }
}
