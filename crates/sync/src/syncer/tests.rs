use std::sync::{Arc, Mutex};

use futures::executor::block_on;
use gerrit_api::{Auth, GerritClient, fake::FakeHttpClient};
use model::{DiffContext, PatchSetNumber, PatchSetRef, Whitespace};
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
    fixture_with(Auth::Anonymous)
}

fn fixture_with(auth: Auth) -> Fixture {
    let http = FakeHttpClient::new();
    let client = GerritClient::new(Arc::new(http.clone()), BASE, auth).unwrap();
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

fn signed_in() -> Auth {
    Auth::Basic {
        username: "u".into(),
        password: "p".into(),
    }
}

fn xssi(body: &Value) -> String {
    format!(")]}}'\n{body}")
}

fn ps(n: u32, sha: &str) -> PatchSetRef {
    PatchSetRef {
        number: PatchSetNumber(n),
        sha: sha.into(),
    }
}

fn files_key() -> FilesKey {
    FilesKey {
        change: ChangeNumber(7),
        base: Some(ps(1, "aaa")),
        target: ps(2, "bbb"),
    }
}

fn diff_key(path: &str) -> DiffKey {
    DiffKey {
        change: ChangeNumber(7),
        base: Some(ps(1, "aaa")),
        target: ps(2, "bbb"),
        path: path.into(),
        whitespace: Whitespace::IgnoreAll,
    }
}

fn diff_body(line: &str) -> String {
    xssi(&json!({
        "meta_b": {"name": "f", "lines": 1},
        "change_type": "MODIFIED",
        "content": [{"a": ["old"], "b": [line]}]
    }))
}

fn file_list() -> Vec<FileEntry> {
    ["/COMMIT_MSG", "a.rs", "b.rs"]
        .into_iter()
        .map(|p| FileEntry {
            path: p.into(),
            status: model::FileStatus::Modified,
            old_path: None,
            lines_inserted: 1,
            lines_deleted: 0,
            size_delta: 1,
            size: 1,
            binary: false,
        })
        .collect()
}

#[test]
fn refresh_files_caches_the_list_anonymously() {
    let f = fixture();
    let key = files_key();
    assert!(f.syncer.cached_files(&key).unwrap().is_none());
    f.http.push_response(
        200,
        xssi(&json!({"b.rs": {"lines_inserted": 2}, "/COMMIT_MSG": {}, "a.rs": {"status": "A"}})),
    );
    let got = block_on(f.syncer.refresh_files(&key)).unwrap();
    let paths: Vec<_> = got.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["/COMMIT_MSG", "a.rs", "b.rs"]);
    assert_eq!(got.reviewed, None);
    assert_eq!(
        f.syncer.cached_files(&key).unwrap().unwrap().files,
        got.files
    );
    // Anonymous: just the one request, with the base patch set number.
    let reqs = f.http.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].uri().to_string(),
        format!("{BASE}/changes/7/revisions/bbb/files?base=1")
    );
}

#[test]
fn refresh_files_fetches_reviewed_marks_when_signed_in() {
    let f = fixture_with(signed_in());
    let key = files_key();
    f.http
        .push_response(200, xssi(&json!({"a.rs": {}, "b.rs": {}})));
    f.http.push_response(200, xssi(&json!(["a.rs"])));
    let got = block_on(f.syncer.refresh_files(&key)).unwrap();
    assert_eq!(got.reviewed, Some(vec!["a.rs".to_owned()]));
    let cached = f.syncer.cached_files(&key).unwrap().unwrap();
    assert_eq!(cached.reviewed, got.reviewed);
    assert!(
        f.http.requests()[1]
            .uri()
            .to_string()
            .ends_with("/files?reviewed")
    );
}

#[test]
fn a_failing_reviewed_request_keeps_the_list_and_the_known_marks() {
    let f = fixture_with(signed_in());
    let key = files_key();
    f.http.push_response(200, xssi(&json!({"a.rs": {}})));
    f.http.push_response(200, xssi(&json!(["a.rs"])));
    block_on(f.syncer.refresh_files(&key)).unwrap();

    f.http
        .push_response(200, xssi(&json!({"a.rs": {}, "b.rs": {}})));
    f.http.push_response(500, "boom");
    let got = block_on(f.syncer.refresh_files(&key)).unwrap();
    assert_eq!(got.files.len(), 2);
    assert_eq!(got.reviewed, Some(vec!["a.rs".to_owned()]));
    assert_eq!(f.syncer.cached_files(&key).unwrap().unwrap().files.len(), 2);

    // The file list request failing is an error and changes nothing.
    f.http.push_response(500, "boom");
    assert!(block_on(f.syncer.refresh_files(&key)).is_err());
    assert_eq!(f.syncer.cached_files(&key).unwrap().unwrap().files.len(), 2);
}

