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
        base_url: URL.into(),
        username: None,
        server: ServerId(1),
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
        server: Some(setup()),
        queries: vec![spec("Open", "status:open")],
        diff: Default::default(),
        start_page: Default::default(),
        filters: Vec::new(),
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
        server: Some(setup()),
        queries: vec![spec("Open", "status:open")],
        diff: Default::default(),
        start_page: Default::default(),
        filters: Vec::new(),
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
        server: Some(setup()),
        queries: vec![spec("Open", "status:open")],
        diff: crate::LocalDiff {
            preferences: DiffPreferences {
                tab_size: Some(2),
                ..Default::default()
            },
            font_family: Some("Iosevka".into()),
        },
        start_page: Default::default(),
        filters: Vec::new(),
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
        outbox::DraftOutbox,
        pages::list::ListEvent,
        session::{Backend, BackendError, SavedQuery, ServerSetup, Startup, StartupPage},
        shell::Shell,
    };

    /// A keyring that has the password.
    struct Keyring;

    impl Backend for Keyring {
        fn load_password(&self, _: &str, _: &str) -> Result<Option<String>, BackendError> {
            Ok(Some("secret".into()))
        }

        fn save_queries(&self, _: &[SavedQuery]) -> Result<Vec<SavedQuery>, BackendError> {
            Err(BackendError("not used".into()))
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
            server: Some(ServerSetup {
                username: Some("u".into()),
                ..setup()
            }),
            queries: vec![spec("Open", "status:open")],
            diff: Default::default(),
            start_page: Default::default(),
            filters: Vec::new(),
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
        let backend = Arc::new(FakeBackend {
            fail,
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
            server: Some(setup()),
            queries: vec![spec("Open", "status:open"), spec("Mine", "owner:self")],
            diff: Default::default(),
            start_page,
            filters,
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
        // Two query rows, the startup row, then the advance row.
        for _ in 0..3 {
            press(cx, &f, "j");
        }
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
        for _ in 0..3 {
            press(cx, &f, "j");
        }
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
        // Two query rows, the startup and advance rows, then the theme row.
        for _ in 0..4 {
            press(cx, &f, "j");
        }
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
        // Two query rows, the startup, advance and theme rows, then the UI
        // mode row: system, light, dark.
        for _ in 0..5 {
            press(cx, &f, "j");
        }
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
        for _ in 0..4 {
            press(cx, &f, "j");
        }
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
        for _ in 0..2 {
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

    /// Cursor on the first filter row: 2 queries, startup, advance, theme, two modes.
    fn to_filters(cx: &mut TestAppContext, f: &Fixture) {
        for _ in 0..7 {
            press(cx, f, "j");
        }
    }

    #[gpui_kit::test]
    fn the_filter_rows_follow_the_cursor_after_the_other_rows(cx: &mut TestAppContext) {
        let f = fixture_with(cx, false, StartupPage::Dashboard, two_rules());
        open_settings(cx, &f);
        assert_eq!(page_state(cx, &f).1, ["gen", "lock"]);
        to_filters(cx, &f);
        assert_eq!(page_state(cx, &f).0, 7);
        press(cx, &f, "j");
        press(cx, &f, "j");
        // The last rule is the last row.
        assert_eq!(page_state(cx, &f).0, 8);
        press(cx, &f, "k");
        assert_eq!(page_state(cx, &f).0, 7);
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
        assert_eq!(page_state(cx, &f).0, 7);
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
        press(cx, &f, "j");
        assert_eq!(page_state(cx, &f), (7, Vec::new()));
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
        assert_eq!(page_state(cx, &f), (7, vec!["lock".into(), "gen".into()]));
        // Removing the rule under the cursor clamps it.
        cx.update(|cx| {
            filters.update(cx, |ff, cx| {
                ff.set_rules(vec![rule("gen", FilterAction::Dim)], cx)
            })
        });
        cx.run_until_parked();
        assert_eq!(page_state(cx, &f), (7, vec!["gen".into()]));
        cx.update(|cx| filters.update(cx, |ff, cx| ff.set_rules(Vec::new(), cx)));
        cx.run_until_parked();
        assert_eq!(page_state(cx, &f), (7, Vec::new()));
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
            server: Some(setup()),
            queries: vec![spec("Open", "status:open")],
            diff: Default::default(),
            start_page: Default::default(),
            filters: Vec::new(),
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
                Some(LinkError::OtherServer("other.example.com".into()))
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
