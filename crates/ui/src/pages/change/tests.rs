//! The change page on GPUI's `TestAppContext`: cache first, the file
//! cursor, the patch set pair and inline diffs.

use std::sync::{Arc, Mutex};

use gerrit_api::{Auth, fake::RouteHttpClient};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
use model::{ChangeNumber, FileStatus, FilesKey, PatchSetNumber, PatchSetRef};
use store::{ServerId, Store};

use super::{ChangeEvent, ChangePage, state::Block};
use crate::{
    LocalDiff,
    filters::{FileFilter, FilterAction},
    nav::ChangeView,
    outbox::DraftOutbox,
    session::Session,
    settings::{DiffSettings, FileFilters, PageLayout},
    syntax_cache::SyntaxCache,
    testing::{detail, file, whole_file},
    tests::{URL, env_with, open, setup},
};

const PATHS: [&str; 4] = ["/COMMIT_MSG", "src/a.rs", "src/b.rs", "src/c.rs"];

/// When the seeded rows were fetched: recent, because a diff fetch prunes
/// old cache rows (the syntax worker fetches diffs on its own).
fn ts() -> jiff::Timestamp {
    jiff::Timestamp::now()
}

fn ps(n: u32) -> PatchSetRef {
    PatchSetRef {
        number: PatchSetNumber(n),
        sha: format!("sha{n}").into(),
    }
}

fn key(base: Option<u32>, target: u32) -> FilesKey {
    FilesKey {
        change: ChangeNumber(7),
        base: base.map(ps),
        target: ps(target),
    }
}

fn files(paths: &[&str]) -> Vec<model::FileEntry> {
    paths
        .iter()
        .map(|p| file(p, FileStatus::Modified))
        .collect()
}

/// Change 7 with three patch sets, files cached for "Base vs PS3" and
/// "PS1 vs PS3" (one file fewer), and a related chain.
fn seeded() -> Store {
    let mut store = Store::open_in_memory().expect("store");
    let server = store.server_id(URL).expect("server");
    assert_eq!(server, ServerId(1));
    store
        .save_change_detail(server, &detail(7, 3, 3), ts())
        .expect("detail");
    store
        .save_file_list(
            server,
            &key(None, 3),
            &files(&PATHS),
            Some(&["src/a.rs".to_owned()]),
            ts(),
        )
        .expect("files");
    store
        .save_file_list(server, &key(Some(1), 3), &files(&PATHS[..3]), None, ts())
        .expect("files");
    store
}

struct Fixture {
    window: AnyWindowHandle,
    page: Entity<ChangePage>,
    filters: Entity<FileFilters>,
    syntax: Entity<SyntaxCache>,
    settings: Entity<DiffSettings>,
    http: RouteHttpClient,
    events: Arc<Mutex<Vec<String>>>,
}

fn diff_body() -> Vec<u8> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/handwritten/diff_rename.response.json");
    std::fs::read(path).expect("fixture")
}

fn page(cx: &mut TestAppContext, online: bool, view: ChangeView) -> Fixture {
    page_with(cx, online, view, |_| {})
}

/// Like [`page`]; `routes` can add to or override the fake server's routes.
fn page_with(
    cx: &mut TestAppContext,
    online: bool,
    view: ChangeView,
    routes: impl FnOnce(&RouteHttpClient),
) -> Fixture {
    let auth = online.then_some(Auth::Anonymous);
    page_as(cx, auth, setup(), view, routes)
}

/// Like [`page_with`], signed in (so that comments can be written).
fn page_signed_in(
    cx: &mut TestAppContext,
    view: ChangeView,
    routes: impl FnOnce(&RouteHttpClient),
) -> Fixture {
    let mut server = setup();
    server.username = Some("u".into());
    let auth = Auth::Basic {
        username: "u".into(),
        password: "p".into(),
    };
    page_as(cx, Some(auth), server, view, routes)
}

fn page_as(
    cx: &mut TestAppContext,
    auth: Option<Auth>,
    server: crate::session::ServerSetup,
    view: ChangeView,
    routes: impl FnOnce(&RouteHttpClient),
) -> Fixture {
    page_in(cx, seeded(), auth, server, view, routes)
}

/// Like [`page_as`], on the given cache.
fn page_in(
    cx: &mut TestAppContext,
    store: Store,
    auth: Option<Auth>,
    server: crate::session::ServerSetup,
    view: ChangeView,
    routes: impl FnOnce(&RouteHttpClient),
) -> Fixture {
    let http = RouteHttpClient::new();
    http.route("/diff", 200, diff_body());
    routes(&http);
    let env = env_with(store, &http);
    let session = Session::new(&env, &server, auth).expect("session");
    let events = Arc::new(Mutex::new(Vec::new()));
    let log = events.clone();
    let mut filters_out = None;
    let mut syntax_out = None;
    let mut settings_out = None;
    let (window, page) = open(cx, |window, cx| {
        let settings = cx.new(|_| DiffSettings::new(LocalDiff::default()));
        settings_out = Some(settings.clone());
        let outbox = cx.new(|_| DraftOutbox::new(session.clone()));
        let layout = cx.new(|_| PageLayout::default());
        let filters = cx.new(|_| FileFilters::new(Vec::new()));
        filters_out = Some(filters.clone());
        let syntax = cx.new(|_| SyntaxCache::new());
        syntax_out = Some(syntax.clone());
        let page = cx.new(|cx| {
            ChangePage::new(
                session,
                settings,
                layout,
                filters,
                syntax,
                ChangeNumber(7),
                view,
                true,
                outbox,
                cx,
            )
        });
        cx.subscribe(&page, move |_, ev: &ChangeEvent, _| {
            let text = match ev {
                ChangeEvent::Unauthorized => "unauthorized".to_owned(),
                // Not what these tests are about (the shell retries drafts).
                ChangeEvent::Refreshed => return,
                ChangeEvent::Reply(_) => "reply".to_owned(),
                ChangeEvent::Actions(_) => "actions".to_owned(),
                ChangeEvent::SubmitChain(_) => "submit chain".to_owned(),
                ChangeEvent::ManageFilters { project } => format!("manage filters {project}"),
                ChangeEvent::OpenChange(n) => format!("change {n}"),
                ChangeEvent::ReplaceChange(n) => format!("replace {n}"),
                ChangeEvent::Advance(n, note) => format!("advance {n}: {note}"),
                ChangeEvent::OpenDiff(d) => format!(
                    "diff {} {:?}..{} {}{}",
                    d.change,
                    d.base.map(|b| b.0),
                    d.target,
                    d.path,
                    d.thread
                        .as_ref()
                        .map_or_else(String::new, |t| format!(" thread={t:?}"))
                ),
            };
            log.lock().expect("log").push(text);
        })
        .detach();
        let focus = page.read(cx).focus_handle_for_test();
        window.focus(&focus, cx);
        page
    });
    cx.run_until_parked();
    Fixture {
        window,
        page,
        filters: filters_out.expect("filters were created"),
        syntax: syntax_out.expect("syntax cache was created"),
        settings: settings_out.expect("settings were created"),
        http,
        events,
    }
}

