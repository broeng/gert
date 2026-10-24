use std::sync::{Arc, Mutex};

use futures::executor::block_on;
use gerrit_api::{Auth, GerritClient, fake::RouteHttpClient};
use model::{ChangeNumber, EventKind};
use serde_json::{Value, json};
use store::Store;

use super::*;

const BASE: &str = "https://g.example";
const Q: &str = "is:open owner:self";
const U1: &str = "2024-02-01 10:00:00.000000000";
const U2: &str = "2024-02-02 10:00:00.000000000";
const U3: &str = "2024-02-03 10:00:00.000000000";

/// A reply to the list query. Only `_number` and `updated` matter to it.
fn list(changes: &[(u64, &str)]) -> String {
    let items: Vec<Value> = changes
        .iter()
        .map(|(n, u)| change(*n, u, json!({})))
        .collect();
    format!(")]}}'\n{}", Value::Array(items))
}

/// A change detail; `extra` is merged over the plain change.
fn change(n: u64, updated: &str, extra: Value) -> Value {
    let mut v = json!({
        "_number": n, "branch": "main", "change_id": format!("I{n:040}"),
        "project": "p", "created": "2024-02-01 09:00:00.000000000",
        "owner": {"_account_id": 1}, "status": "NEW",
        "subject": format!("change {n}"), "updated": updated,
    });
    v.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    v
}

fn route_change(http: &RouteHttpClient, n: u64, updated: &str, extra: Value) {
    http.route_json(
        &format!("GET /changes/{n}?"),
        &change(n, updated, extra).to_string(),
    );
}

fn code_review(value: i32) -> Value {
    json!({"labels": {"Code-Review": {"all": [
        {"_account_id": 2, "value": value, "date": "2024-02-02 09:00:00.000000000"}
    ]}}})
}

struct Fixture {
    http: RouteHttpClient,
    syncer: Syncer,
}

fn fixture() -> Fixture {
    let http = RouteHttpClient::new();
    let client = GerritClient::new(Arc::new(http.clone()), BASE, Auth::Anonymous).unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let server = store.server_id(BASE).unwrap();
    Fixture {
        http,
        syncer: Syncer::new(client, Arc::new(Mutex::new(store)), server),
    }
}

fn pass(f: &Fixture) -> Vec<Event> {
    block_on(f.syncer.watch_pass(Q, WatchOptions::default())).unwrap()
}

fn watched(f: &Fixture) -> bool {
    f.syncer
        .store()
        .watched_since(f.syncer.server)
        .unwrap()
        .is_some()
}

fn fetches(f: &Fixture) -> usize {
    f.http
        .requested()
        .iter()
        .filter(|r| r.starts_with("/changes/") && !r.starts_with("/changes/?"))
        .count()
}

/// A watched server with change 1 cached at `U1` and no events pending.
fn baselined() -> Fixture {
    let f = fixture();
    f.http
        .route_json("GET /changes/?q=", &list(&[(1, U1)])[5..]);
    route_change(&f.http, 1, U1, json!({}));
    assert!(pass(&f).is_empty());
    assert!(watched(&f));
    f
}

#[test]
fn the_baseline_pass_caches_and_reports_nothing() {
    let f = fixture();
    f.http
        .route_json("GET /changes/?q=", &list(&[(1, U1), (2, U1)])[5..]);
    route_change(&f.http, 1, U1, code_review(-1));
    route_change(&f.http, 2, U1, json!({}));
    assert!(!watched(&f));
    assert!(pass(&f).is_empty());
    assert!(watched(&f));
    assert!(f.syncer.cached_change(ChangeNumber(1)).unwrap().is_some());
    assert!(f.syncer.cached_change(ChangeNumber(2)).unwrap().is_some());
}

