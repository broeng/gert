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
//! The changes tested are 1-9 (the seeded ones); `GERT_TEST_CHANGES=10,11`
//! picks others. `GERT_TEST_USER` / `GERT_TEST_PASSWORD` default to alice.
//! Nothing is written to the server. As in the other dev tests, any URL but
//! `http://127.0.0.1:PORT` / `http://localhost:PORT` is refused.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use futures::{FutureExt, executor::block_on, future::BoxFuture};
use gerrit_api::{Auth, DiffParams, GerritClient, HttpClient, HttpError};
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

    let mut t = Tally::default();
    for number in changes_under_test() {
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
        "{} of them /COMMIT_MSG diffs (x 4 whitespace modes)",
        t.commit_msg_diffs
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

    let server = block_on(c.get_files(number, &rev.sha, base_ps)).expect("server files");
    let local = repo
        .file_list(&cmp)
        .expect("local files")
        .available()
        .expect("local file list");
    compare_file_lists(&label, &server, &local, t);

    // `/MERGE_LIST` is not made locally (local changes skip merges).
    for entry in server.iter().filter(|e| e.path != MERGE_LIST_PATH) {
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
