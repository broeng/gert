//! View tests on GPUI's `TestAppContext`. No network: sessions either
//! cannot sync or talk to a scripted fake.

use std::sync::{Arc, Mutex};

use gerrit_api::fake::FakeHttpClient;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, Entity, TestAppContext, WindowOptions, px, size};
use model::{ChangeDetail, ChangeNumber};
use store::{ServerId, Store};

use crate::{
    list_model::PAGE_SIZE,
    login::tests::FakeBackend,
    nav::Route,
    pages::list::{ChangeListPage, ListKind},
    session::{Env, SavedQuery, ServerSetup, Session, Startup},
    shell::Shell,
    testing::summary,
};

pub(crate) const URL: &str = "https://g.example.com";

pub(crate) fn setup() -> ServerSetup {
    ServerSetup {
        key: crate::session::ServerKey::new("test"),
        name: "Test".into(),
        base_url: URL.into(),
        username: None,
        color: None,
        server: ServerId(1),
        queries: Vec::new(),
        bookmarks: Vec::new(),
        projects: Vec::new(),
    }
}

pub(crate) fn env_with(
    store: Store,
    http: &(impl gerrit_api::HttpClient + Clone + 'static),
) -> Env {
    Env {
        http: Arc::new(http.clone()),
        store: Arc::new(Mutex::new(store)),
        backend: Arc::new(FakeBackend::default()),
    }
}

/// A store whose server 1 has the given queries cached.
fn seeded(queries: &[(&str, &[u64])]) -> Store {
    let mut store = Store::open_in_memory().expect("in-memory store");
    let server = store.server_id(URL).expect("server id");
    let now = "2024-06-15T10:05:00Z".parse().expect("timestamp");
    for (q, numbers) in queries {
        let rows: Vec<_> = numbers.iter().map(|n| summary(*n)).collect();
        store
            .save_query_page(server, q, &rows, false, now)
            .expect("save query");
    }
    store
}

fn spec(name: &str, query: &str) -> SavedQuery {
    SavedQuery {
        name: name.into(),
        query: query.into(),
    }
}

pub(crate) fn open<V: gpui_kit::Render + 'static>(
    cx: &mut TestAppContext,
    build: impl FnOnce(&mut gpui_kit::Window, &mut gpui_kit::App) -> Entity<V>,
) -> (gpui_kit::AnyWindowHandle, Entity<V>) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::init(cx);
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(gpui_kit::WindowBounds::Windowed(gpui_kit::Bounds {
                    origin: Default::default(),
                    size: size(px(1100.), px(700.)),
                })),
                ..Default::default()
            },
            cx,
            build,
        )
        .expect("open window")
    })
}

#[gpui_kit::test]
fn dashboard_shows_cached_rows_and_j_k_cross_sections(cx: &mut TestAppContext) {
    let env = env_with(
        seeded(&[("a", &[1, 2]), ("b", &[]), ("c", &[3])]),
        &FakeHttpClient::new(),
    );
    // No credentials yet: the session can read the cache but not sync.
    let session = Session::new(&env, &setup(), None).expect("session");
    let specs = vec![spec("A", "a"), spec("B", "b"), spec("C", "c")];
    let (window, page) = open(cx, |window, cx| {
        let page =
            cx.new(|cx| ChangeListPage::new(session, ListKind::Dashboard, specs, None, true, cx));
        let focus = page.read(cx).focus_handle_for_test();
        window.focus(&focus, cx);
        page
    });
    cx.run_until_parked();

    cx.update(|cx| {
        let p = page.read(cx);
        assert_eq!(p.visible_rows(0).len(), 2);
        assert!(p.visible_rows(1).is_empty());
        assert_eq!(p.visible_rows(2)[0].number, ChangeNumber(3));
        assert_eq!(p.selected_number(), Some(ChangeNumber(1)));
    });

    let press = |cx: &mut TestAppContext, key: &'static str| {
        cx.update_window(window, |_, window, cx| window.press(key, cx))
            .expect("press");
    };
    press(cx, "j");
    press(cx, "j");
    cx.update(|cx| assert_eq!(page.read(cx).selected_number(), Some(ChangeNumber(3))));
    press(cx, "k");
    cx.update(|cx| assert_eq!(page.read(cx).selected_number(), Some(ChangeNumber(2))));
}

#[gpui_kit::test]
fn bookmark_keys_open_the_bookmark_and_b_b_bookmarks_a_search(cx: &mut TestAppContext) {
    use crate::pages::list::ListEvent;
    let env = env_with(seeded(&[("a", &[1])]), &FakeHttpClient::new());
    let session = Session::new(&env, &setup(), None).expect("session");
    let (window, page) = open(cx, |window, cx| {
        let page = cx.new(|cx| {
            ChangeListPage::new(
                session,
                ListKind::Search,
                vec![spec("S", "a")],
                None,
                true,
                cx,
            )
        });
        page.update(cx, |p, cx| {
            p.set_bookmarks(vec![spec("Mine", "owner:self"), spec("Old", "a")], cx)
        });
        let focus = page.read(cx).focus_handle_for_test();
        window.focus(&focus, cx);
        page
    });
    cx.run_until_parked();
    let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = events.clone();
    let _sub = cx.update(|cx| {
        cx.subscribe(&page, move |_, ev: &ListEvent, _| match ev {
            ListEvent::Search(q) => seen.borrow_mut().push(format!("search {q}")),
            ListEvent::SaveBookmarks(l) => seen.borrow_mut().push(format!(
                "save {}",
                l.iter()
                    .map(|b| b.name.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            )),
            _ => {}
        })
    });
    let press = |cx: &mut TestAppContext, key: &'static str| {
        cx.update_window(window, |_, window, cx| window.press(key, cx))
            .expect("press");
    };

    press(cx, "b");
    press(cx, "1");
    // Past the end of the list: nothing.
    press(cx, "b");
    press(cx, "5");
    assert_eq!(*events.borrow(), ["search owner:self"]);

    // This search is bookmarked already: `b b` takes the bookmark away.
    press(cx, "b");
    press(cx, "b");
    assert_eq!(
        events.borrow().last().map(String::as_str),
        Some("save Mine")
    );
}

#[gpui_kit::test]
fn a_new_bookmark_needs_a_label(cx: &mut TestAppContext) {
    use crate::pages::list::ListEvent;
    let env = env_with(seeded(&[("a", &[1])]), &FakeHttpClient::new());
    let session = Session::new(&env, &setup(), None).expect("session");
    let (window, page) = open(cx, |window, cx| {
        let page = cx.new(|cx| {
            ChangeListPage::new(
                session,
                ListKind::Search,
                vec![spec("S", "a")],
                None,
                true,
                cx,
            )
        });
        let focus = page.read(cx).focus_handle_for_test();
        window.focus(&focus, cx);
        page
    });
    cx.run_until_parked();
    let saved = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = saved.clone();
    let _sub = cx.update(|cx| {
        cx.subscribe(&page, move |_, ev: &ListEvent, _| {
            if let ListEvent::SaveBookmarks(l) = ev {
                seen.borrow_mut().push(l.clone());
            }
        })
    });
    let press = |cx: &mut TestAppContext, key: &'static str| {
        cx.update_window(window, |_, window, cx| window.press(key, cx))
            .expect("press");
    };

    // Enter on an empty label does not save; Esc closes the box.
    press(cx, "b");
    press(cx, "b");
    press(cx, "enter");
    assert!(saved.borrow().is_empty());
    press(cx, "escape");
    press(cx, "b");
    press(cx, "b");
    cx.simulate_input(window, "Wip");
    press(cx, "enter");
    assert_eq!(*saved.borrow(), [vec![spec("Wip", "a")]]);
}

#[gpui_kit::test]
fn the_list_follows_the_window_height_when_it_is_resized(cx: &mut TestAppContext) {
    let numbers: Vec<u64> = (1..=60).collect();
    let env = env_with(seeded(&[("a", &numbers)]), &FakeHttpClient::new());
    let session = Session::new(&env, &setup(), None).expect("session");
    let specs = vec![spec("A", "a")];
    let (window, page) = open(cx, |_, cx| {
        cx.new(|cx| ChangeListPage::new(session, ListKind::Dashboard, specs, None, true, cx))
    });
    cx.run_until_parked();
    let scroll = cx.update(|cx| page.read(cx).scroll_for_test());
    let visual = gpui_kit::VisualTestContext::from_window(window, cx);

    // The test window is 700px high; the page is the root, with no footer.
    assert_eq!(scroll.bounds().size.height, px(700.));
    visual.simulate_resize(size(px(1400.), px(1000.)));
    cx.run_until_parked();
    assert_eq!(scroll.bounds().size.height, px(1000.));
    assert_eq!(scroll.bounds().size.width, px(1400.));
    visual.simulate_resize(size(px(900.), px(300.)));
    cx.run_until_parked();
    assert_eq!(scroll.bounds().size.height, px(300.));
    // A dashboard section is capped, the rows beyond it are for the search.
    cx.update(|cx| assert_eq!(page.read(cx).visible_rows(0).len(), 25));
}

