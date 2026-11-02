//! The file list of a comparison, shaped like Gerrit's.

use std::collections::HashMap;

use model::{COMMIT_MSG_PATH, FileEntry, FileStatus, file_order};

use crate::compare::{BaseKind, CommitData, Comparison};
use crate::{Error, Local, Repo};

/// Gerrit's rename similarity threshold.
const RENAME_SCORE: &str = "-M60%";

const NULL_OID_CHAR: u8 = b'0';

impl Repo {
    /// The files that differ in `cmp`, sorted like Gerrit's list with the
    /// synthetic `/COMMIT_MSG` entry first. Renames are detected like
    /// Gerrit does (60% similarity). Line counts of `/COMMIT_MSG` are
    /// approximate: they are counted on a text built like Gerrit's header
    /// plus message, not on Gerrit's exact one.
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
                &cmp.target.sha,
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
struct RawChange {
    old_oid: String,
    new_oid: String,
    status: FileStatus,
    path: String,
    old_path: Option<String>,
    /// `None` for binary files (numstat prints `-`).
    inserted: Option<u32>,
    deleted: Option<u32>,
}

/// Parses `diff-tree -z --raw --numstat`: first every raw record
/// (`:mode mode oid oid status NUL path NUL [path NUL]`), then every numstat
/// record (`added TAB deleted TAB path NUL`, or with an empty path and the
/// old and new paths as two further NUL-separated fields for a rename).
fn parse_diff_tree(out: &[u8]) -> Result<Vec<RawChange>, Error> {
    let bad = |detail: String| Error::Parse {
        command: "diff-tree",
        detail,
    };
    let text = String::from_utf8_lossy(out);
    let mut fields = text.split('\0').filter(|f| !f.is_empty()).peekable();
    let mut changes: Vec<RawChange> = Vec::new();
    while let Some(meta) = fields.next_if(|f| f.starts_with(':')) {
        let parts: Vec<&str> = meta[1..].split(' ').collect();
        let [_, _, old_oid, new_oid, status] = parts.as_slice() else {
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
    let new = synthetic_commit_file(&cmp.target);
    let old = match (cmp.base_kind, &cmp.base_commit) {
        (BaseKind::PatchSet, Some(base)) => synthetic_commit_file(base),
        _ => String::new(),
    };
    let (inserted, deleted) = crate::diff::count_changed_lines(&old, &new);
    let size = new.len() as u64;
    FileEntry {
        path: COMMIT_MSG_PATH.to_owned(),
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

/// Text shaped like Gerrit's `/COMMIT_MSG` file: a header of parent,
/// author and committer lines, a blank line and the message.
fn synthetic_commit_file(c: &CommitData) -> String {
    let mut s = String::new();
    if let Some(parent) = c.parents.first() {
        s.push_str(&format!("Parent:     {}\n", &parent[..parent.len().min(7)]));
    }
    s.push_str(&format!("Author:     {}\n", person(&c.author)));
    s.push_str(&format!("AuthorDate: {}\n", date(&c.author)));
    s.push_str(&format!("Commit:     {}\n", person(&c.committer)));
    s.push_str(&format!("CommitDate: {}\n\n", date(&c.committer)));
    s.push_str(c.message.trim_end());
    s.push('\n');
    s
}

/// `Name <email>` from a stored `Name <email> secs tz` ident.
fn person(ident: &str) -> &str {
    ident.rsplitn(3, ' ').nth(2).unwrap_or(ident)
}

/// `YYYY-MM-DD hh:mm:ss +hhmm` from a stored ident.
fn date(ident: &str) -> String {
    let mut it = ident.rsplitn(3, ' ');
    let (Some(tz), Some(secs)) = (it.next(), it.next()) else {
        return String::new();
    };
    let Ok(secs) = secs.parse::<i64>() else {
        return String::new();
    };
    let offset = tz_offset_seconds(tz);
    let local = secs + offset;
    let days = local.div_euclid(86_400);
    let rem = local.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} {tz}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn tz_offset_seconds(tz: &str) -> i64 {
    let sign = if tz.starts_with('-') { -1 } else { 1 };
    let digits = tz.trim_start_matches(['+', '-']);
    let (h, m) = (digits.get(..2), digits.get(2..4));
    let h: i64 = h.and_then(|h| h.parse().ok()).unwrap_or(0);
    let m: i64 = m.and_then(|m| m.parse().ok()).unwrap_or(0);
    sign * (h * 3600 + m * 60)
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}
