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
use sync::{ConsolidateReport, FoundChange, Ported, PostOutcome, PostReport};

use super::{Consolidator, Request, affected, note, options};
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

    /// Asks for a run at once (as a saved review does), and waits.
    fn trigger(&self, cx: &mut TestAppContext) {
        self.ask(cx, Request::Now);
    }

    fn ask(&self, cx: &mut TestAppContext, kind: Request) {
        self.shell
            .update(cx, |s, cx| s.consolidate_for_test("test", kind, cx));
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
    assert!(options.port.is_some());
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

#[gpui_kit::test]
fn an_edit_of_a_draft_being_sent_is_kept_and_said_on_its_page_only(cx: &mut TestAppContext) {
    use crate::{diff::DraftRequest, outbox, session::ServerKey};

    let f = fixture();
    let s = start(cx, &f, Some("u"));
    let syncer = cx.update(|cx| {
        s.shell
            .read(cx)
            .session_for_test("test")
            .map(|s| s.syncer().clone())
            .expect("session")
    });
    let (_, id) = outbox::tests::claimed_draft(&s.store, &syncer);
    let change = outbox::tests::CHANGE;
    s.open(cx, Route::Change(change));
    let outbox = cx
        .update(|cx| s.shell.read(cx).outbox_of_for_test(&ServerKey::new("test")))
        .expect("outbox");
    let edit = |cx: &mut TestAppContext, text: &str| {
        let request = DraftRequest::Update {
            id,
            message: text.into(),
            unresolved: true,
        };
        outbox.update(cx, |o, cx| o.request(change, request, None, cx));
    };
    edit(cx, "on the page");
    cx.run_until_parked();
    assert_eq!(
        s.page_hint(cx),
        Some((outbox::KEPT_AS_NEW.to_owned(), false))
    );

    // The user moves on before the store answers: the text is kept all
    // the same, and nothing is said on the page they went to.
    edit(cx, "after moving on");
    s.open(cx, Route::Change(ChangeNumber(8)));
    assert_eq!(s.page_hint(cx), None);
    let mut kept: Vec<String> = syncer
        .cached_comments(change)
        .expect("cache")
        .drafts
        .iter()
        .filter(|d| d.is_ordinary())
        .map(|d| d.message.clone())
        .collect();
    kept.sort();
    assert_eq!(kept, ["after moving on", "on the page"]);
}

#[test]
fn a_local_change_without_a_patch_set_keeps_its_newest_version() {
    use store::{ConsolidationRewrite, LocalChangeUpsert, LocalOp, NewLocalPatchSet, PatchSetMove};

    let mut store = Store::open_in_memory().expect("store");
    let server = store.server_id(URL).expect("server");
    let now: Timestamp = "2024-06-15T10:00:00Z".parse().expect("time");
    let version = |sha: &str| NewLocalPatchSet {
        sha: sha.into(),
        parent_sha: None,
        seen_at: now,
    };
    let upsert = LocalChangeUpsert {
        seen: None,
        branch: Some("main".into()),
        change_id: ID_A.into(),
        gerrit_number: None,
        subject: "s".into(),
        author_name: "Me".into(),
        author_email: TEST_EMAIL.into(),
        updated: now,
        state: None,
        patch_sets: vec![version("l1"), version("l2")],
    };
    store
        .reconcile_local(server, "p", &[LocalOp::Upsert(upsert)], now)
        .expect("reconcile");
    let row = store
        .local_changes_by_change_id(server, ID_A)
        .expect("rows")
        .remove(0);
    let versions = store.local_patch_sets(server, row.id).expect("versions");
    // `l1` was pushed and found as patch set 1 of change 42; `l2` is the
    // amended commit, not pushed yet.
    store
        .rewrite_local_change(
            server,
            &ConsolidationRewrite {
                local_change: row.id,
                gerrit_number: GERRIT,
                moves: vec![PatchSetMove {
                    from: versions[0].number,
                    to: model::PatchSetRef {
                        number: PatchSetNumber(1),
                        sha: "l1".into(),
                    },
                    exact: true,
                    drafts: Vec::new(),
                    reviews: Vec::new(),
                }],
                keep: Vec::new(),
                review_states: Vec::new(),
                state: None,
                else_state: None,
            },
        )
        .expect("rewrite")
        .expect("rewritten");
    let env = env_with(store, &RouteHttpClient::new());
    let session = crate::session::Session::new(&env, &setup(), None).expect("session");
    let at = |target| {
        Some(ChangeView {
            target: Some(target),
            ..Default::default()
        })
    };
    // No patch set on the local change is its newest version: it stays
    // the target on the Gerrit change, not Gerrit's current patch set.
    assert_eq!(
        super::redirect(session.syncer(), &Route::Change(row.number), None).expect("redirect"),
        Some(super::Redirect {
            route: Route::Change(GERRIT),
            view: at(versions[1].number),
        })
    );
    // The pushed version, named, leads to its patch set.
    assert_eq!(
        super::redirect(
            session.syncer(),
            &Route::Change(row.number),
            at(versions[0].number).as_ref()
        )
        .expect("redirect")
        .and_then(|r| r.view),
        at(PatchSetNumber(1))
    );
}

/// The open page of the local change, with the change pushed and its
/// local review waiting.
fn on_the_local_page(cx: &mut TestAppContext) -> (Fixture, Started, LocalChange) {
    let f = fixture();
    let s = start(cx, &f, Some("u"));
    let (row, _) = s.row();
    s.review();
    s.open(cx, Route::Change(row.number));
    f.pushed();
    (f, s, row)
}

impl Started {
    fn press(&self, cx: &mut TestAppContext, key: &str) {
        use gpui_kit::test::TestWindowExt as _;
        cx.update_window(self.window, |_, window, cx| window.press(key, cx))
            .expect("press");
        cx.run_until_parked();
    }

    fn input(&self, cx: &mut TestAppContext, text: &str) {
        use gpui_kit::test::TestWindowExt as _;
        cx.update_window(self.window, |_, window, cx| window.input(text, cx))
            .expect("input");
        cx.run_until_parked();
    }

    fn route(&self, cx: &mut TestAppContext) -> Route {
        cx.update(|cx| self.shell.read(cx).route().clone())
    }
}

#[gpui_kit::test]
fn the_page_waits_for_an_open_editor_before_it_follows(cx: &mut TestAppContext) {
    let (f, s, row) = on_the_local_page(cx);
    // `c`: a comment on the local version, being written.
    s.press(cx, "c");
    s.input(cx, "half a thought");
    let editor_text = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            let page = s.shell.read(cx).change_page_for_test()?;
            let view = page.read(cx).ps_view_for_test()?;
            view.read(cx).editor_text(cx)
        })
    };
    assert_eq!(editor_text(cx).as_deref(), Some("half a thought"));
    s.trigger(cx);
    assert_eq!(f.http.posts().len(), 1, "the review was sent");
    // The page stays, with the editor and its text.
    assert_eq!(s.route(cx), Route::Change(row.number));
    assert_eq!(editor_text(cx).as_deref(), Some("half a thought"));

    // Once the editor is closed, the next run (or refresh) moves on.
    s.press(cx, "escape");
    s.trigger(cx);
    assert_eq!(s.route(cx), Route::Change(GERRIT));
}