#[gpui_kit::test]
fn shell_opens_a_change_from_the_dashboard_and_goes_up(cx: &mut TestAppContext) {
    let mut store = seeded(&[("status:open", &[7, 8])]);
    let detail = ChangeDetail {
        summary: summary(8),
        current_revision: Some("abc123".into()),
        current_patch_set: Some(model::PatchSetNumber(2)),
        commit_message: Some("Subject\n\nBody".into()),
        reviewers: vec![],
        cc: vec![],
        revisions: vec![],
        messages: vec![],
        submit_requirements: vec![],
        votes: Default::default(),
        attention: Default::default(),
        permitted_labels: Default::default(),
        removable_reviewers: Default::default(),
        label_values: Default::default(),
        change_actions: Default::default(),
        revision_actions: Default::default(),
    };
    store
        .save_change_detail(
            ServerId(1),
            &detail,
            "2024-06-15T10:05:00Z".parse().expect("ts"),
        )
        .expect("save detail");
    let env = env_with(store, &FakeHttpClient::new());
    let startup = Startup {
        env,
        diff: Default::default(),
        start_page: Default::default(),
        filters: Vec::new(),
        servers: vec![crate::session::ServerSetup {
            queries: vec![spec("Open", "status:open")],
            bookmarks: Vec::new(),
            ..setup()
        }],
        active: None,
    };
    let (window, shell) = open(cx, |window, cx| {
        cx.new(|cx| Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    cx.update(|cx| assert_eq!(shell.read(cx).route(), &Route::Dashboard));

    let press = |cx: &mut TestAppContext, key: &'static str| {
        cx.update_window(window, |_, window, cx| window.press(key, cx))
            .expect("press");
    };
    press(cx, "j");
    press(cx, "enter");
    cx.run_until_parked();
    cx.update(|cx| assert_eq!(shell.read(cx).route(), &Route::Change(ChangeNumber(8))));

    press(cx, "u");
    cx.run_until_parked();
    cx.update(|cx| assert_eq!(shell.read(cx).route(), &Route::Dashboard));
}

#[gpui_kit::test]
fn unauthorized_sends_the_user_to_login(cx: &mut TestAppContext) {
    let http = FakeHttpClient::new();
    http.push_response(401, "denied");
    let env = env_with(seeded(&[("status:open", &[1])]), &http);
    let startup = Startup {
        env,
        diff: Default::default(),
        start_page: Default::default(),
        filters: Vec::new(),
        servers: vec![crate::session::ServerSetup {
            queries: vec![spec("Open", "status:open")],
            bookmarks: Vec::new(),
            ..setup()
        }],
        active: None,
    };
    let (_window, shell) = open(cx, |window, cx| {
        cx.new(|cx| Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    cx.update(|cx| assert_eq!(shell.read(cx).route(), &Route::Login));
}

#[gpui_kit::test]
fn search_pages_through_cached_rows_with_n_and_p(cx: &mut TestAppContext) {
    let numbers: Vec<u64> = (1..=(PAGE_SIZE as u64 + 5)).collect();
    let env = env_with(seeded(&[("owner:self", &numbers)]), &FakeHttpClient::new());
    let session = Session::new(&env, &setup(), None).expect("session");
    let (window, page) = open(cx, |window, cx| {
        let page = cx.new(|cx| {
            ChangeListPage::new(
                session,
                ListKind::Search,
                vec![spec("Search", "owner:self")],
                None,
                true,
                cx,
            )
        });
        let focus = page.read(cx).focus_handle_for_test();
        window.focus(&focus, cx);
        page
    });
    cx.run_until_parked();
    let press = |cx: &mut TestAppContext, key: &'static str| {
        cx.update_window(window, |_, window, cx| window.press(key, cx))
            .expect("press");
    };
    cx.update(|cx| assert_eq!(page.read(cx).visible_rows(0).len(), PAGE_SIZE));
    press(cx, "n");
    cx.update(|cx| {
        let p = page.read(cx);
        assert_eq!(p.visible_rows(0).len(), 5);
        assert_eq!(
            p.selected_number(),
            Some(ChangeNumber(PAGE_SIZE as u64 + 1))
        );
    });
    press(cx, "p");
    cx.update(|cx| assert_eq!(page.read(cx).selected_number(), Some(ChangeNumber(1))));
}

#[gpui_kit::test]
fn cached_server_diff_preferences_sit_under_the_local_overrides(cx: &mut TestAppContext) {
    use model::{DiffPreferences, DiffViewMode};
    let mut store = seeded(&[("status:open", &[1])]);
    store
        .save_diff_preferences(
            ServerId(1),
            &DiffPreferences {
                tab_size: Some(4),
                line_length: Some(120),
                view_mode: Some(DiffViewMode::Unified),
                ..Default::default()
            },
            "2024-06-15T10:05:00Z".parse().expect("ts"),
        )
        .expect("save prefs");
    let startup = Startup {
        env: env_with(store, &FakeHttpClient::new()),
        diff: crate::LocalDiff {
            preferences: DiffPreferences {
                tab_size: Some(2),
                ..Default::default()
            },
            font_family: Some("Iosevka".into()),
        },
        start_page: Default::default(),
        filters: Vec::new(),
        servers: vec![crate::session::ServerSetup {
            queries: vec![spec("Open", "status:open")],
            bookmarks: Vec::new(),
            ..setup()
        }],
        active: None,
    };
    let (_window, shell) = open(cx, |window, cx| {
        cx.new(|cx| Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    cx.update(|cx| {
        let settings = shell.read(cx).settings().read(cx);
        let r = settings.resolved();
        assert_eq!(r.tab_size, 2, "local override wins");
        assert_eq!(r.line_length, 120, "server value fills the gap");
        assert_eq!(r.view_mode, DiffViewMode::Unified);
        assert_eq!(settings.font_family().as_deref(), Some("Iosevka"));
    });
}

// --- drafts that are not on the server ----------------------------------

mod drafts {
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
    fn each_server_has_its_own_outbox_and_one_that_is_not_shown_still_uploads(
        cx: &mut TestAppContext,
    ) {
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
        let run = |cx: &mut TestAppContext, f: &dyn Fn(&mut Shell, &mut gpui_kit::Window, &mut gpui_kit::Context<Shell>)| {
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
    fn anonymous_servers_are_not_polled(cx: &mut TestAppContext) {
        let http = RouteHttpClient::new();
        let (store, ids) = two_server_store(false, false);
        let (_, shell, _) = start_two_with(cx, store, ids, Some("a"), Some("b"), &http);
        advance(cx, 3600);
        assert_eq!(polls(&http, "gb"), 0);
        assert_eq!(attention_of(cx, &shell, "b"), None);
    }

    #[gpui_kit::test]
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
            let (window, shell, shared) =
                start_two_with(cx, store, ids, Some("b"), anonymous, http);
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
        fn a_shown_event_is_also_kept_in_the_inbox_and_counted_by_the_bell(
            cx: &mut TestAppContext,
        ) {
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
        fn the_bell_counts_unread_notifications_of_all_servers_from_the_start(
            cx: &mut TestAppContext,
        ) {
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
        fn d_dismisses_the_row_and_shift_d_asks_before_clearing_the_read_ones(
            cx: &mut TestAppContext,
        ) {
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
}

// --- the settings page ---------------------------------------------------

mod settings_page {
    use std::sync::Arc;

    use gerrit_api::fake::FakeHttpClient;
    use gpui_kit::component::input::InputState;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
    use model::ChangeNumber;

    use super::{env_with, open, seeded, setup, spec};
    use crate::{
        filters::{FileFilter, FilterAction},
        login::tests::FakeBackend,
        nav::Route,
        pages::settings::SettingsTab,
        session::{Env, Startup, StartupPage},
        shell::Shell,
    };

    struct Fixture {
        window: AnyWindowHandle,
        shell: Entity<Shell>,
        backend: Arc<FakeBackend>,
    }

    fn fixture(cx: &mut TestAppContext, fail: bool) -> Fixture {
        fixture_at(cx, fail, StartupPage::Dashboard)
    }

    fn fixture_at(cx: &mut TestAppContext, fail: bool, start_page: StartupPage) -> Fixture {
        fixture_with(cx, fail, start_page, Vec::new())
    }

    fn fixture_with(
        cx: &mut TestAppContext,
        fail: bool,
        start_page: StartupPage,
        filters: Vec<FileFilter>,
    ) -> Fixture {
        fixture_full(cx, fail, start_page, filters, Vec::new())
    }

    fn fixture_full(
        cx: &mut TestAppContext,
        fail: bool,
        start_page: StartupPage,
        filters: Vec<FileFilter>,
        bookmarks: Vec<crate::session::SavedQuery>,
    ) -> Fixture {
        fixture_all(cx, fail, start_page, filters, bookmarks, Vec::new(), None)
    }

    /// `username`: `None` is an anonymous server.
    fn fixture_all(
        cx: &mut TestAppContext,
        fail: bool,
        start_page: StartupPage,
        filters: Vec<FileFilter>,
        bookmarks: Vec<crate::session::SavedQuery>,
        projects: Vec<crate::session::ProjectSettings>,
        username: Option<&str>,
    ) -> Fixture {
        let backend = Arc::new(FakeBackend {
            fail,
            password: username.map(|_| "pw".to_owned()),
            ..Default::default()
        });
        let env = Env {
            backend: backend.clone(),
            ..env_with(
                seeded(&[("status:open", &[1]), ("owner:self", &[2])]),
                &FakeHttpClient::new(),
            )
        };
        let startup = Startup {
            env,
            diff: Default::default(),
            start_page,
            filters,
            servers: vec![crate::session::ServerSetup {
                queries: vec![spec("Open", "status:open"), spec("Mine", "owner:self")],
                bookmarks,
                projects,
                username: username.map(Into::into),
                ..setup()
            }],
            active: None,
        };
        let (window, shell) = open(cx, |window, cx| {
            cx.new(|cx| Shell::new(startup, window, cx))
        });
        cx.run_until_parked();
        Fixture {
            window,
            shell,
            backend,
        }
    }

    fn press(cx: &mut TestAppContext, f: &Fixture, key: &'static str) {
        cx.update_window(f.window, |_, window, cx| window.press(key, cx))
            .expect("press");
        cx.run_until_parked();
    }

    fn open_settings(cx: &mut TestAppContext, f: &Fixture) {
        press(
            cx,
            f,
            if cfg!(target_os = "macos") {
                "cmd-,"
            } else {
                "ctrl-,"
            },
        );
    }

    /// Shows `tab` of the open settings page, the cursor on its first row.
    fn show_tab(cx: &mut TestAppContext, f: &Fixture, tab: SettingsTab) {
        let page = cx.update(|cx| f.shell.read(cx).settings_page_for_test().expect("page"));
        page.update(cx, |p, cx| p.show_tab_for_test(tab, cx));
    }

    fn names(cx: &mut TestAppContext, f: &Fixture) -> Vec<String> {
        cx.update(|cx| {
            f.shell
                .read(cx)
                .queries_for_test()
                .iter()
                .map(|q| q.name.clone())
                .collect()
        })
    }

    fn type_into(
        cx: &mut TestAppContext,
        f: &Fixture,
        input: &Entity<InputState>,
        text: &'static str,
    ) {
        cx.update_window(f.window, |_, window, cx| {
            input.update(cx, |s, cx| s.set_value(text, window, cx))
        })
        .expect("set value");
    }

    fn inputs(cx: &mut TestAppContext, f: &Fixture) -> (Entity<InputState>, Entity<InputState>) {
        cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            page.read(cx).inputs_for_test()
        })
    }

    #[gpui_kit::test]
    fn the_key_opens_the_page_and_u_goes_back_to_the_dashboard(cx: &mut TestAppContext) {
        let f = fixture(cx, false);
        open_settings(cx, &f);
        cx.update(|cx| assert_eq!(f.shell.read(cx).route(), &Route::Settings));
        press(cx, &f, "u");
        cx.update(|cx| assert_eq!(f.shell.read(cx).route(), &Route::Dashboard));
    }

    #[gpui_kit::test]
    fn saving_projects_updates_the_setup_and_the_server_checkouts(cx: &mut TestAppContext) {
        use crate::session::{ProjectSettings, ServerKey};
        let f = fixture(cx, false);
        let key = ServerKey::new("test");
        let checkouts = f
            .shell
            .update(cx, |s, cx| s.checkouts(&key, cx))
            .expect("server");
        assert_eq!(checkouts.read_with(cx, |c, _| c.all().len()), 0);

        let list = vec![ProjectSettings::new("demo/a", "/nowhere/a")];
        f.shell
            .update(cx, |s, cx| s.save_projects(list.clone(), cx));
        cx.run_until_parked();
        assert_eq!(*f.backend.saved_projects.lock().unwrap(), vec![list]);
        let now = checkouts.read_with(cx, |c, _| c.all());
        assert_eq!(now.len(), 1);
        assert_eq!(now[0].project, "demo/a");
        // The shell hands out the same entity again.
        let again = f
            .shell
            .update(cx, |s, cx| s.checkouts(&key, cx))
            .expect("server");
        assert_eq!(again.entity_id(), checkouts.entity_id());
    }

    #[gpui_kit::test]
    fn a_failed_projects_save_shows_a_hint_and_keeps_the_list(cx: &mut TestAppContext) {
        use crate::session::{ProjectSettings, ServerKey};
        let f = fixture(cx, true);
        open_settings(cx, &f);
        show_tab(cx, &f, SettingsTab::Projects);
        let key = ServerKey::new("test");
        let checkouts = f
            .shell
            .update(cx, |s, cx| s.checkouts(&key, cx))
            .expect("server");
        let list = vec![ProjectSettings::new("demo/a", "/nowhere/a")];
        f.shell.update(cx, |s, cx| s.save_projects(list, cx));
        cx.run_until_parked();
        let page = cx.update(|cx| f.shell.read(cx).settings_page_for_test().expect("page"));
        let hint = page.read_with(cx, |p, _| p.hint_for_test());
        let (text, error) = hint.expect("a hint");
        assert!(error);
        assert!(text.contains("Could not save the projects"), "{text}");
        assert!(text.contains("disk full"), "{text}");
        assert!(checkouts.read_with(cx, |c, _| c.all().is_empty()));
    }

    // ---- the Projects tab and the project dialog ----

    fn tmp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("projects-tab-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// `git init` with an environment that ignores the user's git config.
    fn git_init(dir: &std::path::Path) {
        let status = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .expect("run git");
        assert!(status.success());
    }

    fn project(name: &str, dir: &std::path::Path) -> crate::session::ProjectSettings {
        crate::session::ProjectSettings::new(name, dir.to_string_lossy())
    }

    /// The Projects tab of an anonymous server.
    fn projects_fixture(
        cx: &mut TestAppContext,
        projects: Vec<crate::session::ProjectSettings>,
    ) -> Fixture {
        projects_fixture_as(cx, projects, None)
    }

    fn projects_fixture_as(
        cx: &mut TestAppContext,
        projects: Vec<crate::session::ProjectSettings>,
        username: Option<&str>,
    ) -> Fixture {
        let f = fixture_all(
            cx,
            false,
            StartupPage::Dashboard,
            Vec::new(),
            Vec::new(),
            projects,
            username,
        );
        open_settings(cx, &f);
        show_tab(cx, &f, SettingsTab::Projects);
        cx.run_until_parked();
        f
    }

    /// The status words of every project the tab lists.
    fn statuses(cx: &mut TestAppContext, f: &Fixture) -> Vec<(String, String)> {
        cx.update(|cx| {
            let key = crate::session::ServerKey::new("test");
            let checkouts = f
                .shell
                .update(cx, |s, cx| s.checkouts(&key, cx))
                .expect("server");
            checkouts
                .read(cx)
                .all()
                .iter()
                .map(|c| {
                    (
                        c.project.clone(),
                        crate::pages::settings::projects::status_text(c).0,
                    )
                })
                .collect()
        })
    }

    fn project_dialog(
        cx: &mut TestAppContext,
        f: &Fixture,
    ) -> Option<Entity<crate::project_dialog::ProjectDialog>> {
        cx.update(|cx| f.shell.read(cx).project_dialog_for_test())
    }

    /// Types `name` and `path` into the open dialog.
    fn fill_dialog(cx: &mut TestAppContext, f: &Fixture, name: &'static str, path: &'static str) {
        let dialog = project_dialog(cx, f).expect("dialog");
        let (n, p) = cx.update(|cx| {
            let d = dialog.read(cx);
            (d.name_input_for_test(), d.path_input_for_test())
        });
        type_into(cx, f, &n, name);
        type_into(cx, f, &p, path);
    }

    fn saved_projects(f: &Fixture) -> Vec<Vec<crate::session::ProjectSettings>> {
        f.backend.saved_projects.lock().unwrap().clone()
    }

    #[gpui_kit::test]
    fn the_projects_tab_lists_the_checkouts_with_their_status(cx: &mut TestAppContext) {
        let repo = tmp_dir("repo");
        git_init(&repo);
        let plain = tmp_dir("plain");
        let f = projects_fixture(
            cx,
            vec![project("demo/ok", &repo), project("demo/no", &plain)],
        );
        let page = cx.update(|cx| f.shell.read(cx).settings_page_for_test().expect("page"));
        assert_eq!(
            page.read_with(cx, |p, _| p.project_names_for_test().to_vec()),
            ["demo/ok", "demo/no"]
        );
        let st = statuses(cx, &f);
        assert_eq!(st[0], ("demo/ok".into(), "Git checkout".into()));
        assert!(st[1].1.contains("is not a git repository"), "{st:?}");
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&plain);
    }

    #[gpui_kit::test]
    fn showing_the_tab_again_checks_a_failed_checkout_again(cx: &mut TestAppContext) {
        let dir = tmp_dir("again");
        let f = projects_fixture(cx, vec![project("demo/a", &dir)]);
        assert!(statuses(cx, &f)[0].1.contains("not a git repository"));
        git_init(&dir);
        // Nothing changes by itself.
        cx.run_until_parked();
        assert!(statuses(cx, &f)[0].1.contains("not a git repository"));
        show_tab(cx, &f, SettingsTab::Review);
        show_tab(cx, &f, SettingsTab::Projects);
        cx.run_until_parked();
        assert_eq!(statuses(cx, &f)[0].1, "Git checkout");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui_kit::test]
    fn the_seven_key_shows_the_projects_tab(cx: &mut TestAppContext) {
        let f = fixture(cx, false);
        open_settings(cx, &f);
        press(cx, &f, "7");
        assert_eq!(tab_state(cx, &f).0, SettingsTab::Projects);
    }

    #[gpui_kit::test]
    fn adding_a_checkout_saves_the_list_with_the_path_as_written(cx: &mut TestAppContext) {
        let f = projects_fixture(cx, Vec::new());
        // The empty-state row also adds.
        press(cx, &f, "a");
        assert!(project_dialog(cx, &f).is_some());
        fill_dialog(cx, &f, " demo/new ", "~/src/new");
        press(cx, &f, "enter");
        assert!(project_dialog(cx, &f).is_none());
        assert_eq!(
            saved_projects(&f),
            vec![vec![crate::session::ProjectSettings::new(
                "demo/new",
                "~/src/new"
            )]]
        );
        // The tab follows what the shell saved.
        let page = cx.update(|cx| f.shell.read(cx).settings_page_for_test().expect("page"));
        assert_eq!(
            page.read_with(cx, |p, _| p.project_names_for_test().to_vec()),
            ["demo/new"]
        );
    }

    #[gpui_kit::test]
    fn an_added_checkout_is_checked_at_once(cx: &mut TestAppContext) {
        let repo = tmp_dir("added");
        git_init(&repo);
        let f = projects_fixture(cx, Vec::new());
        press(cx, &f, "a");
        let path = repo.to_string_lossy().into_owned();
        let path: &'static str = Box::leak(path.into_boxed_str());
        fill_dialog(cx, &f, "demo/new", path);
        press(cx, &f, "enter");
        cx.run_until_parked();
        assert_eq!(
            statuses(cx, &f),
            vec![("demo/new".to_owned(), "Git checkout".to_owned())]
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[gpui_kit::test]
    fn editing_a_checkout_replaces_it_in_place(cx: &mut TestAppContext) {
        let a = tmp_dir("edit-a");
        let b = tmp_dir("edit-b");
        let f = projects_fixture(cx, vec![project("one", &a), project("two", &a)]);
        press(cx, &f, "j");
        press(cx, &f, "e");
        let dialog = project_dialog(cx, &f).expect("dialog");
        assert!(cx.update(|cx| dialog.read(cx).is_edit_for_test()));
        let path = b.to_string_lossy().into_owned();
        let path: &'static str = Box::leak(path.into_boxed_str());
        fill_dialog(cx, &f, "two", path);
        press(cx, &f, "enter");
        assert_eq!(
            saved_projects(&f),
            vec![vec![project("one", &a), project("two", &b)]]
        );
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[gpui_kit::test]
    fn a_duplicate_name_or_an_empty_field_is_not_saved(cx: &mut TestAppContext) {
        let a = tmp_dir("dup");
        let f = projects_fixture(cx, vec![project("one", &a)]);
        press(cx, &f, "a");
        fill_dialog(cx, &f, "one", "/somewhere");
        press(cx, &f, "enter");
        let dialog = project_dialog(cx, &f).expect("still open");
        assert_eq!(
            cx.update(|cx| dialog.read(cx).problem_for_test(cx)),
            Some("This project already has a checkout.")
        );
        fill_dialog(cx, &f, "", "/somewhere");
        press(cx, &f, "enter");
        assert!(
            cx.update(|cx| dialog.read(cx).problem_for_test(cx))
                .is_some()
        );
        fill_dialog(cx, &f, "two", "  ");
        press(cx, &f, "enter");
        assert!(
            cx.update(|cx| dialog.read(cx).problem_for_test(cx))
                .is_some()
        );
        assert!(saved_projects(&f).is_empty());
        // Escape discards.
        press(cx, &f, "escape");
        assert!(project_dialog(cx, &f).is_none());
        assert!(saved_projects(&f).is_empty());
        let _ = std::fs::remove_dir_all(&a);
    }

    #[gpui_kit::test]
    fn removing_a_checkout_asks_first(cx: &mut TestAppContext) {
        let a = tmp_dir("rm");
        let f = projects_fixture(cx, vec![project("one", &a), project("two", &a)]);
        press(cx, &f, "d");
        press(cx, &f, "n");
        assert!(saved_projects(&f).is_empty());
        press(cx, &f, "d");
        press(cx, &f, "y");
        assert_eq!(saved_projects(&f), vec![vec![project("two", &a)]]);
        let page = cx.update(|cx| f.shell.read(cx).settings_page_for_test().expect("page"));
        assert_eq!(
            page.read_with(cx, |p, _| p.project_names_for_test().to_vec()),
            ["two"]
        );
        let _ = std::fs::remove_dir_all(&a);
    }

    const SAVE: &str = if cfg!(target_os = "macos") {
        "cmd-enter"
    } else {
        "ctrl-enter"
    };

    /// The open dialog's local options, and whether the box and the vote
    /// choice take input.
    fn local_state(
        cx: &mut TestAppContext,
        f: &Fixture,
    ) -> (crate::project_dialog::LocalOptions, bool, bool) {
        let dialog = project_dialog(cx, f).expect("dialog");
        cx.update(|cx| dialog.read(cx).local_for_test())
    }

    /// The Projects tab's word next to each project's status.
    fn local_words(cx: &mut TestAppContext, f: &Fixture) -> Vec<Option<&'static str>> {
        cx.update(|cx| {
            let key = crate::session::ServerKey::new("test");
            let checkouts = f
                .shell
                .update(cx, |s, cx| s.checkouts(&key, cx))
                .expect("server");
            checkouts
                .read(cx)
                .projects()
                .map(crate::pages::settings::projects::local_word)
                .collect()
        })
    }

    #[gpui_kit::test]
    fn the_local_changes_box_turns_the_vote_choice_on_and_both_are_saved(cx: &mut TestAppContext) {
        use model::LocalApproval;
        let f = projects_fixture_as(cx, Vec::new(), Some("u"));
        press(cx, &f, "a");
        fill_dialog(cx, &f, "demo/new", "/src/new");
        let (local, box_on, choice_on) = local_state(cx, &f);
        assert!(!local.changes && box_on && !choice_on, "{local:?}");
        assert_eq!(local.approval, LocalApproval::Exact);
        // Name, directory, Browse, then the box: Space ticks it.
        for key in ["tab", "tab", "tab", "space"] {
            press(cx, &f, key);
        }
        let (local, _, choice_on) = local_state(cx, &f);
        assert!(local.changes && choice_on);
        // The choice is next; Up goes to the more permissive options.
        press(cx, &f, "tab");
        press(cx, &f, "up");
        assert_eq!(local_state(cx, &f).0.approval, LocalApproval::ExactOrManual);
        press(cx, &f, "up");
        press(cx, &f, "up");
        assert_eq!(local_state(cx, &f).0.approval, LocalApproval::Follow);
        press(cx, &f, "down");
        press(cx, &f, "up");
        // Clearing the box turns the choice off, which keys cannot change
        // and Tab skips.
        press(cx, &f, "shift-tab");
        press(cx, &f, "space");
        let (local, _, choice_on) = local_state(cx, &f);
        assert!(!local.changes && !choice_on);
        assert_eq!(local.approval, LocalApproval::Follow);
        press(cx, &f, "tab");
        press(cx, &f, "down");
        assert_eq!(local_state(cx, &f).0.approval, LocalApproval::Follow);
        // That Tab went to Save: Space presses it, with the box clear.
        press(cx, &f, "space");
        assert!(project_dialog(cx, &f).is_none());
        let off = crate::session::ProjectSettings {
            local_approval: LocalApproval::Follow,
            ..crate::session::ProjectSettings::new("demo/new", "/src/new")
        };
        assert_eq!(saved_projects(&f), vec![vec![off.clone()]]);
        assert_eq!(local_words(cx, &f), [None]);

        // Ticked and saved, the tab says so.
        press(cx, &f, "e");
        for key in ["tab", "tab", "tab", "space"] {
            press(cx, &f, key);
        }
        press(cx, &f, SAVE);
        assert!(project_dialog(cx, &f).is_none());
        let on = crate::session::ProjectSettings {
            local_changes: true,
            ..off
        };
        assert_eq!(saved_projects(&f).last(), Some(&vec![on]));
        assert_eq!(local_words(cx, &f), [Some("local changes")]);
    }

    #[gpui_kit::test]
    fn a_click_that_turns_the_choice_off_moves_the_focus_to_the_box(cx: &mut TestAppContext) {
        let f = projects_fixture_as(cx, Vec::new(), Some("u"));
        press(cx, &f, "a");
        fill_dialog(cx, &f, "demo/new", "/src/new");
        for key in ["tab", "tab", "tab", "space", "tab"] {
            press(cx, &f, key);
        }
        let dialog = project_dialog(cx, &f).expect("dialog");
        let focus = |cx: &mut TestAppContext| {
            cx.update_window(f.window, |_, window, cx| {
                dialog.read(cx).approval_focused_for_test(window)
            })
            .expect("window")
        };
        assert_eq!(focus(cx), (true, false));
        assert!(
            cx.update(|cx| dialog.read(cx).footer_hint_for_test())
                .contains("Space ticks")
        );
        cx.update_window(f.window, |_, window, cx| {
            dialog.update(cx, |d, cx| d.toggle_local_for_test(window, cx))
        })
        .expect("window");
        cx.run_until_parked();
        assert_eq!(focus(cx), (false, true));
        // So Tab goes on to Save, which Space presses.
        press(cx, &f, "tab");
        press(cx, &f, "space");
        assert!(project_dialog(cx, &f).is_none());
        assert_eq!(saved_projects(&f).len(), 1);
    }

    #[gpui_kit::test]
    fn the_local_options_cannot_be_changed_on_an_anonymous_server(cx: &mut TestAppContext) {
        use model::LocalApproval;
        let a = tmp_dir("anon-local");
        // Set by hand in the file.
        let entry = crate::session::ProjectSettings {
            local_changes: true,
            local_approval: LocalApproval::ExactOrManual,
            ..project("one", &a)
        };
        let f = projects_fixture(cx, vec![entry.clone()]);
        press(cx, &f, "e");
        let (local, box_on, choice_on) = local_state(cx, &f);
        assert!(local.changes && !box_on && !choice_on);
        assert_eq!(local.approval, LocalApproval::ExactOrManual);
        let dialog = project_dialog(cx, &f).expect("dialog");
        let hint = cx.update(|cx| dialog.read(cx).footer_hint_for_test());
        assert!(!hint.contains("Space") && !hint.contains("Up"), "{hint}");
        // Tab skips both: after Browse comes Save, and Space presses it.
        for key in ["tab", "tab", "tab", "space"] {
            press(cx, &f, key);
        }
        assert!(project_dialog(cx, &f).is_none());
        // What the entry had is kept.
        assert_eq!(saved_projects(&f), vec![vec![entry]]);
        let _ = std::fs::remove_dir_all(&a);
    }

    fn gear(cx: &mut TestAppContext, f: &Fixture, project: &'static str) {
        cx.update_window(f.window, |_, window, cx| {
            f.shell.update(cx, |s, cx| {
                s.open_project_settings_for_test(project, window, cx)
            })
        })
        .expect("gear");
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn the_gear_opens_the_projects_tab_on_the_project(cx: &mut TestAppContext) {
        let a = tmp_dir("gear");
        let f = projects_fixture(cx, vec![project("one", &a), project("two", &a)]);
        let first = tab_state(cx, &f).1;
        press(cx, &f, "u");
        cx.update(|cx| assert_eq!(f.shell.read(cx).route(), &Route::Dashboard));
        gear(cx, &f, "two");
        cx.update(|cx| assert_eq!(f.shell.read(cx).route(), &Route::Settings));
        assert_eq!(tab_state(cx, &f), (SettingsTab::Projects, first + 1));
        assert!(project_dialog(cx, &f).is_none());
        let _ = std::fs::remove_dir_all(&a);
    }

    #[gpui_kit::test]
    fn the_gear_for_an_unconfigured_project_opens_the_add_dialog(cx: &mut TestAppContext) {
        let f = fixture(cx, false);
        gear(cx, &f, "demo/none");
        cx.update(|cx| assert_eq!(f.shell.read(cx).route(), &Route::Settings));
        assert_eq!(tab_state(cx, &f).0, SettingsTab::Projects);
        let dialog = project_dialog(cx, &f).expect("dialog");
        assert!(!cx.update(|cx| dialog.read(cx).is_edit_for_test()));
        let name = cx.update(|cx| dialog.read(cx).name_input_for_test());
        assert_eq!(
            name.read_with(cx, |s, _| s.value().to_string()),
            "demo/none"
        );
        // Saving adds it.
        fill_dialog(cx, &f, "demo/none", "/src/none");
        press(cx, &f, "enter");
        assert_eq!(
            saved_projects(&f),
            vec![vec![crate::session::ProjectSettings::new(
                "demo/none",
                "/src/none"
            )]]
        );
    }

    #[gpui_kit::test]
    fn moving_a_query_saves_it_and_the_dashboard_follows(cx: &mut TestAppContext) {
        let f = fixture(cx, false);
        open_settings(cx, &f);
        press(cx, &f, "shift-j");
        assert_eq!(names(cx, &f), ["Mine", "Open"]);
        let saved = f.backend.saved_queries.lock().unwrap().clone();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0][0].query, "owner:self");

        // The dashboard shows the new order, rows straight from the cache.
        press(cx, &f, "u");
        cx.update(|cx| {
            let list = f.shell.read(cx).list_page_for_test().expect("dashboard");
            let list = list.read(cx);
            assert_eq!(list.visible_rows(0)[0].number, ChangeNumber(2));
            assert_eq!(list.visible_rows(1)[0].number, ChangeNumber(1));
        });
    }

    #[gpui_kit::test]
    fn editing_adding_and_deleting(cx: &mut TestAppContext) {
        let f = fixture(cx, false);
        open_settings(cx, &f);

        // Edit the first row's name; an empty name is refused with a reason.
        press(cx, &f, "e");
        let (name, query) = inputs(cx, &f);
        type_into(cx, &f, &name, "");
        press(cx, &f, "enter");
        assert!(f.backend.saved_queries.lock().unwrap().is_empty());
        cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            assert!(page.read(cx).editing_for_test());
        });
        type_into(cx, &f, &name, "Opened");
        type_into(cx, &f, &query, "status:open project:x");
        press(cx, &f, "enter");
        assert_eq!(names(cx, &f), ["Opened", "Mine"]);

        // Add a row; Escape cancels it.
        press(cx, &f, "a");
        press(cx, &f, "escape");
        assert_eq!(names(cx, &f).len(), 2);
        press(cx, &f, "a");
        type_into(cx, &f, &name, "New");
        type_into(cx, &f, &query, "is:wip");
        press(cx, &f, "enter");
        assert_eq!(names(cx, &f), ["Opened", "Mine", "New"]);

        // Delete asks first; n keeps it, y removes it.
        press(cx, &f, "d");
        press(cx, &f, "n");
        assert_eq!(names(cx, &f).len(), 3);
        press(cx, &f, "d");
        press(cx, &f, "y");
        // The cursor was on the new row.
        assert_eq!(names(cx, &f), ["Opened", "Mine"]);
        assert_eq!(f.backend.saved_queries.lock().unwrap().len(), 3);
    }

    fn bookmark_names(cx: &mut TestAppContext, f: &Fixture) -> Vec<String> {
        cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            let page = page.read(cx);
            page.bookmarks_for_test()
                .iter()
                .map(|q| q.name.clone())
                .collect()
        })
    }

    #[gpui_kit::test]
    fn bookmarks_are_added_edited_moved_and_deleted_below_the_queries(cx: &mut TestAppContext) {
        let f = fixture_full(cx, false, StartupPage::Dashboard, Vec::new(), Vec::new());
        open_settings(cx, &f);
        // Two queries, then the empty bookmarks row.
        press(cx, &f, "j");
        press(cx, &f, "j");
        assert_eq!(page_state(cx, &f).0, 2);
        let (name, query) = inputs(cx, &f);
        press(cx, &f, "a");
        type_into(cx, &f, &name, "First");
        type_into(cx, &f, &query, "is:open");
        press(cx, &f, "enter");
        press(cx, &f, "a");
        type_into(cx, &f, &name, "Second");
        type_into(cx, &f, &query, "is:wip");
        press(cx, &f, "enter");
        assert_eq!(bookmark_names(cx, &f), ["First", "Second"]);
        assert_eq!(names(cx, &f), ["Open", "Mine"]);
        // The cursor stays on the bookmark it added.
        assert_eq!(page_state(cx, &f).0, 3);

        press(cx, &f, "shift-k");
        assert_eq!(bookmark_names(cx, &f), ["Second", "First"]);
        assert_eq!(page_state(cx, &f).0, 2);
        // The first bookmark cannot move into the queries.
        press(cx, &f, "shift-k");
        assert_eq!(bookmark_names(cx, &f), ["Second", "First"]);

        press(cx, &f, "e");
        type_into(cx, &f, &name, "Renamed");
        press(cx, &f, "enter");
        assert_eq!(bookmark_names(cx, &f), ["Renamed", "First"]);

        press(cx, &f, "d");
        press(cx, &f, "y");
        assert_eq!(bookmark_names(cx, &f), ["First"]);
        let saved = f.backend.saved_bookmarks.lock().unwrap().clone();
        assert_eq!(saved.len(), 5);
        assert!(f.backend.saved_queries.lock().unwrap().is_empty());
        // The shell, and so the dashboard's row, follows.
        press(cx, &f, "u");
        cx.update(|cx| {
            let list = f.shell.read(cx).list_page_for_test().expect("dashboard");
            assert_eq!(list.read(cx).bookmarks_for_test(), ["First"]);
        });
    }

    #[gpui_kit::test]
    fn a_failed_save_changes_nothing_and_says_so(cx: &mut TestAppContext) {
        let f = fixture(cx, true);
        open_settings(cx, &f);
        press(cx, &f, "shift-j");
        assert_eq!(names(cx, &f), ["Open", "Mine"]);
        cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            let (text, error) = page.read(cx).hint_for_test().expect("hint");
            assert!(error && text.contains("disk full"), "{text}");
            assert_eq!(page.read(cx).queries_for_test().len(), 2);
        });
    }
    fn advance_on(cx: &mut TestAppContext, f: &Fixture) -> bool {
        cx.update(|cx| {
            f.shell
                .read(cx)
                .layout_for_test()
                .read(cx)
                .advance_after_vote()
        })
    }

    #[gpui_kit::test]
    fn toggling_advance_after_vote_saves_it_and_flips_the_layout(cx: &mut TestAppContext) {
        let f = fixture(cx, false);
        open_settings(cx, &f);
        assert!(advance_on(cx, &f));
        show_tab(cx, &f, SettingsTab::Review);
        press(cx, &f, "space");
        assert!(!advance_on(cx, &f));
        assert_eq!(*f.backend.saved_advance.lock().unwrap(), [false]);
        press(cx, &f, "enter");
        assert!(advance_on(cx, &f));
        assert_eq!(*f.backend.saved_advance.lock().unwrap(), [false, true]);
    }

    #[gpui_kit::test]
    fn a_failed_advance_save_keeps_the_old_value(cx: &mut TestAppContext) {
        let f = fixture(cx, true);
        open_settings(cx, &f);
        show_tab(cx, &f, SettingsTab::Review);
        press(cx, &f, "space");
        assert!(advance_on(cx, &f));
        cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            let (text, error) = page.read(cx).hint_for_test().expect("hint");
            assert!(error && text.contains("disk full"), "{text}");
        });
    }

    #[gpui_kit::test]
    fn switching_the_theme_saves_and_applies_it(cx: &mut TestAppContext) {
        use crate::theme::{self, ThemeName};
        let f = fixture(cx, false);
        open_settings(cx, &f);
        let applied = |cx: &mut TestAppContext| {
            cx.update(|cx| {
                let name = gpui_kit::component::ActiveTheme::theme(cx).theme_name();
                (theme::current(cx), name.to_string())
            })
        };
        assert_eq!(applied(cx).0, ThemeName::Cwc);
        show_tab(cx, &f, SettingsTab::Appearance);
        press(cx, &f, "space");
        let (now, name) = applied(cx);
        assert_eq!(now, ThemeName::Solarized);
        assert!(name.starts_with("Solarized"), "{name}");
        assert_eq!(
            *f.backend.saved_theme.lock().unwrap(),
            [ThemeName::Solarized]
        );
        press(cx, &f, "enter");
        let (now, name) = applied(cx);
        assert_eq!(now, ThemeName::Cwc);
        assert!(name.starts_with("CWC"), "{name}");
    }

    #[gpui_kit::test]
    fn switching_the_modes_saves_and_applies_them(cx: &mut TestAppContext) {
        use crate::theme::{ColorMode, ColorModes, color_modes};
        let f = fixture(cx, false);
        open_settings(cx, &f);
        let ui_dark = |cx: &mut TestAppContext| {
            cx.update(|cx| gpui_kit::component::ActiveTheme::theme(cx).is_dark())
        };
        assert_eq!(cx.update(|cx| color_modes(cx)), ColorModes::default());
        // The theme row, then the UI mode row: system, light, dark.
        show_tab(cx, &f, SettingsTab::Appearance);
        press(cx, &f, "j");
        press(cx, &f, "space");
        press(cx, &f, "space");
        assert_eq!(cx.update(|cx| color_modes(cx).ui), ColorMode::Dark);
        assert!(ui_dark(cx));
        press(cx, &f, "space");
        assert_eq!(cx.update(|cx| color_modes(cx).ui), ColorMode::System);
        // The diff row is separate.
        press(cx, &f, "j");
        press(cx, &f, "space");
        assert_eq!(
            cx.update(|cx| color_modes(cx)),
            ColorModes {
                ui: ColorMode::System,
                diff: ColorMode::Light
            }
        );
    }

    #[gpui_kit::test]
    fn a_failed_theme_save_keeps_the_theme(cx: &mut TestAppContext) {
        let f = fixture(cx, true);
        open_settings(cx, &f);
        show_tab(cx, &f, SettingsTab::Appearance);
        press(cx, &f, "space");
        cx.update(|cx| {
            assert_eq!(crate::theme::current(cx), crate::theme::ThemeName::Cwc);
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            let (text, error) = page.read(cx).hint_for_test().expect("hint");
            assert!(error && text.contains("disk full"), "{text}");
        });
    }

    #[gpui_kit::test]
    fn a_startup_query_needs_text_and_is_saved(cx: &mut TestAppContext) {
        let f = fixture(cx, false);
        open_settings(cx, &f);
        for _ in 0..3 {
            press(cx, &f, "j");
        }
        // Space on "dashboard" starts typing the query; empty is refused.
        press(cx, &f, "space");
        let input = cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            page.read(cx).startup_input_for_test()
        });
        press(cx, &f, "enter");
        assert!(f.backend.saved_startup.lock().unwrap().is_empty());
        cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            assert!(page.read(cx).editing_for_test());
        });
        type_into(cx, &f, &input, " is:open owner:self ");
        press(cx, &f, "enter");
        let want = StartupPage::Search("is:open owner:self".into());
        assert_eq!(
            *f.backend.saved_startup.lock().unwrap(),
            std::slice::from_ref(&want)
        );
        cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            assert_eq!(page.read(cx).start_page_for_test(), &want);
            assert!(!page.read(cx).editing_for_test());
        });

        // Space again goes back to the dashboard.
        press(cx, &f, "space");
        assert_eq!(
            f.backend.saved_startup.lock().unwrap().last(),
            Some(&StartupPage::Dashboard)
        );

        // The shell remembers it for the next time the page opens.
        press(cx, &f, "u");
        open_settings(cx, &f);
        cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            assert_eq!(page.read(cx).start_page_for_test(), &StartupPage::Dashboard);
        });
    }

    #[gpui_kit::test]
    fn the_shell_opens_on_the_configured_search(cx: &mut TestAppContext) {
        let f = fixture_at(cx, false, StartupPage::Search("owner:self".into()));
        cx.update(|cx| {
            assert_eq!(
                f.shell.read(cx).route(),
                &Route::Search("owner:self".into())
            );
        });
        // The dashboard is still one key away.
        press(cx, &f, "u");
        cx.update(|cx| assert_eq!(f.shell.read(cx).route(), &Route::Dashboard));
        let f = fixture(cx, false);
        cx.update(|cx| assert_eq!(f.shell.read(cx).route(), &Route::Dashboard));
    }

    fn rule(name: &str, action: FilterAction) -> FileFilter {
        FileFilter {
            name: name.into(),
            globs: vec![format!("*.{name}")],
            projects: Vec::new(),
            servers: Vec::new(),
            action,
            enabled: true,
        }
    }

    fn two_rules() -> Vec<FileFilter> {
        vec![
            rule("gen", FilterAction::Dim),
            rule("lock", FilterAction::Hide),
        ]
    }

    fn page_state(cx: &mut TestAppContext, f: &Fixture) -> (usize, Vec<String>) {
        cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            let page = page.read(cx);
            (
                page.cursor_for_test(),
                page.filter_names_for_test().to_vec(),
            )
        })
    }

    /// The dialog's cursor and mode, if it is open.
    fn dialog_state(cx: &mut TestAppContext, f: &Fixture) -> Option<(usize, &'static str)> {
        cx.update(|cx| {
            let d = f.shell.read(cx).filters_dialog_for_test()?;
            let d = d.read(cx);
            Some((d.cursor_for_test(), d.mode_for_test()))
        })
    }

    /// Cursor on the first filter row (row 8: 2 queries, the empty
    /// bookmarks row, startup, advance, theme, two modes before it).
    fn to_filters(cx: &mut TestAppContext, f: &Fixture) {
        show_tab(cx, f, SettingsTab::Filters);
    }

    fn tab_state(cx: &mut TestAppContext, f: &Fixture) -> (SettingsTab, usize) {
        cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            let page = page.read(cx);
            (page.tab_for_test(), page.cursor_for_test())
        })
    }

    #[gpui_kit::test]
    fn the_cursor_stays_inside_the_tab_and_jumps_to_its_first_row_on_a_switch(
        cx: &mut TestAppContext,
    ) {
        let f = fixture_with(cx, false, StartupPage::Dashboard, two_rules());
        open_settings(cx, &f);
        // Queries: 2 queries, the empty bookmarks row, the startup row.
        assert_eq!(tab_state(cx, &f), (SettingsTab::Queries, 0));
        for _ in 0..9 {
            press(cx, &f, "j");
        }
        assert_eq!(tab_state(cx, &f).1, 3);
        press(cx, &f, "k");
        for _ in 0..9 {
            press(cx, &f, "k");
        }
        assert_eq!(tab_state(cx, &f).1, 0);

        // Appearance: the theme row and the two mode rows.
        show_tab(cx, &f, SettingsTab::Appearance);
        assert_eq!(tab_state(cx, &f), (SettingsTab::Appearance, 5));
        press(cx, &f, "k");
        assert_eq!(tab_state(cx, &f).1, 5);
        for _ in 0..5 {
            press(cx, &f, "j");
        }
        assert_eq!(tab_state(cx, &f).1, 7);

        // Review has the one row; a does not add a query there.
        show_tab(cx, &f, SettingsTab::Review);
        assert_eq!(tab_state(cx, &f), (SettingsTab::Review, 4));
        press(cx, &f, "j");
        press(cx, &f, "a");
        assert_eq!(tab_state(cx, &f).1, 4);
        cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            assert!(!page.read(cx).editing_for_test());
        });

        show_tab(cx, &f, SettingsTab::Servers);
        assert_eq!(tab_state(cx, &f), (SettingsTab::Servers, 10));
        press(cx, &f, "j");
        assert_eq!(tab_state(cx, &f).1, 10);
    }

    fn tabs_strip(cx: &mut TestAppContext, f: &Fixture) -> gpui_kit::ScrollHandle {
        cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            page.read(cx).tabs_scroll_for_test()
        })
    }

    #[gpui_kit::test]
    fn l_h_the_arrows_and_the_digits_switch_tabs(cx: &mut TestAppContext) {
        let f = fixture_with(cx, false, StartupPage::Dashboard, two_rules());
        open_settings(cx, &f);
        let tab = |cx: &mut TestAppContext| tab_state(cx, &f).0;
        assert_eq!(tab(cx), SettingsTab::Queries);
        press(cx, &f, "l");
        assert_eq!(tab(cx), SettingsTab::Review);
        press(cx, &f, "right");
        assert_eq!(tab(cx), SettingsTab::Appearance);
        press(cx, &f, "l");
        assert_eq!(tab(cx), SettingsTab::Filters);
        press(cx, &f, "l");
        assert_eq!(tab(cx), SettingsTab::Notifications);
        press(cx, &f, "l");
        assert_eq!(tab(cx), SettingsTab::Projects);
        // The strip has ends.
        press(cx, &f, "l");
        assert_eq!(tab(cx), SettingsTab::Projects);
        press(cx, &f, "h");
        assert_eq!(tab(cx), SettingsTab::Notifications);
        press(cx, &f, "h");
        assert_eq!(tab(cx), SettingsTab::Filters);
        press(cx, &f, "h");
        assert_eq!(tab(cx), SettingsTab::Appearance);
        press(cx, &f, "left");
        assert_eq!(tab(cx), SettingsTab::Review);
        for (key, want) in [
            ("1", SettingsTab::Servers),
            ("7", SettingsTab::Projects),
            ("6", SettingsTab::Notifications),
            ("5", SettingsTab::Filters),
            ("2", SettingsTab::Queries),
            ("4", SettingsTab::Appearance),
            ("3", SettingsTab::Review),
        ] {
            press(cx, &f, key);
            assert_eq!(tab(cx), want, "key {key}");
        }
        press(cx, &f, "1");
        press(cx, &f, "h");
        assert_eq!(tab(cx), SettingsTab::Servers);
        // Switching puts the cursor on the first row of the tab.
        press(cx, &f, "5");
        assert_eq!(tab_state(cx, &f).1, 8);
    }

    #[gpui_kit::test]
    fn clicking_a_tab_shows_it(cx: &mut TestAppContext) {
        let f = fixture(cx, false);
        open_settings(cx, &f);
        let strip = tabs_strip(cx, &f);
        for (i, want) in [(3, SettingsTab::Appearance), (0, SettingsTab::Servers)] {
            let at = strip.bounds_for_item(i).expect("tab").center();
            gpui_kit::VisualTestContext::from_window(f.window, cx)
                .simulate_click(at, Default::default());
            cx.run_until_parked();
            assert_eq!(tab_state(cx, &f).0, want);
        }
    }

    #[gpui_kit::test]
    fn the_tab_keys_are_not_swallowed_from_inputs_and_wait_for_answers(cx: &mut TestAppContext) {
        let f = fixture(cx, false);
        open_settings(cx, &f);
        // While a row is edited the keys are typed, not bound.
        press(cx, &f, "e");
        press(cx, &f, "l");
        press(cx, &f, "2");
        press(cx, &f, "left");
        assert_eq!(tab_state(cx, &f), (SettingsTab::Queries, 0));
        cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            assert!(page.read(cx).editing_for_test());
        });
        // A click on a tab does not leave the edit either.
        let strip = tabs_strip(cx, &f);
        let at = strip.bounds_for_item(2).expect("tab").center();
        gpui_kit::VisualTestContext::from_window(f.window, cx)
            .simulate_click(at, Default::default());
        cx.run_until_parked();
        assert_eq!(tab_state(cx, &f).0, SettingsTab::Queries);
        press(cx, &f, "escape");

        // A pending delete is answered first: the keys do nothing, and the
        // question is still there afterwards.
        press(cx, &f, "d");
        press(cx, &f, "l");
        press(cx, &f, "3");
        assert_eq!(tab_state(cx, &f).0, SettingsTab::Queries);
        press(cx, &f, "n");
        assert_eq!(names(cx, &f).len(), 2);
        press(cx, &f, "l");
        assert_eq!(tab_state(cx, &f).0, SettingsTab::Review);
    }

    #[gpui_kit::test]
    fn the_page_opens_on_the_tab_it_was_left_on(cx: &mut TestAppContext) {
        let f = fixture_with(cx, false, StartupPage::Dashboard, two_rules());
        open_settings(cx, &f);
        press(cx, &f, "4");
        assert_eq!(tab_state(cx, &f), (SettingsTab::Appearance, 5));
        press(cx, &f, "j");
        press(cx, &f, "u");
        // The tab is remembered, the cursor starts on the tab's first row.
        open_settings(cx, &f);
        assert_eq!(tab_state(cx, &f), (SettingsTab::Appearance, 5));
        // Every visit updates it.
        press(cx, &f, "5");
        press(cx, &f, "u");
        open_settings(cx, &f);
        assert_eq!(tab_state(cx, &f).0, SettingsTab::Filters);
    }

    #[gpui_kit::test]
    fn the_filter_rows_follow_the_cursor_after_the_other_rows(cx: &mut TestAppContext) {
        let f = fixture_with(cx, false, StartupPage::Dashboard, two_rules());
        open_settings(cx, &f);
        assert_eq!(page_state(cx, &f).1, ["gen", "lock"]);
        to_filters(cx, &f);
        assert_eq!(page_state(cx, &f).0, 8);
        press(cx, &f, "j");
        // The last rule; the cursor stops there, the server row after it is
        // on another tab.
        assert_eq!(page_state(cx, &f).0, 9);
        press(cx, &f, "j");
        assert_eq!(page_state(cx, &f).0, 9);
        press(cx, &f, "k");
        press(cx, &f, "k");
        assert_eq!(page_state(cx, &f).0, 8);
    }

    #[gpui_kit::test]
    fn enter_and_d_on_a_rule_open_the_dialog_on_it(cx: &mut TestAppContext) {
        let f = fixture_with(cx, false, StartupPage::Dashboard, two_rules());
        open_settings(cx, &f);
        to_filters(cx, &f);
        press(cx, &f, "j");
        press(cx, &f, "enter");
        assert_eq!(dialog_state(cx, &f), Some((1, "browsing")));
        press(cx, &f, "escape");
        assert_eq!(dialog_state(cx, &f), None);
        // Focus is back on the page: the cursor still works.
        press(cx, &f, "k");
        assert_eq!(page_state(cx, &f).0, 8);
        press(cx, &f, "d");
        // Deleting is the dialog's business, with its confirmation.
        assert_eq!(dialog_state(cx, &f), Some((0, "browsing")));
        press(cx, &f, "escape");
        press(cx, &f, "e");
        assert_eq!(dialog_state(cx, &f), Some((0, "browsing")));
    }

    #[gpui_kit::test]
    fn a_adds_a_rule_in_the_filters_and_a_query_elsewhere(cx: &mut TestAppContext) {
        let f = fixture_with(cx, false, StartupPage::Dashboard, two_rules());
        open_settings(cx, &f);
        // On a query row: a new query, no dialog.
        press(cx, &f, "a");
        assert_eq!(dialog_state(cx, &f), None);
        cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            assert!(page.read(cx).editing_for_test());
        });
        press(cx, &f, "escape");
        // In the filters section: the dialog, on a new rule.
        to_filters(cx, &f);
        press(cx, &f, "a");
        assert_eq!(dialog_state(cx, &f), Some((2, "adding")));
    }

    #[gpui_kit::test]
    fn without_rules_one_row_says_so_and_enter_adds(cx: &mut TestAppContext) {
        let f = fixture(cx, false);
        open_settings(cx, &f);
        to_filters(cx, &f);
        // The empty-state row is the only one of the filters.
        press(cx, &f, "j");
        assert_eq!(page_state(cx, &f), (8, Vec::new()));
        press(cx, &f, "enter");
        assert_eq!(dialog_state(cx, &f), Some((0, "adding")));
    }

    #[gpui_kit::test]
    fn the_section_follows_the_shared_rules(cx: &mut TestAppContext) {
        let f = fixture_with(cx, false, StartupPage::Dashboard, two_rules());
        open_settings(cx, &f);
        to_filters(cx, &f);
        press(cx, &f, "j");
        // A reorder keeps the cursor on its rule.
        let filters = cx.update(|cx| f.shell.read(cx).filters_for_test().clone());
        cx.update(|cx| {
            filters.update(cx, |ff, cx| {
                ff.set_rules(
                    vec![
                        rule("lock", FilterAction::Hide),
                        rule("gen", FilterAction::Dim),
                    ],
                    cx,
                )
            })
        });
        cx.run_until_parked();
        assert_eq!(page_state(cx, &f), (8, vec!["lock".into(), "gen".into()]));
        // Removing the rule under the cursor clamps it.
        cx.update(|cx| {
            filters.update(cx, |ff, cx| {
                ff.set_rules(vec![rule("gen", FilterAction::Dim)], cx)
            })
        });
        cx.run_until_parked();
        assert_eq!(page_state(cx, &f), (8, vec!["gen".into()]));
        cx.update(|cx| filters.update(cx, |ff, cx| ff.set_rules(Vec::new(), cx)));
        cx.run_until_parked();
        assert_eq!(page_state(cx, &f), (8, Vec::new()));
    }

    mod notifications {
        use sync::notify::NotificationSettings;

        use super::*;

        /// The first row of the tab: the fixture has no filters (one empty row)
        /// and one server.
        const FIRST: usize = 10;

        fn show(cx: &mut TestAppContext, f: &Fixture) {
            open_settings(cx, f);
            press(cx, f, "6");
        }

        fn page_settings(cx: &mut TestAppContext, f: &Fixture) -> NotificationSettings {
            cx.update(|cx| {
                let page = f.shell.read(cx).settings_page_for_test().expect("page");
                page.read(cx).notify_for_test().clone()
            })
        }

        fn shell_settings(cx: &mut TestAppContext, f: &Fixture) -> NotificationSettings {
            cx.update(|cx| f.shell.read(cx).notify_settings_for_test().clone())
        }

        fn writes(f: &Fixture) -> Vec<NotificationSettings> {
            f.backend.saved_notifications.lock().unwrap().clone()
        }

        fn notify_input(cx: &mut TestAppContext, f: &Fixture) -> Entity<InputState> {
            cx.update(|cx| {
                let page = f.shell.read(cx).settings_page_for_test().expect("page");
                page.read(cx).notify_input_for_test()
            })
        }

        fn editing(cx: &mut TestAppContext, f: &Fixture) -> bool {
            cx.update(|cx| {
                let page = f.shell.read(cx).settings_page_for_test().expect("page");
                page.read(cx).editing_for_test()
            })
        }

        #[gpui_kit::test]
        fn six_shows_the_tab_with_its_rows_after_the_servers(cx: &mut TestAppContext) {
            let f = fixture(cx, false);
            show(cx, &f);
            assert_eq!(tab_state(cx, &f), (SettingsTab::Notifications, FIRST));
            let first = cx.update(|cx| {
                let page = f.shell.read(cx).settings_page_for_test().expect("page");
                page.read(cx).notify_row_for_test()
            });
            assert_eq!(first, FIRST);
            // The cursor stays in the tab: the test row ends it, after the
            // options, the timing rows, the four default rules and the reset
            // row.
            for _ in 0..20 {
                press(cx, &f, "j");
            }
            assert_eq!(tab_state(cx, &f).1, FIRST + 11);
            press(cx, &f, "1");
            assert_eq!(tab_state(cx, &f), (SettingsTab::Servers, 9));
        }

        #[gpui_kit::test]
        fn space_flips_each_option_saves_it_and_the_shell_follows(cx: &mut TestAppContext) {
            let f = fixture(cx, false);
            show(cx, &f);
            assert!(page_settings(cx, &f).enabled);
            press(cx, &f, "space");
            assert!(!page_settings(cx, &f).enabled);
            assert!(!shell_settings(cx, &f).enabled);
            press(cx, &f, "j");
            press(cx, &f, "space");
            press(cx, &f, "j");
            press(cx, &f, "space");
            press(cx, &f, "j");
            press(cx, &f, "space");
            let s = page_settings(cx, &f);
            assert!(!s.enabled && !s.only_when_unfocused);
            assert!(!s.suppress_current_change && !s.ignore_own_actions);
            assert_eq!(writes(&f).len(), 4);
            // Enter flips as well, back on.
            press(cx, &f, "enter");
            assert!(page_settings(cx, &f).ignore_own_actions);
            assert_eq!(shell_settings(cx, &f), page_settings(cx, &f));
        }

        #[gpui_kit::test]
        fn a_failed_save_changes_nothing_and_says_so(cx: &mut TestAppContext) {
            let f = fixture(cx, true);
            show(cx, &f);
            press(cx, &f, "space");
            assert!(page_settings(cx, &f).enabled);
            assert!(shell_settings(cx, &f).enabled);
            cx.update(|cx| {
                let page = f.shell.read(cx).settings_page_for_test().expect("page");
                let (text, error) = page.read(cx).hint_for_test().expect("hint");
                assert!(error && text.contains("disk full"), "{text}");
            });
        }

        #[gpui_kit::test]
        fn the_group_window_takes_whole_seconds(cx: &mut TestAppContext) {
            let f = fixture(cx, false);
            show(cx, &f);
            for _ in 0..4 {
                press(cx, &f, "j");
            }
            press(cx, &f, "enter");
            assert!(editing(cx, &f));
            let input = notify_input(cx, &f);
            // The input starts on the current value.
            assert_eq!(cx.update(|cx| input.read(cx).value().to_string()), "20");
            for bad in ["", "soon", "-3", "1.5", "99999"] {
                type_into(cx, &f, &input, bad);
                press(cx, &f, "enter");
                assert!(editing(cx, &f), "{bad:?}");
            }
            assert!(writes(&f).is_empty());
            type_into(cx, &f, &input, " 45 ");
            press(cx, &f, "enter");
            assert!(!editing(cx, &f));
            assert_eq!(page_settings(cx, &f).group_window.as_secs(), 45);
            assert_eq!(shell_settings(cx, &f).group_window.as_secs(), 45);
            // Zero: every event on its own.
            press(cx, &f, "e");
            type_into(cx, &f, &input, "0");
            press(cx, &f, "enter");
            assert_eq!(page_settings(cx, &f).group_window.as_secs(), 0);
            assert_eq!(writes(&f).len(), 2);
        }

        #[gpui_kit::test]
        fn quiet_hours_are_validated_and_can_be_cleared(cx: &mut TestAppContext) {
            let f = fixture(cx, false);
            show(cx, &f);
            for _ in 0..5 {
                press(cx, &f, "j");
            }
            press(cx, &f, "e");
            let input = notify_input(cx, &f);
            for bad in ["22-07", "25:00-07:00", "22:00", "22:00-7:00"] {
                type_into(cx, &f, &input, bad);
                press(cx, &f, "enter");
                assert!(editing(cx, &f), "{bad:?}");
            }
            assert!(writes(&f).is_empty());
            type_into(cx, &f, &input, "22:00-07:00");
            press(cx, &f, "enter");
            assert_eq!(
                page_settings(cx, &f).quiet_hours.map(|q| q.format()),
                Some("22:00-07:00".to_owned())
            );
            // The next edit starts on the saved text; empty clears it.
            press(cx, &f, "space");
            assert_eq!(
                cx.update(|cx| input.read(cx).value().to_string()),
                "22:00-07:00"
            );
            type_into(cx, &f, &input, "");
            press(cx, &f, "enter");
            assert_eq!(page_settings(cx, &f).quiet_hours, None);
            // Escape drops an edit.
            press(cx, &f, "e");
            type_into(cx, &f, &input, "01:00-02:00");
            press(cx, &f, "escape");
            assert!(!editing(cx, &f));
            assert_eq!(page_settings(cx, &f).quiet_hours, None);
            assert_eq!(writes(&f).len(), 2);
        }

        /// Where the rules start: the toggles and the timing rows are before.
        const RULES: usize = FIRST + 6;

        fn rule_titles(cx: &mut TestAppContext, f: &Fixture) -> Vec<String> {
            page_settings(cx, f)
                .rules
                .iter()
                .map(|r| r.title())
                .collect()
        }

        fn at(cx: &mut TestAppContext, f: &Fixture, row: usize) {
            cx.update(|cx| {
                let page = f.shell.read(cx).settings_page_for_test().expect("page");
                page.update(cx, |p, cx| p.select_row_for_test(row, cx));
            });
        }

        fn hint(cx: &mut TestAppContext, f: &Fixture) -> Option<(String, bool)> {
            cx.update(|cx| {
                let page = f.shell.read(cx).settings_page_for_test().expect("page");
                page.read(cx).hint_for_test()
            })
        }

        fn defaults(cx: &mut TestAppContext, f: &Fixture) -> bool {
            cx.update(|cx| {
                let page = f.shell.read(cx).settings_page_for_test().expect("page");
                page.read(cx).rules_are_defaults_for_test()
            })
        }

        #[gpui_kit::test]
        fn the_default_rules_are_listed_and_the_first_change_writes_them_all(
            cx: &mut TestAppContext,
        ) {
            let f = fixture(cx, false);
            show(cx, &f);
            assert!(defaults(cx, &f));
            assert_eq!(rule_titles(cx, &f).len(), 4);
            assert_eq!(rule_titles(cx, &f)[1], "Negative Code-Review on my changes");
            // A global option writes no rules of its own: the defaults stay
            // the defaults.
            assert!(writes(&f).is_empty());
            // Switching a rule off writes the whole list, with that one off.
            at(cx, &f, RULES);
            assert_eq!(tab_state(cx, &f).1, RULES);
            press(cx, &f, "space");
            let written = writes(&f);
            assert_eq!(written.len(), 1);
            assert_eq!(written[0].rules.len(), 4);
            assert!(!written[0].rules[0].enabled && written[0].rules[1].enabled);
            assert!(!defaults(cx, &f));
            assert!(!shell_settings(cx, &f).rules[0].enabled);
            // And back on.
            press(cx, &f, "space");
            assert!(page_settings(cx, &f).rules[0].enabled);
        }

        #[gpui_kit::test]
        fn rules_move_with_shift_j_and_k_and_the_cursor_follows(cx: &mut TestAppContext) {
            let f = fixture(cx, false);
            show(cx, &f);
            at(cx, &f, RULES);
            let before = rule_titles(cx, &f);
            press(cx, &f, "shift-j");
            let after = rule_titles(cx, &f);
            assert_eq!(after[1], before[0]);
            assert_eq!(after[0], before[1]);
            assert_eq!(tab_state(cx, &f).1, RULES + 1);
            press(cx, &f, "shift-k");
            assert_eq!(rule_titles(cx, &f), before);
            assert_eq!(tab_state(cx, &f).1, RULES);
            // The first rule cannot move up, the last cannot move down.
            press(cx, &f, "shift-k");
            assert_eq!(writes(&f).len(), 2);
            at(cx, &f, RULES + 3);
            press(cx, &f, "shift-j");
            assert_eq!(writes(&f).len(), 2);
        }

        #[gpui_kit::test]
        fn deleting_a_rule_asks_first_and_the_last_one_brings_the_defaults_back(
            cx: &mut TestAppContext,
        ) {
            let f = fixture(cx, false);
            show(cx, &f);
            at(cx, &f, RULES + 1);
            press(cx, &f, "d");
            press(cx, &f, "n");
            assert_eq!(rule_titles(cx, &f).len(), 4);
            assert!(writes(&f).is_empty());
            press(cx, &f, "d");
            press(cx, &f, "y");
            assert_eq!(rule_titles(cx, &f).len(), 3);
            assert_eq!(writes(&f).len(), 1);
            assert!(!defaults(cx, &f));
            // The cursor stays on the place of the deleted rule.
            assert_eq!(tab_state(cx, &f).1, RULES + 1);
            // Down to no rules: the file then has none, and the defaults
            // apply again, which the page says.
            for _ in 0..3 {
                at(cx, &f, RULES);
                press(cx, &f, "d");
                press(cx, &f, "enter");
            }
            assert!(writes(&f).last().expect("write").rules.is_empty());
            assert!(defaults(cx, &f));
            let (text, error) = hint(cx, &f).expect("hint");
            assert!(!error && text.contains("default rules"), "{text}");
        }

        #[gpui_kit::test]
        fn a_failed_delete_changes_nothing_and_says_so(cx: &mut TestAppContext) {
            let f = fixture(cx, true);
            show(cx, &f);
            at(cx, &f, RULES);
            press(cx, &f, "d");
            press(cx, &f, "y");
            assert_eq!(rule_titles(cx, &f).len(), 4);
            let (text, error) = hint(cx, &f).expect("hint");
            assert!(error && text.contains("disk full"), "{text}");
            // The question is over: a key works again.
            press(cx, &f, "j");
            assert_eq!(tab_state(cx, &f).1, RULES + 1);
        }

        #[gpui_kit::test]
        fn reset_to_defaults_asks_and_is_idle_on_the_defaults(cx: &mut TestAppContext) {
            let f = fixture(cx, false);
            show(cx, &f);
            let reset = RULES + 4;
            at(cx, &f, reset);
            press(cx, &f, "enter");
            let (text, _) = hint(cx, &f).expect("hint");
            assert!(text.contains("already"), "{text}");
            assert!(writes(&f).is_empty());

            at(cx, &f, RULES);
            press(cx, &f, "space");
            assert!(!defaults(cx, &f));
            at(cx, &f, reset);
            press(cx, &f, "enter");
            press(cx, &f, "n");
            assert!(!defaults(cx, &f));
            press(cx, &f, "enter");
            press(cx, &f, "y");
            assert!(defaults(cx, &f));
            assert!(page_settings(cx, &f).rules[0].enabled);
        }

        fn with_notifier(
            cx: &mut TestAppContext,
            f: &Fixture,
        ) -> Arc<crate::testing::RecordingNotifier> {
            let notifier = Arc::new(crate::testing::RecordingNotifier::default());
            // Off, so that nothing is watched and only the test is shown.
            let settings = NotificationSettings {
                enabled: false,
                ..Default::default()
            };
            let n = notifier.clone();
            cx.update_window(f.window, |_, window, cx| {
                f.shell
                    .update(cx, |s, cx| s.set_notifications(n, settings, window, cx))
            })
            .expect("window");
            cx.run_until_parked();
            notifier
        }

        #[gpui_kit::test]
        fn the_test_row_and_t_show_a_notification_through_the_notifier(cx: &mut TestAppContext) {
            let f = fixture(cx, false);
            let notifier = with_notifier(cx, &f);
            show(cx, &f);
            at(cx, &f, RULES + 5);
            press(cx, &f, "enter");
            assert_eq!(notifier.titles(), ["Test notification"]);
            let (text, error) = hint(cx, &f).expect("hint");
            assert!(!error && text.starts_with("Sent"), "{text}");
            assert!(text.contains("daemon"), "{text}");
            // `t` works from any row of the tab, and while the master
            // switch is off.
            at(cx, &f, FIRST);
            press(cx, &f, "t");
            assert_eq!(notifier.shown().len(), 2);
        }

        #[gpui_kit::test]
        fn without_a_notifier_the_test_says_so(cx: &mut TestAppContext) {
            let f = fixture(cx, false);
            show(cx, &f);
            press(cx, &f, "t");
            let (text, error) = hint(cx, &f).expect("hint");
            assert!(error && text.contains("not available"), "{text}");
        }

        #[gpui_kit::test]
        fn tab_and_row_keys_are_typed_while_an_input_is_edited(cx: &mut TestAppContext) {
            let f = fixture(cx, false);
            show(cx, &f);
            for _ in 0..4 {
                press(cx, &f, "j");
            }
            press(cx, &f, "e");
            for key in ["h", "l", "6", "1", "j", "space"] {
                press(cx, &f, key);
            }
            assert_eq!(tab_state(cx, &f), (SettingsTab::Notifications, FIRST + 4));
            assert!(editing(cx, &f));
            assert!(writes(&f).is_empty());
        }
    }
}