#[test]
fn refresh_diff_round_trip_and_request() {
    let f = fixture();
    let key = diff_key("dir/my file.rs");
    assert!(f.syncer.cached_diff(&key).unwrap().is_none());
    f.http.push_response(200, diff_body("new"));
    let diff = block_on(f.syncer.refresh_diff(&key)).unwrap();
    assert_eq!(f.syncer.cached_diff(&key).unwrap().unwrap().diff, diff);
    assert_eq!(
        f.http.requests()[0].uri().to_string(),
        format!(
            "{BASE}/changes/7/revisions/bbb/files/dir%2Fmy%20file.rs/diff\
             ?base=1&intraline&whitespace=IGNORE_ALL"
        )
    );
    // Another whitespace mode is a different cache entry.
    let other = DiffKey {
        whitespace: Whitespace::IgnoreNone,
        ..key
    };
    assert!(f.syncer.cached_diff(&other).unwrap().is_none());
}

#[test]
fn failed_diff_refresh_keeps_the_cached_diff() {
    let f = fixture();
    let key = diff_key("a.rs");
    f.http.push_response(200, diff_body("v1"));
    let first = block_on(f.syncer.refresh_diff(&key)).unwrap();
    f.http.push_response(500, "boom");
    assert!(block_on(f.syncer.refresh_diff(&key)).is_err());
    f.http.push_response(404, "");
    assert!(matches!(
        block_on(f.syncer.refresh_diff(&key)),
        Err(Error::Api(gerrit_api::Error::NotFound))
    ));
    assert_eq!(f.syncer.cached_diff(&key).unwrap().unwrap().diff, first);
}

#[test]
fn prefetch_fetches_the_next_file_once() {
    let f = fixture();
    let files = file_list();
    let current = diff_key("a.rs");

    f.http.push_response(200, diff_body("b"));
    let fetched = block_on(f.syncer.prefetch_next_diff(&current, &files)).unwrap();
    assert_eq!(fetched, Some(diff_key("b.rs")));
    assert!(f.syncer.cached_diff(&diff_key("b.rs")).unwrap().is_some());
    assert!(f.syncer.cached_diff(&current).unwrap().is_none());
    assert!(
        f.http.requests()[0]
            .uri()
            .to_string()
            .contains("/files/b.rs/diff?")
    );

    // Already cached: no further request (the fake would fail one).
    let again = block_on(f.syncer.prefetch_next_diff(&current, &files)).unwrap();
    assert_eq!(again, None);
    assert_eq!(f.http.requests().len(), 1);
}

#[test]
fn prefetch_does_nothing_at_the_end_or_for_unknown_files() {
    let f = fixture();
    let files = file_list();
    assert_eq!(
        block_on(f.syncer.prefetch_next_diff(&diff_key("b.rs"), &files)).unwrap(),
        None
    );
    assert_eq!(
        block_on(f.syncer.prefetch_next_diff(&diff_key("zzz.rs"), &files)).unwrap(),
        None
    );
    assert_eq!(
        block_on(f.syncer.prefetch_next_diff(&diff_key("a.rs"), &[])).unwrap(),
        None
    );
    assert!(f.http.requests().is_empty());
}

#[test]
fn prefetch_starts_with_the_commit_message() {
    let f = fixture();
    f.http.push_response(200, diff_body("m"));
    let fetched = block_on(
        f.syncer
            .prefetch_next_diff(&diff_key("/COMMIT_MSG"), &file_list()),
    )
    .unwrap();
    assert_eq!(fetched, Some(diff_key("a.rs")));
}

#[test]
fn prefetch_reports_fetch_errors_and_caches_nothing() {
    let f = fixture();
    f.http.push_response(500, "boom");
    assert!(block_on(f.syncer.prefetch_next_diff(&diff_key("a.rs"), &file_list())).is_err());
    assert!(f.syncer.cached_diff(&diff_key("b.rs")).unwrap().is_none());
}

#[test]
fn refresh_related_round_trip() {
    let f = fixture();
    let rev = RevisionId::from("bbb");
    assert!(
        f.syncer
            .cached_related(ChangeNumber(7), &rev)
            .unwrap()
            .is_none()
    );
    f.http.push_response(
        200,
        xssi(&json!({"changes": [
            {"project": "p", "_change_number": 8, "_revision_number": 1,
             "_current_revision_number": 2, "status": "NEW"}
        ]})),
    );
    let related = block_on(f.syncer.refresh_related(ChangeNumber(7), &rev)).unwrap();
    assert_eq!(related.len(), 1);
    let cached = f
        .syncer
        .cached_related(ChangeNumber(7), &rev)
        .unwrap()
        .unwrap();
    assert_eq!(cached.changes, related);

    f.http.push_response(500, "boom");
    assert!(block_on(f.syncer.refresh_related(ChangeNumber(7), &rev)).is_err());
    assert_eq!(
        f.syncer
            .cached_related(ChangeNumber(7), &rev)
            .unwrap()
            .unwrap()
            .changes,
        related
    );
}

