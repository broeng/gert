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
    pages::{
        change::ChangePage,
        list::{ChangeListPage, ListKind},
    },
    session::{Env, SavedQuery, ServerSetup, Session, Startup},
    shell::Shell,
    testing::summary,
};

const URL: &str = "https://g.example.com";

fn setup() -> ServerSetup {
    ServerSetup {
        base_url: URL.into(),
        username: None,
        server: ServerId(1),
    }
}

fn env_with(store: Store, http: &FakeHttpClient) -> Env {
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

fn open<V: gpui_kit::Render + 'static>(
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
        current_patch_set: Some(2),
        commit_message: Some("Subject\n\nBody".into()),
        reviewers: vec![],
        cc: vec![],
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
    };
    let (_window, shell) = open(cx, |window, cx| {
        cx.new(|cx| Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    cx.update(|cx| assert_eq!(shell.read(cx).route(), &Route::Login));
}

#[gpui_kit::test]
fn change_page_shows_cached_detail(cx: &mut TestAppContext) {
    let mut store = seeded(&[]);
    let detail = ChangeDetail {
        summary: summary(5),
        current_revision: None,
        current_patch_set: Some(1),
        commit_message: Some("Fix things".into()),
        reviewers: vec![],
        cc: vec![],
    };
    store
        .save_change_detail(
            ServerId(1),
            &detail,
            "2024-06-15T10:05:00Z".parse().expect("ts"),
        )
        .expect("save detail");
    let env = env_with(store, &FakeHttpClient::new());
    let session = Session::new(&env, &setup(), None).expect("session");
    let (_w, page) = open(cx, |_, cx| {
        cx.new(|cx| ChangePage::new(session, ChangeNumber(5), true, cx))
    });
    cx.run_until_parked();
    cx.update(|cx| {
        let d = page.read(cx).detail().expect("cached detail");
        assert_eq!(d.summary.number, ChangeNumber(5));
    });
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
