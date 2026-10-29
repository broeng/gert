use std::sync::{Arc, Mutex};

use futures::executor::block_on;
use gerrit_api::{
    Auth, GerritClient,
    fake::{FakeHttpClient, RouteHttpClient},
};
use model::{DraftOp, FilesKey, Location, PatchSetNumber, Side, ThreadId};
use serde_json::{Value, json};
use store::Store;

use super::*;

const BASE: &str = "https://g.example";
const CHANGE: ChangeNumber = ChangeNumber(7);

fn signed_in() -> Auth {
    Auth::Basic {
        username: "u".into(),
        password: "p".into(),
    }
}

fn syncer_for(http: Arc<dyn gerrit_api::HttpClient>, auth: Auth) -> Syncer {
    let client = GerritClient::new(http, BASE, auth).unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let server = store.server_id(BASE).unwrap();
    Syncer::new(client, Arc::new(Mutex::new(store)), server)
}

struct Fx {
    http: FakeHttpClient,
    syncer: Syncer,
}

fn fx() -> Fx {
    let http = FakeHttpClient::new();
    Fx {
        syncer: syncer_for(Arc::new(http.clone()), signed_in()),
        http,
    }
}

fn xssi(body: &Value) -> String {
    format!(")]}}'\n{body}")
}

fn ps(n: u32) -> PatchSetRef {
    PatchSetRef {
        number: PatchSetNumber(n),
        sha: format!("sha{n}").as_str().into(),
    }
}

fn input(message: &str) -> DraftInput {
    DraftInput {
        path: "src/a.rs".into(),
        side: Side::Revision,
        parent: None,
        location: Location::Line(7),
        in_reply_to: None,
        message: message.into(),
        unresolved: true,
    }
}

fn create(message: &str) -> DraftSave {
    DraftSave::Create {
        change: CHANGE,
        patch_set: ps(2),
        input: input(message),
    }
}

/// A server reply to creating or updating a draft.
fn draft_reply(id: &str, message: &str) -> String {
    xssi(&json!({
        "id": id, "patch_set": 2, "path": "src/a.rs", "line": 7,
        "updated": "2024-03-04 10:00:00.000000000", "message": message
    }))
}

fn draft(f: &Fx, id: LocalDraftId) -> Draft {
    f.syncer
        .store()
        .load_draft(f.syncer.server, id)
        .unwrap()
        .unwrap()
}

fn uris(http: &FakeHttpClient) -> Vec<String> {
    http.requests()
        .iter()
        .map(|r| format!("{} {}", r.method(), r.uri().path()))
        .collect()
}

#[test]
fn saving_a_draft_uploads_it_and_records_the_server_id() {
    let f = fx();
    f.http.push_response(201, draft_reply("srv1", "hello"));
    let d = block_on(f.syncer.save_draft(create("hello")))
        .unwrap()
        .unwrap();
    assert_eq!(d.sync, DraftSync::Synced);
    assert_eq!(d.server_id.as_deref(), Some("srv1"));
    assert_eq!(uris(&f.http), ["PUT /a/changes/7/revisions/sha2/drafts"]);
    // The draft shows in the change's comments and threads at once.
    let view = f.syncer.cached_comments(CHANGE).unwrap();
    assert_eq!(view.drafts.len(), 1);
    assert_eq!(view.fetched_at, None);
    let threads = view.threads();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].id, ThreadId::Draft(d.local_id));
}

#[test]
fn offline_drafts_stay_pending_and_upload_when_the_connection_is_back() {
    let f = fx();
    // Offline: the network error leaves the draft pending.
    f.http.push_error("dns error");
    let d = block_on(f.syncer.save_draft(create("written offline")))
        .unwrap()
        .unwrap();
    assert_eq!(d.sync, DraftSync::PendingCreate);
    assert_eq!(d.attempts, 1);
    assert!(
        d.last_error
            .as_deref()
            .is_some_and(|e| e.contains("dns error"))
    );
    assert_eq!(d.server_id, None);

    // Still offline when the poll retries: still pending, attempt counted.
    f.http.push_error("dns error");
    let r = block_on(f.syncer.retry_pending_drafts()).unwrap();
    assert_eq!(
        r,
        RetryReport {
            still_pending: 1,
            ..RetryReport::default()
        }
    );
    assert_eq!(draft(&f, d.local_id).attempts, 2);

    // Back online.
    f.http
        .push_response(201, draft_reply("srv9", "written offline"));
    let r = block_on(f.syncer.retry_pending_drafts()).unwrap();
    assert_eq!(
        r,
        RetryReport {
            synced: 1,
            ..RetryReport::default()
        }
    );
    let d = draft(&f, d.local_id);
    assert_eq!(d.sync, DraftSync::Synced);
    assert_eq!(d.server_id.as_deref(), Some("srv9"));
    assert_eq!((d.attempts, d.last_error), (0, None));
    // Nothing left to do.
    let before = f.http.requests().len();
    assert_eq!(
        block_on(f.syncer.retry_pending_drafts()).unwrap(),
        RetryReport::default()
    );
    assert_eq!(f.http.requests().len(), before);
}