#[test]
fn diff_preferences_are_skipped_when_anonymous() {
    let f = fixture();
    assert_eq!(block_on(f.syncer.refresh_diff_preferences()).unwrap(), None);
    assert!(f.http.requests().is_empty());
    assert!(f.syncer.cached_diff_preferences().unwrap().is_none());
}

#[test]
fn diff_preferences_round_trip_when_signed_in() {
    let f = fixture_with(signed_in());
    assert!(f.syncer.cached_diff_preferences().unwrap().is_none());
    f.http
        .push_response(200, xssi(&json!({"context": 3, "tab_size": 2})));
    f.http
        .push_response(200, xssi(&json!({"diff_view": "UNIFIED_DIFF"})));
    let prefs = block_on(f.syncer.refresh_diff_preferences())
        .unwrap()
        .unwrap();
    assert_eq!(prefs.context, Some(DiffContext::Lines(3)));
    assert_eq!(prefs.view_mode, Some(model::DiffViewMode::Unified));
    let cached = f.syncer.cached_diff_preferences().unwrap().unwrap();
    assert_eq!(cached.preferences, prefs);

    // A failed refresh keeps the cached preferences.
    f.http.push_response(401, "");
    assert!(matches!(
        block_on(f.syncer.refresh_diff_preferences()),
        Err(Error::Api(gerrit_api::Error::Unauthorized))
    ));
    assert_eq!(
        f.syncer
            .cached_diff_preferences()
            .unwrap()
            .unwrap()
            .preferences,
        prefs
    );
}

#[test]
fn refreshing_a_diff_prunes_old_cache_rows() {
    let f = fixture();
    // An ancient row, as if cached long ago.
    f.syncer
        .store()
        .save_diff(
            f.syncer.server,
            &diff_key("old.rs"),
            &block_on(async {
                f.http.push_response(200, diff_body("x"));
                f.syncer
                    .client
                    .get_diff(
                        ChangeNumber(7),
                        &"bbb".into(),
                        "old.rs",
                        &gerrit_api::DiffParams::default(),
                    )
                    .await
                    .unwrap()
            }),
            "2020-01-01T00:00:00Z".parse().unwrap(),
        )
        .unwrap();
    assert!(f.syncer.cached_diff(&diff_key("old.rs")).unwrap().is_some());
    f.http.push_response(200, diff_body("y"));
    block_on(f.syncer.refresh_diff(&diff_key("new.rs"))).unwrap();
    assert!(f.syncer.cached_diff(&diff_key("old.rs")).unwrap().is_none());
    assert!(f.syncer.cached_diff(&diff_key("new.rs")).unwrap().is_some());
}

#[test]
fn two_servers_in_one_store_keep_the_same_numbers_and_queries_apart() {
    let mut store = Store::open_in_memory().unwrap();
    let a = store.server_for_slug("a", "https://a.example").unwrap();
    let b = store.server_for_slug("b", "https://b.example").unwrap();
    let store = Arc::new(Mutex::new(store));
    let syncer = |url: &str, id| {
        let http = FakeHttpClient::new();
        let client = GerritClient::new(Arc::new(http.clone()), url, Auth::Anonymous).unwrap();
        (http, Syncer::new(client, store.clone(), id))
    };
    let (http_a, sync_a) = syncer("https://a.example", a);
    let (http_b, sync_b) = syncer("https://b.example", b);
    // Change 1 and the query "q" exist on both, with different content.
    http_a.push_response(200, page(&[(2, U1), (1, U1)], false));
    http_b.push_response(200, page(&[(1, U2)], true));
    block_on(sync_a.refresh_query("q", 25)).unwrap();
    block_on(sync_b.refresh_query("q", 25)).unwrap();

    let (qa, qb) = (
        sync_a.cached_query("q").unwrap().unwrap(),
        sync_b.cached_query("q").unwrap().unwrap(),
    );
    assert_eq!(numbers(&qa), [2, 1]);
    assert_eq!(numbers(&qb), [1]);
    assert_ne!(qa.changes[1].updated, qb.changes[0].updated);
    assert!(!qa.more_changes && qb.more_changes);

    // A refresh on one leaves the other's cache alone.
    http_a.push_response(200, page(&[(1, U2)], false));
    block_on(sync_a.refresh_query("q", 25)).unwrap();
    assert_eq!(numbers(&sync_b.cached_query("q").unwrap().unwrap()), [1]);
    assert_eq!(numbers(&sync_a.cached_query("q").unwrap().unwrap()), [1]);
}
