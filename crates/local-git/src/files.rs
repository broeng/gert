//! The file list of a comparison, shaped like Gerrit's.

use std::collections::HashMap;

use model::{COMMIT_MSG_PATH, FileEntry, FileStatus, file_order};

use crate::compare::Comparison;
use crate::{Error, Local, Repo};

/// Gerrit's rename similarity threshold.
pub(crate) const RENAME_SCORE: &str = "-M60%";

const NULL_OID_CHAR: u8 = b'0';

impl Repo {
    /// The files that differ in `cmp`, sorted like Gerrit's list with the
    /// synthetic `/COMMIT_MSG` entry first. Renames are detected like
    /// Gerrit does (60% similarity). `/COMMIT_MSG` is counted on a text
    /// built like Gerrit's (header plus message), which gives its counts
    /// and size.
    pub fn file_list(&self, cmp: &Comparison) -> Result<Local<Vec<FileEntry>>, Error> {
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
                "--numstat",
                &cmp.base,
                cmp.target.sha.as_str(),
            ],
            None,
        )?;
        let raws = parse_diff_tree(&out)?;

        // Blob sizes of both sides, with one batch-check call.
        let mut oids: Vec<&str> = Vec::new();
        for r in &raws {
            for oid in [&r.old_oid, &r.new_oid] {
                if !is_null_oid(oid) && !oids.contains(&oid.as_str()) {
                    oids.push(oid);
                }
            }
        }
        let sizes = self.blob_sizes(&oids)?;
        let size_of = |oid: &str| sizes.get(oid).copied().unwrap_or(0);

        let mut files: Vec<FileEntry> = raws
            .iter()
            .map(|r| {
                let new_size = size_of(&r.new_oid);
                let old_size = size_of(&r.old_oid);
                FileEntry {
                    path: r.path.clone(),
                    status: r.status.clone(),
                    old_path: r.old_path.clone(),
                    lines_inserted: r.inserted.unwrap_or(0),
                    lines_deleted: r.deleted.unwrap_or(0),
                    size_delta: i64::try_from(new_size).unwrap_or(i64::MAX)
                        - i64::try_from(old_size).unwrap_or(i64::MAX),
                    size: new_size,
                    binary: r.inserted.is_none(),
                }
            })
            .collect();
        files.push(commit_msg_entry(cmp));
        files.sort_by(|a, b| file_order(&a.path, &b.path));
        Ok(Local::Available(files))
    }

    fn blob_sizes(&self, oids: &[&str]) -> Result<HashMap<String, u64>, Error> {
        if oids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut input = oids.join("\n").into_bytes();
        input.push(b'\n');
        let out = self.git.run(&["cat-file", "--batch-check"], Some(&input))?;
        let text = String::from_utf8_lossy(&out);
        let mut sizes = HashMap::new();
        for line in text.lines() {
            // `<oid> <type> <size>`; `<oid> missing` for a submodule commit.
            let mut parts = line.split(' ');
            if let (Some(oid), Some(_kind), Some(size)) = (parts.next(), parts.next(), parts.next())
                && let Ok(size) = size.parse()
            {
                sizes.insert(oid.to_owned(), size);
            }
        }
        Ok(sizes)
    }
}

fn is_null_oid(oid: &str) -> bool {
    oid.bytes().all(|b| b == NULL_OID_CHAR)
}

/// One changed file as `diff-tree --raw --numstat` reports it.
#[derive(Debug)]
pub(crate) struct RawChange {
    pub old_oid: String,
    pub new_oid: String,
    /// File modes as git prints them (`100644`), `000000` for an absent side.
    pub old_mode: String,
    pub new_mode: String,
    pub status: FileStatus,
    /// The similarity of a rename, in percent.
    pub score: Option<u32>,
    pub path: String,
    pub old_path: Option<String>,
    /// `None` for binary files (numstat prints `-`).
    pub inserted: Option<u32>,
    pub deleted: Option<u32>,
}