#[test]
fn editing_and_deleting_offline_are_replayed_in_order() {
    let f = fx();
    f.http.push_response(201, draft_reply("srv1", "one"));
    let one = block_on(f.syncer.save_draft(create("one")))
        .unwrap()
        .unwrap();
    f.http.push_response(201, draft_reply("srv2", "two"));
    let two = block_on(f.syncer.save_draft(create("two")))
        .unwrap()
        .unwrap();

    // Offline: edit one, delete two, write a new one.
    f.http.push_error("offline");
    block_on(f.syncer.save_draft(DraftSave::Update {
        id: one.local_id,
        message: "one, edited".into(),
        unresolved: false,
    }))
    .unwrap();
    f.http.push_error("offline");
    assert_eq!(
        block_on(f.syncer.delete_draft(two.local_id)).unwrap(),
        DeleteOutcome::Pending
    );
    f.http.push_error("offline");
    let three = block_on(f.syncer.save_draft(create("three")))
        .unwrap()
        .unwrap();
    assert_eq!(draft(&f, one.local_id).sync, DraftSync::PendingUpdate);
    assert_eq!(draft(&f, two.local_id).sync, DraftSync::PendingDelete);
    // The deleted draft is hidden from the view.
    let view = f.syncer.cached_comments(CHANGE).unwrap();
    let shown: Vec<_> = view.drafts.iter().map(|d| d.local_id).collect();
    assert_eq!(shown, [one.local_id, three.local_id]);

    let skip = f.http.requests().len();
    f.http
        .push_response(200, draft_reply("srv1", "one, edited"));
    f.http.push_response(204, "");
    f.http.push_response(201, draft_reply("srv3", "three"));
    let r = block_on(f.syncer.retry_pending_drafts()).unwrap();
    assert_eq!(
        r,
        RetryReport {
            synced: 2,
            deleted: 1,
            ..RetryReport::default()
        }
    );
    let sent: Vec<_> = uris(&f.http).split_off(skip);
    assert_eq!(
        sent,
        [
            "PUT /a/changes/7/revisions/sha2/drafts/srv1",
            "DELETE /a/changes/7/revisions/sha2/drafts/srv2",
            "PUT /a/changes/7/revisions/sha2/drafts"
        ]
    );
    let body: Value = serde_json::from_slice(f.http.requests()[skip].body()).unwrap();
    assert_eq!(body["message"], "one, edited");
    assert_eq!(body["unresolved"], false);
    assert!(
        f.syncer
            .store()
            .load_draft(f.syncer.server, two.local_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(draft(&f, three.local_id).sync, DraftSync::Synced);
}

#[test]
fn retry_stops_at_the_first_network_failure() {
    let f = fx();
    for m in ["a", "b", "c"] {
        f.http.push_error("offline");
        block_on(f.syncer.save_draft(create(m))).unwrap();
    }
    let before = f.http.requests().len();
    f.http.push_error("offline");
    let r = block_on(f.syncer.retry_pending_drafts()).unwrap();
    assert_eq!(r.still_pending, 3);
    assert_eq!(
        f.http.requests().len(),
        before + 1,
        "no request for b and c"
    );
}

#[test]
fn deleting_a_draft_the_server_never_saw_needs_no_request() {
    let f = fx();
    f.http.push_error("offline");
    let d = block_on(f.syncer.save_draft(create("x"))).unwrap().unwrap();
    let before = f.http.requests().len();
    assert_eq!(
        block_on(f.syncer.delete_draft(d.local_id)).unwrap(),
        DeleteOutcome::Removed
    );
    assert_eq!(f.http.requests().len(), before);
    assert!(f.syncer.cached_comments(CHANGE).unwrap().drafts.is_empty());
    assert_eq!(
        block_on(f.syncer.retry_pending_drafts()).unwrap(),
        RetryReport::default()
    );
}

#[test]
fn deleting_a_draft_that_is_already_gone_on_the_server_succeeds() {
    let f = fx();
    f.http.push_response(201, draft_reply("srv1", "x"));
    let d = block_on(f.syncer.save_draft(create("x"))).unwrap().unwrap();
    f.http.push_response(404, "Not Found: srv1");
    block_on(f.syncer.delete_draft(d.local_id)).unwrap();
    assert!(
        f.syncer
            .store()
            .load_draft(f.syncer.server, d.local_id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn rejections_fail_the_draft_with_the_servers_message() {
    for (code, body, retryable) in [
        (409, "change is closed", false),
        (403, "not permitted", false),
        (400, "patch set 9 does not exist", false),
        (404, "", false),
        (500, "internal error", true),
    ] {
        let f = fx();
        f.http.push_response(code, body);
        let d = block_on(f.syncer.save_draft(create("x"))).unwrap().unwrap();
        match &d.sync {
            DraftSync::Failed {
                op: DraftOp::Create,
                error,
                retryable: r,
            } => {
                assert_eq!(*r, retryable, "{code}");
                assert!(error.contains(body), "{code}: {error}");
            }
            other => panic!("{code}: {other:?}"),
        }
        // Not retried automatically, and still visible with its error.
        let before = f.http.requests().len();
        assert_eq!(
            block_on(f.syncer.retry_pending_drafts()).unwrap(),
            RetryReport::default()
        );
        assert_eq!(f.http.requests().len(), before);
        assert_eq!(f.syncer.cached_comments(CHANGE).unwrap().drafts.len(), 1);
    }
}

#[test]
fn a_failed_draft_can_be_retried_by_hand() {
    let f = fx();
    f.http.push_response(500, "oops");
    let d = block_on(f.syncer.save_draft(create("x"))).unwrap().unwrap();
    f.http.push_response(201, draft_reply("srv1", "x"));
    let d = block_on(f.syncer.retry_failed_draft(d.local_id))
        .unwrap()
        .unwrap();
    assert_eq!(d.sync, DraftSync::Synced);
    // Retrying a draft that did not fail does nothing.
    let before = f.http.requests().len();
    let again = block_on(f.syncer.retry_failed_draft(d.local_id))
        .unwrap()
        .unwrap();
    assert_eq!(again.sync, DraftSync::Synced);
    assert_eq!(f.http.requests().len(), before);
}

#[test]
fn a_401_stops_the_upload_and_leaves_the_draft_pending() {
    let f = fx();
    f.http.push_response(401, "");
    let err = block_on(f.syncer.save_draft(create("x"))).unwrap_err();
    assert!(err.is_unauthorized());
    let pending = f.syncer.store().pending_drafts(f.syncer.server).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        (pending[0].attempts, &pending[0].sync),
        (0, &DraftSync::PendingCreate)
    );

    f.http.push_response(401, "");
    assert!(
        block_on(f.syncer.retry_pending_drafts())
            .unwrap_err()
            .is_unauthorized()
    );
    // A 403 is not a login problem: the draft fails, the call succeeds.
    f.http.push_response(403, "no");
    assert!(block_on(f.syncer.retry_pending_drafts()).is_ok());
}

#[test]
fn updating_a_draft_that_vanished_on_the_server_creates_it_again() {
    let f = fx();
    f.http.push_response(201, draft_reply("srv1", "v1"));
    let d = block_on(f.syncer.save_draft(create("v1")))
        .unwrap()
        .unwrap();
    f.http.push_response(404, "Not Found");
    f.http.push_response(201, draft_reply("srv2", "v2"));
    let d = block_on(f.syncer.save_draft(DraftSave::Update {
        id: d.local_id,
        message: "v2".into(),
        unresolved: true,
    }))
    .unwrap()
    .unwrap();
    assert_eq!(d.sync, DraftSync::Synced);
    assert_eq!(d.server_id.as_deref(), Some("srv2"));
    let sent = uris(&f.http);
    assert_eq!(sent[1], "PUT /a/changes/7/revisions/sha2/drafts/srv1");
    assert_eq!(sent[2], "PUT /a/changes/7/revisions/sha2/drafts");
}

#[test]
fn updating_a_missing_draft_is_none() {
    let f = fx();
    let r = block_on(f.syncer.save_draft(DraftSave::Update {
        id: LocalDraftId(99),
        message: "x".into(),
        unresolved: true,
    }))
    .unwrap();
    assert!(r.is_none());
    assert!(f.http.requests().is_empty());
}

// ---- refreshing comments ----

fn comments_json() -> String {
    xssi(&json!({
        "src/a.rs": [
            {"patch_set": 2, "id": "c1", "line": 7, "author": {"_account_id": 3, "name": "Bob"},
             "updated": "2024-03-01 10:00:00.000000000", "message": "Why?", "unresolved": true}
        ]
    }))
}

fn drafts_json(ids: &[&str]) -> String {
    let list: Vec<Value> = ids
        .iter()
        .map(|id| {
            json!({"patch_set": 2, "id": id, "line": 7, "in_reply_to": "c1",
                   "updated": "2024-03-02 10:00:00.000000000", "message": format!("draft {id}"),
                   "commit_id": "sha2"})
        })
        .collect();
    xssi(&json!({ "src/a.rs": list }))
}

#[test]
fn refresh_caches_comments_and_merges_drafts_into_threads() {
    let http = RouteHttpClient::new();
    http.route("/changes/7/comments", 200, comments_json());
    http.route("/changes/7/drafts", 200, drafts_json(&["d1"]));
    let syncer = syncer_for(Arc::new(http.clone()), signed_in());
    assert_eq!(syncer.cached_comments(CHANGE).unwrap().fetched_at, None);

    let view = block_on(syncer.refresh_comments(CHANGE)).unwrap();
    assert_eq!(view.comments.len(), 1);
    assert_eq!(view.drafts.len(), 1);
    assert!(view.fetched_at.is_some());
    let threads = view.threads();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].entries.len(), 2);
    assert!(threads[0].has_draft() && !threads[0].is_unresolved());
    assert_eq!(view.drafts[0].sync, DraftSync::Synced);
    assert_eq!(syncer.cached_comments(CHANGE).unwrap(), view);
}

#[test]
fn refresh_uses_the_cached_patch_sets_to_place_server_drafts() {
    let http = RouteHttpClient::new();
    http.route("/changes/7/comments", 200, comments_json());
    http.route("/changes/7/drafts", 200, drafts_json(&["d1"]));
    http.route_json(
        "/changes/7?",
        &json!({
            "_number": 7, "branch": "main", "change_id": "I7", "project": "p",
            "created": "2013-02-01 09:59:32.126000000", "owner": {"_account_id": 1},
            "status": "NEW", "subject": "s", "updated": "2024-02-01 10:00:00.000000000",
            "mergeable": true,
            "current_revision": "shaB",
            "revisions": {"shaA": {"_number": 1}, "shaB": {"_number": 2}}
        })
        .to_string(),
    );
    let syncer = syncer_for(Arc::new(http), signed_in());
    block_on(syncer.refresh_change(CHANGE)).unwrap();
    let view = block_on(syncer.refresh_comments(CHANGE)).unwrap();
    assert_eq!(view.drafts[0].revision.as_str(), "shaB");
}

#[test]
fn refresh_fetches_the_patch_sets_when_a_draft_has_no_commit() {
    let http = RouteHttpClient::new();
    http.route("/changes/7/comments", 200, comments_json());
    http.route(
        "/changes/7/drafts",
        200,
        xssi(&json!({"src/a.rs": [{"patch_set": 2, "id": "d1", "line": 1,
            "updated": "2024-03-02 10:00:00.000000000", "message": "m"}]})),
    );
    http.route_json("/changes/7?", &change_with_ps2());
    let syncer = syncer_for(Arc::new(http.clone()), signed_in());
    let view = block_on(syncer.refresh_comments(CHANGE)).unwrap();
    assert_eq!(view.drafts.len(), 1);
    assert_eq!(view.drafts[0].revision.as_str(), "sha2");
    assert!(
        http.requested()
            .iter()
            .any(|r| r.starts_with("/a/changes/7?"))
    );
}

#[test]
fn refresh_drops_synced_drafts_the_server_no_longer_has_but_keeps_pending_work() {
    let http = RouteHttpClient::new();
    http.route("/changes/7/comments", 200, comments_json());
    http.route("/changes/7/drafts", 200, drafts_json(&["d1", "d2"]));
    http.route_json("/changes/7?", &change_with_ps2());
    let syncer = syncer_for(Arc::new(http.clone()), signed_in());
    block_on(syncer.refresh_change(CHANGE)).unwrap();
    let view = block_on(syncer.refresh_comments(CHANGE)).unwrap();
    assert_eq!(view.drafts.len(), 2);

    // The user edits d2 offline; meanwhile both were published on the web.
    let d2 = view
        .drafts
        .iter()
        .find(|d| d.server_id.as_deref() == Some("d2"))
        .unwrap()
        .local_id;
    syncer
        .save_draft_local(DraftSave::Update {
            id: d2,
            message: "my edit".into(),
            unresolved: true,
        })
        .unwrap();
    http.route("/changes/7/drafts", 200, xssi(&json!({})));
    let view = block_on(syncer.refresh_comments(CHANGE)).unwrap();
    assert_eq!(view.drafts.len(), 1);
    // The edit is not lost: it is created again.
    assert_eq!(view.drafts[0].message, "my edit");
    assert_eq!(view.drafts[0].sync, DraftSync::PendingCreate);
    assert_eq!(view.drafts[0].server_id, None);
}

fn change_with_ps2() -> String {
    json!({
        "_number": 7, "branch": "main", "change_id": "I7", "project": "p",
        "created": "2013-02-01 09:59:32.126000000", "owner": {"_account_id": 1},
        "status": "NEW", "subject": "s", "updated": "2024-02-01 10:00:00.000000000",
            "mergeable": true,
        "current_revision": "sha2",
        "revisions": {"sha2": {"_number": 2}}
    })
    .to_string()
}

#[test]
fn anonymous_refresh_fetches_no_drafts() {
    let http = RouteHttpClient::new();
    http.route("/changes/7/comments", 200, comments_json());
    let syncer = syncer_for(Arc::new(http.clone()), Auth::Anonymous);
    let view = block_on(syncer.refresh_comments(CHANGE)).unwrap();
    assert!(view.drafts.is_empty());
    assert_eq!(http.requested(), ["/changes/7/comments"]);
}

#[test]
fn refresh_survives_a_failing_drafts_request_but_not_a_login_problem() {
    let http = RouteHttpClient::new();
    http.route("/changes/7/comments", 200, comments_json());
    http.route("/changes/7/drafts", 500, "boom");
    let syncer = syncer_for(Arc::new(http.clone()), signed_in());
    let view = block_on(syncer.refresh_comments(CHANGE)).unwrap();
    assert_eq!(view.comments.len(), 1);

    http.route("/changes/7/drafts", 401, "");
    assert!(
        block_on(syncer.refresh_comments(CHANGE))
            .unwrap_err()
            .is_unauthorized()
    );
    http.route("/changes/7/comments", 500, "boom");
    assert!(block_on(syncer.refresh_comments(CHANGE)).is_err());
    // The cache still has the last good comments.
    assert_eq!(syncer.cached_comments(CHANGE).unwrap().comments.len(), 1);
}

#[test]
fn a_draft_uploaded_during_a_refresh_is_not_lost() {
    // The refresh's draft list predates the upload (the upload is recorded
    // after the fetch began); the draft must survive the merge.
    let f = fx();
    f.http.push_response(201, draft_reply("srv1", "x"));
    let d = block_on(f.syncer.save_draft(create("x"))).unwrap().unwrap();
    let store_started = Timestamp::now();
    // Merge a list taken "before" the upload.
    let earlier = store_started
        .checked_sub(jiff::SignedDuration::from_secs(60))
        .unwrap();
    f.syncer
        .store()
        .merge_server_drafts(f.syncer.server, CHANGE, &[], &[], earlier, Timestamp::now())
        .unwrap();
    assert!(
        f.syncer
            .store()
            .load_draft(f.syncer.server, d.local_id)
            .unwrap()
            .is_some()
    );
}

// ---- publishing ----

fn review_with_vote() -> ReviewInput {
    let mut r = ReviewInput::with_message("Looks good");
    r.labels.insert("Code-Review".into(), 1);
    r
}

fn sha2() -> RevisionId {
    ps(2).sha
}

/// Replies for the refreshes after a published review: change detail,
/// comments, drafts.
fn push_refresh(http: &FakeHttpClient) {
    http.push_response(
        200,
        xssi(&serde_json::from_str(&change_with_ps2()).unwrap()),
    );
    http.push_response(200, comments_json());
    http.push_response(200, xssi(&json!({})));
}

#[test]
fn publishing_uploads_pending_drafts_first_and_then_posts_the_review() {
    let f = fx();
    f.http.push_error("offline");
    let d = block_on(f.syncer.save_draft(create("pending")))
        .unwrap()
        .unwrap();
    assert_eq!(d.sync, DraftSync::PendingCreate);
    let skip = f.http.requests().len();

    f.http.push_response(201, draft_reply("srv1", "pending"));
    f.http
        .push_response(200, xssi(&json!({"labels": {"Code-Review": 1}})));
    push_refresh(&f.http);
    let out = block_on(
        f.syncer
            .publish_review(CHANGE, &sha2(), &review_with_vote()),
    )
    .unwrap();

    assert_eq!(out.result.labels["Code-Review"], 1);
    assert!(out.detail.is_some());
    let comments = out.comments.unwrap();
    assert_eq!(comments.comments.len(), 1);
    // The published draft is gone locally, the server's list being empty.
    assert!(comments.drafts.is_empty());
    assert!(
        f.syncer
            .store()
            .load_draft(f.syncer.server, d.local_id)
            .unwrap()
            .is_none()
    );

    let sent = uris(&f.http).split_off(skip);
    assert_eq!(
        sent,
        [
            "PUT /a/changes/7/revisions/sha2/drafts",
            "POST /a/changes/7/revisions/sha2/review",
            "GET /a/changes/7",
            "GET /a/changes/7/comments",
            "GET /a/changes/7/drafts",
        ]
    );
    let body: Value = serde_json::from_slice(f.http.requests()[skip + 1].body()).unwrap();
    assert_eq!(body["drafts"], "PUBLISH_ALL_REVISIONS");
    assert_eq!(body["labels"], json!({"Code-Review": 1}));
    assert_eq!(body["message"], "Looks good");
}

#[test]
fn publishing_refuses_when_drafts_cannot_be_uploaded() {
    let f = fx();
    f.http.push_error("offline");
    let d = block_on(f.syncer.save_draft(create("pending")))
        .unwrap()
        .unwrap();
    f.http.push_response(201, draft_reply("srv2", "synced"));
    block_on(f.syncer.save_draft(create("synced"))).unwrap();
    let before = f.http.requests().len();

    // Still offline.
    f.http.push_error("offline");
    let err = block_on(
        f.syncer
            .publish_review(CHANGE, &sha2(), &review_with_vote()),
    )
    .unwrap_err();
    match err {
        Error::DraftsNotUploaded(list) => {
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].local_id, d.local_id);
            assert_eq!(list[0].path, "src/a.rs");
            assert!(list[0].reason.contains("offline"), "{}", list[0].reason);
        }
        other => panic!("{other}"),
    }
    // Only the upload attempt was made: no review was posted.
    assert_eq!(f.http.requests().len(), before + 1);
    assert_eq!(draft(&f, d.local_id).sync, DraftSync::PendingCreate);
}

