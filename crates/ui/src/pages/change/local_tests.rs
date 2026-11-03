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
    let projects = vec![ck.project()];
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
                ChangeEvent::Refreshed => "refreshed",
                ChangeEvent::Reply(_) => "reply",
                ChangeEvent::Actions(_) => "actions",
                ChangeEvent::Unauthorized => "unauthorized",
                _ => "other",
            };
            log.lock().expect("log").push(text.to_owned());
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
        // No log, no votes.
        assert!(!p.blocks_for_test().contains(&Block::LogHeader));
        assert!(p.quick_buttons_for_test(cx).is_empty());
        assert_eq!(*p.status_for_test(), crate::pages::list::Status::Idle);
    });
    let target = f.target_choices(cx);
    assert_eq!(target, ["local 1 (current)"]);

    // Drawn: the state note.
    let mut visual = gpui_kit::VisualTestContext::from_window(f.window, cx);
    visual.run_until_parked();
    assert!(visual.debug_bounds("local-note-checking").is_some());

    // Neither a reply nor the action menu (both would go to Gerrit), nor
    // reviewed marks.
    f.press(cx, "a");
    assert_eq!(f.hint(cx).as_deref(), Some(crate::reply::LOCAL_LATER));
    f.press(cx, "m");
    assert_eq!(f.hint(cx).as_deref(), Some(crate::action_menu::LOCAL));
    f.press(cx, "r");
    assert_eq!(f.hint(cx).as_deref(), Some(crate::reviewed::LOCAL));
    // Expanding a file diffs it in the checkout; /COMMIT_MSG is not served
    // locally yet, and Gerrit is not asked for it.
    f.press(cx, "shift-i");
    cx.update(|cx| {
        let p = f.page.read(cx);
        assert!(p.inline_ready("README") && p.inline_ready("src/new.rs"));
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

    // The local version as the target: its files come from the checkout.
    let local = f.local_patch_set(cx, 1);
    f.pick(cx, |s, d| s.set_target(d, local));
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
