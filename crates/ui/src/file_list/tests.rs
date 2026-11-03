//! [`FileList`] on its own: where the list comes from, and the reviewed
//! marks. The pages' tests check how they show it.

use std::sync::{Arc, Mutex};

use gerrit_api::fake::RouteHttpClient;
use gpui_kit::{AppContext as _, Entity, TestAppContext};
use model::{ChangeNumber, FileEntry, FileStatus, FilesKey, PatchSetNumber, PatchSetRef};
use store::{CachedFileList, Store};

use super::{Ask, FileList, FileListEvent, Files, LOCAL_FILES_UNAVAILABLE, Seed, seed_of};
use crate::{
    checkouts::Checkouts,
    local_source::{
        LocalState,
        tests::{self as lt, Fixture},
    },
    reviewed,
    session::{ProjectSettings, Session},
    testing::file,
    tests::{URL, env_with, setup},
};

const PROJECT: &str = "platform/foo";
const PUT_A: &str = "PUT /changes/7/revisions/sha1/files/src%2Fa.rs/reviewed";

fn key() -> FilesKey {
    FilesKey {
        change: ChangeNumber(7),
        base: None,
        target: PatchSetRef {
            number: PatchSetNumber(1),
            sha: "sha1".into(),
        },
    }
}

/// `key` with a local version as the target.
fn local_key() -> FilesKey {
    let mut k = key();
    k.target.number = PatchSetNumber(PatchSetNumber::LOCAL_BASE + 1);
    k
}

fn entries(paths: &[&str]) -> Vec<FileEntry> {
    paths
        .iter()
        .map(|p| file(p, FileStatus::Modified))
        .collect()
}

fn cached(paths: &[&str], marks: &[&str]) -> CachedFileList {
    CachedFileList {
        files: entries(paths),
        reviewed: Some(marks.iter().map(|m| (*m).to_owned()).collect()),
        fetched_at: jiff::Timestamp::now(),
    }
}

fn empty_store() -> Store {
    let mut store = Store::open_in_memory().expect("store");
    store.server_id(URL).expect("server");
    store
}

/// A store with `paths` as the list of `key`.
fn store_with(key: &FilesKey, paths: &[&str]) -> Store {
    let mut store = empty_store();
    let server = store.server_id(URL).expect("server");
    store
        .save_file_list(server, key, &entries(paths), None, jiff::Timestamp::now())
        .expect("files");
    store
}

fn gerrit_files(paths: &[&str]) -> Vec<u8> {
    let entries: Vec<_> = paths
        .iter()
        .map(|p| format!("\"{p}\":{{\"size\":1}}"))
        .collect();
    format!(")]}}'\n{{{}}}", entries.join(",")).into_bytes()
}

fn signed_in() -> gerrit_api::Auth {
    gerrit_api::Auth::Basic {
        username: "alice".into(),
        password: "secret".into(),
    }
}

struct Setup {
    list: Entity<FileList>,
    http: RouteHttpClient,
    events: Arc<Mutex<Vec<String>>>,
}

impl Setup {
    fn files(&self, cx: &TestAppContext) -> Files {
        cx.read(|cx| self.list.read(cx).files())
    }

    fn load(&self, cx: &mut TestAppContext, key: FilesKey, seed: Seed, ask: Ask) -> bool {
        cx.update(|cx| {
            self.list
                .update(cx, |l, cx| l.load(Some(key), PROJECT, seed, ask, cx))
        })
    }

    fn paths(&self, cx: &TestAppContext) -> Vec<String> {
        self.files(cx).list.iter().map(|f| f.path.clone()).collect()
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().expect("log").clone()
    }

    fn requests(&self, needle: &str) -> usize {
        self.http
            .requested()
            .iter()
            .filter(|r| r.contains(needle))
            .count()
    }
}