#[test]
fn publishing_also_refuses_failed_drafts_and_unconfirmed_deletes() {
    let f = fx();
    f.http.push_response(409, "change is closed");
    block_on(f.syncer.save_draft(create("failed"))).unwrap();
    f.http.push_response(201, draft_reply("srv2", "x"));
    let d = block_on(f.syncer.save_draft(create("deleted")))
        .unwrap()
        .unwrap();
    f.http.push_error("offline");
    block_on(f.syncer.delete_draft(d.local_id)).unwrap();
    // The upload of the pending delete is attempted, then fails again.
    f.http.push_error("offline");
    let err = block_on(
        f.syncer
            .publish_review(CHANGE, &sha2(), &review_with_vote()),
    )
    .unwrap_err();
    let Error::DraftsNotUploaded(list) = err else {
        panic!("{err}");
    };
    assert_eq!(list.len(), 2);
    assert!(list.iter().any(|d| d.reason.contains("change is closed")));
    assert!(err_text(&list).contains("src/a.rs (patch set 2)"));
}

fn err_text(list: &[PendingDraft]) -> String {
    Error::DraftsNotUploaded(list.to_vec()).to_string()
}

#[test]
fn publishing_with_keep_ignores_pending_drafts() {
    let f = fx();
    f.http.push_error("offline");
    block_on(f.syncer.save_draft(create("pending"))).unwrap();
    let before = f.http.requests().len();
    f.http.push_response(200, xssi(&json!({})));
    push_refresh(&f.http);
    let mut r = review_with_vote();
    r.drafts = DraftHandling::Keep;
    block_on(f.syncer.publish_review(CHANGE, &sha2(), &r)).unwrap();
    let sent = uris(&f.http).split_off(before);
    assert_eq!(sent[0], "POST /a/changes/7/revisions/sha2/review");
    let body: Value = serde_json::from_slice(f.http.requests()[before].body()).unwrap();
    assert_eq!(body["drafts"], "KEEP");
    // The draft is still there.
    assert_eq!(f.syncer.cached_comments(CHANGE).unwrap().drafts.len(), 1);
}

