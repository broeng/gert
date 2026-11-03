//! The change page with local changes and local patch sets: a temporary
//! repository as the configured checkout of project `p`, local change rows
//! written as a reconcile would, and a scripted server. The fake HTTP
//! client panics on any synthetic number, and the tests check the request
//! log too.

use std::sync::{Arc, Mutex};

use gerrit_api::{Auth, fake::RouteHttpClient};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
use local_git::testutil::{TEST_EMAIL, TestRepo};
use model::{
    ChangeNumber, FileStatus, FilesKey, LocalChangeState, PatchSetNumber, PatchSetRef, RevisionId,
    Timestamp,
};
use store::{LocalChangeUpsert, LocalOp, NewLocalPatchSet, ServerId, Store};

use super::{ChangeEvent, ChangePage, state::Block, view::patch_set_menu};
use crate::{
    LocalDiff,
    diff_load::DiffSource,
    nav::ChangeView,
    outbox::DraftOutbox,
    session::{ProjectSettings, Session},
    settings::{DiffSettings, FileFilters, PageLayout},
    syntax_cache::SyntaxCache,
    testing::{detail, file},
    tests::{URL, env_with, open, setup},
};

const SERVER: ServerId = ServerId(1);

fn at(minute: u32) -> Timestamp {
    format!("2024-06-15T10:{minute:02}:00Z")
        .parse()
        .expect("timestamp")
}

/// A checkout with a base commit and one local commit on top that adds
/// `src/new.rs` and changes `README`.
struct Checkout {
    repo: TestRepo,
    base: String,
    local: String,
}

fn checkout() -> Checkout {
    let repo = TestRepo::new();
    repo.write("README", b"one\n");
    let base = repo.commit("Base");
    repo.write("README", b"two\n");
    repo.write("src/new.rs", b"fn main() {}\n");
    let local = repo.commit("Add the new thing\n\nIt is new.\n\nChange-Id: I1");
    Checkout { repo, base, local }
}

impl Checkout {
    fn project(&self) -> ProjectSettings {
        ProjectSettings {
            local_changes: true,
            ..ProjectSettings::new("p", self.repo.path().to_string_lossy())
        }
    }
}

/// A local change row (`gerrit` `None`) or local versions of Gerrit change
/// `gerrit`, with the versions `shas` seen at minutes 40, 41, ...
fn row(
    change_id: &str,
    gerrit: Option<ChangeNumber>,
    state: LocalChangeState,
    shas: &[&str],
    parent: &str,
) -> LocalOp {
    LocalOp::Upsert(LocalChangeUpsert {
        seen: None,
        branch: Some("main".into()),
        change_id: change_id.into(),
        gerrit_number: gerrit,
        subject: "Add the new thing".into(),
        author_name: "Test Author".into(),
        author_email: TEST_EMAIL.into(),
        updated: at(45),
        state: Some(state),
        patch_sets: shas
            .iter()
            .zip(40..)
            .map(|(sha, m)| NewLocalPatchSet {
                sha: RevisionId((*sha).to_owned()),
                parent_sha: Some(RevisionId(parent.to_owned())),
                seen_at: at(m),
            })
            .collect(),
    })
}

fn store_with(ops: &[LocalOp], seed: impl FnOnce(&mut Store)) -> Store {
    let mut store = Store::open_in_memory().expect("store");
    assert_eq!(store.server_id(URL).expect("server"), SERVER);
    seed(&mut store);
    store
        .reconcile_local(SERVER, "p", ops, at(45))
        .expect("reconcile");
    store
}

/// Change 7 of project `p` with patch sets 1..=3 uploaded at minutes 10,
/// 20 and 30, and its "Base vs PS 3" file list.
fn seed_gerrit_change(store: &mut Store) {
    let mut d = detail(7, 3, 3);
    d.summary.project = "p".into();
    d.summary.change_id = Some("I7".into());
    // The user may vote: the header has a quick vote.
    d.permitted_labels = [("Code-Review".to_owned(), vec![-1, 0, 1])].into();
    for (r, m) in d.revisions.iter_mut().zip([10, 20, 30]) {
        r.created = Some(at(m));
    }
    store
        .save_change_detail(SERVER, &d, Timestamp::now())
        .expect("detail");
    let key = FilesKey {
        change: ChangeNumber(7),
        base: None,
        target: PatchSetRef {
            number: PatchSetNumber(3),
            sha: "sha3".into(),
        },
    };
    let files = vec![file("/COMMIT_MSG", FileStatus::Added)];
    store
        .save_file_list(SERVER, &key, &files, None, Timestamp::now())
        .expect("files");
}

struct Fixture {
    window: AnyWindowHandle,
    page: Entity<ChangePage>,
    http: RouteHttpClient,
    store: Arc<Mutex<Store>>,
    events: Arc<Mutex<Vec<String>>>,
}

/// The change page of `number` on `store`, signed in (the session can
/// sync, so nothing but the guards keeps it off the network) with the
/// checkout configured.
fn page(
    cx: &mut TestAppContext,
    store: Store,
    number: ChangeNumber,
    ck: &Checkout,
    routes: impl FnOnce(&RouteHttpClient),
) -> Fixture {
    page_in(cx, store, number, ck.project(), routes)
}

