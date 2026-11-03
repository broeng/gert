//! The scanner and the dashboard's "Local changes" section, against real
//! temporary repositories (local-git's `TestRepo`, whose repository-level
//! emails override the developer's git configuration) and a shell on a
//! scripted server. The fake HTTP client panics on any synthetic number,
//! so every test also checks that none reaches the wire.

use std::sync::{Arc, Mutex};

use gerrit_api::fake::RouteHttpClient;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
use local_git::testutil::{TEST_EMAIL, TestRepo};
use model::{
    ChangeNumber, DraftInput, LocalChangeState, Location, PatchSetRef, RevisionId, Side, Timestamp,
};
use store::{LocalChangeUpsert, LocalOp, NewLocalPatchSet, ServerId, Store};

use super::LocalScanner;
use crate::{
    checkouts::Checkouts,
    login::tests::FakeBackend,
    nav::Route,
    pages::list::{ChangeListPage, ListEvent, ListKind},
    session::{ProjectSettings, SavedQuery, ServerSetup, Session, Startup},
    shell::Shell,
    testing::{detail, summary},
    tests::{URL, env_with, open, setup},
};

const ID_A: &str = "I0123456789abcdef0123456789abcdef01234567";
const ID_B: &str = "Ifedcba9876543210fedcba9876543210fedcba98";
const ID_C: &str = "I00000000000000000000000000000000000000cc";

const SERVER: ServerId = ServerId(1);

fn ts() -> Timestamp {
    "2024-06-15T10:05:00Z".parse().expect("timestamp")
}

/// A checkout of project `p` whose `main` tracks `origin/main` of a bare
/// remote, with one pushed commit.
struct Fixture {
    _remote: TestRepo,
    repo: TestRepo,
    http: RouteHttpClient,
}

fn fixture() -> Fixture {
    let remote = TestRepo::bare();
    let repo = TestRepo::new();
    repo.commit("Base");
    let url = remote.path().to_str().expect("utf-8 temp path").to_owned();
    repo.git(&["remote", "add", "origin", &url]);
    repo.git(&["push", "-q", "-u", "origin", "main"]);
    Fixture {
        _remote: remote,
        repo,
        http: RouteHttpClient::new(),
    }
}

impl Fixture {
    /// A commit of the user's own (by the checkout's email).
    fn own(&self, subject: &str, change_id: &str) -> String {
        self.repo
            .commit(&format!("{subject}\n\nChange-Id: {change_id}"))
    }

    /// A commit by `author` (`Name <email>`).
    fn by(&self, author: &str, subject: &str, change_id: &str) -> String {
        let message = format!("{subject}\n\nChange-Id: {change_id}");
        let author = format!("--author={author}");
        self.repo
            .git(&["commit", "-q", "--allow-empty", &author, "-m", &message]);
        self.repo.git(&["rev-parse", "HEAD"])
    }

    fn project(&self, local_changes: bool) -> ProjectSettings {
        ProjectSettings {
            local_changes,
            ..ProjectSettings::new("p", self.repo.path().to_string_lossy())
        }
    }
}

struct Started {
    window: AnyWindowHandle,
    shell: Entity<Shell>,
    store: Arc<Mutex<Store>>,
}

/// A shell on server `test` (signed in as `username`, the password in the
/// keyring) with project `p` configured at the fixture's checkout.
fn start(
    cx: &mut TestAppContext,
    f: &Fixture,
    username: Option<&str>,
    local_changes: bool,
) -> Started {
    let mut store = Store::open_in_memory().expect("store");
    store.server_id(URL).expect("server id");
    start_with(cx, f, store, username, local_changes)
}

