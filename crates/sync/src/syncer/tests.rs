use std::sync::{Arc, Mutex};

use futures::executor::block_on;
use gerrit_api::{Auth, GerritClient, fake::FakeHttpClient};
use serde_json::{Value, json};

use super::*;

const BASE: &str = "https://g.example";

fn change_json(n: u64, updated: &str) -> Value {
    json!({
        "_number": n,
        "branch": "main",
        "change_id": format!("I{n:040}"),
        "created": "2013-02-01 09:59:32.126000000",
        "id": format!("p~{n}"),
        "owner": {"_account_id": 1},
        "project": "p",
        "status": "NEW",
        "subject": format!("change {n}"),
        "updated": updated,
    })
}

const U1: &str = "2024-02-01 10:00:00.000000000";
const U2: &str = "2024-02-02 10:00:00.000000000";

/// A server reply for a query page; `more` sets `_more_changes` on the
/// last element.
fn page(changes: &[(u64, &str)], more: bool) -> String {
    let mut items: Vec<Value> = changes.iter().map(|(n, u)| change_json(*n, u)).collect();
    if more && let Some(Value::Object(o)) = items.last_mut() {
        o.insert("_more_changes".into(), json!(true));
    }
    format!(")]}}'\n{}", Value::Array(items))
}

struct Fixture {
    http: FakeHttpClient,
    syncer: Syncer,
}

fn fixture() -> Fixture {
    let http = FakeHttpClient::new();
    let client = GerritClient::new(Arc::new(http.clone()), BASE, Auth::Anonymous).unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let server = store.server_id(BASE).unwrap();
    Fixture {
        http,
        syncer: Syncer::new(client, Arc::new(Mutex::new(store)), server),
    }
}

fn numbers(q: &CachedQuery) -> Vec<u64> {
    q.changes.iter().map(|c| c.number.0).collect()
}

#[test]
fn refresh_writes_cache() {
    let f = fixture();
    assert!(f.syncer.cached_query("q").unwrap().is_none());
    f.http.push_response(200, page(&[(2, U1), (1, U1)], true));
    let up = block_on(f.syncer.refresh_query("q", 2)).unwrap();
    assert_eq!(
        up,
        QueryUpdate {
            changed: true,
            cached_count: 2,
            more_changes: true
        }
    );
    let cached = f.syncer.cached_query("q").unwrap().unwrap();
    assert_eq!(numbers(&cached), [2, 1]);
    assert!(cached.more_changes);
}

#[test]
fn unchanged_refresh_reports_no_change() {
    let f = fixture();
    f.http.push_response(200, page(&[(2, U1), (1, U1)], false));
    f.http.push_response(200, page(&[(2, U1), (1, U1)], false));
    assert!(block_on(f.syncer.refresh_query("q", 25)).unwrap().changed);
    let first = f.syncer.cached_query("q").unwrap().unwrap().fetched_at;
    let up = block_on(f.syncer.refresh_query("q", 25)).unwrap();
    assert!(!up.changed);
    assert_eq!(up.cached_count, 2);
    let after = f.syncer.cached_query("q").unwrap().unwrap();
    assert!(after.fetched_at >= first);
}

#[test]
fn changed_updated_reports_change() {
    let f = fixture();
    f.http.push_response(200, page(&[(2, U1), (1, U1)], false));
    f.http.push_response(200, page(&[(2, U1), (1, U2)], false));
    block_on(f.syncer.refresh_query("q", 25)).unwrap();
    assert!(block_on(f.syncer.refresh_query("q", 25)).unwrap().changed);
    let cached = f.syncer.cached_query("q").unwrap().unwrap();
    assert_eq!(
        cached.changes[1].updated.to_string(),
        "2024-02-02T10:00:00Z"
    );
}

#[test]
fn changed_order_membership_or_paging_reports_change() {
    for second in [
        page(&[(1, U1), (2, U1)], false),          // reordered
        page(&[(3, U1), (2, U1), (1, U1)], false), // new change
        page(&[(2, U1)], false),                   // change dropped
        page(&[], false),                          // all dropped
        page(&[(2, U1), (1, U1)], true),           // more_changes flipped
    ] {
        let f = fixture();
        f.http.push_response(200, page(&[(2, U1), (1, U1)], false));
        f.http.push_response(200, second.clone());
        block_on(f.syncer.refresh_query("q", 25)).unwrap();
        assert!(
            block_on(f.syncer.refresh_query("q", 25)).unwrap().changed,
            "{second}"
        );
    }
}

