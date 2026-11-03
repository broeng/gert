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
/// The application name the test environments pass in.
pub(crate) const APP: &str = "test-app";

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
        app_name: APP.into(),
        marks: Default::default(),
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

mod drafts;
mod goto;
mod links;
mod local_review;
mod relation_chain;
mod servers_settings;
mod settings_page;
