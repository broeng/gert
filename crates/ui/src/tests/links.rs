//! Links and the add-server flow with several configured servers.

use gerrit_api::fake::RouteHttpClient;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
use model::ChangeNumber;
use store::Store;

use super::{env_with, open, setup, spec};
use crate::goto::LinkError;
use crate::nav::Route;
use crate::session::{ServerSetup, Startup};
use crate::shell::Shell;

const ACCOUNT: &str = ")]}'\n{\"_account_id\": 1, \"name\": \"Alice\"}";

fn server(key: &str, url: &str, id: i64) -> ServerSetup {
    ServerSetup {
        key: crate::ServerKey::new(key),
        name: key.into(),
        base_url: url.into(),
        server: store::ServerId(id),
        queries: vec![spec(key, "status:open")],
        ..setup()
    }
}

/// Anonymous servers, so that no keyring is involved.
fn start(
    cx: &mut TestAppContext,
    list: &[(&str, &str)],
    active: Option<&str>,
    link: Option<&str>,
    http: &RouteHttpClient,
) -> (AnyWindowHandle, Entity<Shell>) {
    let env = env_with(Store::open_in_memory().expect("store"), http);
    let servers: Vec<_> = list
        .iter()
        .enumerate()
        .map(|(i, (k, u))| server(k, u, i as i64 + 1))
        .collect();
    let startup = Startup {
        env,
        diff: Default::default(),
        start_page: Default::default(),
        filters: Vec::new(),
        servers,
        active: active.map(crate::ServerKey::new),
    };
    let link = link.map(str::to_owned);
    let started = open(cx, |window, cx| {
        cx.new(|cx| {
            let mut shell = Shell::new(startup, window, cx);
            if let Some(link) = &link {
                shell.open_link(link, window, cx);
            }
            shell
        })
    });
    cx.run_until_parked();
    started
}

fn place(cx: &mut TestAppContext, shell: &Entity<Shell>) -> (String, Route) {
    cx.update(|cx| {
        let s = shell.read(cx);
        (
            s.active_for_test().expect("active").to_string(),
            s.route().clone(),
        )
    })
}

fn open_link(cx: &mut TestAppContext, window: AnyWindowHandle, shell: &Entity<Shell>, t: &str) {
    cx.update_window(window, |_, window, cx| {
        shell.update(cx, |s, cx| s.open_link(t, window, cx))
    })
    .expect("open link");
    cx.run_until_parked();
}

fn press(cx: &mut TestAppContext, window: AnyWindowHandle, key: &'static str) {
    cx.update_window(window, |_, window, cx| window.press(key, cx))
        .expect("press");
    cx.run_until_parked();
}

const TWO: [(&str, &str); 2] = [("a", "https://g.example/ga"), ("b", "https://g.example/gb")];

#[gpui_kit::test]
fn a_link_switches_to_the_server_it_is_for(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    let (window, shell) = start(cx, &TWO, Some("a"), None, &http);
    open_link(cx, window, &shell, "https://g.example/gb/c/p/+/5");
    assert_eq!(
        place(cx, &shell),
        ("b".into(), Route::Change(ChangeNumber(5)))
    );
    // Back returns to the page of the other server.
    cx.update_window(window, |_, window, cx| {
        shell.update(cx, |s, cx| s.back_for_test(window, cx))
    })
    .expect("back");
    assert_eq!(place(cx, &shell), ("a".into(), Route::Dashboard));
    // A number stays on the active server.
    open_link(cx, window, &shell, "9");
    assert_eq!(
        place(cx, &shell),
        ("a".into(), Route::Change(ChangeNumber(9)))
    );
}

#[gpui_kit::test]
fn the_same_host_at_different_paths_is_told_apart(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    let list = [("root", "https://g.example"), ("r", "https://g.example/r")];
    let (window, shell) = start(cx, &list, Some("root"), None, &http);
    open_link(cx, window, &shell, "https://g.example/r/c/p/+/5");
    assert_eq!(place(cx, &shell).0, "r");
    open_link(cx, window, &shell, "https://g.example/c/p/+/6");
    assert_eq!(
        place(cx, &shell),
        ("root".into(), Route::Change(ChangeNumber(6)))
    );
}

