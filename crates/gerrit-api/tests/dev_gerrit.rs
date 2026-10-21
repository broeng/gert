//! Opt-in tests against the local dev Gerrit seeded by `dev/seed.sh`
//! (`nix run .#dev-gerrit`, then `nix run .#dev-gerrit-seed`).
//!
//! They are `#[ignore]`d and need the server's URL:
//!
//! ```text
//! GERT_TEST_GERRIT=http://127.0.0.1:8080 \
//!   cargo test -p gerrit-api --test dev_gerrit -- --ignored --nocapture
//! ```
//!
//! `GERT_TEST_USER` / `GERT_TEST_PASSWORD` default to the seeded
//! `alice` / `alice-dev-password`.
//!
//! Most tests only read the seeded data. The write tests (drafts, reviews,
//! reviewed marks) create a throwaway change of their own each time, so the
//! seeded changes stay as the read tests expect them. Per `CLAUDE.md`, writes
//! may only go to a local dev Gerrit: every test refuses to run unless
//! `GERT_TEST_GERRIT` points at 127.0.0.1 or localhost.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use base64::Engine as _;

use futures::{FutureExt, executor::block_on, future::BoxFuture};
use gerrit_api::{Auth, ChangeQuery, DiffParams, Error, GerritClient, HttpClient, HttpError};
use model::{
    ChangeNumber, ChangeStatus, DiffChangeType, DiffChunk, FileStatus, PatchSetNumber, RevisionId,
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
    // The tests write. Never to anything but a local dev server.
    let authority = url
        .strip_prefix("http://")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("");
    // `host` or `host:port`; no user info (`localhost@evil.com`).
    let (host, port) = authority.split_once(':').unwrap_or((authority, "0"));
    let local = matches!(host, "127.0.0.1" | "localhost")
        && port.bytes().all(|b| b.is_ascii_digit())
        && !port.is_empty();
    assert!(
        local,
        "refusing to run against {url:?}: GERT_TEST_GERRIT must be http://127.0.0.1:PORT \
         or http://localhost:PORT"
    );
    url
}

fn client_with(auth: Auth) -> GerritClient {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    GerritClient::new(Arc::new(Ureq(agent)), &base_url(), auth).expect("valid url")
}

fn anon() -> GerritClient {
    client_with(Auth::Anonymous)
}

fn alice() -> GerritClient {
    client_with(Auth::Basic {
        username: std::env::var("GERT_TEST_USER").unwrap_or_else(|_| "alice".into()),
        password: std::env::var("GERT_TEST_PASSWORD")
            .unwrap_or_else(|_| "alice-dev-password".into()),
    })
}

/// The number of the seeded change with this subject.
fn find(c: &GerritClient, subject: &str) -> ChangeNumber {
    let q = format!("project:demo/scheduler subject:\"{subject}\"");
    let page = block_on(c.query_changes(&ChangeQuery::new(q))).expect("query");
    assert_eq!(page.changes.len(), 1, "change {subject:?}");
    page.changes[0].number
}

