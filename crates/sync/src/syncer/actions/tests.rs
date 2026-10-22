use std::sync::{Arc, Mutex};

use futures::executor::block_on;
use gerrit_api::{Auth, GerritClient, fake::RouteHttpClient};
use model::ChangeStatus;
use serde_json::{Value, json};
use store::Store;

use super::*;

const BASE: &str = "https://g.example";
const CHANGE: ChangeNumber = ChangeNumber(7);

fn syncer_for(http: &RouteHttpClient, auth: Auth) -> Syncer {
    let client = GerritClient::new(Arc::new(http.clone()), BASE, auth).unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let server = store.server_id(BASE).unwrap();
    Syncer::new(client, Arc::new(Mutex::new(store)), server)
}

fn signed_in() -> Auth {
    Auth::Basic {
        username: "u".into(),
        password: "p".into(),
    }
}

/// A change as the server reports it, with the actions it allows.
fn change_json(status: &str, actions: Value) -> String {
    json!({
        "_number": 7, "branch": "main", "change_id": "I7", "project": "p",
        "created": "2013-02-01 09:59:32.126000000", "owner": {"_account_id": 1},
        "status": status, "subject": "s", "updated": "2024-02-01 10:00:00.000000000",
        "actions": actions,
    })
    .to_string()
}

fn routes(status: &str, actions: Value) -> RouteHttpClient {
    let http = RouteHttpClient::new();
    http.route_json("GET /changes/7?", &change_json(status, actions));
    http
}