mod goto {
    use gpui_kit::{AnyWindowHandle, ClipboardItem};
    use model::PatchSetNumber;

    use super::*;
    use crate::goto::LinkError;
    use crate::nav::DiffRoute;

    fn start(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Shell>) {
        let env = env_with(seeded(&[("status:open", &[1])]), &FakeHttpClient::new());
        let startup = Startup {
            env,
            diff: Default::default(),
            start_page: Default::default(),
            filters: Vec::new(),
            servers: vec![crate::session::ServerSetup {
                queries: vec![spec("Open", "status:open")],
                bookmarks: Vec::new(),
                ..setup()
            }],
            active: None,
        };
        let (window, shell) = open(cx, |window, cx| {
            cx.new(|cx| Shell::new(startup, window, cx))
        });
        cx.run_until_parked();
        (window, shell)
    }

    fn press(cx: &mut TestAppContext, window: AnyWindowHandle, key: &'static str) {
        cx.update_window(window, |_, window, cx| window.press(key, cx))
            .expect("press");
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn g_fills_in_the_clipboard_and_enter_opens_the_change(cx: &mut TestAppContext) {
        let (window, shell) = start(cx);
        cx.update(|cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(
                "\n  https://g.example.com/c/proj/+/8/2..3  \nmore".into(),
            ))
        });
        press(cx, window, "g");
        let dialog = cx.update(|cx| shell.read(cx).goto_for_test().expect("dialog"));
        cx.update(|cx| {
            assert_eq!(
                dialog.read(cx).text_for_test(cx),
                "https://g.example.com/c/proj/+/8/2..3"
            )
        });
        press(cx, window, "enter");
        cx.update(|cx| {
            let shell = shell.read(cx);
            assert!(shell.goto_for_test().is_none());
            assert_eq!(shell.route(), &Route::Change(ChangeNumber(8)));
            let page = shell.change_page_for_test().expect("change page");
            let view = page.read(cx).view_state();
            assert_eq!(view.base, Some(PatchSetNumber(2)));
            assert_eq!(view.target, Some(PatchSetNumber(3)));
        });
        // Back leads to where we were.
        press(cx, window, "alt-left");
        cx.update(|cx| assert_eq!(shell.read(cx).route(), &Route::Dashboard));
    }

    #[gpui_kit::test]
    fn a_bad_link_says_why_and_esc_closes(cx: &mut TestAppContext) {
        let (window, shell) = start(cx);
        press(cx, window, "g");
        let dialog = cx.update(|cx| shell.read(cx).goto_for_test().expect("dialog"));
        cx.update_window(window, |_, window, cx| {
            dialog.update(cx, |d, cx| {
                d.submit_for_test("https://other.example.com/c/p/+/1", window, cx)
            })
        })
        .expect("submit");
        cx.update(|cx| {
            assert_eq!(
                dialog.read(cx).error_for_test(),
                Some(LinkError::UnknownServer("https://other.example.com".into()))
            )
        });
        press(cx, window, "escape");
        cx.update(|cx| {
            assert!(shell.read(cx).goto_for_test().is_none());
            assert_eq!(shell.read(cx).route(), &Route::Dashboard);
        });
    }

    #[gpui_kit::test]
    fn open_link_goes_to_the_diff_or_asks(cx: &mut TestAppContext) {
        let (window, shell) = start(cx);
        cx.update_window(window, |_, window, cx| {
            shell.update(cx, |s, cx| {
                s.open_link("https://g.example.com/c/proj/+/8/3/src/a.rs", window, cx)
            })
        })
        .expect("open link");
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(
                shell.read(cx).route(),
                &Route::Diff(DiffRoute {
                    change: ChangeNumber(8),
                    base: None,
                    target: PatchSetNumber(3),
                    path: "src/a.rs".into(),
                    thread: None,
                })
            )
        });

        cx.update_window(window, |_, window, cx| {
            shell.update(cx, |s, cx| s.open_link("not a link", window, cx))
        })
        .expect("open link");
        cx.run_until_parked();
        cx.update(|cx| {
            let dialog = shell.read(cx).goto_for_test().expect("dialog");
            assert_eq!(dialog.read(cx).text_for_test(cx), "not a link");
            assert_eq!(
                dialog.read(cx).error_for_test(),
                Some(LinkError::NotAChange)
            );
        });
    }
}

