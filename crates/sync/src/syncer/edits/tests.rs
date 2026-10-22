use std::sync::{Arc, Mutex};

use futures::executor::block_on;
use gerrit_api::{Auth, GerritClient, fake::RouteHttpClient};
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

fn change_json(extra: Value) -> String {
    let mut v = json!({
        "_number": 7, "branch": "main", "change_id": "I7", "project": "p",
        "created": "2013-02-01 09:59:32.126000000", "owner": {"_account_id": 1},
        "status": "NEW", "subject": "s", "updated": "2024-02-01 10:00:00.000000000",
    });
    v.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    v.to_string()
}

fn sent(http: &RouteHttpClient) -> Vec<String> {
    http.sent()
        .into_iter()
        .map(|r| format!("{} {}", r.method, r.target.split('?').next().unwrap_or("")))
        .collect()
}

#[test]
fn adding_a_reviewer_then_caches_the_change_with_them() {
    let http = RouteHttpClient::new();
    http.route_json(
        "GET /changes/7?",
        &change_json(json!({"reviewers": {
            "REVIEWER": [{"_account_id": 3, "name": "Bob"}],
            "CC": [{"_account_id": 4, "name": "Carol"}],
        }})),
    );
    http.route_json(
        "POST /changes/7/reviewers",
        r#"{"input":"bob","reviewers":[{"_account_id":3}]}"#,
    );
    let syncer = syncer_for(&http, signed_in());

    let added =
        block_on(syncer.add_reviewer(CHANGE, "bob", ReviewerState::Reviewer, false)).unwrap();
    assert!(added.result.is_added());
    let detail = added.detail.unwrap();
    assert_eq!(detail.reviewers[0].id.0, 3);
    assert_eq!(detail.cc[0].id.0, 4);
    assert_eq!(
        http.sent()[0].body_text(),
        r#"{"reviewer":"bob","state":"REVIEWER"}"#
    );
    assert_eq!(
        syncer.cached_change(CHANGE).unwrap().unwrap().detail,
        detail
    );
}

#[test]
fn an_add_the_server_did_not_do_leaves_the_cache_alone() {
    let http = RouteHttpClient::new();
    http.route_json("GET /changes/7?", &change_json(json!({})));
    http.route_json(
        "POST /changes/7/reviewers",
        r#"{"input":"zed","error":"Account 'zed' not found"}"#,
    );
    let syncer = syncer_for(&http, signed_in());
    let before = block_on(syncer.refresh_change(CHANGE)).unwrap();
    let n = http.sent().len();

    let added = block_on(syncer.add_reviewer(CHANGE, "zed", ReviewerState::Cc, false)).unwrap();
    assert!(!added.result.is_added());
    assert_eq!(
        added.result.error.as_deref(),
        Some("Account 'zed' not found")
    );
    assert!(added.detail.is_none());
    assert_eq!(http.sent().len(), n + 1, "no refresh");
    assert_eq!(
        syncer.cached_change(CHANGE).unwrap().unwrap().detail,
        before
    );

    // A large group asks for confirmation: also not added.
    http.route_json(
        "POST /changes/7/reviewers",
        r#"{"input":"g","confirm":true,"error":"has 50 members"}"#,
    );
    let added = block_on(syncer.add_reviewer(CHANGE, "g", ReviewerState::Reviewer, false)).unwrap();
    assert!(added.result.confirm && added.detail.is_none());
    assert_eq!(http.sent().len(), n + 2);
}

