//! Line maps between two commits, for carrying comments over to another
//! version of a change.

use model::{COMMIT_MSG_PATH, FileStatus, LineMap, RevisionId};

use crate::diff::{Text, is_binary, map_lines, side, unsupported_path};
use crate::files::{RENAME_SCORE, parse_diff_tree};
use crate::{Error, Local, NotLocal, Repo, commit_msg};

impl Repo {
    /// How the lines of `path` in commit `from` appear in commit `to` (see
    /// [`LineMap`]), following a rename between the two commits: git's
    /// rename detection with Gerrit's 60% similarity, over the two trees
    /// directly. The diff is the one Gerrit shows with no whitespace option
    /// (histogram, lines split on `\n` only), so a line that only differs
    /// in whitespace counts as changed.
    ///
    /// - Equal content (also after a pure rename) is an identity map,
    ///   without diffing.
    /// - A file deleted in `to` is [`LineMap::file_gone`].
    /// - A binary file that changed is [`LineMap::opaque`]: no line maps,
    ///   so line comments on it are held, while comments on the file as a
    ///   whole carry over.
    /// - `/COMMIT_MSG` is mapped through the two commits' synthetic texts
    ///   (as Gerrit builds them), so comments on the commit message carry
    ///   over where their lines are unchanged.
    ///
    /// For comments on the parent side, map between the two commits'
    /// parents; carrying them over only when the parent is unchanged is
    /// up to the caller.
    ///
    /// [`Local::NotLocal`] when a commit is missing
    /// ([`NotLocal::MissingCommit`]), for other synthetic paths
    /// (`/MERGE_LIST`, ...), for paths git cannot be asked about, and when
    /// `from` has no file at `path` ([`NotLocal::NotAFile`]).
    pub fn line_map(
        &self,
        from: &RevisionId,
        to: &RevisionId,
        path: &str,
    ) -> Result<Local<LineMap>, Error> {
        let Some(from_commit) = self.commit_data(from.as_str())? else {
            return Ok(Local::NotLocal(NotLocal::MissingCommit(
                from.as_str().to_owned(),
            )));
        };
        let Some(to_commit) = self.commit_data(to.as_str())? else {
            return Ok(Local::NotLocal(NotLocal::MissingCommit(
                to.as_str().to_owned(),
            )));
        };
        if path == COMMIT_MSG_PATH {
            // A parent missing from the checkout prints with an empty
            // subject on both sides alike, and a different parent changes
            // the line anyway (its hex digits), so the map is right
            // whatever the subject.
            let old = commit_msg::text(&from_commit, &self.parent_subjects(&from_commit)?);
            let new = commit_msg::text(&to_commit, &self.parent_subjects(&to_commit)?);
            let (a, b) = (Text::new(old.as_bytes()), Text::new(new.as_bytes()));
            return Ok(Local::Available(if old == new {
                LineMap::identity(path, line_count(&a))
            } else {
                LineMap::mapped(path, map_lines(&a, &b))
            }));
        }
        if path.starts_with('/') {
            return Ok(Local::NotLocal(NotLocal::SpecialFile(path.to_owned())));
        }
        if unsupported_path(path) {
            return Ok(Local::NotLocal(NotLocal::UnsupportedPath(path.to_owned())));
        }

        // Where the file went. The whole trees are compared, since a
        // rename needs every added file as a candidate.
        let (from_sha, to_sha) = (from_commit.sha.as_str(), to_commit.sha.as_str());
        let out = self.git.run(
            &[
                "diff-tree",
                "-r",
                "-z",
                "--no-ext-diff",
                "--no-textconv",
                "--no-commit-id",
                RENAME_SCORE,
                "--raw",
                from_sha,
                to_sha,
            ],
            None,
        )?;
        let changes = parse_diff_tree(&out)?;
        let renamed = changes
            .iter()
            .find(|c| c.status == FileStatus::Renamed && c.old_path.as_deref() == Some(path));
        let target = match (renamed, changes.iter().find(|c| c.path == path)) {
            (Some(r), _) => r.path.clone(),
            (None, Some(c)) if c.status == FileStatus::Deleted => {
                return Ok(Local::Available(LineMap::file_gone()));
            }
            (None, Some(c)) if c.status == FileStatus::Added => {
                return Ok(Local::NotLocal(NotLocal::NotAFile(path.to_owned())));
            }
            // Modified, or not listed because it is unchanged (or absent
            // from both, which reading it below finds out).
            _ => path.to_owned(),
        };

        let names = [format!("{from_sha}:{path}"), format!("{to_sha}:{target}")];
        let mut objects = self.git.read_objects(&names)?.into_iter();
        let (Some(old), Some(new)) = (objects.next(), objects.next()) else {
            return Err(Error::Parse {
                command: "cat-file --batch",
                detail: "missing answer".into(),
            });
        };
        let old = match side(old, path)? {
            Ok(Some(blob)) => blob,
            Ok(None) => return Ok(Local::NotLocal(NotLocal::NotAFile(path.to_owned()))),
            Err(not_local) => return Ok(Local::NotLocal(not_local)),
        };
        let new = match side(new, &target)? {
            Ok(Some(blob)) => blob,
            // A directory or submodule now, or gone after all.
            Ok(None) | Err(_) => return Ok(Local::Available(LineMap::file_gone())),
        };
        let a = Text::new(&old.data);
        Ok(Local::Available(if old.oid == new.oid {
            LineMap::identity(target, line_count(&a))
        } else if is_binary(&old.data) || is_binary(&new.data) {
            LineMap::opaque(target)
        } else {
            LineMap::mapped(target, map_lines(&a, &Text::new(&new.data)))
        }))
    }
}

fn line_count(t: &Text) -> u32 {
    u32::try_from(t.full_len()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests;
