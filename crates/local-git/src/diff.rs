//! Line diffs of one file, shaped like Gerrit's `DiffInfo`.

use imara_diff::{Algorithm, Diff as LineDiff, InternedInput};
use model::{
    COMMIT_MSG_PATH, ChangedChunk, Diff, DiffChangeType, DiffChunk, DiffFileMeta, MERGE_LIST_PATH,
    Whitespace,
};

use crate::compare::Comparison;
use crate::git::Object;
use crate::{Error, Local, NotLocal, Repo};

/// Git's own rule for "this is a binary file".
const BINARY_SNIFF_BYTES: usize = 8000;

impl Repo {
    /// The diff of `path` in `cmp`: the whole file as alternating `Common`
    /// and `Changed` chunks (the UI folds context itself), computed with the
    /// histogram algorithm. `old_path` is the path on the base side when the
    /// file was renamed. Intraline edits are not computed here.
    ///
    /// [`Local::NotLocal`] for the synthetic `/COMMIT_MSG` and `/MERGE_LIST`
    /// files, unknown whitespace modes, and paths git cannot be asked about.
    /// Reads both blobs with one `cat-file --batch`.
    pub fn diff(
        &self,
        cmp: &Comparison,
        path: &str,
        old_path: Option<&str>,
        whitespace: &Whitespace,
    ) -> Result<Local<Diff>, Error> {
        if path == COMMIT_MSG_PATH || path == MERGE_LIST_PATH {
            return Ok(Local::NotLocal(NotLocal::SpecialFile(path.to_owned())));
        }
        if matches!(whitespace, Whitespace::Unknown(_)) {
            return Ok(Local::NotLocal(NotLocal::UnknownWhitespace));
        }
        let old_name = old_path.unwrap_or(path);
        for p in [path, old_name] {
            // A newline would break the line protocol; `./` and `../`
            // would be resolved against the current directory.
            if p.is_empty()
                || p.contains(['\n', '\0'])
                || p.starts_with("./")
                || p.starts_with("../")
            {
                return Ok(Local::NotLocal(NotLocal::UnsupportedPath(p.to_owned())));
            }
        }
        let names = [
            format!("{}:{old_name}", cmp.base),
            format!("{}:{path}", cmp.target.sha),
        ];
        let mut objects = self.git.read_objects(&names)?.into_iter();
        let (Some(old), Some(new)) = (objects.next(), objects.next()) else {
            return Err(Error::Parse {
                command: "cat-file --batch",
                detail: "missing answer".into(),
            });
        };
        let old = match side(old, old_name)? {
            Ok(s) => s,
            Err(not_local) => return Ok(Local::NotLocal(not_local)),
        };
        let new = match side(new, path)? {
            Ok(s) => s,
            Err(not_local) => return Ok(Local::NotLocal(not_local)),
        };
        if old.is_none() && new.is_none() {
            return Ok(Local::NotLocal(NotLocal::NotAFile(path.to_owned())));
        }

        let change_type = match (&old, &new) {
            (None, _) => DiffChangeType::Added,
            (_, None) => DiffChangeType::Deleted,
            _ if old_name != path => DiffChangeType::Renamed,
            _ => DiffChangeType::Modified,
        };
        let binary = [&old, &new]
            .into_iter()
            .flatten()
            .any(|b| is_binary(&b.data));
        let diff_header = header(old_name, path, &old, &new, &change_type);
        let (lines_a, lines_b, chunks) = if binary {
            (Vec::new(), Vec::new(), Vec::new())
        } else {
            let a = old
                .as_ref()
                .map(|b| split_lines(&b.data))
                .unwrap_or_default();
            let b = new
                .as_ref()
                .map(|b| split_lines(&b.data))
                .unwrap_or_default();
            let chunks = chunks(&a, &b, whitespace);
            (a, b, chunks)
        };
        let meta = |present: bool, name: &str, lines: &[String]| {
            present.then(|| DiffFileMeta {
                name: name.to_owned(),
                content_type: None,
                lines: u32::try_from(lines.len()).unwrap_or(u32::MAX),
            })
        };
        Ok(Local::Available(Diff {
            meta_a: meta(old.is_some(), old_name, &lines_a),
            meta_b: meta(new.is_some(), path, &lines_b),
            change_type: Some(change_type),
            intraline_status: None,
            diff_header,
            chunks,
            binary,
        }))
    }
}

struct Blob {
    oid: String,
    data: Vec<u8>,
}

