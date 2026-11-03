//! The Submit warning against a real temporary checkout (local-git's
//! `TestRepo`, with a bare "remote") and the scanner, on a scripted server.

use std::cell::Cell;
use std::rc::Rc;

use futures::future;
use gpui_kit::Task;
use local_git::testutil::TestRepo;
use model::{ChangeNumber, LocalChangeState};
use store::{LocalChangeUpsert, LocalOp, NewLocalPatchSet, ServerId};

use super::*;
use crate::{
    action_menu::submit_check::{WAIT, newer_local_version},
    checkouts::Checkouts,
    local_scan::{LocalScanner, ScanNowError},
    session::ProjectSettings,
};

const ID_7: &str = "I7777777777777777777777777777777777777777";

/// A checkout of `p` whose `main` tracks `origin/main`, with one pushed
/// commit, and `extra` local commits on top.
struct Checkout {
    _remote: TestRepo,
    repo: TestRepo,
}

fn checkout(extra: &[&str]) -> Checkout {
    let remote = TestRepo::bare();
    let repo = TestRepo::new();
    repo.commit("Base");
    let url = remote.path().to_str().expect("utf-8 temp path").to_owned();
    repo.git(&["remote", "add", "origin", &url]);
    repo.git(&["push", "-q", "-u", "origin", "main"]);
    for message in extra {
        repo.commit(message);
    }
    Checkout {
        _remote: remote,
        repo,
    }
}

impl Checkout {
    fn project(&self, local_changes: bool) -> ProjectSettings {
        ProjectSettings {
            local_changes,
            ..ProjectSettings::new("p", self.repo.path().to_string_lossy())
        }
    }
}

/// [`change`] in project `p` with Change-Id [`ID_7`], its patch sets
/// uploaded in 2024: what the store caches and the dialog shows.
fn change_7() -> ChangeDetail {
    let mut d = change();
    d.summary.project = "p".into();
    d.summary.change_id = Some(ID_7.into());
    for r in &mut d.revisions {
        r.created = Some("2024-06-15T10:00:00Z".parse().expect("timestamp"));
    }
    d
}

const SUBMIT_ROUTES: fn(&RouteHttpClient) = |http| {
    http.route("POST /a/changes/7/submit", 200, CHANGE_OK);
};

/// The dialog of change 7 signed in as bob, with change 7 cached and, when
/// `scanned`, the scanner of a checkout of `p` with the option on.
fn dialog_with(cx: &mut TestAppContext, ck: &Checkout, scanned: bool) -> Fx {
    let mut store = Store::open_in_memory().expect("store");
    let server = store.server_id(URL).expect("server");
    store
        .save_change_detail(server, &change_7(), model::Timestamp::now())
        .expect("detail");
    let http = RouteHttpClient::new();
    SUBMIT_ROUTES(&http);
    let env = env_with(store, &http);
    let mut server = setup();
    server.username = Some("bob".into());
    let auth = Auth::Basic {
        username: "bob".into(),
        password: "p".into(),
    };
    let session = Session::new(&env, &server, Some(auth)).expect("session");
    let events = Arc::new(Mutex::new(Vec::new()));
    let log = events.clone();
    let projects = vec![ck.project(true)];
    let (window, dialog) = open(cx, |window, cx| {
        let checkouts = cx.new(|_| Checkouts::new(projects));
        let scanner = cx.new(|cx| LocalScanner::new(checkouts, cx));
        scanner.update(cx, |s, _| s.set_session(Some(&session)));
        let dialog = cx.new(|cx| {
            let mut d = ActionDialog::new(session, change_7(), window, cx);
            if scanned {
                d.set_local_scanner(scanner);
            }
            d
        });
        cx.subscribe(&dialog, move |_, ev: &ActionEvent, _| {
            if let ActionEvent::Done(d) = ev {
                log.lock().expect("log").push(format!("done {:?}", d.kind));
            }
        })
        .detach();
        dialog
    });
    cx.run_until_parked();
    Fx {
        window,
        dialog,
        http,
        events,
    }
}

fn warning(cx: &mut TestAppContext, f: &Fx) -> Option<String> {
    cx.update(|cx| f.dialog.read(cx).warning_for_test())
}

fn amended() -> String {
    format!("The change, amended\n\nChange-Id: {ID_7}")
}

#[gpui_kit::test]
fn submit_warns_of_a_newer_local_version_and_cancel_sends_nothing(cx: &mut TestAppContext) {
    let ck = checkout(&[&amended()]);
    let f = dialog_with(cx, &ck, true);
    press(cx, &f, &["s"]);
    assert_eq!(step(cx, &f), "Confirm(Submit)");
    assert_eq!(
        warning(cx, &f).as_deref(),
        Some(
            "Your checkout has a newer version (local 1) that is not uploaded. \
             Submit PS 3 anyway?"
        )
    );
    let mut visual = gpui_kit::VisualTestContext::from_window(f.window, cx);
    visual.run_until_parked();
    assert!(visual.debug_bounds("submit-warning").is_some());
    // Enter is on Cancel: back to the menu, nothing sent.
    press(cx, &f, &["enter"]);
    assert_eq!(step(cx, &f), "Menu");
    assert!(writes(&f).is_empty());
    // Asked again when Submit is picked again; Esc cancels too.
    press(cx, &f, &["s"]);
    assert!(warning(cx, &f).is_some());
    press(cx, &f, &["escape"]);
    assert_eq!(step(cx, &f), "Menu");
    assert!(writes(&f).is_empty());
    assert!(events(&f).is_empty());
}

