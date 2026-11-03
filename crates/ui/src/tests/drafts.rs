//! Drafts that are not on the server.

use gerrit_api::fake::RouteHttpClient;
use gpui_kit::{AppContext as _, Entity, TestAppContext};
use model::{ChangeNumber, DraftInput, Location, PatchSetNumber, PatchSetRef, Side};
use store::{DraftFailure, Store};

use super::{env_with, open, seeded, setup, spec};
use crate::{
    filters::FileFilter,
    nav::Route,
    outbox::DraftOutbox,
    pages::list::ListEvent,
    session::{Backend, BackendError, SavedQuery, Startup, StartupPage},
    shell::Shell,
};

/// A keyring that has the password.
struct Keyring;

impl Backend for Keyring {
    fn for_server(
        self: std::sync::Arc<Self>,
        _: &crate::ServerKey,
    ) -> std::sync::Arc<dyn crate::ServerBackend> {
        self
    }

    fn save_filters(&self, f: &[FileFilter]) -> Result<Vec<FileFilter>, BackendError> {
        Ok(f.to_vec())
    }

    fn save_startup_page(&self, p: &StartupPage) -> Result<StartupPage, BackendError> {
        Ok(p.clone())
    }

    fn save_advance_after_vote(&self, on: bool) -> Result<bool, BackendError> {
        Ok(on)
    }

    fn save_theme(
        &self,
        t: crate::theme::ThemeName,
    ) -> Result<crate::theme::ThemeName, BackendError> {
        Ok(t)
    }
}

impl crate::ServerBackend for Keyring {
    fn load_password(&self, _: &str, _: &str) -> Result<Option<String>, BackendError> {
        Ok(Some("secret".into()))
    }

    fn save_queries(&self, _: &[SavedQuery]) -> Result<Vec<SavedQuery>, BackendError> {
        Err(BackendError("not used".into()))
    }

