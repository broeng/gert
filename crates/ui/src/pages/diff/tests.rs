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
    store: Arc<Mutex<Store>>,
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
    diff_page_full(
        cx,
        store,
        route,
        http,
        auth,
        rules,
        seed,
        LocalDiff::default(),
    )
}

/// Like [`diff_page_ruled`], with local preference overrides too.
#[allow(clippy::too_many_arguments)]
fn diff_page_full(
    cx: &mut TestAppContext,
    store: Store,
    route: DiffRoute,
    http: RouteHttpClient,
    auth: gerrit_api::Auth,
    rules: Vec<FileFilter>,
    seed: Vec<(DiffKey, Highlight)>,
    local: LocalDiff,
) -> Page {
    let p = diff_page_loading(cx, store, route, http, auth, rules, seed, local);
    cx.run_until_parked();
    // A frame, so the page can hand its focus to the diff view.
    cx.update_window(p.window, |_, window, cx| window.render_frame(cx))
        .expect("frame");
    cx.run_until_parked();
    p
}

/// The page right after it was opened, before anything has loaded.
#[allow(clippy::too_many_arguments)]
fn diff_page_loading(
    cx: &mut TestAppContext,
    store: Store,
    route: DiffRoute,
    http: RouteHttpClient,
    auth: gerrit_api::Auth,
    rules: Vec<FileFilter>,
    seed: Vec<(DiffKey, Highlight)>,
    local: LocalDiff,
) -> Page {
    diff_page_loading_in(cx, store, route, http, auth, rules, seed, local, Vec::new())
}

/// Like [`diff_page_loading`], with local checkouts configured.
#[allow(clippy::too_many_arguments)]
fn diff_page_loading_in(
    cx: &mut TestAppContext,
    store: Store,
    route: DiffRoute,
    http: RouteHttpClient,
    auth: gerrit_api::Auth,
    rules: Vec<FileFilter>,
    seed: Vec<(DiffKey, Highlight)>,
    local: LocalDiff,
    projects: Vec<crate::session::ProjectSettings>,
) -> Page {
    diff_page_loading_as(
        cx,
        store,
        route,
        http,
        Some(auth),
        rules,
        seed,
        local,
        projects,
    )
}

