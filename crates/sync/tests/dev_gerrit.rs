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
//!
//! The consolidation tests also write as `bob` (a reply, for the attention
//! set) and as `admin` (a project with Gerrit's default permissions, where
//! `alice` may not vote Code-Review +2).
//!
//! `local_e2e` runs local changes end to end: it clones `demo/scheduler`
//! into a temporary directory and pushes new changes to it as alice (the
//! `git` binary is needed).

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

#[path = "dev_gerrit/local_e2e.rs"]
mod local_e2e;

/// Blocking `ureq` behind the async trait, with a switch that makes every
/// request fail like a dead network, and one that loses the answer to the
/// next review post after the server took it (a crash mid-post).
struct Ureq {
    agent: ureq::Agent,
    offline: Arc<AtomicBool>,
    lose_review_answer: Arc<AtomicBool>,
}

impl HttpClient for Ureq {
    fn send(
        &self,
        req: http::Request<Vec<u8>>,
    ) -> BoxFuture<'static, Result<http::Response<Vec<u8>>, HttpError>> {
        let lose = req.method() == http::Method::POST
            && req.uri().path().ends_with("/review")
            && self.lose_review_answer.swap(false, Ordering::SeqCst);
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
        let res = if lose {
            Err(HttpError("connection reset (simulated)".into()))
        } else {
            res
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
    lose_review_answer: Arc<AtomicBool>,
    store: Arc<Mutex<Store>>,
    server: store::ServerId,
}

fn alice() -> Fx {
    let offline = Arc::new(AtomicBool::new(false));
    let lose_review_answer = Arc::new(AtomicBool::new(false));
    let http = Ureq {
        agent: agent(),
        offline: offline.clone(),
        lose_review_answer: lose_review_answer.clone(),
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
    let store = Arc::new(Mutex::new(store));
    Fx {
        syncer: Syncer::new(client, store.clone(), server),
        offline,
        lose_review_answer,
        store,
        server,
    }
}

fn raw(method: &str, path: &str, body: serde_json::Value) -> (u16, String) {
    raw_as("alice", method, path, body)
}

fn raw_as(user: &str, method: &str, path: &str, body: serde_json::Value) -> (u16, String) {
    let token =
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{user}-dev-password"));
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
        lose_review_answer: Arc::default(),
    };
    let resp = block_on(client.send(req)).expect("raw request");
    (
        resp.status().as_u16(),
        String::from_utf8_lossy(resp.body()).into_owned(),
    )
}

/// A new change by alice with the file `probe.txt`.
fn new_change() -> ChangeNumber {
    new_change_in("demo/scheduler", "main")
}

/// A new change by alice in `project` on `branch`: patch set 1 is empty,
/// patch set 2 adds `probe.txt` (three lines).
fn new_change_in(project: &str, branch: &str) -> ChangeNumber {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let (code, body) = raw(
        "POST",
        "/changes/",
        serde_json::json!({"project": project, "branch": branch, "status": "NEW",
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
    block_on(c.set_reviewed(n, &ps.sha, "probe.txt", true))
        .result
        .unwrap();
    assert_eq!(
        c.cached_files(&key).unwrap().unwrap().reviewed,
        Some(vec!["probe.txt".to_owned()])
    );
    let reconciled = block_on(c.refresh_files(&key)).unwrap();
    assert_eq!(reconciled.reviewed, Some(vec!["probe.txt".to_owned()]));
    block_on(c.set_reviewed(n, &ps.sha, "probe.txt", false))
        .result
        .unwrap();
    f.offline.store(true, Ordering::SeqCst);
    assert!(
        block_on(c.set_reviewed(n, &ps.sha, "probe.txt", true))
            .result
            .is_err()
    );
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

// ---- consolidation (LC5a) ----
//
// What these pin about Gerrit 3.14 (the dev Gerrit):
//
// * A review with inline `comments`, a `tag` and `drafts: KEEP` posts
//   exactly those comments, each with the review's tag, and leaves the
//   user's other drafts alone.
// * `ignore_automatic_attention_set_rules` leaves the attention set as it
//   was (a reply by the owner would otherwise take the owner out).
// * The posted review's change message carries the tag and the author, and
//   its `_revision_number` is the patch set posted to: what recovery after
//   a lost answer looks for.
// * Votes on a patch set that is not current are accepted (200), listed as
//   applied in `labels`, and kept on that patch set; the answer's
//   `change_info.current_revision_number` names the current one.
// * An unknown label or a value the label does not have: 200, the vote
//   silently left out of `labels`. There is no `unapplied_labels` field.
// * A vote the account may not give: 403 `Applying label "Code-Review": 2
//   is restricted`.

fn json_of(body: &str) -> serde_json::Value {
    serde_json::from_str(body.trim_start_matches(")]}'")).unwrap()
}

/// `GET` as alice, as JSON.
fn get(path: &str) -> serde_json::Value {
    let (code, body) = raw("GET", path, serde_json::Value::Null);
    assert_eq!(code, 200, "{path}: {body}");
    json_of(&body)
}

impl Fx {
    /// A local change row whose only version is `ps` of change `n`, as if
    /// the user had reviewed the commit before pushing it: a local draft on
    /// line 2 of `probe.txt` and a pending review with `labels` and an
    /// unresolved message.
    fn local_review_of(
        &self,
        n: ChangeNumber,
        ps: &PatchSetRef,
        labels: &[(&str, i32)],
    ) -> (model::LocalChange, model::PendingReview) {
        let detail = self.syncer.cached_change(n).unwrap().unwrap().detail;
        let change_id = detail.summary.change_id.clone().unwrap();
        let row = {
            let mut store = self.store.lock().unwrap();
            let upsert = store::LocalChangeUpsert {
                seen: None,
                branch: Some(detail.summary.branch.clone()),
                change_id: change_id.clone(),
                gerrit_number: Some(n),
                subject: "local".into(),
                author_name: "Alice".into(),
                author_email: "alice@example.com".into(),
                updated: model::Timestamp::now(),
                state: None,
                patch_sets: vec![store::NewLocalPatchSet {
                    sha: ps.sha.clone(),
                    parent_sha: None,
                    seen_at: model::Timestamp::now(),
                }],
            };
            store
                .reconcile_local(
                    self.server,
                    &detail.summary.project,
                    &[store::LocalOp::Upsert(upsert)],
                    model::Timestamp::now(),
                )
                .unwrap();
            store
                .local_changes_by_change_id(self.server, &change_id)
                .unwrap()
                .remove(0)
        };
        let version = self.syncer.local_change_versions(&row);
        self.syncer
            .save_draft_local(DraftSave::Create {
                change: n,
                patch_set: PatchSetRef {
                    number: version.number,
                    sha: version.sha.clone(),
                },
                input: input("local inline"),
            })
            .unwrap()
            .unwrap();
        let review = self
            .syncer
            .create_pending_review(&store::NewPendingReview {
                patch_set: version.number,
                labels: labels.iter().map(|(l, v)| ((*l).to_owned(), *v)).collect(),
                strategy: model::LocalApproval::Exact,
                message: "local review message".into(),
                unresolved: true,
                created: model::Timestamp::now(),
                offline: false,
                replaces_draft: None,
            })
            .unwrap();
        (row, review)
    }

    fn review_now(&self, r: &model::PendingReview) -> model::PendingReview {
        self.store
            .lock()
            .unwrap()
            .load_pending_review(self.server, r.id)
            .unwrap()
            .unwrap()
    }
}

/// The local change's only version.
trait Versions {
    fn local_change_versions(&self, row: &model::LocalChange) -> model::LocalPatchSet;
}

impl Versions for Syncer {
    fn local_change_versions(&self, row: &model::LocalChange) -> model::LocalPatchSet {
        let gerrit = row.gerrit_number.unwrap();
        self.local_patch_sets_of(gerrit)
            .unwrap()
            .into_iter()
            .find(|v| v.local_change == row.id)
            .unwrap()
    }
}

fn attention(n: ChangeNumber) -> Vec<String> {
    let c = get(&format!("/changes/{n}"));
    let mut ids: Vec<String> = c["attention_set"]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    ids.sort();
    ids
}

/// The change messages with `tag`.
fn tagged(n: ChangeNumber, tag: &str) -> Vec<serde_json::Value> {
    get(&format!("/changes/{n}?o=MESSAGES"))["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["tag"] == tag)
        .cloned()
        .collect()
}

fn current_ps(f: &Fx, n: ChangeNumber) -> PatchSetRef {
    let detail = block_on(f.syncer.refresh_change(n)).expect("detail");
    detail.current().expect("current").as_ref()
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn consolidation_posts_one_tagged_review_and_keeps_other_drafts() {
    let f = alice();
    let syncer = f.syncer.clone().with_app_name("devtest");
    let n = new_change();
    let ps = current_ps(&f, n);
    // Bob replies, which gives alice (the owner) the attention.
    let (code, body) = raw_as(
        "bob",
        "POST",
        &format!("/changes/{n}/revisions/current/review"),
        serde_json::json!({"message": "over to you"}),
    );
    assert_eq!(code, 200, "{body}");
    let before = attention(n);
    assert!(!before.is_empty());
    // A draft alice wrote in the web UI.
    let (code, body) = raw(
        "PUT",
        &format!("/changes/{n}/revisions/current/drafts"),
        serde_json::json!({"path": "probe.txt", "line": 1, "message": "web ui draft"}),
    );
    assert_eq!(code, 201, "{body}");
    let (row, review) = f.local_review_of(n, &ps, &[("Code-Review", 1)]);

    let report = block_on(syncer.consolidate(&sync::ConsolidateOptions::default()));
    assert!(
        report.stopped.is_none() && report.errors.is_empty(),
        "{report:?}"
    );
    assert_eq!(report.posts.len(), 1, "{report:?}");
    assert_eq!(report.posts[0].outcome, sync::PostOutcome::Sent);
    assert_eq!(report.consolidated, [(row.id, n)]);
    assert_eq!(f.review_now(&review).state, model::PendingReviewState::Sent);

    // One message, tagged, by alice, on the patch set posted to.
    let messages = get(&format!("/changes/{n}?o=MESSAGES"))["messages"].clone();
    let ours: Vec<&serde_json::Value> = messages
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| {
            m["tag"]
                .as_str()
                .is_some_and(|t| t.starts_with(&format!("devtest-local-{}-", row.id.0)))
        })
        .collect();
    assert_eq!(ours.len(), 1, "{messages}");
    let tag = ours[0]["tag"].as_str().unwrap();
    // The tag names the review, so a post whose claim was lost is found.
    assert!(
        tag.starts_with(&format!("devtest-local-{}-r{}-", row.id.0, review.id.0)),
        "{tag}"
    );
    println!("posted message: {}", ours[0]);
    let me = get("/accounts/self")["_account_id"].clone();
    assert_eq!(ours[0]["author"]["_account_id"], me);
    assert_eq!(ours[0]["_revision_number"], ps.number.0);
    // Recovery reads the votes a post cast from this first line.
    let text = ours[0]["message"].as_str().unwrap();
    assert!(
        text.starts_with(&format!("Patch Set {}: Code-Review+1", ps.number.0)),
        "{text}"
    );

    // Exactly the local comments, with the tag; the web UI draft is still
    // a draft.
    let comments = get(&format!("/changes/{n}/comments"));
    println!("comments: {comments}");
    let inline = comments["probe.txt"].as_array().unwrap();
    assert_eq!(inline.len(), 1);
    assert_eq!(inline[0]["message"], "local inline");
    assert_eq!(inline[0]["line"], 2);
    assert_eq!(inline[0]["unresolved"], true);
    assert_eq!(inline[0]["tag"], tag);
    let top = comments["/PATCHSET_LEVEL"].as_array().unwrap();
    assert_eq!(top.len(), 1);
    assert_eq!(top[0]["message"], "local review message");
    assert_eq!(top[0]["unresolved"], true);
    let drafts = get(&format!("/changes/{n}/drafts"));
    assert_eq!(drafts["probe.txt"][0]["message"], "web ui draft");

    // The attention set is as it was; the vote is there.
    assert_eq!(attention(n), before);
    let labels = get(&format!("/changes/{n}?o=DETAILED_LABELS"))["labels"].clone();
    assert!(
        labels["Code-Review"]["all"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["_account_id"] == me && v["value"] == 1),
        "{labels}"
    );

    // Nothing left to send: the next run posts nothing.
    let report = block_on(syncer.consolidate(&sync::ConsolidateOptions::default()));
    assert!(report.posts.is_empty(), "{report:?}");
    assert_eq!(tagged(n, tag).len(), 1);
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn a_post_whose_answer_was_lost_is_found_by_its_tag_and_not_sent_again() {
    let f = alice();
    let n = new_change();
    let ps = current_ps(&f, n);
    let (_, review) = f.local_review_of(n, &ps, &[("Code-Review", 1)]);
    f.lose_review_answer.store(true, Ordering::SeqCst);
    let report = block_on(f.syncer.consolidate(&sync::ConsolidateOptions::default()));
    assert!(report.stopped.is_some(), "{report:?}");
    let sending = f.review_now(&review);
    assert_eq!(sending.state, model::PendingReviewState::Sending);
    let nonce = sending.nonce.unwrap();

    // A real run waits `CLAIM_STALE` before it recovers another run's
    // post; this test has no other run, so it recovers at once.
    let quick = f
        .syncer
        .clone()
        .with_consolidation_timing(sync::ConsolidationTiming {
            claim_stale: jiff::SignedDuration::from_secs(-1),
            ..sync::ConsolidationTiming::default()
        });
    let report = block_on(quick.consolidate(&sync::ConsolidateOptions::default()));
    assert!(report.stopped.is_none(), "{report:?}");
    assert_eq!(report.posts.len(), 1);
    assert!(report.posts[0].recovered, "{report:?}");
    assert_eq!(f.review_now(&review).state, model::PendingReviewState::Sent);
    let messages = get(&format!("/changes/{n}?o=MESSAGES"))["messages"].clone();
    let ours = messages
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["tag"].as_str().is_some_and(|t| t.ends_with(&nonce)))
        .count();
    assert_eq!(ours, 1, "posted once: {messages}");
    let comments = get(&format!("/changes/{n}/comments"));
    assert_eq!(comments["probe.txt"].as_array().unwrap().len(), 1);
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn gerrit_takes_votes_on_an_older_patch_set_and_drops_unknown_labels() {
    let f = alice();
    let n = new_change();
    let detail = block_on(f.syncer.refresh_change(n)).unwrap();
    let ps1 = detail
        .revision(model::PatchSetNumber(1))
        .unwrap()
        .sha
        .clone();
    let ps2 = detail
        .revision(model::PatchSetNumber(2))
        .unwrap()
        .sha
        .clone();
    let mut old = ReviewInput {
        drafts: model::DraftHandling::Keep,
        ..ReviewInput::default()
    };
    old.labels.insert("Code-Review".into(), 1);
    let out = block_on(f.syncer.publish_review(n, &ps1, &old)).expect("accepted");
    println!("vote on PS 1 of 2: {:?}", out.result);
    assert_eq!(out.result.labels.get("Code-Review"), Some(&1));
    assert_eq!(out.result.current_patch_set, Some(model::PatchSetNumber(2)));
    assert!(out.unapplied_labels.is_empty());

    let mut odd = ReviewInput {
        drafts: model::DraftHandling::Keep,
        ..ReviewInput::default()
    };
    odd.labels.insert("No-Such-Label".into(), 1);
    odd.labels.insert("Code-Review".into(), 3);
    let out = block_on(f.syncer.publish_review(n, &ps2, &odd)).expect("accepted");
    println!("unknown label and value: {:?}", out.result);
    assert_eq!(
        out.unapplied_labels,
        [
            ("Code-Review".to_owned(), 3),
            ("No-Such-Label".to_owned(), 1)
        ]
    );

    // Consolidation on a non-current exact match: comments and message go
    // out, the votes are held.
    let m = new_change();
    let detail = block_on(f.syncer.refresh_change(m)).unwrap();
    let older = detail.revision(model::PatchSetNumber(1)).unwrap().as_ref();
    // PS 1 has no probe.txt: comment on the commit message instead.
    let (row, review) = f.local_review_of_commit_message(m, &older, &[("Code-Review", 2)]);
    let report = block_on(f.syncer.consolidate(&sync::ConsolidateOptions::default()));
    assert!(report.stopped.is_none(), "{report:?}");
    let held = f.review_now(&review);
    assert_eq!(held.state, model::PendingReviewState::Held, "{report:?}");
    println!("held: {:?}", held.held_reason);
    let comments = get(&format!("/changes/{m}/comments"));
    assert_eq!(comments["/PATCHSET_LEVEL"][0]["patch_set"], 1);
    let labels = get(&format!("/changes/{m}?o=DETAILED_LABELS"))["labels"].clone();
    assert!(
        !labels["Code-Review"]["all"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|v| v["value"] == 2),
        "{labels}"
    );
    let _ = row;
}

impl Fx {
    /// [`Fx::local_review_of`] with the local draft on the commit message
    /// (which every patch set has).
    fn local_review_of_commit_message(
        &self,
        n: ChangeNumber,
        ps: &PatchSetRef,
        labels: &[(&str, i32)],
    ) -> (model::LocalChange, model::PendingReview) {
        let (row, review) = self.local_review_of(n, ps, labels);
        // Move the probe.txt draft to the commit message.
        let drafts = self.syncer.cached_comments(n).unwrap().drafts;
        for d in drafts.iter().filter(|d| d.path == "probe.txt") {
            self.syncer.delete_draft_local(d.local_id).unwrap();
        }
        let version = self.syncer.local_change_versions(&row);
        self.syncer
            .save_draft_local(DraftSave::Create {
                change: n,
                patch_set: PatchSetRef {
                    number: version.number,
                    sha: version.sha.clone(),
                },
                input: DraftInput {
                    path: "/COMMIT_MSG".into(),
                    location: Location::Line(1),
                    ..input("on the subject")
                },
            })
            .unwrap()
            .unwrap();
        (row, review)
    }
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn a_restricted_vote_is_refused_and_the_comments_still_go_out() {
    let f = alice();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    // Gerrit's default permissions: registered users vote -1..+1.
    let project = format!("dev-test/perm-{stamp}");
    let (code, body) = raw_as(
        "admin",
        "PUT",
        &format!("/projects/{}", project.replace('/', "%2F")),
        serde_json::json!({"create_empty_commit": true}),
    );
    assert_eq!(code, 201, "{body}");
    let n = new_change_in(&project, "master");
    let ps = current_ps(&f, n);
    let mut plus2 = ReviewInput {
        drafts: model::DraftHandling::Keep,
        ..ReviewInput::default()
    };
    plus2.labels.insert("Code-Review".into(), 2);
    let err = block_on(f.syncer.publish_review(n, &ps.sha, &plus2)).unwrap_err();
    println!("restricted vote: {err}");
    assert!(
        matches!(&err, sync::Error::Api(gerrit_api::Error::Forbidden { message })
            if message.contains("label")),
        "{err}"
    );

    let (_, review) = f.local_review_of(n, &ps, &[("Code-Review", 2)]);
    let report = block_on(f.syncer.consolidate(&sync::ConsolidateOptions::default()));
    assert!(report.stopped.is_none(), "{report:?}");
    let failed = f.review_now(&review);
    assert_eq!(
        failed.state,
        model::PendingReviewState::Failed,
        "{report:?}"
    );
    println!("failed votes: {:?}", failed.error);
    assert!(failed.error.unwrap().contains("restricted"));
    assert_eq!(failed.labels.get("Code-Review"), Some(&2));
    let comments = get(&format!("/changes/{n}/comments"));
    assert_eq!(comments["probe.txt"][0]["message"], "local inline");
    assert_eq!(
        comments["/PATCHSET_LEVEL"][0]["message"],
        "local review message"
    );
}

// ---- consolidation lookups (LC5a) ----
//
// * `change:"project~branch~Change-Id"` finds a change; each part is
//   URL-decoded by Gerrit (`ChangeTriplet.parse`), so a branch with `+` or
//   `%` is found only with its parts percent-encoded.
// * A bare `change:Change-Id` finds the change in any branch.
// * A Change-Id that is not `I` and hex digits makes Gerrit refuse the
//   whole query (400 "Invalid change format"), which is why such rows are
//   never asked about.
// * A posted review's change message starts "Patch Set N: Code-Review+1":
//   the votes it cast, which recovery reads.

impl Fx {
    /// A local change row without a Gerrit change (as the scanner makes
    /// one): project `project`, `branch` if known, one version `sha`, and a
    /// pending review with Code-Review +1 and a message.
    fn purely_local(
        &self,
        project: &str,
        branch: Option<&str>,
        change_id: &str,
        sha: &model::RevisionId,
    ) -> (model::LocalChange, model::PendingReview) {
        let mut store = self.store.lock().unwrap();
        let upsert = store::LocalChangeUpsert {
            seen: None,
            branch: branch.map(Into::into),
            change_id: change_id.into(),
            gerrit_number: None,
            subject: "local".into(),
            author_name: "Alice".into(),
            author_email: "alice@example.com".into(),
            updated: model::Timestamp::now(),
            state: None,
            patch_sets: vec![store::NewLocalPatchSet {
                sha: sha.clone(),
                parent_sha: None,
                seen_at: model::Timestamp::now(),
            }],
        };
        store
            .reconcile_local(
                self.server,
                project,
                &[store::LocalOp::Upsert(upsert)],
                model::Timestamp::now(),
            )
            .unwrap();
        let row = store
            .local_changes_by_change_id(self.server, change_id)
            .unwrap()
            .into_iter()
            .find(|r| r.project == project)
            .unwrap();
        let version = store
            .local_patch_sets(self.server, row.id)
            .unwrap()
            .remove(0);
        let review = store
            .create_pending_review(
                self.server,
                &store::NewPendingReview {
                    patch_set: version.number,
                    labels: [("Code-Review".to_owned(), 1)].into(),
                    strategy: model::LocalApproval::Exact,
                    message: "found by the lookup".into(),
                    unresolved: false,
                    created: model::Timestamp::now(),
                    offline: false,
                    replaces_draft: None,
                },
            )
            .unwrap();
        (row, review)
    }
}

/// The Change-Id and current patch set of change `n`.
fn identity(f: &Fx, n: ChangeNumber) -> (String, PatchSetRef) {
    let detail = block_on(f.syncer.refresh_change(n)).expect("detail");
    (
        detail.summary.change_id.clone().unwrap(),
        detail.current().unwrap().as_ref(),
    )
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn the_lookup_finds_changes_by_triplet_and_by_change_id() {
    let f = alice();
    // Known branch: by triplet.
    let by_triplet = new_change();
    let (id1, ps1) = identity(&f, by_triplet);
    let (row1, review1) = f.purely_local("demo/scheduler", Some("main"), &id1, &ps1.sha);
    // Branch unknown: by Change-Id.
    let by_id = new_change();
    let (id2, ps2) = identity(&f, by_id);
    let (row2, review2) = f.purely_local("demo/scheduler", None, &id2, &ps2.sha);
    // Not on Gerrit.
    let unknown = format!("I{:040x}", 0xdead_beef_u64);
    let (row3, _) = f.purely_local(
        "demo/scheduler",
        Some("main"),
        &unknown,
        &model::RevisionId("0000000000000000000000000000000000000001".into()),
    );
    // A Change-Id Gerrit cannot be asked about is never sent.
    let (row4, _) = f.purely_local(
        "demo/scheduler",
        Some("main"),
        "Inot-a-change-id",
        &model::RevisionId("0000000000000000000000000000000000000002".into()),
    );

    let report = block_on(f.syncer.consolidate(&sync::ConsolidateOptions::default()));
    assert!(
        report.stopped.is_none() && report.errors.is_empty(),
        "{report:?}"
    );
    let found: Vec<(model::LocalChangeId, ChangeNumber)> = report
        .found
        .iter()
        .map(|x| (x.local_change, x.gerrit))
        .collect();
    assert!(found.contains(&(row1.id, by_triplet)), "{report:?}");
    assert!(found.contains(&(row2.id, by_id)), "{report:?}");
    assert_eq!(report.not_found, [row3.id]);
    assert_eq!(
        f.review_now(&review1).state,
        model::PendingReviewState::Sent
    );
    assert_eq!(
        f.review_now(&review2).state,
        model::PendingReviewState::Sent
    );
    let store = f.store.lock().unwrap();
    let state = |id| store.local_change(f.server, id).unwrap().unwrap().state;
    assert_eq!(state(row1.id), model::LocalChangeState::Consolidated);
    assert_eq!(state(row3.id), model::LocalChangeState::Local);
    assert_eq!(state(row4.id), model::LocalChangeState::Local);
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn a_malformed_change_id_fails_the_whole_query() {
    let (code, body) = raw(
        "GET",
        "/changes/?q=change:Inot-a-change-id%20OR%20change:I0000000000000000000000000000000000000000",
        serde_json::Value::Null,
    );
    println!("malformed Change-Id in a query: {code} {body}");
    assert_eq!(code, 400, "{body}");
    assert!(body.contains("Invalid change format"), "{body}");
}

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn a_branch_with_plus_and_percent_is_found_by_its_encoded_triplet() {
    let f = alice();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let branch = format!("dev+test%{stamp}");
    let encoded: String = branch
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    let (code, body) = raw_as(
        "admin",
        "PUT",
        &format!("/projects/demo%2Fscheduler/branches/{encoded}"),
        serde_json::json!({"revision": "main"}),
    );
    assert_eq!(code, 201, "{body}");
    let n = new_change_in("demo/scheduler", &branch);
    let (id, ps) = identity(&f, n);
    // An unencoded triplet does not find it.
    let (code, body) = raw(
        "GET",
        &format!(
            "/changes/?q={}",
            format!("change:\"demo/scheduler~{branch}~{id}\"")
                .replace('%', "%25")
                .replace('+', "%2B")
                .replace('"', "%22")
        ),
        serde_json::Value::Null,
    );
    println!("unencoded triplet: {code} {}", &body[..body.len().min(120)]);
    assert!(
        !body.contains(&id),
        "an unencoded triplet found the change: {body}"
    );
    let (row, review) = f.purely_local("demo/scheduler", Some(&branch), &id, &ps.sha);
    let report = block_on(f.syncer.consolidate(&sync::ConsolidateOptions::default()));
    assert!(report.errors.is_empty(), "{report:?}");
    assert!(
        report
            .found
            .iter()
            .any(|x| x.local_change == row.id && x.gerrit == n),
        "{report:?}"
    );
    assert_eq!(f.review_now(&review).state, model::PendingReviewState::Sent);
}