fn setup_with(
    cx: &mut TestAppContext,
    store: Store,
    http: RouteHttpClient,
    auth: Option<gerrit_api::Auth>,
    checkout: Option<&Fixture>,
) -> Setup {
    let env = env_with(store, &http);
    let mut server = setup();
    if let Some(gerrit_api::Auth::Basic { username, .. }) = &auth {
        server.username = Some(username.clone());
    }
    let session = Session::new(&env, &server, auth).expect("session");
    let events = Arc::new(Mutex::new(Vec::new()));
    let log = events.clone();
    let list = cx.update(|cx| {
        let checkouts = checkout.map(|f| {
            cx.new(|_| {
                Checkouts::new(vec![ProjectSettings::new(
                    PROJECT,
                    f.repo.path().to_string_lossy(),
                )])
            })
        });
        let list = cx.new(|_| {
            let mut l = FileList::new(session);
            l.set_checkouts(checkouts);
            l
        });
        cx.subscribe(&list, move |_, ev: &FileListEvent, _| {
            let text = match ev {
                FileListEvent::Listed => "listed".to_owned(),
                FileListEvent::Changed => "changed".to_owned(),
                FileListEvent::MarksChanged => "marks".to_owned(),
                FileListEvent::LocalResolved => "resolved".to_owned(),
                FileListEvent::ReviewedFailed(note) => format!("failed: {note}"),
                FileListEvent::Unauthorized => "unauthorized".to_owned(),
            };
            log.lock().expect("log").push(text);
        })
        .detach();
        list
    });
    Setup { list, http, events }
}

fn setup_plain(cx: &mut TestAppContext, store: Store, http: RouteHttpClient) -> Setup {
    setup_with(cx, store, http, Some(gerrit_api::Auth::Anonymous), None)
}

/// Runs until `done`: the checkout opens and git runs on real threads.
fn wait(cx: &mut TestAppContext, s: &Setup, done: impl Fn(&Files) -> bool) {
    cx.run_until_parked();
    for _ in 0..100 {
        if done(&s.files(cx)) {
            return;
        }
        cx.executor().allow_parking();
        std::thread::sleep(std::time::Duration::from_millis(20));
        cx.run_until_parked();
    }
    panic!("the file list did not get there");
}

#[gpui_kit::test]
fn a_seeded_list_is_there_at_once_and_nobody_is_asked(cx: &mut TestAppContext) {
    let s = setup_plain(cx, empty_store(), RouteHttpClient::new());
    let restarted = s.load(
        cx,
        key(),
        Seed::Hit(cached(&["a", "b"], &["b"])),
        Ask::OnMiss,
    );
    assert!(restarted);
    let files = s.files(cx);
    assert!(files.loaded && files.settled);
    assert!(files.is_reviewed("b") && !files.is_reviewed("a"));
    assert_eq!(files.renames.len(), 2);
    cx.run_until_parked();
    assert!(s.http.requested().is_empty(), "{:?}", s.http.requested());
    assert_eq!(s.events(), ["listed"]);
}

#[gpui_kit::test]
fn the_same_key_loads_again_without_starting_over(cx: &mut TestAppContext) {
    let s = setup_plain(cx, store_with(&key(), &["a"]), RouteHttpClient::new());
    assert!(s.load(cx, key(), Seed::Unread, Ask::Never));
    cx.run_until_parked();
    assert_eq!(s.paths(cx), ["a"]);
    // A poll: the list stays on show while the store is read again.
    assert!(!s.load(cx, key(), Seed::Unread, Ask::Never));
    assert_eq!(s.paths(cx), ["a"]);
    cx.run_until_parked();
    assert_eq!(s.paths(cx), ["a"]);
    assert!(s.http.requested().is_empty());
}

#[gpui_kit::test]
fn another_key_starts_over_and_a_late_list_of_the_old_one_is_dropped(cx: &mut TestAppContext) {
    let s = setup_plain(cx, store_with(&key(), &["old"]), RouteHttpClient::new());
    s.load(cx, key(), Seed::Unread, Ask::Never);
    // Before the store answers, another pair.
    let mut other = key();
    other.base = Some(PatchSetRef {
        number: PatchSetNumber(1),
        sha: "sha0".into(),
    });
    other.target.sha = "sha2".into();
    assert!(s.load(
        cx,
        other.clone(),
        Seed::Hit(cached(&["new"], &[])),
        Ask::Never
    ));
    cx.run_until_parked();
    assert_eq!(s.files(cx).key, Some(other));
    assert_eq!(s.paths(cx), ["new"]);
}