#[test]
fn a_changed_vote_after_the_baseline_is_one_event() {
    let f = baselined();
    f.http
        .route_json("GET /changes/?q=", &list(&[(1, U2)])[5..]);
    route_change(&f.http, 1, U2, code_review(-1));
    let events = pass(&f);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].change, ChangeNumber(1));
    assert_eq!(events[0].server, f.syncer.server);
    assert_eq!(
        events[0].kind,
        EventKind::Vote {
            label: "Code-Review".into(),
            value: -1,
            previous: None
        }
    );
    assert_eq!(events[0].actor.as_ref().map(|a| a.id.0), Some(2));
}

#[test]
fn a_repeated_pass_reports_nothing_more() {
    let f = baselined();
    f.http
        .route_json("GET /changes/?q=", &list(&[(1, U2)])[5..]);
    route_change(&f.http, 1, U2, code_review(-1));
    assert_eq!(pass(&f).len(), 1);
    let before = fetches(&f);
    assert!(pass(&f).is_empty());
    // Nothing moved, so nothing was fetched either.
    assert_eq!(fetches(&f), before);
}

#[test]
fn the_ledger_suppresses_events_when_the_cache_is_rolled_back() {
    let f = baselined();
    f.http
        .route_json("GET /changes/?q=", &list(&[(1, U2)])[5..]);
    route_change(&f.http, 1, U2, code_review(-1));
    let first = pass(&f);
    assert_eq!(first.len(), 1);
    // As after a lost cache write: the old detail is back, the event is
    // detected again, but it was reported already.
    route_change(&f.http, 1, U1, json!({}));
    block_on(f.syncer.refresh_change(ChangeNumber(1))).unwrap();
    route_change(&f.http, 1, U2, code_review(-1));
    assert!(pass(&f).is_empty());
}

#[test]
fn a_change_never_cached_is_a_baseline_even_on_a_watched_server() {
    let f = baselined();
    f.http
        .route_json("GET /changes/?q=", &list(&[(5, U2), (1, U1)])[5..]);
    route_change(&f.http, 5, U2, code_review(-2));
    assert!(pass(&f).is_empty());
    assert!(f.syncer.cached_change(ChangeNumber(5)).unwrap().is_some());
    // Its later activity is reported.
    f.http
        .route_json("GET /changes/?q=", &list(&[(5, U3), (1, U1)])[5..]);
    route_change(&f.http, 5, U3, code_review(1));
    assert_eq!(pass(&f).len(), 1);
}

#[test]
fn a_failed_fetch_loses_no_other_events_and_is_retried() {
    let f = baselined();
    // Cache change 2 as well.
    f.http
        .route_json("GET /changes/?q=", &list(&[(2, U1), (1, U1)])[5..]);
    route_change(&f.http, 2, U1, json!({}));
    assert!(pass(&f).is_empty());

    f.http
        .route_json("GET /changes/?q=", &list(&[(2, U2), (1, U2)])[5..]);
    f.http.route("GET /changes/2?", 404, "Not found: 2\n");
    route_change(&f.http, 1, U2, code_review(-1));
    let events = pass(&f);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].change, ChangeNumber(1));
    // Change 2 kept its old cache, so it is tried again and now reports.
    let cached = f.syncer.cached_change(ChangeNumber(2)).unwrap().unwrap();
    assert_eq!(
        cached.detail.summary.updated.to_string(),
        "2024-02-01T10:00:00Z"
    );
    route_change(&f.http, 2, U2, code_review(1));
    let events = pass(&f);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].change, ChangeNumber(2));
}

#[test]
fn a_server_that_cannot_answer_stops_the_fetching_but_a_refusal_does_not() {
    let f = baselined();
    f.http
        .route_json("GET /changes/?q=", &list(&[(1, U2), (2, U2), (3, U2)])[5..]);
    // A 404 is the server's answer about that change: all three are tried.
    let before = fetches(&f);
    assert!(pass(&f).is_empty());
    assert_eq!(fetches(&f) - before, 3);
    // A 502 says nothing about the changes: the pass gives up at once.
    f.http.route("GET /changes/1?", 502, "bad gateway\n");
    let before = fetches(&f);
    assert!(pass(&f).is_empty());
    assert_eq!(fetches(&f) - before, 1);
    assert!(watched(&f));
}

