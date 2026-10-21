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
//! `alice` / `alice-dev-password`. The tests only read.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

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
    std::env::var("GERT_TEST_GERRIT")
        .expect("set GERT_TEST_GERRIT to the dev Gerrit URL, e.g. http://127.0.0.1:8080")
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
    let all = block_on(c.query_changes(&ChangeQuery::new("project:demo/scheduler").limit(50)))
        .expect("query");
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
    assert_eq!(a.hashtags, ["backend", "perf"]);
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