#[test]
fn publishing_errors_are_typed_and_leave_drafts_alone() {
    let f = fx();
    f.http.push_response(201, draft_reply("srv1", "x"));
    block_on(f.syncer.save_draft(create("x"))).unwrap();
    for (code, text) in [
        (400, "label \"Code-Review\": 2 is not a permitted value"),
        (409, "change is closed"),
        (403, "not permitted"),
    ] {
        f.http.push_response(code, text);
        let err = block_on(
            f.syncer
                .publish_review(CHANGE, &sha2(), &review_with_vote()),
        )
        .unwrap_err();
        let Error::Api(api) = &err else {
            panic!("{err}");
        };
        assert!(err.to_string().contains(text), "{err}");
        assert!(!api.is_retryable());
        assert!(!err.is_unauthorized());
    }
    f.http.push_response(401, "");
    assert!(
        block_on(
            f.syncer
                .publish_review(CHANGE, &sha2(), &review_with_vote())
        )
        .unwrap_err()
        .is_unauthorized()
    );
    f.http.push_error("offline");
    let err = block_on(
        f.syncer
            .publish_review(CHANGE, &sha2(), &review_with_vote()),
    )
    .unwrap_err();
    assert!(matches!(&err, Error::Api(e) if e.is_retryable()));
    // The synced draft is still a draft after all those failures.
    assert_eq!(f.syncer.cached_comments(CHANGE).unwrap().drafts.len(), 1);
}

