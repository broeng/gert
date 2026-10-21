//! The diff page, and the flow change page -> diff page -> back, on GPUI's
//! `TestAppContext` with a path-routed fake server.

use std::sync::{Arc, Mutex};

use gerrit_api::fake::RouteHttpClient;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
use model::{
    ChangeNumber, DiffContext, DiffKey, DiffViewMode, FileStatus, FilesKey, PatchSetNumber,
    PatchSetRef, Whitespace,
};
use store::Store;

use super::DiffPage;
use crate::{
    LocalDiff,
    nav::{DiffRoute, Route},
    pages::change::ChangePage,
    session::{SavedQuery, Startup},
    settings::DiffSettings,
    shell::Shell,
    testing::{detail, file, summary, whole_file},
    tests::{URL, env_with, open, setup},
};

/// Seeds are stamped now: a diff fetch prunes cache rows older than the
/// retention policy.
fn now() -> jiff::Timestamp {
    jiff::Timestamp::now()
}
const PATHS: [&str; 4] = ["/COMMIT_MSG", "src/a.rs", "src/b.rs", "src/c.rs"];

fn ps(n: u32) -> PatchSetRef {
    PatchSetRef {
        number: PatchSetNumber(n),
        sha: format!("sha{n}").into(),
    }
}

fn files_key() -> FilesKey {
    FilesKey {
        change: ChangeNumber(7),
        base: None,
        target: ps(3),
    }
}

fn seeded() -> Store {
    let mut store = Store::open_in_memory().expect("store");
    let server = store.server_id(URL).expect("server");
    let now = now();
    store
        .save_query_page(server, "status:open", &[summary(7)], false, now)
        .expect("query");
    store
        .save_change_detail(server, &detail(7, 3, 3), now)
        .expect("detail");
    let files: Vec<_> = PATHS
        .iter()
        .map(|p| file(p, FileStatus::Modified))
        .collect();
    store
        .save_file_list(server, &files_key(), &files, None, now)
        .expect("files");
    store
}

fn diff_body() -> Vec<u8> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/handwritten/diff_rename.response.json");
    std::fs::read(path).expect("fixture")
}

fn http_with(body: Vec<u8>) -> RouteHttpClient {
    let http = RouteHttpClient::new();
    http.route("/diff", 200, body);
    http
}

fn http() -> RouteHttpClient {
    http_with(diff_body())
}

fn route(path: &str) -> DiffRoute {
    DiffRoute {
        change: ChangeNumber(7),
        base: None,
        target: PatchSetNumber(3),
        path: path.into(),
    }
}

struct Page {
    window: AnyWindowHandle,
    page: Entity<DiffPage>,
    settings: Entity<DiffSettings>,
    http: RouteHttpClient,
    events: Arc<Mutex<Vec<String>>>,
}

fn diff_page(cx: &mut TestAppContext, store: Store, path: &str) -> Page {
    diff_page_with(cx, store, path, http())
}

fn diff_page_with(
    cx: &mut TestAppContext,
    store: Store,
    path: &str,
    http: RouteHttpClient,
) -> Page {
    let env = env_with(store, &http);
    let session = crate::session::Session::new(&env, &setup(), Some(gerrit_api::Auth::Anonymous))
        .expect("session");
    let events = Arc::new(Mutex::new(Vec::new()));
    let log = events.clone();
    let path = path.to_owned();
    let mut settings_out = None;
    let (window, page) = open(cx, |window, cx| {
        let settings = cx.new(|_| DiffSettings::new(LocalDiff::default()));
        settings_out = Some(settings.clone());
        let page = cx.new(|cx| DiffPage::new(session, settings, route(&path), cx));
        cx.subscribe(&page, move |_, ev: &super::DiffPageEvent, _| {
            let text = match ev {
                super::DiffPageEvent::Unauthorized => "unauthorized".to_owned(),
                super::DiffPageEvent::OpenFile(r) => format!("open {}", r.path),
            };
            log.lock().expect("log").push(text);
        })
        .detach();
        let focus = page.read(cx).focus.clone();
        window.focus(&focus, cx);
        page
    });
    cx.run_until_parked();
    // A frame, so the page can hand its focus to the diff view.
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .expect("frame");
    cx.run_until_parked();
    Page {
        window,
        page,
        settings: settings_out.expect("settings"),
        http,
        events,
    }
}