/// Links and the add-server flow with several configured servers.
mod links {
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
}

// --- the servers section of the settings page ----------------------------

mod servers_settings {
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
    fn renaming_and_coloring_need_no_password(cx: &mut TestAppContext) {
        let f = two(cx);
        let page = on_server(cx, &f, 0);
        let before = f.http.requests().len();
        press(cx, &f, "e");
        let [name, _url, _user, color, _pw] =
            cx.update(|cx| page.read(cx).server_inputs_for_test());
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
        let conn =
            |cx: &mut TestAppContext| cx.update(|cx| f.shell.read(cx).connection_for_test("a"));
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
}

// ---- the relation chain across pages ----

mod relation_chain {
    use gpui_kit::{AnyWindowHandle, ScrollHandle, VisualTestContext};

    use super::*;
    use crate::pages::change::ChangePage;

    /// Changes 100..100+n in one chain (100 the tip), each with a cached
    /// detail; only `at`'s chain is cached, so the others' pages show the
    /// chain they are handed until a fetch that never succeeds.
    fn start(cx: &mut TestAppContext, n: u64, at: u64) -> (AnyWindowHandle, Entity<Shell>) {
        let mut store = seeded(&[]);
        let ts = "2024-06-15T10:05:00Z".parse().expect("ts");
        let chain: Vec<model::RelatedChange> = (0..n)
            .map(|i| model::RelatedChange {
                project: "p".into(),
                change_id: None,
                number: ChangeNumber(100 + i),
                patch_set: model::PatchSetNumber(1),
                current_patch_set: Some(model::PatchSetNumber(1)),
                status: Some(model::ChangeStatus::New),
                submittable: None,
                work_in_progress: false,
                commit: None,
            })
            .collect();
        for i in 0..n {
            store
                .save_change_detail(ServerId(1), &crate::testing::detail(100 + i, 1, 1), ts)
                .expect("detail");
        }
        store
            .save_related(ServerId(1), ChangeNumber(at), &"sha1".into(), &chain, ts)
            .expect("related");
        let startup = Startup {
            env: env_with(store, &FakeHttpClient::new()),
            diff: Default::default(),
            start_page: Default::default(),
            filters: Vec::new(),
            servers: vec![setup()],
            active: None,
        };
        let (window, shell) = open(cx, |window, cx| {
            cx.new(|cx| Shell::new(startup, window, cx))
        });
        cx.run_until_parked();
        go(cx, window, &shell, at);
        (window, shell)
    }