#[test]
fn a_failing_refresh_after_publishing_is_not_an_error() {
    let f = fx();
    f.http.push_response(200, xssi(&json!({"labels": {}})));
    f.http.push_error("offline"); // change
    f.http.push_error("offline"); // comments
    f.http.push_error("offline"); // drafts
    let out = block_on(
        f.syncer
            .publish_review(CHANGE, &sha2(), &review_with_vote()),
    )
    .unwrap();
    assert!(out.detail.is_none() && out.comments.is_none());
}

#[test]
fn the_dev_gerrit_style_partial_failure_is_reported_in_the_result() {
    let f = fx();
    f.http.push_response(
        200,
        xssi(&json!({"error": "Not Found: bob", "labels": {"Code-Review": 1}})),
    );
    push_refresh(&f.http);
    let out = block_on(
        f.syncer
            .publish_review(CHANGE, &sha2(), &review_with_vote()),
    )
    .unwrap();
    assert_eq!(out.result.error.as_deref(), Some("Not Found: bob"));
}

// ---- reviewed marks ----

fn files_key() -> FilesKey {
    FilesKey {
        change: CHANGE,
        base: None,
        target: ps(2),
    }
}

fn cache_files(f: &Fx, reviewed: &[&str]) {
    let files: Vec<model::FileEntry> = ["a.rs", "b.rs"]
        .into_iter()
        .map(|p| model::FileEntry {
            path: p.into(),
            status: model::FileStatus::Modified,
            old_path: None,
            lines_inserted: 0,
            lines_deleted: 0,
            size_delta: 0,
            size: 0,
            binary: false,
        })
        .collect();
    let reviewed: Vec<String> = reviewed.iter().map(|s| (*s).to_owned()).collect();
    f.syncer
        .store()
        .save_file_list(
            f.syncer.server,
            &files_key(),
            &files,
            Some(&reviewed),
            Timestamp::now(),
        )
        .unwrap();
}

