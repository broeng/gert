//! The change page on GPUI's `TestAppContext`: cache first, the file
//! cursor, the patch set pair and inline diffs.

use std::sync::{Arc, Mutex};

use gerrit_api::{Auth, fake::RouteHttpClient};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
use model::{ChangeNumber, DiffContext, FileStatus, FilesKey, PatchSetNumber, PatchSetRef};
use store::{ServerId, Store};

use super::{ChangeEvent, ChangePage, state::Block};
use crate::{
    LocalDiff,
    nav::ChangeView,
    session::Session,
    settings::DiffSettings,
    testing::{detail, file},
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
    let http = RouteHttpClient::new();
    http.route("/diff", 200, diff_body());
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

#[gpui_kit::test]
fn showing_the_whole_file_in_an_inline_diff_refetches_with_all_context(cx: &mut TestAppContext) {
    let f = page(cx, true, ChangeView::default());
    press(cx, &f, &["i"]);
    cx.update(|cx| {
        assert_eq!(
            f.page.read(cx).inline_context("/COMMIT_MSG"),
            Some(DiffContext::Lines(10))
        );
    });
    // Focus the inline view (as a click would) and ask for the whole file.
    cx.update_window(f.window, |_, window, cx| {
        let view = f.page.read(cx).inline_view("/COMMIT_MSG").expect("view");
        let focus = view.read(cx).focus_for_test();
        window.focus(&focus, cx);
    })
    .expect("focus");
    press(cx, &f, &["shift-x"]);
    cx.update(|cx| {
        assert_eq!(
            f.page.read(cx).inline_context("/COMMIT_MSG"),
            Some(DiffContext::All)
        );
    });
    assert!(f.http.requested().iter().any(|r| r.contains("context=ALL")));
    // Esc hands the focus back to the file list: j moves the page cursor.
    press(cx, &f, &["escape", "j"]);
    assert_eq!(cursor(cx, &f).as_deref(), Some("src/a.rs"));
}