    fn go(cx: &mut TestAppContext, window: AnyWindowHandle, shell: &Entity<Shell>, n: u64) {
        cx.update_window(window, |_, window, cx| {
            shell.update(cx, |s, cx| {
                s.navigate_for_test(Route::Change(ChangeNumber(n)), window, cx)
            })
        })
        .expect("navigate");
        cx.run_until_parked();
    }

    fn press(cx: &mut TestAppContext, window: AnyWindowHandle, key: &'static str) {
        cx.update_window(window, |_, window, cx| window.press(key, cx))
            .expect("press");
    }

    fn page(cx: &mut TestAppContext, shell: &Entity<Shell>) -> Entity<ChangePage> {
        cx.update(|cx| shell.read(cx).change_page_for_test().expect("change page"))
    }

    fn scroll(cx: &mut TestAppContext, shell: &Entity<Shell>) -> ScrollHandle {
        let page = page(cx, shell);
        cx.update(|cx| page.read(cx).related_scroll_for_test())
    }

    /// How far row `ix`'s middle sits from the middle of the scroll box.
    fn off_centre(s: &ScrollHandle, ix: usize) -> f32 {
        let row = s.bounds_for_item(ix).expect("row").center().y + s.offset().y;
        (row - s.bounds().center().y).as_f32().abs()
    }