#[gpui_kit::test]
fn a_stored_list_is_refreshed_from_gerrit_only_when_asked_always(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route("/files", 200, gerrit_files(&["a", "fresh"]));
    let s = setup_plain(cx, store_with(&key(), &["a"]), http);
    s.load(cx, key(), Seed::Unread, Ask::OnMiss);
    cx.run_until_parked();
    assert_eq!(s.paths(cx), ["a"]);
    assert_eq!(s.requests("/files"), 0);
    s.load(cx, key(), Seed::Unread, Ask::Always);
    cx.run_until_parked();
    assert_eq!(s.paths(cx), ["a", "fresh"]);
    assert_eq!(s.requests("/files"), 1);
}

#[test]
fn a_store_read_that_fails_is_a_miss() {
    let failed = Err(sync::Error::LeaseLost);
    assert!(matches!(seed_of(failed), Seed::Miss));
    assert!(matches!(seed_of(Ok(None)), Seed::Miss));
    assert!(matches!(
        seed_of(Ok(Some(cached(&["a"], &[])))),
        Seed::Hit(_)
    ));
}

#[gpui_kit::test]
fn another_key_drops_a_gerrit_answer_for_the_old_one(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route("/files", 200, gerrit_files(&["old"]));
    let s = setup_plain(cx, empty_store(), http);
    s.load(cx, key(), Seed::Miss, Ask::OnMiss);
    let mut other = key();
    other.target.sha = "sha2".into();
    s.load(cx, other, Seed::Hit(cached(&["new"], &[])), Ask::Never);
    cx.run_until_parked();
    assert_eq!(s.paths(cx), ["new"]);
}

#[gpui_kit::test]
fn a_miss_asks_gerrit(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route("/files", 200, gerrit_files(&["a"]));
    let s = setup_plain(cx, empty_store(), http);
    s.load(cx, key(), Seed::Unread, Ask::OnMiss);
    assert!(!s.files(cx).settled);
    cx.run_until_parked();
    let files = s.files(cx);
    assert!(files.loaded && files.settled && files.checkout().is_none());
    assert_eq!(s.paths(cx), ["a"]);
}

#[gpui_kit::test]
fn a_failed_request_settles_with_its_error(cx: &mut TestAppContext) {
    // No /files route: a 404.
    let s = setup_plain(cx, empty_store(), RouteHttpClient::new());
    s.load(cx, key(), Seed::Miss, Ask::OnMiss);
    assert!(!s.files(cx).settled);
    cx.run_until_parked();
    let files = s.files(cx);
    assert!(files.settled && !files.loaded);
    assert!(files.error.is_some());
    assert_eq!(s.events(), ["changed"]);
}

#[gpui_kit::test]
fn a_401_on_the_list_goes_to_the_login_flow(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route("/files", 401, "");
    let s = setup_plain(cx, empty_store(), http);
    s.load(cx, key(), Seed::Miss, Ask::OnMiss);
    cx.run_until_parked();
    assert_eq!(s.events(), ["unauthorized"]);
}

#[gpui_kit::test]
fn offline_a_miss_settles_without_a_request(cx: &mut TestAppContext) {
    let s = setup_with(cx, empty_store(), RouteHttpClient::new(), None, None);
    s.load(cx, key(), Seed::Miss, Ask::Always);
    let files = s.files(cx);
    assert!(files.settled && !files.loaded);
    cx.run_until_parked();
    assert!(s.http.requested().is_empty());
}

#[gpui_kit::test]
fn gerrit_is_never_asked_about_a_local_version(cx: &mut TestAppContext) {
    let s = setup_plain(cx, empty_store(), RouteHttpClient::new());
    s.load(cx, local_key(), Seed::Unread, Ask::Always);
    cx.run_until_parked();
    let files = s.files(cx);
    assert!(files.settled && !files.loaded);
    assert_eq!(files.error.as_deref(), Some(LOCAL_FILES_UNAVAILABLE));
    assert!(s.http.requested().is_empty(), "{:?}", s.http.requested());
}