/// Parses `diff-tree -z --raw --numstat`: first every raw record
/// (`:mode mode oid oid status NUL path NUL [path NUL]`), then every numstat
/// record (`added TAB deleted TAB path NUL`, or with an empty path and the
/// old and new paths as two further NUL-separated fields for a rename).
pub(crate) fn parse_diff_tree(out: &[u8]) -> Result<Vec<RawChange>, Error> {
    let bad = |detail: String| Error::Parse {
        command: "diff-tree",
        detail,
    };
    let text = String::from_utf8_lossy(out);
    let mut fields = text.split('\0').filter(|f| !f.is_empty()).peekable();
    let mut changes: Vec<RawChange> = Vec::new();
    while let Some(meta) = fields.next_if(|f| f.starts_with(':')) {
        let parts: Vec<&str> = meta[1..].split(' ').collect();
        let [old_mode, new_mode, old_oid, new_oid, status] = parts.as_slice() else {
            return Err(bad(format!("bad raw record {meta:?}")));
        };
        let letter = status.chars().next().unwrap_or('?');
        let first = fields
            .next()
            .ok_or_else(|| bad("missing path".into()))?
            .to_owned();
        let (path, old_path) = if matches!(letter, 'R' | 'C') {
            let second = fields
                .next()
                .ok_or_else(|| bad("missing new path".into()))?;
            (second.to_owned(), Some(first))
        } else {
            (first, None)
        };
        let score = status.get(1..).and_then(|s| s.parse().ok());
        let status = match letter {
            'A' => FileStatus::Added,
            'D' => FileStatus::Deleted,
            'R' => FileStatus::Renamed,
            'C' => FileStatus::Copied,
            // 'T' (type change) and 'M'.
            _ => FileStatus::Modified,
        };
        changes.push(RawChange {
            old_oid: (*old_oid).to_owned(),
            new_oid: (*new_oid).to_owned(),
            old_mode: (*old_mode).to_owned(),
            new_mode: (*new_mode).to_owned(),
            score,
            status,
            path,
            old_path,
            inserted: None,
            deleted: None,
        });
    }
    let mut stats: HashMap<String, (Option<u32>, Option<u32>)> = HashMap::new();
    while let Some(rec) = fields.next() {
        let mut parts = rec.splitn(3, '\t');
        let (Some(ins), Some(del), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(bad(format!("bad numstat record {rec:?}")));
        };
        let path = if path.is_empty() {
            // A rename: the old path, then the new one.
            fields.next();
            fields
                .next()
                .ok_or_else(|| bad("missing renamed path".into()))?
        } else {
            path
        };
        stats.insert(path.to_owned(), (ins.parse().ok(), del.parse().ok()));
    }
    for c in &mut changes {
        let (ins, del) = stats.get(&c.path).copied().unwrap_or((None, None));
        c.inserted = ins;
        c.deleted = del;
    }
    Ok(changes)
}

/// The `/COMMIT_MSG` entry: the synthetic file against nothing (a parent
/// comparison) or against the base patch set's synthetic file.
fn commit_msg_entry(cmp: &Comparison) -> FileEntry {
    let (old, new) = cmp.commit_msg_texts();
    let old = old.unwrap_or_default();
    let (inserted, deleted) = crate::diff::count_changed_lines(&old, &new);
    let size = new.len() as u64;
    FileEntry {
        path: COMMIT_MSG_PATH.to_owned(),
        // Against the parent it is a new file; between patch sets it is
        // listed as modified (Gerrit prints no status) even if equal.
        status: if old.is_empty() {
            FileStatus::Added
        } else {
            FileStatus::Modified
        },
        old_path: None,
        lines_inserted: inserted,
        lines_deleted: deleted,
        size_delta: i64::try_from(size).unwrap_or(i64::MAX)
            - i64::try_from(old.len()).unwrap_or(i64::MAX),
        size,
        binary: false,
    }
}