/// [`start`] on `store`, whose server row for [`URL`] exists.
fn start_with(
    cx: &mut TestAppContext,
    f: &Fixture,
    mut store: Store,
    username: Option<&str>,
    local_changes: bool,
) -> Started {
    assert_eq!(store.server_id(URL).expect("server id"), SERVER);
    let mut env = env_with(store, &f.http);
    env.backend = Arc::new(FakeBackend {
        password: Some("pw".into()),
        ..Default::default()
    });
    let store = env.store.clone();
    let startup = Startup {
        env,
        diff: Default::default(),
        start_page: Default::default(),
        filters: Vec::new(),
        servers: vec![ServerSetup {
            username: username.map(str::to_owned),
            queries: vec![SavedQuery {
                name: "Open".into(),
                query: "status:open".into(),
            }],
            projects: vec![f.project(local_changes)],
            ..setup()
        }],
        active: None,
    };
    let (window, shell) = open(cx, |window, cx| {
        cx.new(|cx| Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    Started {
        window,
        shell,
        store,
    }
}

impl Started {
    fn page(&self, cx: &mut TestAppContext) -> Entity<ChangeListPage> {
        cx.update(|cx| self.shell.read(cx).list_page_for_test())
            .expect("the dashboard is shown")
    }

    fn scanner(&self, cx: &mut TestAppContext) -> Option<Entity<LocalScanner>> {
        cx.update(|cx| self.shell.read(cx).local_scan_for_test("test"))
    }

    /// The subjects of the dashboard's local changes, sorted.
    fn subjects(&self, cx: &mut TestAppContext) -> Vec<String> {
        let page = self.page(cx);
        let mut subjects: Vec<String> = cx.update(|cx| {
            page.read(cx)
                .local_rows()
                .iter()
                .map(|c| c.subject.clone())
                .collect()
        });
        subjects.sort();
        subjects
    }

    fn full_scans(&self, cx: &mut TestAppContext) -> usize {
        let scanner = self.scanner(cx).expect("a scanner");
        cx.update(|cx| scanner.read(cx).full_scans())
    }

    /// Asks for a round, as the poll tick does, and waits for it.
    fn rescan(&self, cx: &mut TestAppContext) {
        let scanner = self.scanner(cx).expect("a scanner");
        scanner.update(cx, |s, cx| s.request(cx));
        cx.run_until_parked();
        assert!(cx.update(|cx| scanner.read(cx).is_idle()));
    }

    fn press(&self, cx: &mut TestAppContext, key: &str) {
        use gpui_kit::test::TestWindowExt as _;
        cx.update_window(self.window, |_, window, cx| window.press(key, cx))
            .expect("press");
    }

    /// No request so far named a synthetic number (the fake would have
    /// panicked, but a request log is the plainer evidence).
    fn assert_no_local_number_sent(&self, f: &Fixture) {
        let base = ChangeNumber::LOCAL_BASE.to_string();
        let prefix = &base[..6];
        for r in f.http.requested() {
            assert!(!r.contains(prefix), "{r}");
        }
    }
}

#[gpui_kit::test]
fn own_commits_show_in_the_dashboard_and_other_authors_do_not(cx: &mut TestAppContext) {
    let f = fixture();
    f.own("Own change", ID_A);
    f.by("Other Person <other@example.com>", "Their change", ID_B);
    let s = start(cx, &f, Some("u"), true);
    assert_eq!(s.subjects(cx), ["Own change"]);
    let page = s.page(cx);
    cx.update(|cx| {
        let row = &page.read(cx).local_rows()[0];
        assert!(row.number.is_local());
        assert_eq!(row.state, LocalChangeState::Checking);
        assert_eq!(row.branch.as_deref(), Some("main"));
        assert_eq!(row.author_email, TEST_EMAIL);
    });

    // A commit made while the app runs shows after the next round.
    f.own("Second change", ID_C);
    s.rescan(cx);
    assert_eq!(s.subjects(cx), ["Own change", "Second change"]);
    s.assert_no_local_number_sent(&f);
}

/// How many times the account was asked for.
fn account_reads(f: &Fixture) -> usize {
    f.http
        .requested()
        .iter()
        .filter(|r| r.contains("/accounts/self/detail"))
        .count()
}

#[gpui_kit::test]
fn the_account_email_arriving_brings_in_its_commits(cx: &mut TestAppContext) {
    let f = fixture();
    f.by("Me <ME@corp.example>", "Under the account email", ID_A);
    // The server does not answer yet: the account is not asked for, and
    // only the checkout's email counts.
    let s = start(cx, &f, Some("u"), true);
    assert!(s.subjects(cx).is_empty());
    assert_eq!(account_reads(&f), 0);
    let scans = s.full_scans(cx);

    // It answers: the first refresh that works reads the account once, and
    // the email starts another round.
    f.http.route_json("/changes/", "[]");
    f.http.route_json(
        "/accounts/self/detail",
        r#"{"_account_id": 1, "email": "me@corp.example"}"#,
    );
    s.press(cx, "shift-r");
    cx.run_until_parked();
    assert_eq!(account_reads(&f), 1);
    assert_eq!(s.full_scans(cx), scans + 1);
    assert_eq!(s.subjects(cx), ["Under the account email"]);
    assert_eq!(
        s.store
            .lock()
            .expect("store")
            .account_email(SERVER)
            .expect("email")
            .as_deref(),
        Some("me@corp.example")
    );
    s.press(cx, "shift-r");
    cx.run_until_parked();
    assert_eq!(account_reads(&f), 1, "known now");
}

#[gpui_kit::test]
fn the_cached_account_email_counts_offline(cx: &mut TestAppContext) {
    let f = fixture();
    f.by("Me <me@corp.example>", "Under the account email", ID_A);
    let mut store = Store::open_in_memory().expect("store");
    let server = store.server_id(URL).expect("server id");
    store
        .set_account_email(server, Some("me@corp.example"))
        .expect("email");
    let s = start_with(cx, &f, store, Some("u"), true);
    assert_eq!(s.subjects(cx), ["Under the account email"]);
    assert_eq!(account_reads(&f), 0);
}

#[gpui_kit::test]
fn a_failing_account_read_waits_for_the_poll_interval(cx: &mut TestAppContext) {
    let f = fixture();
    f.http.route_json("/changes/", "[]");
    f.http.route("/accounts/self/detail", 500, "broken");
    let s = start(cx, &f, Some("u"), true);
    assert_eq!(account_reads(&f), 1);
    // More refreshes work, but the account is not asked for again yet.
    s.press(cx, "shift-r");
    cx.run_until_parked();
    s.press(cx, "shift-r");
    cx.run_until_parked();
    assert_eq!(account_reads(&f), 1);
}

#[gpui_kit::test]
fn a_round_skips_the_scan_until_the_refs_or_the_cache_move(cx: &mut TestAppContext) {
    let f = fixture();
    let sha = f.own("Own change", ID_A);
    let s = start(cx, &f, Some("u"), true);
    assert_eq!(s.subjects(cx), ["Own change"]);
    let scans = s.full_scans(cx);
    assert!(scans >= 1);

    // Nothing moved: the round stops after `for-each-ref`.
    s.rescan(cx);
    s.rescan(cx);
    assert_eq!(s.full_scans(cx), scans);

    // A new commit moves the refs.
    f.own("Second change", ID_B);
    s.rescan(cx);
    assert_eq!(s.full_scans(cx), scans + 1);
    s.rescan(cx);
    assert_eq!(s.full_scans(cx), scans + 1);

    // The first commit was pushed for review (no local ref moves) and
    // its change detail is cached: the generation moves, and the
    // change is no longer local.
    let mut d = detail(42, 1, 1);
    d.summary.project = "p".into();
    d.summary.change_id = Some(ID_A.into());
    d.revisions[0].sha = RevisionId(sha.clone());
    d.current_revision = Some(RevisionId(sha));
    s.store
        .lock()
        .expect("store")
        .save_change_detail(SERVER, &d, ts())
        .expect("save detail");
    s.rescan(cx);
    assert_eq!(s.full_scans(cx), scans + 2);
    assert_eq!(s.subjects(cx), ["Second change"]);
    s.rescan(cx);
    assert_eq!(s.full_scans(cx), scans + 2);
    s.assert_no_local_number_sent(&f);
}

/// A draft on the first version of the local change with `change_id`.
fn draft_on(store: &Mutex<Store>, change_id: &str) -> model::Draft {
    let mut store = store.lock().expect("store");
    let row = store
        .local_changes_by_change_id(SERVER, change_id)
        .expect("rows")
        .remove(0);
    let version = store
        .local_patch_sets(SERVER, row.id)
        .expect("versions")
        .remove(0);
    let input = DraftInput {
        path: "a.rs".into(),
        side: Side::Revision,
        parent: None,
        location: Location::Line(1),
        in_reply_to: None,
        message: "keep me".into(),
        unresolved: true,
    };
    let target = PatchSetRef {
        number: version.number,
        sha: version.sha,
    };
    store
        .create_draft(SERVER, row.number, &target, &input, ts())
        .expect("draft")
}

#[gpui_kit::test]
fn a_change_merged_on_gerrit_goes_once_a_query_shows_it(cx: &mut TestAppContext) {
    let f = fixture();
    f.own("Merged elsewhere", ID_A);
    f.own("Still mine", ID_B);
    let s = start(cx, &f, Some("u"), true);
    assert_eq!(s.subjects(cx), ["Merged elsewhere", "Still mine"]);
    let scans = s.full_scans(cx);
    // Only a summary, as a dashboard query caches it: no detail, and no
    // local ref moves.
    let mut merged = summary(77);
    merged.project = "p".into();
    merged.change_id = Some(ID_A.into());
    merged.status = model::ChangeStatus::Merged;
    s.store
        .lock()
        .expect("store")
        .save_query_page(SERVER, "status:merged", &[merged], false, ts())
        .expect("save query");
    s.rescan(cx);
    assert_eq!(s.full_scans(cx), scans + 1);
    assert_eq!(s.subjects(cx), ["Still mine"]);
}

#[gpui_kit::test]
fn unsent_data_coming_and_going_is_seen_without_a_ref_moving(cx: &mut TestAppContext) {
    let f = fixture();
    f.own("Keeps a draft", ID_A);
    let s = start(cx, &f, Some("u"), true);
    assert_eq!(s.subjects(cx), ["Keeps a draft"]);
    let draft = draft_on(&s.store, ID_A);
    let scans = s.full_scans(cx);
    s.rescan(cx);
    assert_eq!(s.full_scans(cx), scans + 1, "the row has unsent data now");
    s.rescan(cx);
    assert_eq!(s.full_scans(cx), scans + 1);

    // The commit leaves the branch: the row stays, dimmed, for its draft.
    f.repo.git(&["reset", "-q", "--hard", "HEAD~1"]);
    s.rescan(cx);
    let page = s.page(cx);
    let state = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            page.read(cx)
                .local_rows()
                .iter()
                .map(|c| c.state)
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(state(cx), [LocalChangeState::Vanished]);
    s.rescan(cx);
    assert_eq!(s.full_scans(cx), scans + 2, "settled");

    // The draft is deleted (no ref moves): the row goes.
    s.store
        .lock()
        .expect("store")
        .delete_draft(SERVER, draft.local_id)
        .expect("delete");
    s.rescan(cx);
    assert_eq!(s.full_scans(cx), scans + 3);
    assert!(state(cx).is_empty());
}

#[gpui_kit::test]
fn a_truncated_scan_drops_nothing_and_settles(cx: &mut TestAppContext) {
    let f = fixture();
    f.own("Older", ID_A);
    f.own("Newer", ID_B);
    let s = start(cx, &f, Some("u"), true);
    assert_eq!(s.subjects(cx), ["Newer", "Older"]);
    let scanner = s.scanner(cx).expect("a scanner");
    scanner.update(cx, |s, _| s.set_limit(1));
    let scans = s.full_scans(cx);
    s.rescan(cx);
    assert_eq!(s.full_scans(cx), scans + 1);
    // Only the newest commit was seen, but the older change is not taken
    // as gone, and the round settled.
    assert_eq!(s.subjects(cx), ["Newer", "Older"]);
    s.rescan(cx);
    assert_eq!(s.full_scans(cx), scans + 1);
    // A newer commit is still picked up.
    f.own("Newest", ID_C);
    s.rescan(cx);
    assert_eq!(s.subjects(cx), ["Newer", "Newest", "Older"]);
}

#[gpui_kit::test]
fn turning_the_option_off_removes_the_rows_without_unsent_data(cx: &mut TestAppContext) {
    let f = fixture();
    f.own("Plain", ID_A);
    f.own("With a draft", ID_B);
    let s = start(cx, &f, Some("u"), true);
    assert_eq!(s.subjects(cx), ["Plain", "With a draft"]);
    draft_on(&s.store, ID_B);
    let scans = s.full_scans(cx);

    // Saving the projects (the settings page does this) reaches the
    // scanner through the checkouts.
    s.shell.update(cx, |shell, cx| {
        shell.save_projects(vec![f.project(false)], cx)
    });
    cx.run_until_parked();
    assert_eq!(s.subjects(cx), ["With a draft"]);
    assert_eq!(
        s.full_scans(cx),
        scans,
        "nothing is scanned with the option off"
    );
    let rows = s
        .store
        .lock()
        .expect("store")
        .local_changes(SERVER)
        .expect("rows");
    assert_eq!(rows.len(), 1);

    // Back on: the checkout is scanned again.
    s.shell.update(cx, |shell, cx| {
        shell.save_projects(vec![f.project(true)], cx)
    });
    cx.run_until_parked();
    assert_eq!(s.subjects(cx), ["Plain", "With a draft"]);
    assert_eq!(s.full_scans(cx), scans + 1);
}

#[gpui_kit::test]
fn an_anonymous_server_is_not_scanned(cx: &mut TestAppContext) {
    let f = fixture();
    f.own("Own change", ID_A);
    let s = start(cx, &f, None, true);
    assert!(s.scanner(cx).is_none());
    assert!(s.subjects(cx).is_empty());
    let rows = s
        .store
        .lock()
        .expect("store")
        .local_changes(SERVER)
        .expect("rows");
    assert!(rows.is_empty());
}

#[gpui_kit::test]
fn opening_a_local_row_sends_nothing_and_notifies_nothing(cx: &mut TestAppContext) {
    let f = fixture();
    f.own("Own change", ID_A);
    let s = start(cx, &f, Some("u"), true);
    let page = s.page(cx);
    let number = cx.update(|cx| page.read(cx).local_rows()[0].number);
    // The local section is on top.
    for _ in 0..3 {
        s.press(cx, "k");
    }
    assert_eq!(
        cx.update(|cx| page.read(cx).selected_number()),
        Some(number)
    );
    // No web UI page for it.
    s.press(cx, "shift-o");
    assert_eq!(cx.opened_url(), None);

    s.press(cx, "enter");
    cx.run_until_parked();
    assert_eq!(
        cx.update(|cx| s.shell.read(cx).route().clone()),
        Route::Change(number)
    );
    s.assert_no_local_number_sent(&f);
    // The change page does not even try: no error on it.
    let change = cx
        .update(|cx| s.shell.read(cx).change_page_for_test())
        .expect("the change page");
    cx.update(|cx| {
        let page = change.read(cx);
        assert!(page.detail().is_none());
        assert_eq!(*page.status_for_test(), crate::pages::list::Status::Idle);
    });
    let store = s.store.lock().expect("store");
    assert!(store.pending_events(SERVER).expect("events").is_empty());
    assert!(store.inbox_items(None, 10).expect("inbox").is_empty());
}

/// A local change row in `state`, written as a reconcile would.
fn upsert(
    change_id: &str,
    subject: &str,
    branch: Option<&str>,
    state: LocalChangeState,
) -> LocalOp {
    LocalOp::Upsert(LocalChangeUpsert {
        seen: None,
        branch: branch.map(str::to_owned),
        change_id: change_id.into(),
        gerrit_number: None,
        subject: subject.into(),
        author_name: "Test Author".into(),
        author_email: TEST_EMAIL.into(),
        updated: ts(),
        state: Some(state),
        patch_sets: vec![NewLocalPatchSet {
            sha: RevisionId(format!("{:0>40}", change_id.len() + subject.len())),
            parent_sha: None,
            seen_at: ts(),
        }],
    })
}

#[gpui_kit::test]
fn every_state_renders_and_j_k_cross_into_the_queries(cx: &mut TestAppContext) {
    let mut store = Store::open_in_memory().expect("store");
    let server = store.server_id(URL).expect("server id");
    store
        .save_query_page(server, "status:open", &[summary(1)], false, ts())
        .expect("save query");
    let ops = [
        upsert("I1", "Checking", Some("main"), LocalChangeState::Checking),
        upsert("I2", "Local", Some("main"), LocalChangeState::Local),
        upsert("I3", "Ambiguous", None, LocalChangeState::Ambiguous),
        upsert("I4", "Vanished", Some("main"), LocalChangeState::Vanished),
    ];
    store
        .reconcile_local(server, "p", &ops, ts())
        .expect("reconcile");
    let env = env_with(store, &RouteHttpClient::new());
    let session = Session::new(&env, &setup(), None).expect("session");
    let specs = vec![SavedQuery {
        name: "Open".into(),
        query: "status:open".into(),
    }];
    let (window, page) = open(cx, |window, cx| {
        let checkouts = cx.new(|_| Checkouts::new(Vec::new()));
        // Without a session the scanner never runs a round.
        let scanner = cx.new(|cx| LocalScanner::new(checkouts, cx));
        let page = cx.new(|cx| {
            let mut page = ChangeListPage::new(session, ListKind::Dashboard, specs, None, true, cx);
            page.set_local_scanner(scanner, cx);
            page
        });
        let focus = page.read(cx).focus_handle_for_test();
        window.focus(&focus, cx);
        page
    });
    cx.run_until_parked();
    let (numbers, mut states) = cx.update(|cx| {
        let rows = page.read(cx).local_rows();
        let numbers: Vec<ChangeNumber> = rows.iter().map(|c| c.number).collect();
        let states: Vec<LocalChangeState> = rows.iter().map(|c| c.state).collect();
        (numbers, states)
    });
    states.sort_by_key(|s| s.as_str());
    assert_eq!(
        states,
        [
            LocalChangeState::Ambiguous,
            LocalChangeState::Checking,
            LocalChangeState::Local,
            LocalChangeState::Vanished,
        ]
    );
    // Drawn (the window renders on every update) without trouble.
    cx.update_window(window, |_, window, _| window.refresh())
        .expect("refresh");
    cx.run_until_parked();

    let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = events.clone();
    let _sub = cx.update(|cx| {
        cx.subscribe(&page, move |_, ev: &ListEvent, _| {
            if let ListEvent::Open(n) = ev {
                seen.borrow_mut().push(*n);
            }
        })
    });
    let press = |cx: &mut TestAppContext, key: &str| {
        cx.update_window(window, |_, window, cx| {
            use gpui_kit::test::TestWindowExt as _;
            window.press(key, cx)
        })
        .expect("press");
    };
    let selected = |cx: &mut TestAppContext| cx.update(|cx| page.read(cx).selected_number());
    for _ in 0..6 {
        press(cx, "k");
    }
    assert_eq!(selected(cx), Some(numbers[0]));
    for _ in 0..4 {
        press(cx, "j");
    }
    assert_eq!(selected(cx), Some(ChangeNumber(1)), "past the local rows");
    press(cx, "k");
    assert_eq!(selected(cx), Some(numbers[3]));
    press(cx, "enter");
    assert_eq!(*events.borrow(), [numbers[3]]);
}