fn current_sha(c: &GerritClient, n: ChangeNumber) -> RevisionId {
    let d = block_on(c.get_change(n)).expect("detail");
    d.current_revision.expect("current revision")
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn anonymous_and_authenticated() {
    let anon = anon();
    let version = block_on(anon.server_version()).expect("version");
    println!("server version {version}");
    assert!(version.starts_with("3."), "Gerrit 3.x expected: {version}");

    // Public data is readable anonymously; anything about "self" is not.
    let page = block_on(anon.query_changes(&ChangeQuery::new("project:demo/scheduler")))
        .expect("anonymous query");
    assert!(page.changes.len() >= 9);
    assert!(matches!(
        block_on(anon.account_self()),
        Err(Error::Unauthorized)
    ));
    assert!(matches!(
        block_on(anon.get_diff_preferences()),
        Err(Error::Unauthorized)
    ));

    let me = block_on(alice().account_self()).expect("alice");
    assert_eq!(me.username.as_deref(), Some("alice"));

    let wrong = client_with(Auth::Basic {
        username: "alice".into(),
        password: "wrong".into(),
    });
    assert!(matches!(
        block_on(wrong.account_self()),
        Err(Error::Unauthorized)
    ));
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn query_lists_the_seeded_changes() {
    let c = alice();
    let mut all = block_on(c.query_changes(&ChangeQuery::new("project:demo/scheduler").limit(50)))
        .expect("query");
    // The write tests add changes of their own; the seeded ones are 1-9.
    all.changes.retain(|ch| ch.number.0 <= 9);
    assert_eq!(all.changes.len(), 9);
    assert!(!all.more_changes);
    let by_status = |s: ChangeStatus| all.changes.iter().filter(|ch| ch.status == s).count();
    assert_eq!(by_status(ChangeStatus::Merged), 2);
    assert_eq!(by_status(ChangeStatus::Abandoned), 1);
    assert_eq!(by_status(ChangeStatus::New), 6);
    assert_eq!(
        all.changes.iter().filter(|ch| ch.work_in_progress).count(),
        1
    );

    let a = all
        .changes
        .iter()
        .find(|ch| ch.subject.starts_with("Retry failed jobs"))
        .expect("change A");
    assert_eq!(a.topic.as_deref(), Some("retry-logic"));
    let mut hashtags = a.hashtags.clone();
    hashtags.sort(); // a set: the server's order varies
    assert_eq!(hashtags, ["backend", "perf"]);
    assert_eq!(a.owner.username.as_deref(), Some("alice"));
    assert!(a.labels.contains_key("Code-Review"));
    assert!(a.labels.contains_key("Verified"));
    assert!(a.insertions > 0 && a.deletions > 0);

    // Paging.
    let first = block_on(c.query_changes(&ChangeQuery::new("project:demo/scheduler").limit(4)))
        .expect("page");
    assert_eq!(first.changes.len(), 4);
    assert!(first.more_changes);

    let own = block_on(c.query_changes(&ChangeQuery::new("is:open owner:self"))).expect("self");
    assert!(
        own.changes
            .iter()
            .all(|ch| ch.owner.username.as_deref() == Some("alice"))
    );
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn change_detail_with_patch_sets_votes_and_reviewers() {
    let c = alice();
    let n = find(&c, "Retry failed jobs with exponential backoff");
    let d = block_on(c.get_change(n)).expect("detail");
    assert_eq!(d.revisions.len(), 3);
    assert_eq!(d.current_patch_set, Some(PatchSetNumber(3)));
    assert!(d.revisions.iter().all(|r| r.commit.is_some()));
    assert!(d.commit_message.as_deref().unwrap().contains("Change-Id:"));
    let reviewer = |name: &str| {
        d.reviewers
            .iter()
            .any(|a| a.username.as_deref() == Some(name))
    };
    assert!(reviewer("bob"));
    // Carol was CC'd and then voted, which makes her a reviewer.
    assert!(reviewer("carol"));

    let vote = |label: &str, user: &str| {
        d.votes[label]
            .iter()
            .find(|v| v.account.username.as_deref() == Some(user))
            .map(|v| v.value)
    };
    assert_eq!(vote("Code-Review", "bob"), Some(1));
    assert_eq!(vote("Code-Review", "carol"), Some(-1));
    assert_eq!(vote("Verified", "ci-bot"), Some(1));
    assert!(!d.messages.is_empty());
    assert!(d.summary.total_comment_count > 0);
    assert!(d.summary.unresolved_comment_count > 0);
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn files_with_and_without_base() {
    let c = alice();
    let n = find(&c, "Retry failed jobs with exponential backoff");
    let ps3 = current_sha(&c, n);

    let files = block_on(c.get_files(n, &ps3, None)).expect("files");
    let get = |p: &str| {
        files
            .iter()
            .find(|f| f.path == p)
            .unwrap_or_else(|| panic!("{p}"))
    };
    assert_eq!(files[0].path, "/COMMIT_MSG");
    assert_eq!(get("src/retry.rs").status, FileStatus::Added);
    assert_eq!(get("src/old.rs").status, FileStatus::Deleted);
    let renamed = get("src/helpers.rs");
    assert_eq!(renamed.status, FileStatus::Renamed);
    assert_eq!(renamed.old_path.as_deref(), Some("src/util.rs"));
    assert_eq!(get("src/queue.rs").status, FileStatus::Modified);
    assert!(get("assets/logo.png").binary);
    assert!(!get("data/big.txt").binary);

    // Against patch set 2 only what changed in patch set 3 remains.
    let delta = block_on(c.get_files(n, &ps3, Some(PatchSetNumber(2)))).expect("delta");
    let paths: Vec<_> = delta.iter().map(|f| f.path.as_str()).collect();
    assert!(paths.contains(&"data/big.txt"), "{paths:?}");
    assert!(paths.contains(&"src/helpers.rs"), "{paths:?}");
    assert!(!paths.contains(&"src/retry.rs"), "{paths:?}");
}

/// How many `skip` chunks a raw anonymous diff request with `context=ctx`
/// returns (our client never sends `context`).
fn raw_skip_chunks(n: ChangeNumber, rev: &RevisionId, path: &str, ctx: &str) -> usize {
    let url = format!(
        "{}/changes/{n}/revisions/{rev}/files/{path}/diff?context={ctx}",
        base_url().trim_end_matches('/')
    );
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = agent.get(&url).call().expect("raw diff request");
    assert_eq!(resp.status().as_u16(), 200, "{url}");
    let body = resp.body_mut().read_to_string().expect("body");
    let json = body.strip_prefix(")]}'").expect("XSSI guard").trim_start();
    let v: serde_json::Value = serde_json::from_str(json).expect("json");
    v["content"]
        .as_array()
        .expect("content")
        .iter()
        .filter(|c| c.get("skip").is_some())
        .count()
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn diffs_are_whole_files_whatever_the_context() {
    let c = alice();
    let n = find(&c, "Retry failed jobs with exponential backoff");
    let rev = current_sha(&c, n);

    let diff = |path: &str, params: &DiffParams| {
        block_on(c.get_diff(n, &rev, path, params)).unwrap_or_else(|e| panic!("{path}: {e}"))
    };

    let q = diff("src/queue.rs", &DiffParams::default());
    assert_eq!(q.change_type, Some(DiffChangeType::Modified));
    assert!(
        q.chunks
            .iter()
            .any(|ch| matches!(ch, DiffChunk::Changed(_)))
    );
    assert_eq!(q.a_len(), q.meta_a.as_ref().unwrap().lines);
    assert_eq!(q.b_len(), q.meta_b.as_ref().unwrap().lines);

    let r = diff("src/helpers.rs", &DiffParams::default());
    assert_eq!(r.change_type, Some(DiffChangeType::Renamed));
    let bin = diff("assets/logo.png", &DiffParams::default());
    assert!(bin.binary);
    let del = diff("src/old.rs", &DiffParams::default());
    assert_eq!(del.change_type, Some(DiffChangeType::Deleted));

    // data/big.txt has 400 lines with edits at lines 20 and 380. Gerrit 3.14
    // ignores `context` and always returns the whole file, never a `skip`
    // chunk, so the client folds unchanged runs itself (so our requests
    // do not send `context` at all). A future Gerrit that honors it would
    // still pass: skips are only tolerated when `context=N` was sent.
    let d = diff("data/big.txt", &DiffParams::default());
    assert!(
        !d.chunks
            .iter()
            .any(|ch| matches!(ch, DiffChunk::Skip { .. })),
        "a request without context must return the whole file"
    );
    assert_eq!(d.b_len(), d.meta_b.as_ref().unwrap().lines);
    assert_eq!(d.a_len(), d.meta_a.as_ref().unwrap().lines);
    let whole = d.chunks.len();
    for ctx in ["0", "3", "10", "ALL"] {
        let skips = raw_skip_chunks(n, &rev, "data%2Fbig.txt", ctx);
        println!("context={ctx}: {skips} skip chunks (request without context: {whole} chunks)");
        if ctx == "ALL" {
            assert_eq!(skips, 0, "context=ALL must not skip anything");
        }
        if skips == 0 {
            continue;
        }
        println!(
            "this Gerrit folds server-side for context={ctx}; the client copes with `skip` chunks"
        );
    }

    // Against another patch set.
    let delta = diff(
        "data/big.txt",
        &DiffParams {
            base: Some(PatchSetNumber(2)),
            ..DiffParams::default()
        },
    );
    assert!(
        delta
            .chunks
            .iter()
            .any(|ch| matches!(ch, DiffChunk::Changed(_)))
    );
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn reviewed_related_and_preferences() {
    let c = alice();
    let n = find(&c, "Retry failed jobs with exponential backoff");
    let rev = current_sha(&c, n);

    let reviewed = block_on(c.get_reviewed_files(n, &rev)).expect("reviewed");
    println!("reviewed: {reviewed:?}");
    // Anonymous sessions make no request and see none.
    assert!(
        block_on(anon().get_reviewed_files(n, &rev))
            .expect("anon reviewed")
            .is_empty()
    );

    let mid = find(&c, "Add a Config type");
    let mid_rev = current_sha(&c, mid);
    let related = block_on(c.get_related(mid, &mid_rev)).expect("related");
    let numbers: Vec<_> = related.iter().map(|r| r.number).collect();
    assert_eq!(related.len(), 3, "{numbers:?}");
    assert!(numbers.contains(&mid));
    assert!(related.iter().all(|r| !r.is_outdated()));
    // A change outside any series.
    assert!(
        block_on(c.get_related(n, &rev))
            .expect("related")
            .is_empty()
    );

    let prefs = block_on(c.get_diff_preferences()).expect("prefs");
    println!("{prefs:?}");
    assert!(prefs.context.is_some());
}

// ---- comments, drafts, reviews ----

fn user(name: &str) -> GerritClient {
    client_with(Auth::Basic {
        username: name.into(),
        password: format!("{name}-dev-password"),
    })
}

/// A raw request as `alice`, for what `GerritClient` has no call for
/// (creating changes). Returns status and body.
fn raw(method: &str, path: &str, body: Option<serde_json::Value>) -> (u16, String) {
    use base64::Engine as _;
    let token = base64::engine::general_purpose::STANDARD.encode("alice:alice-dev-password");
    let mut req = http::Request::builder()
        .method(method)
        .uri(format!("{}/a{path}", base_url()))
        .header("authorization", format!("Basic {token}"));
    let bytes = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            serde_json::to_vec(&b).unwrap()
        }
        None => Vec::new(),
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let resp = block_on(Ureq(agent).send(req.body(bytes).unwrap())).expect("raw request");
    (
        resp.status().as_u16(),
        String::from_utf8_lossy(resp.body()).into_owned(),
    )
}

/// A new change by alice with one added file `probe.txt` (three lines), so
/// that tests can comment and vote without touching the seeded data.
/// Returns its number and the SHA of its (only) patch set.
fn new_change(name: &str) -> (ChangeNumber, RevisionId) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let (code, body) = raw(
        "POST",
        "/changes/",
        Some(serde_json::json!({
            "project": "demo/scheduler", "branch": "main", "status": "NEW",
            "subject": format!("Dev test {name} {stamp}"),
        })),
    );
    assert_eq!(code, 201, "{body}");
    let info: serde_json::Value = serde_json::from_str(body.trim_start_matches(")]}'")).unwrap();
    let n = info["_number"].as_u64().expect("number");
    let content =
        base64::engine::general_purpose::STANDARD.encode("line one\nline two\nline three\n");
    let (code, body) = raw(
        "PUT",
        &format!("/changes/{n}/edit/probe.txt"),
        Some(serde_json::json!({"binary_content": format!("data:text/plain;base64,{content}")})),
    );
    assert_eq!(code, 204, "{body}");
    let (code, body) = raw(
        "POST",
        &format!("/changes/{n}/edit:publish"),
        Some(serde_json::json!({})),
    );
    assert_eq!(code, 204, "{body}");
    let n = ChangeNumber(n);
    (n, current_sha(&alice(), n))
}

fn line_draft(message: &str) -> model::DraftInput {
    model::DraftInput {
        path: "probe.txt".into(),
        side: model::Side::Revision,
        parent: None,
        location: model::Location::Line(2),
        in_reply_to: None,
        message: message.into(),
        unresolved: true,
    }
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn seeded_comments_become_the_expected_threads() {
    use model::{Entry, Location, Side, build_threads};
    let c = alice();
    let n = find(&c, "Retry failed jobs with exponential backoff");
    let comments = block_on(c.get_comments(n)).expect("comments");
    println!("{} published comments", comments.len());
    assert!(comments.len() >= 9, "seeded: 9 published comments");
    assert!(
        comments
            .iter()
            .all(|c| !c.id.is_empty() && !c.path.is_empty())
    );
    assert!(
        comments
            .iter()
            .all(|c| c.author.is_some() && c.commit_id.is_some())
    );
    // Special files sort first.
    assert_eq!(comments[0].path, "/COMMIT_MSG");

    let drafts = block_on(c.get_drafts(n)).expect("drafts");
    println!("{} drafts", drafts.len());
    assert!(drafts.len() >= 4, "alice has 4 seeded drafts");
    assert!(drafts.iter().all(|d| d.author.is_none()));
    // Anonymous sessions have none and make no request.
    assert!(block_on(anon().get_drafts(n)).unwrap().is_empty());
    // Gerrit 3.14 has no robot comments endpoint: 404.
    match block_on(c.get_robot_comments(n)) {
        Ok(robots) => assert!(robots.is_empty()),
        Err(e) => assert!(matches!(e, Error::NotFound), "{e}"),
    }

    // Build threads from comments only (drafts are alice's, local).
    let threads = build_threads(&comments, &[]);
    let by_message = |needle: &str| {
        threads
            .iter()
            .find(|t| t.entries.iter().any(|e| e.message().contains(needle)))
            .unwrap_or_else(|| panic!("no thread with {needle:?}"))
    };
    // Commit message comment, unresolved.
    let t = by_message("Should this mention the backof");
    assert_eq!(t.path, "/COMMIT_MSG");
    assert!(t.is_unresolved() && t.entries.len() == 1);
    // File-level comment.
    assert_eq!(
        by_message("Is the new logo final?").location,
        Location::File
    );
    // PARENT side with a reply, resolved by the reply.
    let t = by_message("Not a change from you");
    assert_eq!(t.side, Side::Parent);
    assert_eq!(t.entries.len(), 2);
    assert!(!t.is_unresolved());
    // Range comment.
    assert!(matches!(
        by_message("Cap the doubled delay").location,
        Location::Range(_)
    ));
    // A thread whose reply was made on a later patch set stays one thread
    // anchored at its root.
    let t = by_message("Nit: name the one hour limit");
    assert_eq!(t.entries.len(), 2);
    assert_eq!(t.patch_set, PatchSetNumber(2));
    assert!(!t.is_unresolved());
    let Entry::Published(last) = t.entries.last().unwrap() else {
        panic!("published");
    };
    assert_eq!(last.patch_set, PatchSetNumber(3));
    assert!(threads.iter().all(|t| !t.orphaned));

    // Counts for patch set 2 against its parent: the file list's numbers.
    // A thread belongs to the patch set of its root, so the reply that was
    // made on patch set 3 counts with it, and patch set 1's threads are
    // "elsewhere".
    let detail = block_on(c.get_change(n)).expect("detail");
    let rev2 = detail
        .revision(PatchSetNumber(2))
        .expect("ps 2")
        .sha
        .clone();
    let files = block_on(c.get_files(n, &rev2, None)).expect("files");
    let pair = model::DiffPair {
        base: None,
        target: PatchSetNumber(2),
    };
    let counts = model::comment_counts(&threads, pair, &files);
    println!("{counts:#?}");
    let q = counts["src/queue.rs"];
    assert_eq!((q.comments, q.unresolved), (2, 0));
    assert!(q.elsewhere >= 3, "patch set 1's three threads: {q:?}");
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn draft_create_update_delete() {
    let c = alice();
    let (n, rev) = new_change("drafts");
    assert!(block_on(c.get_drafts(n)).unwrap().is_empty());

    let created = block_on(c.create_draft(n, &rev, &line_draft("first version"))).expect("create");
    println!("created {created:?}");
    assert!(!created.id.is_empty());

    // A range draft and a file-level draft.
    let mut ranged = line_draft("a range");
    ranged.location = model::Location::Range(model::CommentRange {
        start_line: 1,
        start_character: 2,
        end_line: 2,
        end_character: 4,
    });
    let range = ranged.location;
    let ranged = block_on(c.create_draft(n, &rev, &ranged)).expect("range draft");
    let mut file_level = line_draft("on the whole file");
    file_level.location = model::Location::File;
    file_level.unresolved = false;
    let file_level = block_on(c.create_draft(n, &rev, &file_level)).expect("file draft");

    // The server's list has what we sent, with the patch set and commit
    // the answer to the PUT leaves out.
    // A patch-set level draft (no side, no line).
    let mut psl = line_draft("overall");
    psl.path = "/PATCHSET_LEVEL".into();
    psl.location = model::Location::File;
    psl.unresolved = false;
    let psl = block_on(c.create_draft(n, &rev, &psl)).expect("patch-set level draft");

    let listed = block_on(c.get_drafts(n)).unwrap();
    assert_eq!(listed.len(), 4);
    assert!(listed.iter().all(|d| d.commit_id.as_ref() == Some(&rev)));
    assert!(
        listed
            .iter()
            .all(|d| d.patch_set == PatchSetNumber(2) && d.author.is_none())
    );
    let find_draft = |id: &str| listed.iter().find(|d| d.id == id).expect("listed");
    let first = find_draft(&created.id);
    assert_eq!(first.path, "probe.txt");
    assert_eq!(first.location, model::Location::Line(2));
    assert!(first.unresolved);
    assert_eq!(find_draft(&ranged.id).location, range);
    assert_eq!(find_draft(&file_level.id).location, model::Location::File);
    assert!(!find_draft(&file_level.id).unresolved);

    // Update: new text, resolved.
    let mut changed = line_draft("second version with ünïcode and \"quotes\"\nand a new line");
    changed.unresolved = false;
    let updated = block_on(c.update_draft(n, &rev, &created.id, &changed)).expect("update");
    assert_eq!(updated.id, created.id);
    let listed = block_on(c.get_drafts(n)).unwrap();
    let again = listed.iter().find(|d| d.id == created.id).unwrap();
    assert_eq!(again.message, changed.message);
    assert!(!again.unresolved);

    // Delete, twice: the second says not found.
    block_on(c.delete_draft(n, &rev, &created.id)).expect("delete");
    assert!(matches!(
        block_on(c.delete_draft(n, &rev, &created.id)),
        Err(Error::NotFound)
    ));
    assert!(matches!(
        block_on(c.update_draft(n, &rev, &created.id, &changed)),
        Err(Error::NotFound)
    ));
    assert_eq!(block_on(c.get_drafts(n)).unwrap().len(), 3);
    // Other users do not see the drafts.
    assert!(block_on(user("bob").get_drafts(n)).unwrap().is_empty());
    block_on(c.delete_draft(n, &rev, &ranged.id)).unwrap();
    block_on(c.delete_draft(n, &rev, &file_level.id)).unwrap();
    block_on(c.delete_draft(n, &rev, &psl.id)).unwrap();
    assert!(block_on(c.get_drafts(n)).unwrap().is_empty());
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn draft_errors_are_typed() {
    let c = alice();
    let (n, rev) = new_change("draft-errors");
    // A patch set (revision) that does not exist.
    let r = block_on(c.create_draft(n, &RevisionId::from("0".repeat(40)), &line_draft("x")));
    println!("unknown revision: {r:?}");
    assert!(matches!(
        r,
        Err(Error::NotFound) | Err(Error::BadRequest { .. })
    ));
    // A change that does not exist.
    let r = block_on(c.create_draft(ChangeNumber(99999), &rev, &line_draft("x")));
    println!("unknown change: {r:?}");
    assert!(matches!(r, Err(Error::NotFound)));
    // The server accepts a file that is not in the patch set and any line
    // number (the comment just never shows), but not an empty message.
    let mut odd = line_draft("x");
    odd.path = "no/such/file.rs".into();
    odd.location = model::Location::Line(999);
    let d = block_on(c.create_draft(n, &rev, &odd)).expect("accepted");
    block_on(c.delete_draft(n, &rev, &d.id)).unwrap();
    let r = block_on(c.create_draft(n, &rev, &line_draft("")));
    println!("empty message: {r:?}");
    assert!(matches!(&r, Err(Error::BadRequest { message }) if message.contains("non-empty")));
    // Anonymous: refused before any request.
    assert!(matches!(
        block_on(anon().create_draft(n, &rev, &line_draft("x"))),
        Err(Error::Unauthorized)
    ));
    // Wrong password: 401.
    let wrong = client_with(Auth::Basic {
        username: "alice".into(),
        password: "wrong".into(),
    });
    assert!(matches!(
        block_on(wrong.create_draft(n, &rev, &line_draft("x"))),
        Err(Error::Unauthorized)
    ));
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn publish_a_review_with_a_vote_and_drafts() {
    use model::{AttentionSetInput, ReviewInput};
    let alice_c = alice();
    let bob = user("bob");
    let (n, rev) = new_change("review");

    // Bob drafts a comment, then publishes it with a message and a vote.
    block_on(bob.create_draft(n, &rev, &line_draft("Please rename this line"))).expect("draft");
    let mut review = ReviewInput::with_message("Looks mostly fine.");
    review.labels.insert("Code-Review".into(), 1);
    // Ask bob's own turn to be removed and alice's to be added.
    let alice_id = block_on(alice_c.account_self()).unwrap().id;
    let bob_id = block_on(bob.account_self()).unwrap().id;
    review.add_to_attention_set = vec![AttentionSetInput {
        user: alice_id,
        reason: "please have a look".into(),
    }];
    review.remove_from_attention_set = vec![AttentionSetInput {
        user: bob_id,
        reason: "done reviewing".into(),
    }];
    let result = block_on(bob.post_review(n, &rev, &review)).expect("review");
    println!("{result:?}");
    assert_eq!(result.labels.get("Code-Review"), Some(&1));
    assert!(result.error.is_none());

    // The draft became a published comment; no draft is left.
    assert!(block_on(bob.get_drafts(n)).unwrap().is_empty());
    let comments = block_on(alice_c.get_comments(n)).unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].message, "Please rename this line");
    assert!(
        comments[0]
            .author
            .as_ref()
            .is_some_and(|a| a.username.as_deref() == Some("bob"))
    );
    assert!(comments[0].unresolved);
    assert!(comments[0].change_message_id.is_some());

    // The vote and message are visible.
    let detail = block_on(alice_c.get_change(n)).unwrap();
    let votes = &detail.votes["Code-Review"];
    assert!(
        votes
            .iter()
            .any(|v| v.value == 1 && v.account.username.as_deref() == Some("bob"))
    );
    assert!(
        detail
            .messages
            .iter()
            .any(|m| m.message.contains("Looks mostly fine."))
    );
    // Bob may vote on this label; permitted values come with the detail.
    let bobs = block_on(bob.get_change(n)).unwrap();
    assert!(bobs.permitted_labels["Code-Review"].contains(&2));
    assert!(
        bobs.label_values["Code-Review"]
            .iter()
            .any(|v| v.value == -2 && !v.description.is_empty())
    );
    assert!(bobs.permitted_labels["Verified"].contains(&-1));
    // Anonymous sessions have no permitted labels.
    assert!(
        block_on(anon().get_change(n))
            .unwrap()
            .permitted_labels
            .is_empty()
    );

    // Alice replies to the thread with a draft; both end up in one thread.
    let mut reply = line_draft("Done");
    reply.in_reply_to = Some(comments[0].id.clone());
    reply.unresolved = false;
    block_on(alice_c.create_draft(n, &rev, &reply)).unwrap();
    let published =
        block_on(alice_c.post_review(n, &rev, &ReviewInput::with_message("Fixed."))).unwrap();
    assert!(published.error.is_none());
    let comments = block_on(alice_c.get_comments(n)).unwrap();
    let threads = model::build_threads(&comments, &[]);
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].entries.len(), 2);
    assert!(!threads[0].is_unresolved());
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn review_errors_carry_the_servers_message() {
    use model::ReviewInput;
    let c = alice();
    let (n, rev) = new_change("review-errors");

    // Gerrit silently ignores a vote value the label does not have and a
    // label that does not exist: 200 and an empty result. The input can
    // tell what was dropped.
    let mut bad = ReviewInput::with_message("x");
    bad.labels.insert("Code-Review".into(), 5);
    bad.labels.insert("No-Such-Label".into(), 1);
    let result = block_on(c.post_review(n, &rev, &bad)).expect("accepted");
    println!("bad values: {result:?}");
    assert!(result.error.is_none());
    assert_eq!(
        bad.unapplied_labels(&result),
        [
            ("Code-Review".to_owned(), 5),
            ("No-Such-Label".to_owned(), 1)
        ]
    );
    // A valid vote next to a dropped one is applied.
    let mut mixed = ReviewInput::default();
    mixed.labels.insert("Code-Review".into(), 1);
    mixed.labels.insert("No-Such-Label".into(), 1);
    let result = block_on(c.post_review(n, &rev, &mixed)).unwrap();
    assert_eq!(
        mixed.unapplied_labels(&result),
        [("No-Such-Label".to_owned(), 1)]
    );

    // A vote without a message, and a message without a vote.
    let only_message = ReviewInput::with_message("only words");
    let result = block_on(c.post_review(n, &rev, &only_message)).unwrap();
    assert!(only_message.unapplied_labels(&result).is_empty());

    // A change that does not exist, and a revision that does not.
    let r = block_on(c.post_review(ChangeNumber(99999), &rev, &ReviewInput::with_message("x")));
    assert!(matches!(r, Err(Error::NotFound)));
    let r = block_on(c.post_review(
        n,
        &RevisionId::from("0".repeat(40)),
        &ReviewInput::with_message("x"),
    ));
    println!("unknown revision: {r:?}");
    assert!(matches!(r, Err(Error::NotFound | Error::BadRequest { .. })));

    let detail = block_on(c.get_change(n)).unwrap();
    assert!(detail.votes["Code-Review"].iter().any(|v| v.value == 1));
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn reviewed_marks_toggle() {
    let c = alice();
    let (n, rev) = new_change("reviewed");
    assert!(block_on(c.get_reviewed_files(n, &rev)).unwrap().is_empty());
    block_on(c.set_reviewed(n, &rev, "probe.txt", true)).expect("mark");
    // Again: not an error.
    block_on(c.set_reviewed(n, &rev, "probe.txt", true)).expect("mark twice");
    assert_eq!(
        block_on(c.get_reviewed_files(n, &rev)).unwrap(),
        ["probe.txt"]
    );
    block_on(c.set_reviewed(n, &rev, "/COMMIT_MSG", true)).expect("commit message");
    block_on(c.set_reviewed(n, &rev, "probe.txt", false)).expect("unmark");
    assert_eq!(
        block_on(c.get_reviewed_files(n, &rev)).unwrap(),
        ["/COMMIT_MSG"]
    );
    // Unmarking an unmarked file.
    let r = block_on(c.set_reviewed(n, &rev, "probe.txt", false));
    println!("unmark twice: {r:?}");
    assert!(matches!(
        block_on(anon().set_reviewed(n, &rev, "probe.txt", true)),
        Err(Error::Unauthorized)
    ));
}