fn press(cx: &mut TestAppContext, f: &Fixture, keys: &[&'static str]) {
    for k in keys {
        cx.update_window(f.window, |_, window, cx| window.press(k, cx))
            .expect("press");
        cx.run_until_parked();
    }
}

fn cursor(cx: &mut TestAppContext, f: &Fixture) -> Option<String> {
    cx.update(|cx| f.page.read(cx).cursor_path().map(str::to_owned))
}

#[gpui_kit::test]
fn renders_from_the_cache_without_asking_the_server(cx: &mut TestAppContext) {
    let f = page(cx, false, ChangeView::default());
    cx.update(|cx| {
        let p = f.page.read(cx);
        assert_eq!(p.detail().map(|d| d.summary.number), Some(ChangeNumber(7)));
        assert_eq!(p.file_paths(), PATHS);
        assert_eq!(p.files_key(), Some(&key(None, 3)));
        assert!(p.blocks_for_test().contains(&Block::File(3)));
    });
    assert!(f.http.requested().is_empty());
}

#[gpui_kit::test]
fn a_session_that_can_sync_refreshes_the_detail_files_and_related(cx: &mut TestAppContext) {
    // Nothing is routed: every refresh fails, the cached view stays.
    let f = page(cx, true, ChangeView::default());
    let seen = f.http.requested();
    assert!(
        seen.iter().any(|r| r.starts_with("/changes/7?")),
        "{seen:?}"
    );
    cx.update(|cx| assert_eq!(f.page.read(cx).file_paths(), PATHS));
}

#[gpui_kit::test]
fn j_and_k_move_the_file_cursor_and_stop_at_the_ends(cx: &mut TestAppContext) {
    let f = page(cx, false, ChangeView::default());
    assert_eq!(cursor(cx, &f).as_deref(), Some("/COMMIT_MSG"));
    press(cx, &f, &["k"]);
    assert_eq!(cursor(cx, &f).as_deref(), Some("/COMMIT_MSG"));
    press(cx, &f, &["j", "j"]);
    assert_eq!(cursor(cx, &f).as_deref(), Some("src/b.rs"));
    press(cx, &f, &["j", "j", "j", "j"]);
    assert_eq!(cursor(cx, &f).as_deref(), Some("src/c.rs"));
    press(cx, &f, &["k"]);
    assert_eq!(cursor(cx, &f).as_deref(), Some("src/b.rs"));
}

#[gpui_kit::test]
fn brackets_move_the_cursor_like_j_and_k(cx: &mut TestAppContext) {
    let f = page(cx, false, ChangeView::default());
    press(cx, &f, &["]"]);
    assert_eq!(cursor(cx, &f).as_deref(), Some("src/a.rs"));
    press(cx, &f, &["["]);
    assert_eq!(cursor(cx, &f).as_deref(), Some("/COMMIT_MSG"));
    press(cx, &f, &["["]);
    assert_eq!(cursor(cx, &f).as_deref(), Some("/COMMIT_MSG"));
}

#[gpui_kit::test]
fn the_remembered_view_restores_the_cursor_file(cx: &mut TestAppContext) {
    let f = page(
        cx,
        false,
        ChangeView {
            base: None,
            target: None,
            file: Some("src/b.rs".into()),
        },
    );
    assert_eq!(cursor(cx, &f).as_deref(), Some("src/b.rs"));
    cx.update(|cx| {
        let v = f.page.read(cx).view_state();
        assert_eq!(v.file.as_deref(), Some("src/b.rs"));
    });
}

#[gpui_kit::test]
fn enter_asks_to_open_the_diff_of_the_file_under_the_cursor(cx: &mut TestAppContext) {
    let f = page(cx, false, ChangeView::default());
    press(cx, &f, &["j", "j", "enter"]);
    let events = f.events.lock().expect("log").clone();
    assert_eq!(events, ["diff 7 None..3 src/b.rs".to_owned()]);
}

#[gpui_kit::test]
fn picking_another_base_loads_the_file_list_of_that_pair(cx: &mut TestAppContext) {
    let f = page(cx, false, ChangeView::default());
    press(cx, &f, &["j", "j"]);
    press(cx, &f, &["b"]); // Base -> PS1
    cx.update(|cx| {
        let p = f.page.read(cx);
        assert_eq!(p.files_key(), Some(&key(Some(1), 3)));
        assert_eq!(p.file_paths(), &PATHS[..3]);
        assert_eq!(
            p.selection_for_test(),
            (Some(PatchSetNumber(1)), Some(PatchSetNumber(3)))
        );
        // The cursor stays on the same file where the new list has it.
        assert_eq!(p.cursor_path(), Some("src/b.rs"));
    });
    // PS2 has no cached list: the page shows none instead of stale rows.
    press(cx, &f, &["b"]);
    cx.update(|cx| {
        let p = f.page.read(cx);
        assert_eq!(p.files_key(), Some(&key(Some(2), 3)));
        assert!(p.file_paths().is_empty());
    });
    press(cx, &f, &["b", "b"]); // PS2 -> Base (wraps) -> PS1
    press(cx, &f, &["shift-b"]);
    cx.update(|cx| assert_eq!(f.page.read(cx).files_key(), Some(&key(None, 3))));
}

#[gpui_kit::test]
fn picking_an_older_target_resets_a_base_that_is_not_older(cx: &mut TestAppContext) {
    let f = page(cx, false, ChangeView::default());
    press(cx, &f, &["b", "b"]); // base PS2
    press(cx, &f, &["shift-t"]); // target PS2
    cx.update(|cx| {
        let p = f.page.read(cx);
        assert_eq!(p.selection_for_test(), (None, Some(PatchSetNumber(2))));
        assert_eq!(p.files_key(), Some(&key(None, 2)));
    });
}

#[gpui_kit::test]
fn i_toggles_the_inline_diff_of_the_current_file(cx: &mut TestAppContext) {
    let f = page(cx, true, ChangeView::default());
    press(cx, &f, &["j", "i"]);
    cx.update(|cx| {
        let p = f.page.read(cx);
        assert_eq!(p.expanded_paths(), ["src/a.rs"]);
        assert!(p.inline_ready("src/a.rs"));
    });
    assert!(
        f.http
            .requested()
            .iter()
            .any(|r| r.contains("src%2Fa.rs/diff")),
        "{:?}",
        f.http.requested()
    );
    // j still moves the file cursor: the diff is display-only.
    press(cx, &f, &["j"]);
    assert_eq!(cursor(cx, &f).as_deref(), Some("src/b.rs"));
    press(cx, &f, &["k", "i"]);
    cx.update(|cx| assert!(f.page.read(cx).expanded_paths().is_empty()));
}

#[gpui_kit::test]
fn shift_i_expands_every_file_and_again_collapses_them(cx: &mut TestAppContext) {
    let f = page(cx, true, ChangeView::default());
    press(cx, &f, &["shift-i"]);
    cx.update(|cx| {
        let p = f.page.read(cx);
        assert_eq!(p.expanded_paths().len(), PATHS.len());
        assert!(PATHS.iter().all(|path| p.inline_ready(path)));
    });
    press(cx, &f, &["shift-i"]);
    cx.update(|cx| assert!(f.page.read(cx).expanded_paths().is_empty()));
    // Expanding one file first and then all fills in the rest.
    press(cx, &f, &["i", "shift-i"]);
    cx.update(|cx| assert_eq!(f.page.read(cx).expanded_paths().len(), PATHS.len()));
}

fn inline_folds(cx: &mut TestAppContext, f: &Fixture) -> usize {
    cx.update(|cx| {
        let view = f.page.read(cx).inline_view("/COMMIT_MSG").expect("view");
        view.read(cx)
            .layout()
            .rows()
            .iter()
            .filter(|r| matches!(r, crate::diff::layout::Row::Fold(_)))
            .count()
    })
}

#[gpui_kit::test]
fn an_inline_diff_is_folded_and_shift_x_opens_it_without_a_request(cx: &mut TestAppContext) {
    let f = page_with(cx, true, ChangeView::default(), |http| {
        http.route("%2FCOMMIT_MSG/diff", 200, whole_file());
    });
    press(cx, &f, &["i"]);
    assert_eq!(inline_folds(cx, &f), 2);
    // No context is requested: the server would ignore it.
    assert!(f.http.requested().iter().all(|r| !r.contains("context")));
    // Focus the inline view (as a click would) and open the folds.
    cx.update_window(f.window, |_, window, cx| {
        let view = f.page.read(cx).inline_view("/COMMIT_MSG").expect("view");
        let focus = view.read(cx).focus_for_test();
        window.focus(&focus, cx);
    })
    .expect("focus");
    let n = f.http.requested().len();
    press(cx, &f, &["shift-x"]);
    assert_eq!(inline_folds(cx, &f), 0);
    assert_eq!(f.http.requested().len(), n);
    cx.update(|cx| {
        let view = f.page.read(cx).inline_view("/COMMIT_MSG").expect("view");
        assert_eq!(view.read(cx).status(), None);
    });
}

/// The real key path: the shell around a change page with many files. An
/// inline diff that has the focus (after a click) is scrolled out of the
/// virtualized list, so it is not in the frame. GPUI then dispatches keys from
/// the window root, where no binding matches: Esc, j and every other key were
/// dead until the user clicked something. The shell must give the focus back
/// to the page when the focused element vanishes.
#[gpui_kit::test]
fn esc_works_after_the_focused_inline_diff_scrolled_out_of_the_list(cx: &mut TestAppContext) {
    let (window, shell, page) = shell_with_many_files(cx);
    focus_inline(cx, window, &page);
    // Scroll far away, like the mouse wheel does.
    cx.update(|cx| page.update(cx, |p, _| p.scroll_to_end_for_test()));
    cx.run_until_parked();
    keys(cx, window, &["escape"]);
    cx.update(|cx| assert_eq!(shell.read(cx).route(), &crate::nav::Route::Dashboard));
}

/// Esc closes the help overlay in one press, also when an inline diff has
/// the focus,
/// and the page keys do nothing while it is open.
#[gpui_kit::test]
fn esc_closes_the_help_overlay_from_an_inline_diff(cx: &mut TestAppContext) {
    let (window, shell, page) = shell_with_many_files(cx);
    focus_inline(cx, window, &page);
    keys(cx, window, &["?"]);
    cx.update(|cx| assert!(shell.read(cx).help_open_for_test()));
    keys(cx, window, &["j"]);
    cx.update(|cx| assert_eq!(page.read(cx).cursor_path(), Some("/COMMIT_MSG")));
    keys(cx, window, &["escape"]);
    cx.update(|cx| assert!(!shell.read(cx).help_open_for_test()));
    // The next Esc leaves the page, whatever has the focus.
    keys(cx, window, &["escape"]);
    cx.update(|cx| {
        assert_ne!(
            shell.read(cx).route(),
            &crate::nav::Route::Change(model::ChangeNumber(7))
        )
    });
}

fn keys(cx: &mut TestAppContext, window: AnyWindowHandle, keys: &[&'static str]) {
    for k in keys {
        cx.update_window(window, |_, window, cx| window.press(k, cx))
            .expect("press");
        cx.run_until_parked();
    }
}

/// Opens the first change (120 files) in a shell and expands the inline diff
/// of `/COMMIT_MSG`.
fn shell_with_many_files(
    cx: &mut TestAppContext,
) -> (
    AnyWindowHandle,
    Entity<crate::shell::Shell>,
    Entity<ChangePage>,
) {
    let paths: Vec<String> = std::iter::once("/COMMIT_MSG".to_owned())
        .chain((0..120).map(|i| format!("src/f{i:03}.rs")))
        .collect();
    let mut store = seeded();
    let server = store.server_id(URL).expect("server");
    let list: Vec<_> = paths
        .iter()
        .map(|p| file(p, FileStatus::Modified))
        .collect();
    store
        .save_file_list(server, &key(None, 3), &list, None, ts())
        .expect("files");
    store
        .save_query_page(
            server,
            "status:open",
            &[crate::testing::summary(7)],
            false,
            ts(),
        )
        .expect("query");
    let http = RouteHttpClient::new();
    http.route("/diff", 200, diff_body());
    let startup = crate::session::Startup {
        env: env_with(store, &http),
        diff: LocalDiff::default(),
        start_page: Default::default(),
        filters: Vec::new(),
        servers: vec![crate::session::ServerSetup {
            queries: vec![crate::session::SavedQuery {
                name: "Open".into(),
                query: "status:open".into(),
            }],
            bookmarks: Vec::new(),
            ..setup()
        }],
        active: None,
    };
    let (window, shell) = open(cx, |window, cx| {
        cx.new(|cx| crate::shell::Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    keys(cx, window, &["j", "enter", "i"]);
    let page = cx.update(|cx| shell.read(cx).change_page_for_test().expect("change page"));
    (window, shell, page)
}

/// Gives the inline diff of `/COMMIT_MSG` the focus, as a click does.
fn focus_inline(cx: &mut TestAppContext, window: AnyWindowHandle, page: &Entity<ChangePage>) {
    cx.update_window(window, |_, window, cx| {
        let view = page.read(cx).inline_view("/COMMIT_MSG").expect("view");
        let focus = view.read(cx).focus_for_test();
        window.focus(&focus, cx);
    })
    .expect("focus");
    cx.run_until_parked();
}

#[gpui_kit::test]
fn inline_diffs_show_the_threads_of_their_file(cx: &mut TestAppContext) {
    let comments = r#"{"/COMMIT_MSG":[{"id":"c1","patch_set":3,"line":1,"message":"nit","updated":"2024-06-15 10:00:00.000000000","unresolved":true},{"id":"c2","patch_set":2,"line":1,"message":"older patch set","updated":"2024-06-15 10:00:00.000000000"}]}"#;
    let f = page_with(cx, true, ChangeView::default(), |http| {
        http.route_json("/comments", comments);
    });
    press(cx, &f, &["i"]);
    cx.update(|cx| {
        let view = f.page.read(cx).inline_view("/COMMIT_MSG").expect("view");
        let shown: Vec<_> = view
            .read(cx)
            .shown_threads()
            .map(|t| t.id.clone())
            .collect();
        assert_eq!(shown, [model::ThreadId::Comment("c1".into())]);
    });
}

// --- comments: counts, thread list, patch-set level threads ----------------

const COMMENTS: &str = r#"{
  "src/a.rs": [
    {"id":"a1","patch_set":3,"line":5,"message":"fix this\nplease","updated":"2024-06-15 10:00:00.000000000","unresolved":true,"author":{"_account_id":5,"name":"Rita"}},
    {"id":"a2","patch_set":3,"line":9,"message":"fine","updated":"2024-06-15 10:01:00.000000000","unresolved":false}
  ],
  "src/b.rs": [
    {"id":"b1","patch_set":1,"line":2,"message":"old","updated":"2024-06-15 10:00:00.000000000","unresolved":true}
  ],
  "/PATCHSET_LEVEL": [
    {"id":"p1","patch_set":3,"message":"overall","updated":"2024-06-15 10:02:00.000000000","unresolved":true}
  ]
}"#;

fn with_comments(cx: &mut TestAppContext) -> Fixture {
    page_with(cx, true, ChangeView::default(), |http| {
        http.route_json("/comments", COMMENTS);
    })
}

fn listed(cx: &mut TestAppContext, f: &Fixture) -> Vec<String> {
    let mut ids = cx.update(|cx| f.page.read(cx).listed_for_test());
    ids.sort();
    ids
}

#[gpui_kit::test]
fn the_file_list_counts_the_comments_of_the_selected_pair(cx: &mut TestAppContext) {
    let f = with_comments(cx);
    cx.update(|cx| {
        let p = f.page.read(cx);
        let a = p.counts_for_test("src/a.rs").expect("counts");
        assert_eq!((a.comments, a.unresolved, a.drafts), (2, 1, 0));
        // b.rs has a comment on PS1 only: not in Base..PS3.
        let b = p.counts_for_test("src/b.rs").expect("counts");
        assert_eq!((b.comments, b.elsewhere), (0, 1));
        assert_eq!(p.counts_for_test("src/c.rs"), None);
    });
    // Another pair counts again: PS1..PS3 shows PS1's comment on the left.
    press(cx, &f, &["b"]);
    cx.update(|cx| {
        let b = f.page.read(cx).counts_for_test("src/b.rs").expect("counts");
        assert_eq!((b.comments, b.unresolved, b.elsewhere), (1, 1, 0));
    });
}

#[gpui_kit::test]
fn the_thread_list_defaults_to_unresolved_and_the_filter_keys_change_it(cx: &mut TestAppContext) {
    let f = with_comments(cx);
    assert_eq!(listed(cx, &f), ["a1", "b1", "p1"]);
    press(cx, &f, &["shift-a"]);
    assert_eq!(listed(cx, &f), ["a1", "a2", "b1", "p1"]);
    press(cx, &f, &["shift-d"]);
    assert!(listed(cx, &f).is_empty());
    press(cx, &f, &["shift-u"]);
    assert_eq!(listed(cx, &f), ["a1", "b1", "p1"]);
    // The blocks follow: a heading, the rows, the patch-set comments.
    cx.update(|cx| {
        let b = f.page.read(cx).blocks_for_test();
        let at = b
            .iter()
            .position(|b| *b == Block::ThreadsHeader)
            .expect("header");
        assert_eq!(
            b[at..at + 5],
            [
                Block::ThreadsHeader,
                Block::Thread(0),
                Block::Thread(1),
                Block::Thread(2),
                Block::PatchSetComments
            ]
        );
    });
}

#[gpui_kit::test]
fn without_unresolved_threads_the_list_shows_all(cx: &mut TestAppContext) {
    let resolved = r#"{"src/a.rs":[{"id":"a2","patch_set":3,"line":9,"message":"fine","updated":"2024-06-15 10:01:00.000000000","unresolved":false}]}"#;
    let f = page_with(cx, true, ChangeView::default(), |http| {
        http.route_json("/comments", resolved);
    });
    assert_eq!(listed(cx, &f), ["a2"]);
}

fn thread_cursor(cx: &mut TestAppContext, f: &Fixture) -> Option<String> {
    cx.update(|cx| match f.page.read(cx).thread_cursor_for_test() {
        Some(model::ThreadId::Comment(id)) => Some(id),
        _ => None,
    })
}

/// Moves the thread cursor to the thread `id` with shift-n.
fn to_thread(cx: &mut TestAppContext, f: &Fixture, id: &str) {
    for _ in 0..6 {
        if thread_cursor(cx, f).as_deref() == Some(id) {
            return;
        }
        press(cx, f, &["shift-n"]);
    }
    panic!("no thread {id}");
}

#[gpui_kit::test]
fn x_opens_the_diff_of_the_thread_with_a_pair_that_shows_it(cx: &mut TestAppContext) {
    let f = with_comments(cx);
    // shift-p from nowhere starts at the end, shift-n at the start.
    press(cx, &f, &["shift-n"]);
    assert!(thread_cursor(cx, &f).is_some());
    to_thread(cx, &f, "a1");
    press(cx, &f, &["x"]);
    // Shown in the current pair (Base..PS3): that pair, with the thread.
    to_thread(cx, &f, "b1");
    press(cx, &f, &["x"]);
    let events = f.events.lock().expect("log").clone();
    assert_eq!(
        events,
        [
            "diff 7 None..3 src/a.rs thread=Comment(\"a1\")".to_owned(),
            // PS1's comment is not in Base..PS3: its own patch set.
            "diff 7 None..1 src/b.rs thread=Comment(\"b1\")".to_owned(),
        ]
    );
    // Enter still opens the file under the file cursor.
    press(cx, &f, &["enter"]);
    assert_eq!(
        f.events.lock().expect("log").last().map(String::as_str),
        Some("diff 7 None..3 /COMMIT_MSG")
    );
}

#[gpui_kit::test]
fn a_patchset_level_thread_opens_as_a_card_on_the_page(cx: &mut TestAppContext) {
    let f = with_comments(cx);
    to_thread(cx, &f, "p1");
    press(cx, &f, &["x"]);
    assert!(f.events.lock().expect("log").is_empty(), "no diff page");
    cx.update_window(f.window, |_, window, cx| {
        let view = f.page.read(cx).ps_view_for_test().expect("view");
        assert!(view.read(cx).focus_for_test().is_focused(window));
        assert_eq!(
            view.read(cx).cursor_thread(),
            Some(model::ThreadId::Comment("p1".into()))
        );
    })
    .expect("window");
}

fn type_text(cx: &mut TestAppContext, f: &Fixture, text: &str) {
    cx.update_window(f.window, |_, window, cx| window.input(text, cx))
        .expect("input");
}

#[gpui_kit::test]
fn c_writes_a_patchset_level_draft_on_the_current_patch_set(cx: &mut TestAppContext) {
    let f = page_signed_in(cx, ChangeView::default(), |http| {
        http.route_json("/comments", COMMENTS);
        http.route(
            "/revisions/sha3/drafts",
            201,
            ")]}'\n{\"id\":\"srv1\",\"patch_set\":3,\"path\":\"/PATCHSET_LEVEL\",\"updated\":\"2024-06-15 10:05:00.000000000\",\"message\":\"looks good\"}",
        );
    });
    // A target other than the current one does not matter.
    press(cx, &f, &["shift-t"]);
    press(cx, &f, &["c"]);
    type_text(cx, &f, "  looks good  ");
    let key = if cfg!(target_os = "macos") {
        "cmd-enter"
    } else {
        "ctrl-enter"
    };
    press(cx, &f, &[key]);
    cx.run_until_parked();
    let drafts: Vec<_> = cx.update(|cx| {
        f.page
            .read(cx)
            .threads_for_test()
            .iter()
            .flat_map(|t| t.entries.iter())
            .filter_map(|e| match e {
                model::Entry::Draft(d) => Some((d.path.clone(), d.patch_set.0, d.message.clone())),
                _ => None,
            })
            .collect()
    });
    assert_eq!(
        drafts,
        [("/PATCHSET_LEVEL".to_owned(), 3, "looks good".to_owned())]
    );
    assert!(
        f.http
            .requested()
            .iter()
            .any(|r| r.contains("/revisions/sha3/drafts")),
        "{:?}",
        f.http.requested()
    );
    // The thread list has it (a new unresolved thread of its own).
    assert!(listed(cx, &f).iter().any(|id| id.starts_with("draft")));
}

fn reviewed(cx: &mut TestAppContext, f: &Fixture, path: &str) -> bool {
    cx.update(|cx| f.page.read(cx).is_reviewed_for_test(path))
}

const PUT_B: &str = "PUT /changes/7/revisions/sha3/files/src%2Fb.rs/reviewed";
const DELETE_A: &str = "DELETE /changes/7/revisions/sha3/files/src%2Fa.rs/reviewed";

#[gpui_kit::test]
fn r_marks_the_file_under_the_cursor_reviewed_and_again_clears_it(cx: &mut TestAppContext) {
    let f = page_signed_in(cx, ChangeView::default(), |http| {
        http.route(PUT_B, 201, "");
        http.route(DELETE_A, 204, "");
    });
    // The seed has src/a.rs marked.
    assert!(reviewed(cx, &f, "src/a.rs") && !reviewed(cx, &f, "src/b.rs"));
    press(cx, &f, &["j", "j", "r"]);
    assert!(reviewed(cx, &f, "src/b.rs"));
    press(cx, &f, &["k", "r"]);
    assert!(!reviewed(cx, &f, "src/a.rs"));
    let sent: Vec<_> = f
        .http
        .sent()
        .into_iter()
        .filter(|r| r.target.ends_with("/reviewed"))
        .map(|r| format!("{} {}", r.method, r.target))
        .collect();
    assert_eq!(
        sent,
        [
            "PUT /a/changes/7/revisions/sha3/files/src%2Fb.rs/reviewed",
            "DELETE /a/changes/7/revisions/sha3/files/src%2Fa.rs/reviewed",
        ]
    );
    assert_eq!(cx.update(|cx| f.page.read(cx).hint_for_test()), None);
}

#[gpui_kit::test]
fn the_mark_goes_back_when_the_server_refuses(cx: &mut TestAppContext) {
    let f = page_signed_in(cx, ChangeView::default(), |http| {
        http.route(PUT_B, 403, "not allowed");
    });
    press(cx, &f, &["j", "j", "r"]);
    assert!(!reviewed(cx, &f, "src/b.rs"));
    let (note, error) = cx
        .update(|cx| f.page.read(cx).hint_for_test())
        .expect("a note");
    assert!(error && note.contains("src/b.rs"), "{note}");
    // A 403 is not a login problem.
    assert!(f.events.lock().expect("log").is_empty());
}

#[gpui_kit::test]
fn a_failed_request_keeps_the_mark_that_was_there(cx: &mut TestAppContext) {
    // Nothing is routed for the mark: the fake answers 404.
    let f = page_signed_in(cx, ChangeView::default(), |_| {});
    press(cx, &f, &["j", "r"]);
    assert!(reviewed(cx, &f, "src/a.rs"));
    assert!(cx.update(|cx| f.page.read(cx).hint_for_test()).is_some());
}

#[gpui_kit::test]
fn a_401_on_the_mark_goes_to_the_login_flow(cx: &mut TestAppContext) {
    let f = page_signed_in(cx, ChangeView::default(), |http| {
        http.route(PUT_B, 401, "");
    });
    press(cx, &f, &["j", "j", "r"]);
    assert!(!reviewed(cx, &f, "src/b.rs"));
    assert_eq!(*f.events.lock().expect("log"), ["unauthorized"]);
}

#[gpui_kit::test]
fn r_only_hints_for_anonymous_sessions(cx: &mut TestAppContext) {
    let f = page(cx, true, ChangeView::default());
    press(cx, &f, &["j", "r"]);
    assert!(!reviewed(cx, &f, "src/b.rs"));
    let (note, error) = cx
        .update(|cx| f.page.read(cx).hint_for_test())
        .expect("a note");
    assert!(!error && note.starts_with("Sign in"), "{note}");
    assert!(f.http.sent().iter().all(|r| r.method == "GET"));
}

/// [`seeded`] plus a chain of `n` changes around change 7 (the middle one),
/// cached for its current patch set.
fn seeded_with_chain(n: u64) -> Store {
    seeded_with_chain_at(n, n / 2)
}

/// Like [`seeded_with_chain`], with change 7 at row `at` (0 is the tip).
/// The others are 100 + their row.
fn seeded_with_chain_at(n: u64, at: u64) -> Store {
    let mut store = seeded();
    let chain: Vec<model::RelatedChange> = (0..n)
        .map(|i| model::RelatedChange {
            project: "p".into(),
            change_id: None,
            number: ChangeNumber(if i == at { 7 } else { 100 + i }),
            patch_set: PatchSetNumber(1),
            current_patch_set: Some(PatchSetNumber(1)),
            status: Some(model::ChangeStatus::New),
            submittable: None,
            work_in_progress: false,
            commit: None,
        })
        .collect();
    store
        .save_related(ServerId(1), ChangeNumber(7), &"sha3".into(), &chain, ts())
        .expect("related");
    store
}

fn wheel(cx: &mut TestAppContext, f: &Fixture, at: gpui_kit::Point<gpui_kit::Pixels>, dy: f32) {
    let mut vcx = gpui_kit::VisualTestContext::from_window(f.window, cx);
    vcx.simulate_event(gpui_kit::ScrollWheelEvent {
        position: at,
        delta: gpui_kit::ScrollDelta::Pixels(gpui_kit::point(gpui_kit::px(0.), gpui_kit::px(dy))),
        ..Default::default()
    });
}

#[gpui_kit::test]
fn the_wheel_over_the_relation_chain_scrolls_the_chain_and_not_the_page(cx: &mut TestAppContext) {
    let f = page_in(
        cx,
        seeded_with_chain(14),
        None,
        setup(),
        ChangeView::default(),
        |_| {},
    );
    // A window too short for the page to fit, so that the page can scroll.
    gpui_kit::VisualTestContext::from_window(f.window, cx)
        .simulate_resize(gpui_kit::size(gpui_kit::px(1100.), gpui_kit::px(400.)));
    cx.run_until_parked();
    let (chain, list) = cx.update(|cx| {
        let p = f.page.read(cx);
        (p.related_scroll_for_test(), p.list_for_test())
    });
    let max = chain.max_offset().y;
    assert!(max > gpui_kit::px(0.), "the chain overflows its box");
    let b = chain.bounds();
    let over = gpui_kit::point(b.center().x, b.top() + gpui_kit::px(20.));
    let page_top = |list: &gpui_kit::ListState| {
        let t = list.logical_scroll_top();
        (t.item_ix, t.offset_in_item)
    };
    assert_eq!(page_top(&list), (0, gpui_kit::px(0.)));

    // Down inside the box: only the box moves.
    wheel(cx, &f, over, -30.);
    assert_eq!(chain.offset().y, gpui_kit::px(-30.));
    assert_eq!(page_top(&list), (0, gpui_kit::px(0.)));

    // At the end of the box the wheel goes on to the page.
    wheel(cx, &f, over, -(max.as_f32() + 100.));
    assert_eq!(chain.offset().y, -max);
    assert_eq!(page_top(&list), (0, gpui_kit::px(0.)));
    wheel(cx, &f, over, -40.);
    assert_eq!(chain.offset().y, -max);
    assert_ne!(page_top(&list), (0, gpui_kit::px(0.)));
}

fn chain_page(cx: &mut TestAppContext, n: u64, at: u64) -> Fixture {
    page_in(
        cx,
        seeded_with_chain_at(n, at),
        None,
        setup(),
        ChangeView::default(),
        |_| {},
    )
}

fn events(f: &Fixture) -> Vec<String> {
    f.events.lock().expect("log").clone()
}

fn panel_shown(cx: &mut TestAppContext, f: &Fixture) -> bool {
    cx.update(|cx| f.page.read(cx).panel_shown_for_test())
}

fn resize(cx: &mut TestAppContext, f: &Fixture, w: f32) {
    gpui_kit::VisualTestContext::from_window(f.window, cx)
        .simulate_resize(gpui_kit::size(gpui_kit::px(w), gpui_kit::px(800.)));
    cx.run_until_parked();
}

#[gpui_kit::test]
fn shift_j_and_k_replace_the_page_with_the_next_and_previous_chain_entry(cx: &mut TestAppContext) {
    // Rows: 100, 101, 7, 103, 104 (newest descendant first).
    let f = chain_page(cx, 5, 2);
    press(cx, &f, &["shift-j"]);
    press(cx, &f, &["shift-k"]);
    assert_eq!(events(&f), ["replace 103", "replace 101"]);
}

#[gpui_kit::test]
fn shift_j_and_k_do_nothing_at_the_ends_of_the_chain(cx: &mut TestAppContext) {
    let f = chain_page(cx, 3, 0);
    press(cx, &f, &["shift-k"]);
    assert!(events(&f).is_empty());
    press(cx, &f, &["shift-j"]);
    assert_eq!(events(&f), ["replace 101"]);

    let f = chain_page(cx, 3, 2);
    press(cx, &f, &["shift-j"]);
    assert!(events(&f).is_empty());

    // No chain at all.
    let f = page(cx, false, ChangeView::default());
    press(cx, &f, &["shift-j", "shift-k"]);
    assert!(events(&f).is_empty());
}

#[gpui_kit::test]
fn clicking_a_chain_row_opens_it_as_a_new_history_entry(cx: &mut TestAppContext) {
    let f = chain_page(cx, 5, 2);
    let scroll = cx.update(|cx| f.page.read(cx).related_scroll_for_test());
    let at = scroll.bounds_for_item(3).expect("row 3").center();
    gpui_kit::VisualTestContext::from_window(f.window, cx).simulate_click(at, Default::default());
    cx.run_until_parked();
    assert_eq!(events(&f), ["change 103"]);
}

#[gpui_kit::test]
fn shift_l_and_the_link_toggle_the_relation_panel(cx: &mut TestAppContext) {
    let f = chain_page(cx, 5, 2);
    // Wide enough for the panel.
    resize(cx, &f, 1400.);
    assert!(!panel_shown(cx, &f));
    press(cx, &f, &["shift-l"]);
    assert!(panel_shown(cx, &f));
    press(cx, &f, &["shift-l"]);
    assert!(!panel_shown(cx, &f));

    // The link under the box does the same, and so does the one in the panel.
    for expect in [true, false] {
        let scroll = cx.update(|cx| f.page.read(cx).related_scroll_for_test());
        let b = scroll.bounds();
        let at = gpui_kit::point(b.left() + gpui_kit::px(20.), b.bottom() + gpui_kit::px(8.));
        let at = if expect {
            at
        } else {
            gpui_kit::point(at.x, at.y + gpui_kit::px(8.))
        };
        gpui_kit::VisualTestContext::from_window(f.window, cx)
            .simulate_click(at, Default::default());
        cx.run_until_parked();
        assert_eq!(panel_shown(cx, &f), expect);
    }
}

#[gpui_kit::test]
fn the_panel_gives_way_to_the_box_in_a_narrow_window(cx: &mut TestAppContext) {
    let f = chain_page(cx, 5, 2);
    resize(cx, &f, 1400.);
    press(cx, &f, &["shift-l"]);
    assert!(panel_shown(cx, &f));
    resize(cx, &f, 1100.);
    assert!(!panel_shown(cx, &f));
    // The wish is kept: widening brings the panel back.
    resize(cx, &f, 1400.);
    assert!(panel_shown(cx, &f));
}

#[gpui_kit::test]
fn no_chain_no_panel(cx: &mut TestAppContext) {
    let f = page(cx, false, ChangeView::default());
    press(cx, &f, &["shift-l"]);
    assert!(!panel_shown(cx, &f));
}

// ---- quick votes ----

const REVIEW: &str = "POST /changes/7/revisions/sha3/review";

/// Change 7 as a reviewer "u" sees it: Code-Review and Verified may be
/// voted on; in a chain of three where 7 is the middle (child 100, parent
/// 102). `child` is the status of 100.
fn voting_store(child: model::ChangeStatus) -> Store {
    let mut store = seeded_with_chain_at(3, 1);
    let mut d = detail(7, 3, 3);
    d.permitted_labels = [
        ("Code-Review".to_owned(), vec![-2, -1, 0, 1, 2]),
        ("Verified".to_owned(), vec![-1, 0, 1]),
    ]
    .into();
    store
        .save_change_detail(ServerId(1), &d, ts())
        .expect("detail");
    let mut chain = store
        .load_related(ServerId(1), ChangeNumber(7), &"sha3".into())
        .expect("related")
        .expect("cached")
        .changes;
    chain[0].status = Some(child);
    store
        .save_related(ServerId(1), ChangeNumber(7), &"sha3".into(), &chain, ts())
        .expect("related");
    store
}

fn voting_page(
    cx: &mut TestAppContext,
    child: model::ChangeStatus,
    routes: impl FnOnce(&RouteHttpClient),
) -> Fixture {
    let mut server = setup();
    server.username = Some("u".into());
    let auth = Auth::Basic {
        username: "u".into(),
        password: "p".into(),
    };
    page_in(
        cx,
        voting_store(child),
        Some(auth),
        server,
        ChangeView::default(),
        routes,
    )
}

fn ok_review(http: &RouteHttpClient) {
    http.route(REVIEW, 200, ")]}'\n{\"labels\":{\"Code-Review\":2}}");
}

fn reviews(f: &Fixture) -> Vec<serde_json::Value> {
    f.http
        .sent()
        .into_iter()
        .filter(|r| r.method == "POST" && r.target.contains("/review"))
        .map(|r| serde_json::from_slice(&r.body).expect("a JSON body"))
        .collect()
}

fn hint(cx: &mut TestAppContext, f: &Fixture) -> Option<(String, bool)> {
    cx.update(|cx| f.page.read(cx).hint_for_test())
}

#[gpui_kit::test]
fn shift_v_posts_the_first_vote_only_and_publishes_the_drafts(cx: &mut TestAppContext) {
    let f = voting_page(cx, model::ChangeStatus::Merged, ok_review);
    press(cx, &f, &["shift-v"]);
    cx.run_until_parked();
    let bodies = reviews(&f);
    assert_eq!(bodies.len(), 1, "{:?}", f.http.sent());
    assert_eq!(bodies[0]["labels"], serde_json::json!({"Code-Review": 2}));
    assert_eq!(bodies[0]["drafts"], "PUBLISH_ALL_REVISIONS");
    assert!(bodies[0].get("message").is_none(), "{}", bodies[0]);
    // The change is fetched again afterwards.
    let seen = f.http.requested();
    let at = seen
        .iter()
        .position(|r| r.ends_with("/review"))
        .expect("sent");
    assert!(
        seen[at..].iter().any(|r| r.starts_with("/a/changes/7?")),
        "{seen:?}"
    );
    // Merged child: stays and says so.
    assert!(events(&f).is_empty(), "{:?}", events(&f));
    assert_eq!(
        hint(cx, &f),
        Some(("Voted Code-Review +2".to_owned(), false))
    );
}

#[gpui_kit::test]
fn v_then_a_number_picks_that_button(cx: &mut TestAppContext) {
    let f = voting_page(cx, model::ChangeStatus::Merged, |http| {
        http.route(REVIEW, 200, ")]}'\n{\"labels\":{\"Verified\":1}}");
    });
    press(cx, &f, &["v", "2"]);
    cx.run_until_parked();
    let bodies = reviews(&f);
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["labels"], serde_json::json!({"Verified": 1}));
    // Out of range: a note, nothing sent.
    press(cx, &f, &["v", "3"]);
    cx.run_until_parked();
    assert_eq!(reviews(&f).len(), 1);
    let (note, error) = hint(cx, &f).expect("a note");
    assert!(!error && note.contains("No such"), "{note}");
}

#[gpui_kit::test]
fn a_vote_moves_on_to_the_open_child_with_a_note(cx: &mut TestAppContext) {
    let f = voting_page(cx, model::ChangeStatus::New, ok_review);
    press(cx, &f, &["shift-v"]);
    cx.run_until_parked();
    assert_eq!(
        events(&f),
        ["advance 100: Voted Code-Review +2, moved to change 100"]
    );
}

#[gpui_kit::test]
fn a_reply_with_a_vote_moves_on_and_one_without_does_not(cx: &mut TestAppContext) {
    let f = voting_page(cx, model::ChangeStatus::New, |_| {});
    let sent = |voted: Vec<(String, i32)>, unapplied: Vec<(String, i32)>| crate::reply::Sent {
        detail: None,
        unapplied,
        warning: None,
        voted,
    };
    let page = f.page.clone();
    let deliver = |cx: &mut TestAppContext, s: crate::reply::Sent| {
        page.update(cx, |p, cx| p.reply_sent(&s, cx));
    };
    deliver(cx, sent(vec![], vec![]));
    assert!(events(&f).is_empty());
    assert_eq!(hint(cx, &f), Some(("Reply sent".to_owned(), false)));
    // A vote the server dropped is a warning and does not move.
    deliver(cx, sent(vec![], vec![("Code-Review".to_owned(), 2)]));
    assert!(events(&f).is_empty());
    deliver(cx, sent(vec![("Verified".to_owned(), 1)], vec![]));
    assert_eq!(
        events(&f),
        ["advance 100: Voted Verified +1, moved to change 100"]
    );
}

#[gpui_kit::test]
fn the_buttons_follow_the_dialog_order_and_show_an_applied_vote(cx: &mut TestAppContext) {
    let f = voting_page(cx, model::ChangeStatus::New, |_| {});
    let texts = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            f.page
                .read(cx)
                .quick_buttons_for_test(cx)
                .into_iter()
                .map(|b| (b.text(), b.applied))
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(
        texts(cx),
        [
            ("Code-Review +2".to_owned(), false),
            ("Verified +1".to_owned(), false)
        ]
    );
}

#[gpui_kit::test]
fn no_quick_votes_for_anonymous_sessions(cx: &mut TestAppContext) {
    let f = page(cx, true, ChangeView::default());
    press(cx, &f, &["shift-v", "v", "1"]);
    cx.run_until_parked();
    assert!(reviews(&f).is_empty());
    assert!(f.http.sent().iter().all(|r| r.method == "GET"));
}

#[gpui_kit::test]
fn a_401_goes_to_the_login_flow_and_other_errors_are_noted(cx: &mut TestAppContext) {
    let f = voting_page(cx, model::ChangeStatus::New, |http| {
        http.route(REVIEW, 401, "");
    });
    press(cx, &f, &["shift-v"]);
    cx.run_until_parked();
    assert_eq!(events(&f), ["unauthorized"]);

    let f = voting_page(cx, model::ChangeStatus::New, |http| {
        http.route(REVIEW, 403, "not allowed to vote");
    });
    press(cx, &f, &["shift-v"]);
    cx.run_until_parked();
    let (note, error) = hint(cx, &f).expect("a note");
    assert!(error && note.contains("not allowed to vote"), "{note}");
    assert!(events(&f).is_empty());
}

#[gpui_kit::test]
fn a_second_vote_while_one_is_on_its_way_is_ignored(cx: &mut TestAppContext) {
    let f = voting_page(cx, model::ChangeStatus::Merged, ok_review);
    f.page.update(cx, |p, cx| {
        p.quick_vote(0, cx);
        p.quick_vote(0, cx);
        p.quick_vote(1, cx);
    });
    cx.run_until_parked();
    assert_eq!(reviews(&f).len(), 1);
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

fn set_rules(cx: &mut TestAppContext, f: &Fixture, rules: Vec<FileFilter>) {
    f.filters
        .update(cx, |filters, cx| filters.set_rules(rules, cx));
    cx.run_until_parked();
}

fn visible(cx: &mut TestAppContext, f: &Fixture) -> Vec<String> {
    cx.update(|cx| {
        f.page
            .read(cx)
            .visible_paths()
            .into_iter()
            .map(str::to_owned)
            .collect()
    })
}

#[gpui_kit::test]
fn hidden_files_leave_the_blocks_and_the_cursor_follows_the_visible_ones(cx: &mut TestAppContext) {
    let f = page(cx, false, ChangeView::default());
    set_rules(cx, &f, vec![rule("Gen", "a.rs", FilterAction::Hide)]);
    assert_eq!(visible(cx, &f), ["/COMMIT_MSG", "src/b.rs", "src/c.rs"]);
    cx.update(|cx| {
        let p = f.page.read(cx);
        assert_eq!(p.file_paths(), PATHS, "the list itself is unchanged");
        let blocks = p.blocks_for_test();
        assert!(blocks.contains(&Block::File(2)));
        assert!(!blocks.contains(&Block::File(3)));
        let at = |b| blocks.iter().position(|x| *x == b).expect("block");
        assert_eq!(at(Block::FilterBar), at(Block::PatchSets) + 1);
    });
    // j steps over the hidden file; Enter opens the file under the cursor,
    // not the entry of the full list with the same number.
    press(cx, &f, &["j", "enter"]);
    assert_eq!(cursor(cx, &f).as_deref(), Some("src/b.rs"));
    let events = f.events.lock().expect("log").clone();
    assert_eq!(events, ["diff 7 None..3 src/b.rs".to_owned()]);
}

#[gpui_kit::test]
fn the_cursor_stays_on_its_file_when_the_rules_change(cx: &mut TestAppContext) {
    let f = page(cx, false, ChangeView::default());
    press(cx, &f, &["j", "j"]);
    assert_eq!(cursor(cx, &f).as_deref(), Some("src/b.rs"));
    set_rules(cx, &f, vec![rule("Gen", "a.rs", FilterAction::Hide)]);
    assert_eq!(cursor(cx, &f).as_deref(), Some("src/b.rs"));
    // Hiding the file under the cursor puts it on a file that is shown.
    set_rules(cx, &f, vec![rule("Gen", "*.rs", FilterAction::Hide)]);
    assert_eq!(visible(cx, &f), ["/COMMIT_MSG"]);
    assert_eq!(cursor(cx, &f).as_deref(), Some("/COMMIT_MSG"));
    set_rules(cx, &f, Vec::new());
    assert_eq!(visible(cx, &f), PATHS);
}

#[gpui_kit::test]
fn brackets_skip_dimmed_files_but_j_and_k_do_not(cx: &mut TestAppContext) {
    let f = page(cx, false, ChangeView::default());
    set_rules(cx, &f, vec![rule("Dim", "b.rs", FilterAction::Dim)]);
    press(cx, &f, &["]", "]"]);
    assert_eq!(cursor(cx, &f).as_deref(), Some("src/c.rs"));
    press(cx, &f, &["["]);
    assert_eq!(cursor(cx, &f).as_deref(), Some("src/a.rs"));
    press(cx, &f, &["j"]);
    assert_eq!(cursor(cx, &f).as_deref(), Some("src/b.rs"));
    press(cx, &f, &["j", "k"]);
    assert_eq!(cursor(cx, &f).as_deref(), Some("src/b.rs"));
    // Highlighted files are not skipped.
    set_rules(
        cx,
        &f,
        vec![rule(
            "Hi",
            "b.rs",
            FilterAction::Highlight {
                color: None,
                bold: true,
            },
        )],
    );
    press(cx, &f, &["["]);
    assert_eq!(cursor(cx, &f).as_deref(), Some("src/a.rs"));
    press(cx, &f, &["]"]);
    assert_eq!(cursor(cx, &f).as_deref(), Some("src/b.rs"));
}

#[gpui_kit::test]
fn toggling_a_rule_restyles_the_page_live(cx: &mut TestAppContext) {
    let f = page(cx, false, ChangeView::default());
    set_rules(cx, &f, vec![rule("Gen", "a.rs", FilterAction::Hide)]);
    assert_eq!(visible(cx, &f).len(), 3);
    f.filters
        .update(cx, |filters, cx| filters.toggle("Gen", cx));
    cx.run_until_parked();
    assert_eq!(visible(cx, &f), PATHS);
    // `f 1` is the same toggle.
    press(cx, &f, &["f", "1"]);
    assert_eq!(visible(cx, &f).len(), 3);
    press(cx, &f, &["f", "1"]);
    assert_eq!(visible(cx, &f), PATHS);
    // No second rule: a note, nothing else changes.
    press(cx, &f, &["f", "2"]);
    assert_eq!(visible(cx, &f), PATHS);
    assert!(cx.update(|cx| f.page.read(cx).hint_for_test()).is_some());
}

#[gpui_kit::test]
fn rules_of_other_projects_do_not_apply_or_get_a_key(cx: &mut TestAppContext) {
    let f = page(cx, false, ChangeView::default());
    let mut other = rule("Other", "a.rs", FilterAction::Hide);
    other.projects = vec!["some/other".into()];
    set_rules(
        cx,
        &f,
        vec![other, rule("Mine", "b.rs", FilterAction::Hide)],
    );
    assert_eq!(visible(cx, &f), ["/COMMIT_MSG", "src/a.rs", "src/c.rs"]);
    press(cx, &f, &["f", "1"]); // the first button is "Mine"
    assert_eq!(visible(cx, &f), PATHS);
}

#[gpui_kit::test]
fn a_change_whose_files_are_all_hidden_says_so(cx: &mut TestAppContext) {
    let mut store = seeded();
    let server = store.server_id(URL).expect("server");
    store
        .save_file_list(
            server,
            &key(None, 3),
            &files(&["a.pb.go", "b.pb.go"]),
            None,
            ts(),
        )
        .expect("files");
    let f = page_in(cx, store, None, setup(), ChangeView::default(), |_| {});
    set_rules(cx, &f, vec![rule("Gen", "*.pb.go", FilterAction::Hide)]);
    cx.update(|cx| {
        let p = f.page.read(cx);
        assert!(p.visible_paths().is_empty());
        assert!(p.blocks_for_test().contains(&Block::FilesNotice));
        assert!(p.blocks_for_test().contains(&Block::FilterBar));
        assert_eq!(
            p.files_notice_text(),
            ("All 2 files are hidden by filters".to_owned(), false)
        );
    });
    assert_eq!(cursor(cx, &f), None);
    press(cx, &f, &["j", "enter", "]"]);
    assert!(f.events.lock().expect("log").is_empty());
}

#[gpui_kit::test]
fn manage_filters_emits_an_event_with_the_project(cx: &mut TestAppContext) {
    let f = page(cx, false, ChangeView::default());
    press(cx, &f, &["f", "m"]);
    let events = f.events.lock().expect("log").clone();
    assert_eq!(events, ["manage filters platform/foo".to_owned()]);
}

// --- syntax highlighting ahead -------------------------------------------

fn queued(cx: &mut TestAppContext, f: &Fixture) -> Vec<String> {
    cx.update(|cx| {
        f.syntax
            .read(cx)
            .last_queue_for_test()
            .iter()
            .map(|k| k.path.clone())
            .collect()
    })
}

fn set_syntax(cx: &mut TestAppContext, f: &Fixture, on: bool) {
    f.settings.update(cx, |s, cx| {
        s.update_session(|p| p.syntax_highlighting = Some(on), cx)
    });
    cx.run_until_parked();
}

#[gpui_kit::test]
fn opening_queues_the_first_file_that_is_not_magic(cx: &mut TestAppContext) {
    let f = page(cx, false, ChangeView::default());
    assert_eq!(queued(cx, &f), ["src/a.rs"]);
}

#[gpui_kit::test]
fn a_hidden_or_dimmed_first_file_moves_the_queue_on(cx: &mut TestAppContext) {
    let f = page(cx, false, ChangeView::default());
    set_rules(cx, &f, vec![rule("A", "src/a.rs", FilterAction::Hide)]);
    assert_eq!(queued(cx, &f), ["src/b.rs"]);
    set_rules(
        cx,
        &f,
        vec![
            rule("A", "src/a.rs", FilterAction::Dim),
            rule("B", "src/b.rs", FilterAction::Dim),
        ],
    );
    assert_eq!(queued(cx, &f), ["src/c.rs"]);
}

#[gpui_kit::test]
fn nothing_is_queued_with_syntax_highlighting_off_until_it_is_on(cx: &mut TestAppContext) {
    let f = page(cx, false, ChangeView::default());
    set_syntax(cx, &f, false);
    assert!(queued(cx, &f).is_empty());
    set_syntax(cx, &f, true);
    assert_eq!(queued(cx, &f), ["src/a.rs"]);
}

#[test]
fn blocking_votes_name_the_label_and_vote() {
    use model::LabelSummary;
    let mut s = crate::testing::summary(1);
    s.labels.insert(
        "Code-Review".into(),
        LabelSummary {
            rejected: true,
            ..Default::default()
        },
    );
    s.labels.insert(
        "Verified".into(),
        LabelSummary {
            rejected: true,
            min: Some(-1),
            ..Default::default()
        },
    );
    s.labels.insert(
        "Style".into(),
        LabelSummary {
            disliked: true,
            ..Default::default()
        },
    );
    assert_eq!(
        super::view::blocking_votes(Some(&s)),
        ["Rejected", "Style -1", "Verified -1"]
    );
    assert!(super::view::blocking_votes(None).is_empty());
}