#[gpui_kit::test]
fn the_page_waits_for_the_reply_dialog_before_it_follows(cx: &mut TestAppContext) {
    let (f, s, row) = on_the_local_page(cx);
    s.press(cx, "a");
    assert!(cx.update(|cx| s.shell.read(cx).reply_for_test().is_some()));
    s.trigger(cx);
    assert_eq!(f.http.posts().len(), 1);
    assert_eq!(s.route(cx), Route::Change(row.number));
    assert!(
        cx.update(|cx| s.shell.read(cx).reply_for_test().is_some()),
        "the dialog stays open"
    );
    // Closing the dialog lets the page follow.
    s.press(cx, "escape");
    assert!(cx.update(|cx| s.shell.read(cx).reply_for_test().is_none()));
    assert_eq!(s.route(cx), Route::Change(GERRIT));
}

#[gpui_kit::test]
fn the_note_stays_off_other_pages(cx: &mut TestAppContext) {
    let f = fixture();
    let s = start(cx, &f, Some("u"));
    s.review();
    // Another change's page.
    s.open(cx, Route::Change(ChangeNumber(8)));
    f.pushed();
    s.trigger(cx);
    assert_eq!(f.http.posts().len(), 1);
    assert_eq!(s.page_hint(cx), None);
    assert_eq!(s.route(cx), Route::Change(ChangeNumber(8)));
}

