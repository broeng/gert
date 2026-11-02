//! Opt-in parity test: what `local-git` computes from a clone against what
//! the local dev Gerrit answers (`nix run .#dev-gerrit -- --seed`).
//!
//! It is `#[ignore]`d and needs the server's URL (and a `git` binary):
//!
//! ```text
//! GERT_TEST_GERRIT=http://127.0.0.1:8080 \
//!   cargo test -p local-git --test dev_gerrit -- --ignored --nocapture
//! ```
//!
//! The test clones the seeded project `demo/scheduler` over HTTP into a
//! temporary directory, fetches `refs/changes/*`, and for each change
//! compares, for every patch set against the parent and against the
//! previous patch set (when the parents agree, else local-git declines):
//!
//! - the file list, `/COMMIT_MSG` included (its text is rebuilt locally in
//!   Gerrit's format),
//! - every file's whole-file diff in each of the four whitespace modes:
//!   metadata, change type, header and chunk structure **exactly**; also
//!   for `/COMMIT_MSG`, whose chunks hold the synthetic header lines
//!   (parent, author, committer, dates) and so compare their exact text,
//!   and whose content type is compared too.
//!
//! Intraline edits are compared loosely: the test prints every chunk whose
//! marks differ from Gerrit's, reports how many are identical (all but a
//! handful on the seed data), and fails only if fewer than 80% are, because
//! the character diff is modelled on Gerrit's, not a copy.
//!
//! The changes tested are 1-9 (the seeded ones; `GERT_TEST_CHANGES=10,11`
//! picks others) and four probe changes for `/COMMIT_MSG` cases the seed
//! lacks: author and committer in the zones +0530, -0700, -0000 and +1245,
//! a parent subject wrapped over two lines, a message without a final
//! newline, two patch sets with the same `/COMMIT_MSG` text (one skip),
//! and a merge (its `Merge Of:` header, between two patch sets, since a
//! merge against its parent is Gerrit's auto-merge). The test pushes the
//! probes itself as alice, with the topic `local-git-parity`, the first
//! time it runs against a site; later runs find them by that topic. That
//! push is the only write, and it adds changes after the seeded ones, so
//! changes 1-9 keep their numbers. `GERT_TEST_USER` /
//! `GERT_TEST_PASSWORD` default to alice. As in the other dev tests, any
//! URL but `http://127.0.0.1:PORT` / `http://localhost:PORT` is refused.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use futures::{FutureExt, executor::block_on, future::BoxFuture};
use gerrit_api::{Auth, ChangeQuery, DiffParams, GerritClient, HttpClient, HttpError};
use local_git::{Local, Repo};
use model::{
    COMMIT_MSG_PATH, ChangeNumber, ChangedChunk, Diff, DiffChunk, FileEntry, MERGE_LIST_PATH,
    PatchSetNumber, Revision, Whitespace,
};

/// Blocking `ureq` behind the async trait; fine for tests.
struct Ureq(ureq::Agent);

impl HttpClient for Ureq {
    fn send(
        &self,
        req: http::Request<Vec<u8>>,
    ) -> BoxFuture<'static, Result<http::Response<Vec<u8>>, HttpError>> {
        let res = self
            .0
            .run(req)
            .map_err(|e| HttpError(e.to_string()))
            .and_then(|resp| {
                let (parts, mut body) = resp.into_parts();
                let bytes = body.read_to_vec().map_err(|e| HttpError(e.to_string()))?;
                Ok(http::Response::from_parts(parts, bytes))
            });
        async move { res }.boxed()
    }
}

fn base_url() -> String {
    let url = std::env::var("GERT_TEST_GERRIT")
        .expect("set GERT_TEST_GERRIT to the dev Gerrit URL, e.g. http://127.0.0.1:8080");
    let authority = url
        .strip_prefix("http://")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("");
    // `host` or `host:port`; no user info (`localhost@evil.com`).
    let (host, port) = authority.split_once(':').unwrap_or((authority, "0"));
    let local = matches!(host, "127.0.0.1" | "localhost")
        && !port.is_empty()
        && port.bytes().all(|b| b.is_ascii_digit());
    assert!(
        local,
        "refusing to run against {url:?}: GERT_TEST_GERRIT must be http://127.0.0.1:PORT \
         or http://localhost:PORT"
    );
    url
}

