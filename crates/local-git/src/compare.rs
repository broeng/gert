//! Resolving which two commits a Gerrit comparison means.

use model::RevisionId;

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

/// The parts of a commit the comparison needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitData {
    pub sha: String,
    pub parents: Vec<String>,
    /// `Name <email> <unix seconds> <+hhmm>` of the author, as stored.
    pub author: String,
    pub committer: String,
    /// The full commit message, as stored.
    pub message: String,
}

impl CommitData {
    /// The first line of the message.
    pub fn subject(&self) -> &str {
        self.message.lines().next().unwrap_or("")
    }
}

/// Two sides to compare, both present in the checkout. Made by
/// [`Repo::resolve`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comparison {
    /// The base: a commit, or the empty tree id for a root commit.
    pub(crate) base: String,
    pub(crate) base_kind: BaseKind,
    /// The base commit for [`BaseKind::PatchSet`].
    pub(crate) base_commit: Option<CommitData>,
    pub(crate) target: CommitData,
    /// The subject of the target's first parent, which Gerrit prints in the
    /// `Parent:` line of `/COMMIT_MSG`. Empty for a root commit or when the
    /// parent is not in the checkout.
    pub(crate) parent_subject: String,
}

impl Comparison {
    pub fn base_kind(&self) -> BaseKind {
        self.base_kind
    }

    /// The target commit.
    pub fn target(&self) -> &CommitData {
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
                        parent_subject: String::new(),
                    }))
                }
                [parent] => {
                    let Some(parent_data) = self.commit_data(parent)? else {
                        return Ok(Local::NotLocal(NotLocal::MissingCommit(parent.clone())));
                    };
                    Ok(Local::Available(Comparison {
                        base: parent.clone(),
                        base_kind: BaseKind::Parent,
                        base_commit: None,
                        target,
                        parent_subject: parent_data.subject().to_owned(),
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
                let parent_subject = match target.parents.first() {
                    Some(p) => self
                        .commit_data(p)?
                        .map(|c| c.subject().to_owned())
                        .unwrap_or_default(),
                    None => String::new(),
                };
                Ok(Local::Available(Comparison {
                    base: base.sha.clone(),
                    base_kind: BaseKind::PatchSet,
                    base_commit: Some(base),
                    target,
                    parent_subject,
                }))
            }
        }
    }

    /// The commit's parents and message, or `None` when it is not here.
    /// Read from the raw object, so shallow and graft files do not hide
    /// parents.
    fn commit_data(&self, sha: &str) -> Result<Option<CommitData>, Error> {
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

fn parse_commit(sha: &str, raw: &[u8]) -> CommitData {
    let text = String::from_utf8_lossy(raw);
    let (headers, message) = match text.split_once("\n\n") {
        Some((h, m)) => (h, m),
        None => (&*text, ""),
    };
    let mut parents = Vec::new();
    let (mut author, mut committer) = (String::new(), String::new());
    for line in headers.lines() {
        if let Some(p) = line.strip_prefix("parent ") {
            parents.push(p.to_owned());
        } else if let Some(a) = line.strip_prefix("author ") {
            author = a.to_owned();
        } else if let Some(c) = line.strip_prefix("committer ") {
            committer = c.to_owned();
        }
    }
    CommitData {
        sha: sha.to_owned(),
        parents,
        author,
        committer,
        message: message.to_owned(),
    }
}
