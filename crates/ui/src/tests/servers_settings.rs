//! The servers section of the settings page.

use std::sync::Arc;

use gerrit_api::fake::FakeHttpClient;
use gpui_kit::component::input::InputState;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
use store::Store;

use super::{env_with, open, setup, spec};
use crate::{
    login::tests::FakeBackend,
    nav::Route,
    pages::settings::SettingsPage,
    pages::settings::servers::TestState,
    session::{Env, ServerSetup, Startup},
    shell::Shell,
};

const ACCOUNT: &str = ")]}'\n{\"_account_id\": 1, \"name\": \"Alice\"}";

struct Fixture {
    window: AnyWindowHandle,
    shell: Entity<Shell>,
    backend: Arc<FakeBackend>,
    http: FakeHttpClient,
}

fn server(key: &str, user: Option<&str>, id: i64) -> ServerSetup {
    ServerSetup {
        key: crate::ServerKey::new(key),
        name: key.to_uppercase(),
        base_url: format!("https://{key}.example"),
        username: user.map(Into::into),
        server: store::ServerId(id),
        queries: vec![spec(key, "status:open")],
        ..setup()
    }
}

/// Servers `a` (user `u`) and `b` (anonymous) unless `list` says
/// otherwise; a password is saved for every account.
fn start(cx: &mut TestAppContext, list: Vec<ServerSetup>, active: Option<&str>) -> Fixture {
    let backend = Arc::new(FakeBackend {
        password: Some("pw".into()),
        ..Default::default()
    });
    let http = FakeHttpClient::new();
    let env = Env {
        backend: backend.clone(),
        ..env_with(Store::open_in_memory().expect("store"), &http)
    };
    let startup = Startup {
        env,
        diff: Default::default(),
        start_page: Default::default(),
        filters: Vec::new(),
        servers: list,
        active: active.map(crate::ServerKey::new),
    };
    let (window, shell) = open(cx, |window, cx| {
        cx.new(|cx| Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    Fixture {
        window,
        shell,
        backend,
        http,
    }
}

fn two(cx: &mut TestAppContext) -> Fixture {
    start(
        cx,
        vec![server("a", Some("u"), 1), server("b", None, 2)],
        Some("a"),
    )
}

fn press(cx: &mut TestAppContext, f: &Fixture, key: &'static str) {
    cx.update_window(f.window, |_, window, cx| window.press(key, cx))
        .expect("press");
    cx.run_until_parked();
}

/// Opens the settings and puts the cursor on server row `i`.
fn on_server(cx: &mut TestAppContext, f: &Fixture, i: usize) -> Entity<SettingsPage> {
    press(
        cx,
        f,
        if cfg!(target_os = "macos") {
            "cmd-,"
        } else {
            "ctrl-,"
        },
    );
    let page = cx.update(|cx| f.shell.read(cx).settings_page_for_test().expect("page"));
    page.update(cx, |p, cx| {
        let row = p.servers_row_for_test() + i;
        p.select_row_for_test(row, cx)
    });
    page
}

fn type_into(cx: &mut TestAppContext, f: &Fixture, input: &Entity<InputState>, text: &str) {
    let text = text.to_owned();
    cx.update_window(f.window, |_, window, cx| {
        input.update(cx, |s, cx| s.set_value(text, window, cx))
    })
    .expect("set value");
}

fn place(cx: &mut TestAppContext, f: &Fixture) -> (Option<String>, Route) {
    cx.update(|cx| {
        let s = f.shell.read(cx);
        (
            s.active_for_test().map(ToString::to_string),
            s.route().clone(),
        )
    })
}

fn keys(cx: &mut TestAppContext, f: &Fixture) -> Vec<String> {
    cx.update(|cx| f.shell.read(cx).server_keys_for_test())
}

fn page_keys(cx: &mut TestAppContext, page: &Entity<SettingsPage>) -> Vec<String> {
    cx.update(|cx| {
        page.read(cx)
            .servers_for_test()
            .iter()
            .map(|s| s.key.to_string())
            .collect()
    })
}

#[gpui_kit::test]
fn removing_the_active_server_switches_to_the_first_remaining(cx: &mut TestAppContext) {
    let f = two(cx);
    let page = on_server(cx, &f, 0);
    // It asks first; n keeps the server.
    press(cx, &f, "d");
    assert!(cx.update(|cx| page.read(cx).confirming_server_for_test()));
    press(cx, &f, "n");
    assert!(f.backend.removed.lock().unwrap().is_empty());
    press(cx, &f, "d");
    press(cx, &f, "y");
    assert_eq!(*f.backend.removed.lock().unwrap(), ["a"]);
    assert_eq!(keys(cx, &f), ["b"]);
    assert_eq!(place(cx, &f), (Some("b".into()), Route::Dashboard));
    // The settings page of the removed server is gone from the history.
    cx.update(|cx| {
        let (trail, _) = f.shell.read(cx).trail_for_test();
        assert!(
            trail.iter().all(|(s, _)| s.as_deref() != Some("a")),
            "{trail:?}"
        );
    });
}

#[gpui_kit::test]
fn removing_another_server_keeps_the_page_and_the_active_server(cx: &mut TestAppContext) {
    let f = two(cx);
    let page = on_server(cx, &f, 1);
    press(cx, &f, "d");
    press(cx, &f, "y");
    assert_eq!(*f.backend.removed.lock().unwrap(), ["b"]);
    assert_eq!(keys(cx, &f), ["a"]);
    assert_eq!(page_keys(cx, &page), ["a"]);
    assert_eq!(place(cx, &f), (Some("a".into()), Route::Settings));
    // What was written is the list without it.
    let saved = f.backend.saved_servers.lock().unwrap().clone();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].len(), 1);
}

#[gpui_kit::test]
fn removing_the_last_server_opens_the_add_server_login(cx: &mut TestAppContext) {
    let f = start(cx, vec![server("b", None, 2)], None);
    let _page = on_server(cx, &f, 0);
    press(cx, &f, "d");
    press(cx, &f, "y");
    assert_eq!(*f.backend.removed.lock().unwrap(), ["b"]);
    assert!(keys(cx, &f).is_empty());
    assert_eq!(place(cx, &f), (None, Route::Login));
    let login = cx.update(|cx| f.shell.read(cx).login_page_for_test());
    assert!(login.is_some());
}

#[gpui_kit::test]
fn a_failed_removal_changes_nothing(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend {
        password: Some("pw".into()),
        fail: true,
        ..Default::default()
    });
    let http = FakeHttpClient::new();
    let env = Env {
        backend: backend.clone(),
        ..env_with(Store::open_in_memory().expect("store"), &http)
    };
    let startup = Startup {
        env,
        diff: Default::default(),
        start_page: Default::default(),
        filters: Vec::new(),
        servers: vec![server("a", None, 1), server("b", None, 2)],
        active: None,
    };
    let (window, shell) = open(cx, |window, cx| {
        cx.new(|cx| Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    let f = Fixture {
        window,
        shell,
        backend,
        http,
    };
    let page = on_server(cx, &f, 1);
    press(cx, &f, "d");
    press(cx, &f, "y");
    assert_eq!(keys(cx, &f), ["a", "b"]);
    assert_eq!(page_keys(cx, &page), ["a", "b"]);
    assert!(cx.update(|cx| page.read(cx).hint_for_test().is_some_and(|h| h.1)));
}

#[gpui_kit::test]
fn reordering_saves_the_new_order_and_the_shell_follows(cx: &mut TestAppContext) {
    let f = two(cx);
    let page = on_server(cx, &f, 0);
    press(cx, &f, "shift-j");
    assert_eq!(page_keys(cx, &page), ["b", "a"]);
    assert_eq!(keys(cx, &f), ["b", "a"]);
    let saved = f.backend.saved_servers.lock().unwrap().clone();
    let order: Vec<_> = saved[0].iter().map(|s| s.key.to_string()).collect();
    assert_eq!(order, ["b", "a"]);
    // The cursor followed the server; the top one cannot move up.
    press(cx, &f, "shift-k");
    assert_eq!(keys(cx, &f), ["a", "b"]);
    press(cx, &f, "shift-k");
    assert_eq!(keys(cx, &f), ["a", "b"]);
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn renaming_and_coloring_need_no_password(cx: &mut TestAppContext) {
    let f = two(cx);
    let page = on_server(cx, &f, 0);
    let before = f.http.requests().len();
    press(cx, &f, "e");
    let [name, _url, _user, color, _pw] = cx.update(|cx| page.read(cx).server_inputs_for_test());
    type_into(cx, &f, &name, "Day job");
    type_into(cx, &f, &color, "#FF8800");
    press(cx, &f, "enter");
    assert!(
        cx.update(|cx| page.read(cx).server_form_error_for_test())
            .is_none()
    );
    let saved = f.backend.saved_servers.lock().unwrap().clone();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0][0].name, "Day job");
    assert_eq!(saved[0][0].color.as_deref(), Some("#ff8800"));
    assert_eq!(
        cx.update(|cx| f.shell.read(cx).server_name_for_test("a"))
            .as_deref(),
        Some("Day job")
    );
    // No login happened: nothing was sent, no secret moved.
    assert_eq!(f.http.requests().len(), before);
    assert!(f.backend.saved.lock().unwrap().is_empty());
    assert!(f.backend.forgotten.lock().unwrap().is_empty());
    // Automatic again: an empty color.
    press(cx, &f, "e");
    type_into(cx, &f, &color, "");
    press(cx, &f, "enter");
    let saved = f.backend.saved_servers.lock().unwrap().clone();
    assert_eq!(saved[1][0].color, None);
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_new_url_asks_for_the_password_and_moves_the_secret(cx: &mut TestAppContext) {
    let f = two(cx);
    let page = on_server(cx, &f, 0);
    let before = f.http.requests().len();
    press(cx, &f, "e");
    let [_name, url, _user, _color, password] =
        cx.update(|cx| page.read(cx).server_inputs_for_test());
    type_into(cx, &f, &url, "https://moved.example/");
    press(cx, &f, "enter");
    // Without the password nothing happens.
    assert!(cx.update(|cx| page.read(cx).editing_for_test()));
    let why = cx.update(|cx| page.read(cx).server_form_error_for_test());
    assert!(why.is_some_and(|w| w.contains("password")));
    assert_eq!(f.http.requests().len(), before);
    assert!(f.backend.saved_servers.lock().unwrap().is_empty());

    type_into(cx, &f, &password, "new-pw");
    f.http.push_response(200, ACCOUNT);
    press(cx, &f, "enter");
    // Checked against the new URL, stored under it, old secret dropped.
    // (The new session fetches things of its own after the check.)
    let sent = f.http.requests();
    assert!(sent.len() > before);
    assert!(
        sent[before]
            .uri()
            .to_string()
            .starts_with("https://moved.example")
    );
    assert_eq!(
        *f.backend.saved.lock().unwrap(),
        [(
            "https://moved.example".to_owned(),
            Some("u".to_owned()),
            Some("new-pw".to_owned())
        )]
    );
    assert_eq!(
        *f.backend.forgotten.lock().unwrap(),
        [("https://a.example".to_owned(), "u".to_owned())]
    );
    assert_eq!(
        cx.update(|cx| f.shell.read(cx).server_url_for_test("a")),
        Some(("https://moved.example".to_owned(), Some("u".to_owned())))
    );
    assert!(!cx.update(|cx| page.read(cx).editing_for_test()));
    assert_eq!(page_keys(cx, &page), ["a", "b"]);
    assert_eq!(
        cx.update(|cx| page.read(cx).servers_for_test()[0].url.clone()),
        "https://moved.example"
    );
}

#[gpui_kit::test]
fn a_failed_check_or_a_cancel_leaves_everything_as_it_was(cx: &mut TestAppContext) {
    let f = two(cx);
    let page = on_server(cx, &f, 0);
    press(cx, &f, "e");
    let [_name, url, _user, _color, password] =
        cx.update(|cx| page.read(cx).server_inputs_for_test());
    type_into(cx, &f, &url, "https://moved.example");
    type_into(cx, &f, &password, "wrong");
    f.http.push_response(401, "Unauthorized");
    press(cx, &f, "enter");
    assert!(cx.update(|cx| page.read(cx).editing_for_test()));
    let why = cx.update(|cx| page.read(cx).server_form_error_for_test());
    assert!(
        why.is_some_and(|w| w.contains("wrong username")),
        "form error"
    );
    assert!(f.backend.saved.lock().unwrap().is_empty());
    assert!(f.backend.forgotten.lock().unwrap().is_empty());
    assert!(f.backend.saved_servers.lock().unwrap().is_empty());
    assert_eq!(
        cx.update(|cx| f.shell.read(cx).server_url_for_test("a")),
        Some(("https://a.example".to_owned(), Some("u".to_owned())))
    );

    press(cx, &f, "escape");
    assert!(!cx.update(|cx| page.read(cx).editing_for_test()));
    assert!(f.backend.saved_servers.lock().unwrap().is_empty());
    assert_eq!(
        cx.update(|cx| page.read(cx).servers_for_test()[0].url.clone()),
        "https://a.example"
    );
}

#[gpui_kit::test]
fn the_connection_is_only_tested_on_request_with_the_saved_password(cx: &mut TestAppContext) {
    let f = two(cx);
    let page = on_server(cx, &f, 0);
    let before = f.http.requests().len();
    // Opening the page and moving to the row sent nothing.
    press(cx, &f, "j");
    press(cx, &f, "k");
    assert_eq!(f.http.requests().len(), before);
    f.http.push_response(200, ACCOUNT);
    press(cx, &f, "t");
    assert_eq!(f.http.requests().len(), before + 1);
    assert_eq!(
        cx.update(|cx| page.read(cx).connection_test_for_test("a")),
        Some(TestState::Passed)
    );
    // The registry took the verdict over.
    let conn = |cx: &mut TestAppContext| cx.update(|cx| f.shell.read(cx).connection_for_test("a"));
    assert_eq!(conn(cx), Some(crate::Connection::Online));
    // A refusal is shown on the row.
    f.http.push_response(401, "Unauthorized");
    press(cx, &f, "t");
    assert_eq!(conn(cx), Some(crate::Connection::Unauthorized));
    let state = cx.update(|cx| page.read(cx).connection_test_for_test("a"));
    assert!(
        matches!(&state, Some(TestState::Failed(e)) if e.contains("wrong username")),
        "{state:?}"
    );
    // The other server was never asked.
    assert!(
        f.http
            .requests()
            .iter()
            .all(|r| r.uri().to_string().contains("a.example"))
    );
}

#[gpui_kit::test]
fn the_add_key_opens_the_login_page_for_a_new_server(cx: &mut TestAppContext) {
    let f = two(cx);
    let _page = on_server(cx, &f, 0);
    press(cx, &f, "a");
    assert_eq!(place(cx, &f), (Some("a".into()), Route::Login));
    let login = cx.update(|cx| f.shell.read(cx).login_page_for_test().expect("login"));
    assert!(cx.update(|cx| login.read(cx).is_new_server_for_test()));
}

#[gpui_kit::test]
fn the_queries_are_labelled_with_the_active_server_only_when_there_are_several(
    cx: &mut TestAppContext,
) {
    let f = two(cx);
    let page = on_server(cx, &f, 0);
    let label = |cx: &mut TestAppContext, page: &Entity<SettingsPage>| {
        cx.update(|cx| page.read(cx).active_badge(cx).map(|b| b.name.to_string()))
    };
    assert_eq!(label(cx, &page).as_deref(), Some("A"));
    press(cx, &f, "d");
    press(cx, &f, "y");
    // Removing the active server went to B, which is alone now.
    press(
        cx,
        &f,
        if cfg!(target_os = "macos") {
            "cmd-,"
        } else {
            "ctrl-,"
        },
    );
    let page = cx.update(|cx| f.shell.read(cx).settings_page_for_test().expect("page"));
    assert_eq!(label(cx, &page), None);
}