/// Like [`page`], with the checkout of `project`.
fn page_in(
    cx: &mut TestAppContext,
    store: Store,
    number: ChangeNumber,
    project: ProjectSettings,
    routes: impl FnOnce(&RouteHttpClient),
) -> Fixture {
    let http = RouteHttpClient::new();
    routes(&http);
    let env = env_with(store, &http);
    let store = env.store.clone();
    let mut server = setup();
    server.username = Some("u".into());
    let auth = Auth::Basic {
        username: "u".into(),
        password: "p".into(),
    };
    let session = Session::new(&env, &server, Some(auth)).expect("session");
    let projects = vec![project];
    let events = Arc::new(Mutex::new(Vec::new()));
    let log = events.clone();
    let (window, page) = open(cx, |window, cx| {
        let settings = cx.new(|_| DiffSettings::new(LocalDiff::default()));
        let outbox = cx.new(|_| DraftOutbox::new(session.clone()));
        let layout = cx.new(|_| PageLayout::default());
        let filters = cx.new(|_| FileFilters::new(Vec::new()));
        let syntax = cx.new(|_| SyntaxCache::new());
        let checkouts = cx.new(|_| crate::checkouts::Checkouts::new(projects));
        let page = cx.new(|cx| {
            let mut page = ChangePage::new(
                session,
                settings,
                layout,
                filters,
                syntax,
                number,
                ChangeView::default(),
                true,
                outbox,
                cx,
            );
            page.set_checkouts(Some(checkouts));
            page
        });
        cx.subscribe(&page, move |_, ev: &ChangeEvent, _| {
            let text = match ev {
                ChangeEvent::Refreshed => "refreshed".to_owned(),
                ChangeEvent::Reply(r) => match r.edit {
                    Some(id) => format!("reply edit {}", id.0),
                    None => "reply".to_owned(),
                },
                ChangeEvent::Actions(_) => "actions".to_owned(),
                ChangeEvent::Unauthorized => "unauthorized".to_owned(),
                ChangeEvent::OpenChange(n) => format!("change {n}"),
                ChangeEvent::ReplaceChange(n) => format!("replace {n}"),
                ChangeEvent::OpenChangeAt(n, v) => format!("change {n} at {:?}", v.target),
                ChangeEvent::ReplaceChangeAt(n, v) => format!("replace {n} at {:?}", v.target),
                ChangeEvent::Advance(n, at, note) => format!("advance {n} at {at:?}: {note}"),
                ChangeEvent::LocalReviewSaved => "local review saved".to_owned(),
                _ => "other".to_owned(),
            };
            log.lock().expect("log").push(text);
        })
        .detach();
        let focus = page.read(cx).focus_handle_for_test();
        window.focus(&focus, cx);
        page
    });
    cx.run_until_parked();
    Fixture {
        window,
        page,
        http,
        store,
        events,
    }
}

impl Fixture {
    fn press(&self, cx: &mut TestAppContext, key: &str) {
        cx.update_window(self.window, |_, window, cx| window.press(key, cx))
            .expect("press");
        cx.run_until_parked();
    }

    fn hint(&self, cx: &mut TestAppContext) -> Option<String> {
        cx.update(|cx| self.page.read(cx).hint_for_test().map(|(t, _)| t))
    }

    /// Everything the patch set dropdowns say, and the notices.
    fn texts(&self, cx: &mut TestAppContext) -> Vec<String> {
        cx.update(|cx| {
            let p = self.page.read(cx);
            let mut out = vec![p.files_notice_text().0];
            if let Some(d) = p.detail() {
                let menu = patch_set_menu(d, &p.sel);
                out.push(menu.base_text);
                out.push(menu.target_text);
                for c in menu.base_choices.iter().chain(&menu.target_choices) {
                    out.push(c.label.to_string());
                }
            }
            out.extend(p.hint_for_test().map(|(t, _)| t));
            out
        })
    }

    /// The target dropdown's entries, newest first.
    fn target_choices(&self, cx: &mut TestAppContext) -> Vec<String> {
        cx.update(|cx| {
            let p = self.page.read(cx);
            let d = p.detail().expect("detail");
            patch_set_menu(d, &p.sel)
                .target_choices
                .iter()
                .map(|c| c.label.to_string())
                .collect()
        })
    }

    fn local_patch_set(&self, cx: &mut TestAppContext, ordinal: usize) -> PatchSetNumber {
        cx.update(|cx| {
            self.page
                .read(cx)
                .detail()
                .expect("detail")
                .revisions
                .iter()
                .filter(|r| r.number.is_local())
                .nth(ordinal - 1)
                .expect("a local patch set")
                .number
        })
    }

    fn pick(
        &self,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut super::PatchSel, &model::ChangeDetail),
    ) {
        self.page.update(cx, |p, cx| p.change_patch_sets(f, cx));
        cx.run_until_parked();
    }

    /// No request named a synthetic number or a local commit.
    fn assert_nothing_local_sent(&self, shas: &[&str]) {
        let change = ChangeNumber::LOCAL_BASE.to_string();
        let patch_set = PatchSetNumber::LOCAL_BASE.to_string();
        for r in self.http.requested() {
            assert!(!r.contains(&change[..6]), "{r}");
            assert!(!r.contains(&patch_set[..5]), "{r}");
            for sha in shas {
                assert!(!r.contains(sha), "{r}");
            }
        }
    }
}

/// No text names a synthetic number.
fn assert_no_synthetic(texts: &[String], numbers: &[u64]) {
    for t in texts {
        for n in numbers {
            assert!(!t.contains(&n.to_string()), "{t:?} shows {n}");
        }
    }
}