    fn save_login(
        &self,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<Vec<SavedQuery>, BackendError> {
        Err(BackendError("not used".into()))
    }
}

const RECEIPT: &str = r#")]}'
{"id":"srv1","patch_set":1,"updated":"2024-06-15 10:00:00.000000000"}"#;

fn input(message: &str) -> DraftInput {
    DraftInput {
        path: "a.rs".into(),
        side: Side::Revision,
        parent: None,
        location: Location::Line(3),
        in_reply_to: None,
        message: message.into(),
        unresolved: true,
    }
}

/// A store with a pending draft (`pending`) and a failed one (`failed`:
/// whether a retry may help). (The fake server gives every draft the
/// same id, so tests that upload two would clash.)
fn store_with_drafts(pending: bool, failed: Option<bool>) -> Store {
    let mut store = seeded(&[("status:open", &[1])]);
    let server = setup().server;
    let now = "2024-06-15T10:00:00Z".parse().expect("ts");
    let target = PatchSetRef {
        number: PatchSetNumber(1),
        sha: "sha1".into(),
    };
    if pending {
        let d = store
            .create_draft(server, ChangeNumber(7), &target, &input("waits"), now)
            .expect("draft");
        store
            .record_draft_failure(
                server,
                d.local_id,
                d.version,
                &DraftFailure::Transient("offline".into()),
                now,
            )
            .expect("failure");
    }
    if let Some(retryable) = failed {
        let d = store
            .create_draft(server, ChangeNumber(7), &target, &input("refused"), now)
            .expect("draft");
        store
            .record_draft_failure(
                server,
                d.local_id,
                d.version,
                &DraftFailure::Rejected {
                    error: "no".into(),
                    retryable,
                },
                now,
            )
            .expect("failure");
    }
    store
}

fn start(
    cx: &mut TestAppContext,
    store: Store,
    http: &RouteHttpClient,
) -> (gpui_kit::AnyWindowHandle, Entity<Shell>) {
    let mut env = env_with(store, http);
    env.backend = std::sync::Arc::new(Keyring);
    let startup = Startup {
        env,
        diff: Default::default(),
        start_page: Default::default(),
        filters: Vec::new(),
        servers: vec![crate::session::ServerSetup {
            username: Some("u".into()),
            queries: vec![spec("Open", "status:open")],
            bookmarks: Vec::new(),
            ..setup()
        }],
        active: None,
    };
    let started = open(cx, |window, cx| {
        cx.new(|cx| Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    started
}

fn counts(cx: &mut TestAppContext, shell: &Entity<Shell>) -> (usize, usize) {
    cx.update(|cx| {
        let outbox: Entity<DraftOutbox> = shell.read(cx).outbox_for_test().expect("outbox");
        let c = outbox.read(cx).counts();
        (c.pending, c.failed)
    })
}

fn uploads(http: &RouteHttpClient) -> usize {
    http.requested()
        .iter()
        .filter(|r| r.contains("/revisions/sha1/drafts"))
        .count()
}

#[gpui_kit::test]
fn drafts_that_waited_go_up_when_the_session_starts(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route("/revisions/sha1/drafts", 201, RECEIPT);
    let (_window, shell) = start(cx, store_with_drafts(true, Some(false)), &http);
    // The pending draft went up with the restored credentials; the
    // refused one stays failed.
    assert_eq!(uploads(&http), 1);
    assert_eq!(counts(cx, &shell), (0, 1));
}

#[gpui_kit::test]
fn a_successful_refresh_retries_pending_drafts(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route("/revisions/sha1/drafts", 503, "down");
    let (_window, shell) = start(cx, store_with_drafts(true, Some(false)), &http);
    assert_eq!(counts(cx, &shell), (1, 1));
    let before = uploads(&http);

    // Back online: the next successful refresh brings the draft up.
    http.route("/revisions/sha1/drafts", 201, RECEIPT);
    let page = cx
        .update(|cx| shell.read(cx).list_page_for_test())
        .expect("list page");
    cx.update(|cx| page.update(cx, |_, cx| cx.emit(ListEvent::Refreshed)));
    cx.run_until_parked();
    assert_eq!(uploads(&http), before + 1);
    assert_eq!(counts(cx, &shell), (0, 1));
}

#[gpui_kit::test]
fn the_indicators_retry_tries_failed_drafts_again(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route("/revisions/sha1/drafts", 201, RECEIPT);
    let (_window, shell) = start(cx, store_with_drafts(false, Some(true)), &http);
    assert_eq!(counts(cx, &shell), (0, 1));
    let before = uploads(&http);
    cx.update(|cx| shell.update(cx, |s, cx| s.retry_drafts_for_test(cx)));
    cx.run_until_parked();
    assert_eq!(uploads(&http), before + 1);
    assert_eq!(counts(cx, &shell), (0, 0));
}

// ---- several servers ----

const A: &str = "https://g.example/ga";
const B: &str = "https://g.example/gb";

/// A store with two servers that share change number 7 and the query,
/// and a draft each on change 7 (`a_state`/`b_state` as in
/// `store_with_drafts`: pending, or none).
fn two_server_store(a_pending: bool, b_pending: bool) -> (Store, [store::ServerId; 2]) {
    let mut store = Store::open_in_memory().expect("store");
    let ids = [
        store.server_for_slug("a", A).expect("a"),
        store.server_for_slug("b", B).expect("b"),
    ];
    let now = "2024-06-15T10:00:00Z".parse().expect("ts");
    let target = PatchSetRef {
        number: PatchSetNumber(1),
        sha: "sha1".into(),
    };
    for (id, pending) in ids.into_iter().zip([a_pending, b_pending]) {
        let rows = [crate::testing::summary(7)];
        store
            .save_query_page(id, "status:open", &rows, false, now)
            .expect("query");
        if pending {
            let d = store
                .create_draft(id, ChangeNumber(7), &target, &input("waits"), now)
                .expect("draft");
            store
                .record_draft_failure(
                    id,
                    d.local_id,
                    d.version,
                    &DraftFailure::Transient("offline".into()),
                    now,
                )
                .expect("failure");
        }
    }
    (store, ids)
}

fn start_two(
    cx: &mut TestAppContext,
    store: Store,
    ids: [store::ServerId; 2],
    active: Option<&str>,
    http: &RouteHttpClient,
) -> (Entity<Shell>, std::sync::Arc<std::sync::Mutex<Store>>) {
    let (_, shell, shared) = start_two_in(cx, store, ids, active, http);
    (shell, shared)
}

#[allow(clippy::type_complexity)]
fn start_two_in(
    cx: &mut TestAppContext,
    store: Store,
    ids: [store::ServerId; 2],
    active: Option<&str>,
    http: &RouteHttpClient,
) -> (
    gpui_kit::AnyWindowHandle,
    Entity<Shell>,
    std::sync::Arc<std::sync::Mutex<Store>>,
) {
    start_two_with(cx, store, ids, active, None, http)
}

/// As `start_two_in`, with server `anonymous` (if any) not signed in.
#[allow(clippy::type_complexity)]
fn start_two_with(
    cx: &mut TestAppContext,
    store: Store,
    ids: [store::ServerId; 2],
    active: Option<&str>,
    anonymous: Option<&str>,
    http: &RouteHttpClient,
) -> (
    gpui_kit::AnyWindowHandle,
    Entity<Shell>,
    std::sync::Arc<std::sync::Mutex<Store>>,
) {
    let mut env = env_with(store, http);
    env.backend = std::sync::Arc::new(Keyring);
    let shared = env.store.clone();
    let server = |key: &str, url: &str, id| crate::session::ServerSetup {
        key: crate::ServerKey::new(key),
        name: key.into(),
        base_url: url.into(),
        username: (anonymous != Some(key)).then(|| "u".into()),
        queries: vec![spec(key, "status:open")],
        server: id,
        ..setup()
    };
    let startup = Startup {
        env,
        diff: Default::default(),
        start_page: Default::default(),
        filters: Vec::new(),
        servers: vec![server("a", A, ids[0]), server("b", B, ids[1])],
        active: active.map(crate::ServerKey::new),
    };
    let (window, shell) = open(cx, |window, cx| {
        cx.new(|cx| Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    (window, shell, shared)
}

fn key(k: &str) -> crate::ServerKey {
    crate::ServerKey::new(k)
}

/// Where the shell is: the active server and the route.
fn place(cx: &mut TestAppContext, shell: &Entity<Shell>) -> (String, Route) {
    cx.update(|cx| {
        let s = shell.read(cx);
        (
            s.active_for_test().expect("active").to_string(),
            s.route().clone(),
        )
    })
}

fn at(server: &str, route: Route) -> (String, Route) {
    (server.to_owned(), route)
}

fn counts_of(cx: &mut TestAppContext, shell: &Entity<Shell>, key: &str) -> (usize, usize) {
    cx.update(|cx| {
        let outbox = shell
            .read(cx)
            .outbox_of_for_test(&crate::ServerKey::new(key))
            .expect("outbox");
        let c = outbox.read(cx).counts();
        (c.pending, c.failed)
    })
}

#[gpui_kit::test]
fn each_server_has_its_own_outbox_and_one_that_is_not_shown_still_uploads(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route("/revisions/sha1/drafts", 201, RECEIPT);
    // Change 7 has a draft on b only; a is the one shown.
    let (store, ids) = two_server_store(false, true);
    let (shell, _) = start_two(cx, store, ids, Some("a"), &http);
    let uploaded: Vec<String> = http
        .requested()
        .into_iter()
        .filter(|r| r.contains("/revisions/sha1/drafts"))
        .collect();
    assert_eq!(uploaded.len(), 1, "{uploaded:?}");
    assert!(uploaded[0].contains("/gb/"), "{uploaded:?}");
    assert_eq!(counts_of(cx, &shell, "a"), (0, 0));
    assert_eq!(counts_of(cx, &shell, "b"), (0, 0));
    // The title bar and the pages follow the active server.
    cx.update(|cx| {
        let active = shell.read(cx).outbox_for_test().expect("outbox");
        let a = shell
            .read(cx)
            .outbox_of_for_test(&crate::ServerKey::new("a"))
            .expect("a");
        assert_eq!(active, a);
    });
}

#[gpui_kit::test]
fn the_startup_server_is_the_requested_one_else_the_first_and_is_remembered(
    cx: &mut TestAppContext,
) {
    let http = RouteHttpClient::new();
    let (store, ids) = two_server_store(false, false);
    let (shell, shared) = start_two(cx, store, ids, Some("b"), &http);
    cx.update(|cx| {
        assert_eq!(shell.read(cx).queries_for_test()[0].name, "b");
        assert_eq!(
            shell.read(cx).list_page_for_test().map(|_| ()),
            Some(()),
            "the dashboard of b is shown"
        );
    });
    // A requested server that is still configured needs no write.
    let state = |shared: &std::sync::Arc<std::sync::Mutex<Store>>| {
        shared
            .lock()
            .expect("lock")
            .app_state(crate::ACTIVE_SERVER_STATE)
            .expect("state")
    };
    assert_eq!(state(&shared), None);

    let (store, ids) = two_server_store(false, false);
    let (shell, shared) = start_two(cx, store, ids, Some("gone"), &http);
    cx.update(|cx| assert_eq!(shell.read(cx).queries_for_test()[0].name, "a"));
    assert_eq!(state(&shared).as_deref(), Some("a"));
}

#[gpui_kit::test]
fn highlights_of_two_servers_do_not_share_a_cache(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    let (store, ids) = two_server_store(false, false);
    let (shell, _) = start_two(cx, store, ids, Some("a"), &http);
    let key = model::DiffKey {
        change: ChangeNumber(7),
        base: None,
        target: PatchSetRef {
            number: PatchSetNumber(1),
            sha: "sha1".into(),
        },
        path: "f.rs".into(),
        whitespace: model::Whitespace::IgnoreNone,
    };
    cx.update(|cx| {
        let (a, b) = shell.update(cx, |s, cx| {
            let a = s.syntax_of_for_test(&crate::ServerKey::new("a"), cx);
            let b = s.syntax_of_for_test(&crate::ServerKey::new("b"), cx);
            (a.expect("a"), b.expect("b"))
        });
        a.update(cx, |c, _| c.insert(key.clone(), None));
        // The same change number, SHAs and path on the other server.
        assert!(b.update(cx, |c, _| c.get(&key)).is_none());
        assert!(matches!(a.update(cx, |c, _| c.get(&key)), Some(None)));
    });
}

#[gpui_kit::test]
fn back_and_switching_cross_servers_and_land_on_the_last_page(cx: &mut TestAppContext) {
    use gpui_kit::test::TestWindowExt as _;
    let http = RouteHttpClient::new();
    let (store, ids) = two_server_store(false, false);
    let (window, shell, shared) = start_two_in(cx, store, ids, Some("a"), &http);
    let c7 = || Route::Change(ChangeNumber(7));
    let run =
        |cx: &mut TestAppContext,
         f: &dyn Fn(&mut Shell, &mut gpui_kit::Window, &mut gpui_kit::Context<Shell>)| {
            cx.update_window(window, |_, window, cx| {
                shell.update(cx, |s, cx| f(s, window, cx))
            })
            .expect("update");
            cx.run_until_parked();
        };
    assert_eq!(place(cx, &shell), at("a", Route::Dashboard));

    run(cx, &|s, w, cx| s.navigate_for_test(c7(), w, cx));
    assert_eq!(place(cx, &shell), at("a", c7()));

    // First visit of b: its start page, not a's change.
    run(cx, &|s, w, cx| s.switch_to(&key("b"), w, cx));
    assert_eq!(place(cx, &shell), at("b", Route::Dashboard));
    // Change numbers collide; b's 7 is another change.
    run(cx, &|s, w, cx| s.navigate_for_test(c7(), w, cx));
    assert_eq!(place(cx, &shell), at("b", c7()));
    assert!(
        cx.update(|cx| shell.read(cx).change_page_for_test())
            .is_some()
    );

    // Back crosses servers: b's dashboard, then a's change 7.
    run(cx, &|s, w, cx| s.back_for_test(w, cx));
    assert_eq!(place(cx, &shell), at("b", Route::Dashboard));
    run(cx, &|s, w, cx| s.back_for_test(w, cx));
    assert_eq!(place(cx, &shell), at("a", c7()));

    // Switching lands on the server's last page, and is remembered.
    run(cx, &|s, w, cx| s.switch_to(&key("b"), w, cx));
    assert_eq!(place(cx, &shell), at("b", c7()));
    assert_eq!(
        shared
            .lock()
            .expect("lock")
            .app_state(crate::ACTIVE_SERVER_STATE)
            .expect("state")
            .as_deref(),
        Some("b")
    );
    run(cx, &|s, w, cx| s.switch_to(&key("a"), w, cx));
    assert_eq!(place(cx, &shell), at("a", c7()));
    // An unknown server and the active one change nothing.
    run(cx, &|s, w, cx| s.switch_to(&key("nope"), w, cx));
    run(cx, &|s, w, cx| s.switch_to(&key("a"), w, cx));
    assert_eq!(place(cx, &shell), at("a", c7()));

    // Ctrl-2 and Ctrl-1 jump by config order; Ctrl-3 has no server.
    cx.update_window(window, |_, window, cx| window.press("ctrl-2", cx))
        .expect("press");
    assert_eq!(place(cx, &shell), at("b", c7()));
    cx.update_window(window, |_, window, cx| window.press("ctrl-3", cx))
        .expect("press");
    assert_eq!(place(cx, &shell), at("b", c7()));
    cx.update_window(window, |_, window, cx| window.press("ctrl-1", cx))
        .expect("press");
    assert_eq!(place(cx, &shell), at("a", c7()));
}

#[gpui_kit::test]
fn diff_preferences_follow_the_active_server(cx: &mut TestAppContext) {
    use model::DiffPreferences;
    let http = RouteHttpClient::new();
    let (mut store, ids) = two_server_store(false, false);
    let now = "2024-06-15T10:05:00Z".parse().expect("ts");
    for (id, tab) in ids.into_iter().zip([4, 8]) {
        store
            .save_diff_preferences(
                id,
                &DiffPreferences {
                    tab_size: Some(tab),
                    ..Default::default()
                },
                now,
            )
            .expect("prefs");
    }
    let (window, shell, _) = start_two_in(cx, store, ids, Some("a"), &http);
    let tab = |cx: &mut TestAppContext| {
        cx.update(|cx| shell.read(cx).settings().read(cx).resolved().tab_size)
    };
    assert_eq!(tab(cx), 4);
    let switch = |cx: &mut TestAppContext, to: &'static str| {
        cx.update_window(window, |_, window, cx| {
            shell.update(cx, |s, cx| s.switch_to(&key(to), window, cx))
        })
        .expect("update");
        cx.run_until_parked();
    };
    switch(cx, "b");
    assert_eq!(tab(cx), 8);
    switch(cx, "a");
    assert_eq!(tab(cx), 4);
    // Going back in the history changes the server too.
    switch(cx, "b");
    cx.update_window(window, |_, window, cx| {
        shell.update(cx, |s, cx| s.back_for_test(window, cx))
    })
    .expect("update");
    cx.run_until_parked();
    assert_eq!(tab(cx), 4);
}

#[gpui_kit::test]
fn the_shown_pages_use_their_own_servers_outbox_and_requests(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route("/revisions/sha1/drafts", 503, "down");
    // Both servers have a draft on change 7 that cannot go up yet.
    let (store, ids) = two_server_store(true, true);
    let (window, shell, _) = start_two_in(cx, store, ids, Some("a"), &http);
    assert_eq!(counts_of(cx, &shell, "a"), (1, 0));
    assert_eq!(counts_of(cx, &shell, "b"), (1, 0));

    cx.update_window(window, |_, window, cx| {
        shell.update(cx, |s, cx| s.switch_to(&key("b"), window, cx))
    })
    .expect("update");
    cx.run_until_parked();
    // The indicator follows the active server.
    cx.update(|cx| {
        let b = shell.read(cx).outbox_of_for_test(&key("b")).expect("b");
        assert_eq!(shell.read(cx).outbox_for_test(), Some(b));
    });
    let before = http.requested().len();
    http.route("/revisions/sha1/drafts", 201, RECEIPT);
    cx.update(|cx| shell.update(cx, |s, cx| s.retry_drafts_for_test(cx)));
    cx.run_until_parked();
    let uploads: Vec<String> = http.requested()[before..]
        .iter()
        .filter(|r| r.contains("/revisions/sha1/drafts"))
        .cloned()
        .collect();
    assert_eq!(uploads.len(), 1, "{uploads:?}");
    assert!(uploads[0].contains("/gb/"), "{uploads:?}");
    assert_eq!(counts_of(cx, &shell, "b"), (0, 0));
    assert_eq!(counts_of(cx, &shell, "a"), (1, 0));
}

#[gpui_kit::test]
fn a_refused_server_is_marked_and_the_other_one_is_left_alone(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route("/revisions/sha1/drafts", 401, "no");
    // b's draft is refused with 401 while a is shown.
    let (store, ids) = two_server_store(false, true);
    let (shell, _) = start_two(cx, store, ids, Some("a"), &http);
    cx.update(|cx| {
        let s = shell.read(cx);
        assert_eq!(
            s.connection_for_test("b"),
            Some(crate::Connection::Unauthorized)
        );
        assert_ne!(
            s.connection_for_test("a"),
            Some(crate::Connection::Unauthorized)
        );
        // The shown server did not lose its page.
        assert!(s.list_page_for_test().is_some());
    });
}

// ---- the chip and the switcher ----

fn press_key(cx: &mut TestAppContext, window: gpui_kit::AnyWindowHandle, key: &'static str) {
    use gpui_kit::test::TestWindowExt as _;
    cx.update_window(window, |_, window, cx| window.press(key, cx))
        .expect("press");
    cx.run_until_parked();
}

fn switcher_of(
    cx: &mut TestAppContext,
    shell: &Entity<Shell>,
) -> Option<Entity<crate::switcher::ServerSwitcher>> {
    cx.update(|cx| shell.read(cx).switcher_for_test())
}

#[gpui_kit::test]
fn the_chip_shows_with_two_servers_and_not_with_one(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    let (store, ids) = two_server_store(false, false);
    let (window, shell, _) = start_two_in(cx, store, ids, Some("a"), &http);
    let shown = |cx: &mut TestAppContext, shell: &Entity<Shell>| {
        cx.update(|cx| shell.update(cx, |s, cx| s.chip_shown_for_test(cx)))
    };
    assert!(shown(cx, &shell));
    let _ = window;

    let env = env_with(seeded(&[("status:open", &[7])]), &http);
    let startup = Startup {
        env,
        diff: Default::default(),
        start_page: Default::default(),
        filters: Vec::new(),
        servers: vec![setup()],
        active: None,
    };
    let (window, one) = open(cx, |window, cx| {
        cx.new(|cx| Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    assert!(!shown(cx, &one));
    // Ctrl-K does nothing there.
    press_key(cx, window, "ctrl-k");
    assert!(switcher_of(cx, &one).is_none());
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn ctrl_k_opens_the_switcher_typing_filters_and_enter_switches(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    let (store, ids) = two_server_store(false, false);
    let (window, shell, _) = start_two_in(cx, store, ids, Some("a"), &http);
    assert!(switcher_of(cx, &shell).is_none());
    press_key(cx, window, "ctrl-k");
    let popup = switcher_of(cx, &shell).expect("open");
    // Both listed, the cursor on the active one.
    cx.update(|cx| {
        assert_eq!(
            popup.read(cx).listed_for_test(),
            (vec!["a".to_owned(), "b".to_owned()], Some("a".to_owned()))
        )
    });
    // Typing filters by name or address.
    cx.update_window(window, |_, window, cx| {
        popup.update(cx, |p, cx| p.type_for_test("g.example/gb", window, cx))
    })
    .expect("type");
    cx.update(|cx| {
        assert_eq!(
            popup.read(cx).listed_for_test(),
            (vec!["b".to_owned()], Some("b".to_owned()))
        )
    });
    press_key(cx, window, "enter");
    assert!(switcher_of(cx, &shell).is_none());
    assert_eq!(place(cx, &shell), at("b", Route::Dashboard));

    // Arrows move, Esc and a second Ctrl-K close without switching.
    press_key(cx, window, "ctrl-k");
    let popup = switcher_of(cx, &shell).expect("open");
    cx.update(|cx| assert_eq!(popup.read(cx).listed_for_test().1.as_deref(), Some("b")));
    press_key(cx, window, "down");
    cx.update(|cx| assert_eq!(popup.read(cx).listed_for_test().1.as_deref(), Some("a")));
    press_key(cx, window, "escape");
    assert!(switcher_of(cx, &shell).is_none());
    assert_eq!(place(cx, &shell), at("b", Route::Dashboard));
    press_key(cx, window, "ctrl-k");
    assert!(switcher_of(cx, &shell).is_some());
    press_key(cx, window, "ctrl-k");
    assert!(switcher_of(cx, &shell).is_none());

    // Down then Enter picks the other one; Ctrl-1 inside closes it too.
    press_key(cx, window, "ctrl-k");
    press_key(cx, window, "down");
    press_key(cx, window, "enter");
    assert_eq!(place(cx, &shell), at("a", Route::Dashboard));
    press_key(cx, window, "ctrl-k");
    press_key(cx, window, "ctrl-2");
    assert!(switcher_of(cx, &shell).is_none());
    assert_eq!(place(cx, &shell), at("b", Route::Dashboard));
}

#[gpui_kit::test]
fn rows_carry_markers_drafts_and_number_hints(cx: &mut TestAppContext) {
    use crate::Connection;
    let http = RouteHttpClient::new();
    let (store, ids) = two_server_store(false, true);
    // b has a draft that cannot go out (no route answers).
    let (window, shell, _) = start_two_in(cx, store, ids, Some("a"), &http);
    cx.update(|cx| {
        shell.update(cx, |s, _| {
            s.set_connection_for_test("a", Connection::Offline);
            s.set_connection_for_test("b", Connection::Unauthorized);
        })
    });
    press_key(cx, window, "ctrl-k");
    let popup = switcher_of(cx, &shell).expect("open");
    cx.update(|cx| {
        let rows = popup.read(cx).rows_for_test();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].connection, Connection::Offline);
        assert_eq!(rows[1].connection, Connection::Unauthorized);
        assert_eq!(rows[0].position, 0);
        assert_eq!(rows[1].position, 1);
        assert!(rows[0].active && !rows[1].active);
        assert_eq!(rows[0].drafts, 0);
        assert_eq!(rows[1].drafts, 1);
        assert!(rows[1].detail.contains("u") && rows[1].detail.contains(B));
    });
}

// ---- background polling of the servers that are not shown ----

fn attention_page(numbers: &[u64]) -> String {
    let items: Vec<String> = numbers
        .iter()
        .map(|n| {
            format!(
                r#"{{"_number": {n}, "branch": "main", "change_id": "I{n:040}",
                    "created": "2013-02-01 09:59:32.126000000", "id": "p~{n}",
                    "owner": {{"_account_id": 1}}, "project": "p", "status": "NEW",
                    "subject": "change {n}", "updated": "2024-02-01 10:00:00.000000000"}}"#
            )
        })
        .collect();
    format!(")]}}'\n[{}]", items.join(","))
}

/// The attention requests seen so far, `(server path, count)`.
fn polls(http: &RouteHttpClient, server: &str) -> usize {
    http.requested()
        .iter()
        .filter(|r| r.contains("attention") && r.contains(&format!("/{server}/")))
        .count()
}

fn advance(cx: &mut TestAppContext, secs: u64) {
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(secs));
    cx.run_until_parked();
}

fn attention_of(cx: &mut TestAppContext, shell: &Entity<Shell>, key: &str) -> Option<usize> {
    cx.update(|cx| shell.read(cx).attention_for_test(key))
}

fn connection_of(
    cx: &mut TestAppContext,
    shell: &Entity<Shell>,
    key: &str,
) -> Option<crate::Connection> {
    cx.update(|cx| shell.read(cx).connection_for_test(key))
}

#[gpui_kit::test]
fn only_the_inactive_server_is_polled_and_its_count_is_kept(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route("/gb/a/changes/?q=attention", 200, attention_page(&[3, 4]));
    let (store, ids) = two_server_store(false, false);
    let (_, shell, _) = start_two_in(cx, store, ids, Some("a"), &http);
    assert_eq!(polls(&http, "gb"), 1);
    assert_eq!(polls(&http, "ga"), 0);
    assert_eq!(attention_of(cx, &shell, "b"), Some(2));
    assert_eq!(attention_of(cx, &shell, "a"), None);
    assert_eq!(
        connection_of(cx, &shell, "b"),
        Some(crate::Connection::Online)
    );

    // Slowly: nothing before five minutes, then once more.
    advance(cx, 299);
    assert_eq!(polls(&http, "gb"), 1);
    http.route("/gb/a/changes/?q=attention", 200, attention_page(&[3]));
    advance(cx, 2);
    assert_eq!(polls(&http, "gb"), 2);
    assert_eq!(attention_of(cx, &shell, "b"), Some(1));
}

#[gpui_kit::test]
fn the_last_count_comes_from_the_cache_at_start(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    // The server refuses the query, so only the cache can tell.
    http.route("/gb/a/changes/?q=attention", 403, "no");
    let (mut store, ids) = two_server_store(false, false);
    let now = "2024-06-15T10:00:00Z".parse().expect("ts");
    let rows = [crate::testing::summary(5), crate::testing::summary(6)];
    store
        .save_query_page(ids[1], "attention:self", &rows, false, now)
        .expect("cache");
    let (_, shell, _) = start_two_in(cx, store, ids, Some("a"), &http);
    assert_eq!(attention_of(cx, &shell, "b"), Some(2));
    // It answered, if with a refusal: not offline.
    assert_eq!(
        connection_of(cx, &shell, "b"),
        Some(crate::Connection::Online)
    );
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn an_unreachable_server_goes_offline_and_is_asked_less_often(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route("/gb/a/changes/?q=attention", 503, "down");
    let (store, ids) = two_server_store(false, false);
    let (_, shell, _) = start_two_in(cx, store, ids, Some("a"), &http);
    assert_eq!(
        connection_of(cx, &shell, "b"),
        Some(crate::Connection::Offline)
    );
    // The shown server is not touched by the poll.
    assert_ne!(
        connection_of(cx, &shell, "a"),
        Some(crate::Connection::Offline)
    );
    let first = polls(&http, "gb");
    assert!(first >= 1);
    // After one failure the wait doubles: nothing at six minutes ...
    advance(cx, 360);
    assert_eq!(polls(&http, "gb"), first);
    // ... and it recovers when the server is back.
    http.route("/gb/a/changes/?q=attention", 200, attention_page(&[9]));
    advance(cx, 245);
    assert!(polls(&http, "gb") > first);
    assert_eq!(
        connection_of(cx, &shell, "b"),
        Some(crate::Connection::Online)
    );
    assert_eq!(attention_of(cx, &shell, "b"), Some(1));
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_refused_login_is_marked_and_polling_stops(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route("/gb/a/changes/?q=attention", 401, "no");
    let (store, ids) = two_server_store(false, false);
    let (_, shell, _) = start_two_in(cx, store, ids, Some("a"), &http);
    assert_eq!(
        connection_of(cx, &shell, "b"),
        Some(crate::Connection::Unauthorized)
    );
    let n = polls(&http, "gb");
    assert_eq!(n, 1);
    advance(cx, 4 * 3600);
    assert_eq!(polls(&http, "gb"), n);
    // The shown server keeps its page.
    cx.update(|cx| assert!(shell.read(cx).list_page_for_test().is_some()));
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn anonymous_servers_are_not_polled(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    let (store, ids) = two_server_store(false, false);
    let (_, shell, _) = start_two_with(cx, store, ids, Some("a"), Some("b"), &http);
    advance(cx, 3600);
    assert_eq!(polls(&http, "gb"), 0);
    assert_eq!(attention_of(cx, &shell, "b"), None);
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn switching_swaps_which_server_is_polled(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route("/gb/a/changes/?q=attention", 200, attention_page(&[1]));
    http.route(
        "/ga/a/changes/?q=attention",
        200,
        attention_page(&[1, 2, 3]),
    );
    let (store, ids) = two_server_store(false, false);
    let (window, shell, _) = start_two_in(cx, store, ids, Some("a"), &http);
    assert_eq!((polls(&http, "ga"), polls(&http, "gb")), (0, 1));

    cx.update_window(window, |_, window, cx| {
        shell.update(cx, |s, cx| s.switch_to(&key("b"), window, cx))
    })
    .expect("switch");
    cx.run_until_parked();
    // a is polled at once now, b not any more, however long we wait.
    assert_eq!((polls(&http, "ga"), polls(&http, "gb")), (1, 1));
    assert_eq!(attention_of(cx, &shell, "a"), Some(3));
    advance(cx, 3 * 300 + 1);
    assert_eq!(polls(&http, "gb"), 1);
    assert_eq!(polls(&http, "ga"), 4);
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_removed_server_is_never_asked_again(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route("/gb/a/changes/?q=attention", 200, attention_page(&[1]));
    let (store, ids) = two_server_store(false, false);
    let (window, shell, _) = start_two_in(cx, store, ids, Some("a"), &http);
    assert_eq!(polls(&http, "gb"), 1);
    cx.update_window(window, |_, window, cx| {
        shell.update(cx, |s, cx| s.remove_server_for_test(&key("b"), window, cx))
    })
    .expect("remove");
    advance(cx, 3600);
    assert_eq!(polls(&http, "gb"), 1);
}

#[gpui_kit::test]
fn the_switcher_shows_what_waits_on_the_other_servers(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    http.route("/gb/a/changes/?q=attention", 200, attention_page(&[1, 2]));
    http.route("/ga/a/changes/?q=attention", 200, attention_page(&[]));
    let (store, ids) = two_server_store(false, false);
    let (window, shell, _) = start_two_in(cx, store, ids, Some("a"), &http);
    let badge = |cx: &mut TestAppContext| cx.update(|cx| shell.read(cx).chip_badge_for_test());
    // b has two changes waiting: the chip of the shown server says so
    // and b's row has the count; a's own row has none.
    assert_eq!(badge(cx).as_deref(), Some("2"));
    press_key(cx, window, "ctrl-k");
    let popup = switcher_of(cx, &shell).expect("open");
    cx.update(|cx| {
        let rows = popup.read(cx).rows_for_test();
        assert_eq!(rows[0].attention, None);
        assert_eq!(rows[1].attention, Some(2));
    });
    press_key(cx, window, "escape");

    // Shown, b no longer counts as "other"; a was polled and has none,
    // so there is no badge at all.
    cx.update_window(window, |_, window, cx| {
        shell.update(cx, |s, cx| s.switch_to(&key("b"), window, cx))
    })
    .expect("switch");
    cx.run_until_parked();
    assert_eq!(badge(cx), None);
    press_key(cx, window, "ctrl-k");
    let popup = switcher_of(cx, &shell).expect("open");
    cx.update(|cx| {
        let rows = popup.read(cx).rows_for_test();
        assert_eq!(rows[0].attention, Some(0));
        assert_eq!(rows[1].attention, None);
    });
}

#[gpui_kit::test]
fn with_one_server_there_is_no_badge(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    let env = env_with(seeded(&[("status:open", &[7])]), &http);
    let startup = Startup {
        env,
        diff: Default::default(),
        start_page: Default::default(),
        filters: Vec::new(),
        servers: vec![setup()],
        active: None,
    };
    let (_, one) = open(cx, |window, cx| {
        cx.new(|cx| Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| one.read(cx).chip_badge_for_test()), None);
}

#[gpui_kit::test]
fn the_reply_dialog_and_the_menu_name_their_server_only_with_several(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    let (store, ids) = two_server_store(false, false);
    let (window, shell, _) = start_two_in(cx, store, ids, Some("b"), &http);
    let open_dialog = |cx: &mut TestAppContext, shell: &Entity<Shell>, menu: bool| {
        cx.update_window(window, |_, window, cx| {
            shell.update(cx, |s, cx| {
                s.open_dialogs_for_test(crate::testing::detail(7, 1, 1), menu, window, cx)
            })
        })
        .expect("open");
        cx.run_until_parked();
    };
    open_dialog(cx, &shell, false);
    cx.update(|cx| {
        let dialog = shell.read(cx).reply_for_test().expect("reply");
        let name = dialog
            .read(cx)
            .server_badge_for_test()
            .map(|b| b.name.to_string());
        assert_eq!(name.as_deref(), Some("b"));
    });
    press_key(cx, window, "escape");
    open_dialog(cx, &shell, true);
    cx.update(|cx| {
        let dialog = shell.read(cx).menu_for_test().expect("menu");
        assert!(dialog.read(cx).server_badge_for_test().is_some());
    });

    let env = env_with(seeded(&[("status:open", &[7])]), &http);
    let startup = Startup {
        env,
        diff: Default::default(),
        start_page: Default::default(),
        filters: Vec::new(),
        servers: vec![setup()],
        active: None,
    };
    let (window, one) = open(cx, |window, cx| {
        cx.new(|cx| Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        one.update(cx, |s, cx| {
            s.open_dialogs_for_test(crate::testing::detail(7, 1, 1), false, window, cx)
        })
    })
    .expect("open");
    cx.run_until_parked();
    cx.update(|cx| {
        let dialog = one.read(cx).reply_for_test().expect("reply");
        assert!(dialog.read(cx).server_badge_for_test().is_none());
    });
}

// ---- identity marks ----

#[gpui_kit::test]
fn palette_colors_of_the_first_eight_servers_differ_in_both_modes(cx: &mut TestAppContext) {
    use gpui_kit::component::{Theme, ThemeMode};
    cx.update(|cx| {
        gpui_kit::init(cx);
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            Theme::change(mode, None, cx);
            let colors: Vec<_> = (0..8)
                .map(|i| {
                    let c = crate::theme::server_color(
                        i,
                        None,
                        gpui_kit::component::ActiveTheme::theme(cx),
                    );
                    format!("{c:?}")
                })
                .collect();
            let distinct: std::collections::HashSet<_> = colors.iter().collect();
            assert_eq!(distinct.len(), 8, "{mode:?}: {colors:?}");
            // The configured color wins over the palette.
            let own = crate::theme::server_color(
                0,
                Some("#102030"),
                gpui_kit::component::ActiveTheme::theme(cx),
            );
            assert_ne!(format!("{own:?}"), colors[0]);
        }
    });
}

#[gpui_kit::test]
fn pages_carry_the_server_badge_only_with_several_servers(cx: &mut TestAppContext) {
    let http = RouteHttpClient::new();
    let (store, ids) = two_server_store(false, false);
    let (window, shell, _) = start_two_in(cx, store, ids, Some("b"), &http);
    let go = |cx: &mut TestAppContext, route: Route| {
        cx.update_window(window, |_, window, cx| {
            shell.update(cx, |s, cx| s.navigate_for_test(route, window, cx))
        })
        .expect("navigate");
        cx.run_until_parked();
    };
    go(cx, Route::Change(ChangeNumber(7)));
    let badge = cx.update(|cx| {
        let page = shell.read(cx).change_page_for_test().expect("change page");
        page.read(cx).server_badge_for_test().cloned()
    });
    assert_eq!(badge.expect("badge").name.as_ref(), "b");
    go(
        cx,
        Route::Diff(crate::nav::DiffRoute {
            change: ChangeNumber(7),
            base: None,
            target: PatchSetNumber(1),
            path: "f.rs".into(),
            thread: None,
        }),
    );
    cx.update(|cx| {
        let page = shell.read(cx).diff_page_for_test().expect("diff page");
        assert!(page.read(cx).server_badge_for_test().is_some());
    });

    // One server: nothing.
    let env = env_with(seeded(&[("status:open", &[7])]), &http);
    let startup = Startup {
        env,
        diff: Default::default(),
        start_page: Default::default(),
        filters: Vec::new(),
        servers: vec![crate::session::ServerSetup {
            queries: vec![spec("Open", "status:open")],
            ..setup()
        }],
        active: None,
    };
    let (window, shell) = open(cx, |window, cx| {
        cx.new(|cx| Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        shell.update(cx, |s, cx| {
            s.navigate_for_test(Route::Change(ChangeNumber(7)), window, cx)
        })
    })
    .expect("navigate");
    cx.run_until_parked();
    cx.update(|cx| {
        let page = shell.read(cx).change_page_for_test().expect("change page");
        assert!(page.read(cx).server_badge_for_test().is_none());
    });
}

// ---- desktop notifications ----

mod notifying {
    use std::sync::Arc;

    use sync::notify::NotificationSettings;

    use super::*;
    use crate::testing::RecordingNotifier;

    const U1: &str = "2024-02-01 10:00:00.000000000";
    const U2: &str = "2024-02-02 10:00:00.000000000";

    /// Change 7 of the server at `/gb`, owned by account 1, with a
    /// Code-Review `vote` by Bob if there is one.
    fn change(updated: &str, vote: Option<i32>) -> String {
        let labels = vote.map_or_else(String::new, |v| {
            format!(
                r#", "labels": {{"Code-Review": {{"all": [{{"_account_id": 2,
                    "name": "Bob", "value": {v}, "date": "{}"}}]}}}}"#,
                // Recent: the inbox drops what is older than its retention.
                jiff::Timestamp::now().strftime("%Y-%m-%d %H:%M:%S.000000000")
            )
        });
        format!(
            r#"{{"_number": 7, "branch": "main", "change_id": "I{:040}",
                "created": "2024-02-01 09:00:00.000000000", "project": "p",
                "owner": {{"_account_id": 1}}, "status": "NEW", "subject": "Fix it",
                "updated": "{updated}"{labels}}}"#,
            7
        )
    }

    /// The server `b` answers with change 7 as of `updated`.
    fn serve(http: &RouteHttpClient, updated: &str, vote: Option<i32>) {
        http.route_json(
            "/gb/a/accounts/self/detail",
            r#"{"_account_id": 1, "username": "u"}"#,
        );
        http.route_json("/gb/a/changes/7?", &change(updated, vote));
        // The list query of the watch; the attention count has its own.
        http.route_json(
            "/gb/a/changes/?q=is",
            &format!("[{}]", change(updated, vote)),
        );
        http.route_json("/gb/a/changes/?q=attention", "[]");
    }

    /// Watches of the changes list queries seen so far on server `b`.
    fn watch_requests(http: &RouteHttpClient) -> usize {
        http.requested()
            .iter()
            .filter(|r| r.starts_with("/gb/a/changes/?q=is"))
            .count()
    }

    struct Run {
        window: gpui_kit::AnyWindowHandle,
        shell: Entity<Shell>,
        notifier: Arc<RecordingNotifier>,
        store: Arc<std::sync::Mutex<Store>>,
        b: store::ServerId,
    }

    /// Both servers are configured, `b` (shown) signed in, `anonymous`
    /// not. Notifications are on with `settings`.
    fn start(
        cx: &mut TestAppContext,
        http: &RouteHttpClient,
        settings: NotificationSettings,
        anonymous: Option<&str>,
    ) -> Run {
        let (store, ids) = two_server_store(false, false);
        let (window, shell, shared) = start_two_with(cx, store, ids, Some("b"), anonymous, http);
        let notifier = Arc::new(RecordingNotifier::default());
        cx.update_window(window, |_, window, cx| {
            shell.update(cx, |s, cx| {
                s.set_window_active_for_test(false);
                s.set_notifications(notifier.clone(), settings, window, cx)
            })
        })
        .expect("window");
        cx.run_until_parked();
        Run {
            window,
            shell,
            notifier,
            store: shared,
            b: ids[1],
        }
    }

    fn pending(run: &Run) -> usize {
        run.store
            .lock()
            .expect("lock")
            .pending_events(run.b)
            .expect("pending")
            .len()
    }

    #[gpui_kit::test]
    #[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
    fn an_event_is_shown_once_and_acknowledged(cx: &mut TestAppContext) {
        let http = RouteHttpClient::new();
        serve(&http, U1, None);
        let run = start(cx, &http, NotificationSettings::default(), Some("a"));
        // The first pass only caches.
        assert_eq!(watch_requests(&http), 1, "{:?}", http.requested());
        assert!(run.notifier.shown().is_empty());

        serve(&http, U2, Some(-1));
        advance(cx, 301);
        assert_eq!(run.notifier.titles(), ["Code-Review -1 from Bob"]);
        assert_eq!(
            run.notifier.shown()[0].change,
            Some(ChangeNumber(7)),
            "the click opens the change"
        );
        assert_eq!(pending(&run), 0);

        // Nothing new: not shown again.
        advance(cx, 301);
        assert_eq!(run.notifier.shown().len(), 1);
        assert!(watch_requests(&http) >= 3);
    }

    fn inbox(run: &Run) -> Vec<store::InboxItem> {
        run.store
            .lock()
            .expect("lock")
            .inbox_items(None, 100)
            .expect("inbox")
    }

    fn bell(cx: &mut TestAppContext, run: &Run) -> usize {
        cx.update(|cx| run.shell.read(cx).inbox_unread_for_test())
    }

    #[gpui_kit::test]
    #[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
    fn a_shown_event_is_also_kept_in_the_inbox_and_counted_by_the_bell(cx: &mut TestAppContext) {
        let http = RouteHttpClient::new();
        serve(&http, U1, None);
        let run = start(cx, &http, NotificationSettings::default(), Some("a"));
        assert!(inbox(&run).is_empty());
        assert_eq!(bell(cx, &run), 0);

        serve(&http, U2, Some(-1));
        advance(cx, 301);
        assert_eq!(run.notifier.titles(), ["Code-Review -1 from Bob"]);
        let items = inbox(&run);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].server, run.b);
        assert!(items[0].shown_on_os && items[0].read_at.is_none());
        assert_eq!(items[0].rule_title, "Negative Code-Review on my changes");
        assert_eq!(pending(&run), 0);
        assert_eq!(bell(cx, &run), 1);

        // Nothing new: nothing more kept.
        advance(cx, 301);
        assert_eq!(inbox(&run).len(), 1);
    }

    #[gpui_kit::test]
    fn a_suppressed_event_is_only_kept_in_the_inbox(cx: &mut TestAppContext) {
        let http = RouteHttpClient::new();
        serve(&http, U1, None);
        let run = start(cx, &http, NotificationSettings::default(), Some("a"));
        cx.update(|cx| {
            run.shell
                .update(cx, |s, _| s.set_window_active_for_test(true))
        });
        serve(&http, U2, Some(-1));
        advance(cx, 301);
        assert!(run.notifier.shown().is_empty());
        let items = inbox(&run);
        assert_eq!(items.len(), 1);
        assert!(!items[0].shown_on_os);
        assert_eq!(pending(&run), 0);
        assert_eq!(bell(cx, &run), 1);
    }

    #[gpui_kit::test]
    fn a_focused_window_gets_no_popup_but_the_event_is_acknowledged(cx: &mut TestAppContext) {
        let http = RouteHttpClient::new();
        serve(&http, U1, None);
        let run = start(cx, &http, NotificationSettings::default(), Some("a"));
        cx.update(|cx| {
            run.shell
                .update(cx, |s, _| s.set_window_active_for_test(true))
        });
        serve(&http, U2, Some(-1));
        advance(cx, 301);
        assert!(watch_requests(&http) >= 2);
        assert!(run.notifier.shown().is_empty());
        assert_eq!(pending(&run), 0);
    }

    #[gpui_kit::test]
    fn a_focused_window_shows_them_when_the_setting_says_so(cx: &mut TestAppContext) {
        let http = RouteHttpClient::new();
        serve(&http, U1, None);
        let settings = NotificationSettings {
            only_when_unfocused: false,
            ..Default::default()
        };
        let run = start(cx, &http, settings, Some("a"));
        cx.update(|cx| {
            run.shell
                .update(cx, |s, _| s.set_window_active_for_test(true))
        });
        serve(&http, U2, Some(-1));
        advance(cx, 301);
        assert_eq!(run.notifier.shown().len(), 1);
    }

    #[gpui_kit::test]
    #[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
    fn anonymous_servers_are_not_watched(cx: &mut TestAppContext) {
        let http = RouteHttpClient::new();
        serve(&http, U1, None);
        let run = start(cx, &http, NotificationSettings::default(), Some("a"));
        advance(cx, 3600);
        assert!(
            http.requested().iter().all(|r| !r.starts_with("/ga/")),
            "{:?}",
            http.requested()
        );
        cx.update(|cx| {
            let s = run.shell.read(cx);
            assert!(!s.watching_for_test("a"));
            assert!(s.watching_for_test("b"));
        });
    }

    #[gpui_kit::test]
    #[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
    fn the_master_switch_stops_and_starts_the_watch(cx: &mut TestAppContext) {
        let http = RouteHttpClient::new();
        serve(&http, U1, None);
        let off = NotificationSettings {
            enabled: false,
            ..Default::default()
        };
        let run = start(cx, &http, off, Some("a"));
        advance(cx, 3600);
        assert_eq!(watch_requests(&http), 0);
        cx.update(|cx| assert!(!run.shell.read(cx).watching_for_test("b")));

        cx.update_window(run.window, |_, window, cx| {
            run.shell.update(cx, |s, cx| {
                s.set_notification_settings(NotificationSettings::default(), window, cx)
            })
        })
        .expect("window");
        cx.run_until_parked();
        assert_eq!(watch_requests(&http), 1);
        cx.update(|cx| assert!(run.shell.read(cx).watching_for_test("b")));

        cx.update_window(run.window, |_, window, cx| {
            run.shell.update(cx, |s, cx| {
                s.set_notification_settings(
                    NotificationSettings {
                        enabled: false,
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            })
        })
        .expect("window");
        let before = watch_requests(&http);
        advance(cx, 3600);
        assert_eq!(watch_requests(&http), before);
    }

    #[gpui_kit::test]
    fn a_refresh_of_the_change_is_shown_without_waiting_for_a_pass(cx: &mut TestAppContext) {
        let http = RouteHttpClient::new();
        serve(&http, U1, None);
        let run = start(cx, &http, NotificationSettings::default(), Some("a"));
        // Something other than the watch refreshes the change: its events
        // are queued, and the page's refresh event picks them up.
        serve(&http, U2, Some(-1));
        let syncer = cx.update(|cx| {
            run.shell
                .read(cx)
                .session_for_test("b")
                .expect("session")
                .syncer()
                .clone()
        });
        futures::executor::block_on(syncer.refresh_change(ChangeNumber(7))).expect("refresh");
        assert!(run.notifier.shown().is_empty());
        assert_eq!(pending(&run), 1);
        cx.update_window(run.window, |_, window, cx| {
            run.shell.update(cx, |s, cx| {
                s.navigate_for_test(Route::Change(ChangeNumber(7)), window, cx)
            })
        })
        .expect("window");
        cx.run_until_parked();
        assert_eq!(run.notifier.titles(), ["Code-Review -1 from Bob"]);
        assert_eq!(pending(&run), 0);
    }

    #[gpui_kit::test]
    #[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
    fn clicking_a_notification_opens_its_change_on_its_server(cx: &mut TestAppContext) {
        let http = RouteHttpClient::new();
        serve(&http, U1, None);
        let run = start(cx, &http, NotificationSettings::default(), Some("a"));
        serve(&http, U2, Some(-1));
        advance(cx, 301);
        assert_eq!(run.notifier.shown().len(), 1);

        // Looking at the other server's dashboard when it is clicked.
        cx.update_window(run.window, |_, window, cx| {
            run.shell
                .update(cx, |s, cx| s.switch_to(&key("a"), window, cx))
        })
        .expect("window");
        assert_eq!(place(cx, &run.shell), at("a", Route::Dashboard));
        run.notifier.click(0);
        cx.run_until_parked();
        assert_eq!(
            place(cx, &run.shell),
            at("b", Route::Change(ChangeNumber(7)))
        );
    }
}

// ---- the notification inbox ----

mod inbox {
    use std::{sync::Arc, time::Duration};

    use jiff::Timestamp;
    use model::{ChangeNumber, Event, EventKind};
    use store::{InboxItem, NewInboxItem};

    use super::*;
    use crate::pages::inbox::InboxPage;

    struct Run {
        window: gpui_kit::AnyWindowHandle,
        shell: Entity<Shell>,
        store: Arc<std::sync::Mutex<Store>>,
        ids: [store::ServerId; 2],
    }

    /// Seconds before now.
    fn ago(secs: u64) -> Timestamp {
        Timestamp::now()
            .checked_sub(Duration::from_secs(secs))
            .expect("ts")
    }

    fn new_item(server: store::ServerId, change: u64, at: Timestamp) -> NewInboxItem {
        NewInboxItem {
            event: Event {
                server,
                change: ChangeNumber(change),
                project: "proj".into(),
                subject: format!("Subject {change}"),
                owner: None,
                actor: None,
                at,
                kind: EventKind::Merged,
                subject_account: None,
            },
            rule_title: "Merged".into(),
            mention: false,
            shown_on_os: change.is_multiple_of(2),
        }
    }

    /// Server `b` is shown. The inbox has, newest first: change 10 of
    /// `a` (just now), change 11 of `b` (a minute ago) and change 12
    /// of `a` (five days ago); `read` of them are read, from the top.
    fn start(cx: &mut TestAppContext, read: usize) -> Run {
        let (mut store, ids) = two_server_store(false, false);
        let now = Timestamp::now();
        store
            .add_inbox(
                ids[0],
                &[
                    new_item(ids[0], 10, ago(1)),
                    new_item(ids[0], 12, ago(5 * 86_400)),
                ],
                now,
            )
            .expect("a");
        store
            .add_inbox(ids[1], &[new_item(ids[1], 11, ago(60))], now)
            .expect("b");
        let first = store.inbox_items(None, 10).expect("items");
        let done: Vec<_> = first.iter().take(read).map(|i| i.id).collect();
        store.mark_inbox_read(&done, now).expect("read");
        let http = RouteHttpClient::new();
        let (window, shell, shared) = start_two_in(cx, store, ids, Some("b"), &http);
        Run {
            window,
            shell,
            store: shared,
            ids,
        }
    }

    fn press(cx: &mut TestAppContext, run: &Run, key: &'static str) {
        press_key(cx, run.window, key);
    }

    fn open(cx: &mut TestAppContext, run: &Run) -> Entity<InboxPage> {
        press(cx, run, "ctrl-shift-n");
        cx.update(|cx| {
            run.shell
                .read(cx)
                .inbox_page_for_test()
                .expect("inbox page")
        })
    }

    fn bell(cx: &mut TestAppContext, run: &Run) -> usize {
        cx.update(|cx| run.shell.read(cx).inbox_unread_for_test())
    }

    fn stored(run: &Run) -> Vec<InboxItem> {
        run.store
            .lock()
            .expect("lock")
            .inbox_items(None, 100)
            .expect("items")
    }

    /// Change numbers and read marks of the rows of the page.
    fn rows(cx: &mut TestAppContext, page: &Entity<InboxPage>) -> Vec<(u64, bool)> {
        cx.update(|cx| {
            page.read(cx)
                .items_for_test()
                .iter()
                .map(|i| (i.event.change.0, i.read_at.is_some()))
                .collect()
        })
    }

    fn cursor(cx: &mut TestAppContext, page: &Entity<InboxPage>) -> usize {
        cx.update(|cx| page.read(cx).cursor_for_test())
    }

    fn route(cx: &mut TestAppContext, run: &Run) -> Route {
        cx.update(|cx| run.shell.read(cx).route().clone())
    }

    #[gpui_kit::test]
    fn the_bell_counts_unread_notifications_of_all_servers_from_the_start(cx: &mut TestAppContext) {
        let run = start(cx, 0);
        assert_eq!(bell(cx, &run), 3);
        let run = start(cx, 1);
        assert_eq!(bell(cx, &run), 2);
        let run = start(cx, 3);
        assert_eq!(bell(cx, &run), 0);
    }

    #[gpui_kit::test]
    fn the_key_opens_the_inbox_newest_first_in_day_groups_and_marks_nothing_read(
        cx: &mut TestAppContext,
    ) {
        let run = start(cx, 0);
        let page = open(cx, &run);
        assert_eq!(route(cx, &run), Route::Inbox);
        assert_eq!(rows(cx, &page), [(10, false), (11, false), (12, false)]);
        cx.update(|cx| {
            let groups = page.read(cx).groups_for_test();
            assert_eq!(groups.len(), 2, "{groups:?}");
            assert_eq!(groups[0], ("Today".to_owned(), 2));
            assert_eq!(groups[1].1, 1);
        });
        assert_eq!(cursor(cx, &page), 0);
        // Opening it resets the bell, but the rows stay unread.
        assert_eq!(bell(cx, &run), 0);
        assert!(stored(&run).iter().all(|i| i.read_at.is_none()));
    }

    #[gpui_kit::test]
    fn a_new_event_bumps_the_bell_again_unless_the_page_is_open(cx: &mut TestAppContext) {
        let run = start(cx, 0);
        assert_eq!(bell(cx, &run), 3);
        let page = open(cx, &run);
        assert_eq!(bell(cx, &run), 0);

        // An event arrives while the page is open: shown, and no badge.
        let item = |change: u64| store::NewInboxItem {
            event: model::Event {
                server: run.ids[0],
                change: ChangeNumber(change),
                project: "p".into(),
                subject: "s".into(),
                owner: None,
                actor: None,
                at: Timestamp::now(),
                kind: model::EventKind::Merged,
                subject_account: None,
            },
            rule_title: "r".into(),
            mention: false,
            shown_on_os: false,
        };
        run.store
            .lock()
            .expect("lock")
            .add_inbox(run.ids[0], &[item(40)], Timestamp::now())
            .expect("add");
        cx.update(|cx| run.shell.update(cx, |s, cx| s.inbox_reload_for_test(cx)));
        cx.run_until_parked();
        assert_eq!(rows(cx, &page).len(), 4);
        assert_eq!(bell(cx, &run), 0);
        assert!(rows(cx, &page).iter().all(|r| !r.1));

        // After leaving, the next arrival counts.
        press(cx, &run, "u");
        assert_eq!(route(cx, &run), Route::Dashboard);
        run.store
            .lock()
            .expect("lock")
            .add_inbox(run.ids[0], &[item(41)], Timestamp::now())
            .expect("add");
        cx.update(|cx| run.shell.update(cx, |s, cx| s.inbox_reload_for_test(cx)));
        cx.run_until_parked();
        assert_eq!(bell(cx, &run), 1);
    }

    #[gpui_kit::test]
    fn j_and_k_move_the_cursor_and_stop_at_the_ends(cx: &mut TestAppContext) {
        let run = start(cx, 0);
        let page = open(cx, &run);
        press(cx, &run, "k");
        assert_eq!(cursor(cx, &page), 0);
        press(cx, &run, "j");
        press(cx, &run, "down");
        assert_eq!(cursor(cx, &page), 2);
        press(cx, &run, "j");
        assert_eq!(cursor(cx, &page), 2);
        press(cx, &run, "up");
        assert_eq!(cursor(cx, &page), 1);
    }

    #[gpui_kit::test]
    fn enter_opens_the_change_on_its_server_and_marks_it_read(cx: &mut TestAppContext) {
        let run = start(cx, 0);
        let page = open(cx, &run);
        // The page was opened on `b`; the first row belongs to `a`.
        press(cx, &run, "enter");
        assert_eq!(
            place(cx, &run.shell),
            at("a", Route::Change(ChangeNumber(10)))
        );
        // Seen on opening the page, so reading a row changes nothing.
        assert_eq!(bell(cx, &run), 0);
        let read: Vec<_> = stored(&run).iter().map(|i| i.read_at.is_some()).collect();
        assert_eq!(read, [true, false, false]);
        assert_eq!(rows(cx, &page)[0], (10, true));

        // Back is the inbox again, which shows what is stored.
        press(cx, &run, "alt-left");
        assert_eq!(route(cx, &run), Route::Inbox);
        let page = cx.update(|cx| run.shell.read(cx).inbox_page_for_test().expect("page"));
        assert_eq!(rows(cx, &page), [(10, true), (11, false), (12, false)]);
        // The second row is on `b`.
        press(cx, &run, "j");
        press(cx, &run, "enter");
        assert_eq!(
            place(cx, &run.shell),
            at("b", Route::Change(ChangeNumber(11)))
        );
        assert_eq!(bell(cx, &run), 0);
    }

    #[gpui_kit::test]
    fn r_toggles_read_and_the_bell_follows(cx: &mut TestAppContext) {
        let run = start(cx, 0);
        let page = open(cx, &run);
        press(cx, &run, "r");
        assert_eq!(rows(cx, &page)[0], (10, true));
        // The bell was reset on opening; marking read or unread does
        // not bring a seen row back into the count.
        assert_eq!(bell(cx, &run), 0);
        assert!(stored(&run)[0].read_at.is_some());
        press(cx, &run, "r");
        assert_eq!(rows(cx, &page)[0], (10, false));
        assert_eq!(bell(cx, &run), 0);
        assert!(stored(&run)[0].read_at.is_none());
        // The cursor stayed.
        assert_eq!(cursor(cx, &page), 0);
    }

    #[gpui_kit::test]
    fn shift_r_and_shift_a_mark_everything_read(cx: &mut TestAppContext) {
        let run = start(cx, 1);
        let page = open(cx, &run);
        press(cx, &run, "shift-r");
        assert!(rows(cx, &page).iter().all(|r| r.1));
        assert_eq!(bell(cx, &run), 0);
        assert!(stored(&run).iter().all(|i| i.read_at.is_some()));
        press(cx, &run, "r");
        assert_eq!(bell(cx, &run), 0);
        press(cx, &run, "shift-a");
        assert_eq!(bell(cx, &run), 0);
    }

    #[gpui_kit::test]
    fn d_dismisses_the_row_and_shift_d_asks_before_clearing_the_read_ones(cx: &mut TestAppContext) {
        let run = start(cx, 1);
        let page = open(cx, &run);
        press(cx, &run, "j");
        press(cx, &run, "j");
        press(cx, &run, "d");
        assert_eq!(rows(cx, &page), [(10, true), (11, false)]);
        // The cursor moved up onto what is last now.
        assert_eq!(cursor(cx, &page), 1);
        assert_eq!(stored(&run).len(), 2);
        assert_eq!(bell(cx, &run), 0);

        // Shift-D: the first press only asks.
        press(cx, &run, "shift-d");
        assert_eq!(rows(cx, &page).len(), 2);
        cx.update(|cx| {
            let (text, error) = page.read(cx).hint_for_test().expect("hint");
            assert!(!error && text.contains("again"), "{text}");
        });
        // Another key cancels the question.
        press(cx, &run, "k");
        press(cx, &run, "shift-d");
        assert_eq!(rows(cx, &page).len(), 2);
        press(cx, &run, "shift-d");
        assert_eq!(rows(cx, &page), [(11, false)]);
        assert_eq!(stored(&run).len(), 1);
        assert_eq!(bell(cx, &run), 0);
    }

    #[gpui_kit::test]
    fn an_empty_inbox_says_so_and_the_keys_do_nothing(cx: &mut TestAppContext) {
        let run = start(cx, 0);
        run.store
            .lock()
            .expect("lock")
            .delete_read_inbox(None)
            .expect("clear");
        run.store
            .lock()
            .expect("lock")
            .mark_all_inbox_read(None, Timestamp::now())
            .expect("read");
        run.store
            .lock()
            .expect("lock")
            .delete_read_inbox(None)
            .expect("clear");
        let page = open(cx, &run);
        cx.update(|cx| assert!(page.read(cx).loaded_for_test()));
        assert!(rows(cx, &page).is_empty());
        for key in ["j", "enter", "r", "d", "shift-d", "shift-r"] {
            press(cx, &run, key);
        }
        assert_eq!(route(cx, &run), Route::Inbox);
        assert_eq!(bell(cx, &run), 0);
    }

    #[gpui_kit::test]
    fn u_and_escape_leave_and_history_goes_back_and_forward(cx: &mut TestAppContext) {
        let run = start(cx, 0);
        open(cx, &run);
        press(cx, &run, "u");
        assert_eq!(route(cx, &run), Route::Dashboard);
        press(cx, &run, "alt-right");
        assert_eq!(route(cx, &run), Route::Inbox);
        press(cx, &run, "escape");
        assert_eq!(route(cx, &run), Route::Dashboard);
        press(cx, &run, "alt-right");
        assert_eq!(route(cx, &run), Route::Inbox);
        press(cx, &run, "alt-left");
        assert_eq!(route(cx, &run), Route::Dashboard);
    }

    #[gpui_kit::test]
    fn the_keys_do_not_fire_behind_a_modal_dialog(cx: &mut TestAppContext) {
        let run = start(cx, 0);
        press(cx, &run, "g");
        cx.update(|cx| assert!(run.shell.read(cx).goto_for_test().is_some()));
        press(cx, &run, "ctrl-shift-n");
        assert_eq!(route(cx, &run), Route::Dashboard);
        cx.update(|cx| assert!(run.shell.read(cx).goto_for_test().is_some()));
        press(cx, &run, "escape");

        // On the inbox page the dialog takes the row keys too.
        let page = open(cx, &run);
        press(cx, &run, "g");
        press(cx, &run, "r");
        press(cx, &run, "d");
        assert_eq!(rows(cx, &page).len(), 3);
        assert_eq!(bell(cx, &run), 0);
        assert!(stored(&run).iter().all(|i| i.read_at.is_none()));
    }

    #[gpui_kit::test]
    fn the_bell_button_opens_the_inbox(cx: &mut TestAppContext) {
        let run = start(cx, 0);
        cx.update_window(run.window, |_, window, cx| {
            run.shell
                .update(cx, |s, cx| s.open_inbox_for_test(window, cx))
        })
        .expect("window");
        cx.run_until_parked();
        assert_eq!(route(cx, &run), Route::Inbox);
    }
}