#[gpui_kit::test]
fn polls_ask_at_most_once_per_interval_and_do_not_queue(cx: &mut TestAppContext) {
    let f = fixture();
    let s = start(cx, &f, Some("u"));
    // The start's refreshes asked: one run, the others waited.
    let runs = s.runs(cx);
    assert!(runs >= 1);
    s.ask(cx, Request::Poll);
    s.ask(cx, Request::Poll);
    assert_eq!(s.runs(cx), runs, "within the poll interval");
    // A saved review does not wait.
    s.ask(cx, Request::Now);
    assert_eq!(s.runs(cx), runs + 1);

    // A poll during a run makes no second run; a saved review does.
    let review = s.review();
    f.pushed();
    f.http.hold_posts();
    s.ask(cx, Request::Now);
    assert_eq!(s.review_state(review.id), PendingReviewState::Sending);
    s.ask(cx, Request::Poll);
    f.http.release();
    cx.run_until_parked();
    assert_eq!(s.runs(cx), runs + 2);
}

#[test]
fn only_rows_whose_state_moved_count_as_affected() {
    let row = LocalChangeId(5);
    let number = row.number().expect("number");
    let looked_up = ConsolidateReport {
        ran: true,
        not_found: vec![row],
        ..Default::default()
    };
    assert!(affected(&looked_up).is_empty());
    let moved = ConsolidateReport {
        state_changed: vec![row],
        ..looked_up
    };
    assert_eq!(affected(&moved), [number]);
    // Ported (or held) comments: the Gerrit change and the row's pages.
    let ported = ConsolidateReport {
        ported: vec![ported(5, true)],
        ..Default::default()
    };
    assert_eq!(affected(&ported), [GERRIT, number]);
}

/// Local version 1 of row `row` ported onto PS 2 of `GERRIT` with a draft
/// held, or (`to` false) held whole.
fn ported(row: i64, to: bool) -> Ported {
    Ported {
        local_change: LocalChangeId(row),
        change: GERRIT,
        from: PatchSetNumber(1 << 24 | 1),
        to: to.then_some(PatchSetNumber(2)),
        drafts: 1,
        reviews: 0,
        held: vec![(model::LocalDraftId(9), "Line 3 changed.".into())],
        held_reviews: 0,
    }
}

#[test]
fn the_note_says_when_comments_did_not_fit() {
    let mut report = ConsolidateReport {
        ran: true,
        ported: vec![ported(3, true)],
        ..Default::default()
    };
    assert_eq!(
        note(&report, GERRIT),
        Some((
            "Some of your local comments did not fit PS 2 and are held".to_owned(),
            false
        ))
    );
    report.posts.push(post(3, GERRIT, true, PostOutcome::Sent));
    let (text, _) = note(&report, GERRIT).expect("note");
    assert!(
        text.starts_with("Your local review was sent to Gerrit; some comments"),
        "{text}"
    );
    report.ported = vec![ported(3, false)];
    let (text, _) = note(&report, GERRIT).expect("note");
    assert!(
        text.contains("No patch set of this change is in your checkout"),
        "{text}"
    );
    // Nothing held: only what was sent.
    report.ported[0].held.clear();
    assert_eq!(
        note(&report, GERRIT),
        Some(("Your local review was sent to Gerrit".to_owned(), false))
    );
}