#[gpui_kit::test]
fn servers_sharing_a_url_prefer_the_active_one(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    let list = [("alice", "https://g.example"), ("bob", "https://g.example")];
    let (window, shell) = start(cx, &list, Some("bob"), None, &http);
    open_link(cx, window, &shell, "https://g.example/c/p/+/5");
    assert_eq!(
        place(cx, &shell),
        ("bob".into(), Route::Change(ChangeNumber(5)))
    );
    let (window, shell) = start(cx, &list, Some("alice"), None, &http);
    open_link(cx, window, &shell, "https://g.example/c/p/+/5");
    assert_eq!(place(cx, &shell).0, "alice");
}

#[gpui_kit::test]
fn the_startup_link_selects_its_server(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    let (_window, shell) = start(
        cx,
        &TWO,
        Some("a"),
        Some("https://g.example/gb/c/p/+/5/2/x.rs"),
        &http,
    );
    let (server, route) = place(cx, &shell);
    assert_eq!(server, "b");
    assert!(matches!(route, Route::Diff(_)), "{route:?}");
}

#[gpui_kit::test]
fn an_unknown_link_offers_to_add_the_server_and_sends_nothing(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route("/accounts/self/detail", 200, ACCOUNT);
    let (window, shell) = start(
        cx,
        &TWO,
        Some("a"),
        Some("https://new.example.org/gerrit/c/p/+/3"),
        &http,
    );
    // The dialog says why and offers the address.
    let dialog = cx.update(|cx| shell.read(cx).goto_for_test().expect("dialog"));
    cx.update(|cx| {
        assert_eq!(
            dialog.read(cx).error_for_test(),
            Some(LinkError::UnknownServer(
                "https://new.example.org/gerrit".into()
            ))
        );
        assert_eq!(
            dialog.read(cx).offer_for_test().as_deref(),
            Some("https://new.example.org/gerrit")
        );
    });
    assert_eq!(place(cx, &shell), ("a".into(), Route::Dashboard));

    // Add server (the key): the login page for a new server, filled in.
    press(cx, window, "ctrl-enter");
    assert!(cx.update(|cx| shell.read(cx).goto_for_test().is_none()));
    assert_eq!(place(cx, &shell), ("a".into(), Route::Login));
    let page = cx.update(|cx| shell.read(cx).login_page_for_test().expect("login"));
    cx.update(|cx| {
        let p = page.read(cx);
        assert!(p.is_new_server_for_test());
        assert_eq!(p.url_for_test(cx), "https://new.example.org/gerrit");
        assert!(
            p.notice_for_test()
                .expect("notice")
                .contains("nothing has been")
        );
    });
    // Nothing went to that host so far. The fake records paths only;
    // the new server has the path prefix /gerrit, and only a login asks
    // for the account.
    let to_new = |http: &RouteHttpClient| {
        http.requested()
            .iter()
            .filter(|r| r.starts_with("/gerrit/") || r.contains("accounts/self"))
            .count()
    };
    assert_eq!(to_new(&http), 0, "{:?}", http.requested());

    // The login is the user's confirmation.
    cx.update_window(window, |_, window, cx| {
        page.update(cx, |p, cx| {
            p.login_for_test("Work", "alice", "pw", window, cx)
        })
    })
    .expect("login");
    cx.run_until_parked();
    assert!(to_new(&http) >= 1);
    // The new server is active and shows the link; the login page took
    // the place of its history entry, the rest of the history is kept.
    assert_eq!(
        place(cx, &shell),
        ("default".into(), Route::Change(ChangeNumber(3)))
    );
    cx.update(|cx| {
        assert_eq!(
            shell.read(cx).server_name_for_test("default").as_deref(),
            Some("Work")
        )
    });
    let (trail, at) = cx.update(|cx| shell.read(cx).trail_for_test());
    assert_eq!(
        trail,
        [
            (Some("a".into()), Route::Dashboard),
            (Some("default".into()), Route::Change(ChangeNumber(3))),
        ]
    );
    assert_eq!(at, 1);
}

#[gpui_kit::test]
fn an_unknown_startup_link_starts_normally_and_makes_the_offer(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    let (_window, shell) = start(
        cx,
        &TWO,
        Some("b"),
        Some("https://new.example.org/c/p/+/3"),
        &http,
    );
    assert_eq!(place(cx, &shell), ("b".into(), Route::Dashboard));
    let dialog = cx.update(|cx| shell.read(cx).goto_for_test().expect("dialog"));
    cx.update(|cx| {
        assert_eq!(
            dialog.read(cx).offer_for_test().as_deref(),
            Some("https://new.example.org")
        )
    });
    assert!(
        http.requested()
            .iter()
            .all(|r| !r.contains("new.example.org")),
        "{:?}",
        http.requested()
    );
}

