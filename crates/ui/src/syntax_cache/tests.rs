use std::sync::Arc;

use gerrit_api::fake::RouteHttpClient;
use gpui_kit::{AppContext as _, Entity, TestAppContext};
use model::{ChangeNumber, DiffKey, PatchSetNumber, PatchSetRef, Whitespace};
use store::Store;

use super::{Highlight, Lru, SyntaxCache};
use crate::{
    diff::syntax::{DiffSyntax, SideSyntax},
    session::Session,
    tests::{URL, env_with, setup},
};

fn key(path: &str) -> DiffKey {
    DiffKey {
        change: ChangeNumber(7),
        base: None,
        target: PatchSetRef {
            number: PatchSetNumber(3),
            sha: "sha3".into(),
        },
        path: path.into(),
        whitespace: Whitespace::IgnoreNone,
    }
}

/// A result with `lines` lines on the new side.
fn lines(n: usize) -> Highlight {
    let side = SideSyntax::blank_for_test(n);
    Some(DiffSyntax {
        old: None,
        new: Some(Arc::new(side)),
    })
}

fn paths(lru: &Lru) -> Vec<String> {
    lru.entries.iter().map(|(k, _)| k.path.clone()).collect()
}

#[test]
fn the_entry_bound_evicts_the_least_recently_used() {
    let mut lru = Lru::new(2, 1000);
    lru.insert(key("a"), lines(1));
    lru.insert(key("b"), lines(1));
    lru.insert(key("c"), lines(1));
    assert_eq!(paths(&lru), ["b", "c"]);
}

#[test]
fn the_line_bound_evicts_oldest_first_and_counts_both_sides() {
    let mut lru = Lru::new(10, 100);
    lru.insert(key("a"), lines(60));
    lru.insert(key("b"), lines(30));
    assert_eq!(lru.lines, 90);
    lru.insert(key("c"), lines(30));
    assert_eq!(paths(&lru), ["b", "c"]);
    assert_eq!(lru.lines, 60);
    // Replacing an entry does not count it twice.
    lru.insert(key("c"), lines(10));
    assert_eq!(lru.lines, 40);
}

#[test]
fn an_oversized_newest_entry_is_kept_alone() {
    let mut lru = Lru::new(10, 100);
    lru.insert(key("a"), lines(10));
    lru.insert(key("big"), lines(500));
    assert_eq!(paths(&lru), ["big"]);
}

#[test]
fn get_marks_an_entry_as_recently_used() {
    let mut lru = Lru::new(2, 1000);
    lru.insert(key("a"), lines(1));
    lru.insert(key("b"), lines(1));
    assert!(lru.get(&key("a")).is_some());
    lru.insert(key("c"), lines(1));
    assert_eq!(paths(&lru), ["a", "c"]);
    assert!(lru.get(&key("b")).is_none());
}

#[test]
fn a_plain_result_is_cached_and_costs_no_lines() {
    let mut lru = Lru::new(2, 10);
    lru.insert(key("a"), None);
    assert!(matches!(lru.get(&key("a")), Some(None)));
    assert_eq!(lru.lines, 0);
}

struct Rig {
    cache: Entity<SyntaxCache>,
    session: Session,
    http: RouteHttpClient,
}

fn rust_body() -> Vec<u8> {
    let body = serde_json::json!({
        "meta_a": {"name": "a.rs", "content_type": "text/x-rust", "lines": 2},
        "meta_b": {"name": "a.rs", "content_type": "text/x-rust", "lines": 2},
        "change_type": "MODIFIED",
        "content": [
            {"ab": ["// hello"]},
            {"a": ["fn a() {}"], "b": ["fn b() {}"]},
        ],
    });
    format!(")]}}'\n{body}").into_bytes()
}

/// `cached` paths have their diff in the store.
fn rig(cx: &mut TestAppContext, cached: &[&str]) -> Rig {
    rig_as(cx, cached, Some(gerrit_api::Auth::Anonymous))
}

fn rig_as(cx: &mut TestAppContext, cached: &[&str], auth: Option<gerrit_api::Auth>) -> Rig {
    let mut store = Store::open_in_memory().expect("store");
    let server = store.server_id(URL).expect("server");
    let http = RouteHttpClient::new();
    http.route("/diff", 200, rust_body());
    for path in cached {
        let diff = crate::diff::tests::fixture("handwritten/diff_rename.response.json");
        store
            .save_diff(server, &key(path), &diff, jiff::Timestamp::now())
            .expect("save");
    }
    let env = env_with(store, &http);
    let session = Session::new(&env, &setup(), auth).expect("session");
    let cache = cx.new(|_| SyntaxCache::new());
    Rig {
        cache,
        session,
        http,
    }
}

