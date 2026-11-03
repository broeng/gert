//! Consolidation driven from the UI: a shell on a scripted server, a real
//! temporary checkout (local-git's `TestRepo`) with the user's own commit,
//! and the run that sends its local review once the server has the
//! commit. The fake HTTP client panics on any synthetic number, so every
//! test also checks that none reaches the wire.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use futures::{
    channel::oneshot,
    future::{BoxFuture, FutureExt as _},
};
use gerrit_api::{HttpClient, HttpError, fake::RouteHttpClient};
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
use local_git::testutil::{TEST_EMAIL, TestRepo};
use model::{
    ChangeNumber, LocalApproval, LocalChange, LocalChangeId, LocalPatchSet, PatchSetNumber,
    PendingReview, PendingReviewId, PendingReviewState, Timestamp,
};
use serde_json::{Value, json};
use store::{NewPendingReview, ServerId, Store};
use sync::{ConsolidateReport, FoundChange, PostOutcome, PostReport};

use super::{Consolidator, note, options};
use crate::{
    checkouts::Checkouts,
    nav::{ChangeView, DiffRoute, Route},
    servers::Connection,
    session::{ProjectSettings, SavedQuery, ServerSetup, Startup},
    shell::Shell,
    tests::{APP, URL, env_with, open, setup},
};

const ID_A: &str = "I0123456789abcdef0123456789abcdef01234567";
const SERVER: ServerId = ServerId(1);
const ME: u64 = 1000;
const GERRIT: ChangeNumber = ChangeNumber(42);

/// The scripted server, and POSTs held back while `hold` is set.
#[derive(Clone, Default)]
struct Http {
    routes: RouteHttpClient,
    hold: Arc<Mutex<Option<Vec<oneshot::Sender<()>>>>>,
}

impl Http {
    /// From now on POSTs wait for [`Http::release`].
    fn hold_posts(&self) {
        *self.hold.lock().expect("hold") = Some(Vec::new());
    }

    /// Answers the POSTs that waited, and those to come at once.
    fn release(&self) {
        for tx in self.hold.lock().expect("hold").take().unwrap_or_default() {
            tx.send(()).ok();
        }
    }

    fn posts(&self) -> Vec<(String, Value)> {
        self.routes
            .sent()
            .into_iter()
            .filter(|r| r.method == "POST")
            .map(|r| (r.target, serde_json::from_slice(&r.body).expect("json")))
            .collect()
    }
}

impl HttpClient for Http {
    fn send(
        &self,
        req: http::Request<Vec<u8>>,
    ) -> BoxFuture<'static, Result<http::Response<Vec<u8>>, HttpError>> {
        let post = req.method() == http::Method::POST;
        let routed = self.routes.send(req);
        let wait = post
            .then(|| {
                self.hold.lock().expect("hold").as_mut().map(|waiting| {
                    let (tx, rx) = oneshot::channel();
                    waiting.push(tx);
                    rx
                })
            })
            .flatten();
        async move {
            if let Some(rx) = wait {
                rx.await.ok();
            }
            routed.await
        }
        .boxed()
    }
}

/// A checkout of project `p` whose `main` tracks `origin/main`, with one
/// pushed commit and one commit of the user's own on top.
struct Fixture {
    _remote: TestRepo,
    repo: TestRepo,
    http: Http,
    /// The user's own commit.
    sha: String,
}

fn fixture() -> Fixture {
    let remote = TestRepo::bare();
    let repo = TestRepo::new();
    repo.commit("Base");
    let url = remote.path().to_str().expect("utf-8 temp path").to_owned();
    repo.git(&["remote", "add", "origin", &url]);
    repo.git(&["push", "-q", "-u", "origin", "main"]);
    let sha = repo.commit(&format!("Own change\n\nChange-Id: {ID_A}"));
    let http = Http::default();
    // The dashboard's queries and the lookup find nothing yet.
    http.routes.route_json("/changes/", "[]");
    http.routes.route_json(
        "/accounts/self",
        &json!({"_account_id": ME, "email": TEST_EMAIL, "username": "u"}).to_string(),
    );
    Fixture {
        _remote: remote,
        repo,
        http,
        sha,
    }
}