#[gpui_kit::test]
fn submit_anyway_submits_the_current_patch_set(cx: &mut TestAppContext) {
    let ck = checkout(&[&amended()]);
    let f = dialog_with(cx, &ck, true);
    press(cx, &f, &["s"]);
    assert!(warning(cx, &f).is_some());
    press(cx, &f, &[CONFIRM]);
    assert_eq!(writes(&f), ["POST /a/changes/7/submit"]);
    assert_eq!(events(&f), ["done Submit"]);
    // Nothing local went to the server.
    let base = ChangeNumber::LOCAL_BASE.to_string();
    assert!(f.http.requested().iter().all(|r| !r.contains(&base[..6])));
}

#[gpui_kit::test]
fn submit_asks_as_before_without_a_local_version(cx: &mut TestAppContext) {
    // Another change of the user's is no version of 7.
    let ck = checkout(&["Unrelated\n\nChange-Id: I0000000000000000000000000000000000000001"]);
    let f = dialog_with(cx, &ck, true);
    press(cx, &f, &["s"]);
    assert_eq!(step(cx, &f), "Confirm(Submit)");
    assert_eq!(warning(cx, &f), None);
    assert!(!cx.update(|cx| f.dialog.read(cx).checking_for_test()));
    press(cx, &f, &["enter"]);
    assert_eq!(writes(&f), ["POST /a/changes/7/submit"]);
}

#[gpui_kit::test]
fn no_warning_where_the_project_is_not_scanned(cx: &mut TestAppContext) {
    // The shell gives no scanner when the option is off.
    let ck = checkout(&[&amended()]);
    let f = dialog_with(cx, &ck, false);
    press(cx, &f, &["s"]);
    assert_eq!(warning(cx, &f), None);
    press(cx, &f, &["enter"]);
    assert_eq!(writes(&f), ["POST /a/changes/7/submit"]);
}

#[gpui_kit::test]
fn the_submit_chain_button_stops_at_the_warning(cx: &mut TestAppContext) {
    let ck = checkout(&[&amended()]);
    let f = dialog_with(cx, &ck, true);
    cx.update_window(f.window, |_, window, cx| {
        f.dialog.update(cx, |d, cx| d.submit_now(window, cx))
    })
    .expect("window");
    cx.run_until_parked();
    assert!(warning(cx, &f).is_some());
    assert!(writes(&f).is_empty());

    // Without a newer version it goes straight out once the check is done.
    let ck = checkout(&[]);
    let f = dialog_with(cx, &ck, true);
    cx.update_window(f.window, |_, window, cx| {
        f.dialog.update(cx, |d, cx| d.submit_now(window, cx))
    })
    .expect("window");
    cx.run_until_parked();
    assert_eq!(writes(&f), ["POST /a/changes/7/submit"]);
}

/// A store with change 7 cached and a local version of it seen now.
fn store_with_version() -> Store {
    let mut store = Store::open_in_memory().expect("store");
    let server = store.server_id(URL).expect("server");
    assert_eq!(server, ServerId(1));
    store
        .save_change_detail(server, &change_7(), model::Timestamp::now())
        .expect("detail");
    let now = model::Timestamp::now();
    let op = LocalOp::Upsert(LocalChangeUpsert {
        seen: None,
        branch: Some("main".into()),
        change_id: ID_7.into(),
        gerrit_number: Some(ChangeNumber(7)),
        subject: "The change, amended".into(),
        author_name: "Me".into(),
        author_email: "me@example.com".into(),
        updated: now,
        state: Some(LocalChangeState::Local),
        patch_sets: vec![NewLocalPatchSet {
            sha: "abc".into(),
            parent_sha: None,
            seen_at: now,
        }],
    });
    store
        .reconcile_local(server, "p", &[op], now)
        .expect("reconcile");
    store
}

/// What [`newer_local_version`] answers for change 7 given `scan`, checked
/// before and after [`WAIT`] passes.
fn answer_of(
    cx: &mut TestAppContext,
    scan: impl FnOnce(&mut TestAppContext) -> Task<Result<store::ReconcileReport, ScanNowError>>,
) -> (Option<Option<u32>>, Option<Option<u32>>) {
    let env = env_with(store_with_version(), &RouteHttpClient::new());
    let session = Session::new(&env, &setup(), None).expect("session");
    let syncer = session.syncer().clone();
    let scan = scan(cx);
    let since = change_7().current().and_then(|r| r.created);
    let answer = Rc::new(Cell::new(None));
    let put = answer.clone();
    cx.update(|cx| {
        cx.spawn(async move |cx| {
            let found = newer_local_version(scan, WAIT, syncer, ChangeNumber(7), since, cx).await;
            put.set(Some(found));
        })
        .detach()
    });
    cx.run_until_parked();
    let before = answer.get();
    cx.executor().advance_clock(WAIT);
    cx.run_until_parked();
    (before, answer.get())
}

#[gpui_kit::test]
fn a_scan_that_does_not_answer_in_time_leaves_it_to_the_store(cx: &mut TestAppContext) {
    let (before, after) = answer_of(cx, |cx| cx.background_executor.spawn(future::pending()));
    assert_eq!(before, None, "it waits for the scan");
    assert_eq!(after, Some(Some(1)));
}

#[gpui_kit::test]
fn a_failed_scan_leaves_it_to_the_store_and_an_unscanned_project_says_nothing(
    cx: &mut TestAppContext,
) {
    let (before, _) = answer_of(cx, |_| Task::ready(Err(ScanNowError::Failed("git".into()))));
    assert_eq!(before, Some(Some(1)));
    let (before, _) = answer_of(cx, |_| {
        Task::ready(Err(ScanNowError::NotScanned("p".into())))
    });
    assert_eq!(before, Some(None));
}