#[gpui_kit::test]
fn the_mark_is_set_at_once_and_stays_when_the_server_takes_it(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route(PUT_A, 204, "");
    let s = setup_with(cx, empty_store(), http, Some(signed_in()), None);
    s.load(cx, key(), Seed::Hit(cached(&["src/a.rs"], &[])), Ask::Never);
    cx.update(|cx| {
        s.list.update(cx, |l, cx| {
            l.toggle_reviewed("src/a.rs", cx).expect("toggled")
        })
    });
    assert!(s.files(cx).is_reviewed("src/a.rs"));
    cx.run_until_parked();
    assert!(s.files(cx).is_reviewed("src/a.rs"));
    assert_eq!(s.requests("src%2Fa.rs/reviewed"), 1);
    assert_eq!(s.events(), ["listed", "marks"]);
}

#[gpui_kit::test]
fn a_refused_mark_goes_back_with_a_note(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route(PUT_A, 403, "not allowed");
    let s = setup_with(cx, empty_store(), http, Some(signed_in()), None);
    s.load(cx, key(), Seed::Hit(cached(&["src/a.rs"], &[])), Ask::Never);
    cx.update(|cx| {
        s.list.update(cx, |l, cx| {
            l.toggle_reviewed("src/a.rs", cx).expect("toggled")
        })
    });
    cx.run_until_parked();
    assert!(!s.files(cx).is_reviewed("src/a.rs"));
    let events = s.events();
    assert_eq!(events[..3], ["listed", "marks", "marks"]);
    assert!(events[3].starts_with("failed: ") && events[3].contains("src/a.rs"));
    // A 403 is not a login problem.
    assert!(!events.contains(&"unauthorized".to_owned()));
}

#[gpui_kit::test]
fn a_401_on_the_mark_puts_it_back_and_goes_to_the_login_flow(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route(PUT_A, 401, "");
    let s = setup_with(cx, empty_store(), http, Some(signed_in()), None);
    s.load(cx, key(), Seed::Hit(cached(&["src/a.rs"], &[])), Ask::Never);
    cx.update(|cx| {
        s.list.update(cx, |l, cx| {
            l.toggle_reviewed("src/a.rs", cx).expect("toggled")
        })
    });
    cx.run_until_parked();
    assert!(!s.files(cx).is_reviewed("src/a.rs"));
    assert!(s.events().contains(&"unauthorized".to_owned()));
}

#[gpui_kit::test]
fn a_mark_waits_for_gerrits_list(cx: &mut TestAppContext) {
    let s = setup_with(
        cx,
        empty_store(),
        RouteHttpClient::new(),
        Some(signed_in()),
        None,
    );
    s.load(cx, key(), Seed::Hit(cached(&["src/a.rs"], &[])), Ask::Never);
    cx.update(|cx| s.list.update(cx, |l, _| l.unload_for_test()));
    let toggled = cx.update(|cx| s.list.update(cx, |l, cx| l.toggle_reviewed("src/a.rs", cx)));
    assert_eq!(toggled, Err(reviewed::WAITS));
    cx.run_until_parked();
    assert!(s.http.requested().is_empty());
}

#[gpui_kit::test]
fn a_failed_first_toggle_does_not_undo_the_third(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    // On (refused), off (works), on (refused).
    http.route(PUT_A, 403, "not allowed");
    http.route(
        "DELETE /changes/7/revisions/sha1/files/src%2Fa.rs/reviewed",
        204,
        "",
    );
    let s = setup_with(cx, empty_store(), http, Some(signed_in()), None);
    s.load(cx, key(), Seed::Hit(cached(&["src/a.rs"], &[])), Ask::Never);
    toggle(cx, &s, 3);
    assert!(s.files(cx).is_reviewed("src/a.rs"));
    cx.run_until_parked();
    // The server has it off, as the DELETE left it; one note for the
    // last refusal.
    assert!(!s.files(cx).is_reviewed("src/a.rs"));
    let failed = s
        .events()
        .iter()
        .filter(|e| e.starts_with("failed: "))
        .count();
    assert_eq!(failed, 1, "{:?}", s.events());
}