#[test]
fn a_query_failure_is_an_error_and_changes_nothing() {
    let f = baselined();
    f.http.route("GET /changes/?q=", 500, "boom\n");
    let r = block_on(f.syncer.watch_pass(Q, WatchOptions::default()));
    assert!(matches!(r, Err(Error::Api(_))));
    assert!(watched(&f));
}

#[test]
fn a_pass_fetches_at_most_the_bound_and_the_rest_follow() {
    let f = baselined();
    // Cache 2..=4.
    f.http.route_json(
        "GET /changes/?q=",
        &list(&[(1, U1), (2, U1), (3, U1), (4, U1)])[5..],
    );
    for n in 2..=4 {
        route_change(&f.http, n, U1, json!({}));
    }
    assert!(pass(&f).is_empty());

    f.http.route_json(
        "GET /changes/?q=",
        &list(&[(1, U2), (2, U2), (3, U2), (4, U2)])[5..],
    );
    for n in 1..=4 {
        route_change(&f.http, n, U2, code_review(-1));
    }
    let options = WatchOptions {
        max_fetches: 3,
        ..WatchOptions::default()
    };
    let before = fetches(&f);
    let first = block_on(f.syncer.watch_pass(Q, options)).unwrap();
    assert_eq!(fetches(&f) - before, 3);
    assert_eq!(first.len(), 3);
    let second = block_on(f.syncer.watch_pass(Q, options)).unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].change, ChangeNumber(4));
    assert!(pass(&f).is_empty());
}

#[test]
fn an_incomplete_first_pass_stays_a_baseline() {
    let f = fixture();
    f.http
        .route_json("GET /changes/?q=", &list(&[(1, U1), (2, U1)])[5..]);
    route_change(&f.http, 1, U1, json!({}));
    route_change(&f.http, 2, U1, json!({}));
    let options = WatchOptions {
        max_fetches: 1,
        ..WatchOptions::default()
    };
    assert!(
        block_on(f.syncer.watch_pass(Q, options))
            .unwrap()
            .is_empty()
    );
    assert!(!watched(&f));
    assert!(pass(&f).is_empty());
    assert!(watched(&f));
}

#[test]
fn the_first_pass_does_not_report_what_the_cache_already_differs_in() {
    // The change page cached an older state before any watching.
    let f = fixture();
    route_change(&f.http, 1, U1, json!({}));
    block_on(f.syncer.refresh_change(ChangeNumber(1))).unwrap();
    f.http
        .route_json("GET /changes/?q=", &list(&[(1, U2)])[5..]);
    route_change(&f.http, 1, U2, code_review(-1));
    assert!(pass(&f).is_empty());
    assert!(watched(&f));
}

#[test]
fn events_come_out_oldest_first() {
    let f = baselined();
    f.http
        .route_json("GET /changes/?q=", &list(&[(2, U1), (1, U2)])[5..]);
    route_change(&f.http, 2, U1, json!({}));
    assert!(pass(&f).is_empty()); // cache 2 as a baseline
    f.http
        .route_json("GET /changes/?q=", &list(&[(2, U3), (1, U2)])[5..]);
    // Change 2 is listed first but its event is the newer one.
    route_change(
        &f.http,
        2,
        U3,
        json!({"labels": {"Code-Review": {"all": [
            {"_account_id": 3, "value": 1, "date": "2024-02-03 09:00:00.000000000"}]}}}),
    );
    route_change(&f.http, 1, U2, code_review(-1));
    let events = pass(&f);
    let order: Vec<_> = events.iter().map(|e| e.change.0).collect();
    assert_eq!(order, [1, 2]);
}