fn reviewed(f: &Fx) -> Vec<String> {
    f.syncer
        .cached_files(&files_key())
        .unwrap()
        .unwrap()
        .reviewed
        .unwrap()
}

#[test]
fn setting_a_reviewed_mark_updates_the_cache_and_the_server() {
    let f = fx();
    cache_files(&f, &["a.rs"]);
    f.http.push_response(201, "");
    block_on(f.syncer.set_reviewed(CHANGE, &sha2(), "b.rs", true)).unwrap();
    assert_eq!(reviewed(&f), ["a.rs", "b.rs"]);
    f.http.push_response(204, "");
    block_on(f.syncer.set_reviewed(CHANGE, &sha2(), "a.rs", false)).unwrap();
    assert_eq!(reviewed(&f), ["b.rs"]);
    assert_eq!(
        uris(&f.http),
        [
            "PUT /a/changes/7/revisions/sha2/files/b.rs/reviewed",
            "DELETE /a/changes/7/revisions/sha2/files/a.rs/reviewed"
        ]
    );
}

#[test]
fn a_failed_reviewed_mark_is_put_back() {
    let f = fx();
    cache_files(&f, &["a.rs"]);
    // Rejected, offline and logged out all revert.
    f.http.push_response(403, "no");
    assert!(block_on(f.syncer.set_reviewed(CHANGE, &sha2(), "b.rs", true)).is_err());
    assert_eq!(reviewed(&f), ["a.rs"]);
    f.http.push_error("offline");
    let err = block_on(f.syncer.set_reviewed(CHANGE, &sha2(), "a.rs", false)).unwrap_err();
    assert!(matches!(err, Error::Api(ref e) if e.is_retryable()));
    assert_eq!(reviewed(&f), ["a.rs"]);
    f.http.push_response(401, "");
    assert!(
        block_on(f.syncer.set_reviewed(CHANGE, &sha2(), "b.rs", true))
            .unwrap_err()
            .is_unauthorized()
    );
    assert_eq!(reviewed(&f), ["a.rs"]);
}