fn credentials() -> (String, String) {
    (
        std::env::var("GERT_TEST_USER").unwrap_or_else(|_| "alice".into()),
        std::env::var("GERT_TEST_PASSWORD").unwrap_or_else(|_| "alice-dev-password".into()),
    )
}

fn client() -> GerritClient {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let (username, password) = credentials();
    GerritClient::new(
        Arc::new(Ureq(agent)),
        &base_url(),
        Auth::Basic { username, password },
    )
    .expect("valid url")
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Clones `demo/scheduler` over HTTP and fetches every change ref.
fn clone_project(dir: &Path) {
    let (user, pass) = credentials();
    let authority = base_url().trim_start_matches("http://").to_owned();
    let url = format!(
        "http://{user}:{pass}@{}/a/demo/scheduler",
        authority.trim_end_matches('/')
    );
    let status = Command::new("git")
        .arg("clone")
        .arg("-q")
        .arg(&url)
        .arg(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .expect("run git clone");
    assert!(status.success(), "git clone failed");
    git(
        dir,
        &["fetch", "-q", "origin", "+refs/changes/*:refs/changes/*"],
    );
}

fn changes_under_test() -> Vec<ChangeNumber> {
    let list = std::env::var("GERT_TEST_CHANGES").unwrap_or_else(|_| "1,2,3,4,5,6,7,8,9".into());
    list.split(',')
        .map(|n| ChangeNumber(n.trim().parse().expect("change number")))
        .collect()
}

/// The topic of the probe changes, which finds them on later runs.
const PROBE_TOPIC: &str = "local-git-parity";
const PROBE_CHANGES: usize = 4;

/// Runs git in `dir` as alice with `env` and `stdin`, returning stdout.
fn git_with(dir: &Path, args: &[&str], env: &[(&str, &str)], stdin: Option<&str>) -> String {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Alice Anderson")
        .env("GIT_AUTHOR_EMAIL", "alice@example.com")
        .env("GIT_COMMITTER_NAME", "Alice Anderson")
        .env("GIT_COMMITTER_EMAIL", "alice@example.com")
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run git");
    let mut pipe = child.stdin.take().expect("stdin");
    pipe.write_all(stdin.unwrap_or("").as_bytes())
        .expect("write stdin");
    drop(pipe);
    let out = child.wait_with_output().expect("wait for git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Makes a commit without touching the work tree: the tree of `tree_of`
/// plus `files`, the message exactly as given (`commit-tree` adds no
/// newline), and fixed dates (`<unix seconds> <zone>`), so the probes are
/// the same commits on every site.
fn probe_commit(
    dir: &Path,
    parents: &[&str],
    tree_of: &str,
    files: &[(&str, &str)],
    message: &str,
    (author_date, committer_date): (&str, &str),
) -> String {
    let index = dir.join(".git").join("probe-index");
    let index = index.to_str().expect("utf8 path");
    let env = [
        ("GIT_INDEX_FILE", index),
        ("GIT_AUTHOR_DATE", author_date),
        ("GIT_COMMITTER_DATE", committer_date),
    ];
    git_with(dir, &["read-tree", tree_of], &env, None);
    for (path, content) in files {
        let blob = git_with(dir, &["hash-object", "-w", "--stdin"], &env, Some(content));
        let info = format!("100644,{blob},{path}");
        git_with(
            dir,
            &["update-index", "--add", "--cacheinfo", &info],
            &env,
            None,
        );
    }
    let tree = git_with(dir, &["write-tree"], &env, None);
    let mut args = vec!["commit-tree", tree.as_str()];
    for p in parents {
        args.extend(["-p", p]);
    }
    git_with(dir, &args, &env, Some(message))
}

/// The probe changes, pushed first if the server does not have them.
fn probe_changes(c: &GerritClient, dir: &Path) -> Vec<ChangeNumber> {
    let find = || {
        let q = format!("project:demo/scheduler topic:{PROBE_TOPIC}");
        let page = block_on(c.query_changes(&ChangeQuery::new(q))).expect("probe query");
        let mut numbers: Vec<ChangeNumber> = page.changes.iter().map(|ch| ch.number).collect();
        numbers.sort();
        numbers
    };
    let found = find();
    if found.len() == PROBE_CHANGES {
        return found;
    }
    assert!(
        found.is_empty(),
        "found {} of the {PROBE_CHANGES} probe changes; reset the dev Gerrit",
        found.len()
    );
    let id = |n: u64| format!("I{:040x}", 0x7061_7269_7479_0000_u64 + n);
    let base = git_with(dir, &["rev-parse", "origin/main"], &[], None);
    let a = probe_commit(
        dir,
        &[&base],
        &base,
        &[("probe/a.txt", "a\n")],
        &format!(
            "Parity probe with a subject\nthat wraps onto a second line\n\nChange-Id: {}\n",
            id(1)
        ),
        ("1774728000 +0530", "1774780200 -0700"),
    );
    // No final newline, and a zero offset written as -0000.
    let b_message = format!(
        "Parity probe without a final newline\n\nChange-Id: {}",
        id(2)
    );
    let b_dates = ("1700000000 -0000", "1700000000 +1245");
    let b1 = probe_commit(
        dir,
        &[&a],
        &a,
        &[("probe/b.txt", "one\n")],
        &b_message,
        b_dates,
    );
    // Another patch set with the same message and dates: the same text.
    let b2 = probe_commit(
        dir,
        &[&a],
        &a,
        &[("probe/b.txt", "two\n")],
        &b_message,
        b_dates,
    );
    let dates = ("1767225600 +0100", "1767225600 +0100");
    let side = probe_commit(
        dir,
        &[&base],
        &base,
        &[("probe/side.txt", "side\n")],
        &format!("Parity probe side commit\n\nChange-Id: {}\n", id(3)),
        dates,
    );
    let merge_files = [("probe/side.txt", "side\n")];
    let m1 = probe_commit(
        dir,
        &[&side, &b2],
        &b2,
        &merge_files,
        &format!("Parity probe merge\n\nChange-Id: {}\n", id(4)),
        dates,
    );
    let m2 = probe_commit(
        dir,
        &[&side, &b2],
        &b2,
        &merge_files,
        &format!(
            "Parity probe merge\n\nNow with a body.\n\nChange-Id: {}\n",
            id(4)
        ),
        dates,
    );
    for commit in [&b1, &b2, &m1, &m2] {
        let refspec = format!("{commit}:refs/for/main%topic={PROBE_TOPIC}");
        git_with(dir, &["push", "-q", "origin", &refspec], &[], None);
    }
    git(
        dir,
        &["fetch", "-q", "origin", "+refs/changes/*:refs/changes/*"],
    );
    let found = find();
    assert_eq!(found.len(), PROBE_CHANGES, "probe changes after the push");
    found
}

const MODES: [Whitespace; 4] = [
    Whitespace::IgnoreNone,
    Whitespace::IgnoreTrailing,
    Whitespace::IgnoreLeadingAndTrailing,
    Whitespace::IgnoreAll,
];

#[derive(Default)]
struct Tally {
    comparisons: usize,
    not_local: usize,
    diffs: usize,
    commit_msg_diffs: usize,
    /// `/COMMIT_MSG` diffs that Gerrit answered with one skip (equal texts).
    commit_msg_skips: usize,
    /// `/COMMIT_MSG` diffs whose text has a `Merge Of:` line.
    merge_headers: usize,
    changed_chunks: usize,
    intraline_chunks: usize,
    intraline_identical: usize,
    problems: Vec<String>,
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn local_git_matches_gerrit() {
    let c = client();
    let tmp = tempfile::tempdir().expect("temp dir");
    let dir = tmp.path().join("scheduler");
    clone_project(&dir);
    let repo = Repo::open(&dir).expect("open clone");

    let mut numbers = changes_under_test();
    for n in probe_changes(&c, &dir) {
        if !numbers.contains(&n) {
            numbers.push(n);
        }
    }

    let mut t = Tally::default();
    for number in numbers {
        let detail = block_on(c.get_change(number)).expect("change detail");
        for (i, rev) in detail.revisions.iter().enumerate() {
            let bases: Vec<Option<&Revision>> = if i == 0 {
                vec![None]
            } else {
                vec![None, Some(&detail.revisions[i - 1])]
            };
            for base in bases {
                compare_one(&c, &repo, number, rev, base, &mut t);
            }
        }
    }

    println!(
        "{} comparisons ({} declined by local-git), {} file diffs x 4 whitespace modes",
        t.comparisons, t.not_local, t.diffs
    );
    println!(
        "{} of them /COMMIT_MSG diffs (x 4 whitespace modes), {} with equal texts, {} of merges",
        t.commit_msg_diffs, t.commit_msg_skips, t.merge_headers
    );
    println!(
        "intraline: {} of {} chunks with edits on both sides carry exactly Gerrit's marks",
        t.intraline_identical, t.intraline_chunks
    );
    println!("{} changed chunks in total", t.changed_chunks);
    for p in &t.problems {
        println!("MISMATCH {p}");
    }
    assert!(t.problems.is_empty(), "{} mismatches", t.problems.len());
    assert!(
        t.intraline_identical * 10 >= t.intraline_chunks * 8,
        "intraline marks are too far from Gerrit's: {} of {}",
        t.intraline_identical,
        t.intraline_chunks
    );
    assert!(t.diffs > 0, "nothing was compared");
    assert!(t.commit_msg_diffs > 0, "no /COMMIT_MSG was compared");
    assert!(
        t.commit_msg_skips > 0,
        "no equal /COMMIT_MSG texts compared"
    );
    assert!(t.merge_headers > 0, "no merge's /COMMIT_MSG compared");
}

fn compare_one(
    c: &GerritClient,
    repo: &Repo,
    number: ChangeNumber,
    rev: &Revision,
    base: Option<&Revision>,
    t: &mut Tally,
) {
    let label = format!(
        "change {number} ps{} vs {}",
        rev.number,
        base.map_or("parent".to_owned(), |b| format!("ps{}", b.number))
    );
    t.comparisons += 1;
    let base_sha = base.map(|b| b.sha.clone());
    let cmp = match repo.resolve(&rev.sha, base_sha.as_ref()).expect("resolve") {
        Local::Available(cmp) => cmp,
        Local::NotLocal(why) => {
            println!("{label}: not local ({why})");
            t.not_local += 1;
            return;
        }
    };
    let base_ps: Option<PatchSetNumber> = base.map(|b| b.number);

    let mut server = block_on(c.get_files(number, &rev.sha, base_ps)).expect("server files");
    // Gerrit lists `/MERGE_LIST` for a merge; local-git does not make it
    // (local changes skip merges).
    server.retain(|e| e.path != MERGE_LIST_PATH);
    let local = repo
        .file_list(&cmp)
        .expect("local files")
        .available()
        .expect("local file list");
    compare_file_lists(&label, &server, &local, t);

    for entry in &server {
        t.diffs += 1;
        if entry.path == COMMIT_MSG_PATH {
            t.commit_msg_diffs += 1;
        }
        for ws in MODES {
            let params = DiffParams {
                base: base_ps,
                intraline: true,
                whitespace: ws.clone(),
            };
            let s = block_on(c.get_diff(number, &rev.sha, &entry.path, &params))
                .unwrap_or_else(|e| panic!("{label} {}: {e}", entry.path));
            let l = repo
                .diff(&cmp, &entry.path, entry.old_path.as_deref(), &ws)
                .expect("local diff");
            let Local::Available(l) = l else {
                t.problems.push(format!(
                    "{label} {} [{}]: local-git declined",
                    entry.path,
                    ws.as_wire()
                ));
                continue;
            };
            let what = format!("{label} {} [{}]", entry.path, ws.as_wire());
            if entry.path == COMMIT_MSG_PATH && ws == Whitespace::IgnoreNone {
                if matches!(s.chunks.as_slice(), [DiffChunk::Skip { .. }]) {
                    t.commit_msg_skips += 1;
                }
                let merge_of = |c: &DiffChunk| match c {
                    DiffChunk::Common { lines } => lines.iter().any(|l| l.starts_with("Merge Of:")),
                    DiffChunk::Changed(c) => c.b.iter().any(|l| l.starts_with("Merge Of:")),
                    DiffChunk::Skip { .. } => false,
                };
                if s.chunks.iter().any(merge_of) {
                    t.merge_headers += 1;
                }
            }
            compare_diff(&what, &s, &l, t);
        }
    }
}

fn compare_file_lists(label: &str, server: &[FileEntry], local: &[FileEntry], t: &mut Tally) {
    let paths = |l: &[FileEntry]| l.iter().map(|e| e.path.clone()).collect::<Vec<_>>();
    if paths(server) != paths(local) {
        t.problems.push(format!(
            "{label}: file paths differ\n  server: {:?}\n  local:  {:?}",
            paths(server),
            paths(local)
        ));
        return;
    }
    for (s, l) in server.iter().zip(local) {
        if s != l {
            t.problems.push(format!(
                "{label} {}: file entry differs\n  server: {s:?}\n  local:  {l:?}",
                s.path
            ));
        }
    }
}

/// Compact rendering of the chunk structure, for messages.
fn render(d: &Diff) -> String {
    let mut out = String::new();
    for c in &d.chunks {
        match c {
            DiffChunk::Common { lines } => out.push_str(&format!("  = {lines:?}\n")),
            DiffChunk::Changed(c) => out.push_str(&format!(
                "  ! common={} rebase={} a={:?} b={:?}\n",
                c.common, c.due_to_rebase, c.a, c.b
            )),
            DiffChunk::Skip { lines } => out.push_str(&format!("  skip {lines}\n")),
        }
    }
    out
}

fn structure(d: &Diff) -> Vec<DiffChunk> {
    d.chunks
        .iter()
        .map(|c| match c {
            DiffChunk::Changed(c) => DiffChunk::Changed(ChangedChunk {
                edits_a: Vec::new(),
                edits_b: Vec::new(),
                ..c.clone()
            }),
            other => other.clone(),
        })
        .collect()
}

fn compare_diff(what: &str, server: &Diff, local: &Diff, t: &mut Tally) {
    let mut problems = Vec::new();
    // Local diffs of files carry no content type; `/COMMIT_MSG` has
    // Gerrit's fixed one.
    let commit_msg = server
        .meta_b
        .as_ref()
        .is_some_and(|m| m.name == COMMIT_MSG_PATH);
    let meta = |m: &Option<model::DiffFileMeta>| {
        m.as_ref().map(|m| {
            (
                m.name.clone(),
                m.lines,
                commit_msg.then(|| m.content_type.clone()),
            )
        })
    };
    if meta(&server.meta_a) != meta(&local.meta_a) {
        problems.push(format!(
            "meta_a {:?} vs local {:?}",
            server.meta_a, local.meta_a
        ));
    }
    if meta(&server.meta_b) != meta(&local.meta_b) {
        problems.push(format!(
            "meta_b {:?} vs local {:?}",
            server.meta_b, local.meta_b
        ));
    }
    if server.change_type != local.change_type {
        problems.push(format!(
            "change_type {:?} vs local {:?}",
            server.change_type, local.change_type
        ));
    }
    if server.binary != local.binary {
        problems.push(format!(
            "binary {} vs local {}",
            server.binary, local.binary
        ));
    }
    if server.diff_header != local.diff_header {
        problems.push(format!(
            "diff_header {:?} vs local {:?}",
            server.diff_header, local.diff_header
        ));
    }
    if server.intraline_status != local.intraline_status {
        problems.push(format!(
            "intraline_status {:?} vs local {:?}",
            server.intraline_status, local.intraline_status
        ));
    }
    if structure(server) != structure(local) {
        problems.push(format!(
            "chunks differ\n server:\n{}\n local:\n{}",
            render(server),
            render(local)
        ));
    } else {
        // Same structure: compare the marks loosely.
        for (s, l) in server.chunks.iter().zip(&local.chunks) {
            let (DiffChunk::Changed(s), DiffChunk::Changed(l)) = (s, l) else {
                continue;
            };
            t.changed_chunks += 1;
            if s.a.is_empty() || s.b.is_empty() {
                continue;
            }
            t.intraline_chunks += 1;
            if s.edits_a == l.edits_a && s.edits_b == l.edits_b {
                t.intraline_identical += 1;
            } else {
                println!(
                    "intraline differs in {what}\n  a={:?}\n  b={:?}\n  server a={:?} b={:?}\n  local  a={:?} b={:?}",
                    s.a,
                    s.b,
                    s.marks_a(),
                    s.marks_b(),
                    l.marks_a(),
                    l.marks_b()
                );
                println!(
                    "  raw server a={:?} b={:?}\n  raw local  a={:?} b={:?}",
                    s.edits_a, s.edits_b, l.edits_a, l.edits_b
                );
            }
        }
    }
    if !problems.is_empty() {
        t.problems.push(format!("{what}: {}", problems.join("; ")));
    }
}