#[gpui_kit::test]
fn without_any_server_a_link_opens_the_login_page_filled_in(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route("/accounts/self/detail", 200, ACCOUNT);
    let (window, shell) = start(
        cx,
        &[],
        None,
        Some("https://new.example.org/c/p/+/3"),
        &http,
    );
    assert_eq!(shell_route(cx, &shell), Route::Login);
    let page = cx.update(|cx| shell.read(cx).login_page_for_test().expect("login"));
    cx.update(|cx| assert_eq!(page.read(cx).url_for_test(cx), "https://new.example.org"));
    assert!(http.requested().is_empty());
    cx.update_window(window, |_, window, cx| {
        page.update(cx, |p, cx| p.login_for_test("", "alice", "pw", window, cx))
    })
    .expect("login");
    cx.run_until_parked();
    assert_eq!(
        place(cx, &shell),
        ("default".into(), Route::Change(ChangeNumber(3)))
    );
    // No name given: the host.
    cx.update(|cx| {
        assert_eq!(
            shell.read(cx).server_name_for_test("default").as_deref(),
            Some("new.example.org")
        )
    });
}

fn shell_route(cx: &mut TestAppContext, shell: &Entity<Shell>) -> Route {
    cx.update(|cx| shell.read(cx).route().clone())
}

#[gpui_kit::test]
fn the_add_server_key_keeps_the_history_and_back_leaves_the_login(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    let (window, shell) = start(cx, &TWO, Some("a"), None, &http);
    open_link(cx, window, &shell, "https://g.example/gb/c/p/+/5");
    press(cx, window, "ctrl-shift-a");
    assert_eq!(place(cx, &shell), ("b".into(), Route::Login));
    let page = cx.update(|cx| shell.read(cx).login_page_for_test().expect("login"));
    cx.update(|cx| {
        assert!(page.read(cx).is_new_server_for_test());
        assert_eq!(page.read(cx).url_for_test(cx), "");
    });
    let (trail, _) = cx.update(|cx| shell.read(cx).trail_for_test());
    assert_eq!(trail.len(), 3, "{trail:?}");
    cx.update_window(window, |_, window, cx| {
        shell.update(cx, |s, cx| s.back_for_test(window, cx))
    })
    .expect("back");
    assert_eq!(
        place(cx, &shell),
        ("b".into(), Route::Change(ChangeNumber(5)))
    );
}

#[gpui_kit::test]
fn the_switcher_offers_add_server(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    let (window, shell) = start(cx, &TWO, Some("a"), None, &http);
    press(cx, window, "ctrl-k");
    assert!(cx.update(|cx| shell.read(cx).switcher_for_test().is_some()));
    press(cx, window, "ctrl-shift-a");
    assert!(cx.update(|cx| shell.read(cx).switcher_for_test().is_none()));
    assert_eq!(place(cx, &shell), ("a".into(), Route::Login));
}

#[gpui_kit::test]
fn showing_a_server_that_lost_its_session_keeps_the_history(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    let (window, shell) = start(cx, &TWO, Some("a"), None, &http);
    open_link(cx, window, &shell, "7");
    press(cx, window, "ctrl-2");
    assert_eq!(place(cx, &shell), ("b".into(), Route::Dashboard));
    // The server a refuses the credentials while b is shown.
    cx.update_window(window, |_, window, cx| {
        shell.update(cx, |s, cx| s.unauthorized_for_test("a", window, cx))
    })
    .expect("unauthorized");
    press(cx, window, "ctrl-1");
    assert_eq!(place(cx, &shell), ("a".into(), Route::Login));
    let (trail, at) = cx.update(|cx| shell.read(cx).trail_for_test());
    assert_eq!(at, trail.len() - 1);
    assert_eq!(
        trail[..3],
        [
            (Some("a".into()), Route::Dashboard),
            (Some("a".into()), Route::Change(ChangeNumber(7))),
            (Some("b".into()), Route::Dashboard),
        ]
    );
}