#[test]
fn abandon_posts_then_caches_the_new_state_and_actions() {
    let http = routes("ABANDONED", json!({"restore": {"enabled": true}}));
    http.route_json(
        "POST /changes/7/abandon",
        &change_json("ABANDONED", json!({})),
    );
    let syncer = syncer_for(&http, signed_in());

    let detail = block_on(syncer.abandon(CHANGE, Some("obsolete")))
        .unwrap()
        .unwrap();
    assert_eq!(detail.summary.status, ChangeStatus::Abandoned);
    assert!(detail.action("restore").unwrap().enabled);
    assert!(detail.action("abandon").is_none());

    let sent = http.sent();
    assert_eq!(sent[0].method, "POST");
    assert_eq!(sent[0].target, "/a/changes/7/abandon");
    assert_eq!(sent[0].body_text(), r#"{"message":"obsolete"}"#);
    assert_eq!(sent[1].method, "GET");

    let cached = syncer.cached_change(CHANGE).unwrap().unwrap();
    assert_eq!(cached.detail, detail);
}

#[test]
fn each_action_hits_its_endpoint_and_refreshes() {
    let http = routes("NEW", json!({}));
    let ok = change_json("NEW", json!({}));
    for path in ["submit", "rebase", "restore"] {
        http.route_json(&format!("POST /changes/7/{path}"), &ok);
    }
    for path in ["wip", "ready", "private"] {
        http.route(&format!("POST /changes/7/{path}"), 200, "");
    }
    http.route("DELETE /changes/7/private", 204, "");
    let syncer = syncer_for(&http, signed_in());

    assert!(block_on(syncer.submit(CHANGE)).unwrap().is_some());
    assert!(
        block_on(syncer.rebase(CHANGE, Some("abc"), Default::default()))
            .unwrap()
            .is_some()
    );
    assert!(block_on(syncer.restore(CHANGE, None)).unwrap().is_some());
    assert!(
        block_on(syncer.set_work_in_progress(CHANGE, true, None))
            .unwrap()
            .is_some()
    );
    assert!(
        block_on(syncer.set_work_in_progress(CHANGE, false, None))
            .unwrap()
            .is_some()
    );
    assert!(
        block_on(syncer.set_private(CHANGE, true))
            .unwrap()
            .is_some()
    );
    assert!(
        block_on(syncer.set_private(CHANGE, false))
            .unwrap()
            .is_some()
    );

    let sent: Vec<_> = http
        .sent()
        .into_iter()
        .map(|r| format!("{} {}", r.method, r.target.split('?').next().unwrap_or("")))
        .collect();
    let expect = [
        "POST /a/changes/7/submit",
        "POST /a/changes/7/rebase",
        "POST /a/changes/7/restore",
        "POST /a/changes/7/wip",
        "POST /a/changes/7/ready",
        "POST /a/changes/7/private",
        "DELETE /a/changes/7/private",
    ];
    let posts: Vec<_> = sent.iter().filter(|s| !s.starts_with("GET")).collect();
    assert_eq!(posts, expect);
    assert_eq!(sent.iter().filter(|s| s.starts_with("GET")).count(), 7);
}

#[test]
fn rebase_options_reach_the_server_and_the_change_is_refreshed() {
    let http = routes("NEW", json!({}));
    http.route_json("POST /changes/7/rebase", &change_json("NEW", json!({})));
    let syncer = syncer_for(&http, signed_in());
    let options = model::RebaseOptions {
        allow_conflicts: true,
        on_behalf_of_uploader: true,
    };
    assert!(
        block_on(syncer.rebase(CHANGE, None, options))
            .unwrap()
            .is_some()
    );
    let sent = http.sent();
    assert_eq!(sent[0].target, "/a/changes/7/rebase");
    assert_eq!(
        sent[0].body_text(),
        r#"{"allow_conflicts":true,"on_behalf_of_uploader":true}"#
    );
    assert_eq!(sent[1].method, "GET");
}

#[test]
fn refused_actions_leave_the_cache_alone() {
    let http = routes("NEW", json!({"submit": {"enabled": true}}));
    let syncer = syncer_for(&http, signed_in());
    let before = block_on(syncer.refresh_change(CHANGE)).unwrap();

    http.route("POST /changes/7/submit", 409, "change is not submittable\n");
    http.route("POST /changes/7/abandon", 403, "not allowed to abandon\n");
    http.route("POST /changes/7/rebase", 401, "");
    let requests_before = http.sent().len();

    let err = block_on(syncer.submit(CHANGE)).unwrap_err();
    assert!(
        matches!(&err, Error::Api(gerrit_api::Error::Conflict { message }) if message == "change is not submittable"),
        "{err:?}"
    );
    let err = block_on(syncer.abandon(CHANGE, None)).unwrap_err();
    assert!(
        matches!(&err, Error::Api(gerrit_api::Error::Forbidden { .. })),
        "{err:?}"
    );
    assert!(!err.is_unauthorized());
    let err = block_on(syncer.rebase(CHANGE, None, Default::default())).unwrap_err();
    assert!(err.is_unauthorized());

    // No refresh was attempted and the cached detail is unchanged.
    assert_eq!(http.sent().len(), requests_before + 3);
    assert_eq!(
        syncer.cached_change(CHANGE).unwrap().unwrap().detail,
        before
    );
}

#[test]
fn a_refusal_fetches_the_change_but_other_failures_do_not() {
    let http = routes("NEW", json!({"submit": {"enabled": true}}));
    let syncer = syncer_for(&http, signed_in());
    block_on(syncer.refresh_change(CHANGE)).unwrap();

    // Meanwhile the change was merged elsewhere; the submit is refused.
    http.route_json("GET /changes/7?", &change_json("MERGED", json!({})));
    http.route("POST /changes/7/submit", 409, "change is merged\n");
    http.route("POST /changes/7/abandon", 403, "no\n");
    http.route("POST /changes/7/rebase", 401, "");
    http.route("POST /changes/7/restore", 503, "down");
    let err = block_on(syncer.submit(CHANGE)).unwrap_err();
    let detail = block_on(syncer.refresh_after_refusal(CHANGE, &err)).unwrap();
    assert_eq!(detail.summary.status, ChangeStatus::Merged);
    assert_eq!(
        syncer.cached_change(CHANGE).unwrap().unwrap().detail,
        detail
    );

    // 403 counts too.
    let err = block_on(syncer.abandon(CHANGE, None)).unwrap_err();
    assert!(block_on(syncer.refresh_after_refusal(CHANGE, &err)).is_some());

    // 401, a transient 5xx and an unreachable server do not.
    let n = http.sent().len();
    for err in [
        block_on(syncer.rebase(CHANGE, None, Default::default())).unwrap_err(),
        block_on(syncer.restore(CHANGE, None)).unwrap_err(),
        Error::Api(gerrit_api::Error::Http(gerrit_api::HttpError(
            "reset".into(),
        ))),
    ] {
        assert!(block_on(syncer.refresh_after_refusal(CHANGE, &err)).is_none());
    }
    // Only the two writes went out.
    assert_eq!(http.sent().len(), n + 2);
}

#[test]
fn a_failed_refresh_after_an_accepted_action_is_not_an_error() {
    let http = RouteHttpClient::new();
    http.route("POST /changes/7/wip", 200, "");
    http.route("GET /changes/7?", 503, "down");
    let syncer = syncer_for(&http, signed_in());
    assert_eq!(
        block_on(syncer.set_work_in_progress(CHANGE, true, None)).unwrap(),
        None
    );
    assert!(syncer.cached_change(CHANGE).unwrap().is_none());
}

#[test]
fn anonymous_sessions_cannot_run_actions() {
    let http = routes("NEW", json!({}));
    let syncer = syncer_for(&http, Auth::Anonymous);
    assert!(
        block_on(syncer.submit(CHANGE))
            .unwrap_err()
            .is_unauthorized()
    );
    assert!(http.sent().is_empty());
}
