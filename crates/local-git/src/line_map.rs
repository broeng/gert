//! Line maps between two commits, for carrying comments over to another
//! version of a change.

use std::collections::{HashMap, HashSet};

use model::{COMMIT_MSG_PATH, CommitInfo, FileStatus, LineMap, RevisionId};

use crate::compare::{commit_from_object, commit_name};
use crate::diff::{Blob, Text, is_binary, map_lines, side, unsupported_path};
use crate::files::{RENAME_SCORE, parse_diff_tree};
use crate::{Error, Local, NotLocal, Repo, commit_msg};

/// git's file mode of a submodule.
const GITLINK_MODE: &str = "160000";

/// One entry per requested path.
type LineMaps = HashMap<String, Local<LineMap>>;

impl Repo {
    /// How the lines of `path` in commit `from` appear in commit `to`: one
    /// path of [`line_maps`](Self::line_maps), which says how it works.
    /// Several paths of the same two commits are much cheaper in one call
    /// there.
    pub fn line_map(
        &self,
        from: &RevisionId,
        to: &RevisionId,
        path: &str,
    ) -> Result<Local<LineMap>, Error> {
        Ok(match self.line_maps(from, to, &[path])? {
            Local::Available(mut maps) => maps
                .remove(path)
                // `line_maps` answers every path; this is not reached.
                .unwrap_or_else(|| Local::NotLocal(NotLocal::NotAFile(path.to_owned()))),
            Local::NotLocal(why) => Local::NotLocal(why),
        })
    }

    /// How the lines of each of `paths` in commit `from` appear in commit
    /// `to` (see [`LineMap`]), following renames between the two commits.
    /// The diff is the one Gerrit shows with no whitespace option
    /// (histogram, lines split on `\n` only), so a line that only differs
    /// in whitespace counts as changed.
    ///
    /// The outer [`Local`] is [`NotLocal::MissingCommit`] when either
    /// commit is not in the checkout; otherwise there is one entry per
    /// path:
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
    /// - Other synthetic paths are [`NotLocal::SpecialFile`]. For
    ///   `/PATCHSET_LEVEL` that is expected: patch set level comments have
    ///   no lines and always carry over.
    /// - A path that is not a file in `from` is [`NotLocal::NotAFile`].
    ///   That includes a file that `from` deletes: a comment on it can only
    ///   be on the file as a whole, and carries over when `to`'s file list
    ///   has the path as deleted too. (In a partial clone, a file whose
    ///   content is not here looks the same on the `from` side.)
    /// - A file still in `to` whose content cannot be read there (a
    ///   partial clone) is [`NotLocal::MissingContent`], and a submodule
    ///   in its place is [`NotLocal::NotAFile`]; neither is taken as gone.
    /// - Paths git cannot be asked about are
    ///   [`NotLocal::UnsupportedPath`].
    ///
    /// For comments on the parent side, map between the two commits'
    /// parents; carrying them over only when the parent is unchanged is
    /// up to the caller.
    ///
    /// Cost: one `cat-file` for both commits and one for every blob (and
    /// the parents for `/COMMIT_MSG`). Renames are looked for only when a
    /// file of `from` is missing from `to` (without break detection, git
    /// never takes a path that exists in both trees as a rename source):
    /// then one `diff-tree` with Gerrit's 60% similarity over the two
    /// whole trees, as for the file list, and one more `cat-file` for the
    /// new names.
    pub fn line_maps(
        &self,
        from: &RevisionId,
        to: &RevisionId,
        paths: &[&str],
    ) -> Result<Local<LineMaps>, Error> {
        let mut commits = self
            .read_commits(&[from.as_str(), to.as_str()])?
            .into_iter();
        let (from_commit, to_commit) = (commits.next().flatten(), commits.next().flatten());
        let Some(from_commit) = from_commit else {
            return Ok(Local::NotLocal(NotLocal::MissingCommit(
                from.as_str().to_owned(),
            )));
        };
        let Some(to_commit) = to_commit else {
            return Ok(Local::NotLocal(NotLocal::MissingCommit(
                to.as_str().to_owned(),
            )));
        };

        let mut maps = LineMaps::new();
        let mut files: Vec<&str> = Vec::new();
        let mut seen = HashSet::new();
        let mut commit_msg = false;
        for &path in paths {
            if !seen.insert(path) {
                continue;
            }
            if path == COMMIT_MSG_PATH {
                commit_msg = true;
            } else if path.starts_with('/') {
                let why = NotLocal::SpecialFile(path.to_owned());
                maps.insert(path.to_owned(), Local::NotLocal(why));
            } else if unsupported_path(path) {
                let why = NotLocal::UnsupportedPath(path.to_owned());
                maps.insert(path.to_owned(), Local::NotLocal(why));
            } else {
                files.push(path);
            }
        }

        // Both sides of every file, then the parents for `/COMMIT_MSG`, in
        // one `cat-file`.
        let (from_sha, to_sha) = (from_commit.sha.as_str(), to_commit.sha.as_str());
        let mut names: Vec<String> = Vec::new();
        for path in &files {
            names.push(format!("{from_sha}:{path}"));
            names.push(format!("{to_sha}:{path}"));
        }
        let mut parents: Vec<&str> = Vec::new();
        if commit_msg {
            for p in from_commit.parents.iter().chain(&to_commit.parents) {
                if !parents.contains(&p.as_str()) && commit_name(p.as_str()).is_some() {
                    parents.push(p.as_str());
                }
            }
            names.extend(parents.iter().filter_map(|p| commit_name(p)));
        }
        let mut objects = self.git.read_objects(&names)?.into_iter();
        let mut next = || {
            objects.next().ok_or_else(|| Error::Parse {
                command: "cat-file --batch",
                detail: "missing answer".into(),
            })
        };

        // Files whose path is not a file in `to`, maybe renamed.
        let mut moved: Vec<(&str, Blob)> = Vec::new();
        for &path in &files {
            let (old, new) = (next()?, next()?);
            let old = match side(old, path)? {
                Ok(Some(blob)) => blob,
                Ok(None) => {
                    let why = NotLocal::NotAFile(path.to_owned());
                    maps.insert(path.to_owned(), Local::NotLocal(why));
                    continue;
                }
                Err(why) => {
                    maps.insert(path.to_owned(), Local::NotLocal(why));
                    continue;
                }
            };
            match side(new, path)? {
                Ok(Some(new)) => {
                    maps.insert(path.to_owned(), Local::Available(map(path, &old, &new)));
                }
                // Absent, a directory or a submodule: let `diff-tree` say.
                Ok(None) | Err(_) => moved.push((path, old)),
            }
        }

        if commit_msg {
            let mut subjects: HashMap<&str, String> = HashMap::new();
            for &p in &parents {
                if let Some(c) = commit_from_object(p, next()?) {
                    subjects.insert(p, c.subject());
                }
            }
            maps.insert(
                COMMIT_MSG_PATH.to_owned(),
                Local::Available(commit_msg_map(&from_commit, &to_commit, &subjects)),
            );
        }

        if !moved.is_empty() {
            self.follow_moves(from_sha, to_sha, moved, &mut maps)?;
        }
        Ok(Local::Available(maps))
    }