    #[gpui_kit::test]
    fn the_box_keeps_the_current_change_in_the_middle_while_stepping(cx: &mut TestAppContext) {
        let (window, shell) = start(cx, 40, 120);
        let s = scroll(cx, &shell);
        assert!(s.max_offset().y > px(0.), "the chain overflows its box");
        assert!(off_centre(&s, 20) < 1., "{}", off_centre(&s, 20));

        press(cx, window, "shift-j");
        cx.run_until_parked();
        let s = scroll(cx, &shell);
        cx.update(|cx| assert_eq!(shell.read(cx).route(), &Route::Change(ChangeNumber(121))));
        assert!(off_centre(&s, 21) < 1., "{}", off_centre(&s, 21));
    }

    #[gpui_kit::test]
    fn the_panel_keeps_its_chain_and_scroll_position_while_stepping(cx: &mut TestAppContext) {
        let (window, shell) = start(cx, 80, 140);
        VisualTestContext::from_window(window, cx).simulate_resize(size(px(1400.), px(800.)));
        cx.run_until_parked();
        press(cx, window, "shift-l");
        cx.run_until_parked();
        let p = page(cx, &shell);
        assert!(cx.update(|cx| p.read(cx).panel_shown_for_test()));
        let s = scroll(cx, &shell);
        assert!(s.max_offset().y > px(0.), "the chain overflows the panel");
        // The user scrolls a little; the current row stays in view.
        let row = s.bounds_for_item(40).expect("row").size.height;
        s.set_offset(gpui_kit::point(px(0.), s.offset().y - row));
        cx.run_until_parked();
        let before = s.offset().y;

        // The new page has the chain at once, before reading its own.
        press(cx, window, "shift-j");
        let next = page(cx, &shell);
        assert_ne!(next, p);
        assert_eq!(cx.update(|cx| next.read(cx).chain_len_for_test()), 80);
        cx.run_until_parked();
        assert!(cx.update(|cx| next.read(cx).panel_shown_for_test()));
        assert_eq!(scroll(cx, &shell).offset().y, before);

        // A change outside the chain starts afresh.
        go(cx, window, &shell, 7);
        let fresh = page(cx, &shell);
        assert_eq!(cx.update(|cx| fresh.read(cx).chain_len_for_test()), 0);
    }
}

/// The reply dialog's local reviews as the shell handles them.
mod local_review {
    use gerrit_api::fake::FakeHttpClient;
    use gpui_kit::{AppContext as _, TestAppContext};
    use model::ChangeNumber;
    use store::Store;

