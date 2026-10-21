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
    nav::ChangeView,
    session::Session,
    settings::DiffSettings,
    testing::{detail, file, whole_file},
    tests::{URL, env_with, open, setup},
};

const NOW: &str = "2024-06-15T10:05:00Z";
const PATHS: [&str; 4] = ["/COMMIT_MSG", "src/a.rs", "src/b.rs", "src/c.rs"];

fn ts() -> jiff::Timestamp {
    NOW.parse().expect("timestamp")
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
    let http = RouteHttpClient::new();
    http.route("/diff", 200, diff_body());
    routes(&http);
    let env = env_with(seeded(), &http);
    let auth = online.then_some(Auth::Anonymous);
    let session = Session::new(&env, &setup(), auth).expect("session");
    let events = Arc::new(Mutex::new(Vec::new()));
    let log = events.clone();
    let (window, page) = open(cx, |window, cx| {
        let settings = cx.new(|_| DiffSettings::new(LocalDiff::default()));
        let page = cx.new(|cx| ChangePage::new(session, settings, ChangeNumber(7), view, true, cx));
        cx.subscribe(&page, move |_, ev: &ChangeEvent, _| {
            let text = match ev {
                ChangeEvent::Unauthorized => "unauthorized".to_owned(),
                ChangeEvent::OpenChange(n) => format!("change {n}"),
                ChangeEvent::OpenDiff(d) => format!(
                    "diff {} {:?}..{} {}",
                    d.change,
                    d.base.map(|b| b.0),
                    d.target,
                    d.path
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
    // Esc hands the focus back to the file list: j moves the page cursor.
    press(cx, &f, &["escape", "j"]);
    assert_eq!(cursor(cx, &f).as_deref(), Some("src/a.rs"));
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
    keys(cx, window, &["escape", "j"]);
    cx.update(|cx| assert_eq!(page.read(cx).cursor_path(), Some("src/f000.rs")));
    drop(shell);
}

/// Esc closes the help overlay in one press, also when an inline diff has
/// the focus (where Esc otherwise hands the focus back to the file list),
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
    keys(cx, window, &["escape", "j"]);
    cx.update(|cx| assert_eq!(page.read(cx).cursor_path(), Some("src/f000.rs")));
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
        server: Some(setup()),
        queries: vec![crate::session::SavedQuery {
            name: "Open".into(),
            query: "status:open".into(),
        }],
        diff: LocalDiff::default(),
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