fn queue(cx: &mut TestAppContext, r: &Rig, keys: &[&str], enabled: bool) {
    let keys = keys.iter().map(|p| key(p)).collect();
    let session = r.session.clone();
    r.cache
        .update(cx, |c, cx| c.set_queue(keys, &session, enabled, cx));
}

fn cached(cx: &mut TestAppContext, r: &Rig, path: &str) -> Option<Highlight> {
    r.cache.update(cx, |c, _| c.get(&key(path)))
}

fn requests(r: &Rig) -> usize {
    r.http
        .requested()
        .iter()
        .filter(|u| u.contains("/diff"))
        .count()
}

#[gpui_kit::test]
fn the_worker_highlights_queued_cached_diffs(cx: &mut TestAppContext) {
    let r = rig(cx, &["x.rs", "y.py"]);
    queue(cx, &r, &["x.rs", "y.py"], true);
    cx.run_until_parked();
    assert!(matches!(cached(cx, &r, "x.rs"), Some(Some(_))));
    assert!(cached(cx, &r, "y.py").is_some());
    assert_eq!(requests(&r), 0);
    cx.read(|cx| assert_eq!(r.cache.read(cx).queued_for_test(), 0));
}

#[gpui_kit::test]
fn a_cached_key_is_skipped_without_a_fetch_or_a_new_highlight(cx: &mut TestAppContext) {
    let r = rig(cx, &[]);
    r.cache.update(cx, |c, _| c.insert(key("x.rs"), None));
    queue(cx, &r, &["x.rs"], true);
    cx.run_until_parked();
    assert_eq!(requests(&r), 0);
    // Still the plain marker: a highlight would have made it colored.
    assert_eq!(cached(cx, &r, "x.rs").map(|h| h.is_none()), Some(true));
}

#[gpui_kit::test]
fn a_new_queue_replaces_the_old_one(cx: &mut TestAppContext) {
    let r = rig(cx, &["a.rs", "b.rs", "c.rs"]);
    queue(cx, &r, &["a.rs", "b.rs"], true);
    // Nothing ran yet: the replacement drops both.
    queue(cx, &r, &["c.rs"], true);
    cx.run_until_parked();
    assert!(cached(cx, &r, "a.rs").is_none());
    assert!(cached(cx, &r, "b.rs").is_none());
    assert!(cached(cx, &r, "c.rs").is_some());
}

#[gpui_kit::test]
fn nothing_is_done_with_the_preference_off(cx: &mut TestAppContext) {
    let r = rig(cx, &["a.rs"]);
    queue(cx, &r, &["a.rs"], true);
    // Turning it off clears what was queued.
    queue(cx, &r, &["a.rs"], false);
    cx.run_until_parked();
    assert!(cached(cx, &r, "a.rs").is_none());
    cx.read(|cx| assert_eq!(r.cache.read(cx).queued_for_test(), 0));
}

#[gpui_kit::test]
fn a_missing_diff_is_fetched_when_the_session_can_sync(cx: &mut TestAppContext) {
    let r = rig(cx, &[]);
    assert!(r.session.can_sync());
    queue(cx, &r, &["m.rs"], true);
    cx.run_until_parked();
    assert_eq!(requests(&r), 1);
    assert!(matches!(cached(cx, &r, "m.rs"), Some(Some(_))));
}

#[gpui_kit::test]
fn a_missing_diff_is_not_fetched_when_the_session_cannot_sync(cx: &mut TestAppContext) {
    let r = rig_as(cx, &[], None);
    assert!(!r.session.can_sync());
    queue(cx, &r, &["m.rs"], true);
    cx.run_until_parked();
    assert_eq!(requests(&r), 0);
    assert!(cached(cx, &r, "m.rs").is_none());
}

#[gpui_kit::test]
fn the_queue_is_cut_at_the_entry_capacity(cx: &mut TestAppContext) {
    let r = rig_as(cx, &[], None);
    let names: Vec<String> = (0..80).map(|i| format!("f{i}.rs")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    queue(cx, &r, &refs, true);
    cx.read(|cx| assert_eq!(r.cache.read(cx).queued_for_test(), super::MAX_ENTRIES));
}