impl Fixture {
    fn project(&self) -> ProjectSettings {
        ProjectSettings {
            local_changes: true,
            ..ProjectSettings::new("p", self.repo.path().to_string_lossy())
        }
    }

    /// The user pushed the commit: Gerrit has it as change 42, patch set 1,
    /// and takes reviews on it.
    fn pushed(&self) {
        let summary = summary_json();
        let mut detail = summary.clone();
        let fields = json!({
            "current_revision": self.sha,
            "current_revision_number": 1,
            "revisions": {self.sha.clone(): {"_number": 1}},
            "labels": {
                "Code-Review": {"values": {"-2": "", "-1": "", " 0": "", "+1": "", "+2": ""},
                                "all": [{"_account_id": ME, "value": 0}]},
            },
            "messages": [],
        });
        for (k, v) in fields.as_object().expect("object") {
            detail[k] = v.clone();
        }
        let routes = &self.http.routes;
        routes.route_json("/changes/?q=%28", &json!([summary]).to_string());
        routes.route_json("/changes/?q=change%3A", &json!([detail]).to_string());
        routes.route_json("/changes/42?", &detail.to_string());
        routes.route_json(
            "POST /changes/42/revisions/",
            &json!({"labels": {"Code-Review": 1},
                    "change_info": {"current_revision_number": 1}})
            .to_string(),
        );
    }
}

fn summary_json() -> Value {
    json!({
        "_number": GERRIT.0, "project": "p", "branch": "main", "change_id": ID_A,
        "subject": "Own change", "status": "NEW",
        "created": "2024-01-01 00:00:00.000000000",
        "updated": "2024-01-02 00:00:00.000000000", "owner": {"_account_id": ME},
    })
}

struct Started {
    window: AnyWindowHandle,
    shell: Entity<Shell>,
    store: Arc<Mutex<Store>>,
}