    /// Finds out what became of files of `from` that are not files at
    /// the same path in `to`: renamed (mapped under the new name), deleted
    /// (gone), or still there but not readable as a file.
    fn follow_moves(
        &self,
        from_sha: &str,
        to_sha: &str,
        moved: Vec<(&str, Blob)>,
        maps: &mut LineMaps,
    ) -> Result<(), Error> {
        // The whole trees, since a rename needs every added file as a
        // candidate.
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
        let mut renamed: Vec<(&str, String, Blob)> = Vec::new();
        for (path, old) in moved {
            let rename = changes
                .iter()
                .find(|c| c.status == FileStatus::Renamed && c.old_path.as_deref() == Some(path));
            let result = match (rename, changes.iter().find(|c| c.path == path)) {
                (Some(r), _) => {
                    renamed.push((path, r.path.clone(), old));
                    continue;
                }
                // Deleted, or replaced by a directory (`-r` lists that as
                // a deletion and the directory's files as added).
                (None, Some(c)) if c.status == FileStatus::Deleted => {
                    Local::Available(LineMap::file_gone())
                }
                (None, Some(c)) if c.new_mode == GITLINK_MODE => {
                    Local::NotLocal(NotLocal::NotAFile(path.to_owned()))
                }
                // Still a file in `to`, but its content is not here.
                _ => Local::NotLocal(NotLocal::MissingContent(path.to_owned())),
            };
            maps.insert(path.to_owned(), result);
        }
        if renamed.is_empty() {
            return Ok(());
        }
        let names: Vec<String> = renamed
            .iter()
            .map(|(_, target, _)| format!("{to_sha}:{target}"))
            .collect();
        let objects = self.git.read_objects(&names)?;
        for ((path, target, old), new) in renamed.into_iter().zip(objects) {
            let result = match side(new, &target)? {
                Ok(Some(new)) => Local::Available(map(&target, &old, &new)),
                Ok(None) => Local::NotLocal(NotLocal::MissingContent(target)),
                Err(why) => Local::NotLocal(why),
            };
            maps.insert(path.to_owned(), result);
        }
        Ok(())
    }
}

/// The map between two blobs of a file that is at `target` in `to`.
fn map(target: &str, old: &Blob, new: &Blob) -> LineMap {
    let a = Text::new(&old.data);
    if old.oid == new.oid {
        LineMap::identity(target, line_count(&a))
    } else if is_binary(&old.data) || is_binary(&new.data) {
        LineMap::opaque(target)
    } else {
        LineMap::mapped(target, map_lines(&a, &Text::new(&new.data)))
    }
}

/// The map between the two commits' `/COMMIT_MSG` texts. A parent missing
/// from the checkout prints with an empty subject on both sides alike, and
/// a different parent changes the line anyway (its hex digits), so the map
/// is right whatever the subject.
fn commit_msg_map(from: &CommitInfo, to: &CommitInfo, subjects: &HashMap<&str, String>) -> LineMap {
    let text = |c: &CommitInfo| {
        let s: Vec<String> = c
            .parents
            .iter()
            .map(|p| subjects.get(p.as_str()).cloned().unwrap_or_default())
            .collect();
        commit_msg::text(c, &s)
    };
    let (old, new) = (text(from), text(to));
    let a = Text::new(old.as_bytes());
    if old == new {
        LineMap::identity(COMMIT_MSG_PATH, line_count(&a))
    } else {
        LineMap::mapped(COMMIT_MSG_PATH, map_lines(&a, &Text::new(new.as_bytes())))
    }
}

fn line_count(t: &Text) -> u32 {
    u32::try_from(t.full_len()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests;