fn local_number(store: &Mutex<Store>, change_id: &str) -> ChangeNumber {
    store
        .lock()
        .expect("store")
        .local_changes_by_change_id(SERVER, change_id)
        .expect("rows")
        .remove(0)
        .number
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_local_change_shows_its_header_files_and_message_without_the_network(cx: &mut TestAppContext) {
    let ck = checkout();
    let store = store_with(
        &[row(
            "I1",
            None,
            LocalChangeState::Checking,
            &[&ck.local],
            &ck.base,
        )],
        |_| {},
    );
    let number = store
        .local_changes_by_change_id(SERVER, "I1")
        .expect("rows")[0]
        .number;
    let f = page(cx, store, number, &ck, |_| {});
    cx.update(|cx| {
        let p = f.page.read(cx);
        let d = p.detail().expect("the synthesised detail");
        assert_eq!(d.summary.subject, "Add the new thing");
        assert_eq!(d.summary.project, "p");
        assert_eq!(d.summary.branch, "main");
        // From git: the message, and the author on the version.
        assert_eq!(
            d.commit_message.as_deref().map(str::trim_end),
            Some("Add the new thing\n\nIt is new.\n\nChange-Id: I1")
        );
        let author = d.revisions[0]
            .commit
            .as_ref()
            .and_then(|c| c.author.clone());
        assert_eq!(author.and_then(|a| a.email).as_deref(), Some(TEST_EMAIL));
        // The labels a reply will use: nothing cached, so Code-Review.
        assert_eq!(d.permitted_labels["Code-Review"], [-2, -1, 0, 1, 2]);
        assert_eq!(
            p.local_row_for_test().map(|r| r.state),
            Some(LocalChangeState::Checking)
        );
        // The files come from the checkout.
        assert_eq!(p.file_paths(), ["/COMMIT_MSG", "README", "src/new.rs"]);
        assert_eq!(p.files_local_for_test(), Some(ck.repo.path()));
        assert_eq!(p.files_error_for_test(), None);
        // No log; a quick vote on the borrowed Code-Review.
        assert!(!p.blocks_for_test().contains(&Block::LogHeader));
        assert_eq!(p.quick_buttons_for_test(cx).len(), 1);
        assert_eq!(*p.status_for_test(), crate::pages::list::Status::Idle);
    });
    let target = f.target_choices(cx);
    assert_eq!(target, ["local 1 (current)"]);

    // Drawn: the state note.
    let mut visual = gpui_kit::VisualTestContext::from_window(f.window, cx);
    visual.run_until_parked();
    assert!(visual.debug_bounds("local-note-checking").is_some());

    // A reply is a local review (the shell opens it); neither the action
    // menu (it would go to Gerrit) nor reviewed marks.
    f.press(cx, "a");
    assert_eq!(f.events.lock().expect("log").clone(), ["reply"]);
    f.events.lock().expect("log").clear();
    f.press(cx, "m");
    assert_eq!(f.hint(cx).as_deref(), Some(crate::action_menu::LOCAL));
    f.press(cx, "r");
    assert_eq!(f.hint(cx).as_deref(), Some(crate::reviewed::LOCAL));
    // Expanding a file diffs it in the checkout, the commit message too.
    f.press(cx, "shift-i");
    cx.update(|cx| {
        let p = f.page.read(cx);
        for path in ["/COMMIT_MSG", "README", "src/new.rs"] {
            assert!(p.inline_ready(path), "{path}");
            assert_eq!(p.inline_source(path), Some(DiffSource::Local), "{path}");
        }
        let view = p.inline_view("/COMMIT_MSG").expect("a view");
        let text = format!("{:?}", view.read(cx).diff_for_test().chunks);
        assert!(text.contains("It is new."), "{text}");
    });
    // A refresh reads the store again, still without the network.
    f.press(cx, "shift-r");
    assert!(f.events.lock().expect("log").is_empty(), "nothing emitted");
    assert!(f.http.requested().is_empty(), "{:?}", f.http.requested());

    let mut texts = f.texts(cx);
    texts.push(f.hint(cx).unwrap_or_default());
    let ps = f.local_patch_set(cx, 1);
    assert_no_synthetic(&texts, &[number.0, u64::from(ps.0)]);
}

#[gpui_kit::test]
fn a_local_change_borrows_the_labels_of_the_newest_cached_change(cx: &mut TestAppContext) {
    let ck = checkout();
    let store = store_with(
        &[row(
            "I1",
            None,
            LocalChangeState::Local,
            &[&ck.local],
            &ck.base,
        )],
        |store| {
            let mut lender = detail(9, 1, 1);
            lender.summary.project = "p".into();
            lender.permitted_labels = [("Verified".to_owned(), vec![-1, 0, 1])].into();
            lender.label_values = [(
                "Verified".to_owned(),
                vec![model::LabelValue {
                    value: 1,
                    description: "Works".into(),
                }],
            )]
            .into();
            store
                .save_change_detail(SERVER, &lender, at(1))
                .expect("detail");
        },
    );
    let number = store
        .local_changes_by_change_id(SERVER, "I1")
        .expect("rows")[0]
        .number;
    let f = page(cx, store, number, &ck, |_| {});
    cx.update(|cx| {
        let d = f.page.read(cx).detail().cloned().expect("detail");
        assert_eq!(d.permitted_labels["Verified"], [-1, 0, 1]);
        assert!(!d.permitted_labels.contains_key("Code-Review"));
    });
    // A plain local change has no state note.
    let mut visual = gpui_kit::VisualTestContext::from_window(f.window, cx);
    visual.run_until_parked();
    assert!(visual.debug_bounds("local-note-local").is_none());
    assert!(f.http.requested().is_empty());
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_gerrit_change_lists_its_local_version_and_shows_its_files_from_the_checkout(
    cx: &mut TestAppContext,
) {
    let ck = checkout();
    let store = store_with(
        &[row(
            "I7",
            Some(ChangeNumber(7)),
            LocalChangeState::Local,
            &[&ck.local],
            &ck.base,
        )],
        seed_gerrit_change,
    );
    // Gerrit answers nothing: every request fails.
    let f = page(cx, store, ChangeNumber(7), &ck, |_| {});
    assert_eq!(
        f.target_choices(cx),
        ["local 1", "PS 3 (current)", "PS 2", "PS 1"]
    );
    cx.update(|cx| assert_eq!(f.page.read(cx).file_paths(), ["/COMMIT_MSG"]));

    let quick =
        |cx: &mut TestAppContext| cx.update(|cx| f.page.read(cx).quick_buttons_for_test(cx).len());
    assert_eq!(quick(cx), 1, "a quick vote on PS 3");

    // The local version as the target: its files come from the checkout.
    let local = f.local_patch_set(cx, 1);
    f.pick(cx, |s, d| s.set_target(d, local));
    // A vote or a reply now is a local review of the version shown, never
    // a request for PS 3 (the tests in `review` say more).
    assert_eq!(quick(cx), 1);
    f.press(cx, "a");
    assert!(f.events.lock().expect("log").contains(&"reply".to_owned()));
    cx.update(|cx| {
        let p = f.page.read(cx);
        assert_eq!(p.selection_for_test(), (None, Some(local)));
        assert_eq!(p.file_paths(), ["/COMMIT_MSG", "README", "src/new.rs"]);
        assert_eq!(p.files_local_for_test(), Some(ck.repo.path()));
        assert_eq!(p.files_error_for_test(), None);
    });

    // Against PS 3: a Gerrit base with a local target. The checkout does
    // not have PS 3, so there is no list, and the notice says why in its
    // own words (not Gerrit's).
    f.pick(cx, |s, d| s.set_base(d, Some(PatchSetNumber(3))));
    cx.update(|cx| {
        let p = f.page.read(cx);
        assert_eq!(
            p.selection_for_test(),
            (Some(PatchSetNumber(3)), Some(local))
        );
        let error = p.files_error_for_test().expect("a notice");
        assert!(error.contains("checkout"), "{error}");
    });
    // PS 3 back as the target: the base goes back to Base (PS 3 itself).
    f.pick(cx, |s, d| s.set_target(d, PatchSetNumber(3)));
    cx.update(|cx| {
        assert_eq!(
            f.page.read(cx).selection_for_test(),
            (None, Some(PatchSetNumber(3)))
        )
    });
    f.press(cx, "shift-r");
    f.assert_nothing_local_sent(&[&ck.local]);
    let texts = f.texts(cx);
    assert_no_synthetic(&texts, &[u64::from(local.0)]);
}

/// Change 7 as the server sends it, with patch sets 1..=`n`.
fn change_7(n: u32) -> String {
    let revisions: Vec<String> = (1..=n)
        .map(|i| {
            format!(
                r#""sha{i}": {{"_number": {i}, "created": "2024-06-15 10:{:02}:00.000000000"}}"#,
                i * 10
            )
        })
        .collect();
    format!(
        r#"{{"_number": 7, "branch": "main", "change_id": "I7", "project": "p",
        "created": "2024-06-15 10:00:00.000000000", "owner": {{"_account_id": 1}},
        "status": "NEW", "subject": "Fix it", "updated": "2024-06-15 11:00:00.000000000",
        "mergeable": true, "current_revision": "sha{n}", "revisions": {{{}}}}}"#,
        revisions.join(", ")
    )
}

