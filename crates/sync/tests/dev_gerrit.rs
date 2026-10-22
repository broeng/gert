//! Opt-in test of the sync layer against the local dev Gerrit; see
//! `crates/gerrit-api/tests/dev_gerrit.rs` for how to start it.
//!
//! ```text
//! GERT_TEST_GERRIT=http://127.0.0.1:8080 \
//!   cargo test -p sync --test dev_gerrit -- --ignored --nocapture
//! ```
//!
//! It writes (drafts, a review, reviewed marks) to a throwaway change of
//! its own, as `alice`. Per `CLAUDE.md` it refuses to run against anything
//! but 127.0.0.1 or localhost.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use base64::Engine as _;
use futures::{FutureExt, executor::block_on, future::BoxFuture};
use gerrit_api::{Auth, GerritClient, HttpClient, HttpError};
use model::{
    ChangeNumber, DraftInput, DraftSync, FilesKey, Location, PatchSetRef, ReviewInput, Side,
};
use store::Store;
use sync::{DraftSave, Syncer};

/// Blocking `ureq` behind the async trait, with a switch that makes every
/// request fail like a dead network.
struct Ureq {
    agent: ureq::Agent,
    offline: Arc<AtomicBool>,
}

impl HttpClient for Ureq {
    fn send(
        &self,
        req: http::Request<Vec<u8>>,
    ) -> BoxFuture<'static, Result<http::Response<Vec<u8>>, HttpError>> {
        let res = if self.offline.load(Ordering::SeqCst) {
            Err(HttpError("network is unreachable (simulated)".into()))
        } else {
            self.agent
                .run(req)
                .map_err(|e| HttpError(e.to_string()))
                .and_then(|resp| {
                    let (parts, mut body) = resp.into_parts();
                    let bytes = body.read_to_vec().map_err(|e| HttpError(e.to_string()))?;
                    Ok(http::Response::from_parts(parts, bytes))
                })
        };
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
    let (host, port) = authority.split_once(':').unwrap_or((authority, ""));
    assert!(
        matches!(host, "127.0.0.1" | "localhost")
            && !port.is_empty()
            && port.bytes().all(|b| b.is_ascii_digit()),
        "refusing to run against {url:?}: GERT_TEST_GERRIT must be http://127.0.0.1:PORT \
         or http://localhost:PORT"
    );
    url
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into()
}

struct Fx {
    syncer: Syncer,
    offline: Arc<AtomicBool>,
}

fn alice() -> Fx {
    let offline = Arc::new(AtomicBool::new(false));
    let http = Ureq {
        agent: agent(),
        offline: offline.clone(),
    };
    let client = GerritClient::new(
        Arc::new(http),
        &base_url(),
        Auth::Basic {
            username: "alice".into(),
            password: "alice-dev-password".into(),
        },
    )
    .unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let server = store.server_id(&base_url()).unwrap();
    Fx {
        syncer: Syncer::new(client, Arc::new(Mutex::new(store)), server),
        offline,
    }
}

fn raw(method: &str, path: &str, body: serde_json::Value) -> (u16, String) {
    let token = base64::engine::general_purpose::STANDARD.encode("alice:alice-dev-password");
    let req = http::Request::builder()
        .method(method)
        .uri(format!("{}/a{path}", base_url()))
        .header("authorization", format!("Basic {token}"))
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&body).unwrap())
        .unwrap();
    let client = Ureq {
        agent: agent(),
        offline: Arc::default(),
    };
    let resp = block_on(client.send(req)).expect("raw request");
    (
        resp.status().as_u16(),
        String::from_utf8_lossy(resp.body()).into_owned(),
    )
}