#[test]
fn load_more_appends_from_the_cached_count() {
    let f = fixture();
    f.http.push_response(200, page(&[(5, U1), (4, U1)], true));
    f.http.push_response(200, page(&[(3, U1)], false));
    block_on(f.syncer.refresh_query("q", 2)).unwrap();
    let up = block_on(f.syncer.load_more("q", 2)).unwrap();
    assert_eq!(
        up,
        QueryUpdate {
            changed: true,
            cached_count: 3,
            more_changes: false
        }
    );
    let reqs = f.http.requests();
    let uri = reqs[1].uri().to_string();
    assert!(uri.contains("S=2"), "{uri}");
    assert!(uri.contains("n=2"), "{uri}");
    assert!(!reqs[0].uri().to_string().contains("S="));
    let cached = f.syncer.cached_query("q").unwrap().unwrap();
    assert_eq!(numbers(&cached), [5, 4, 3]);
    assert!(!cached.more_changes);
}

#[test]
fn refresh_keeps_loaded_pages_when_first_page_is_unchanged() {
    let f = fixture();
    f.http.push_response(200, page(&[(5, U1), (4, U1)], true));
    f.http.push_response(200, page(&[(3, U1)], false));
    f.http.push_response(200, page(&[(5, U1), (4, U1)], true));
    block_on(f.syncer.refresh_query("q", 2)).unwrap();
    block_on(f.syncer.load_more("q", 2)).unwrap();
    let up = block_on(f.syncer.refresh_query("q", 2)).unwrap();
    assert!(!up.changed);
    assert_eq!(up.cached_count, 3);
    assert_eq!(
        numbers(&f.syncer.cached_query("q").unwrap().unwrap()),
        [5, 4, 3]
    );
}

#[test]
fn refresh_resets_to_first_page_when_it_changed() {
    let f = fixture();
    f.http.push_response(200, page(&[(5, U1), (4, U1)], true));
    f.http.push_response(200, page(&[(3, U1)], false));
    f.http.push_response(200, page(&[(6, U2), (5, U1)], true));
    block_on(f.syncer.refresh_query("q", 2)).unwrap();
    block_on(f.syncer.load_more("q", 2)).unwrap();
    assert!(block_on(f.syncer.refresh_query("q", 2)).unwrap().changed);
    assert_eq!(
        numbers(&f.syncer.cached_query("q").unwrap().unwrap()),
        [6, 5]
    );
}

#[test]
fn refresh_change_round_trip() {
    let f = fixture();
    assert!(f.syncer.cached_change(ChangeNumber(7)).unwrap().is_none());
    f.http
        .push_response(200, format!(")]}}'\n{}", change_json(7, U1)));
    let detail = block_on(f.syncer.refresh_change(ChangeNumber(7))).unwrap();
    assert_eq!(detail.summary.number, ChangeNumber(7));
    let cached = f.syncer.cached_change(ChangeNumber(7)).unwrap().unwrap();
    assert_eq!(cached.detail, detail);
    assert!(f.http.requests()[0].uri().path().ends_with("/changes/7"));
}

#[test]
fn server_error_leaves_cache_intact() {
    let f = fixture();
    f.http.push_response(200, page(&[(2, U1), (1, U1)], false));
    block_on(f.syncer.refresh_query("q", 25)).unwrap();
    let before = f.syncer.cached_query("q").unwrap();

    f.http.push_response(500, "boom");
    let err = block_on(f.syncer.refresh_query("q", 25)).unwrap_err();
    assert!(matches!(
        err,
        Error::Api(gerrit_api::Error::Status { code: 500, .. })
    ));
    f.http.push_error("connection reset");
    assert!(block_on(f.syncer.refresh_query("q", 25)).is_err());
    f.http.push_response(500, "boom");
    assert!(block_on(f.syncer.load_more("q", 25)).is_err());
    f.http.push_response(404, "");
    assert!(block_on(f.syncer.refresh_change(ChangeNumber(2))).is_err());

    assert_eq!(f.syncer.cached_query("q").unwrap(), before);
    assert!(f.syncer.cached_change(ChangeNumber(2)).unwrap().is_none());
}

#[test]
fn usable_as_a_refresh_source() {
    let f = fixture();
    f.http.push_response(200, page(&[(1, U1)], false));
    let source: &dyn RefreshSource = &f.syncer;
    assert!(block_on(source.refresh_query("q", 25)).unwrap().changed);
}