#[gpui_kit::test]
fn a_refusal_that_a_later_mark_covers_says_nothing(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route(PUT_A, 403, "not allowed");
    http.route(
        "DELETE /changes/7/revisions/sha1/files/src%2Fa.rs/reviewed",
        204,
        "",
    );
    let s = setup_with(cx, empty_store(), http, Some(signed_in()), None);
    s.load(cx, key(), Seed::Hit(cached(&["src/a.rs"], &[])), Ask::Never);
    // On (refused) then off (works): off is what was asked for last.
    toggle(cx, &s, 2);
    cx.run_until_parked();
    assert!(!s.files(cx).is_reviewed("src/a.rs"));
    assert!(!s.events().iter().any(|e| e.starts_with("failed: ")));
}

fn toggle(cx: &mut TestAppContext, s: &Setup, times: usize) {
    for _ in 0..times {
        cx.update(|cx| {
            s.list
                .update(cx, |l, cx| l.toggle_reviewed("src/a.rs", cx))
                .expect("toggled")
        });
    }
}

#[gpui_kit::test]
fn the_request_ends_also_when_the_list_is_gone(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route(PUT_A, 204, "");
    let s = setup_with(cx, empty_store(), http, Some(signed_in()), None);
    s.load(cx, key(), Seed::Hit(cached(&["src/a.rs"], &[])), Ask::Never);
    cx.update(|cx| {
        s.list.update(cx, |l, cx| {
            l.toggle_reviewed("src/a.rs", cx).expect("toggled")
        })
    });
    let Setup { list, http, .. } = s;
    drop(list);
    cx.run_until_parked();
    assert_eq!(
        http.requested()
            .iter()
            .filter(|r| r.contains("src%2Fa.rs/reviewed"))
            .count(),
        1
    );
}

// ---- the checkout ----

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_miss_lists_in_the_checkout_when_gerrit_fails(cx: &mut TestAppContext) {
    let f = lt::fixture();
    // No /files route: Gerrit answers 404.
    let s = setup_with(
        cx,
        empty_store(),
        RouteHttpClient::new(),
        Some(gerrit_api::Auth::Anonymous),
        Some(&f),
    );
    s.load(cx, lt::files_key(&f.target), Seed::Miss, Ask::OnMiss);
    wait(cx, &s, |files| files.loaded);
    let files = s.files(cx);
    assert_eq!(files.checkout(), Some(f.repo.path()));
    assert!(files.local_source().is_some() && files.error.is_none());
    let paths: Vec<_> = files.list.iter().map(|e| e.path.clone()).collect();
    assert_eq!(paths, ["/COMMIT_MSG", "src/a.rs"]);
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_late_checkout_list_does_not_replace_gerrits(cx: &mut TestAppContext) {
    // The fake answers at once, so Gerrit's list comes first; the page
    // tests race the other order.
    let f = lt::fixture();
    let http = RouteHttpClient::new();
    http.route(
        "/files",
        200,
        gerrit_files(&["/COMMIT_MSG", "src/a.rs", "x"]),
    );
    let s = setup_with(
        cx,
        empty_store(),
        http,
        Some(gerrit_api::Auth::Anonymous),
        Some(&f),
    );
    s.load(cx, lt::files_key(&f.target), Seed::Miss, Ask::OnMiss);
    wait(cx, &s, |files| {
        files.loaded && files.local_source().is_some()
    });
    // Give a late checkout list the time to land, if it is going to.
    for _ in 0..10 {
        cx.executor().allow_parking();
        std::thread::sleep(std::time::Duration::from_millis(20));
        cx.run_until_parked();
    }
    assert_eq!(s.files(cx).checkout(), None);
    assert_eq!(s.paths(cx), ["/COMMIT_MSG", "src/a.rs", "x"]);
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_commit_the_checkout_lacks_gives_no_local_list(cx: &mut TestAppContext) {
    let f = lt::fixture();
    let s = setup_with(
        cx,
        empty_store(),
        RouteHttpClient::new(),
        Some(gerrit_api::Auth::Anonymous),
        Some(&f),
    );
    let mut k = lt::files_key(&f.target);
    k.target.sha = "0123456789abcdef0123456789abcdef01234567".into();
    s.load(cx, k, Seed::Miss, Ask::OnMiss);
    wait(cx, &s, |files| {
        files.settled && matches!(files.source, LocalState::Ready(..))
    });
    let files = s.files(cx);
    assert!(!files.loaded && files.checkout().is_none());
    assert!(files.local_source().is_none());
}