/// A new change by alice with the file `probe.txt`.
fn new_change() -> ChangeNumber {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let (code, body) = raw(
        "POST",
        "/changes/",
        serde_json::json!({"project": "demo/scheduler", "branch": "main", "status": "NEW",
                           "subject": format!("Dev test sync {stamp}")}),
    );
    assert_eq!(code, 201, "{body}");
    let info: serde_json::Value = serde_json::from_str(body.trim_start_matches(")]}'")).unwrap();
    let n = info["_number"].as_u64().unwrap();
    let content = base64::engine::general_purpose::STANDARD.encode("one\ntwo\nthree\n");
    let (code, body) = raw(
        "PUT",
        &format!("/changes/{n}/edit/probe.txt"),
        serde_json::json!({"binary_content": format!("data:text/plain;base64,{content}")}),
    );
    assert_eq!(code, 204, "{body}");
    let (code, body) = raw(
        "POST",
        &format!("/changes/{n}/edit:publish"),
        serde_json::json!({}),
    );
    assert_eq!(code, 204, "{body}");
    ChangeNumber(n)
}

fn input(message: &str) -> DraftInput {
    DraftInput {
        path: "probe.txt".into(),
        side: Side::Revision,
        parent: None,
        location: Location::Line(2),
        in_reply_to: None,
        message: message.into(),
        unresolved: true,
    }
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn seeded_drafts_merge_into_threads() {
    let f = alice();
    let c = f.syncer.clone();
    // Find the seeded change.
    let page = block_on(c.refresh_query("project:demo/scheduler subject:\"Retry failed jobs\"", 5))
        .expect("query");
    assert!(page.cached_count >= 1);
    let n = c
        .cached_query("project:demo/scheduler subject:\"Retry failed jobs\"")
        .unwrap()
        .unwrap()
        .changes[0]
        .number;
    block_on(c.refresh_change(n)).expect("change");
    let view = block_on(c.refresh_comments(n)).expect("comments");
    assert!(view.comments.len() >= 9);
    assert!(view.drafts.len() >= 4, "alice's four seeded drafts");
    assert!(view.drafts.iter().all(|d| d.sync == DraftSync::Synced));
    assert!(view.drafts.iter().all(|d| !d.revision.as_str().is_empty()));
    let threads = view.threads();
    // The draft that answers a published comment joined its thread.
    let t = threads
        .iter()
        .find(|t| {
            t.entries
                .iter()
                .any(|e| e.message().contains("Done in the next patch set."))
        })
        .expect("thread with the reply draft");
    assert!(t.has_draft() && t.entries.len() >= 2);
    // Refreshing again changes nothing (no duplicates).
    let again = block_on(c.refresh_comments(n)).unwrap();
    assert_eq!(again.drafts.len(), view.drafts.len());
    assert_eq!(again.threads().len(), threads.len());
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn offline_drafts_sync_publish_and_reviewed_marks() {
    let f = alice();
    let c = &f.syncer;
    let n = new_change();
    let detail = block_on(c.refresh_change(n)).expect("detail");
    let ps = detail.current().expect("current").as_ref();
    let ps = PatchSetRef {
        number: ps.number,
        sha: ps.sha,
    };

    // Written offline: pending, with the reason.
    f.offline.store(true, Ordering::SeqCst);
    let d = block_on(c.save_draft(DraftSave::Create {
        change: n,
        patch_set: ps.clone(),
        input: input("written on the train"),
    }))
    .unwrap()
    .unwrap();
    assert_eq!(d.sync, DraftSync::PendingCreate);
    assert!(d.last_error.is_some());
    // A second one, edited and a third deleted, all while offline.
    let d2 = block_on(c.save_draft(DraftSave::Create {
        change: n,
        patch_set: ps.clone(),
        input: input("to be deleted"),
    }))
    .unwrap()
    .unwrap();
    block_on(c.delete_draft(d2.local_id)).unwrap();
    let r = block_on(c.retry_pending_drafts()).unwrap();
    assert_eq!(r.synced, 0);
    // Publishing is refused offline; nothing is lost.
    let err = block_on(c.publish_review(n, &ps.sha, &ReviewInput::with_message("x"))).unwrap_err();
    println!("publish offline: {err}");

    // Back online.
    f.offline.store(false, Ordering::SeqCst);
    let r = block_on(c.retry_pending_drafts()).unwrap();
    assert_eq!((r.synced, r.rejected, r.still_pending), (1, 0, 0));
    let view = block_on(c.refresh_comments(n)).unwrap();
    assert_eq!(
        view.drafts.len(),
        1,
        "no duplicate after the merge: {:?}",
        view.drafts
    );
    assert_eq!(view.drafts[0].message, "written on the train");
    assert_eq!(view.drafts[0].sync, DraftSync::Synced);
    assert!(view.drafts[0].server_id.is_some());

    // Edit online, then offline-edit and replay.
    f.offline.store(true, Ordering::SeqCst);
    block_on(c.save_draft(DraftSave::Update {
        id: d.local_id,
        message: "edited offline".into(),
        unresolved: false,
    }))
    .unwrap();
    let view = block_on(c.refresh_comments(n));
    assert!(view.is_err(), "refresh needs the network");
    f.offline.store(false, Ordering::SeqCst);
    // The refresh merges before the retry: the pending edit must win.
    let view = block_on(c.refresh_comments(n)).unwrap();
    assert_eq!(view.drafts[0].message, "edited offline");
    assert_eq!(view.drafts[0].sync, DraftSync::PendingUpdate);
    let r = block_on(c.retry_pending_drafts()).unwrap();
    assert_eq!(r.synced, 1);
    let view = block_on(c.refresh_comments(n)).unwrap();
    assert_eq!(view.drafts.len(), 1);
    assert_eq!(view.drafts[0].message, "edited offline");
    assert_eq!(view.drafts[0].sync, DraftSync::Synced);
    assert!(!view.drafts[0].unresolved);

    // Reviewed marks, optimistic and reconciled.
    let key = FilesKey {
        change: n,
        base: None,
        target: ps.clone(),
    };
    let files = block_on(c.refresh_files(&key)).unwrap();
    assert_eq!(files.reviewed.as_deref(), Some(&[][..]));
    block_on(c.set_reviewed(n, &ps.sha, "probe.txt", true)).unwrap();
    assert_eq!(
        c.cached_files(&key).unwrap().unwrap().reviewed,
        Some(vec!["probe.txt".to_owned()])
    );
    let reconciled = block_on(c.refresh_files(&key)).unwrap();
    assert_eq!(reconciled.reviewed, Some(vec!["probe.txt".to_owned()]));
    block_on(c.set_reviewed(n, &ps.sha, "probe.txt", false)).unwrap();
    f.offline.store(true, Ordering::SeqCst);
    assert!(block_on(c.set_reviewed(n, &ps.sha, "probe.txt", true)).is_err());
    f.offline.store(false, Ordering::SeqCst);
    assert_eq!(
        c.cached_files(&key).unwrap().unwrap().reviewed,
        Some(vec![]),
        "reverted after the failure"
    );

    // Publish: the draft, a vote and a message in one review.
    let mut review = ReviewInput::with_message("Done with my review.");
    review.labels.insert("Code-Review".into(), 1);
    let out = block_on(c.publish_review(n, &ps.sha, &review)).unwrap();
    assert!(
        out.unapplied_labels.is_empty(),
        "{:?}",
        out.unapplied_labels
    );
    assert!(out.result.error.is_none());
    let comments = out.comments.expect("refreshed comments");
    assert!(comments.drafts.is_empty());
    assert_eq!(comments.comments.len(), 1);
    assert_eq!(comments.comments[0].message, "edited offline");
    assert!(!comments.comments[0].unresolved);
    let detail = out.detail.expect("refreshed detail");
    assert!(
        detail
            .messages
            .iter()
            .any(|m| m.message.contains("Done with my review."))
    );
    assert!(detail.votes["Code-Review"].iter().any(|v| v.value == 1));
    // The local table is empty again.
    assert!(c.cached_comments(n).unwrap().drafts.is_empty());
    assert_eq!(block_on(c.retry_pending_drafts()).unwrap().synced, 0);
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn actions_refresh_the_cached_change() {
    let f = alice();
    let c = &f.syncer;
    let n = new_change();
    block_on(c.refresh_change(n)).expect("detail");

    let d = block_on(c.abandon(n, Some("not needed")))
        .expect("abandon")
        .expect("refreshed");
    assert_eq!(d.summary.status, model::ChangeStatus::Abandoned);
    assert!(d.action("restore").is_some_and(|a| a.enabled));
    // The store has it too, including the list row.
    assert_eq!(c.cached_change(n).unwrap().unwrap().detail, d);

    // A refused action leaves the cache as it was.
    let err = block_on(c.abandon(n, None)).unwrap_err();
    assert!(!err.is_unauthorized(), "{err}");
    assert_eq!(c.cached_change(n).unwrap().unwrap().detail, d);

    let d = block_on(c.restore(n, None)).unwrap().unwrap();
    assert_eq!(d.summary.status, model::ChangeStatus::New);

    let d = block_on(c.set_work_in_progress(n, true, None))
        .unwrap()
        .unwrap();
    assert!(d.summary.work_in_progress && d.action("ready").is_some());
    let d = block_on(c.set_work_in_progress(n, false, None))
        .unwrap()
        .unwrap();
    assert!(!d.summary.work_in_progress);
    let d = block_on(c.set_private(n, true)).unwrap().unwrap();
    assert!(d.summary.is_private);
    let d = block_on(c.set_private(n, false)).unwrap().unwrap();
    assert!(!d.summary.is_private);
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn reviewer_topic_and_hashtag_edits_refresh_the_cached_change() {
    let f = alice();
    let c = &f.syncer;
    let n = new_change();
    block_on(c.refresh_change(n)).expect("detail");

    let found = block_on(c.suggest_reviewers(n, "bo")).expect("suggest");
    assert!(found.iter().any(|s| s.reviewer_input() != ""), "{found:?}");
    // Suggestions are not cached.
    assert!(
        c.cached_change(n)
            .unwrap()
            .unwrap()
            .detail
            .reviewers
            .is_empty()
    );

    let added = block_on(c.add_reviewer(n, "carol", model::ReviewerState::Cc, false)).unwrap();
    assert!(added.result.is_added(), "{:?}", added.result);
    let d = added.detail.expect("refreshed");
    assert!(d.cc.iter().any(|a| a.username.as_deref() == Some("carol")));
    assert_eq!(c.cached_change(n).unwrap().unwrap().detail, d);

    // An unknown account is not an error but is not added; cache unchanged.
    let bad = block_on(c.add_reviewer(
        n,
        "nobody-by-that-name",
        model::ReviewerState::Reviewer,
        false,
    ));
    if let Ok(bad) = bad {
        assert!(!bad.result.is_added() && bad.detail.is_none());
    }
    assert_eq!(c.cached_change(n).unwrap().unwrap().detail, d);

    let d = block_on(c.remove_reviewer(n, "carol")).unwrap().unwrap();
    assert!(d.cc.is_empty() && d.reviewers.is_empty());

    let d = block_on(c.set_topic(n, Some("sync-topic")))
        .unwrap()
        .unwrap();
    assert_eq!(d.summary.topic.as_deref(), Some("sync-topic"));
    let d = block_on(c.set_topic(n, None)).unwrap().unwrap();
    assert_eq!(d.summary.topic, None);

    let d = block_on(c.edit_hashtags(n, &["one".into()], &[]))
        .unwrap()
        .unwrap();
    assert_eq!(d.summary.hashtags, ["one"]);
    let d = block_on(c.edit_hashtags(n, &[], &["one".into()]))
        .unwrap()
        .unwrap();
    assert!(d.summary.hashtags.is_empty());
    assert_eq!(c.cached_change(n).unwrap().unwrap().detail, d);
}
