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
    /// There were more descendants than the limit: the ones furthest from
    /// the commit (nearest the branch tips) were left out.
    pub more_descendants: bool,
    /// The commit itself, then its first parent, that one's first parent,
    /// and so on, down to (not including) the first commit a
    /// remote-tracking branch has.
    pub ancestors: Vec<CommitInfo>,
    /// The limit ended the walk down before a remote-tracking branch did.
    pub more_ancestors: bool,
}

impl Repo {
    /// The series around `sha`: at most `limit` ancestors (the commit
    /// included) along first parents while no remote-tracking branch
    /// (`refs/remotes/*`) has them, and at most `limit` descendants on the
    /// named local branches (`refs/heads/*`) that contain it, those nearest
    /// the commit. Neither side walks into history a remote-tracking
    /// branch has (upstream commits merged into a branch, say). A detached
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
        // One more than asked for, to know whether the limit cut anything.
        let max = format!("--max-count={}", limit.saturating_add(1));
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
        let mut below = object_ids(&out);
        let more_ancestors = below.len() > limit;
        below.truncate(limit);

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
        let mut above = if tips.is_empty() {
            Vec::new()
        } else {
            let not_own = format!("^{}", own.sha.as_str());
            let mut args = vec!["rev-list", "--topo-order", "--parents"];
            args.extend(tips.iter().map(String::as_str));
            args.extend([not_own.as_str(), "--not", "--remotes"]);
            let out = self.git.run(&args, None)?;
            descendants_of(own.sha.as_str(), &out)
        };
        // Newest first, so the ones nearest the commit are at the end. No
        // cap on the walk: it only covers local history, and a capped
        // topological walk would keep the commits nearest the tips.
        let more_descendants = above.len() > limit;
        above.drain(..above.len().saturating_sub(limit));

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
            more_descendants,
            ancestors,
            more_ancestors,
        }))
    }
}

/// The commits of a `rev-list --topo-order --parents` answer (`<sha>
/// <parent>...` per line, children before parents) that descend from
/// `root`, in the answer's order. The walk was cut at `root` and at
/// history remote-tracking branches have, so it also lists commits on
/// other lines (a side branch merged in) that a plain `--ancestry-path`
/// would take for descendants when there are several such boundaries.
fn descendants_of(root: &str, out: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(out);
    let lines: Vec<Vec<&str>> = text
        .lines()
        .map(|l| l.split(' ').collect::<Vec<_>>())
        .filter(|f| f.first().is_some_and(|s| is_object_id(s)))
        .collect();
    let mut found: std::collections::HashSet<&str> = std::collections::HashSet::from([root]);
    let mut keep = vec![false; lines.len()];
    for (i, fields) in lines.iter().enumerate().rev() {
        if fields[1..].iter().any(|p| found.contains(p)) {
            found.insert(fields[0]);
            keep[i] = true;
        }
    }
    lines
        .iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|(f, _)| f[0].to_owned())
        .collect()
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