#[gpui_kit::test]
fn signing_out_mid_run_stops_it_without_a_rerun(cx: &mut TestAppContext) {
    let f = fixture();
    let s = start(cx, &f, Some("u"));
    let first = s.review();
    let second = s.review();
    f.pushed();
    f.http.hold_posts();
    let runs = s.runs(cx);
    s.trigger(cx);
    cx.update_window(s.window, |_, window, cx| {
        s.shell
            .update(cx, |s, cx| s.unauthorized_for_test("test", window, cx))
    })
    .expect("window");
    f.http.release();
    cx.run_until_parked();
    // The post in flight went through; the run stopped before the next.
    assert_eq!(f.http.posts().len(), 1);
    assert_eq!(s.review_state(first.id), PendingReviewState::Sent);
    assert_eq!(s.review_state(second.id), PendingReviewState::Pending);
    assert_eq!(s.runs(cx), runs + 1, "no rerun");
    let c = s.consolidator(cx);
    assert!(!cx.update(|cx| c.read(cx).is_running()));
    // Signed out: nothing runs.
    c.update(cx, |c, cx| c.request(Request::Now, cx));
    assert_eq!(s.runs(cx), runs + 1);
}

#[gpui_kit::test]
fn another_user_mid_run_gets_one_run_of_their_own(cx: &mut TestAppContext) {
    let f = fixture();
    let s = start(cx, &f, Some("u"));
    let first = s.review();
    s.review();
    f.pushed();
    f.http.hold_posts();
    let runs = s.runs(cx);
    s.trigger(cx);
    let account_reads = || {
        f.http
            .routes
            .requested()
            .iter()
            .filter(|r| r.contains("/accounts/self"))
            .count()
    };
    let reads = account_reads();
    cx.update_window(s.window, |_, window, cx| {
        s.shell
            .update(cx, |s, cx| s.sign_in_for_test("test", "v", window, cx))
    })
    .expect("window");
    f.http.release();
    cx.run_until_parked();
    // The old run finished its post and stopped, and nothing runs for the
    // new user before their account is read ...
    assert_eq!(s.review_state(first.id), PendingReviewState::Sent);
    assert_eq!(s.runs(cx), runs + 1);
    s.trigger(cx);
    assert_eq!(s.runs(cx), runs + 1);
    // ... which the next refresh does; then exactly one run follows.
    s.press(cx, "shift-r");
    assert!(account_reads() > reads, "the new user's account was read");
    assert_eq!(s.runs(cx), runs + 2);
    let c = s.consolidator(cx);
    assert!(!cx.update(|cx| c.read(cx).is_running()));
}

#[gpui_kit::test]
fn a_refused_login_mid_run_signs_the_server_out(cx: &mut TestAppContext) {
    let f = fixture();
    let s = start(cx, &f, Some("u"));
    let review = s.review();
    f.pushed();
    f.http
        .routes
        .route("POST /changes/42/revisions/", 401, "Unauthorized");
    let runs = s.runs(cx);
    s.trigger(cx);
    assert_eq!(s.runs(cx), runs + 1);
    assert_eq!(
        cx.update(|cx| s.shell.read(cx).connection_for_test("test")),
        Some(Connection::Unauthorized)
    );
    // Gerrit did not take it: the review waits for the next login.
    assert_eq!(s.review_state(review.id), PendingReviewState::Pending);
    let c = s.consolidator(cx);
    c.update(cx, |c, cx| c.request(Request::Now, cx));
    assert_eq!(s.runs(cx), runs + 1, "no session, no run");
}

