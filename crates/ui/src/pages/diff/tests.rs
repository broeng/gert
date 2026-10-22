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
    filters::{FileFilter, FilterAction},
    nav::{DiffRoute, Route},
    outbox::DraftOutbox,
    pages::change::ChangePage,
    session::{SavedQuery, Startup},
    settings::{DiffSettings, FileFilters},
    shell::Shell,
    syntax_cache::{Highlight, SyntaxCache},
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
        thread: None,
    }
}

struct Page {
    window: AnyWindowHandle,
    page: Entity<DiffPage>,
    settings: Entity<DiffSettings>,
    filters: Entity<FileFilters>,
    syntax: Entity<SyntaxCache>,
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
    diff_page_as(cx, store, path, http, gerrit_api::Auth::Anonymous)
}

fn diff_page_as(
    cx: &mut TestAppContext,
    store: Store,
    path: &str,
    http: RouteHttpClient,
    auth: gerrit_api::Auth,
) -> Page {
    diff_page_route(cx, store, route(path), http, auth)
}

fn diff_page_route(
    cx: &mut TestAppContext,
    store: Store,
    route: DiffRoute,
    http: RouteHttpClient,
    auth: gerrit_api::Auth,
) -> Page {
    diff_page_ruled(cx, store, route, http, auth, Vec::new(), Vec::new())
}