#[test]
fn each_edit_hits_its_endpoint_and_refreshes() {
    let http = RouteHttpClient::new();
    http.route_json("GET /changes/7?", &change_json(json!({"topic": "t"})));
    http.route("DELETE /changes/7/reviewers/3", 204, "");
    http.route_json("PUT /changes/7/topic", r#""t""#);
    http.route("DELETE /changes/7/topic", 204, "");
    http.route_json("POST /changes/7/hashtags", r#"["a"]"#);
    let syncer = syncer_for(&http, signed_in());

    assert!(
        block_on(syncer.remove_reviewer(CHANGE, "3"))
            .unwrap()
            .is_some()
    );
    let d = block_on(syncer.set_topic(CHANGE, Some("t")))
        .unwrap()
        .unwrap();
    assert_eq!(d.summary.topic.as_deref(), Some("t"));
    assert!(block_on(syncer.set_topic(CHANGE, None)).unwrap().is_some());
    assert!(
        block_on(syncer.edit_hashtags(CHANGE, &["a".into()], &[]))
            .unwrap()
            .is_some()
    );
    let all = sent(&http);
    let writes: Vec<_> = all.iter().filter(|s| !s.starts_with("GET")).collect();
    assert_eq!(
        writes,
        [
            "DELETE /a/changes/7/reviewers/3",
            "PUT /a/changes/7/topic",
            "DELETE /a/changes/7/topic",
            "POST /a/changes/7/hashtags",
        ]
    );
    assert_eq!(all.iter().filter(|s| s.starts_with("GET")).count(), 4);
}

#[test]
fn refused_edits_leave_the_cache_alone() {
    let http = RouteHttpClient::new();
    http.route_json("GET /changes/7?", &change_json(json!({})));
    let syncer = syncer_for(&http, signed_in());
    let before = block_on(syncer.refresh_change(CHANGE)).unwrap();
    http.route("DELETE /changes/7/reviewers/3", 403, "not allowed\n");
    http.route("PUT /changes/7/topic", 409, "frozen\n");
    http.route("POST /changes/7/hashtags", 401, "");
    http.route("POST /changes/7/reviewers", 403, "no\n");
    let n = http.sent().len();

    let err = block_on(syncer.remove_reviewer(CHANGE, "3")).unwrap_err();
    assert!(matches!(
        &err,
        Error::Api(gerrit_api::Error::Forbidden { .. })
    ));
    let err = block_on(syncer.set_topic(CHANGE, Some("x"))).unwrap_err();
    assert!(
        matches!(&err, Error::Api(gerrit_api::Error::Conflict { message }) if message == "frozen")
    );
    let err = block_on(syncer.edit_hashtags(CHANGE, &["a".into()], &[])).unwrap_err();
    assert!(err.is_unauthorized());
    let err = block_on(syncer.add_reviewer(CHANGE, "bob", ReviewerState::Cc, false)).unwrap_err();
    assert!(matches!(
        &err,
        Error::Api(gerrit_api::Error::Forbidden { .. })
    ));

    assert_eq!(http.sent().len(), n + 4, "no refresh");
    assert_eq!(
        syncer.cached_change(CHANGE).unwrap().unwrap().detail,
        before
    );
}

#[test]
fn suggestions_are_not_cached_and_blank_queries_send_nothing() {
    let http = RouteHttpClient::new();
    http.route_json(
        "GET /changes/7/suggest_reviewers",
        r#"[{"account":{"_account_id":3,"name":"Bob"}},{"group":{"id":"g","name":"G"}}]"#,
    );
    let syncer = syncer_for(&http, signed_in());
    assert!(
        block_on(syncer.suggest_reviewers(CHANGE, "  "))
            .unwrap()
            .is_empty()
    );
    assert!(http.sent().is_empty());
    let got = block_on(syncer.suggest_reviewers(CHANGE, "bo")).unwrap();
    assert_eq!(got.len(), 2);
    assert!(http.sent()[0].target.contains("q=bo&n=10"));
    assert!(syncer.cached_change(CHANGE).unwrap().is_none());
}

#[test]
fn anonymous_sessions_cannot_edit() {
    let http = RouteHttpClient::new();
    let syncer = syncer_for(&http, Auth::Anonymous);
    assert!(
        block_on(syncer.set_topic(CHANGE, Some("t")))
            .unwrap_err()
            .is_unauthorized()
    );
    assert!(
        block_on(syncer.add_reviewer(CHANGE, "bob", ReviewerState::Cc, false))
            .unwrap_err()
            .is_unauthorized()
    );
    assert!(http.sent().is_empty());
}