    use super::{URL, env_with, open, setup};
    use crate::{
        nav::Route,
        reply::ReplyRequest,
        session::{Session, Startup},
        shell::Shell,
    };

    #[gpui_kit::test]
    fn a_saved_local_review_closes_the_dialog_and_the_page_says_so(cx: &mut TestAppContext) {
        let mut store = Store::open_in_memory().expect("store");
        let server = store.server_id(URL).expect("server");
        let mut d = crate::testing::detail(7, 1, 1);
        d.summary.project = "p".into();
        d.permitted_labels = [("Code-Review".to_owned(), vec![-1, 0, 1])].into();
        store
            .save_change_detail(server, &d, jiff::Timestamp::now())
            .expect("detail");
        let version = crate::testing::local_version(&mut store, Some(ChangeNumber(7)), "I7", "l7");
        let http = FakeHttpClient::new();
        let env = env_with(store, &http);
        let detail = {
            let session = Session::new(&env, &setup(), None).expect("session");
            crate::change_detail::cached(session.syncer(), ChangeNumber(7))
                .expect("store")
                .expect("cached")
                .detail
        };
        let startup = Startup {
            env,
            diff: Default::default(),
            start_page: Default::default(),
            filters: Vec::new(),
            servers: vec![setup()],
            active: None,
        };
        let (window, shell) = open(cx, |window, cx| {
            cx.new(|cx| Shell::new(startup, window, cx))
        });
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            shell.update(cx, |s, cx| {
                s.navigate_for_test(Route::Change(ChangeNumber(7)), window, cx);
                s.open_reply_for_test(
                    ReplyRequest {
                        detail,
                        target: Some(version.number),
                        edit: None,
                        strategy: model::LocalApproval::Exact,
                    },
                    window,
                    cx,
                );
            })
        })
        .expect("open");
        cx.run_until_parked();
        let dialog = cx
            .update(|cx| shell.read(cx).reply_for_test())
            .expect("the dialog");
        cx.update_window(window, |_, window, cx| {
            dialog.update(cx, |d, cx| {
                d.set_text_for_test("Looks right", window, cx);
                d.send(window, cx);
            })
        })
        .expect("send");
        cx.run_until_parked();
        cx.update(|cx| {
            let s = shell.read(cx);
            assert!(s.reply_for_test().is_none(), "the dialog closed");
            let page = s.change_page_for_test().expect("change page");
            assert_eq!(
                page.read(cx).hint_for_test().map(|(t, _)| t),
                Some(crate::reply::local_note("local 1"))
            );
        });
        // The page reads Gerrit; nothing is written there.
        let writes: Vec<_> = http
            .requests()
            .into_iter()
            .filter(|r| r.method() != "GET")
            .collect();
        assert!(writes.is_empty(), "{writes:?}");
    }
}