/// Like [`diff_page_loading_in`]; `None` is a session that cannot sync.
#[allow(clippy::too_many_arguments)]
fn diff_page_loading_as(
    cx: &mut TestAppContext,
    store: Store,
    route: DiffRoute,
    http: RouteHttpClient,
    auth: Option<gerrit_api::Auth>,
    rules: Vec<FileFilter>,
    seed: Vec<(DiffKey, Highlight)>,
    local: LocalDiff,
    projects: Vec<crate::session::ProjectSettings>,
) -> Page {
    let env = env_with(store, &http);
    let store = env.store.clone();
    let mut server = setup();
    if let Some(gerrit_api::Auth::Basic { username, .. }) = &auth {
        server.username = Some(username.clone());
    }
    let session = crate::session::Session::new(&env, &server, auth).expect("session");
    let events = Arc::new(Mutex::new(Vec::new()));
    let log = events.clone();
    let mut settings_out = None;
    let mut filters_out = None;
    let mut syntax_out = None;
    let (window, page) = open(cx, |window, cx| {
        let settings = cx.new(|_| DiffSettings::new(local));
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
        let checkouts = cx.new(|_| crate::checkouts::Checkouts::new(projects));
        let page = cx.new(|cx| {
            let mut page = DiffPage::new(session, settings, filters, syntax, route, outbox, cx);
            page.set_checkouts(Some(checkouts), cx);
            page
        });
        cx.subscribe(&page, move |_, ev: &super::DiffPageEvent, _| {
            let text = match ev {
                super::DiffPageEvent::Unauthorized => "unauthorized".to_owned(),
                super::DiffPageEvent::OpenFile(r) => format!("open {}", r.path),
                super::DiffPageEvent::Compare(r) => format!("compare {:?} {}", r.base, r.path),
                // A local review names its version.
                super::DiffPageEvent::Reply(r) => match r.target {
                    Some(t) if t.is_local() => format!("reply to {}", t.0),
                    _ => "reply".to_owned(),
                },
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
    Page {
        window,
        page,
        settings: settings_out.expect("settings"),
        filters: filters_out.expect("filters were created"),
        syntax: syntax_out.expect("cache was created"),
        http,
        store,
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
fn a_missing_diff_is_fetched_once_and_the_files_after_it_are_fetched_ahead(
    cx: &mut TestAppContext,
) {
    let p = diff_page(cx, seeded(), "src/a.rs");
    cx.update(|cx| assert!(p.page.read(cx).view_for_test().is_some()));
    assert_eq!(requests(&p.http, "src%2Fa.rs/diff"), 1);
    // The syntax worker does the fetching, each file once; the magic file
    // is not worth coloring.
    assert_eq!(requests(&p.http, "src%2Fb.rs/diff"), 1);
    assert_eq!(requests(&p.http, "src%2Fc.rs/diff"), 1);
    assert_eq!(
        requests(&p.http, "COMMIT_MSG"),
        0,
        "{:?}",
        p.http.requested()
    );
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
fn brackets_ask_for_the_neighbouring_files_and_the_last_one_goes_up(cx: &mut TestAppContext) {
    let p = diff_page(cx, seeded(), "src/a.rs");
    press(cx, p.window, &["]"]);
    press(cx, p.window, &["["]);
    assert_eq!(
        *p.events.lock().expect("log"),
        ["open src/b.rs", "open /COMMIT_MSG"]
    );
    // `]` on the last file goes up to the change; `[` on the first file
    // has nowhere to go.
    let last = diff_page(cx, seeded(), "src/c.rs");
    press(cx, last.window, &["]"]);
    assert_eq!(opened(&last), ["up"]);
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
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
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
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
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
    shell_flow_with(cx, seeded())
}

/// Like [`shell_flow`], over `store`.
fn shell_flow_with(
    cx: &mut TestAppContext,
    store: Store,
) -> (AnyWindowHandle, Entity<Shell>, RouteHttpClient) {
    let http = http();
    let startup = Startup {
        env: env_with(store, &http),
        diff: LocalDiff::default(),
        start_page: Default::default(),
        filters: Vec::new(),
        servers: vec![crate::session::ServerSetup {
            queries: vec![SavedQuery {
                name: "Open".into(),
                query: "status:open".into(),
            }],
            bookmarks: Vec::new(),
            ..setup()
        }],
        active: None,
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
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn enter_opens_the_diff_page_and_u_returns_with_the_cursor_on_that_file(cx: &mut TestAppContext) {
    let (window, shell, _http) = shell_flow(cx);
    press(cx, window, &["j", "enter"]);
    assert_eq!(current_route(cx, &shell), Route::Change(ChangeNumber(7)));
    press(cx, window, &["j", "j", "enter"]);
    assert_eq!(current_route(cx, &shell), Route::Diff(route("src/b.rs")));
    // [ and ] move between files without adding history entries.
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
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn right_bracket_on_the_last_file_returns_to_the_change_with_the_cursor_on_it(
    cx: &mut TestAppContext,
) {
    let (window, shell, _http) = shell_flow(cx);
    press(cx, window, &["j", "enter", "j", "j", "j", "enter"]);
    assert_eq!(current_route(cx, &shell), Route::Diff(route("src/c.rs")));
    press(cx, window, &["]"]);
    assert_eq!(current_route(cx, &shell), Route::Change(ChangeNumber(7)));
    cx.update(|cx| {
        let page = shell.read(cx).change_page_for_test().expect("change page");
        assert_eq!(page.read(cx).cursor_path(), Some("src/c.rs"));
    });
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
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
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
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
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_chain_row_opens_a_change_at_a_patch_set_and_back_restores_the_page(cx: &mut TestAppContext) {
    use crate::{nav::ChangeView, pages::change::ChangeEvent};
    let mut store = seeded();
    let server = store.server_id(URL).expect("server");
    store
        .save_change_detail(server, &detail(8, 3, 3), now())
        .expect("detail");
    let (window, shell, _http) = shell_flow_with(cx, store);
    press(cx, window, &["j", "enter", "j", "j"]);
    assert_eq!(current_route(cx, &shell), Route::Change(ChangeNumber(7)));
    let index = |cx: &mut TestAppContext| cx.update(|cx| shell.read(cx).trail_for_test());
    let (_, before) = index(cx);
    let at = |n: u32| ChangeView {
        target: Some(PatchSetNumber(n)),
        ..ChangeView::default()
    };
    let emit = |cx: &mut TestAppContext, ev: ChangeEvent| {
        let page = cx.update(|cx| shell.read(cx).change_page_for_test().expect("change page"));
        page.update(cx, |_, cx| cx.emit(ev));
        cx.run_until_parked();
    };
    let selection = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            let page = shell.read(cx).change_page_for_test().expect("change page");
            page.read(cx).selection_for_test()
        })
    };

    // A new entry, on the patch set asked for.
    emit(cx, ChangeEvent::OpenChangeAt(ChangeNumber(8), at(2)));
    assert_eq!(current_route(cx, &shell), Route::Change(ChangeNumber(8)));
    assert_eq!(selection(cx), (None, Some(PatchSetNumber(2))));
    assert_eq!(index(cx).1, before + 1);

    // Back: change 7 as it was left.
    press(cx, window, &["alt-left"]);
    assert_eq!(current_route(cx, &shell), Route::Change(ChangeNumber(7)));
    cx.update(|cx| {
        let page = shell.read(cx).change_page_for_test().expect("change page");
        assert_eq!(page.read(cx).cursor_path(), Some("src/b.rs"));
    });

    // Replacing does not stack an entry.
    emit(cx, ChangeEvent::ReplaceChangeAt(ChangeNumber(8), at(1)));
    assert_eq!(current_route(cx, &shell), Route::Change(ChangeNumber(8)));
    assert_eq!(selection(cx), (None, Some(PatchSetNumber(1))));
    let (trail, position) = index(cx);
    assert_eq!((trail.len(), position), (before + 1, before));
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn compare_with_the_parent_adds_a_history_entry(cx: &mut TestAppContext) {
    let (window, shell, _http) = shell_flow(cx);
    press(cx, window, &["j", "enter", "j", "j", "enter"]);
    assert_eq!(current_route(cx, &shell), Route::Diff(route("src/b.rs")));
    let before = cx.update(|cx| shell.read(cx).trail_for_test().1);
    let other = DiffRoute {
        base: Some(PatchSetNumber(1)),
        ..route("src/b.rs")
    };
    let page = cx.update(|cx| shell.read(cx).diff_page_for_test().expect("diff page"));
    page.update(cx, |_, cx| {
        cx.emit(super::DiffPageEvent::Compare(other.clone()))
    });
    cx.run_until_parked();
    assert_eq!(current_route(cx, &shell), Route::Diff(other));
    assert_eq!(
        cx.update(|cx| shell.read(cx).trail_for_test().1),
        before + 1
    );
    press(cx, window, &["alt-left"]);
    assert_eq!(current_route(cx, &shell), Route::Diff(route("src/b.rs")));
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
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
            Some(("Rebased; PS 4 is now current".to_owned(), false))
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
        servers: Vec::new(),
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
fn a_bracket_pressed_while_the_page_loads_is_done_once_the_files_are_there(
    cx: &mut TestAppContext,
) {
    // Stepping through files opens a new page each time; a quick second
    // `]` lands before that page knows its files and must not be lost.
    let p = diff_page_loading(
        cx,
        seeded(),
        route("src/a.rs"),
        http(),
        gerrit_api::Auth::Anonymous,
        Vec::new(),
        Vec::new(),
        LocalDiff::default(),
    );
    cx.update(|cx| assert!(p.page.read(cx).files.list.is_empty()));
    press(cx, p.window, &["]"]);
    assert_eq!(opened(&p), ["open src/b.rs"]);
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
    // Past the last unskipped file `]` goes up.
    let p = filtered_page(cx, "src/c.rs", rules.clone());
    press(cx, p.window, &["]"]);
    assert_eq!(opened(&p), ["up"]);
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

// ---- highlighting the files ahead ----

fn queued(cx: &mut TestAppContext, p: &Page) -> Vec<String> {
    cx.update(|cx| {
        p.syntax
            .read(cx)
            .last_queue_for_test()
            .iter()
            .map(|k| k.path.clone())
            .collect()
    })
}

fn path_key(path: &str) -> DiffKey {
    DiffKey {
        path: path.into(),
        ..a_key()
    }
}

#[gpui_kit::test]
fn the_files_after_the_shown_one_are_queued_then_the_earlier_ones(cx: &mut TestAppContext) {
    let p = diff_page(cx, seeded(), "src/b.rs");
    assert_eq!(queued(cx, &p), ["src/c.rs", "src/a.rs"]);
    let p = diff_page(cx, seeded(), "/COMMIT_MSG");
    assert_eq!(queued(cx, &p), ["src/a.rs", "src/b.rs", "src/c.rs"]);
}

#[gpui_kit::test]
fn queued_files_end_up_highlighted_in_the_cache(cx: &mut TestAppContext) {
    let p = diff_page_with(cx, seeded(), "src/a.rs", http_with(rust_body()));
    cx.update(|cx| {
        p.syntax.update(cx, |c, _| {
            for path in ["src/b.rs", "src/c.rs"] {
                let hit = c.get(&path_key(path));
                assert!(matches!(hit, Some(Some(_))), "{path}: {hit:?}");
            }
            assert!(c.get(&path_key("/COMMIT_MSG")).is_none());
        });
    });
}

#[gpui_kit::test]
fn hidden_and_dimmed_files_are_not_fetched_ahead(cx: &mut TestAppContext) {
    let p = filtered_page(
        cx,
        "/COMMIT_MSG",
        vec![
            rule("Hide", "a.rs", FilterAction::Hide),
            rule("Dim", "b.rs", FilterAction::Dim),
        ],
    );
    assert_eq!(queued(cx, &p), ["src/c.rs"]);
    assert_eq!(requests(&p.http, "src%2Fa.rs/diff"), 0);
    assert_eq!(requests(&p.http, "src%2Fb.rs/diff"), 0);
    assert_eq!(requests(&p.http, "src%2Fc.rs/diff"), 1);
    // Showing a rule again queues the file.
    p.filters.update(cx, |f, cx| f.toggle("Dim", cx));
    cx.run_until_parked();
    assert_eq!(queued(cx, &p), ["src/b.rs", "src/c.rs"]);
    assert_eq!(requests(&p.http, "src%2Fb.rs/diff"), 1);
}

fn syntax_off() -> LocalDiff {
    let mut local = LocalDiff::default();
    local.preferences.syntax_highlighting = Some(false);
    local
}

#[gpui_kit::test]
fn with_highlighting_off_nothing_is_queued_and_the_next_diff_is_prefetched(
    cx: &mut TestAppContext,
) {
    let p = diff_page_full(
        cx,
        seeded(),
        route("src/a.rs"),
        http(),
        gerrit_api::Auth::Anonymous,
        Vec::new(),
        Vec::new(),
        syntax_off(),
    );
    assert!(queued(cx, &p).is_empty());
    assert_eq!(requests(&p.http, "src%2Fb.rs/diff"), 1);
    assert_eq!(requests(&p.http, "src%2Fc.rs/diff"), 0);
    // Turning it on queues the files.
    p.settings.update(cx, |s, cx| {
        s.update_session(|p| p.syntax_highlighting = Some(true), cx)
    });
    cx.run_until_parked();
    assert_eq!(queued(cx, &p), ["src/b.rs", "src/c.rs"]);
    assert_eq!(requests(&p.http, "src%2Fc.rs/diff"), 1);
    // And off again clears the queue.
    p.settings.update(cx, |s, cx| {
        s.update_session(|p| p.syntax_highlighting = Some(false), cx)
    });
    cx.run_until_parked();
    assert!(queued(cx, &p).is_empty());
}

#[gpui_kit::test]
fn the_last_file_that_is_not_skipped_counts_as_the_last(cx: &mut TestAppContext) {
    let p = filtered_page(cx, "src/b.rs", vec![rule("Gen", "c.rs", FilterAction::Dim)]);
    press(cx, p.window, &["]"]);
    assert_eq!(opened(&p), ["up"]);
}

// ---- diffs from the local checkout ----

mod local {
    use super::*;
    use crate::{
        diff_load::DiffSource,
        local_source::tests::{self as lt, Fixture},
        session::ProjectSettings,
    };

    fn store_for(sha: &str) -> Store {
        let mut store = Store::open_in_memory().expect("store");
        let server = store.server_id(URL).expect("server");
        store
            .save_change_detail(server, &lt::detail_at(sha), now())
            .expect("detail");
        store
            .save_file_list(server, &lt::files_key(sha), &lt::paths(), None, now())
            .expect("files");
        store
    }

    fn open_local(
        cx: &mut TestAppContext,
        f: &Fixture,
        store: Store,
        http: RouteHttpClient,
        auth: gerrit_api::Auth,
        path: &str,
    ) -> Page {
        open_local_with(cx, f, store, http, auth, path, LocalDiff::default())
    }

    fn open_local_with(
        cx: &mut TestAppContext,
        f: &Fixture,
        store: Store,
        http: RouteHttpClient,
        auth: gerrit_api::Auth,
        path: &str,
        local: LocalDiff,
    ) -> Page {
        let projects = vec![ProjectSettings::new(
            "platform/foo",
            f.repo.path().to_string_lossy(),
        )];
        let route = DiffRoute {
            target: PatchSetNumber(1),
            ..route(path)
        };
        let p = diff_page_loading_in(
            cx,
            store,
            route,
            http,
            auth,
            Vec::new(),
            Vec::new(),
            local,
            projects,
        );
        cx.run_until_parked();
        // The checkout opens and git runs on real threads.
        for _ in 0..50 {
            if p.page.read_with(cx, |p, _| p.shown_for_test().is_some()) {
                break;
            }
            cx.executor().allow_parking();
            std::thread::sleep(std::time::Duration::from_millis(20));
            cx.run_until_parked();
        }
        p
    }

    fn shown(cx: &TestAppContext, p: &Page) -> Option<DiffSource> {
        p.page.read_with(cx, |p, _| {
            p.view_for_test().and_then(|_| p.source_for_test())
        })
    }

    #[gpui_kit::test]
    fn a_diff_comes_from_the_checkout_when_the_server_cannot_serve_it(cx: &mut TestAppContext) {
        let f = lt::fixture();
        // No /diff route: any request to Gerrit would fail the page.
        let p = open_local(
            cx,
            &f,
            store_for(&f.target),
            RouteHttpClient::new(),
            gerrit_api::Auth::Anonymous,
            "src/a.rs",
        );
        assert_eq!(shown(cx, &p), Some(DiffSource::Local));
        assert_eq!(requests(&p.http, "/diff"), 0, "{:?}", p.http.requested());
        // Nothing local reaches the store, neither the shown file nor the
        // ones highlighted ahead.
        for path in ["src/a.rs", "src/b.rs"] {
            let key = crate::syntax_cache::diff_key(
                &lt::files_key(&f.target),
                path,
                Whitespace::IgnoreNone,
            );
            let server = store::ServerId(1);
            assert!(
                !p.store
                    .lock()
                    .expect("store")
                    .has_diff(server, &key)
                    .expect("has")
            );
        }
    }

    #[gpui_kit::test]
    fn the_next_file_is_not_prefetched_from_gerrit_when_the_checkout_serves_it(
        cx: &mut TestAppContext,
    ) {
        let f = lt::fixture();
        // Without highlighting the syntax worker is idle, so only the
        // prefetch could ask Gerrit for src/b.rs.
        let p = open_local_with(
            cx,
            &f,
            store_for(&f.target),
            http(),
            gerrit_api::Auth::Anonymous,
            "src/a.rs",
            syntax_off(),
        );
        assert_eq!(requests(&p.http, "/diff"), 0, "{:?}", p.http.requested());
    }

    fn unlisted_store_for(sha: &str) -> Store {
        let mut store = Store::open_in_memory().expect("store");
        let server = store.server_id(URL).expect("server");
        store
            .save_change_detail(server, &lt::detail_at(sha), now())
            .expect("detail");
        store
    }

    fn list_of(cx: &TestAppContext, p: &Page) -> Vec<String> {
        p.page.read_with(cx, |p, _| {
            p.file_paths_for_test()
                .into_iter()
                .map(str::to_owned)
                .collect()
        })
    }

    #[gpui_kit::test]
    fn an_uncached_list_comes_from_the_checkout_when_gerrit_has_none(cx: &mut TestAppContext) {
        let f = lt::fixture();
        // No /files and no /diff route: Gerrit fails both.
        let p = open_local(
            cx,
            &f,
            unlisted_store_for(&f.target),
            RouteHttpClient::new(),
            gerrit_api::Auth::Anonymous,
            "src/a.rs",
        );
        assert_eq!(list_of(cx, &p), ["/COMMIT_MSG", "src/a.rs"]);
        p.page.read_with(cx, |p, _| {
            assert_eq!(p.files_local_for_test(), Some(f.repo.path()));
            assert!(!p.is_reviewed_for_test());
        });
        // The list gave the diff its renames, so that is local too.
        assert_eq!(shown(cx, &p), Some(DiffSource::Local));
        assert!(
            !p.store
                .lock()
                .expect("store")
                .load_file_list(store::ServerId(1), &lt::files_key(&f.target))
                .expect("load")
                .is_some()
        );
    }

    #[gpui_kit::test]
    fn offline_with_a_checkout_a_stored_diff_does_not_wait_for_git(cx: &mut TestAppContext) {
        let f = lt::fixture();
        let mut store = unlisted_store_for(&f.target);
        let key = crate::syntax_cache::diff_key(
            &lt::files_key(&f.target),
            "src/a.rs",
            Whitespace::IgnoreNone,
        );
        let cached = crate::diff::tests::fixture("handwritten/diff_rename.response.json");
        store
            .save_diff(store::ServerId(1), &key, &cached, now())
            .expect("save");
        let projects = vec![ProjectSettings::new(
            "platform/foo",
            f.repo.path().to_string_lossy(),
        )];
        let p = diff_page_loading_as(
            cx,
            store,
            DiffRoute {
                target: PatchSetNumber(1),
                ..route("src/a.rs")
            },
            RouteHttpClient::new(),
            None,
            Vec::new(),
            Vec::new(),
            LocalDiff::default(),
            projects,
        );
        // One pass, no waiting for the real threads git runs on.
        cx.run_until_parked();
        assert_eq!(shown(cx, &p), Some(DiffSource::Cache));
    }

    #[gpui_kit::test]
    fn r_does_nothing_while_the_list_is_the_local_one(cx: &mut TestAppContext) {
        let f = lt::fixture();
        let p = open_local(
            cx,
            &f,
            unlisted_store_for(&f.target),
            RouteHttpClient::new(),
            signed_in(),
            "src/a.rs",
        );
        p.page
            .read_with(cx, |p, _| assert!(p.files_local_for_test().is_some()));
        press(cx, p.window, &["r"]);
        let hint = p.page.read_with(cx, |p, _| p.hint_for_test());
        assert!(hint.expect("a hint").0.contains("Gerrit's file list"));
        assert!(!p.http.sent().iter().any(|r| r.method == "PUT"));
    }

    #[gpui_kit::test]
    #[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
    fn gerrits_list_replaces_the_local_one(cx: &mut TestAppContext) {
        let f = lt::fixture();
        let http = RouteHttpClient::new();
        http.route(
            "/files",
            200,
            ")]}'\n{\"/COMMIT_MSG\":{\"size\":1},\"gerrit/only.rs\":{\"size\":1}}",
        );
        let p = open_local(
            cx,
            &f,
            unlisted_store_for(&f.target),
            http,
            gerrit_api::Auth::Anonymous,
            "src/a.rs",
        );
        for _ in 0..20 {
            cx.executor().allow_parking();
            std::thread::sleep(std::time::Duration::from_millis(20));
            cx.run_until_parked();
        }
        // Whichever came first, Gerrit's list is what stays.
        assert_eq!(list_of(cx, &p), ["/COMMIT_MSG", "gerrit/only.rs"]);
        p.page
            .read_with(cx, |p, _| assert_eq!(p.files_local_for_test(), None));
    }

    #[gpui_kit::test]
    fn a_stored_list_is_used_without_asking_git_or_gerrit(cx: &mut TestAppContext) {
        let f = lt::fixture();
        let p = open_local(
            cx,
            &f,
            store_for(&f.target),
            RouteHttpClient::new(),
            gerrit_api::Auth::Anonymous,
            "src/a.rs",
        );
        assert_eq!(list_of(cx, &p), ["/COMMIT_MSG", "src/a.rs", "src/b.rs"]);
        p.page
            .read_with(cx, |p, _| assert_eq!(p.files_local_for_test(), None));
        assert_eq!(requests(&p.http, "/files"), 0, "{:?}", p.http.requested());
    }

    #[gpui_kit::test]
    fn a_commit_the_checkout_lacks_goes_to_the_server(cx: &mut TestAppContext) {
        let f = lt::fixture();
        let missing = "0123456789012345678901234567890123456789";
        let p = open_local(
            cx,
            &f,
            store_for(missing),
            http(),
            gerrit_api::Auth::Anonymous,
            "src/a.rs",
        );
        assert_eq!(shown(cx, &p), Some(DiffSource::Gerrit));
        assert_eq!(requests(&p.http, "src%2Fa.rs/diff"), 1);
    }

    #[gpui_kit::test]
    fn a_cached_diff_is_shown_as_cached_even_with_a_checkout(cx: &mut TestAppContext) {
        let f = lt::fixture();
        let mut store = store_for(&f.target);
        let key = crate::syntax_cache::diff_key(
            &lt::files_key(&f.target),
            "src/a.rs",
            Whitespace::IgnoreNone,
        );
        let cached = crate::diff::tests::fixture("handwritten/diff_rename.response.json");
        store
            .save_diff(store::ServerId(1), &key, &cached, now())
            .expect("save");
        let p = open_local(
            cx,
            &f,
            store,
            RouteHttpClient::new(),
            gerrit_api::Auth::Anonymous,
            "src/a.rs",
        );
        assert_eq!(shown(cx, &p), Some(DiffSource::Cache));
    }

    #[gpui_kit::test]
    fn without_a_configured_checkout_nothing_changes(cx: &mut TestAppContext) {
        let f = lt::fixture();
        let p = diff_page_route(
            cx,
            store_for(&f.target),
            DiffRoute {
                target: PatchSetNumber(1),
                ..route("src/a.rs")
            },
            http(),
            gerrit_api::Auth::Anonymous,
        );
        assert_eq!(shown(cx, &p), Some(DiffSource::Gerrit));
    }
}

// ---- a store miss: the file list comes after the page opens ----

fn unlisted_store() -> Store {
    let mut store = Store::open_in_memory().expect("store");
    let server = store.server_id(URL).expect("server");
    store
        .save_change_detail(server, &detail(7, 3, 3), now())
        .expect("detail");
    store
}

fn gerrit_files() -> Vec<u8> {
    let entries: Vec<_> = PATHS
        .iter()
        .map(|p| format!("\"{p}\":{{\"size\":1}}"))
        .collect();
    format!(")]}}'\n{{{}}}", entries.join(",")).into_bytes()
}

fn http_with_files() -> RouteHttpClient {
    let http = http();
    http.route("/files", 200, gerrit_files());
    http
}

fn miss_page(
    cx: &mut TestAppContext,
    store: Store,
    http: RouteHttpClient,
    auth: Option<gerrit_api::Auth>,
) -> Page {
    diff_page_loading_as(
        cx,
        store,
        route("src/a.rs"),
        http,
        auth,
        Vec::new(),
        Vec::new(),
        syntax_off(),
        Vec::new(),
    )
}

#[gpui_kit::test]
fn after_a_miss_gerrits_list_comes_and_the_diff_is_fetched_once(cx: &mut TestAppContext) {
    let p = miss_page(
        cx,
        unlisted_store(),
        http_with_files(),
        Some(gerrit_api::Auth::Anonymous),
    );
    cx.run_until_parked();
    assert_eq!(
        requests(&p.http, "src%2Fa.rs/diff"),
        1,
        "{:?}",
        p.http.requested()
    );
    cx.update(|cx| {
        let page = p.page.read(cx);
        assert_eq!(page.file_paths_for_test(), PATHS);
        assert_eq!(page.files_local_for_test(), None);
        assert!(page.shown_for_test().is_some());
    });
}

#[gpui_kit::test]
fn a_bracket_pressed_during_a_miss_goes_to_the_right_file(cx: &mut TestAppContext) {
    let p = miss_page(
        cx,
        unlisted_store(),
        http_with_files(),
        Some(gerrit_api::Auth::Anonymous),
    );
    cx.update(|cx| assert!(p.page.read(cx).file_paths_for_test().is_empty()));
    press(cx, p.window, &["]"]);
    assert_eq!(opened(&p), ["open src/b.rs"]);
}

#[gpui_kit::test]
fn the_diff_is_fetched_although_the_files_request_fails(cx: &mut TestAppContext) {
    // No /files route: a 404.
    let p = miss_page(
        cx,
        unlisted_store(),
        http(),
        Some(gerrit_api::Auth::Anonymous),
    );
    cx.run_until_parked();
    assert!(requests(&p.http, "/files") > 0);
    cx.update(|cx| {
        let page = p.page.read(cx);
        assert!(
            page.shown_for_test().is_some(),
            "{:?}",
            page.error_for_test()
        );
        assert!(page.file_paths_for_test().is_empty());
    });
}

#[gpui_kit::test]
fn r_waits_when_the_list_could_not_be_had(cx: &mut TestAppContext) {
    // No /files route: the list request fails, so there is nothing to
    // mark against.
    let p = miss_page(cx, unlisted_store(), http(), Some(signed_in()));
    cx.run_until_parked();
    press(cx, p.window, &["r"]);
    let hint = p.page.read_with(cx, |p, _| p.hint_for_test());
    assert!(hint.expect("a hint").0.contains("Gerrit's file list"));
    assert!(!p.http.sent().iter().any(|r| r.method == "PUT"));
}

use crate::diff_load::DiffSource;

fn with_stored_diff(mut store: Store) -> Store {
    let server = store.server_id(URL).expect("server");
    let key = crate::syntax_cache::diff_key(&files_key(), "src/a.rs", Whitespace::IgnoreNone);
    let cached = crate::diff::tests::fixture("handwritten/diff_rename.response.json");
    store.save_diff(server, &key, &cached, now()).expect("diff");
    store
}

#[gpui_kit::test]
fn offline_a_miss_still_shows_a_stored_diff(cx: &mut TestAppContext) {
    let p = miss_page(cx, with_stored_diff(unlisted_store()), http(), None);
    cx.run_until_parked();
    assert_eq!(
        p.page.read_with(cx, |p, _| p.source_for_test()),
        Some(DiffSource::Cache)
    );
    assert!(p.http.requested().is_empty(), "{:?}", p.http.requested());
}

#[gpui_kit::test]
fn r_waits_for_gerrits_list(cx: &mut TestAppContext) {
    let http = http_with_files();
    http.route(PUT_A, 201, "");
    let p = diff_page_loading_as(
        cx,
        unlisted_store(),
        route("src/a.rs"),
        http,
        Some(signed_in()),
        Vec::new(),
        Vec::new(),
        syntax_off(),
        Vec::new(),
    );
    cx.run_until_parked();
    // The window between the page knowing the comparison and the list
    // arriving cannot be held open with a synchronous fake: put it back.
    p.page.update(cx, |p, _| {
        p.files.loaded = false;
        p.files.settled = false;
    });
    press(cx, p.window, &["r"]);
    let hint = p.page.read_with(cx, |p, _| p.hint_for_test());
    assert!(hint.expect("a hint").0.contains("Gerrit's file list"));
    assert!(!is_reviewed(cx, &p));
    assert!(!p.http.sent().iter().any(|r| r.method == "PUT"));
}

#[gpui_kit::test]
fn a_local_version_of_a_gerrit_change_is_named_in_the_header_and_cannot_be_replied_to(
    cx: &mut TestAppContext,
) {
    use local_git::testutil::{TEST_EMAIL, TestRepo};
    use store::{LocalChangeUpsert, LocalOp, NewLocalPatchSet, ServerId};
    let repo = TestRepo::new();
    repo.write("README", b"one\n");
    let base = repo.commit("Base");
    repo.write("README", b"two\n");
    let sha = repo.commit("Amended locally\n\nChange-Id: I7");
    let mut store = seeded();
    let mut d = detail(7, 3, 3);
    d.summary.project = "p".into();
    store
        .save_change_detail(ServerId(1), &d, now())
        .expect("detail");
    let op = LocalOp::Upsert(LocalChangeUpsert {
        seen: None,
        branch: Some("main".into()),
        change_id: "I7".into(),
        gerrit_number: Some(ChangeNumber(7)),
        subject: "Amended locally".into(),
        author_name: "Test Author".into(),
        author_email: TEST_EMAIL.into(),
        updated: now(),
        state: Some(model::LocalChangeState::Local),
        patch_sets: vec![NewLocalPatchSet {
            sha: sha.clone().into(),
            parent_sha: Some(base.into()),
            seen_at: now(),
        }],
    });
    store
        .reconcile_local(ServerId(1), "p", &[op], now())
        .expect("reconcile");
    let local = store
        .local_patch_sets_by_sha(ServerId(1), &sha.clone().into())
        .expect("versions")[0]
        .number;
    let route = DiffRoute {
        change: ChangeNumber(7),
        base: Some(PatchSetNumber(3)),
        target: local,
        path: "README".into(),
        thread: None,
    };
    let project = crate::session::ProjectSettings::new("p", repo.path().to_string_lossy());
    let p = diff_page_loading_in(
        cx,
        store,
        route,
        http(),
        gerrit_api::Auth::Anonymous,
        Vec::new(),
        Vec::new(),
        LocalDiff::default(),
        vec![project],
    );
    let mut visual = gpui_kit::VisualTestContext::from_window(p.window, cx);
    visual.run_until_parked();
    assert!(visual.debug_bounds("diff-change: Change 7:").is_some());
    assert!(
        visual
            .debug_bounds("diff-pair: PS 3 \u{2192} local 1")
            .is_some()
    );
    // Signed out: no reply, and certainly none to PS 3.
    press(cx, p.window, &["a"]);
    let note = cx.update(|cx| p.page.read(cx).hint_for_test());
    assert_eq!(note.map(|n| n.0).as_deref(), Some("Sign in to reply"));
    assert!(p.events.lock().expect("log").is_empty());
    for r in p.http.requested() {
        assert!(!r.contains(&sha), "{r}");
    }
}

// ---- local changes and local versions ----

/// The diff page of local changes and local versions of a Gerrit change,
/// over a real checkout with the session able to sync: only the guards
/// keep it off the network, and the fake client panics on a synthetic
/// number.
mod local_versions {
    use local_git::testutil::{TEST_EMAIL, TestRepo};
    use store::{LocalChangeUpsert, LocalOp, NewLocalPatchSet, ServerId};

    use super::*;
    use crate::{diff_load::DiffSource, session::ProjectSettings};

    const SERVER: ServerId = ServerId(1);

    /// A checkout: `base`, then `local` (Change-Id I1) changing `README`
    /// and adding `src/new.rs`.
    struct Checkout {
        repo: TestRepo,
        base: String,
        local: String,
    }

    fn checkout() -> Checkout {
        let repo = TestRepo::new();
        repo.write("README", b"one\n");
        let base = repo.commit("Base");
        repo.write("README", b"two\n");
        repo.write("src/new.rs", b"fn main() {}\n");
        let local = repo.commit("Add the new thing\n\nIt is new.\n\nChange-Id: I1");
        Checkout { repo, base, local }
    }

    impl Checkout {
        /// A second version of the change on another parent (a rebase).
        fn rebased(&self) -> (String, String) {
            self.repo
                .git(&["checkout", "-q", "-b", "other", &self.base]);
            self.repo.write("OTHER", b"upstream\n");
            let parent = self.repo.commit("Upstream moved");
            self.repo.write("README", b"two\n");
            self.repo.write("src/new.rs", b"fn main() {}\n");
            let v2 = self
                .repo
                .commit("Add the new thing\n\nIt is new.\n\nChange-Id: I1");
            (parent, v2)
        }
    }

    /// A row of change `change_id` (`gerrit` `None`: a local change) with
    /// the versions `shas`, seen one after the other.
    fn row(change_id: &str, gerrit: Option<ChangeNumber>, shas: &[&str]) -> LocalOp {
        LocalOp::Upsert(LocalChangeUpsert {
            seen: None,
            branch: Some("main".into()),
            change_id: change_id.into(),
            gerrit_number: gerrit,
            subject: "Add the new thing".into(),
            author_name: "Test Author".into(),
            author_email: TEST_EMAIL.into(),
            updated: now(),
            state: Some(model::LocalChangeState::Local),
            patch_sets: shas
                .iter()
                .zip(0..)
                .map(|(sha, i)| NewLocalPatchSet {
                    sha: (*sha).into(),
                    parent_sha: None,
                    seen_at: now() + jiff::SignedDuration::from_secs(i),
                })
                .collect(),
        })
    }

    fn store_with(ops: &[LocalOp], seed: impl FnOnce(&mut Store)) -> Store {
        let mut store = Store::open_in_memory().expect("store");
        assert_eq!(store.server_id(URL).expect("server"), SERVER);
        seed(&mut store);
        store
            .reconcile_local(SERVER, "p", ops, now())
            .expect("reconcile");
        store
    }

    /// The local change I1 and its versions' numbers.
    fn local_change(store: &Store) -> (ChangeNumber, Vec<PatchSetNumber>) {
        let row = store
            .local_changes_by_change_id(SERVER, "I1")
            .expect("rows")
            .remove(0);
        let versions = store.local_patch_sets(SERVER, row.id).expect("versions");
        (row.number, versions.iter().map(|v| v.number).collect())
    }

    /// Change 7 of project `p` with patch sets 1..=3 (commits not in the
    /// checkout).
    fn seed_change_7(store: &mut Store) {
        let mut d = detail(7, 3, 3);
        d.summary.project = "p".into();
        d.summary.change_id = Some("I7".into());
        store.save_change_detail(SERVER, &d, now()).expect("detail");
    }

    fn open_page(
        cx: &mut TestAppContext,
        store: Store,
        route: DiffRoute,
        ck: &Checkout,
        local: LocalDiff,
    ) -> Page {
        let project = ProjectSettings {
            local_changes: true,
            ..ProjectSettings::new("p", ck.repo.path().to_string_lossy())
        };
        let p = diff_page_loading_as(
            cx,
            store,
            route,
            RouteHttpClient::new(),
            Some(signed_in()),
            Vec::new(),
            Vec::new(),
            local,
            vec![project],
        );
        cx.run_until_parked();
        // The checkout opens and git runs on real threads.
        for _ in 0..100 {
            let settled = p.page.read_with(cx, |p, _| {
                p.shown_for_test().is_some() || p.error_for_test().is_some()
            });
            if settled {
                break;
            }
            cx.executor().allow_parking();
            std::thread::sleep(std::time::Duration::from_millis(20));
            cx.run_until_parked();
        }
        cx.update_window(p.window, |_, window, cx| window.render_frame(cx))
            .expect("frame");
        cx.run_until_parked();
        p
    }

    fn source(cx: &TestAppContext, p: &Page) -> Option<DiffSource> {
        p.page.read_with(cx, |p, _| {
            p.view_for_test().and_then(|_| p.source_for_test())
        })
    }

    fn drawn(cx: &mut TestAppContext, p: &Page, selector: &'static str) -> bool {
        let mut visual = gpui_kit::VisualTestContext::from_window(p.window, cx);
        visual.run_until_parked();
        visual.debug_bounds(selector).is_some()
    }

    fn local_route(change: ChangeNumber, target: PatchSetNumber, path: &str) -> DiffRoute {
        DiffRoute {
            change,
            base: None,
            target,
            path: path.into(),
            thread: None,
        }
    }

    #[gpui_kit::test]
    fn a_local_change_is_diffed_in_the_checkout_without_a_request(cx: &mut TestAppContext) {
        let ck = checkout();
        let store = store_with(&[row("I1", None, &[&ck.local])], |_| {});
        let (number, versions) = local_change(&store);
        let p = open_page(
            cx,
            store,
            local_route(number, versions[0], "README"),
            &ck,
            LocalDiff::default(),
        );
        assert_eq!(source(cx, &p), Some(DiffSource::Local));
        p.page.read_with(cx, |p, _| {
            assert_eq!(
                p.file_paths_for_test(),
                ["/COMMIT_MSG", "README", "src/new.rs"]
            );
            assert_eq!(p.files_local_for_test(), Some(ck.repo.path()));
        });
        assert!(drawn(cx, &p, "diff-change: Local change:"));
        assert!(drawn(cx, &p, "diff-pair: Base \u{2192} local 1"));

        // `]` and `[` step through the checkout's list.
        press(cx, p.window, &["]", "["]);
        assert_eq!(opened(&p), ["open src/new.rs", "open /COMMIT_MSG"]);

        // No reviewed marks: Gerrit keeps them, and has no local version.
        press(cx, p.window, &["r"]);
        let hint = p.page.read_with(cx, |p, _| p.hint_for_test());
        assert_eq!(hint.map(|h| h.0).as_deref(), Some(crate::reviewed::LOCAL));
        p.page
            .read_with(cx, |p, _| assert!(!p.is_reviewed_for_test()));
        // `a` asks for the reply dialog: a local review of this version.
        press(cx, p.window, &["a"]);
        let asked = format!("reply to {}", versions[0].0);
        assert!(p.events.lock().expect("log").contains(&asked));
        assert!(p.http.requested().is_empty(), "{:?}", p.http.requested());
    }

    #[gpui_kit::test]
    fn the_commit_message_of_a_local_change_comes_from_the_checkout(cx: &mut TestAppContext) {
        let ck = checkout();
        let store = store_with(&[row("I1", None, &[&ck.local])], |_| {});
        let (number, versions) = local_change(&store);
        let p = open_page(
            cx,
            store,
            local_route(number, versions[0], "/COMMIT_MSG"),
            &ck,
            LocalDiff::default(),
        );
        assert_eq!(source(cx, &p), Some(DiffSource::Local));
        let text = cx.update(|cx| {
            let view = p.page.read(cx).view_for_test().expect("a view");
            format!("{:?}", view.read(cx).diff_for_test().chunks)
        });
        assert!(text.contains("It is new."), "{text}");
        assert!(p.http.requested().is_empty(), "{:?}", p.http.requested());
    }

    #[gpui_kit::test]
    fn the_files_ahead_are_highlighted_from_the_checkout_never_from_gerrit(
        cx: &mut TestAppContext,
    ) {
        let ck = checkout();
        let store = store_with(&[row("I1", None, &[&ck.local])], |_| {});
        let (number, versions) = local_change(&store);
        let p = open_page(
            cx,
            store,
            local_route(number, versions[0], "README"),
            &ck,
            LocalDiff::default(),
        );
        // The syntax worker has the next file, a local key.
        let queue = cx.update(|cx| p.syntax.read(cx).last_queue_for_test().to_vec());
        assert_eq!(
            queue.iter().map(|k| k.path.as_str()).collect::<Vec<_>>(),
            ["src/new.rs"]
        );
        assert!(queue.iter().all(DiffKey::is_local));
        // It gets the diff from the checkout and highlights it.
        let mut done = false;
        for _ in 0..100 {
            done = p.syntax.update(cx, |c, _| c.get(&queue[0]).is_some());
            if done {
                break;
            }
            cx.executor().allow_parking();
            std::thread::sleep(std::time::Duration::from_millis(20));
            cx.run_until_parked();
        }
        assert!(done, "the next file was not highlighted");
        assert!(p.http.requested().is_empty(), "{:?}", p.http.requested());

        // With highlighting off the next file would be prefetched, but
        // Gerrit has nothing to give for a local version.
        let ck = checkout();
        let store = store_with(&[row("I1", None, &[&ck.local])], |_| {});
        let (number, versions) = local_change(&store);
        let p = open_page(
            cx,
            store,
            local_route(number, versions[0], "README"),
            &ck,
            syntax_off(),
        );
        assert_eq!(source(cx, &p), Some(DiffSource::Local));
        press(cx, p.window, &["]"]);
        assert!(p.http.requested().is_empty(), "{:?}", p.http.requested());
    }

    #[gpui_kit::test]
    fn a_local_version_of_a_gerrit_change_is_diffed_in_the_checkout(cx: &mut TestAppContext) {
        let ck = checkout();
        let store = store_with(
            &[row("I7", Some(ChangeNumber(7)), &[&ck.local])],
            seed_change_7,
        );
        let target = store
            .local_patch_sets_by_sha(SERVER, &ck.local.clone().into())
            .expect("versions")[0]
            .number;
        let p = open_page(
            cx,
            store,
            local_route(ChangeNumber(7), target, "src/new.rs"),
            &ck,
            LocalDiff::default(),
        );
        assert_eq!(source(cx, &p), Some(DiffSource::Local));
        assert!(drawn(cx, &p, "diff-change: Change 7:"));
        assert!(drawn(cx, &p, "diff-pair: Base \u{2192} local 1"));
        press(cx, p.window, &["r", "["]);
        assert_eq!(opened(&p), ["open README"]);
        // The change's comments may be asked for; nothing about the local
        // version is.
        for r in p.http.requested() {
            assert!(!r.contains(&ck.local), "{r}");
            assert!(!r.contains("/diff") && !r.contains("/files"), "{r}");
        }
        assert!(!p.http.sent().iter().any(|r| r.method == "PUT"));
    }

    #[gpui_kit::test]
    fn local_versions_on_different_parents_say_why_they_cannot_be_compared(
        cx: &mut TestAppContext,
    ) {
        let ck = checkout();
        let (_, v2) = ck.rebased();
        let store = store_with(&[row("I1", None, &[&ck.local, &v2])], |_| {});
        let (number, versions) = local_change(&store);
        let route = DiffRoute {
            base: Some(versions[0]),
            ..local_route(number, versions[1], "README")
        };
        let p = open_page(cx, store, route, &ck, LocalDiff::default());
        assert!(drawn(cx, &p, "diff-parents-differ"));
        assert!(drawn(cx, &p, "diff-pair: local 1 \u{2192} local 2"));
        p.page
            .read_with(cx, |p, _| assert!(p.view_for_test().is_none()));
        // The way out: the same file against the parent.
        p.page.update(cx, |p, cx| p.compare_with_parent(cx));
        assert_eq!(opened(&p), ["compare None README"]);
        assert!(p.http.requested().is_empty(), "{:?}", p.http.requested());
    }
}