fn press(cx: &mut TestAppContext, window: AnyWindowHandle, keys: &[&'static str]) {
    for k in keys {
        cx.update_window(window, |_, window, cx| window.press(k, cx))
            .expect("press");
        cx.run_until_parked();
    }
}

fn requests(http: &RouteHttpClient, needle: &str) -> usize {
    http.requested()
        .iter()
        .filter(|r| r.contains(needle))
        .count()
}

#[gpui_kit::test]
fn a_cached_diff_is_shown_without_a_request(cx: &mut TestAppContext) {
    let mut store = seeded();
    let server = store.server_id(URL).expect("server");
    // Warm the cache the way a first visit would.
    let diff = crate::diff::tests::fixture("handwritten/diff_rename.response.json");
    let key = DiffKey {
        change: ChangeNumber(7),
        base: None,
        target: ps(3),
        path: "src/a.rs".into(),
        whitespace: Whitespace::IgnoreNone,
    };
    store.save_diff(server, &key, &diff, now()).expect("diff");
    let p = diff_page(cx, store, "src/a.rs");
    cx.update(|cx| assert!(p.page.read(cx).view_for_test().is_some()));
    assert_eq!(
        requests(&p.http, "src%2Fa.rs/diff"),
        0,
        "{:?}",
        p.http.requested()
    );
}

#[gpui_kit::test]
fn a_missing_diff_is_fetched_once_and_the_next_one_is_prefetched(cx: &mut TestAppContext) {
    let p = diff_page(cx, seeded(), "src/a.rs");
    cx.update(|cx| assert!(p.page.read(cx).view_for_test().is_some()));
    assert_eq!(requests(&p.http, "src%2Fa.rs/diff"), 1);
    assert_eq!(
        requests(&p.http, "src%2Fb.rs/diff"),
        1,
        "{:?}",
        p.http.requested()
    );
    assert_eq!(requests(&p.http, "src%2Fc.rs/diff"), 0);
}

#[gpui_kit::test]
fn an_unknown_patch_set_is_an_error_not_a_blank_page(cx: &mut TestAppContext) {
    let http = http();
    let env = env_with(seeded(), &http);
    let session = crate::session::Session::new(&env, &setup(), Some(gerrit_api::Auth::Anonymous))
        .expect("session");
    let (_w, page) = open(cx, |_, cx| {
        let settings = cx.new(|_| DiffSettings::new(LocalDiff::default()));
        cx.new(|cx| {
            DiffPage::new(
                session,
                settings,
                DiffRoute {
                    target: PatchSetNumber(9),
                    ..route("src/a.rs")
                },
                cx,
            )
        })
    });
    cx.run_until_parked();
    cx.update(|cx| {
        let p = page.read(cx);
        assert!(p.view_for_test().is_none());
        assert!(
            p.error_for_test()
                .is_some_and(|e| e.contains("not available"))
        );
    });
}

#[gpui_kit::test]
fn brackets_ask_for_the_neighbouring_files_and_stop_at_the_ends(cx: &mut TestAppContext) {
    let p = diff_page(cx, seeded(), "src/a.rs");
    press(cx, p.window, &["]"]);
    press(cx, p.window, &["["]);
    assert_eq!(
        *p.events.lock().expect("log"),
        ["open src/b.rs", "open /COMMIT_MSG"]
    );
    // The last and first file have no neighbour in that direction.
    let last = diff_page(cx, seeded(), "src/c.rs");
    press(cx, last.window, &["]"]);
    assert!(last.events.lock().expect("log").is_empty());
    let first = diff_page(cx, seeded(), "/COMMIT_MSG");
    press(cx, first.window, &["["]);
    assert!(first.events.lock().expect("log").is_empty());
}

fn folds(cx: &mut TestAppContext, p: &Page) -> Vec<u32> {
    cx.update(|cx| {
        let v = p.page.read(cx).view_for_test().expect("view");
        v.read(cx)
            .layout()
            .rows()
            .iter()
            .filter_map(|r| match r {
                crate::diff::layout::Row::Fold(f) => Some(f.lines),
                _ => None,
            })
            .collect()
    })
}

#[gpui_kit::test]
fn the_server_diff_is_folded_by_the_preferred_context(cx: &mut TestAppContext) {
    let p = diff_page_with(cx, seeded(), "src/a.rs", http_with(whole_file()));
    // Default context 10: 40 lines before the change, 39 after it.
    assert_eq!(folds(cx, &p), [30, 29]);
    // The request carries no context, the server would ignore it.
    assert_eq!(requests(&p.http, "context"), 0, "{:?}", p.http.requested());
}

#[gpui_kit::test]
fn shift_x_opens_the_folds_locally_and_keeps_the_cursor(cx: &mut TestAppContext) {
    let p = diff_page_with(cx, seeded(), "src/a.rs", http_with(whole_file()));
    cx.update(|cx| {
        let v = p.page.read(cx).view_for_test().expect("view");
        v.update(cx, |v, cx| v.set_cursor(14, cx));
    });
    let before = cx.update(|cx| {
        let v = p.page.read(cx).view_for_test().expect("view");
        v.read(cx).cursor_anchor()
    });
    let n = p.http.requested().len();
    press(cx, p.window, &["shift-x"]);
    assert!(folds(cx, &p).is_empty());
    assert_eq!(p.http.requested().len(), n, "{:?}", p.http.requested());
    cx.update(|cx| {
        let page = p.page.read(cx);
        let v = page.view_for_test().expect("view");
        assert_eq!(v.read(cx).cursor_anchor(), before);
        assert_eq!(v.read(cx).status(), None);
    });
    press(cx, p.window, &["shift-x"]);
    assert_eq!(folds(cx, &p), [30, 29]);
}

#[gpui_kit::test]
fn enter_on_a_fold_opens_it_without_a_request(cx: &mut TestAppContext) {
    let p = diff_page_with(cx, seeded(), "src/a.rs", http_with(whole_file()));
    let n = p.http.requested().len();
    press(cx, p.window, &["enter"]); // the cursor starts on the first fold
    assert_eq!(folds(cx, &p), [29]);
    assert_eq!(p.http.requested().len(), n);
}

/// The header says what is on screen, and a refetch that fails shows up on
/// the view (it used to be swallowed: the old diff stayed, with no sign).
#[gpui_kit::test]
fn a_failed_whitespace_refetch_is_reported_and_the_header_stays_truthful(cx: &mut TestAppContext) {
    let http = http_with(whole_file());
    http.route("whitespace=IGNORE_TRAILING", 400, "bad request");
    let p = diff_page_with(cx, seeded(), "src/a.rs", http);
    press(cx, p.window, &[",", "w"]);
    cx.update(|cx| {
        let page = p.page.read(cx);
        assert_eq!(
            page.shown_for_test().map(|k| k.whitespace.clone()),
            Some(Whitespace::IgnoreNone),
            "still showing the old diff"
        );
        assert_eq!(
            page.key_for_test().map(|k| k.whitespace.clone()),
            Some(Whitespace::IgnoreNone)
        );
        let v = page.view_for_test().expect("view");
        match v.read(cx).status() {
            Some(crate::diff::DiffStatus::Error(m)) => {
                assert!(m.contains("Could not load the diff"), "{m}")
            }
            other => panic!("expected an error status, got {other:?}"),
        }
    });
}

#[gpui_kit::test]
fn a_whitespace_refetch_keeps_the_folds_the_user_opened(cx: &mut TestAppContext) {
    let p = diff_page_with(cx, seeded(), "src/a.rs", http_with(whole_file()));
    press(cx, p.window, &["enter"]);
    assert_eq!(folds(cx, &p), [29]);
    press(cx, p.window, &[",", "w", "escape"]);
    assert_eq!(
        requests(&p.http, "a.rs/diff?intraline&whitespace=IGNORE_TRAILING"),
        1
    );
    assert_eq!(folds(cx, &p), [29]);
}

#[gpui_kit::test]
fn the_preferences_panel_changes_session_settings_and_refetches(cx: &mut TestAppContext) {
    let p = diff_page_with(cx, seeded(), "src/a.rs", http_with(whole_file()));
    press(cx, p.window, &[","]);
    cx.update(|cx| assert!(p.page.read(cx).prefs_open_for_test()));

    press(cx, p.window, &["w"]); // whitespace: trailing
    assert_eq!(
        requests(&p.http, "a.rs/diff?intraline&whitespace=IGNORE_TRAILING"),
        1,
        "{:?}",
        p.http.requested()
    );
    // Context only folds differently: 40 lines, 25 of them kept.
    let n = p.http.requested().len();
    press(cx, p.window, &["c"]); // context 10 -> 25
    assert_eq!(p.http.requested().len(), n, "{:?}", p.http.requested());
    assert_eq!(folds(cx, &p), [15, 14]);
    press(cx, p.window, &["m"]);
    cx.update(|cx| {
        let r = p.settings.read(cx).resolved();
        assert_eq!(r.ignore_whitespace, Whitespace::IgnoreTrailing);
        assert_eq!(r.context, DiffContext::Lines(25));
        assert_eq!(r.view_mode, DiffViewMode::Unified);
        let v = p.page.read(cx).view_for_test().expect("view");
        assert_eq!(v.read(cx).mode(), DiffViewMode::Unified);
    });
    press(cx, p.window, &["t", "s", "i"]);
    cx.update(|cx| {
        let r = p.settings.read(cx).resolved();
        assert_eq!(r.tab_size, 2); // 8 -> 2
        assert!(!r.show_tabs);
        assert!(!r.intraline_difference);
    });
    // Esc closes the panel and the diff view has the keyboard again.
    press(cx, p.window, &["escape"]);
    cx.update(|cx| assert!(!p.page.read(cx).prefs_open_for_test()));
    let before = cx.update(|cx| {
        let v = p.page.read(cx).view_for_test().expect("view");
        v.read(cx).cursor()
    });
    press(cx, p.window, &["j"]);
    cx.update(|cx| {
        let v = p.page.read(cx).view_for_test().expect("view");
        assert!(v.read(cx).cursor() > before);
    });
}

fn shell_flow(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Shell>, RouteHttpClient) {
    let http = http();
    let startup = Startup {
        env: env_with(seeded(), &http),
        server: Some(setup()),
        queries: vec![SavedQuery {
            name: "Open".into(),
            query: "status:open".into(),
        }],
        diff: LocalDiff::default(),
    };
    let (window, shell) = open(cx, |window, cx| {
        cx.new(|cx| Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    (window, shell, http)
}

fn current_route(cx: &mut TestAppContext, shell: &Entity<Shell>) -> Route {
    cx.update(|cx| shell.read(cx).route().clone())
}

#[gpui_kit::test]
fn enter_opens_the_diff_page_and_u_returns_with_the_cursor_on_that_file(cx: &mut TestAppContext) {
    let (window, shell, _http) = shell_flow(cx);
    press(cx, window, &["j", "enter"]);
    assert_eq!(current_route(cx, &shell), Route::Change(ChangeNumber(7)));
    press(cx, window, &["j", "j", "enter"]);
    assert_eq!(current_route(cx, &shell), Route::Diff(route("src/b.rs")));
    // [ and ] move between files; every file is a history entry.
    press(cx, window, &["]"]);
    assert_eq!(current_route(cx, &shell), Route::Diff(route("src/c.rs")));
    press(cx, window, &["]"]);
    assert_eq!(current_route(cx, &shell), Route::Diff(route("src/c.rs")));
    press(cx, window, &["u"]);
    assert_eq!(current_route(cx, &shell), Route::Change(ChangeNumber(7)));
    cx.update(|cx| {
        let page: Entity<ChangePage> = shell.read(cx).change_page_for_test().expect("change page");
        assert_eq!(page.read(cx).cursor_path(), Some("src/c.rs"));
    });
    // Forward goes back into the diff history.
    press(cx, window, &["alt-right"]);
    assert_eq!(current_route(cx, &shell), Route::Diff(route("src/b.rs")));
}

#[gpui_kit::test]
fn back_from_a_diff_restores_the_change_page_cursor(cx: &mut TestAppContext) {
    let (window, shell, _http) = shell_flow(cx);
    press(cx, window, &["j", "enter", "j", "j", "enter"]);
    press(cx, window, &["alt-left"]);
    assert_eq!(current_route(cx, &shell), Route::Change(ChangeNumber(7)));
    cx.update(|cx| {
        let page = shell.read(cx).change_page_for_test().expect("change page");
        assert_eq!(page.read(cx).cursor_path(), Some("src/b.rs"));
    });
}

#[gpui_kit::test]
fn the_view_mode_survives_moving_between_files(cx: &mut TestAppContext) {
    let (window, shell, _http) = shell_flow(cx);
    press(cx, window, &["j", "enter", "j", "enter"]);
    press(cx, window, &["m"]);
    cx.update(|cx| {
        let page = shell.read(cx).diff_page_for_test().expect("diff page");
        let v = page.read(cx).view_for_test().expect("view");
        assert_eq!(v.read(cx).mode(), DiffViewMode::Unified);
    });
    press(cx, window, &["]"]);
    cx.update(|cx| {
        let page = shell.read(cx).diff_page_for_test().expect("diff page");
        let v = page.read(cx).view_for_test().expect("view");
        assert_eq!(v.read(cx).mode(), DiffViewMode::Unified);
    });
}