#[test]
fn a_redundant_failed_mark_does_not_flip_the_cache() {
    let f = fx();
    cache_files(&f, &["a.rs"]);
    // Already marked: the cache does not change, so nothing to put back.
    f.http.push_error("offline");
    assert!(block_on(f.syncer.set_reviewed(CHANGE, &sha2(), "a.rs", true)).is_err());
    assert_eq!(reviewed(&f), ["a.rs"]);
}

#[test]
fn anonymous_sessions_cannot_write() {
    let http = FakeHttpClient::new();
    let syncer = syncer_for(Arc::new(http.clone()), Auth::Anonymous);
    let err = block_on(syncer.save_draft(create("x"))).unwrap_err();
    // The draft is stored, the upload refused without a request.
    assert!(err.is_unauthorized());
    assert!(http.requests().is_empty());
    let mut keep = review_with_vote();
    keep.drafts = DraftHandling::Keep;
    assert!(
        block_on(syncer.publish_review(CHANGE, &sha2(), &keep))
            .unwrap_err()
            .is_unauthorized()
    );
    assert!(http.requests().is_empty());
}

#[test]
fn draft_counts_follow_a_draft_through_pending_failed_and_synced() {
    let f = fx();
    assert!(f.syncer.draft_counts().unwrap().is_empty());
    // Offline: pending, and counted as such.
    f.http.push_error("dns error");
    let d = block_on(f.syncer.save_draft(create("a"))).unwrap().unwrap();
    assert_eq!(
        f.syncer.draft_counts().unwrap(),
        DraftCounts {
            pending: 1,
            failed: 0
        }
    );
    // Refused: failed, no longer pending.
    f.http.push_response(409, "closed");
    block_on(f.syncer.retry_pending_drafts()).unwrap();
    assert_eq!(
        f.syncer.draft_counts().unwrap(),
        DraftCounts {
            pending: 0,
            failed: 1
        }
    );
    // A manual retry that works clears it.
    f.http.push_response(201, draft_reply("srv1", "a"));
    block_on(f.syncer.retry_failed_draft(d.local_id)).unwrap();
    assert!(f.syncer.draft_counts().unwrap().is_empty());
}

#[test]
fn retrying_failed_drafts_leaves_the_definite_rejections_alone() {
    let f = fx();
    f.http.push_response(500, "oops");
    let flaky = block_on(f.syncer.save_draft(create("flaky")))
        .unwrap()
        .unwrap();
    f.http.push_response(409, "closed");
    let refused = block_on(f.syncer.save_draft(create("refused")))
        .unwrap()
        .unwrap();
    assert_eq!(
        f.syncer.draft_counts().unwrap(),
        DraftCounts {
            pending: 0,
            failed: 2
        }
    );
    f.http.push_response(201, draft_reply("srv1", "flaky"));
    let report = block_on(f.syncer.retry_failed_drafts()).unwrap();
    assert_eq!((report.synced, report.rejected), (1, 0));
    assert_eq!(draft(&f, flaky.local_id).sync, DraftSync::Synced);
    assert!(matches!(
        draft(&f, refused.local_id).sync,
        DraftSync::Failed { .. }
    ));
    assert_eq!(f.http.requests().len(), 3, "the refused one was not sent");
}