/// Like [`diff_page_route`], with file filter rules from the start.
fn diff_page_ruled(
    cx: &mut TestAppContext,
    store: Store,
    route: DiffRoute,
    http: RouteHttpClient,
    auth: gerrit_api::Auth,
    rules: Vec<FileFilter>,
    seed: Vec<(DiffKey, Highlight)>,
) -> Page {
    let env = env_with(store, &http);
    let mut server = setup();
    if let gerrit_api::Auth::Basic { username, .. } = &auth {
        server.username = Some(username.clone());
    }
    let session = crate::session::Session::new(&env, &server, Some(auth)).expect("session");
    let events = Arc::new(Mutex::new(Vec::new()));
    let log = events.clone();
    let mut settings_out = None;
    let mut filters_out = None;
    let mut syntax_out = None;
    let (window, page) = open(cx, |window, cx| {
        let settings = cx.new(|_| DiffSettings::new(LocalDiff::default()));
        settings_out = Some(settings.clone());
        let outbox = cx.new(|_| DraftOutbox::new(session.clone()));
        let filters = cx.new(|_| FileFilters::new(rules));
        filters_out = Some(filters.clone());
        let syntax = cx.new(|_| {
            let mut c = SyntaxCache::new();
            for (k, h) in seed {
                c.insert(k, h);
            }
            c
        });
        syntax_out = Some(syntax.clone());
        let page =
            cx.new(|cx| DiffPage::new(session, settings, filters, syntax, route, outbox, cx));
        cx.subscribe(&page, move |_, ev: &super::DiffPageEvent, _| {
            let text = match ev {
                super::DiffPageEvent::Unauthorized => "unauthorized".to_owned(),
                super::DiffPageEvent::OpenFile(r) => format!("open {}", r.path),
                super::DiffPageEvent::Reply(_) => "reply".to_owned(),
                super::DiffPageEvent::Actions(_) => "actions".to_owned(),
                super::DiffPageEvent::UpToChange => "up".to_owned(),
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
        filters: filters_out.expect("filters were created"),
        syntax: syntax_out.expect("cache was created"),
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
        let outbox = cx.new(|_| DraftOutbox::new(session.clone()));
        let filters = cx.new(|_| FileFilters::new(Vec::new()));
        let syntax = cx.new(|_| SyntaxCache::new());
        cx.new(|cx| {
            DiffPage::new(
                session,
                settings,
                filters,
                syntax,
                DiffRoute {
                    target: PatchSetNumber(9),
                    ..route("src/a.rs")
                },
                outbox,
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
    press(cx, p.window, &["t", "s", "i", "y"]);
    cx.update(|cx| {
        let r = p.settings.read(cx).resolved();
        assert_eq!(r.tab_size, 2); // 8 -> 2
        assert!(!r.show_tabs);
        assert!(!r.intraline_difference);
        assert!(!r.syntax_highlighting);
    });
    // Esc closes the panel and the diff view has the keyboard again.
    press(cx, p.window, &["escape"]);
    cx.update(|cx| assert!(!p.page.read(cx).prefs_open_for_test()));
    // `y` toggles syntax highlighting without the panel too.
    press(cx, p.window, &["y"]);
    cx.update(|cx| assert!(p.settings.read(cx).resolved().syntax_highlighting));
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
        start_page: Default::default(),
        filters: Vec::new(),
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
    // [ and ] move between files without adding history entries.
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
    // Forward goes back into the diff, on the file last viewed.
    press(cx, window, &["alt-right"]);
    assert_eq!(current_route(cx, &shell), Route::Diff(route("src/c.rs")));
}

#[gpui_kit::test]
fn back_after_stepping_through_files_leads_to_the_change(cx: &mut TestAppContext) {
    let (window, shell, _http) = shell_flow(cx);
    press(cx, window, &["j", "enter", "j", "j", "enter"]);
    press(cx, window, &["]"]);
    press(cx, window, &["["]);
    press(cx, window, &["["]);
    assert_eq!(current_route(cx, &shell), Route::Diff(route("src/a.rs")));
    press(cx, window, &["alt-left"]);
    assert_eq!(current_route(cx, &shell), Route::Change(ChangeNumber(7)));
    press(cx, window, &["alt-left"]);
    assert_eq!(current_route(cx, &shell), Route::Dashboard);
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

// --- comment threads ---------------------------------------------------

fn comments_json(ids: &[&str]) -> String {
    let entries: Vec<String> = ids
        .iter()
        .map(|id| {
            format!(
                r#"{{"id":"{id}","patch_set":3,"line":1,"message":"say {id}","updated":"2024-06-15 10:00:00.000000000","unresolved":true,"author":{{"_account_id":5,"name":"Rita"}}}}"#
            )
        })
        .collect();
    format!(r#"{{"src/a.rs":[{}]}}"#, entries.join(","))
}

fn shown_threads(cx: &mut TestAppContext, p: &Page) -> Vec<String> {
    cx.update(|cx| {
        let v = p.page.read(cx).view_for_test().expect("view");
        v.read(cx)
            .shown_threads()
            .map(|t| match &t.id {
                model::ThreadId::Comment(id) => id.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    })
}

#[gpui_kit::test]
fn cached_comments_show_first_and_the_server_refreshes_them(cx: &mut TestAppContext) {
    let mut store = seeded();
    let server = store.server_id(URL).expect("server");
    let mut cached =
        crate::diff::tests::threads::comment("old", model::Location::Line(1), "cached", true);
    cached.path = "src/a.rs".into();
    cached.patch_set = PatchSetNumber(3);
    store
        .save_comments(server, ChangeNumber(7), &[cached], now())
        .expect("comments");
    let http = http();
    http.route_json("/comments", &comments_json(&["old", "new"]));
    let p = diff_page_with(cx, store, "src/a.rs", http);
    let mut got = shown_threads(cx, &p);
    got.sort();
    assert_eq!(got, ["new", "old"]);
    assert_eq!(requests(&p.http, "/comments"), 1);
    assert!(p.events.lock().expect("log").is_empty());
}

#[gpui_kit::test]
fn a_route_with_a_thread_puts_the_cursor_on_it_once_the_comments_are_there(
    cx: &mut TestAppContext,
) {
    let http = http();
    http.route_json("/comments", &comments_json(&["one", "two"]));
    let p = diff_page_route(
        cx,
        seeded(),
        DiffRoute {
            thread: Some(model::ThreadId::Comment("two".into())),
            ..route("src/a.rs")
        },
        http,
        gerrit_api::Auth::Anonymous,
    );
    cx.update(|cx| {
        let v = p.page.read(cx).view_for_test().expect("view");
        let v = v.read(cx);
        let two = model::ThreadId::Comment("two".into());
        assert_eq!(v.cursor_thread(), Some(two.clone()));
        assert!(v.is_thread_open(&two));
    });
    // Moving to another file does not carry the thread along.
    press(cx, p.window, &["]"]);
    assert_eq!(p.events.lock().expect("log").clone(), ["open src/b.rs"]);
}

#[gpui_kit::test]
fn a_401_on_comments_sends_the_user_to_login_but_a_403_does_not(cx: &mut TestAppContext) {
    let http = http();
    http.route("/comments", 401, "no");
    let p = diff_page_with(cx, seeded(), "src/a.rs", http);
    assert!(
        p.events
            .lock()
            .expect("log")
            .contains(&"unauthorized".to_owned())
    );

    // Logged in, but not allowed: the user stays logged in.
    let http = self::http();
    http.route("/comments", 403, "forbidden");
    let auth = gerrit_api::Auth::Basic {
        username: "alice".into(),
        password: "secret".into(),
    };
    let p = diff_page_as(cx, seeded(), "src/a.rs", http, auth);
    assert!(p.events.lock().expect("log").is_empty());
    // The diff itself still works.
    cx.update(|cx| assert!(p.page.read(cx).view_for_test().is_some()));
}

fn signed_in() -> gerrit_api::Auth {
    gerrit_api::Auth::Basic {
        username: "alice".into(),
        password: "secret".into(),
    }
}

#[gpui_kit::test]
fn a_comment_written_on_the_page_is_saved_uploaded_and_shown(cx: &mut TestAppContext) {
    let http = http();
    http.route_json("/comments", "{}");
    http.route_json("/changes/7/drafts", "{}");
    http.route(
        "/revisions/sha3/drafts",
        201,
        r#")]}'
{"id":"srv1","patch_set":3,"path":"src/a.rs","line":1,"updated":"2024-06-15 10:00:00.000000000","message":"m"}"#,
    );
    let p = diff_page_as(cx, seeded(), "src/a.rs", http, signed_in());
    press(cx, p.window, &["c"]);
    cx.update_window(p.window, |_, window, cx| window.input("looks odd", cx))
        .expect("input");
    let save = if cfg!(target_os = "macos") {
        "cmd-enter"
    } else {
        "ctrl-enter"
    };
    press(cx, p.window, &[save]);
    cx.run_until_parked();
    assert_eq!(requests(&p.http, "/revisions/sha3/drafts"), 1);
    // The draft is a thread of its own now, uploaded.
    let (threads, synced) = cx.update(|cx| {
        let v = p.page.read(cx).view_for_test().expect("view");
        let v = v.read(cx);
        let threads: Vec<_> = v.shown_threads().collect();
        let synced = threads
            .iter()
            .all(|t| matches!(t.draft().map(|d| &d.sync), Some(model::DraftSync::Synced)));
        (threads.len(), synced)
    });
    assert_eq!((threads, synced), (1, true));
}

fn reviewed_store(paths: &[&str]) -> Store {
    let mut store = seeded();
    let server = store.server_id(URL).expect("server");
    let files: Vec<_> = PATHS
        .iter()
        .map(|p| file(p, FileStatus::Modified))
        .collect();
    let marks: Vec<String> = paths.iter().map(|p| (*p).to_owned()).collect();
    store
        .save_file_list(server, &files_key(), &files, Some(&marks), now())
        .expect("files");
    store
}

fn is_reviewed(cx: &mut TestAppContext, p: &Page) -> bool {
    cx.update(|cx| p.page.read(cx).is_reviewed_for_test())
}

const PUT_A: &str = "PUT /changes/7/revisions/sha3/files/src%2Fa.rs/reviewed";
const DELETE_A: &str = "DELETE /changes/7/revisions/sha3/files/src%2Fa.rs/reviewed";

#[gpui_kit::test]
fn r_toggles_the_reviewed_mark_of_the_shown_file(cx: &mut TestAppContext) {
    let http = http();
    http.route(PUT_A, 201, "").route(DELETE_A, 204, "");
    let p = diff_page_route(
        cx,
        reviewed_store(&[]),
        route("src/a.rs"),
        http,
        signed_in(),
    );
    assert!(!is_reviewed(cx, &p));
    press(cx, p.window, &["r"]);
    assert!(is_reviewed(cx, &p));
    press(cx, p.window, &["r"]);
    assert!(!is_reviewed(cx, &p));
    let sent: Vec<_> = p
        .http
        .sent()
        .into_iter()
        .filter(|r| r.target.ends_with("/reviewed"))
        .map(|r| r.method)
        .collect();
    assert_eq!(sent, ["PUT", "DELETE"]);
}

#[gpui_kit::test]
fn the_cached_marks_show_on_the_diff_page(cx: &mut TestAppContext) {
    let p = diff_page_route(
        cx,
        reviewed_store(&["src/a.rs"]),
        route("src/a.rs"),
        http(),
        signed_in(),
    );
    assert!(is_reviewed(cx, &p));
    let other = diff_page_route(
        cx,
        reviewed_store(&["src/a.rs"]),
        route("src/b.rs"),
        http(),
        signed_in(),
    );
    assert!(!is_reviewed(cx, &other));
}

#[gpui_kit::test]
fn a_refused_mark_is_taken_back_with_a_note(cx: &mut TestAppContext) {
    let http = http();
    http.route(PUT_A, 403, "no");
    let p = diff_page_route(
        cx,
        reviewed_store(&[]),
        route("src/a.rs"),
        http,
        signed_in(),
    );
    press(cx, p.window, &["r"]);
    assert!(!is_reviewed(cx, &p));
    let note = cx.update(|cx| p.page.read(cx).hint_for_test());
    assert!(matches!(note, Some((t, true)) if t.contains("src/a.rs")));
    assert!(
        p.events.lock().expect("log").is_empty(),
        "403 keeps the login"
    );
}

#[gpui_kit::test]
fn a_401_on_the_mark_asks_for_a_login(cx: &mut TestAppContext) {
    let http = http();
    http.route(PUT_A, 401, "");
    let p = diff_page_route(
        cx,
        reviewed_store(&[]),
        route("src/a.rs"),
        http,
        signed_in(),
    );
    press(cx, p.window, &["r"]);
    assert!(!is_reviewed(cx, &p));
    assert_eq!(*p.events.lock().expect("log"), ["unauthorized"]);
}

#[gpui_kit::test]
fn r_only_hints_when_anonymous(cx: &mut TestAppContext) {
    let p = diff_page_route(
        cx,
        reviewed_store(&[]),
        route("src/a.rs"),
        http(),
        gerrit_api::Auth::Anonymous,
    );
    press(cx, p.window, &["r"]);
    assert!(!is_reviewed(cx, &p));
    let note = cx.update(|cx| p.page.read(cx).hint_for_test());
    assert!(matches!(note, Some((t, false)) if t.starts_with("Sign in")));
    assert!(p.http.sent().iter().all(|r| r.method == "GET"));
}

#[gpui_kit::test]
fn a_asks_for_the_reply_dialog_when_signed_in(cx: &mut TestAppContext) {
    let p = diff_page_route(
        cx,
        reviewed_store(&[]),
        route("src/a.rs"),
        http(),
        signed_in(),
    );
    press(cx, p.window, &["a"]);
    assert_eq!(*p.events.lock().expect("log"), ["reply"]);
}

#[gpui_kit::test]
fn a_only_hints_when_anonymous(cx: &mut TestAppContext) {
    let p = diff_page_route(
        cx,
        reviewed_store(&[]),
        route("src/a.rs"),
        http(),
        gerrit_api::Auth::Anonymous,
    );
    press(cx, p.window, &["a"]);
    assert!(p.events.lock().expect("log").is_empty());
    let note = cx.update(|cx| p.page.read(cx).hint_for_test());
    assert!(matches!(note, Some((t, false)) if t == "Sign in to reply"));
}

#[gpui_kit::test]
fn shift_a_asks_for_the_action_menu_and_m_still_toggles_the_mode(cx: &mut TestAppContext) {
    let p = diff_page_route(
        cx,
        reviewed_store(&[]),
        route("src/a.rs"),
        http(),
        signed_in(),
    );
    let mode = |cx: &mut TestAppContext, p: &Page| {
        cx.update(|cx| p.settings.read(cx).resolved().view_mode)
    };
    let before = mode(cx, &p);
    press(cx, p.window, &["m"]);
    assert_ne!(mode(cx, &p), before);
    assert!(p.events.lock().expect("log").is_empty());
    press(cx, p.window, &["shift-a"]);
    assert_eq!(*p.events.lock().expect("log"), ["actions"]);
}

#[gpui_kit::test]
fn shift_a_only_hints_when_anonymous(cx: &mut TestAppContext) {
    let p = diff_page_route(
        cx,
        reviewed_store(&[]),
        route("src/a.rs"),
        http(),
        gerrit_api::Auth::Anonymous,
    );
    press(cx, p.window, &["shift-a"]);
    assert!(p.events.lock().expect("log").is_empty());
    let note = cx.update(|cx| p.page.read(cx).hint_for_test());
    assert!(matches!(note, Some((t, false)) if t == "Sign in to change this change"));
}

fn done(
    kind: crate::action_menu::Kind,
    detail: Option<model::ChangeDetail>,
) -> crate::action_menu::Done {
    crate::action_menu::Done {
        kind,
        detail,
        opened: None,
    }
}

#[gpui_kit::test]
fn an_action_on_the_current_patch_set_moves_the_page_when_a_new_one_arrives(
    cx: &mut TestAppContext,
) {
    use crate::action_menu::Kind;
    let p = diff_page_route(
        cx,
        reviewed_store(&[]),
        route("src/a.rs"),
        http(),
        signed_in(),
    );
    // A rebase: patch set 4 is current now.
    let moved = cx.update(|cx| {
        p.page.update(cx, |p, cx| {
            p.action_done(&done(Kind::Rebase, Some(detail(7, 4, 4))), cx)
        })
    });
    let mut want = route("src/a.rs");
    want.target = PatchSetNumber(4);
    assert_eq!(moved, Some(want));
    // Something else changed: the chips follow, the page stays.
    let mut abandoned = detail(7, 4, 4);
    abandoned.summary.status = model::ChangeStatus::Abandoned;
    let moved = cx.update(|cx| {
        p.page.update(cx, |p, cx| {
            p.action_done(&done(Kind::Abandon, Some(abandoned)), cx)
        })
    });
    assert_eq!(moved, None);
    cx.update(|cx| {
        let page = p.page.read(cx);
        let d = page.detail_for_test().expect("detail");
        assert_eq!(d.summary.status, model::ChangeStatus::Abandoned);
        assert_eq!(page.hint_for_test(), Some(("Abandoned".to_owned(), false)));
    });
}

#[gpui_kit::test]
fn an_older_patch_set_stays_and_the_note_names_the_current_one(cx: &mut TestAppContext) {
    use crate::action_menu::Kind;
    let mut older = route("src/a.rs");
    older.target = PatchSetNumber(2);
    let p = diff_page_route(cx, reviewed_store(&[]), older, http(), signed_in());
    let moved = cx.update(|cx| {
        p.page.update(cx, |p, cx| {
            p.action_done(&done(Kind::Rebase, Some(detail(7, 4, 4))), cx)
        })
    });
    assert_eq!(moved, None);
    cx.update(|cx| {
        let page = p.page.read(cx);
        assert_eq!(
            page.hint_for_test(),
            Some(("Rebased; patchset 4 is now current".to_owned(), false))
        );
        assert_eq!(
            page.detail_for_test().and_then(|d| d.current_patch_set),
            Some(PatchSetNumber(4))
        );
    });
}

// --- file filters ------------------------------------------------------

fn rule(name: &str, glob: &str, action: FilterAction) -> FileFilter {
    FileFilter {
        name: name.into(),
        globs: vec![glob.into()],
        projects: Vec::new(),
        action,
        enabled: true,
    }
}

fn filtered_page(cx: &mut TestAppContext, path: &str, rules: Vec<FileFilter>) -> Page {
    diff_page_ruled(
        cx,
        seeded(),
        route(path),
        http(),
        gerrit_api::Auth::Anonymous,
        rules,
        Vec::new(),
    )
}

fn opened(p: &Page) -> Vec<String> {
    p.events.lock().expect("log").clone()
}

#[gpui_kit::test]
fn brackets_skip_hidden_and_dimmed_files(cx: &mut TestAppContext) {
    // Files: COMMIT_MSG, a (dimmed), b (hidden), c.
    let rules = vec![
        rule("Dim", "a.rs", FilterAction::Dim),
        rule("Gen", "b.rs", FilterAction::Hide),
    ];
    let p = filtered_page(cx, "/COMMIT_MSG", rules.clone());
    press(cx, p.window, &["]"]);
    assert_eq!(opened(&p), ["open src/c.rs"]);
    let p = filtered_page(cx, "src/c.rs", rules.clone());
    press(cx, p.window, &["["]);
    assert_eq!(opened(&p), ["open /COMMIT_MSG"]);
    // Past the last unskipped file there is nowhere to go.
    press(cx, p.window, &["]"]);
    assert_eq!(opened(&p), ["open /COMMIT_MSG"]);
    // A skipped file can be shown (from a comment); `]` goes on from it.
    let p = filtered_page(cx, "src/a.rs", rules);
    press(cx, p.window, &["]"]);
    assert_eq!(opened(&p), ["open src/c.rs"]);
}

#[gpui_kit::test]
fn a_toggle_changes_where_brackets_go_at_once(cx: &mut TestAppContext) {
    let p = filtered_page(
        cx,
        "src/a.rs",
        vec![rule("Gen", "b.rs", FilterAction::Hide)],
    );
    press(cx, p.window, &["]"]);
    assert_eq!(opened(&p), ["open src/c.rs"]);
    p.filters.update(cx, |f, cx| f.toggle("Gen", cx));
    press(cx, p.window, &["]"]);
    assert_eq!(opened(&p), ["open src/c.rs", "open src/b.rs"]);
}

#[gpui_kit::test]
fn the_position_counts_only_files_that_are_not_hidden(cx: &mut TestAppContext) {
    let rules = vec![rule("Gen", "a.rs", FilterAction::Hide)];
    let p = filtered_page(cx, "src/b.rs", rules.clone());
    cx.update(|cx| {
        assert_eq!(p.page.read(cx).position_label().as_deref(), Some("2 of 3"));
    });
    let p = filtered_page(cx, "src/a.rs", rules);
    cx.update(|cx| {
        let page = p.page.read(cx);
        assert_eq!(page.position_label().as_deref(), Some("\u{2013} of 3"));
        let style = page.current_style().expect("style");
        assert!(style.hidden);
        assert_eq!(style.rules, ["Gen"]);
    });
    // Un-hiding restyles the open page.
    p.filters.update(cx, |f, cx| f.toggle("Gen", cx));
    cx.update(|cx| {
        let page = p.page.read(cx);
        assert_eq!(page.position_label().as_deref(), Some("2 of 4"));
        assert!(page.current_style().is_some_and(|s| s.rules.is_empty()));
    });
}

#[gpui_kit::test]
fn the_prefetch_goes_to_the_next_unskipped_file(cx: &mut TestAppContext) {
    let p = filtered_page(
        cx,
        "src/a.rs",
        vec![rule("Gen", "b.rs", FilterAction::Hide)],
    );
    assert_eq!(requests(&p.http, "src%2Fa.rs/diff"), 1);
    assert_eq!(
        requests(&p.http, "src%2Fb.rs/diff"),
        0,
        "{:?}",
        p.http.requested()
    );
    assert_eq!(requests(&p.http, "src%2Fc.rs/diff"), 1);
}

// ---- syntax cache ----

fn rust_body() -> Vec<u8> {
    let body = serde_json::json!({
        "meta_a": {"name": "src/a.rs", "content_type": "text/x-rust", "lines": 3},
        "meta_b": {"name": "src/a.rs", "content_type": "text/x-rust", "lines": 3},
        "change_type": "MODIFIED",
        "content": [
            {"ab": ["// hello", "fn main() {"]},
            {"a": ["    let x = 1;"], "b": ["    let x = \"two\";"]},
            {"ab": ["}"]},
        ],
    });
    format!(")]}}'\n{body}").into_bytes()
}

fn a_key() -> DiffKey {
    DiffKey {
        change: ChangeNumber(7),
        base: None,
        target: ps(3),
        path: "src/a.rs".into(),
        whitespace: Whitespace::IgnoreNone,
    }
}

/// Stands for a result only the cache could have: a real job fills the sides.
fn marker() -> crate::diff::syntax::DiffSyntax {
    crate::diff::syntax::DiffSyntax::default()
}

fn view_syntax_is_marker(cx: &mut TestAppContext, p: &Page) -> bool {
    cx.update(|cx| {
        let v = p.page.read(cx).view_for_test().expect("view");
        v.read(cx)
            .syntax_for_test()
            .is_some_and(|s| s.old.is_none() && s.new.is_none())
    })
}

#[gpui_kit::test]
fn a_cache_hit_is_the_views_syntax_with_no_job(cx: &mut TestAppContext) {
    let p = diff_page_ruled(
        cx,
        seeded(),
        route("src/a.rs"),
        http_with(rust_body()),
        gerrit_api::Auth::Anonymous,
        Vec::new(),
        vec![(a_key(), Some(marker()))],
    );
    // A job would have replaced the marker with real sides.
    assert!(view_syntax_is_marker(cx, &p));
}

#[gpui_kit::test]
fn a_cache_hit_for_a_refetched_diff_replaces_the_colors_at_once(cx: &mut TestAppContext) {
    let p = diff_page_with(cx, seeded(), "src/a.rs", http_with(rust_body()));
    assert!(!view_syntax_is_marker(cx, &p));
    // The colors of the diff with trailing whitespace ignored are known.
    cx.update(|cx| {
        p.syntax.update(cx, |c, _| {
            c.insert(
                DiffKey {
                    whitespace: Whitespace::IgnoreTrailing,
                    ..a_key()
                },
                Some(marker()),
            )
        });
    });
    press(cx, p.window, &[",", "w", "escape"]);
    assert!(view_syntax_is_marker(cx, &p));
}

#[gpui_kit::test]
fn a_miss_is_highlighted_by_the_view_and_stored(cx: &mut TestAppContext) {
    let p = diff_page_with(cx, seeded(), "src/a.rs", http_with(rust_body()));
    cx.update(|cx| {
        let got = p.syntax.update(cx, |c, _| c.get(&a_key()));
        let colored = got.expect("stored").expect("rust is colored");
        assert!(colored.new.is_some());
    });
}

#[gpui_kit::test]
fn a_plain_file_is_stored_as_plain(cx: &mut TestAppContext) {
    let p = diff_page_with(cx, seeded(), "/COMMIT_MSG", http_with(rust_body()));
    cx.update(|cx| {
        let key = DiffKey {
            path: "/COMMIT_MSG".into(),
            ..a_key()
        };
        assert_eq!(
            p.syntax.update(cx, |c, _| c.get(&key).map(|h| h.is_none())),
            Some(true)
        );
    });
}
