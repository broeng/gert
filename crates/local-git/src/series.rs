//! The line of history around one commit, for a relation chain of local
//! commits.

use model::{CommitInfo, RevisionId};

use crate::{Error, Local, NotLocal, Repo, is_object_id};

/// The commits around one commit in the checkout: what it was built on and
/// what was built on it. Made by [`Repo::series`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Series {
    /// The commits on local branches that contain the commit and were
    /// built on it, newest first and never a parent before its children.
    pub descendants: Vec<CommitInfo>,
    /// The commit itself, then its first parent, that one's first parent,
    /// and so on, down to (not including) the first commit a
    /// remote-tracking branch has.
    pub ancestors: Vec<CommitInfo>,
}

impl Repo {
    /// The series around `sha`: at most `limit` ancestors (the commit
    /// included) along first parents while no remote-tracking branch
    /// (`refs/remotes/*`) has them, and at most `limit` descendants on the
    /// named local branches (`refs/heads/*`) that contain it. A detached
    /// HEAD does not count, as in [`scan`](Self::scan). A commit that a
    /// remote-tracking branch has is still its own first ancestor.
    ///
    /// [`NotLocal::MissingCommit`] when the checkout has no such commit.
    /// Three or four git calls.
    pub fn series(&self, sha: &RevisionId, limit: usize) -> Result<Local<Series>, Error> {
        let Some(own) = self.commit_data(sha.as_str())? else {
            return Ok(Local::NotLocal(NotLocal::MissingCommit(
                sha.as_str().to_owned(),
            )));
        };
        let limit = limit.max(1);
        let max = format!("--max-count={limit}");
        // `own.sha` is a full object id: `commit_data` reads nothing else.
        let out = self.git.run(
            &[
                "rev-list",
                "--first-parent",
                &max,
                own.sha.as_str(),
                "--not",
                "--remotes",
            ],
            None,
        )?;
        let below = object_ids(&out);

        let out = self.git.run(
            &[
                "for-each-ref",
                "--format=%(objectname)",
                "--contains",
                own.sha.as_str(),
                "refs/heads/",
            ],
            None,
        )?;
        let tips = object_ids(&out);
        let above = if tips.is_empty() {
            Vec::new()
        } else {
            let mut input = String::new();
            for tip in &tips {
                input.push_str(tip);
                input.push('\n');
            }
            input.push('^');
            input.push_str(own.sha.as_str());
            input.push('\n');
            let out = self.git.run(
                &[
                    "rev-list",
                    "--topo-order",
                    "--ancestry-path",
                    &max,
                    "--stdin",
                ],
                Some(input.as_bytes()),
            )?;
            object_ids(&out)
        };

        // The commit itself is read already; everything else in one go.
        let wanted: Vec<&str> = above
            .iter()
            .chain(below.iter().filter(|s| **s != own.sha.as_str()))
            .map(String::as_str)
            .collect();
        let mut read = self.read_commits(&wanted)?.into_iter();
        let mut take = |n: usize| -> Vec<CommitInfo> { read.by_ref().take(n).flatten().collect() };
        let descendants = take(above.len());
        let mut ancestors = vec![own];
        ancestors.extend(take(wanted.len() - above.len()));
        Ok(Local::Available(Series {
            descendants,
            ancestors,
        }))
    }
}

/// The full object ids among the lines of `out`.
fn object_ids(out: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(out)
        .lines()
        .filter(|l| is_object_id(l))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests;
