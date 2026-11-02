//! Line diffs of one file, shaped like Gerrit's `DiffInfo`.

use std::ops::Range;

use imara_diff::{Algorithm, Diff as LineDiff, InternedInput};
use model::{
    COMMIT_MSG_PATH, ChangedChunk, Diff, DiffChangeType, DiffChunk, DiffFileMeta, IntralineStatus,
    MERGE_LIST_PATH, Whitespace,
};

use crate::compare::Comparison;
use crate::files::{RawChange, parse_diff_tree};
use crate::git::Object;
use crate::{Error, Local, NotLocal, Repo};

/// How much changed text (bytes of the lines of both sides, summed over a
/// file) gets intraline edits before we give up and answer like Gerrit does
/// when its intraline diff runs out of time: status `Timeout` and no edits
/// at all. Gerrit's limit is a time, ours is a size: the character diff is
/// quick up to this and the cap keeps one huge generated file from
/// stalling the background thread.
const INTRALINE_BUDGET: usize = 500_000;

/// Git's own rule for "this is a binary file".
const BINARY_SNIFF_BYTES: usize = 8000;

impl Repo {
    /// The diff of `path` in `cmp`: the whole file as alternating `Common`
    /// and `Changed` chunks (the UI folds context itself), computed with the
    /// histogram algorithm. `old_path` is the path on the base side when the
    /// file was renamed. The chunks carry intraline edits (as if the server
    /// had been asked for them), see the `intraline` module.
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
        let raw = self.raw_change(cmp, old_name, path)?;
        let diff_header = header(
            old_name,
            path,
            &old,
            &new,
            &change_type,
            raw.as_ref(),
            binary,
        );
        let text = |b: &Option<Blob>| b.as_ref().map(|b| Text::new(&b.data));
        let (text_a, text_b) = (text(&old), text(&new));
        let mut timed_out = false;
        let chunks = if binary {
            // Gerrit has no chunks for an added or deleted binary file and
            // one `skip` over the (meaningless) lines of a changed one.
            match (&text_a, &text_b) {
                (Some(_), Some(b)) if b.full_len() > 0 => vec![DiffChunk::Skip {
                    lines: u32::try_from(b.full_len()).unwrap_or(u32::MAX),
                }],
                _ => Vec::new(),
            }
        } else {
            let (chunks, over_budget) = chunks(
                text_a.as_ref(),
                text_b.as_ref(),
                whitespace,
                matches!((&old, &new), (Some(o), Some(n)) if o.oid == n.oid),
            );
            timed_out = over_budget;
            chunks
        };
        let meta = |t: &Option<Text>, name: &str| {
            t.as_ref().map(|t| DiffFileMeta {
                name: name.to_owned(),
                content_type: None,
                lines: u32::try_from(t.full_len()).unwrap_or(u32::MAX),
            })
        };
        Ok(Local::Available(Diff {
            meta_a: meta(&text_a, old_name),
            meta_b: meta(&text_b, path),
            change_type: Some(change_type),
            intraline_status: Some(if timed_out {
                IntralineStatus::Timeout
            } else {
                IntralineStatus::Ok
            }),
            diff_header,
            chunks,
            binary,
        }))
    }

    /// What `diff-tree` says about this one file: its modes and, for a
    /// rename, the similarity. Limited to the two paths, so cheap.
    fn raw_change(
        &self,
        cmp: &Comparison,
        old_name: &str,
        path: &str,
    ) -> Result<Option<RawChange>, Error> {
        let mut args = vec![
            "diff-tree",
            "-r",
            "-z",
            "--no-ext-diff",
            "--no-textconv",
            "--no-commit-id",
            "-M60%",
            "--raw",
            cmp.base.as_str(),
            cmp.target.sha.as_str(),
            "--",
            path,
        ];
        if old_name != path {
            args.push(old_name);
        }
        let out = self.git.run(&args, None)?;
        Ok(parse_diff_tree(&out)?.into_iter().find(|r| r.path == path))
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

/// A file's lines the way Gerrit presents them (verified against Gerrit
/// 3.14): split on `\n` only, so a `\r` stays on its line (the diff view
/// shows it as whitespace). The diff itself runs over [`Text::lines`],
/// which, like JGit's `RawText`, has no extra empty line for a final
/// newline; the presentation then adds that empty last line
/// ([`Text::full_len`] counts it, `a\n` shows as `["a", ""]`). A file
/// without a final newline just has no empty last line, and an empty file
/// has no lines at all.
struct Text {
    lines: Vec<String>,
    /// Whether the data ends with a newline (and so shows one more, empty,
    /// line).
    final_newline: bool,
}

impl Text {
    fn new(data: &[u8]) -> Self {
        if data.is_empty() {
            return Self {
                lines: Vec::new(),
                final_newline: false,
            };
        }
        let final_newline = data.ends_with(b"\n");
        let body = if final_newline {
            &data[..data.len() - 1]
        } else {
            data
        };
        Self {
            lines: body
                .split(|&b| b == b'\n')
                .map(|l| String::from_utf8_lossy(l).into_owned())
                .collect(),
            final_newline,
        }
    }

    /// The number of lines as shown, with the empty one after a final
    /// newline.
    fn full_len(&self) -> usize {
        self.lines.len() + usize::from(self.final_newline)
    }
}

/// The `git diff` header as JGit's `DiffFormatter` writes it (which is what
/// Gerrit sends): mode changes, `new file` / `deleted file`, rename lines
/// with the similarity, and only when the content differs the `index` line
/// (with the mode if it did not change) and the `---` / `+++` pair, then
/// `Binary files differ` for binary files.
fn header(
    old_name: &str,
    path: &str,
    old: &Option<Blob>,
    new: &Option<Blob>,
    change_type: &DiffChangeType,
    raw: Option<&RawChange>,
    binary: bool,
) -> Vec<String> {
    let abbrev = |b: &Option<Blob>| {
        b.as_ref().map_or_else(
            || "0000000".to_owned(),
            |b| b.oid[..7.min(b.oid.len())].to_owned(),
        )
    };
    let (old_mode, new_mode) = raw.map_or(("100644", "100644"), |r| {
        (r.old_mode.as_str(), r.new_mode.as_str())
    });
    let same_content = matches!((old, new), (Some(o), Some(n)) if o.oid == n.oid);
    // Gerrit names an added file's path on both sides, and a deleted one's.
    let mut h = vec![match change_type {
        DiffChangeType::Added => format!("diff --git a/{path} b/{path}"),
        _ => format!("diff --git a/{old_name} b/{path}"),
    }];
    match change_type {
        DiffChangeType::Added => h.push(format!("new file mode {new_mode}")),
        DiffChangeType::Deleted => h.push(format!("deleted file mode {old_mode}")),
        DiffChangeType::Renamed => {
            h.push(format!(
                "similarity index {}%",
                raw.and_then(|r| r.score).unwrap_or(100)
            ));
            h.push(format!("rename from {old_name}"));
            h.push(format!("rename to {path}"));
        }
        _ => {}
    }
    if old.is_some() && new.is_some() && old_mode != new_mode {
        h.push(format!("old mode {old_mode}"));
        h.push(format!("new mode {new_mode}"));
    }
    if !same_content {
        let mode = if old_mode == new_mode {
            format!(" {new_mode}")
        } else {
            String::new()
        };
        h.push(format!("index {}..{}{mode}", abbrev(old), abbrev(new)));
        h.push(match old {
            Some(_) => format!("--- a/{old_name}"),
            None => "--- /dev/null".to_owned(),
        });
        h.push(match new {
            Some(_) => format!("+++ b/{path}"),
            None => "+++ /dev/null".to_owned(),
        });
        if binary {
            h.push("Binary files differ".to_owned());
        }
    }
    h
}

/// The characters Gerrit's whitespace modes treat as whitespace (JGit's
/// `RawCharUtil.isWhitespace`): no form feed, vertical tab or no-break space.
fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

/// The key a line is compared by under a whitespace mode, which follows
/// JGit's comparators (verified against Gerrit 3.14): `IGNORE_TRAILING`
/// drops trailing whitespace, `IGNORE_LEADING_AND_TRAILING` (JGit's
/// "ignore change in amount of whitespace", despite the name) also treats
/// every run of whitespace as a single space, so `x` and ` x` still
/// differ, and `IGNORE_ALL` drops all whitespace.
fn normalize(line: &str, ws: &Whitespace) -> String {
    match ws {
        Whitespace::IgnoreTrailing => line.trim_end_matches(is_ws).to_owned(),
        Whitespace::IgnoreLeadingAndTrailing => {
            let mut out = String::with_capacity(line.len());
            let mut in_run = false;
            for c in line.trim_end_matches(is_ws).chars() {
                if is_ws(c) {
                    if !in_run {
                        out.push(' ');
                    }
                    in_run = true;
                } else {
                    in_run = false;
                    out.push(c);
                }
            }
            out
        }
        Whitespace::IgnoreAll => line.chars().filter(|&c| !is_ws(c)).collect(),
        Whitespace::IgnoreNone | Whitespace::Unknown(_) => line.to_owned(),
    }
}

/// Gerrit joins two changed regions into one chunk when everything between
/// them is boring (it asks for intraline edits, which is what does this):
/// blank lines, a line that ends in `{` or `:`, and a lone `}`.
fn is_boring(line: &str) -> bool {
    let line = line.trim_end_matches([' ', '\t']);
    line.is_empty() || line.ends_with(['{', ':']) || line.trim_start_matches([' ', '\t']) == "}"
}

/// The whole-file chunks, as Gerrit builds them (verified against 3.14):
///
/// - The diff runs over the lines without the empty one a final newline
///   adds; with no whitespace option a last line without a newline is not
///   equal to the same text with one. The empty last line is appended
///   afterwards: common when both sides have it, else to the end of the
///   side that has it.
/// - Changed regions separated only by [`is_boring`] lines are one chunk.
/// - Lines that are equal under the whitespace mode but not as text are
///   `Changed` with `common` set (a run of them is one chunk, with both
///   sides' text); lines equal as text are `Common`.
/// - If the mode finds no change at all in files with different content,
///   the whole file is one `Skip`.
fn chunks(
    a: Option<&Text>,
    b: Option<&Text>,
    ws: &Whitespace,
    same_blob: bool,
) -> (Vec<DiffChunk>, bool) {
    let empty = Text::new(b"");
    let (a, b) = (a.unwrap_or(&empty), b.unwrap_or(&empty));
    let keep_newline = matches!(ws, Whitespace::IgnoreNone | Whitespace::Unknown(_));
    let key = |t: &Text, i: usize, l: &String| {
        (
            normalize(l, ws),
            keep_newline && !t.final_newline && i + 1 == t.lines.len(),
        )
    };
    let mut input = InternedInput::default();
    input.update_before(a.lines.iter().enumerate().map(|(i, l)| key(a, i, l)));
    input.update_after(b.lines.iter().enumerate().map(|(i, l)| key(b, i, l)));
    let diff = LineDiff::compute(Algorithm::Histogram, &input);

    // The changed regions as (a range, b range), joined across boring gaps.
    let mut regions: Vec<(Range<usize>, Range<usize>)> = Vec::new();
    for h in diff.hunks() {
        let ra = h.before.start as usize..h.before.end as usize;
        let rb = h.after.start as usize..h.after.end as usize;
        if let Some((pa, pb)) = regions.last_mut()
            && a.lines[pa.end..ra.start].iter().all(|l| is_boring(l))
            && b.lines[pb.end..rb.start].iter().all(|l| is_boring(l))
        {
            pa.end = ra.end;
            pb.end = rb.end;
            continue;
        }
        regions.push((ra, rb));
    }

    if regions.is_empty() && !keep_newline && !same_blob && a.final_newline == b.final_newline {
        let lines = u32::try_from(b.full_len()).unwrap_or(u32::MAX);
        let chunks = if lines == 0 {
            Vec::new()
        } else {
            vec![DiffChunk::Skip { lines }]
        };
        return (chunks, false);
    }

    let mut out = Vec::new();
    let mut budget = INTRALINE_BUDGET;
    let mut over_budget = false;
    let (mut pa, mut pb) = (0_usize, 0_usize);
    for (ra, rb) in regions {
        push_equal_run(&mut out, &a.lines[pa..ra.start], &b.lines[pb..rb.start]);
        let mut chunk = ChangedChunk {
            a: a.lines[ra.clone()].to_vec(),
            b: b.lines[rb.clone()].to_vec(),
            ..Default::default()
        };
        let size = chunk
            .a
            .iter()
            .chain(&chunk.b)
            .map(|l| l.len() + 1)
            .sum::<usize>();
        if !chunk.a.is_empty() && !chunk.b.is_empty() && budget.checked_sub(size).is_none() {
            // Like Gerrit when it runs out of time: no edits at all.
            over_budget = true;
        } else if !chunk.a.is_empty() && !chunk.b.is_empty() {
            budget -= size;
            (chunk.edits_a, chunk.edits_b) = crate::intraline::edits(
                &chunk.a,
                &chunk.b,
                a.final_newline || ra.end < a.lines.len(),
                b.final_newline || rb.end < b.lines.len(),
            );
        }
        out.push(DiffChunk::Changed(chunk));
        (pa, pb) = (ra.end, rb.end);
    }
    push_equal_run(&mut out, &a.lines[pa..], &b.lines[pb..]);

    let last_line = String::new();
    match (a.final_newline, b.final_newline) {
        (true, true) => match out.last_mut() {
            Some(DiffChunk::Common { lines }) => lines.push(last_line),
            _ => out.push(DiffChunk::Common {
                lines: vec![last_line],
            }),
        },
        (true, false) | (false, true) => {
            let in_a = a.final_newline;
            match out.last_mut() {
                Some(DiffChunk::Changed(c)) if !c.common => {
                    if in_a {
                        c.a.push(last_line);
                    } else {
                        c.b.push(last_line);
                    }
                }
                _ => out.push(DiffChunk::Changed(ChangedChunk {
                    a: if in_a {
                        vec![last_line.clone()]
                    } else {
                        Vec::new()
                    },
                    b: if in_a { Vec::new() } else { vec![last_line] },
                    ..Default::default()
                })),
            }
        }
        (false, false) => {}
    }
    if over_budget {
        for c in &mut out {
            if let DiffChunk::Changed(c) = c {
                c.edits_a.clear();
                c.edits_b.clear();
            }
        }
    }
    (out, over_budget)
}

/// Lines the diff found equal (`a` and `b` have the same length): runs that
/// are the same text become `Common` (joining a `Common` just before), runs
/// that only equal under the whitespace mode become `Changed` with
/// `common` set.
fn push_equal_run(out: &mut Vec<DiffChunk>, a: &[String], b: &[String]) {
    for (la, lb) in a.iter().zip(b) {
        let same = la == lb;
        match out.last_mut() {
            Some(DiffChunk::Common { lines }) if same => lines.push(lb.clone()),
            Some(DiffChunk::Changed(c)) if !same && c.common => {
                c.a.push(la.clone());
                c.b.push(lb.clone());
            }
            _ if same => out.push(DiffChunk::Common {
                lines: vec![lb.clone()],
            }),
            _ => out.push(DiffChunk::Changed(ChangedChunk {
                a: vec![la.clone()],
                b: vec![lb.clone()],
                common: true,
                ..Default::default()
            })),
        }
    }
}

/// Inserted and deleted line counts between two texts.
pub(crate) fn count_changed_lines(old: &str, new: &str) -> (u32, u32) {
    let (a, b) = (Text::new(old.as_bytes()), Text::new(new.as_bytes()));
    let mut input = InternedInput::default();
    input.update_before(a.lines.iter().map(String::as_str));
    input.update_after(b.lines.iter().map(String::as_str));
    let diff = LineDiff::compute(Algorithm::Histogram, &input);
    (diff.count_additions(), diff.count_removals())
}