#[gpui_kit::test]
fn the_note_stays_off_another_servers_page(cx: &mut TestAppContext) {
    use crate::session::ServerKey;

    let f = fixture();
    let mut store = Store::open_in_memory().expect("store");
    assert_eq!(store.server_id(URL).expect("server id"), SERVER);
    let other_url = "https://other.example.com";
    let other_id = store.server_id(other_url).expect("server id");
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
        servers: vec![
            ServerSetup {
                username: Some("u".into()),
                queries: vec![SavedQuery {
                    name: "Open".into(),
                    query: "status:open".into(),
                }],
                projects: vec![f.project()],
                ..setup()
            },
            ServerSetup {
                key: ServerKey::new("other"),
                name: "Other".into(),
                base_url: other_url.into(),
                server: other_id,
                ..setup()
            },
        ],
        active: None,
    };
    let (window, shell) = open(cx, |window, cx| {
        cx.new(|cx| Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    let s = Started {
        window,
        shell,
        store,
    };
    s.review();
    // Change 42 of the other server is shown.
    cx.update_window(window, |_, window, cx| {
        s.shell.update(cx, |s, cx| {
            s.go_to(
                &ServerKey::new("other"),
                Route::Change(GERRIT),
                None,
                window,
                cx,
            )
        })
    })
    .expect("window");
    cx.run_until_parked();
    f.pushed();
    s.trigger(cx);
    assert_eq!(f.http.posts().len(), 1);
    assert_eq!(s.page_hint(cx), None);
    assert_eq!(s.route(cx), Route::Change(GERRIT));
    assert_eq!(
        cx.update(|cx| s.shell.read(cx).active_for_test().cloned()),
        Some(ServerKey::new("other"))
    );
}

#[gpui_kit::test]
fn comments_on_a_version_amended_before_upload_are_ported_through_the_checkout(
    cx: &mut TestAppContext,
) {
    use model::{DraftInput, Location, PatchSetRef, Side};
    use sync::DraftSave;

    let mut f = fixture();
    // The user's commit has a file.
    f.repo.write("a.txt", b"1\n2\n3\n4\n5\n");
    f.repo.git(&["add", "-A"]);
    f.repo.git(&["commit", "-q", "--amend", "--no-edit"]);
    let s = start(cx, &f, Some("u"));
    let (row, amended) = s.row();
    let syncer = cx.update(|cx| {
        s.shell
            .read(cx)
            .session_for_test("test")
            .map(|s| s.syncer().clone())
            .expect("session")
    });
    let draft = |line: u32, message: &str| {
        syncer
            .save_draft_local(DraftSave::Create {
                change: row.number,
                patch_set: PatchSetRef {
                    number: amended.number,
                    sha: amended.sha.clone(),
                },
                input: DraftInput {
                    path: "a.txt".into(),
                    side: Side::Revision,
                    parent: None,
                    location: Location::Line(line),
                    in_reply_to: None,
                    message: message.into(),
                    unresolved: true,
                },
            })
            .expect("store")
            .expect("draft")
    };
    let kept = draft(4, "on an unchanged line");
    let changed = draft(2, "on a line that changes");

    // Amended again (a line on top, line 2 changed), seen by the scanner,
    // and pushed.
    f.repo.write("a.txt", b"0\n1\nTWO\n3\n4\n5\n");
    f.repo.git(&["add", "-A"]);
    f.repo.git(&["commit", "-q", "--amend", "--no-edit"]);
    f.sha = f.repo.git(&["rev-parse", "HEAD"]);
    let scanner = cx
        .update(|cx| s.shell.read(cx).local_scan_for_test("test"))
        .expect("a scanner");
    let scanned = scanner.update(cx, |sc, cx| sc.scan_now("p", cx));
    cx.run_until_parked();
    scanned.now_or_never().expect("answered").expect("scanned");
    f.pushed();
    f.http.routes.route_json(
        &format!("/revisions/{}/files", f.sha),
        &json!({"/COMMIT_MSG": {"status": "A"}, "a.txt": {"lines_inserted": 2}}).to_string(),
    );
    s.trigger(cx);

    let posts = f.http.posts();
    assert_eq!(posts.len(), 1, "{posts:?}");
    let (target, body) = &posts[0];
    assert!(
        target.contains(&format!("/changes/42/revisions/{}/review", f.sha)),
        "{target}"
    );
    assert_eq!(
        body["comments"]["a.txt"],
        json!([{"side": "REVISION", "line": 5, "message": "on an unchanged line",
                "unresolved": true}])
    );
    let store = s.store.lock().expect("store");
    assert_eq!(store.load_draft(SERVER, kept.local_id).expect("load"), None);
    let held = store
        .load_draft(SERVER, changed.local_id)
        .expect("load")
        .expect("held");
    assert!(
        held.held_reason
            .as_deref()
            .is_some_and(|r| r.contains("Line 2 of a.txt changed in PS 1")),
        "{held:?}"
    );
    drop(store);
    assert_no_local_number_sent(&f);
    for r in f.http.routes.requested() {
        assert!(!r.contains(amended.sha.as_str()), "{r}");
    }
}

#[gpui_kit::test]
fn comments_on_an_amend_the_branch_was_reset_from_are_ported(cx: &mut TestAppContext) {
    use model::{DraftInput, Location, PatchSetRef, Side};
    use sync::DraftSave;

    let mut f = fixture();
    f.repo.write("a.txt", b"1\n2\n3\n4\n5\n");
    f.repo.git(&["add", "-A"]);
    f.repo.git(&["commit", "-q", "--amend", "--no-edit"]);
    let pushed = f.repo.git(&["rev-parse", "HEAD"]);
    let s = start(cx, &f, Some("u"));
    let scanner = cx
        .update(|cx| s.shell.read(cx).local_scan_for_test("test"))
        .expect("a scanner");
    let scan = |cx: &mut TestAppContext| {
        let task = scanner.update(cx, |sc, cx| sc.scan_now("p", cx));
        cx.run_until_parked();
        task.now_or_never().expect("answered").expect("scanned");
    };

    // Amended (a line on top, line 2 changed), seen, and commented on.
    f.repo.write("a.txt", b"0\n1\nTWO\n3\n4\n5\n");
    f.repo.git(&["add", "-A"]);
    f.repo.git(&["commit", "-q", "--amend", "--no-edit"]);
    let amend = f.repo.git(&["rev-parse", "HEAD"]);
    scan(cx);
    let (row, _) = s.row();
    let versions = s
        .store
        .lock()
        .expect("store")
        .local_patch_sets(SERVER, row.id)
        .expect("versions");
    let amended = versions
        .iter()
        .find(|v| v.sha.as_str() == amend)
        .expect("the amend is a version")
        .clone();
    let syncer = cx.update(|cx| {
        s.shell
            .read(cx)
            .session_for_test("test")
            .map(|s| s.syncer().clone())
            .expect("session")
    });
    let draft = |line: u32, message: &str| {
        syncer
            .save_draft_local(DraftSave::Create {
                change: row.number,
                patch_set: PatchSetRef {
                    number: amended.number,
                    sha: amended.sha.clone(),
                },
                input: DraftInput {
                    path: "a.txt".into(),
                    side: Side::Revision,
                    parent: None,
                    location: Location::Line(line),
                    in_reply_to: None,
                    message: message.into(),
                    unresolved: true,
                },
            })
            .expect("store")
            .expect("draft")
    };
    let kept = draft(5, "on an unchanged line");
    let changed = draft(3, "on a changed line");

    // The user resets the branch to the first version and pushes that: the
    // amend is on no branch any more and will never reach Gerrit.
    f.repo.git(&["reset", "-q", "--hard", &pushed]);
    scan(cx);
    f.sha = pushed;
    f.pushed();
    f.http.routes.route_json(
        &format!("/revisions/{}/files", f.sha),
        &json!({"/COMMIT_MSG": {"status": "A"}, "a.txt": {"status": "A"}}).to_string(),
    );
    s.trigger(cx);

    let posts = f.http.posts();
    assert_eq!(posts.len(), 1, "{posts:?}");
    let (target, body) = &posts[0];
    assert!(
        target.contains(&format!("/changes/42/revisions/{}/review", f.sha)),
        "{target}"
    );
    assert_eq!(
        body["comments"]["a.txt"],
        json!([{"side": "REVISION", "line": 4, "message": "on an unchanged line",
                "unresolved": true}])
    );
    let store = s.store.lock().expect("store");
    assert_eq!(store.load_draft(SERVER, kept.local_id).expect("load"), None);
    let held = store
        .load_draft(SERVER, changed.local_id)
        .expect("load")
        .expect("held");
    assert!(
        held.held_reason
            .as_deref()
            .is_some_and(|r| r.contains("Line 3 of a.txt changed in PS 1")),
        "{held:?}"
    );
    drop(store);
    assert_no_local_number_sent(&f);
    for r in f.http.routes.requested() {
        assert!(!r.contains(&amend), "{r}");
    }
}