/// A shell on server `test`, signed in as `username` (the password in the
/// keyring) or anonymous, with project `p` at the fixture's checkout.
fn start(cx: &mut TestAppContext, f: &Fixture, username: Option<&str>) -> Started {
    let mut store = Store::open_in_memory().expect("store");
    assert_eq!(store.server_id(URL).expect("server id"), SERVER);
    let mut env = env_with(store, &f.http);
    env.backend = Arc::new(crate::login::tests::FakeBackend {
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
            projects: vec![f.project()],
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
    fn consolidator(&self, cx: &mut TestAppContext) -> Entity<Consolidator> {
        cx.update(|cx| self.shell.read(cx).consolidation_for_test("test"))
            .expect("a consolidator")
    }

    fn runs(&self, cx: &mut TestAppContext) -> usize {
        let c = self.consolidator(cx);
        cx.update(|cx| c.read(cx).runs())
    }

    /// Asks for a run as a refresh that worked does, and waits.
    fn trigger(&self, cx: &mut TestAppContext) {
        self.shell
            .update(cx, |s, cx| s.consolidate_for_test("test", cx));
        cx.run_until_parked();
    }

    /// The local change row the scanner made of the commit, and its
    /// version.
    fn row(&self) -> (LocalChange, LocalPatchSet) {
        let store = self.store.lock().expect("store");
        let row = store
            .local_changes_by_change_id(SERVER, ID_A)
            .expect("rows")
            .into_iter()
            .next()
            .expect("the scanner found the commit");
        let version = store
            .local_patch_sets(SERVER, row.id)
            .expect("versions")
            .remove(0);
        (row, version)
    }

    /// A local review of the commit: Code-Review +1 and a message.
    fn review(&self) -> PendingReview {
        let (_, version) = self.row();
        self.store
            .lock()
            .expect("store")
            .create_pending_review(
                SERVER,
                &NewPendingReview {
                    patch_set: version.number,
                    labels: BTreeMap::from([("Code-Review".to_owned(), 1)]),
                    strategy: LocalApproval::Exact,
                    message: "Looks right".into(),
                    unresolved: false,
                    created: Timestamp::now(),
                    replaces_draft: None,
                },
            )
            .expect("review")
    }

    fn review_state(&self, id: PendingReviewId) -> PendingReviewState {
        self.store
            .lock()
            .expect("store")
            .load_pending_review(SERVER, id)
            .expect("load")
            .expect("review")
            .state
    }

    fn open(&self, cx: &mut TestAppContext, route: Route) {
        cx.update_window(self.window, |_, window, cx| {
            self.shell
                .update(cx, |s, cx| s.navigate_for_test(route, window, cx))
        })
        .expect("window");
        cx.run_until_parked();
    }

    fn page_hint(&self, cx: &mut TestAppContext) -> Option<(String, bool)> {
        cx.update(|cx| {
            let shell = self.shell.read(cx);
            if let Some(p) = shell.change_page_for_test() {
                return p.read(cx).hint_for_test();
            }
            shell.diff_page_for_test()?.read(cx).hint_for_test()
        })
    }
}

/// No request named a synthetic number.
fn assert_no_local_number_sent(f: &Fixture) {
    let base = ChangeNumber::LOCAL_BASE.to_string();
    let prefix = &base[..6];
    for r in f.http.routes.requested() {
        assert!(!r.contains(prefix), "{r}");
    }
}

#[gpui_kit::test]
fn a_pushed_local_change_has_its_review_sent_under_the_app_name(cx: &mut TestAppContext) {
    let f = fixture();
    let s = start(cx, &f, Some("u"));
    // The scanner found the commit, and the dashboard shows it.
    let (row, version) = s.row();
    assert!(s.runs(cx) >= 1, "the first refresh asked for a run");
    let list = cx
        .update(|cx| s.shell.read(cx).list_page_for_test())
        .expect("dashboard");
    assert_eq!(cx.update(|cx| list.read(cx).local_rows().len()), 1);
    let review = s.review();

    // Not on Gerrit yet: nothing is sent.
    s.trigger(cx);
    assert!(f.http.posts().is_empty());
    assert_eq!(s.review_state(review.id), PendingReviewState::Pending);

    f.pushed();
    s.trigger(cx);
    let posts = f.http.posts();
    assert_eq!(posts.len(), 1, "{posts:?}");
    let (target, body) = &posts[0];
    assert!(
        target.contains(&format!("/changes/42/revisions/{}/review", version.sha.0)),
        "{target}"
    );
    assert_eq!(body["labels"], json!({"Code-Review": 1}));
    let tag = body["tag"].as_str().expect("tag");
    assert!(
        tag.starts_with(&format!("{APP}-local-{}-", row.id.0)),
        "{tag}"
    );
    assert_eq!(s.review_state(review.id), PendingReviewState::Sent);
    // The dashboard read its local section again: the row is done.
    assert_eq!(cx.update(|cx| list.read(cx).local_rows().len()), 0);
    assert_no_local_number_sent(&f);
}

#[gpui_kit::test]
fn the_open_page_follows_the_change_to_gerrit_and_says_what_was_sent(cx: &mut TestAppContext) {
    let f = fixture();
    let s = start(cx, &f, Some("u"));
    let (row, version) = s.row();
    s.review();
    s.open(cx, Route::Change(row.number));
    cx.update_window(s.window, |_, window, cx| {
        s.shell.update(cx, |s, cx| {
            s.nav_for_test().set_change_view(Some(ChangeView {
                target: Some(version.number),
                ..Default::default()
            }));
            s.show_for_test(window, cx);
        })
    })
    .expect("window");
    cx.run_until_parked();
    let (trail, position) = cx.update(|cx| s.shell.read(cx).trail_for_test());

    f.pushed();
    s.trigger(cx);
    // The same history entry, now on the Gerrit change at the patch set the
    // version became; nothing was added or dropped.
    assert_eq!(
        cx.update(|cx| s.shell.read(cx).route().clone()),
        Route::Change(GERRIT)
    );
    let (now, at) = cx.update(|cx| s.shell.read(cx).trail_for_test());
    assert_eq!((now.len(), at), (trail.len(), position));
    let page = cx
        .update(|cx| s.shell.read(cx).change_page_for_test())
        .expect("change page");
    assert_eq!(
        cx.update(|cx| page.read(cx).view_state().target),
        Some(PatchSetNumber(1))
    );
    assert_eq!(
        s.page_hint(cx),
        Some(("Your local review was sent to Gerrit".to_owned(), false))
    );
    assert_no_local_number_sent(&f);
}

#[gpui_kit::test]
fn history_entries_of_the_local_change_lead_to_gerrit_when_visited(cx: &mut TestAppContext) {
    let f = fixture();
    let s = start(cx, &f, Some("u"));
    let (row, version) = s.row();
    s.review();
    let diff = |change, target| DiffRoute {
        change,
        base: None,
        target,
        path: "a.txt".into(),
        thread: None,
    };
    s.open(cx, Route::Change(row.number));
    s.open(cx, Route::Diff(diff(row.number, version.number)));
    s.open(cx, Route::Search("status:open".into()));
    f.pushed();
    s.trigger(cx);
    let back = |cx: &mut TestAppContext| {
        cx.update_window(s.window, |_, window, cx| {
            s.shell.update(cx, |s, cx| s.back_for_test(window, cx))
        })
        .expect("window");
        cx.run_until_parked();
        cx.update(|cx| s.shell.read(cx).route().clone())
    };
    assert_eq!(back(cx), Route::Diff(diff(GERRIT, PatchSetNumber(1))));
    assert_eq!(back(cx), Route::Change(GERRIT));
    // Forward still leads the way it did.
    let (trail, position) = cx.update(|cx| s.shell.read(cx).trail_for_test());
    let routes: Vec<Route> = trail.into_iter().map(|(_, r)| r).collect();
    assert_eq!(
        &routes[position..],
        [
            Route::Change(GERRIT),
            Route::Diff(diff(GERRIT, PatchSetNumber(1))),
            Route::Search("status:open".into()),
        ]
    );
    assert_no_local_number_sent(&f);
}

#[test]
fn a_local_version_of_a_gerrit_change_leads_to_the_patch_set_it_became() {
    use store::{LocalChangeUpsert, LocalOp, NewLocalPatchSet};

    let mut store = Store::open_in_memory().expect("store");
    let server = store.server_id(URL).expect("server");
    let now: Timestamp = "2024-06-15T10:00:00Z".parse().expect("time");
    let upsert = LocalChangeUpsert {
        seen: None,
        branch: Some("main".into()),
        change_id: ID_A.into(),
        gerrit_number: Some(GERRIT),
        subject: "s".into(),
        author_name: "Me".into(),
        author_email: TEST_EMAIL.into(),
        updated: now,
        state: None,
        patch_sets: vec![NewLocalPatchSet {
            sha: "l2".into(),
            parent_sha: None,
            seen_at: now,
        }],
    };
    store
        .reconcile_local(server, "p", &[LocalOp::Upsert(upsert)], now)
        .expect("reconcile");
    let row = store
        .local_changes_by_change_id(server, ID_A)
        .expect("rows")
        .remove(0);
    let version = store
        .local_patch_sets(server, row.id)
        .expect("versions")
        .remove(0);
    let env = env_with(store, &RouteHttpClient::new());
    let session = crate::session::Session::new(&env, &setup(), None).expect("session");
    let syncer = session.syncer();
    let at = |target| ChangeView {
        target: Some(target),
        file: Some("a.txt".into()),
        ..Default::default()
    };
    let route = Route::Change(GERRIT);

    // Not a Gerrit patch set yet: the page stays.
    assert_eq!(
        super::redirect(syncer, &route, Some(&at(version.number))).expect("redirect"),
        None
    );
    // Uploaded as patch set 2: the selection follows.
    let mut d = crate::testing::detail(GERRIT.0, 2, 2);
    d.summary.project = "p".into();
    d.summary.change_id = Some(ID_A.into());
    d.revisions[1].sha = "l2".into();
    d.current_revision = Some("l2".into());
    env.store
        .lock()
        .expect("store")
        .save_change_detail(server, &d, now)
        .expect("save");
    assert_eq!(
        super::redirect(syncer, &route, Some(&at(version.number))).expect("redirect"),
        Some(super::Redirect {
            route: route.clone(),
            view: Some(at(PatchSetNumber(2))),
        })
    );
    // A route without local numbers is never looked at.
    assert_eq!(
        super::redirect(syncer, &route, Some(&at(PatchSetNumber(1)))).expect("redirect"),
        None
    );
}

#[gpui_kit::test]
fn nothing_runs_offline_or_anonymously(cx: &mut TestAppContext) {
    let f = fixture();
    let anonymous = start(cx, &f, None);
    assert!(
        cx.update(|cx| anonymous.shell.read(cx).consolidation_for_test("test"))
            .is_none()
    );

    let s = start(cx, &f, Some("u"));
    s.review();
    f.pushed();
    s.shell.update(cx, |s, _| {
        s.set_connection_for_test("test", Connection::Offline)
    });
    let runs = s.runs(cx);
    s.trigger(cx);
    assert_eq!(s.runs(cx), runs);
    assert!(f.http.posts().is_empty());

    // Online again (a refresh worked): it runs.
    s.shell.update(cx, |s, _| {
        s.set_connection_for_test("test", Connection::Online)
    });
    s.trigger(cx);
    assert_eq!(s.runs(cx), runs + 1);
    assert_eq!(f.http.posts().len(), 1);
}

#[gpui_kit::test]
fn a_failed_run_backs_off_and_marks_the_server_offline(cx: &mut TestAppContext) {
    let f = fixture();
    // The lookup does not get through.
    f.http.routes.route("/changes/?q=%28", 503, "unavailable");
    let s = start(cx, &f, Some("u"));
    s.row();
    let c = s.consolidator(cx);
    assert!(s.runs(cx) >= 1);
    assert!(cx.update(|cx| c.read(cx).backing_off()));
    assert_eq!(
        cx.update(|cx| s.shell.read(cx).connection_for_test("test")),
        Some(Connection::Offline)
    );

    // A refresh works again, but the back-off holds.
    s.shell.update(cx, |s, _| {
        s.set_connection_for_test("test", Connection::Online)
    });
    let runs = s.runs(cx);
    s.trigger(cx);
    assert_eq!(s.runs(cx), runs);

    // Once it has passed, the next request runs, and a run that works
    // ends the back-off.
    f.http.routes.route_json("/changes/?q=%28", "[]");
    c.update(cx, |c, _| c.end_backoff());
    s.trigger(cx);
    assert_eq!(s.runs(cx), runs + 1);
    assert!(!cx.update(|cx| c.read(cx).backing_off()));
}

#[gpui_kit::test]
fn requests_during_a_run_make_one_more_run_after_it(cx: &mut TestAppContext) {
    let f = fixture();
    let s = start(cx, &f, Some("u"));
    let review = s.review();
    f.pushed();
    f.http.hold_posts();
    let runs = s.runs(cx);
    s.trigger(cx);
    let c = s.consolidator(cx);
    assert!(
        cx.update(|cx| c.read(cx).is_running()),
        "waiting for the POST"
    );
    assert_eq!(s.review_state(review.id), PendingReviewState::Sending);

    // More requests, from a refresh and a scan: none starts a run.
    s.trigger(cx);
    s.trigger(cx);
    assert_eq!(s.runs(cx), runs + 1);
    assert_eq!(f.http.posts().len(), 1);

    f.http.release();
    cx.run_until_parked();
    // The post went through, then one more run, which had nothing to send.
    assert_eq!(s.runs(cx), runs + 2);
    assert!(!cx.update(|cx| c.read(cx).is_running()));
    assert_eq!(f.http.posts().len(), 1);
    assert_eq!(s.review_state(review.id), PendingReviewState::Sent);
}

#[test]
fn a_run_takes_each_projects_local_approval() {
    let project = |name: &str, approval| ProjectSettings {
        local_approval: approval,
        ..ProjectSettings::new(name, "/tmp/x")
    };
    let checkouts = Checkouts::new(vec![
        project("a", LocalApproval::Follow),
        project("b", LocalApproval::ExactOrManual),
        ProjectSettings {
            local_changes: true,
            ..project("c", LocalApproval::Exact)
        },
    ]);
    let options = options(&checkouts);
    assert_eq!(
        options.local_approval,
        BTreeMap::from([
            ("a".to_owned(), LocalApproval::Follow),
            ("b".to_owned(), LocalApproval::ExactOrManual),
            ("c".to_owned(), LocalApproval::Exact),
        ])
    );
}

fn post(row: i64, change: ChangeNumber, review: bool, outcome: PostOutcome) -> PostReport {
    PostReport {
        local_change: LocalChangeId(row),
        change,
        patch_set: PatchSetNumber(1),
        review: review.then_some(PendingReviewId(1)),
        comments: 1,
        votes: Vec::new(),
        recovered: false,
        outcome,
    }
}

#[test]
fn the_note_names_the_worst_outcome_for_the_change_shown() {
    let local = LocalChangeId(3).number().expect("number");
    let mut report = ConsolidateReport {
        ran: true,
        found: vec![FoundChange {
            local_change: LocalChangeId(3),
            local_number: local,
            gerrit: GERRIT,
        }],
        ..Default::default()
    };
    // Found, nothing sent.
    assert_eq!(
        note(&report, local),
        Some(("This change is on Gerrit now".to_owned(), false))
    );
    assert_eq!(note(&report, ChangeNumber(7)), None);

    report.posts.push(post(3, GERRIT, false, PostOutcome::Sent));
    let sent = Some(("Your local comments were sent to Gerrit".to_owned(), false));
    assert_eq!(note(&report, local), sent);
    assert_eq!(note(&report, GERRIT), sent, "the same change by its number");

    report.posts.push(post(
        3,
        GERRIT,
        true,
        PostOutcome::VotesHeld {
            votes: vec![("Code-Review".into(), 2)],
            drop_only: vec!["Code-Review".into()],
            reason: "PS 2 is current".into(),
        },
    ));
    let (text, error) = note(&report, GERRIT).expect("note");
    assert!(
        text.contains("some votes were held: PS 2 is current"),
        "{text}"
    );
    assert!(!error);

    report.posts.push(post(
        3,
        GERRIT,
        true,
        PostOutcome::Refused { error: "no".into() },
    ));
    assert_eq!(
        note(&report, GERRIT),
        Some(("Gerrit refused your local review: no".to_owned(), true))
    );

    // Another change, found closed.
    let closed = ConsolidateReport {
        ran: true,
        closed: vec![(LocalChangeId(4), ChangeNumber(8))],
        ..Default::default()
    };
    assert!(note(&closed, ChangeNumber(8)).is_some_and(|(t, _)| t.contains("closed on Gerrit")));
    assert_eq!(note(&closed, GERRIT), None);
}
