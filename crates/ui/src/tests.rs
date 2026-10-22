//! View tests on GPUI's `TestAppContext`. No network: sessions either
//! cannot sync or talk to a scripted fake.

use std::sync::{Arc, Mutex};

use gerrit_api::fake::FakeHttpClient;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, Entity, TestAppContext, WindowOptions, px, size};
use model::{ChangeDetail, ChangeNumber};
use store::{ServerId, Store};

use crate::{
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
    };
    let (_window, shell) = open(cx, |window, cx| {
        cx.new(|cx| Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    cx.update(|cx| assert_eq!(shell.read(cx).route(), &Route::Login));
}

#[gpui_kit::test]
fn search_pages_through_cached_rows_with_n_and_p(cx: &mut TestAppContext) {
    let numbers: Vec<u64> = (1..=30).collect();
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
    cx.update(|cx| assert_eq!(page.read(cx).visible_rows(0).len(), 25));
    press(cx, "n");
    cx.update(|cx| {
        let p = page.read(cx);
        assert_eq!(p.visible_rows(0).len(), 5);
        assert_eq!(p.selected_number(), Some(ChangeNumber(26)));
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
        outbox::DraftOutbox,
        pages::list::ListEvent,
        session::{Backend, BackendError, SavedQuery, ServerSetup, Startup},
        shell::Shell,
    };

    /// A keyring that has the password.
    struct Keyring;

    impl Backend for Keyring {
        fn load_password(&self, _: &str, _: &str) -> Result<Option<String>, BackendError> {
            Ok(Some("secret".into()))
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