/// A blob, nothing (the file is absent on this side) or the reason this
/// cannot be diffed from the checkout.
fn side(obj: Object, name: &str) -> Result<Result<Option<Blob>, NotLocal>, Error> {
    Ok(match obj {
        Object::Missing => Ok(None),
        Object::Found { oid, kind, data } if kind == "blob" => Ok(Some(Blob { oid, data })),
        // A directory or a submodule where a file was expected.
        Object::Found { .. } => Err(NotLocal::NotAFile(name.to_owned())),
    })
}

fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(BINARY_SNIFF_BYTES)].contains(&0)
}

/// Splits on `\n` only, like JGit's `RawText`: a `\r` stays on its line
/// (the diff view shows it as whitespace), a missing final newline still
/// leaves a last line, and a final newline does not add an empty one.
fn split_lines(data: &[u8]) -> Vec<String> {
    let mut lines: Vec<&[u8]> = data.split(|&b| b == b'\n').collect();
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
        .into_iter()
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .collect()
}

fn header(
    old_name: &str,
    path: &str,
    old: &Option<Blob>,
    new: &Option<Blob>,
    change_type: &DiffChangeType,
) -> Vec<String> {
    let abbrev = |b: &Option<Blob>| {
        b.as_ref().map_or_else(
            || "0000000".to_owned(),
            |b| b.oid[..7.min(b.oid.len())].to_owned(),
        )
    };
    let mut h = vec![format!("diff --git a/{old_name} b/{path}")];
    match change_type {
        DiffChangeType::Added => h.push("new file mode 100644".to_owned()),
        DiffChangeType::Deleted => h.push("deleted file mode 100644".to_owned()),
        DiffChangeType::Renamed => {
            h.push(format!("rename from {old_name}"));
            h.push(format!("rename to {path}"));
        }
        _ => {}
    }
    h.push(format!("index {}..{}", abbrev(old), abbrev(new)));
    h.push(match old {
        Some(_) => format!("--- a/{old_name}"),
        None => "--- /dev/null".to_owned(),
    });
    h.push(match new {
        Some(_) => format!("+++ b/{path}"),
        None => "+++ /dev/null".to_owned(),
    });
    h
}

/// How a line is compared for a whitespace mode.
///
/// Kept in one place, with [`chunks`], so the mapping to Gerrit's
/// behaviour is easy to adjust.
fn normalize(line: &str, ws: &Whitespace) -> String {
    match ws {
        Whitespace::IgnoreTrailing => line.trim_end().to_owned(),
        Whitespace::IgnoreLeadingAndTrailing => line.trim().to_owned(),
        Whitespace::IgnoreAll => line.chars().filter(|c| !c.is_whitespace()).collect(),
        Whitespace::IgnoreNone | Whitespace::Unknown(_) => line.to_owned(),
    }
}

/// The whole-file chunks: lines equal under the whitespace mode are
/// `Common` (carrying the new side's text, as Gerrit does), the rest are
/// `Changed`.
fn chunks(a: &[String], b: &[String], ws: &Whitespace) -> Vec<DiffChunk> {
    let mut input = InternedInput::default();
    input.update_before(a.iter().map(|l| normalize(l, ws)));
    input.update_after(b.iter().map(|l| normalize(l, ws)));
    let diff = LineDiff::compute(Algorithm::Histogram, &input);

    let mut out = Vec::new();
    let mut b_pos = 0_usize;
    for hunk in diff.hunks() {
        let (bs, be) = (hunk.after.start as usize, hunk.after.end as usize);
        if bs > b_pos {
            out.push(DiffChunk::Common {
                lines: b[b_pos..bs].to_vec(),
            });
        }
        out.push(DiffChunk::Changed(ChangedChunk {
            a: a[hunk.before.start as usize..hunk.before.end as usize].to_vec(),
            b: b[bs..be].to_vec(),
            ..Default::default()
        }));
        b_pos = be;
    }
    if b_pos < b.len() {
        out.push(DiffChunk::Common {
            lines: b[b_pos..].to_vec(),
        });
    }
    out
}

/// Inserted and deleted line counts between two texts.
pub(crate) fn count_changed_lines(old: &str, new: &str) -> (u32, u32) {
    let a = split_lines(old.as_bytes());
    let b = split_lines(new.as_bytes());
    let mut input = InternedInput::default();
    input.update_before(a.iter().map(String::as_str));
    input.update_after(b.iter().map(String::as_str));
    let diff = LineDiff::compute(Algorithm::Histogram, &input);
    (diff.count_additions(), diff.count_removals())
}