#[gpui_kit::test]
fn refreshing_keeps_the_local_versions_and_the_store_never_gets_them(cx: &mut TestAppContext) {
    let ck = checkout();
    let store = store_with(
        &[row(
            "I7",
            Some(ChangeNumber(7)),
            LocalChangeState::Local,
            &[&ck.local],
            &ck.base,
        )],
        seed_gerrit_change,
    );
    let f = page(cx, store, ChangeNumber(7), &ck, |http| {
        http.route_json("/changes/7?", &change_7(4));
    });
    // The refresh brought PS 4 (uploaded at minute 40, the minute the
    // local version was seen: it goes before it).
    assert!(
        f.events
            .lock()
            .expect("log")
            .contains(&"refreshed".to_owned())
    );
    assert_eq!(
        f.target_choices(cx),
        ["local 1", "PS 4 (current)", "PS 3", "PS 2", "PS 1"]
    );
    // Picked, refreshed again: still there, still picked, and its commit
    // (read from the checkout) kept.
    let local = f.local_patch_set(cx, 1);
    f.pick(cx, |s, d| s.set_target(d, local));
    f.press(cx, "shift-r");
    cx.update(|cx| {
        let p = f.page.read(cx);
        assert_eq!(p.selection_for_test(), (None, Some(local)));
        let d = p.detail().expect("detail");
        let commit = d.revision(local).and_then(|r| r.commit.clone());
        assert_eq!(
            commit.map(|c| c.subject).as_deref(),
            Some("Add the new thing")
        );
    });
    // A write's answer (no local versions in it) keeps them too.
    let fresh = f
        .store
        .lock()
        .expect("store")
        .load_change_detail(SERVER, ChangeNumber(7))
        .expect("read")
        .expect("cached")
        .detail;
    f.page.update(cx, |p, cx| p.detail_refreshed(&fresh, cx));
    assert_eq!(f.target_choices(cx)[0], "local 1");
    // The store has only what Gerrit sent.
    let stored = f
        .store
        .lock()
        .expect("store")
        .load_change_detail(SERVER, ChangeNumber(7))
        .expect("read")
        .expect("cached")
        .detail;
    assert_eq!(stored.revisions.len(), 4);
    assert!(stored.revisions.iter().all(|r| !r.number.is_local()));
    f.assert_nothing_local_sent(&[&ck.local]);
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_local_change_picks_up_a_new_version_on_refresh(cx: &mut TestAppContext) {
    let ck = checkout();
    let store = store_with(
        &[row(
            "I1",
            None,
            LocalChangeState::Local,
            &[&ck.local],
            &ck.base,
        )],
        |_| {},
    );
    let number = store
        .local_changes_by_change_id(SERVER, "I1")
        .expect("rows")[0]
        .number;
    let f = page(cx, store, number, &ck, |_| {});
    // Amended: the scanner adds the new version.
    ck.repo.write("README", b"three\n");
    ck.repo.git(&["commit", "-q", "-a", "--amend", "--no-edit"]);
    let amended = ck.repo.git(&["rev-parse", "HEAD"]);
    f.store
        .lock()
        .expect("store")
        .reconcile_local(
            SERVER,
            "p",
            &[row(
                "I1",
                None,
                LocalChangeState::Local,
                &[&amended],
                &ck.base,
            )],
            at(46),
        )
        .expect("reconcile");
    assert_eq!(local_number(&f.store, "I1"), number);
    f.press(cx, "shift-r");
    assert_eq!(f.target_choices(cx), ["local 2 (current)", "local 1"]);
    cx.update(|cx| {
        let p = f.page.read(cx);
        assert_eq!(
            p.detail().and_then(|d| d.current_revision.clone()),
            Some(RevisionId(amended.clone()))
        );
        assert_eq!(p.file_paths(), ["/COMMIT_MSG", "README", "src/new.rs"]);
    });
    // Local 2 against local 1: a local base.
    let first = f.local_patch_set(cx, 1);
    f.pick(cx, |s, d| s.set_base(d, Some(first)));
    cx.update(|cx| {
        let p = f.page.read(cx);
        assert_eq!(p.selection_for_test().0, Some(first));
        assert_eq!(p.file_paths(), ["/COMMIT_MSG", "README"]);
    });
    assert!(f.http.requested().is_empty());
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_checkout_that_breaks_costs_one_git_read_per_refresh(cx: &mut TestAppContext) {
    let ck = checkout();
    let store = store_with(
        &[row(
            "I1",
            None,
            LocalChangeState::Local,
            &[&ck.local],
            &ck.base,
        )],
        |_| {},
    );
    let number = store
        .local_changes_by_change_id(SERVER, "I1")
        .expect("rows")[0]
        .number;
    let f = page(cx, store, number, &ck, |_| {});
    // The checkout goes away while it counts as open, and a new version
    // shows up in the store: its commit cannot be read.
    std::fs::remove_dir_all(ck.repo.path()).expect("remove");
    let new = "1".repeat(40);
    f.store
        .lock()
        .expect("store")
        .reconcile_local(
            SERVER,
            "p",
            &[row("I1", None, LocalChangeState::Local, &[&new], &ck.base)],
            at(46),
        )
        .expect("reconcile");
    // Each refresh tries once and settles (a loop would never park).
    for _ in 0..2 {
        f.press(cx, "shift-r");
        cx.update(|cx| {
            let p = f.page.read(cx);
            let d = p.detail().expect("detail");
            assert_eq!(d.revisions.len(), 2);
            assert!(d.revisions[1].commit.is_none());
            assert!(d.revisions[0].commit.is_some(), "read before it broke");
            assert_eq!(*p.status_for_test(), crate::pages::list::Status::Idle);
        });
    }
    assert!(f.http.requested().is_empty());
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_patch_set_and_a_rebased_local_version_say_why_they_cannot_be_compared(
    cx: &mut TestAppContext,
) {
    let ck = checkout();
    // PS 1 of change 7 is `local`; the local version is the same change
    // rebased onto a newer upstream commit.
    ck.repo.git(&["checkout", "-q", "-b", "other", &ck.base]);
    ck.repo.write("OTHER", b"upstream\n");
    let upstream = ck.repo.commit("Upstream moved");
    ck.repo.write("README", b"two\n");
    let rebased = ck.repo.commit("Add the new thing\n\nChange-Id: I7");
    let store = store_with(
        &[row(
            "I7",
            Some(ChangeNumber(7)),
            LocalChangeState::Local,
            &[&rebased],
            &upstream,
        )],
        |store| {
            let mut d = detail(7, 1, 1);
            d.summary.project = "p".into();
            d.summary.change_id = Some("I7".into());
            d.revisions[0].sha = ck.local.clone().into();
            d.revisions[0].created = Some(at(10));
            d.current_revision = Some(ck.local.clone().into());
            store
                .save_change_detail(SERVER, &d, Timestamp::now())
                .expect("detail");
        },
    );
    let f = page(cx, store, ChangeNumber(7), &ck, |_| {});
    let local = f.local_patch_set(cx, 1);
    f.pick(cx, |s, d| s.set_target(d, local));
    f.pick(cx, |s, d| s.set_base(d, Some(PatchSetNumber(1))));
    // The resolve runs on real threads.
    for _ in 0..100 {
        if cx.update(|cx| f.page.read(cx).parents_differ()) {
            break;
        }
        cx.executor().allow_parking();
        std::thread::sleep(std::time::Duration::from_millis(20));
        cx.run_until_parked();
    }
    cx.update(|cx| {
        let p = f.page.read(cx);
        assert!(p.parents_differ());
        assert_eq!(
            p.files_notice_text(),
            (crate::pages::diff::PARENTS_DIFFER.to_owned(), false)
        );
        assert_eq!(p.files_error_for_test(), None);
    });
    // "Compare with Base": the checkout lists that.
    f.pick(cx, |s, d| s.set_base(d, None));
    cx.update(|cx| {
        let p = f.page.read(cx);
        assert!(!p.parents_differ());
        assert_eq!(p.file_paths(), ["/COMMIT_MSG", "README"]);
    });
    f.assert_nothing_local_sent(&[&rebased]);
}

/// The Change-Id of change 7 in [`series`].
fn id_of_7() -> String {
    format!("I{}", "7".repeat(40))
}

/// A series of local commits on `main` on top of patch set 1 of Gerrit
/// change 7: a local change (I1), a local version of Gerrit change 9 (I9)
/// and another local change (I3), newest last.
struct Series {
    repo: TestRepo,
    gerrit: String,
    first: String,
    second: String,
    third: String,
}

fn series() -> Series {
    let repo = TestRepo::new();
    repo.write("README", b"one\n");
    repo.commit("Base");
    repo.write("README", b"two\n");
    // A real Change-Id: the Gerrit patch set is found through it.
    let gerrit = repo.commit(&format!("On Gerrit\n\nChange-Id: {}", id_of_7()));
    repo.write("a", b"a\n");
    let first = repo.commit("First local\n\nChange-Id: I1");
    repo.write("b", b"b\n");
    let second = repo.commit("Second, amended\n\nChange-Id: I9");
    repo.write("c", b"c\n");
    let third = repo.commit("Third local\n\nChange-Id: I3");
    Series {
        repo,
        gerrit,
        first,
        second,
        third,
    }
}

impl Series {
    fn store(&self) -> Store {
        let rows = [
            row(
                "I1",
                None,
                LocalChangeState::Local,
                &[&self.first],
                &self.gerrit,
            ),
            row(
                "I9",
                Some(ChangeNumber(9)),
                LocalChangeState::Local,
                &[&self.second],
                &self.first,
            ),
            row(
                "I3",
                None,
                LocalChangeState::Local,
                &[&self.third],
                &self.second,
            ),
        ];
        store_with(&rows, |store| {
            // 7's only patch set is the bottom of the series; 9 has one
            // patch set the checkout does not have.
            for (n, sha) in [(7, self.gerrit.clone()), (9, "sha9".to_owned())] {
                let mut d = detail(n, 1, 1);
                d.summary.project = "p".into();
                d.summary.change_id = Some(if n == 7 { id_of_7() } else { "I9".into() });
                d.revisions[0].sha = sha.clone().into();
                d.revisions[0].created = Some(at(1));
                d.current_revision = Some(sha.into());
                store
                    .save_change_detail(SERVER, &d, Timestamp::now())
                    .expect("detail");
            }
        })
    }

    fn project(&self) -> ProjectSettings {
        ProjectSettings {
            local_changes: true,
            ..ProjectSettings::new("p", self.repo.path().to_string_lossy())
        }
    }
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_local_series_has_its_relation_chain_from_the_checkout(cx: &mut TestAppContext) {
    let s = series();
    let store = s.store();
    let number = |id: &str| {
        store
            .local_changes_by_change_id(SERVER, id)
            .expect("rows")
            .remove(0)
            .number
    };
    let (first, third) = (number("I1"), number("I3"));
    let version_of_9 = store
        .local_patch_sets_by_sha(SERVER, &s.second.clone().into())
        .expect("versions")[0]
        .number;
    let f = page_in(cx, store, first, s.project(), |_| {});
    let chain = |cx: &mut TestAppContext, f: &Fixture| {
        for _ in 0..100 {
            if !cx.update(|cx| f.page.read(cx).chain_for_test()).is_empty() {
                break;
            }
            cx.executor().allow_parking();
            std::thread::sleep(std::time::Duration::from_millis(20));
            cx.run_until_parked();
        }
        cx.update(|cx| f.page.read(cx).chain_for_test())
    };
    let row = |label: &str, subject: &str, this: bool| (label.to_owned(), subject.to_owned(), this);
    // Newest first, down to (and with) the Gerrit patch set it is built
    // on; the base commit below is 7's own business.
    assert_eq!(
        chain(cx, &f),
        [
            row("local", "Third local", false),
            row("#9 local 1", "Second, amended", false),
            row("local", "First local", true),
            row("#7 PS 1", "On Gerrit", false),
        ]
    );
    let mut visual = gpui_kit::VisualTestContext::from_window(f.window, cx);
    visual.run_until_parked();
    assert!(visual.debug_bounds("chain-row: #9 local 1").is_some());

    // A click opens the row: a local change by its route, a local version
    // at its patch set, a current Gerrit patch set as its change. This
    // change's own row does nothing.
    for ix in [0, 1, 3, 2] {
        f.page.update(cx, |p, cx| p.open_chain_row(ix, false, cx));
    }
    // Shift-K and Shift-J step to the neighbours.
    f.press(cx, "shift-k");
    f.press(cx, "shift-j");
    let v9 = format!("{:?}", Some(version_of_9));
    assert_eq!(
        *f.events.lock().expect("log"),
        [
            format!("change {third}"),
            format!("change 9 at {v9}"),
            "change 7".to_owned(),
            format!("replace 9 at {v9}"),
            "replace 7".to_owned(),
        ]
    );
    let labels: Vec<String> = chain(cx, &f).into_iter().map(|r| r.0).collect();
    assert_no_synthetic(&labels, &[first.0, third.0, u64::from(version_of_9.0)]);
    assert!(f.http.requested().is_empty(), "{:?}", f.http.requested());

    // Gerrit change 9 with its local version as the target: the same
    // chain from the checkout, with that version on show.
    let f = page_in(cx, s.store(), ChangeNumber(9), s.project(), |_| {});
    let local = f.local_patch_set(cx, 1);
    f.pick(cx, |sel, d| sel.set_target(d, local));
    let rows = chain(cx, &f);
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[1], row("#9 local 1", "Second, amended", true));
    f.assert_nothing_local_sent(&[&s.first, &s.second, &s.third]);
}

#[gpui_kit::test]
fn a_chain_opens_an_older_version_at_that_version_and_steps_past_plain_commits(
    cx: &mut TestAppContext,
) {
    // B (I3) is built on A's first version through a plain commit; A (I1)
    // was then amended on another branch, B not rebased.
    let repo = TestRepo::new();
    repo.write("README", b"one\n");
    let base = repo.commit("Base");
    repo.write("a", b"a\n");
    let a1 = repo.commit("First local\n\nChange-Id: I1");
    repo.write("p", b"p\n");
    repo.commit("Plain");
    repo.write("b", b"b\n");
    let b = repo.commit("Built on the first\n\nChange-Id: I3");
    repo.git(&["checkout", "-q", "-b", "other", &base]);
    repo.write("a", b"amended\n");
    let a2 = repo.commit("First local, amended\n\nChange-Id: I1");
    let store = store_with(
        &[
            row("I1", None, LocalChangeState::Local, &[&a1, &a2], &base),
            row("I3", None, LocalChangeState::Local, &[&b], &a1),
        ],
        |_| {},
    );
    let a = store
        .local_changes_by_change_id(SERVER, "I1")
        .expect("rows")
        .remove(0);
    let first_of_a = store.local_patch_sets(SERVER, a.id).expect("versions")[0].number;
    let number_of_b = store
        .local_changes_by_change_id(SERVER, "I3")
        .expect("rows")[0]
        .number;
    let project = ProjectSettings {
        local_changes: true,
        ..ProjectSettings::new("p", repo.path().to_string_lossy())
    };
    let f = page_in(cx, store, number_of_b, project, |_| {});
    for _ in 0..100 {
        if !cx.update(|cx| f.page.read(cx).chain_for_test()).is_empty() {
            break;
        }
        cx.executor().allow_parking();
        std::thread::sleep(std::time::Duration::from_millis(20));
        cx.run_until_parked();
    }
    let labels: Vec<String> = cx.update(|cx| {
        f.page
            .read(cx)
            .chain_for_test()
            .into_iter()
            .map(|r| r.0)
            .collect()
    });
    assert_eq!(labels.len(), 4, "{labels:?}");
    assert_eq!(labels[0], "local");
    assert_eq!(labels[2], "local");

    // A's first version, not its newest one (which B is not built on).
    f.page.update(cx, |p, cx| p.open_chain_row(2, false, cx));
    // Shift-J passes the plain commit; Shift-K at the top does nothing.
    f.press(cx, "shift-j");
    f.press(cx, "shift-k");
    let at = format!("{:?}", Some(first_of_a));
    assert_eq!(
        *f.events.lock().expect("log"),
        [
            format!("change {} at {at}", a.number),
            format!("replace {} at {at}", a.number),
        ]
    );
    assert!(f.http.requested().is_empty());
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_vote_on_a_local_version_moves_on_along_the_chain_from_the_checkout(cx: &mut TestAppContext) {
    let s = series();
    let store = s.store();
    let number = |id: &str| {
        store
            .local_changes_by_change_id(SERVER, id)
            .expect("rows")
            .remove(0)
            .number
    };
    let (first, third) = (number("I1"), number("I3"));
    let version_of_9 = store
        .local_patch_sets_by_sha(SERVER, &s.second.clone().into())
        .expect("versions")[0]
        .number;
    let wait_for_chain = |cx: &mut TestAppContext, f: &Fixture| {
        for _ in 0..100 {
            if !cx.update(|cx| f.page.read(cx).chain_for_test()).is_empty() {
                break;
            }
            cx.executor().allow_parking();
            std::thread::sleep(std::time::Duration::from_millis(20));
            cx.run_until_parked();
        }
    };

    // A local change: on to the local version of Gerrit change 9 built on
    // it, at that version.
    let f = page_in(cx, store, first, s.project(), |_| {});
    wait_for_chain(cx, &f);
    f.press(cx, "shift-v");
    assert_eq!(
        *f.events.lock().expect("log"),
        [
            "local review saved".to_owned(),
            format!(
                "advance 9 at {:?}: Saved Code-Review +2 locally; sent when local 1 is on \
                 Gerrit, moved to #9 local 1",
                Some(version_of_9)
            )
        ]
    );

    // That version: on to the local change built on it, named by its
    // subject (its number is synthetic).
    let mut store = s.store();
    let mut nine = store
        .load_change_detail(SERVER, ChangeNumber(9))
        .expect("load")
        .expect("cached")
        .detail;
    nine.permitted_labels = [("Code-Review".to_owned(), vec![-2, -1, 0, 1, 2])].into();
    store
        .save_change_detail(SERVER, &nine, Timestamp::now())
        .expect("detail");
    let f = page_in(cx, store, ChangeNumber(9), s.project(), |_| {});
    let local = f.local_patch_set(cx, 1);
    f.pick(cx, |sel, d| sel.set_target(d, local));
    wait_for_chain(cx, &f);
    f.press(cx, "shift-v");
    let events = f.events.lock().expect("log").clone();
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(
        events[1].starts_with(&format!("advance {third} at None: Saved ")),
        "{events:?}"
    );
    assert!(
        events[1].ends_with("moved to local change \u{201c}Third local\u{201d}"),
        "{events:?}"
    );
    f.assert_nothing_local_sent(&[&s.first, &s.second, &s.third]);
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_long_chain_keeps_the_commits_nearest_and_says_there_are_more(cx: &mut TestAppContext) {
    let repo = TestRepo::new();
    repo.commit("Base");
    for i in 0..26 {
        repo.commit(&format!("Below {i}"));
    }
    let target = repo.commit("The change\n\nChange-Id: I1");
    for i in 0..26 {
        repo.commit(&format!("Above {i}"));
    }
    let store = store_with(
        &[row("I1", None, LocalChangeState::Local, &[&target], "x")],
        |_| {},
    );
    let number = store
        .local_changes_by_change_id(SERVER, "I1")
        .expect("rows")[0]
        .number;
    let project = ProjectSettings {
        local_changes: true,
        ..ProjectSettings::new("p", repo.path().to_string_lossy())
    };
    let f = page_in(cx, store, number, project, |_| {});
    let mut rows = Vec::new();
    for _ in 0..100 {
        rows = cx.update(|cx| f.page.read(cx).chain_for_test());
        if !rows.is_empty() {
            break;
        }
        cx.executor().allow_parking();
        std::thread::sleep(std::time::Duration::from_millis(20));
        cx.run_until_parked();
    }
    let subjects: Vec<&str> = rows.iter().map(|r| r.1.as_str()).collect();
    let at = rows.iter().position(|r| r.2).expect("this change");
    // 25 above, nearest first kept; the commit and 24 below it.
    assert_eq!(rows.len(), 1 + 25 + 1 + 24 + 1, "{subjects:?}");
    assert_eq!(rows[0].0, "\u{2026}");
    assert_eq!(subjects[1], "Above 24");
    assert_eq!(subjects[at - 1], "Above 0");
    assert_eq!(subjects[at], "The change");
    assert_eq!(subjects[at + 1], "Below 25");
    assert_eq!(subjects[rows.len() - 2], "Below 2");
    assert_eq!(rows[rows.len() - 1].0, "\u{2026}");
}

/// The relation chain of `f`'s page once it has `rows` rows (git runs on
/// the background executor), as drawn.
fn chain_of(cx: &mut TestAppContext, f: &Fixture, rows: usize) -> Vec<(String, String, bool)> {
    for _ in 0..100 {
        if cx.update(|cx| f.page.read(cx).chain_for_test()).len() >= rows {
            break;
        }
        cx.executor().allow_parking();
        std::thread::sleep(std::time::Duration::from_millis(20));
        cx.run_until_parked();
    }
    cx.update(|cx| f.page.read(cx).chain_for_test())
}

/// Gerrit change 7, whose patch set 3 is in the checkout on `main` with
/// two local commits on top: a local change (I1), then a local version
/// of Gerrit change 9 (I9). Gerrit's chain of PS 3, cached, is 7 on 6.
struct Stacked {
    repo: TestRepo,
    ps3: String,
    first: String,
    second: String,
}

fn stacked() -> Stacked {
    let repo = TestRepo::new();
    repo.write("README", b"one\n");
    repo.commit("Base");
    repo.write("README", b"two\n");
    let ps3 = repo.commit(&format!("On Gerrit\n\nChange-Id: {}", id_of_7()));
    repo.write("a", b"a\n");
    let first = repo.commit("First local\n\nChange-Id: I1");
    repo.write("b", b"b\n");
    let second = repo.commit("Second, a version of 9\n\nChange-Id: I9");
    Stacked {
        repo,
        ps3,
        first,
        second,
    }
}

impl Stacked {
    fn store(&self) -> Store {
        let rows = [
            row(
                "I1",
                None,
                LocalChangeState::Local,
                &[&self.first],
                &self.ps3,
            ),
            row(
                "I9",
                Some(ChangeNumber(9)),
                LocalChangeState::Local,
                &[&self.second],
                &self.first,
            ),
        ];
        store_with(&rows, |store| {
            let mut d = detail(7, 3, 3);
            d.summary.project = "p".into();
            d.summary.change_id = Some(id_of_7());
            d.revisions[2].sha = self.ps3.clone().into();
            d.current_revision = Some(self.ps3.clone().into());
            store
                .save_change_detail(SERVER, &d, Timestamp::now())
                .expect("detail");
            let mut d9 = detail(9, 1, 1);
            d9.summary.project = "p".into();
            d9.summary.change_id = Some("I9".into());
            store
                .save_change_detail(SERVER, &d9, Timestamp::now())
                .expect("detail");
            let related = |n: u64, ps: u32| model::RelatedChange {
                project: "p".into(),
                change_id: None,
                number: ChangeNumber(n),
                patch_set: PatchSetNumber(ps),
                current_patch_set: Some(PatchSetNumber(ps)),
                status: Some(model::ChangeStatus::New),
                submittable: None,
                work_in_progress: false,
                commit: None,
            };
            store
                .save_related(
                    SERVER,
                    ChangeNumber(7),
                    &self.ps3.clone().into(),
                    &[related(7, 3), related(6, 1)],
                    Timestamp::now(),
                )
                .expect("related");
        })
    }

    fn project(&self, local_changes: bool) -> ProjectSettings {
        ProjectSettings {
            local_changes,
            ..ProjectSettings::new("p", self.repo.path().to_string_lossy())
        }
    }
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn local_commits_on_a_gerrit_patch_set_join_its_relation_chain(cx: &mut TestAppContext) {
    let s = stacked();
    let store = s.store();
    let first = store
        .local_changes_by_change_id(SERVER, "I1")
        .expect("rows")
        .remove(0)
        .number;
    let version_of_9 = store
        .local_patch_sets_by_sha(SERVER, &s.second.clone().into())
        .expect("versions")[0]
        .number;
    let f = page_in(cx, store, ChangeNumber(7), s.project(true), |_| {});
    let row = |label: &str, subject: &str, this: bool| (label.to_owned(), subject.to_owned(), this);
    // Above PS 3 in Gerrit's chain, newest first, named as local.
    let chain = chain_of(cx, &f, 4);
    assert_eq!(chain.len(), 4, "{chain:?}");
    assert_eq!(chain[0], row("#9 local 1", "Second, a version of 9", false));
    assert_eq!(chain[1], row("local", "First local", false));
    assert!(chain[2].0 == "#7" && chain[2].2, "{chain:?}");
    assert_eq!(chain[3].0, "#6");
    let mut visual = gpui_kit::VisualTestContext::from_window(f.window, cx);
    visual.run_until_parked();
    assert!(visual.debug_bounds("chain-row: #9 local 1").is_some());

    // Clicks open each at the right version; Shift-K steps up to the
    // local change, then to the version of 9; Shift-J back down to 6.
    f.page.update(cx, |p, cx| p.open_chain_row(0, false, cx));
    f.page.update(cx, |p, cx| p.open_chain_row(1, false, cx));
    f.press(cx, "shift-k");
    f.press(cx, "shift-j");
    let v9 = format!("{:?}", Some(version_of_9));
    assert_eq!(
        *f.events.lock().expect("log"),
        [
            format!("change 9 at {v9}"),
            format!("change {first}"),
            format!("replace {first}"),
            "replace 6".to_owned(),
        ]
    );
    let labels: Vec<String> = chain_of(cx, &f, 4).into_iter().map(|r| r.0).collect();
    assert_no_synthetic(&labels, &[first.0, u64::from(version_of_9.0)]);
    f.assert_nothing_local_sent(&[&s.first, &s.second]);

    // Without the option the chain is Gerrit's alone.
    let f = page_in(cx, s.store(), ChangeNumber(7), s.project(false), |_| {});
    // Give a read the time it would take.
    chain_of(cx, &f, 3);
    let labels: Vec<String> = chain_of(cx, &f, 2).into_iter().map(|r| r.0).collect();
    assert_eq!(labels, ["#7", "#6"]);
}

#[gpui_kit::test]
fn local_commits_make_a_chain_of_a_change_gerrit_has_none_for(cx: &mut TestAppContext) {
    let s = stacked();
    let mut store = s.store();
    store
        .save_related(
            SERVER,
            ChangeNumber(7),
            &s.ps3.clone().into(),
            &[],
            Timestamp::now(),
        )
        .expect("related");
    let f = page_in(cx, store, ChangeNumber(7), s.project(true), |_| {});
    let chain = chain_of(cx, &f, 3);
    let labels: Vec<&str> = chain.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(labels, ["#9 local 1", "local", "#7"], "{chain:?}");
    assert!(chain[2].2);
    f.press(cx, "shift-k");
    assert_eq!(f.events.lock().expect("log").len(), 1);
    f.assert_nothing_local_sent(&[&s.first, &s.second]);
}

#[gpui_kit::test]
fn a_commit_gerrit_has_in_the_chain_shows_once(cx: &mut TestAppContext) {
    // `first` was just pushed as change 8: Gerrit's chain has it, but 8
    // is not cached, so the store still takes it for a local change.
    let s = stacked();
    let mut store = s.store();
    let related = |n: u64, sha: Option<&str>, subject: &str| model::RelatedChange {
        project: "p".into(),
        change_id: None,
        number: ChangeNumber(n),
        patch_set: PatchSetNumber(1),
        current_patch_set: Some(PatchSetNumber(1)),
        status: Some(model::ChangeStatus::New),
        submittable: None,
        work_in_progress: false,
        commit: Some(model::Commit {
            sha: sha.map(Into::into),
            parents: Vec::new(),
            author: None,
            committer: None,
            subject: subject.into(),
            message: None,
        }),
    };
    let chain = [
        related(8, Some(&s.first), "First local"),
        related(7, Some(&s.ps3), "On Gerrit"),
        related(6, None, "Below"),
    ];
    store
        .save_related(
            SERVER,
            ChangeNumber(7),
            &s.ps3.clone().into(),
            &chain,
            Timestamp::now(),
        )
        .expect("related");
    let f = page_in(cx, store, ChangeNumber(7), s.project(true), |_| {});
    let chain = chain_of(cx, &f, 4);
    let labels: Vec<&str> = chain.iter().map(|r| r.0.as_str()).collect();
    // The version of 9 goes above 8, which it is built on; `first` is 8's
    // row only.
    assert_eq!(labels, ["#9 local 1", "#8", "#7", "#6"], "{chain:?}");
    f.assert_nothing_local_sent(&[&s.first, &s.second]);
}

/// The held card: what consolidation held, published or dropped item by
/// item.
mod held;
/// Reviews on local versions: drafts, the reply dialog's pending reviews,
/// quick votes and the list of pending reviews.
mod review;
