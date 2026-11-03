use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use futures::{
    executor::block_on,
    future::{BoxFuture, FutureExt},
};
use gerrit_api::{Auth, GerritClient, HttpClient, HttpError, fake::RouteHttpClient};
use model::{
    DraftInput, LocalPatchSet, Location, PATCHSET_LEVEL_PATH, PatchSetRef, PendingReviewState, Side,
};
use serde_json::{Value, json};
use store::{LocalChangeUpsert, LocalOp, NewLocalPatchSet, NewPendingReview, Store};

use jiff::SignedDuration;

use super::*;
use crate::DraftSave;

const BASE: &str = "https://g.example";
const ME: u64 = 1000;
const CID: &str = "I0123456789abcdef0123456789abcdef01234567";
const CHANGE: ChangeNumber = ChangeNumber(7);

/// A needle and the replies queued for it (status and body).
type Queued = (String, VecDeque<(u16, String)>);

/// Path-routed replies (a [`RouteHttpClient`], which records every request
/// and panics on a local number), plus queued replies per needle that win
/// while they last: for a refusal followed by success, or a lost
/// connection. Status 0 is a transport error, 1 no answer at all.
#[derive(Clone, Default)]
struct Http {
    routes: RouteHttpClient,
    queued: Arc<Mutex<Vec<Queued>>>,
    /// Answers built from the bodies of the posts so far, for one needle.
    dynamic: Arc<Mutex<Option<Dynamic>>>,
    /// Every answer waits for one turn of the executor first, so that
    /// two runs joined on one thread interleave.
    yielding: Arc<std::sync::atomic::AtomicBool>,
}

/// Pending once, then ready: lets other futures of a `join` run.
struct YieldOnce(bool);

impl std::future::Future for YieldOnce {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.0 {
            std::task::Poll::Ready(())
        } else {
            self.0 = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    }
}

/// A needle and how to answer it from the posts' bodies so far.
type Dynamic = (String, Box<dyn Fn(&[Value]) -> Value + Send>);

impl Http {
    fn queue(&self, needle: &str, status: u16, body: &str) {
        let mut q = self.queued.lock().unwrap();
        match q.iter_mut().find(|(n, _)| n == needle) {
            Some((_, list)) => list.push_back((status, body.to_owned())),
            None => q.push((
                needle.to_owned(),
                VecDeque::from([(status, body.to_owned())]),
            )),
        }
    }

    /// Answers requests matching `needle` with `answer(posts so far)`.
    fn respond(&self, needle: &str, answer: impl Fn(&[Value]) -> Value + Send + 'static) {
        *self.dynamic.lock().unwrap() = Some((needle.to_owned(), Box::new(answer)));
    }

    fn posts(&self) -> Vec<Value> {
        self.routes
            .sent()
            .iter()
            .filter(|r| r.method == "POST")
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    fn post_targets(&self) -> Vec<String> {
        self.routes
            .sent()
            .iter()
            .filter(|r| r.method == "POST")
            .map(|r| r.target.clone())
            .collect()
    }

    /// Every request went to the server's own numbers and commits: no
    /// local SHA (`local`) and no synthetic number of `store` in a target
    /// or a body.
    fn assert_nothing_local(&self, store: &Store, server: model::ServerId, local: &[&str]) {
        let mut numbers: Vec<String> = Vec::new();
        for c in store.local_changes(server).unwrap() {
            numbers.push(c.number.0.to_string());
            for v in store.local_patch_sets(server, c.id).unwrap() {
                numbers.push(v.number.0.to_string());
            }
        }
        assert!(!numbers.is_empty());
        for r in self.routes.sent() {
            let text = format!("{} {}", r.target, r.body_text());
            for l in local {
                assert!(!text.contains(l), "{l} was sent: {text}");
            }
            for word in text.split(|c: char| !c.is_ascii_digit()) {
                assert!(
                    !numbers.iter().any(|n| n == word),
                    "a local number was sent: {text}"
                );
            }
        }
    }
}

impl HttpClient for Http {
    fn send(
        &self,
        req: http::Request<Vec<u8>>,
    ) -> BoxFuture<'static, Result<http::Response<Vec<u8>>, HttpError>> {
        let method = req.method().as_str().to_owned();
        let target = req
            .uri()
            .path_and_query()
            .map_or_else(String::new, |p| p.as_str().to_owned());
        let routed = self.routes.send(req);
        let over = self
            .queued
            .lock()
            .unwrap()
            .iter_mut()
            .find(|(needle, list)| {
                let (m, path) = needle.split_once(' ').unwrap_or(("", needle));
                (m.is_empty() || m == method) && target.contains(path) && !list.is_empty()
            })
            .and_then(|(_, list)| list.pop_front());
        let over = over.or_else(|| {
            let dynamic = self.dynamic.lock().unwrap();
            let (needle, answer) = dynamic.as_ref()?;
            target
                .contains(needle.as_str())
                .then(|| (200, format!(")]}}'\n{}", answer(&self.posts()))))
        });
        let yielding = self.yielding.load(std::sync::atomic::Ordering::SeqCst);
        async move {
            if yielding {
                YieldOnce(false).await;
            }
            let routed = routed.await;
            match over {
                Some((0, why)) => Err(HttpError(why)),
                // A server that never answers.
                Some((1, _)) => futures::future::pending().await,
                Some((status, body)) => Ok(http::Response::builder()
                    .status(status)
                    .body(body.into_bytes())
                    .unwrap()),
                None => routed,
            }
        }
        .boxed()
    }
}

struct Fx {
    http: Http,
    syncer: Syncer,
}

/// Recovers the claims of a run that ended at once: in these tests no
/// other run can be going on unless a test makes one.
fn quick() -> ConsolidationTiming {
    ConsolidationTiming {
        claim_stale: SignedDuration::from_secs(-1),
        ..ConsolidationTiming::default()
    }
}

fn fx() -> Fx {
    fx_with(quick())
}

fn fx_with(timing: ConsolidationTiming) -> Fx {
    let mut store = Store::open_in_memory().unwrap();
    let server = store.server_id(BASE).unwrap();
    store
        .set_account(server, AccountId(ME), Some("me"), Some("me@example.com"))
        .unwrap();
    fx_on(Arc::new(Mutex::new(store)), server, timing)
}

/// Asks about changes with nothing to send on every run, too.
fn eager() -> ConsolidationTiming {
    ConsolidationTiming {
        slow_recheck: SignedDuration::from_secs(-1),
        ..quick()
    }
}

/// A syncer on `store` with a fake server of its own.
fn fx_on(store: Arc<Mutex<Store>>, server: model::ServerId, timing: ConsolidationTiming) -> Fx {
    let http = Http::default();
    let client = GerritClient::new(
        Arc::new(http.clone()),
        BASE,
        Auth::Basic {
            username: "me".into(),
            password: "p".into(),
        },
    )
    .unwrap();
    Fx {
        http,
        syncer: Syncer::new(client, store, server).with_consolidation_timing(timing),
    }
}

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

impl Fx {
    /// A local change of project `p` with versions `shas`.
    fn local(
        &self,
        change_id: &str,
        branch: Option<&str>,
        gerrit: Option<ChangeNumber>,
        shas: &[&str],
    ) -> (LocalChange, Vec<LocalPatchSet>) {
        let mut store = self.syncer.store();
        let upsert = LocalChangeUpsert {
            seen: None,
            branch: branch.map(Into::into),
            change_id: change_id.into(),
            gerrit_number: gerrit,
            subject: "local".into(),
            author_name: "Me".into(),
            author_email: "me@example.com".into(),
            updated: ts("2024-01-01T00:00:00Z"),
            state: None,
            patch_sets: shas
                .iter()
                .map(|s| NewLocalPatchSet {
                    sha: (*s).into(),
                    parent_sha: None,
                    seen_at: ts("2024-01-01T00:00:00Z"),
                })
                .collect(),
        };
        store
            .reconcile_local(
                self.syncer.server,
                "p",
                &[LocalOp::Upsert(upsert)],
                ts("2024-01-01T00:00:00Z"),
            )
            .unwrap();
        let row = store
            .local_changes_by_change_id(self.syncer.server, change_id)
            .unwrap()
            .into_iter()
            .find(|r| r.branch.as_deref() == branch)
            .unwrap();
        let versions = store.local_patch_sets(self.syncer.server, row.id).unwrap();
        (row, versions)
    }

    /// A local change with one version `sha` that vanished from the
    /// checkout (it is kept only while it has unsent data, so give it some).
    fn vanished(&self, change_id: &str, sha: &str) -> (LocalChange, Vec<LocalPatchSet>) {
        let (row, v) = self.local(change_id, Some("main"), None, &[sha]);
        self.syncer
            .store()
            .set_local_change_state(self.syncer.server, row.id, LocalChangeState::Vanished)
            .unwrap();
        (self.row(&row), v)
    }

    fn draft(&self, row: &LocalChange, v: &LocalPatchSet, message: &str) -> Draft {
        self.syncer
            .save_draft_local(DraftSave::Create {
                change: row.gerrit_number.unwrap_or(row.number),
                patch_set: PatchSetRef {
                    number: v.number,
                    sha: v.sha.clone(),
                },
                input: DraftInput {
                    path: "src/a.rs".into(),
                    side: Side::Revision,
                    parent: None,
                    location: Location::Line(7),
                    in_reply_to: None,
                    message: message.into(),
                    unresolved: true,
                },
            })
            .unwrap()
            .unwrap()
    }

    fn review(&self, v: &LocalPatchSet, labels: &[(&str, i32)], message: &str) -> PendingReview {
        self.review_with(v, labels, message, LocalApproval::Exact, Timestamp::now())
    }

    fn review_with(
        &self,
        v: &LocalPatchSet,
        labels: &[(&str, i32)],
        message: &str,
        strategy: LocalApproval,
        created: Timestamp,
    ) -> PendingReview {
        self.syncer
            .create_pending_review(&NewPendingReview {
                patch_set: v.number,
                labels: labels.iter().map(|(l, v)| ((*l).to_owned(), *v)).collect(),
                strategy,
                message: message.into(),
                unresolved: false,
                created,
                replaces_draft: None,
            })
            .unwrap()
    }

    fn assert_nothing_local(&self, local: &[&str]) {
        let store = self.syncer.store();
        self.http
            .assert_nothing_local(&store, self.syncer.server, local);
    }

    fn row(&self, row: &LocalChange) -> LocalChange {
        self.syncer
            .store()
            .local_change(self.syncer.server, row.id)
            .unwrap()
            .unwrap()
    }

    fn reloaded(&self, r: &PendingReview) -> PendingReview {
        self.syncer
            .store()
            .load_pending_review(self.syncer.server, r.id)
            .unwrap()
            .unwrap()
    }

    fn load_draft(&self, d: &Draft) -> Option<Draft> {
        self.syncer
            .store()
            .load_draft(self.syncer.server, d.local_id)
            .unwrap()
    }

    fn run(&self) -> ConsolidateReport {
        self.run_with(&ConsolidateOptions::default())
    }

    fn run_with(&self, options: &ConsolidateOptions) -> ConsolidateReport {
        block_on(self.syncer.consolidate(options))
    }

    /// Gerrit's change 7 with `status`, patch sets `shas` (1, 2, ...), the
    /// last one current, and `messages`.
    fn gerrit(&self, status: &str, shas: &[&str], messages: Value) {
        self.gerrit_json(change_json(status, shas, messages));
    }

    /// Change 7 as `change`: its detail, and in the batch query of known
    /// changes.
    fn gerrit_json(&self, change: Value) {
        self.http
            .routes
            .route_json("/changes/7?", &change.to_string());
        self.http
            .routes
            .route_json("/changes/?q=change%3A", &json!([change]).to_string());
    }

    /// The requests for the detail of change 7.
    fn detail_fetches(&self) -> usize {
        self.http
            .routes
            .requested()
            .iter()
            .filter(|r| r.contains("/changes/7?"))
            .count()
    }

    fn query_finds(&self, hits: &[(u64, &str, &str)]) {
        let list: Vec<Value> = hits
            .iter()
            .map(|(n, branch, status)| summary_json(*n, branch, status))
            .collect();
        self.http
            .routes
            .route_json("/changes/?q=%28", &Value::Array(list).to_string());
    }

    /// Every post is answered with `labels` applied and patch set
    /// `current` current.
    fn posts_apply(&self, labels: Value, current: u32) {
        self.http.routes.route_json(
            "POST /changes/7/revisions/",
            &json!({"labels": labels, "change_info": {"current_revision_number": current}})
                .to_string(),
        );
    }
}

fn summary_json(n: u64, branch: &str, status: &str) -> Value {
    json!({
        "_number": n, "project": "p", "branch": branch, "change_id": CID, "subject": "s",
        "status": status, "created": "2024-01-01 00:00:00.000000000",
        "updated": "2024-01-02 00:00:00.000000000", "owner": {"_account_id": ME},
    })
}

fn change_json(status: &str, shas: &[&str], messages: Value) -> Value {
    let revisions: serde_json::Map<String, Value> = shas
        .iter()
        .zip(1..)
        .map(|(sha, n)| ((*sha).to_owned(), json!({"_number": n})))
        .collect();
    let mut c = summary_json(7, "main", status);
    let fields = json!({
        "mergeable": true,
        "current_revision": shas.last(),
        "current_revision_number": shas.len(),
        "revisions": revisions,
        "labels": {
            "Code-Review": {"values": {"-2": "", "-1": "", " 0": "", "+1": "", "+2": ""},
                            "all": [{"_account_id": ME, "value": 0}]},
            "Verified": {"values": {"-1": "", " 0": "", "+1": ""}},
        },
        "messages": messages,
    });
    for (k, v) in fields.as_object().unwrap() {
        c[k] = v.clone();
    }
    c
}

/// The tag of the first post, and its nonce part.
fn first_tag(f: &Fx) -> String {
    f.http.posts()[0]["tag"].as_str().unwrap().to_owned()
}

// ---- lookup ----

#[test]
fn not_found_makes_checking_local_and_deletes_a_vanished_change() {
    let f = fx();
    let (checking, _) = f.local(CID, Some("main"), None, &["l1"]);
    let other = "Iffffffffffffffffffffffffffffffffffffffff";
    let (vanished, v) = f.vanished(other, "l2");
    let r = f.review(&v[0], &[("Code-Review", 1)], "msg");
    f.query_finds(&[]);
    let report = f.run();
    assert!(report.ran && report.stopped.is_none(), "{report:?}");
    assert_eq!(report.not_found.len(), 2);
    assert_eq!(f.row(&checking).state, LocalChangeState::Local);
    // O1: gone from the checkout, and Gerrit does not have it: deleted
    // with its local data.
    assert!(
        f.syncer
            .store()
            .local_change(f.syncer.server, vanished.id)
            .unwrap()
            .is_none()
    );
    assert!(
        f.syncer
            .store()
            .load_pending_review(f.syncer.server, r.id)
            .unwrap()
            .is_none()
    );
    assert_eq!(report.state_changed, [vanished.id, checking.id]);
    // One query for both, by triplet, in the project.
    let sent = f.http.routes.requested();
    assert_eq!(sent.len(), 1, "{sent:?}");
    let q = sent[0]
        .replace("%22", "\"")
        .replace("%7E", "~")
        .replace("%2F", "/");
    let q = q.replace('+', " ").replace("%20", " ").replace("%3A", ":");
    assert!(q.contains(&format!("change:\"p~main~{CID}\"")), "{q}");
    assert!(q.contains(&format!("change:\"p~main~{other}\"")), "{q}");
    assert!(q.contains(" OR ") && q.contains("project:\"p\""), "{q}");
    // Looked up again: nothing changes state this time.
    let again = f.run();
    assert_eq!(again.not_found.len(), 1);
    assert!(again.state_changed.is_empty(), "{again:?}");
}

#[test]
fn a_vanished_change_stays_unless_gerrit_definitely_does_not_have_it() {
    // A failed query.
    let f = fx();
    let (row, v) = f.vanished(CID, "l1");
    let d = f.draft(&row, &v[0], "keep me");
    f.http.queue("/changes/?q=", 400, "Invalid change format");
    let report = f.run();
    assert!(
        report.not_found.is_empty() && report.errors.len() == 1,
        "{report:?}"
    );
    assert_eq!(f.row(&row).state, LocalChangeState::Vanished);
    // A lost connection.
    f.http.queue("/changes/?q=", 0, "connection reset");
    let report = f.run();
    assert!(report.stopped.is_some(), "{report:?}");
    assert_eq!(f.row(&row).state, LocalChangeState::Vanished);
    // A cut-off answer.
    let mut hit = summary_json(9, "other", "NEW");
    hit["_more_changes"] = json!(true);
    f.http
        .routes
        .route_json("/changes/?q=", &json!([hit]).to_string());
    let report = f.run();
    assert!(report.not_found.is_empty(), "{report:?}");
    assert_eq!(f.row(&row).state, LocalChangeState::Vanished);
    assert!(f.load_draft(&d).is_some());

    // A Change-Id Gerrit cannot be asked about: never deleted.
    let f = fx();
    let (row, v) = f.vanished("Ixyz", "l1");
    f.draft(&row, &v[0], "keep me");
    let report = f.run();
    assert!(report.not_found.is_empty(), "{report:?}");
    assert_eq!(f.row(&row).state, LocalChangeState::Vanished);
    assert!(f.http.routes.requested().is_empty());
}

#[test]
fn a_vanished_change_that_gerrit_has_is_consolidated() {
    let f = fx();
    let (row, v) = f.vanished(CID, "s1");
    let d = f.draft(&row, &v[0], "a comment");
    f.query_finds(&[(7, "main", "NEW")]);
    f.gerrit("NEW", &["s1"], json!([]));
    f.posts_apply(json!({}), 1);
    let report = f.run();
    assert_eq!(report.found.len(), 1, "{report:?}");
    assert_eq!(f.load_draft(&d), None);
    assert_eq!(f.http.posts().len(), 1);
    assert_eq!(f.row(&row).state, LocalChangeState::Consolidated);
}

#[test]
fn lookups_go_in_batches_of_fifty() {
    let f = fx();
    for i in 0..51 {
        f.local(
            &format!("I{i:040x}"),
            Some("main"),
            None,
            &[&format!("s{i}")],
        );
    }
    f.query_finds(&[]);
    let report = f.run();
    assert_eq!(report.not_found.len(), 51);
    assert_eq!(f.http.routes.requested().len(), 2);
}

#[test]
fn several_hits_without_a_branch_are_ambiguous() {
    let f = fx();
    let (row, v) = f.local(CID, None, None, &["l1"]);
    f.review(&v[0], &[("Code-Review", 2)], "LGTM");
    f.query_finds(&[(7, "main", "NEW"), (8, "stable", "NEW")]);
    let report = f.run();
    assert_eq!(report.ambiguous, [row.id]);
    assert_eq!(report.state_changed, [row.id]);
    let row = f.row(&row);
    assert_eq!(
        (row.state, row.gerrit_number),
        (LocalChangeState::Ambiguous, None)
    );
    // Not consolidated: one query, nothing else.
    assert_eq!(f.http.routes.requested().len(), 1);
    assert!(f.http.posts().is_empty());
}

#[test]
fn a_known_branch_takes_only_its_own_hit() {
    let f = fx();
    let (row, _) = f.local(CID, Some("main"), None, &["l1"]);
    f.query_finds(&[(8, "stable", "NEW")]);
    let report = f.run();
    assert_eq!(report.not_found, [row.id]);
}

#[test]
fn a_cut_off_answer_decides_nothing() {
    let f = fx();
    let (row, _) = f.local(CID, Some("main"), None, &["l1"]);
    let mut hit = summary_json(9, "other", "NEW");
    hit["_more_changes"] = json!(true);
    f.http
        .routes
        .route_json("/changes/?q=", &json!([hit]).to_string());
    let report = f.run();
    assert!(report.not_found.is_empty() && report.found.is_empty());
    assert_eq!(f.row(&row).state, LocalChangeState::Checking);
}

#[test]
fn a_malformed_change_id_is_not_asked_about() {
    let f = fx();
    let (row, _) = f.local("Ixyz", Some("main"), None, &["l1"]);
    let report = f.run();
    assert!(report.not_found.is_empty());
    // Gerrit refuses a whole query naming it, and would not have taken it
    // in a push: local at once, not "checking with Gerrit" forever.
    assert_eq!(f.row(&row).state, LocalChangeState::Local);
    assert!(f.http.routes.requested().is_empty());
}

// ---- closed ----

#[test]
fn a_closed_change_holds_everything_also_after_a_restore() {
    let f = fx_with(eager());
    let (row, v) = f.local(CID, Some("main"), None, &["s1"]);
    let d = f.draft(&row, &v[0], "a comment");
    let r = f.review(&v[0], &[("Code-Review", 2)], "LGTM");
    f.query_finds(&[(7, "main", "MERGED")]);
    f.gerrit("MERGED", &["s1"], json!([]));
    let report = f.run();
    assert_eq!(report.closed, [(row.id, CHANGE)]);
    assert_eq!(report.found.len(), 1);
    let now = f.row(&row);
    assert_eq!(
        (now.state, now.gerrit_number),
        (LocalChangeState::Closed, Some(CHANGE))
    );
    assert!(f.http.posts().is_empty());
    // Held: the review and the draft, with the reason, for the held card.
    let r = f.reloaded(&r);
    assert_eq!(
        (r.state, r.held_reason.as_deref()),
        (PendingReviewState::Held, Some("change merged"))
    );
    let held = f.load_draft(&d).unwrap();
    assert_eq!(
        (held.change, held.patch_set, held.held_reason.as_deref()),
        (CHANGE, v[0].number, Some("change merged"))
    );
    let items = f
        .syncer
        .store()
        .held_items(f.syncer.server, CHANGE)
        .unwrap();
    assert_eq!((items.drafts.len(), items.reviews.len()), (1, 1));

    // Asked again: still closed, nothing new, no detail fetched.
    let fetches = f.detail_fetches();
    let report = f.run();
    assert!(report.closed.is_empty() && f.http.posts().is_empty());
    assert_eq!(f.row(&row).state, LocalChangeState::Closed);
    assert_eq!(f.detail_fetches(), fetches);

    // Restored on Gerrit: the change is open again, but what was held
    // stays held; the user publishes it from the held card.
    f.gerrit("NEW", &["s1"], json!([]));
    f.posts_apply(json!({"Code-Review": 2}), 1);
    let report = f.run();
    assert_eq!(report.reopened, [(row.id, CHANGE)]);
    assert!(f.http.posts().is_empty());
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Held);
    assert!(f.load_draft(&d).unwrap().held_reason.is_some());
    assert_eq!(f.row(&row).state, LocalChangeState::Local);
    f.run();
    assert!(f.http.posts().is_empty());
    f.assert_nothing_local(&[]);
}

#[test]
fn a_closed_answer_to_a_post_holds_the_change() {
    let f = fx();
    let (row, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let d = f.draft(&row, &v[0], "a comment");
    let r = f.review(&v[0], &[("Code-Review", -1)], "");
    f.gerrit("NEW", &["s1"], json!([]));
    // Merged between the query and the post.
    f.http.queue(
        "POST /changes/7/revisions/",
        409,
        "Cannot reduce vote on labels for closed change: Code-Review",
    );
    // The detail is open when the run looks, merged when it looks again.
    for status in ["NEW", "MERGED"] {
        let body = format!(")]}}'\n{}", change_json(status, &["s1"], json!([])));
        f.http.queue("/changes/7?", 200, &body);
    }
    let report = f.run();
    // Not posted again without the vote: the change is closed.
    assert_eq!(f.http.posts().len(), 1);
    assert_eq!(report.closed, [(row.id, CHANGE)]);
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Held);
    assert_eq!(
        f.load_draft(&d).unwrap().held_reason.as_deref(),
        Some("change merged")
    );
    assert!(
        f.syncer
            .store()
            .unfinished_local_posts(f.syncer.server)
            .unwrap()
            .is_empty()
    );
}

// ---- exact matches ----

#[test]
fn an_exact_match_on_the_current_patch_set_is_sent_in_one_post() {
    let f = fx();
    let (row, v) = f.local(CID, Some("main"), None, &["s1"]);
    let d = f.draft(&row, &v[0], "a comment");
    let r = f.review(&v[0], &[("Code-Review", 2), ("Verified", 1)], "LGTM");
    f.query_finds(&[(7, "main", "NEW")]);
    f.gerrit("NEW", &["s1"], json!([]));
    f.posts_apply(json!({"Code-Review": 2, "Verified": 1}), 1);
    let report = f.run();
    assert!(
        report.stopped.is_none() && report.errors.is_empty(),
        "{report:?}"
    );

    assert_eq!(f.http.post_targets().len(), 1);
    assert!(f.http.post_targets()[0].contains("/changes/7/revisions/s1/review"));
    let body = &f.http.posts()[0];
    assert_eq!(body["labels"], json!({"Code-Review": 2, "Verified": 1}));
    assert_eq!(body["drafts"], "KEEP");
    assert_eq!(body["ignore_automatic_attention_set_rules"], true);
    assert!(body.get("notify").is_none() && body.get("message").is_none());
    let tag = body["tag"].as_str().unwrap();
    assert!(
        tag.starts_with(&format!("client-local-{}-", row.id.0)),
        "{tag}"
    );
    assert_eq!(
        body["comments"]["src/a.rs"],
        json!([{"side": "REVISION", "line": 7, "message": "a comment", "unresolved": true}])
    );
    assert_eq!(
        body["comments"][PATCHSET_LEVEL_PATH],
        json!([{"message": "LGTM", "unresolved": false}])
    );

    assert_eq!(f.load_draft(&d), None);
    let r = f.reloaded(&r);
    assert_eq!(r.state, PendingReviewState::Sent);
    assert_eq!(r.message_draft, None);
    assert_eq!(f.row(&row).state, LocalChangeState::Consolidated);
    assert_eq!(
        report.found,
        [FoundChange {
            local_change: row.id,
            local_number: row.number,
            gerrit: CHANGE
        }]
    );
    assert_eq!(report.consolidated, [(row.id, CHANGE)]);
    assert_eq!(report.posts.len(), 1);
    assert_eq!(report.posts[0].outcome, PostOutcome::Sent);
    assert_eq!(report.posts[0].comments, 2);
    assert_eq!(report.changed_gerrit_changes(), BTreeSet::from([CHANGE]));
    f.assert_nothing_local(&[]);
}

#[test]
fn the_app_name_starts_the_tag() {
    let f = fx();
    let syncer = f.syncer.clone().with_app_name("gertish");
    let (row, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    f.review(&v[0], &[("Code-Review", 1)], "");
    f.gerrit("NEW", &["s1"], json!([]));
    f.posts_apply(json!({"Code-Review": 1}), 1);
    block_on(syncer.consolidate(&ConsolidateOptions::default()));
    assert!(
        first_tag(&f).starts_with(&format!("gertish-local-{}-", row.id.0)),
        "{}",
        first_tag(&f)
    );
}

#[test]
fn web_ui_drafts_are_never_published() {
    let f = fx();
    // A local patch set of Gerrit change 7, and an ordinary draft the user
    // wrote on Gerrit's patch set 1 (say in the web UI).
    let (row, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let ordinary = f
        .syncer
        .save_draft_local(DraftSave::Create {
            change: CHANGE,
            patch_set: PatchSetRef {
                number: PatchSetNumber(1),
                sha: "s1".into(),
            },
            input: DraftInput {
                path: "src/b.rs".into(),
                side: Side::Revision,
                parent: None,
                location: Location::Line(1),
                in_reply_to: None,
                message: "not for this review".into(),
                unresolved: false,
            },
        })
        .unwrap()
        .unwrap();
    f.draft(&row, &v[0], "local");
    f.gerrit("NEW", &["s1"], json!([]));
    f.posts_apply(json!({}), 1);
    f.run();
    let body = &f.http.posts()[0];
    assert_eq!(body["drafts"], "KEEP");
    assert!(body.get("draft_ids_to_publish").is_none());
    let text = body.to_string();
    assert!(!text.contains("not for this review") && text.contains("local"));
    // The ordinary draft is still the user's.
    assert_eq!(f.load_draft(&ordinary), Some(ordinary));
}

#[test]
fn an_exact_match_on_an_older_patch_set_posts_comments_and_holds_the_votes() {
    let f = fx();
    // A local patch set of Gerrit change 7 that became PS 1; PS 2 is
    // current: the strict rule.
    let (row, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let d = f.draft(&row, &v[0], "a comment");
    let r = f.review(&v[0], &[("Code-Review", 1), ("Verified", 1)], "LGTM");
    f.gerrit("NEW", &["s1", "s2"], json!([]));
    f.posts_apply(json!({}), 2);
    let report = f.run();

    let body = &f.http.posts()[0];
    assert!(f.http.post_targets()[0].contains("/revisions/s1/review"));
    assert!(body.get("labels").is_none(), "{body}");
    assert_eq!(body["comments"]["src/a.rs"][0]["message"], "a comment");
    assert_eq!(body["comments"][PATCHSET_LEVEL_PATH][0]["message"], "LGTM");
    assert_eq!(f.load_draft(&d), None);

    let r = f.reloaded(&r);
    assert_eq!(r.state, PendingReviewState::Held);
    assert_eq!(
        r.labels,
        BTreeMap::from([("Code-Review".to_owned(), 1), ("Verified".to_owned(), 1)])
    );
    assert_eq!(r.message_draft, None);
    // Verified +1 is that label's highest value: an approval.
    let reason = r.held_reason.unwrap();
    assert!(
        reason.contains("PS 1 is not the current patch set"),
        "{reason}"
    );
    assert!(
        reason.contains("Verified +1: an approval goes only onto")
            && reason.contains("can only be dropped"),
        "{reason}"
    );
    assert!(
        reason.contains("Code-Review +1: publish onto the current patch set"),
        "{reason}"
    );
    assert!(matches!(
        &report.posts[0].outcome,
        PostOutcome::VotesHeld { votes, .. } if votes.len() == 2
    ));
    // The held review keeps the change open.
    assert_eq!(f.row(&row).state, LocalChangeState::Local);
    assert!(report.consolidated.is_empty());
}

#[test]
fn a_purely_local_approval_may_be_published_under_exact_or_manual() {
    let f = fx();
    let (row, v) = f.local(CID, Some("main"), None, &["s1"]);
    // Given long before Gerrit's change was created (2024-01-01).
    let r = f.review_with(
        &v[0],
        &[("Code-Review", 2)],
        "",
        LocalApproval::ExactOrManual,
        ts("2023-06-01T00:00:00Z"),
    );
    f.query_finds(&[(7, "main", "NEW")]);
    f.gerrit("NEW", &["s1", "s2"], json!([]));
    let options = ConsolidateOptions {
        local_approval: BTreeMap::from([("p".to_owned(), LocalApproval::ExactOrManual)]),
        ..Default::default()
    };
    let report = f.run_with(&options);
    // Nothing to post: no comments, the vote held.
    assert!(f.http.posts().is_empty());
    let r = f.reloaded(&r);
    assert_eq!(r.state, PendingReviewState::Held);
    let reason = r.held_reason.clone().unwrap();
    assert!(
        reason.contains("Code-Review +2: publish onto the current patch set"),
        "{reason}"
    );
    assert!(matches!(
        report.posts[0].outcome,
        PostOutcome::VotesHeld { .. }
    ));
    // Publishable: no drop-only marker.
    assert!(r.drop_only.is_empty());
    // The project's setting now is stricter (no entry: exact): drop only.
    let votes: Votes = r.labels.clone().into_iter().collect();
    let drop_only = drop_only_labels(
        &row,
        &model_detail(&["s1", "s2"]),
        Some(&r),
        &votes,
        &ConsolidateOptions::default(),
    );
    assert_eq!(drop_only, ["Code-Review".to_owned()].into());
    let reason = held_reason(
        PatchSetNumber(1),
        Some(PatchSetNumber(2)),
        &votes,
        &drop_only,
    );
    assert!(reason.contains("can only be dropped"), "{reason}");
}

fn model_detail(shas: &[&str]) -> ChangeDetail {
    let info = change_json("NEW", shas, json!([]));
    // Through the client's conversion, as a run sees it.
    let http = Http::default();
    http.routes.route_json("/changes/7?", &info.to_string());
    let client = GerritClient::new(Arc::new(http), BASE, Auth::Anonymous).unwrap();
    block_on(client.get_change(CHANGE)).unwrap()
}

#[test]
fn an_approval_is_the_highest_positive_value_of_the_target_change() {
    let mut d = model_detail(&["s1"]);
    assert!(is_approval(&d, "Code-Review", 2));
    assert!(!is_approval(&d, "Code-Review", 1));
    assert!(is_approval(&d, "Verified", 1));
    assert!(!is_approval(&d, "Verified", -1));
    // A label the change does not have, and one whose top value is 0.
    assert!(!is_approval(&d, "Borrowed", 2));
    // Above the change's highest value (given with borrowed labels whose
    // range is wider): an approval too.
    assert!(is_approval(&d, "Verified", 2));
    d.label_values.insert(
        "Block".into(),
        vec![
            model::LabelValue {
                value: -1,
                description: String::new(),
            },
            model::LabelValue {
                value: 0,
                description: String::new(),
            },
        ],
    );
    assert!(!is_approval(&d, "Block", 0));
}

#[test]
fn a_newer_review_supersedes_an_older_ones_vote() {
    let f = fx();
    let (row, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let older = f.review(&v[0], &[("Code-Review", 1), ("Verified", 1)], "first");
    let newer = f.review(&v[0], &[("Code-Review", 2)], "second");
    f.gerrit("NEW", &["s1"], json!([]));
    f.posts_apply(json!({"Code-Review": 2, "Verified": 1}), 1);
    let report = f.run();
    let posts = f.http.posts();
    assert_eq!(posts.len(), 2);
    // In creation order; the older one without its superseded vote.
    assert_eq!(posts[0]["labels"], json!({"Verified": 1}));
    assert_eq!(
        posts[0]["comments"][PATCHSET_LEVEL_PATH][0]["message"],
        "first"
    );
    assert_eq!(posts[1]["labels"], json!({"Code-Review": 2}));
    assert_eq!(
        posts[1]["comments"][PATCHSET_LEVEL_PATH][0]["message"],
        "second"
    );
    assert_ne!(posts[0]["tag"], posts[1]["tag"]);
    for r in [&older, &newer] {
        assert_eq!(f.reloaded(r).state, PendingReviewState::Sent);
    }
    assert_eq!(report.posts.len(), 2);
    assert_eq!(f.row(&row).state, LocalChangeState::Consolidated);
}

#[test]
fn a_stopped_run_ends_before_its_next_post() {
    let f = fx();
    let (row, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let first = f.review(&v[0], &[("Code-Review", 1)], "first");
    let second = f.review(&v[0], &[], "second");
    f.gerrit("NEW", &["s1"], json!([]));
    let options = ConsolidateOptions::default();
    // The first post is answered, and the run is asked to stop meanwhile
    // (the user signed out while it was in flight).
    let stop = options.stop.clone();
    f.http.respond("/review", move |_| {
        stop.stop();
        json!({"labels": {"Code-Review": 1}, "change_info": {"current_revision_number": 1}})
    });
    let report = f.run_with(&options);
    assert!(matches!(report.stopped, Some(Error::Stopped)), "{report:?}");
    assert_eq!(f.http.posts().len(), 1, "the post in flight completes");
    assert_eq!(f.reloaded(&first).state, PendingReviewState::Sent);
    assert_eq!(f.reloaded(&second).state, PendingReviewState::Pending);
    assert_ne!(f.row(&row).state, LocalChangeState::Consolidated);

    // A run with a flag set from the start posts nothing; the next one
    // goes on.
    let stopped = ConsolidateOptions::default();
    stopped.stop.stop();
    let report = f.run_with(&stopped);
    assert!(matches!(report.stopped, Some(Error::Stopped)), "{report:?}");
    assert_eq!(f.http.posts().len(), 1);
    let report = f.run();
    assert!(report.stopped.is_none(), "{report:?}");
    assert_eq!(f.http.posts().len(), 2);
    assert_eq!(f.reloaded(&second).state, PendingReviewState::Sent);
}

#[test]
fn stop_flags_are_equal_only_to_their_clones() {
    let a = StopFlag::default();
    assert_eq!(a, a.clone());
    assert_ne!(a, StopFlag::default());
}

#[test]
fn a_vote_refusal_reposts_without_the_votes() {
    let f = fx();
    let (row, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let d = f.draft(&row, &v[0], "a comment");
    let r = f.review(&v[0], &[("Code-Review", 2)], "LGTM");
    f.gerrit("NEW", &["s1"], json!([]));
    f.posts_apply(json!({}), 1);
    f.http.queue(
        "POST /changes/7/revisions/",
        403,
        "Applying label \"Code-Review\": 2 is restricted",
    );
    let report = f.run();
    let posts = f.http.posts();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[0]["labels"], json!({"Code-Review": 2}));
    assert!(posts[1].get("labels").is_none());
    // The same post otherwise, under the same tag.
    assert_eq!(posts[0]["comments"], posts[1]["comments"]);
    assert_eq!(posts[0]["tag"], posts[1]["tag"]);
    // Comments and message are posted (once: the first try was refused).
    assert_eq!(f.load_draft(&d), None);
    let r = f.reloaded(&r);
    assert_eq!(r.state, PendingReviewState::Failed);
    assert_eq!(
        r.error.as_deref(),
        Some("Applying label \"Code-Review\": 2 is restricted")
    );
    assert_eq!(r.labels, BTreeMap::from([("Code-Review".to_owned(), 2)]));
    assert_eq!(r.message_draft, None);
    assert!(matches!(
        &report.posts[0].outcome,
        PostOutcome::VotesFailed { votes, .. } if votes == &[("Code-Review".to_owned(), 2)]
    ));
    // The failed votes keep the change open for the held card.
    assert_eq!(f.row(&row).state, LocalChangeState::Local);
    let held = f
        .syncer
        .store()
        .held_items(f.syncer.server, CHANGE)
        .unwrap();
    assert_eq!(held.reviews.len(), 1);
    // And nothing is sent again by the next run.
    f.run();
    assert_eq!(f.http.posts().len(), 2);
}

#[test]
fn votes_gerrit_does_not_apply_are_failed() {
    let f = fx();
    let (_, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let r = f.review(&v[0], &[("Code-Review", 2), ("Verified", 1)], "LGTM");
    f.gerrit("NEW", &["s1"], json!([]));
    // As Gerrit 3.14 answers a label it does not know: 200, left out.
    f.posts_apply(json!({"Code-Review": 2}), 1);
    let report = f.run();
    let r = f.reloaded(&r);
    assert_eq!(r.state, PendingReviewState::Failed);
    assert_eq!(r.labels, BTreeMap::from([("Verified".to_owned(), 1)]));
    assert_eq!(r.error.as_deref(), Some("Gerrit did not apply Verified +1"));
    assert_eq!(report.posts[0].votes, [("Code-Review".to_owned(), 2)]);
}

#[test]
fn votes_that_arrive_after_a_new_patch_set_are_held_where_they_landed() {
    let f = fx();
    let (_, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let r = f.review(&v[0], &[("Code-Review", 2), ("Verified", -1)], "");
    f.gerrit("NEW", &["s1"], json!([]));
    // Applied, but PS 2 was uploaded in between.
    f.posts_apply(json!({"Code-Review": 2, "Verified": -1}), 2);
    let report = f.run();
    let r = f.reloaded(&r);
    assert_eq!(r.state, PendingReviewState::Held);
    let reason = r.held_reason.unwrap();
    assert!(
        reason.contains("cast on PS 1, which was no longer current (PS 2 is)"),
        "{reason}"
    );
    assert_eq!(r.labels.len(), 2);
    // The approval may only be dropped; the other vote may be published.
    assert_eq!(r.drop_only, ["Code-Review".to_owned()].into());
    // The report says what was cast, on PS 1.
    let post = &report.posts[0];
    assert_eq!(post.patch_set, PatchSetNumber(1));
    assert_eq!(
        post.votes,
        [("Code-Review".to_owned(), 2), ("Verified".to_owned(), -1)]
    );
    assert!(
        matches!(&post.outcome, PostOutcome::VotesHeld { drop_only, .. }
        if drop_only == &["Code-Review".to_owned()])
    );
}

#[test]
fn a_refused_post_fails_the_review_and_its_comments() {
    let f = fx();
    let (row, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let d = f.draft(&row, &v[0], "a comment");
    let r = f.review(&v[0], &[("Code-Review", 1)], "LGTM");
    f.gerrit("NEW", &["s1"], json!([]));
    f.http.routes.route(
        "POST /changes/7/revisions/",
        400,
        "file src/a.rs not found in revision 7,1",
    );
    let report = f.run();
    assert_eq!(f.http.posts().len(), 1, "not a vote problem: no second try");
    let r = f.reloaded(&r);
    assert_eq!(r.state, PendingReviewState::Failed);
    assert_eq!(
        r.error.as_deref(),
        Some("file src/a.rs not found in revision 7,1")
    );
    // The comment waits in the held card with Gerrit's answer; it blocks
    // nothing.
    let d = f.load_draft(&d).unwrap();
    assert_eq!(
        d.held_reason.as_deref(),
        Some("Gerrit refused the post: file src/a.rs not found in revision 7,1")
    );
    let items = f
        .syncer
        .store()
        .held_items(f.syncer.server, CHANGE)
        .unwrap();
    assert_eq!((items.drafts.len(), items.reviews.len()), (1, 1));
    assert!(matches!(
        report.posts[0].outcome,
        PostOutcome::Refused { .. }
    ));
    assert!(
        f.syncer
            .store()
            .unfinished_local_posts(f.syncer.server)
            .unwrap()
            .is_empty()
    );
    // Not posted again automatically.
    f.run();
    assert_eq!(f.http.posts().len(), 1);
}

// ---- recovery and failures ----

/// A local change on Gerrit change 7 with a draft and a review, whose post
/// is lost on the way (no answer): its claim stays.
fn interrupted(f: &Fx) -> (LocalChange, Draft, PendingReview, String) {
    let (row, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let d = f.draft(&row, &v[0], "a comment");
    let r = f.review(&v[0], &[("Code-Review", 2)], "LGTM");
    f.gerrit("NEW", &["s1"], json!([]));
    f.http
        .queue("POST /changes/7/revisions/", 0, "connection reset");
    let report = f.run();
    assert!(report.stopped.is_some());
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Sending);
    assert_eq!(
        f.syncer
            .store()
            .unfinished_local_posts(f.syncer.server)
            .unwrap()
            .len(),
        1
    );
    (row, d, r, first_tag(f))
}

#[test]
fn after_an_interruption_a_post_found_on_the_server_is_not_sent_again() {
    let f = fx();
    let (row, d, r, tag) = interrupted(&f);
    // It did arrive: the change has my message with the tag, and my vote.
    let mut change = change_json(
        "NEW",
        &["s1"],
        json!([{"id": "m1", "author": {"_account_id": ME}, "date": "2024-01-03 00:00:00.000000000",
                "message": "Patch Set 1: Code-Review+2\n\n(2 comments)", "tag": tag,
                "_revision_number": 1}]),
    );
    change["labels"]["Code-Review"]["all"] = json!([{"_account_id": ME, "value": 2}]);
    f.http.routes.route_json("/changes/7?", &change.to_string());
    let report = f.run();
    assert!(report.stopped.is_none(), "{report:?}");
    assert_eq!(f.http.posts().len(), 1, "not posted again");
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Sent);
    assert_eq!(f.load_draft(&d), None);
    assert!(report.posts[0].recovered);
    assert_eq!(report.posts[0].outcome, PostOutcome::Sent);
    assert_eq!(f.row(&row).state, LocalChangeState::Consolidated);
}

#[test]
fn a_message_by_someone_else_with_the_tag_does_not_count() {
    let f = fx();
    let (_, _, r, tag) = interrupted(&f);
    f.gerrit(
        "NEW",
        &["s1"],
        json!([{"id": "m1", "author": {"_account_id": 5}, "date": "2024-01-03 00:00:00.000000000",
                "message": "x", "tag": tag, "_revision_number": 1}]),
    );
    f.posts_apply(json!({"Code-Review": 2}), 1);
    f.run();
    assert_eq!(f.http.posts().len(), 2);
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Sent);
}

#[test]
fn after_an_interruption_a_post_not_on_the_server_is_sent_again_under_its_nonce() {
    let f = fx();
    let (row, d, r, tag) = interrupted(&f);
    f.posts_apply(json!({"Code-Review": 2}), 1);
    let report = f.run();
    let posts = f.http.posts();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[1]["tag"], tag.as_str());
    assert_eq!(posts[0], posts[1]);
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Sent);
    assert_eq!(f.load_draft(&d), None);
    assert!(!report.posts[0].recovered);
    assert_eq!(f.row(&row).state, LocalChangeState::Consolidated);
}

#[test]
fn a_lost_connection_ends_the_run_and_leaves_the_rest_alone() {
    let f = fx();
    let (row, v) = f.local(CID, Some("main"), None, &["s1"]);
    let r = f.review(&v[0], &[("Code-Review", 2)], "LGTM");
    f.query_finds(&[(7, "main", "NEW")]);
    f.http.queue("/changes/7?", 0, "network is unreachable");
    let report = f.run();
    assert!(matches!(report.stopped, Some(ref e) if e.to_string().contains("unreachable")));
    // Nothing half done: the row was not found yet, nothing claimed.
    let now = f.row(&row);
    assert_eq!(
        (now.state, now.gerrit_number),
        (LocalChangeState::Checking, None)
    );
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Pending);
    assert!(f.http.posts().is_empty());
    // Back online, the next run does it.
    f.gerrit("NEW", &["s1"], json!([]));
    f.posts_apply(json!({"Code-Review": 2}), 1);
    f.run();
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Sent);
}

#[test]
fn a_refused_login_releases_the_claim_and_ends_the_run() {
    let f = fx();
    let (_, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let r = f.review(&v[0], &[("Code-Review", 2)], "LGTM");
    f.gerrit("NEW", &["s1"], json!([]));
    f.http.queue("POST /changes/7/revisions/", 401, "");
    let report = f.run();
    assert!(report.stopped.unwrap().is_unauthorized());
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Pending);
    assert!(
        f.syncer
            .store()
            .unfinished_local_posts(f.syncer.server)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn only_one_run_at_a_time() {
    let f = fx();
    let (_, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    f.review(&v[0], &[("Code-Review", 2)], "LGTM");
    // Another run (here or in another process on this cache) holds the
    // lease.
    let now = Timestamp::now();
    let later = now.checked_add(SignedDuration::from_mins(5)).unwrap();
    assert!(
        f.syncer
            .store()
            .take_consolidation_lease(f.syncer.server, "other-run", now, later)
            .unwrap()
    );
    let report = f.run();
    assert!(!report.ran);
    assert!(f.http.routes.requested().is_empty());
    f.syncer
        .store()
        .end_consolidation_lease(f.syncer.server, "other-run")
        .unwrap();
    f.gerrit("NEW", &["s1"], json!([]));
    f.posts_apply(json!({"Code-Review": 2}), 1);
    assert!(f.run().ran);
    assert_eq!(f.http.posts().len(), 1);
}

#[test]
fn an_anonymous_session_does_nothing() {
    let http = Http::default();
    let client = GerritClient::new(Arc::new(http.clone()), BASE, Auth::Anonymous).unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let server = store.server_id(BASE).unwrap();
    let syncer = Syncer::new(client, Arc::new(Mutex::new(store)), server);
    let report = block_on(syncer.consolidate(&ConsolidateOptions::default()));
    assert!(report.ran);
    assert!(http.routes.requested().is_empty());
}

#[test]
fn a_version_that_is_not_on_gerrit_stays_unsent() {
    let f = fx();
    // s1 was pushed as PS 1; l2 is an amend that never was.
    let (row, v) = f.local(CID, Some("main"), None, &["s1", "l2"]);
    let pushed = f.review(&v[0], &[("Code-Review", 1)], "on the pushed one");
    let amended = f.review(&v[1], &[("Code-Review", 2)], "on the amend");
    let local_draft = f.draft(&row, &v[1], "on the amend");
    f.query_finds(&[(7, "main", "NEW")]);
    f.gerrit("NEW", &["s1"], json!([]));
    f.posts_apply(json!({"Code-Review": 1}), 1);
    let report = f.run();
    assert_eq!(f.http.posts().len(), 1);
    assert_eq!(f.reloaded(&pushed).state, PendingReviewState::Sent);
    // The amend's review and draft: untouched, on the Gerrit change now,
    // still on their local patch set, nothing held (porting is LC5b's).
    let amended = f.reloaded(&amended);
    assert_eq!(
        (amended.state, amended.change, amended.patch_set),
        (PendingReviewState::Pending, CHANGE, v[1].number)
    );
    let d = f.load_draft(&local_draft).unwrap();
    assert_eq!((d.change, d.patch_set), (CHANGE, v[1].number));
    assert!(d.held_reason.is_none() && d.reserved_by.is_none());
    assert_eq!(f.row(&row).state, LocalChangeState::Local);
    assert!(report.consolidated.is_empty());
    f.assert_nothing_local(&["l2"]);
}

// ---- runs that must not step on each other ----

#[test]
fn a_claim_still_in_flight_is_not_recovered_by_another_syncer() {
    // Syncer A's post is lost on the way: its claim stays, young.
    let a = fx_with(ConsolidationTiming::default());
    let (row, d, r, tag) = interrupted(&a);
    // Syncer B on the same cache (another process, or a new session) does
    // not take it over while it may still be in flight.
    let b = fx_on(
        a.syncer.store.clone(),
        a.syncer.server,
        ConsolidationTiming::default(),
    );
    b.gerrit("NEW", &["s1"], json!([]));
    b.posts_apply(json!({"Code-Review": 2}), 1);
    let report = b.run();
    assert!(report.ran && report.posts.is_empty(), "{report:?}");
    assert!(b.http.posts().is_empty());
    assert_eq!(a.reloaded(&r).state, PendingReviewState::Sending);
    // Once it is old enough, a run recovers it: not on the server, so it
    // is posted again under its own tag.
    let c = fx_on(a.syncer.store.clone(), a.syncer.server, quick());
    c.gerrit("NEW", &["s1"], json!([]));
    c.posts_apply(json!({"Code-Review": 2}), 1);
    let report = c.run();
    assert_eq!(c.http.posts().len(), 1, "{report:?}");
    assert_eq!(c.http.posts()[0]["tag"], tag.as_str());
    assert_eq!(a.reloaded(&r).state, PendingReviewState::Sent);
    assert_eq!(a.load_draft(&d), None);
    assert_eq!(a.row(&row).state, LocalChangeState::Consolidated);
    c.assert_nothing_local(&[]);
}

#[test]
fn a_second_syncer_does_not_run_while_one_holds_the_lease() {
    let a = fx();
    let (_, v) = a.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    a.review(&v[0], &[("Code-Review", 2)], "LGTM");
    let b = fx_on(a.syncer.store.clone(), a.syncer.server, quick());
    // A run of `a` is going on (it took the lease).
    let now = Timestamp::now();
    let later = now.checked_add(CLAIM_STALE).unwrap();
    assert!(
        a.syncer
            .store()
            .take_consolidation_lease(a.syncer.server, "a-run", now, later)
            .unwrap()
    );
    assert!(!b.run().ran);
    assert!(b.http.routes.requested().is_empty());
    // An expired lease is taken over.
    let earlier = now.checked_sub(SignedDuration::from_secs(1)).unwrap();
    a.syncer
        .store()
        .take_consolidation_lease(a.syncer.server, "a-run", earlier, earlier)
        .unwrap();
    b.gerrit("NEW", &["s1"], json!([]));
    b.posts_apply(json!({"Code-Review": 2}), 1);
    assert!(b.run().ran);
    assert_eq!(b.http.posts().len(), 1);
}

// ---- the current patch set moving ----

#[test]
fn once_a_new_patch_set_shows_up_later_posts_hold_their_votes() {
    let f = fx();
    let (_, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let first = f.review(&v[0], &[("Verified", 1)], "first");
    let second = f.review(&v[0], &[("Code-Review", 2)], "second");
    f.gerrit("NEW", &["s1"], json!([]));
    // The first answer says PS 2 is current now.
    f.posts_apply(json!({"Verified": 1}), 2);
    f.run();
    let posts = f.http.posts();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[0]["labels"], json!({"Verified": 1}));
    assert!(posts[1].get("labels").is_none(), "{}", posts[1]);
    assert_eq!(
        posts[1]["comments"][PATCHSET_LEVEL_PATH][0]["message"],
        "second"
    );
    let first = f.reloaded(&first);
    assert_eq!(first.state, PendingReviewState::Held);
    assert!(first.held_reason.unwrap().contains("cast on PS 1"));
    let second = f.reloaded(&second);
    assert_eq!(second.state, PendingReviewState::Held);
    assert!(
        second
            .held_reason
            .unwrap()
            .contains("PS 1 is not the current patch set on Gerrit (PS 2 is)")
    );
    assert_eq!(second.drop_only, ["Code-Review".to_owned()].into());
}

#[test]
fn votes_on_an_old_patch_set_are_noticed_without_change_info() {
    let f = fx();
    let (_, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let r = f.review(&v[0], &[("Code-Review", 2)], "");
    // Before the post: PS 1 is current. The answer has no `change_info`
    // (an older server).
    f.gerrit("NEW", &["s1"], json!([]));
    f.http.routes.route_json(
        "POST /changes/7/revisions/",
        &json!({"labels": {"Code-Review": 2}}).to_string(),
    );
    // After the post the change shows PS 2, uploaded before our message.
    f.http.queue(
        "/changes/7?",
        200,
        &format!(")]}}'\n{}", change_json("NEW", &["s1"], json!([]))),
    );
    f.http.respond("/changes/7?", |posts| {
        let tag = posts.last().map(|p| p["tag"].clone()).unwrap_or_default();
        let mut c = change_json(
            "NEW",
            &["s1", "s2"],
            json!([{"id": "m1", "author": {"_account_id": ME},
                    "date": "2024-01-03 00:00:00.000000000", "message": "Patch Set 1: Code-Review+2",
                    "tag": tag, "_revision_number": 1}]),
        );
        c["revisions"]["s2"]["created"] = json!("2024-01-02 00:00:00.000000000");
        c
    });
    let report = f.run();
    let r = f.reloaded(&r);
    assert_eq!(r.state, PendingReviewState::Held, "{report:?}");
    assert!(r.held_reason.unwrap().contains("cast on PS 1"));
    assert_eq!(r.drop_only, ["Code-Review".to_owned()].into());
    assert!(matches!(
        report.posts[0].outcome,
        PostOutcome::VotesHeld { .. }
    ));
}

#[test]
fn a_newer_review_whose_votes_are_held_supersedes_nothing() {
    let f = fx();
    // PS 1 and PS 2 are both local versions that were pushed; PS 2 is
    // current.
    let (row, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1", "s2"]);
    let older = f.review(&v[1], &[("Code-Review", 1)], "");
    let newer = f.review(&v[0], &[("Code-Review", 2)], "");
    f.gerrit("NEW", &["s1", "s2"], json!([]));
    f.posts_apply(json!({"Code-Review": 1}), 2);
    f.run();
    // The newer vote (on PS 1) is held, so the older one, meant for the
    // current PS 2, is still cast.
    let posts = f.http.posts();
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert_eq!(posts[0]["labels"], json!({"Code-Review": 1}));
    assert_eq!(f.reloaded(&older).state, PendingReviewState::Sent);
    assert_eq!(f.reloaded(&newer).state, PendingReviewState::Held);
    let _ = row;
}

// ---- recovery reads the votes from the message ----

#[test]
fn recovery_takes_the_cast_votes_from_the_tagged_message() {
    let f = fx();
    let (_, _, r, tag) = interrupted(&f);
    // The message records the vote; the change's vote list (current patch
    // set only) does not show it.
    f.gerrit(
        "NEW",
        &["s1"],
        json!([{"id": "m1", "author": {"_account_id": ME}, "date": "2024-01-03 00:00:00.000000000",
                "message": "Patch Set 1: Code-Review+2\n\n(2 comments)", "tag": tag,
                "_revision_number": 1}]),
    );
    let report = f.run();
    assert_eq!(f.http.posts().len(), 1, "not posted again");
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Sent);
    assert_eq!(report.posts[0].votes, [("Code-Review".to_owned(), 2)]);
    f.assert_nothing_local(&[]);

    // A message without the vote: it did not apply.
    let f = fx();
    let (_, _, r, tag) = interrupted(&f);
    f.gerrit(
        "NEW",
        &["s1"],
        json!([{"id": "m1", "author": {"_account_id": ME}, "date": "2024-01-03 00:00:00.000000000",
                "message": "Patch Set 1:\n\n(2 comments)", "tag": tag, "_revision_number": 1}]),
    );
    f.run();
    let r = f.reloaded(&r);
    assert_eq!(r.state, PendingReviewState::Failed);
    assert_eq!(r.labels, BTreeMap::from([("Code-Review".to_owned(), 2)]));
    f.assert_nothing_local(&[]);
}

#[test]
fn votes_are_read_from_a_change_message() {
    assert_eq!(
        votes_in_message("Patch Set 2: Code-Review+2 Verified-1 -My-Label\n\n(1 comment)"),
        BTreeMap::from([
            ("Code-Review".to_owned(), 2),
            ("My-Label".to_owned(), 0),
            ("Verified".to_owned(), -1)
        ])
    );
    assert!(votes_in_message("Patch Set 2:\n\nLGTM").is_empty());
    assert!(votes_in_message("Uploaded patch set 3.").is_empty());
}

// ---- asking rarely ----

#[test]
fn a_change_with_nothing_to_send_is_asked_about_rarely() {
    let f = fx_with(quick());
    let (row, v) = f.local(CID, Some("main"), Some(CHANGE), &["l1"]);
    let r = f.review(&v[0], &[("Code-Review", 2)], "");
    f.syncer
        .store()
        .set_pending_review_state(
            f.syncer.server,
            r.id,
            &[PendingReviewState::Pending],
            PendingReviewState::Held,
            Some("held"),
        )
        .unwrap();
    f.gerrit("NEW", &["s1"], json!([]));
    f.run();
    assert_eq!(f.http.routes.requested().len(), 1, "one batch query");
    f.run();
    assert_eq!(f.http.routes.requested().len(), 1, "not again soon");
    assert_eq!(f.row(&row).state, LocalChangeState::Local);
}

#[test]
fn known_changes_are_asked_about_in_one_query() {
    let f = fx();
    // Two Gerrit changes with local versions that are not on Gerrit, each
    // with a pending review: nothing to move or send yet.
    let other = "I1111111111111111111111111111111111111111";
    let (_, a) = f.local(CID, Some("main"), Some(CHANGE), &["l1"]);
    let (_, b) = f.local(other, Some("main"), Some(ChangeNumber(8)), &["l2"]);
    f.review(&a[0], &[("Code-Review", 1)], "");
    f.review(&b[0], &[("Code-Review", 1)], "");
    let mut eight = change_json("NEW", &["t1"], json!([]));
    eight["_number"] = json!(8);
    eight["change_id"] = json!(other);
    f.http.routes.route_json(
        "/changes/?q=change%3A",
        &json!([change_json("NEW", &["s1"], json!([])), eight]).to_string(),
    );
    let report = f.run();
    assert!(report.errors.is_empty(), "{report:?}");
    let sent = f.http.routes.requested();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("ALL_REVISIONS"), "{sent:?}");
    f.assert_nothing_local(&["l1", "l2"]);
}

// ---- bounded posts ----

#[test]
fn a_post_without_an_answer_is_given_up_and_left_for_the_tag_check() {
    let f = fx_with(ConsolidationTiming {
        post_deadline: std::time::Duration::from_millis(50),
        ..quick()
    });
    let (row, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let d = f.draft(&row, &v[0], "a comment");
    let r = f.review(&v[0], &[("Code-Review", 1)], "");
    f.gerrit("NEW", &["s1"], json!([]));
    f.http.queue("POST /changes/7/revisions/", 1, "");
    let report = f.run();
    assert!(
        matches!(report.stopped, Some(Error::PostTimedOut)),
        "{report:?}"
    );
    // The claim stays: the next run checks the change's messages.
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Sending);
    assert!(f.load_draft(&d).is_some());
    assert_eq!(
        f.syncer
            .store()
            .unfinished_local_posts(f.syncer.server)
            .unwrap()
            .len(),
        1
    );
    let tag = first_tag(&f);
    f.gerrit(
        "NEW",
        &["s1"],
        json!([{"id": "m1", "author": {"_account_id": ME}, "date": "2024-01-03 00:00:00.000000000",
                "message": "Patch Set 1: Code-Review+1", "tag": tag, "_revision_number": 1}]),
    );
    f.run();
    assert_eq!(f.http.posts().len(), 1, "it arrived: not posted again");
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Sent);
}

#[test]
fn a_second_run_in_the_same_process_waits_for_the_first() {
    let f = fx();
    let (_, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    f.review(&v[0], &[("Code-Review", 1)], "");
    // A run of this store and server is going on in this process (it may
    // be asleep in a post); the lease alone would not tell, since its
    // clock may have jumped.
    let key = (Arc::as_ptr(&f.syncer.store) as usize, f.syncer.server.0);
    let running = Running::start(key).unwrap();
    let report = f.run();
    assert!(!report.ran);
    assert!(f.http.routes.requested().is_empty());
    drop(running);
    f.gerrit("NEW", &["s1"], json!([]));
    f.posts_apply(json!({"Code-Review": 1}), 1);
    assert!(f.run().ran);
}

// ---- second review round ----

#[test]
fn losing_the_lease_mid_run_stops_before_the_next_post() {
    let f = fx();
    let (_, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let first = f.review(&v[0], &[("Code-Review", 1)], "first");
    let second = f.review(&v[0], &[("Verified", 1)], "second");
    f.gerrit("NEW", &["s1"], json!([]));
    // While the first post is answered, another run takes the lease over
    // (as if this run's had expired).
    let store = f.syncer.store.clone();
    let server = f.syncer.server;
    f.http.respond("/review", move |_| {
        let far = Timestamp::now()
            .checked_add(SignedDuration::from_hours(1))
            .unwrap();
        let later = far.checked_add(SignedDuration::from_hours(1)).unwrap();
        store
            .lock()
            .unwrap()
            .take_consolidation_lease(server, "intruder", far, later)
            .unwrap();
        json!({"labels": {"Code-Review": 1}, "change_info": {"current_revision_number": 1}})
    });
    let report = f.run();
    assert!(
        matches!(report.stopped, Some(Error::LeaseLost)),
        "{report:?}"
    );
    assert_eq!(f.http.posts().len(), 1, "no post after the lease was lost");
    assert_eq!(f.reloaded(&first).state, PendingReviewState::Sent);
    assert_eq!(f.reloaded(&second).state, PendingReviewState::Pending);
    assert!(
        f.syncer
            .store()
            .unfinished_local_posts(f.syncer.server)
            .unwrap()
            .is_empty()
    );
    // The intruder's lease is not ended by this run.
    let now = Timestamp::now();
    assert!(
        !f.syncer
            .store()
            .take_consolidation_lease(f.syncer.server, "someone", now, now)
            .unwrap()
    );
}

#[test]
fn a_refused_post_holds_only_its_own_revisions_comments() {
    let f = fx();
    let (row, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1", "s2"]);
    let on1 = f.draft(&row, &v[0], "on PS 1");
    let on2 = f.draft(&row, &v[1], "on PS 2");
    let r = f.review(&v[1], &[("Code-Review", 1)], "");
    f.gerrit("NEW", &["s1", "s2"], json!([]));
    f.posts_apply(json!({}), 2);
    // The post to PS 2 is refused; PS 1's comment-only post goes through.
    f.http
        .queue("POST /changes/7/revisions/s2/review", 400, "bad line");
    f.run();
    let targets = f.http.post_targets();
    assert_eq!(targets.len(), 2, "{targets:?}");
    assert!(targets[0].contains("/revisions/s1/review"));
    assert!(targets[1].contains("/revisions/s2/review"));
    assert_eq!(f.load_draft(&on1), None, "posted on PS 1");
    assert_eq!(
        f.load_draft(&on2).unwrap().held_reason.as_deref(),
        Some("Gerrit refused the post: bad line")
    );
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Failed);
}

#[test]
fn recovery_gives_a_post_up_when_its_change_is_gone() {
    let f = fx();
    let (_, d, r, _) = interrupted(&f);
    f.http.routes.route("/changes/7?", 404, "Not found: 7");
    let report = f.run();
    assert!(report.stopped.is_none(), "{report:?}");
    let r = f.reloaded(&r);
    assert_eq!(r.state, PendingReviewState::Failed);
    assert_eq!(
        r.error.as_deref(),
        Some("change 7 is not on Gerrit any more")
    );
    assert_eq!(
        f.load_draft(&d).unwrap().held_reason.as_deref(),
        Some("change 7 is not on Gerrit any more")
    );
    assert!(
        f.syncer
            .store()
            .unfinished_local_posts(f.syncer.server)
            .unwrap()
            .is_empty()
    );
    assert_eq!(f.http.posts().len(), 1);
}

#[test]
fn recovery_gives_a_post_up_when_its_patch_set_is_gone() {
    let f = fx();
    let (_, d, r, _) = interrupted(&f);
    // PS 1 was deleted (only PS 2 is listed).
    let mut c = change_json("NEW", &["s1", "s2"], json!([]));
    c["revisions"].as_object_mut().unwrap().remove("s1");
    f.gerrit_json(c);
    f.run();
    let r = f.reloaded(&r);
    assert_eq!(r.state, PendingReviewState::Failed);
    assert_eq!(r.error.as_deref(), Some("PS 1 is not on change 7 any more"));
    assert!(f.load_draft(&d).unwrap().held_reason.is_some());
    assert_eq!(f.http.posts().len(), 1);
    f.assert_nothing_local(&[]);
}

#[test]
fn a_draft_written_while_closed_stays_held_after_a_restore() {
    let f = fx_with(eager());
    let (row, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    f.review(&v[0], &[("Code-Review", 1)], "");
    f.gerrit("ABANDONED", &["s1"], json!([]));
    f.run();
    assert_eq!(f.row(&row).state, LocalChangeState::Closed);
    // Written while closed: held at once.
    let late = f.draft(&row, &v[0], "written while closed");
    assert!(late.held_reason.is_some());
    f.run();
    // Restored: nothing goes out.
    f.gerrit("NEW", &["s1"], json!([]));
    f.posts_apply(json!({}), 1);
    let report = f.run();
    assert_eq!(report.reopened, [(row.id, CHANGE)]);
    f.run();
    assert!(f.http.posts().is_empty());
    assert!(f.load_draft(&late).unwrap().held_reason.is_some());
}

#[test]
fn a_review_posted_before_a_lost_claim_is_not_posted_again() {
    let f = fx();
    let (row, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let posted = f.draft(&row, &v[0], "a comment");
    let r = f.review(&v[0], &[("Code-Review", 1)], "LGTM");
    // Written after the lost post: still to send.
    let later = f.draft(&row, &v[0], "written later");
    // The change shows the review's post (its tag names the review), and
    // the comments it published; the store lost the claim and the result.
    let tag = format!("client-local-{}-r{}-lostnonce", row.id.0, r.id.0);
    let mut c = change_json(
        "NEW",
        &["s1"],
        json!([{"id": "m1", "author": {"_account_id": ME}, "date": "2024-01-03 00:00:00.000000000",
                "message": "Patch Set 1: Code-Review+1\n\n(2 comments)", "tag": tag,
                "_revision_number": 1}]),
    );
    c["labels"]["Code-Review"]["all"] = json!([{"_account_id": ME, "value": 1}]);
    f.gerrit_json(c);
    let comment = |path: &str, line: Option<u32>, message: &str| {
        let mut c = json!({"id": message, "patch_set": 1, "message": message, "tag": tag,
                           "updated": "2024-01-03 00:00:00.000000000",
                           "author": {"_account_id": ME}, "unresolved": false});
        if let Some(l) = line {
            c["line"] = json!(l);
        }
        let _ = path;
        c
    };
    f.http.routes.route_json(
        "/changes/7/comments",
        &json!({
            "src/a.rs": [comment("src/a.rs", Some(7), "a comment")],
            "/PATCHSET_LEVEL": [comment("/PATCHSET_LEVEL", None, "LGTM")],
        })
        .to_string(),
    );
    f.posts_apply(json!({}), 1);
    let report = f.run();
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Sent, "{report:?}");
    assert_eq!(f.load_draft(&posted), None);
    // Only the later comment goes out, in a comment-only post.
    let posts = f.http.posts();
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert!(posts[0].get("labels").is_none());
    assert_eq!(
        posts[0]["comments"]["src/a.rs"][0]["message"],
        "written later"
    );
    assert!(posts[0]["comments"].get(PATCHSET_LEVEL_PATH).is_none());
    assert_eq!(f.load_draft(&later), None);
    f.assert_nothing_local(&[]);
}

#[test]
fn two_runs_joined_on_one_thread_do_not_both_post() {
    let a = fx();
    let (_, v) = a.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    a.review(&v[0], &[("Code-Review", 1)], "");
    let b = fx_on(a.syncer.store.clone(), a.syncer.server, quick());
    for f in [&a, &b] {
        f.http
            .yielding
            .store(true, std::sync::atomic::Ordering::SeqCst);
        f.gerrit("NEW", &["s1"], json!([]));
        f.posts_apply(json!({"Code-Review": 1}), 1);
    }
    let options = ConsolidateOptions::default();
    let (ra, rb) = block_on(futures::future::join(
        a.syncer.consolidate(&options),
        b.syncer.consolidate(&options),
    ));
    assert_eq!(u8::from(ra.ran) + u8::from(rb.ran), 1, "{ra:?} {rb:?}");
    assert_eq!(a.http.posts().len() + b.http.posts().len(), 1);
}

#[test]
fn recovery_counts_an_unchanged_vote_the_change_shows() {
    let f = fx();
    let (_, _, r, tag) = interrupted(&f);
    // Gerrit leaves a vote that did not change out of the message; the
    // change's votes (current patch set) show it.
    let mut c = change_json(
        "NEW",
        &["s1"],
        json!([{"id": "m1", "author": {"_account_id": ME}, "date": "2024-01-03 00:00:00.000000000",
                "message": "Patch Set 1:\n\n(2 comments)", "tag": tag, "_revision_number": 1}]),
    );
    c["labels"]["Code-Review"]["all"] = json!([{"_account_id": ME, "value": 2}]);
    f.gerrit_json(c);
    f.run();
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Sent);
    assert_eq!(f.http.posts().len(), 1);
}

#[test]
fn votes_gerrit_drops_after_a_new_patch_set_are_held_too() {
    let f = fx();
    let (_, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let r = f.review(&v[0], &[("Code-Review", 2), ("Verified", 1)], "");
    f.gerrit("NEW", &["s1"], json!([]));
    f.posts_apply(json!({"Code-Review": 2}), 2);
    f.run();
    let r = f.reloaded(&r);
    assert_eq!(r.state, PendingReviewState::Held);
    assert_eq!(r.labels.len(), 2);
    assert!(
        r.held_reason
            .unwrap()
            .contains("Gerrit did not apply Verified +1")
    );
}

// ---- votes of a ported review ----

/// Gerrit's change 7 was created at 2024-01-01T00:00:00Z: votes given six
/// minutes before were purely local, four minutes before were not (the
/// clock skew margin is five minutes).
const PURELY_LOCAL: &str = "2023-12-31T23:54:00Z";
const STRICT: &str = "2023-12-31T23:56:00Z";

impl Fx {
    /// Moves review `r` of local version `v` onto Gerrit's PS `ps` with
    /// commit `sha`, as a port does (not exact).
    fn port_review(
        &self,
        row: &LocalChange,
        v: &LocalPatchSet,
        r: &PendingReview,
        ps: u32,
        sha: &str,
    ) {
        let rewrite = ConsolidationRewrite {
            local_change: row.id,
            gerrit_number: CHANGE,
            moves: vec![PatchSetMove {
                from: v.number,
                to: PatchSetRef {
                    number: PatchSetNumber(ps),
                    sha: sha.into(),
                },
                exact: false,
                drafts: Vec::new(),
                reviews: vec![r.id],
            }],
            keep: Vec::new(),
            review_states: Vec::new(),
            state: None,
            else_state: None,
        };
        let report = self
            .syncer
            .store()
            .rewrite_local_change(self.syncer.server, &rewrite)
            .unwrap()
            .unwrap();
        assert_eq!(report.reviews_moved, 1);
    }
}

/// What became of a review with Code-Review +2 (an approval) and Verified
/// -1 given on local commit `l1` at `voted_at` under `snapshot`, ported onto
/// PS `target` of Gerrit's patch sets `shas` (the last current), with the
/// project's setting `setting`: the votes posted (`Null` for none), and the
/// review.
fn ported_votes(
    snapshot: LocalApproval,
    setting: Option<LocalApproval>,
    voted_at: &str,
    shas: &[&str],
    target: u32,
) -> (Value, PendingReview) {
    let f = fx();
    let (row, v) = f.local(CID, Some("main"), Some(CHANGE), &["l1"]);
    let r = f.review_with(
        &v[0],
        &[("Code-Review", 2), ("Verified", -1)],
        "carried over",
        snapshot,
        ts(voted_at),
    );
    f.port_review(&row, &v[0], &r, target, shas[target as usize - 1]);
    f.gerrit("NEW", shas, json!([]));
    let current = u32::try_from(shas.len()).unwrap();
    f.posts_apply(json!({"Code-Review": 2, "Verified": -1}), current);
    let options = ConsolidateOptions {
        local_approval: setting
            .map(|s| BTreeMap::from([("p".to_owned(), s)]))
            .unwrap_or_default(),
        ..Default::default()
    };
    let report = f.run_with(&options);
    assert!(
        report.stopped.is_none() && report.errors.is_empty(),
        "{report:?}"
    );
    // The message is posted whatever happens to the votes, on the target.
    let posts = f.http.posts();
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert!(
        f.http.post_targets()[0]
            .contains(&format!("/revisions/{}/review", shas[target as usize - 1])),
        "{:?}",
        f.http.post_targets()
    );
    assert_eq!(
        posts[0]["comments"][PATCHSET_LEVEL_PATH][0]["message"],
        "carried over"
    );
    f.assert_nothing_local(&["l1"]);
    let labels = posts[0].get("labels").cloned().unwrap_or(Value::Null);
    (labels, f.reloaded(&r))
}

fn both_votes() -> BTreeMap<String, i32> {
    BTreeMap::from([("Code-Review".to_owned(), 2), ("Verified".to_owned(), -1)])
}

/// Held with both votes kept; `drop_only` as given.
fn assert_held(r: &PendingReview, drop_only: &[&str]) {
    assert_eq!(r.state, PendingReviewState::Held, "{r:?}");
    assert_eq!(r.labels, both_votes());
    let expected: BTreeSet<String> = drop_only.iter().map(|l| (*l).to_owned()).collect();
    assert_eq!(r.drop_only, expected, "{:?}", r.held_reason);
    assert!(
        r.held_reason
            .as_deref()
            .is_some_and(|h| h.contains("local commit l1"))
    );
}

#[test]
fn a_ported_review_of_a_change_already_on_gerrit_follows_the_strict_rule() {
    // Given four minutes before Gerrit's change was created: within the
    // margin, so the change counts as on Gerrit, whatever the strategy.
    for strategy in LocalApproval::ALL {
        let (labels, r) = ported_votes(strategy, Some(strategy), STRICT, &["s1"], 1);
        assert_eq!(labels, Value::Null, "{strategy:?}");
        // The approval only drops; Verified -1 may be published.
        assert_held(&r, &["Code-Review"]);
        let reason = r.held_reason.unwrap();
        assert!(reason.contains("not moved"), "{reason}");
        assert!(
            reason.contains("Code-Review +2: an approval goes only onto the commit")
                && reason.contains("Verified -1: publish onto the current patch set"),
            "{reason}"
        );
    }
}

#[test]
fn a_ported_purely_local_review_under_follow_casts_every_vote_on_the_current_patch_set() {
    let (labels, r) = ported_votes(
        LocalApproval::Follow,
        Some(LocalApproval::Follow),
        PURELY_LOCAL,
        &["s1"],
        1,
    );
    assert_eq!(labels, json!({"Code-Review": 2, "Verified": -1}));
    assert_eq!(r.state, PendingReviewState::Sent);
}

#[test]
fn a_ported_review_under_follow_holds_its_votes_on_a_patch_set_that_is_not_current() {
    let (labels, r) = ported_votes(
        LocalApproval::Follow,
        Some(LocalApproval::Follow),
        PURELY_LOCAL,
        &["s1", "s2"],
        1,
    );
    assert_eq!(labels, Value::Null);
    // Publishable, approval and all.
    assert_held(&r, &[]);
    let reason = r.held_reason.unwrap();
    assert!(
        reason.contains("PS 1 is not the current patch set on Gerrit (PS 2 is)"),
        "{reason}"
    );
}

#[test]
fn a_ported_review_under_exact_or_manual_holds_every_vote_publishable() {
    let (labels, r) = ported_votes(
        LocalApproval::ExactOrManual,
        Some(LocalApproval::ExactOrManual),
        PURELY_LOCAL,
        &["s1"],
        1,
    );
    assert_eq!(labels, Value::Null);
    assert_held(&r, &[]);
    let reason = r.held_reason.unwrap();
    assert!(reason.contains("only onto the exact commit"), "{reason}");
    assert!(
        reason.contains("Code-Review +2, Verified -1: publish onto the current patch set"),
        "{reason}"
    );
}

#[test]
fn a_ported_review_under_exact_holds_its_approval_drop_only() {
    let (labels, r) = ported_votes(
        LocalApproval::Exact,
        Some(LocalApproval::Exact),
        PURELY_LOCAL,
        &["s1"],
        1,
    );
    assert_eq!(labels, Value::Null);
    assert_held(&r, &["Code-Review"]);
}

#[test]
fn the_stricter_of_snapshot_and_setting_decides_for_a_ported_review() {
    use LocalApproval::{Exact, ExactOrManual, Follow};
    // (snapshot, setting now, the drop-only labels of the held review)
    let cases: [(LocalApproval, Option<LocalApproval>, &[&str]); 5] = [
        (Follow, Some(Exact), &["Code-Review"]),
        (Exact, Some(Follow), &["Code-Review"]),
        (Follow, None, &["Code-Review"]),
        (ExactOrManual, Some(Follow), &[]),
        (Follow, Some(ExactOrManual), &[]),
    ];
    for (snapshot, setting, drop_only) in cases {
        let (labels, r) = ported_votes(snapshot, setting, PURELY_LOCAL, &["s1"], 1);
        assert_eq!(labels, Value::Null, "{snapshot:?} {setting:?}");
        assert_held(&r, drop_only);
    }
}

#[test]
fn a_held_ported_vote_supersedes_nothing_and_a_cast_one_does() {
    // An older review on the pushed commit s1 (exact, current) and a newer
    // one ported from l2 onto it, both on Code-Review.
    let run = |newer_strategy: LocalApproval, newer_voted: &str| {
        let f = fx();
        let (row, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1", "l2"]);
        let older = f.review(&v[0], &[("Code-Review", 1)], "older");
        let newer = f.review_with(
            &v[1],
            &[("Code-Review", 2)],
            "newer",
            newer_strategy,
            ts(newer_voted),
        );
        f.port_review(&row, &v[1], &newer, 1, "s1");
        f.gerrit("NEW", &["s1"], json!([]));
        // Every post's votes apply.
        f.http.respond("/review", |posts| {
            let labels = posts.last().and_then(|p| p.get("labels")).cloned();
            json!({"labels": labels.unwrap_or(json!({})),
                   "change_info": {"current_revision_number": 1}})
        });
        let options = ConsolidateOptions {
            local_approval: BTreeMap::from([("p".to_owned(), newer_strategy)]),
            ..Default::default()
        };
        f.run_with(&options);
        let labels: Vec<Value> = f
            .http
            .posts()
            .iter()
            .map(|p| p.get("labels").cloned().unwrap_or(Value::Null))
            .collect();
        (labels, f.reloaded(&older), f.reloaded(&newer))
    };
    // Held (the strict rule): the older +1 is still cast.
    let (labels, older, newer) = run(LocalApproval::Follow, STRICT);
    assert_eq!(labels, [json!({"Code-Review": 1}), Value::Null]);
    assert_eq!(older.state, PendingReviewState::Sent);
    assert_eq!(newer.state, PendingReviewState::Held);
    // Cast (follow, purely local): the older review loses its vote.
    let (labels, older, newer) = run(LocalApproval::Follow, PURELY_LOCAL);
    assert_eq!(labels, [Value::Null, json!({"Code-Review": 2})]);
    assert_eq!(older.state, PendingReviewState::Sent);
    assert!(older.labels.is_empty());
    assert_eq!(newer.state, PendingReviewState::Sent);
}

#[test]
fn recovery_of_a_ported_review_whose_votes_were_held_holds_them_again() {
    let f = fx();
    let (row, v) = f.local(CID, Some("main"), Some(CHANGE), &["l1"]);
    let r = f.review_with(
        &v[0],
        &[("Code-Review", 2)],
        "msg",
        LocalApproval::Follow,
        ts(STRICT),
    );
    f.port_review(&row, &v[0], &r, 1, "s1");
    f.gerrit("NEW", &["s1"], json!([]));
    f.http
        .queue("POST /changes/7/revisions/", 0, "connection reset");
    assert!(f.run().stopped.is_some());
    let tag = first_tag(&f);
    assert!(f.http.posts()[0].get("labels").is_none());
    // It arrived, without votes, as meant: held, not failed.
    f.gerrit(
        "NEW",
        &["s1"],
        json!([{"id": "m1", "author": {"_account_id": ME}, "date": "2024-01-03 00:00:00.000000000",
                "message": "Patch Set 1:\n\n(1 comment)", "tag": tag, "_revision_number": 1}]),
    );
    let report = f.run();
    assert!(report.stopped.is_none(), "{report:?}");
    assert_eq!(f.http.posts().len(), 1, "not posted again");
    let r = f.reloaded(&r);
    assert_eq!(r.state, PendingReviewState::Held);
    assert_eq!(r.drop_only, BTreeSet::from(["Code-Review".to_owned()]));
}

// ---- porting ----

use crate::{HasCheckout, NotLocalPath, PathMap, PortCommit, PortError, PortMaps, PortSource};
use model::{CommentRange, LineMap};

/// A checkout: commits with their parents, and line maps per pair of
/// commits. Records every `line_maps` call.
#[derive(Default)]
struct FakePort {
    commits: BTreeMap<String, Vec<String>>,
    maps: BTreeMap<(String, String), PortMaps>,
    calls: Mutex<Vec<(String, String, Vec<String>)>>,
    /// What `has_checkout` answers; `None`: yes. Calls fail with
    /// `NoCheckout` unless it is yes.
    checkout: Option<HasCheckout>,
    /// `line_maps` fails as git would.
    git_fails: bool,
}

impl FakePort {
    /// Commits `l1`, `s1` and `s2` on `base`, and `base` itself.
    fn new() -> Self {
        let mut p = Self::default();
        for c in ["l1", "s1", "s2"] {
            p.commits.insert(c.into(), vec!["base".into()]);
        }
        p.commits.insert("base".into(), Vec::new());
        p
    }

    fn map(mut self, from: &str, to: &str, path: &str, map: PathMap) -> Self {
        self.maps
            .entry((from.into(), to.into()))
            .or_default()
            .insert(path.into(), map);
        self
    }

    fn without(mut self, commit: &str) -> Self {
        self.commits.remove(commit);
        self
    }

    fn options(self) -> (Arc<Self>, ConsolidateOptions) {
        let port = Arc::new(self);
        let options = ConsolidateOptions {
            port: Some(port.clone() as Arc<dyn PortSource>),
            ..Default::default()
        };
        (port, options)
    }
}

impl PortSource for FakePort {
    fn has_checkout(&self, project: &str) -> HasCheckout {
        assert_eq!(project, "p");
        self.checkout.unwrap_or(HasCheckout::Yes)
    }

    fn commit(&self, project: &str, sha: &RevisionId) -> Result<Option<PortCommit>, PortError> {
        if self.has_checkout(project) != HasCheckout::Yes {
            return Err(PortError::NoCheckout);
        }
        Ok(self.commits.get(sha.as_str()).map(|parents| PortCommit {
            parents: parents.iter().map(|p| p.as_str().into()).collect(),
        }))
    }

    fn line_maps(
        &self,
        project: &str,
        from: &RevisionId,
        to: &RevisionId,
        paths: &[String],
    ) -> Result<PortMaps, PortError> {
        assert_eq!(project, "p");
        self.calls
            .lock()
            .unwrap()
            .push((from.to_string(), to.to_string(), paths.to_vec()));
        if self.has_checkout(project) != HasCheckout::Yes {
            return Err(PortError::NoCheckout);
        }
        if self.git_fails {
            return Err(PortError::Git("git cat-file failed".into()));
        }
        for c in [from, to] {
            if !self.commits.contains_key(c.as_str()) {
                return Err(PortError::MissingCommit(c.clone()));
            }
        }
        let maps = self.maps.get(&(from.to_string(), to.to_string()));
        Ok(paths
            .iter()
            .filter_map(|p| {
                if from == to {
                    return Some((p.clone(), PathMap::Lines(LineMap::identity(p.clone(), 100))));
                }
                Some((p.clone(), maps?.get(p)?.clone()))
            })
            .collect())
    }
}

/// Line 7 is line 9 now (two lines inserted above); line 3 changed.
fn shifted(path: &str) -> PathMap {
    let mut map: Vec<Option<u32>> = (1..=20).map(|l| Some(l + 2)).collect();
    map[2] = None;
    PathMap::Lines(LineMap::mapped(path, map))
}

/// Gerrit's files of PS 1 (`s1`) and PS 2 (`s2`) against their parent.
fn route_files(f: &Fx) {
    let files = json!({
        "/COMMIT_MSG": {"status": "A"},
        "src/a.rs": {"lines_inserted": 2},
        "src/new.rs": {"status": "R", "old_path": "src/old.rs"},
        "del.rs": {"status": "D"},
        "gone.rs": {"status": "D"},
        "logo.png": {"binary": true},
    })
    .to_string();
    for sha in ["s1", "s2"] {
        f.http
            .routes
            .route_json(&format!("/revisions/{sha}/files"), &files);
    }
}

fn input(path: &str, side: Side, location: Location, message: &str) -> DraftInput {
    DraftInput {
        path: path.into(),
        side,
        parent: None,
        location,
        in_reply_to: None,
        message: message.into(),
        unresolved: false,
    }
}

impl Fx {
    /// A local change on Gerrit change 7 (known, or found by the lookup
    /// when `known` is false) with versions `(sha, parent)`, in the order
    /// they were seen.
    fn local_on(
        &self,
        known: bool,
        versions: &[(&str, &str)],
    ) -> (LocalChange, Vec<LocalPatchSet>) {
        let mut store = self.syncer.store();
        let upsert = LocalChangeUpsert {
            seen: None,
            branch: Some("main".into()),
            change_id: CID.into(),
            gerrit_number: known.then_some(CHANGE),
            subject: "local".into(),
            author_name: "Me".into(),
            author_email: "me@example.com".into(),
            updated: ts("2024-01-01T00:00:00Z"),
            state: None,
            patch_sets: versions
                .iter()
                .map(|(sha, parent)| NewLocalPatchSet {
                    sha: (*sha).into(),
                    parent_sha: Some((*parent).into()),
                    seen_at: ts("2023-12-01T00:00:00Z"),
                })
                .collect(),
        };
        store
            .reconcile_local(
                self.syncer.server,
                "p",
                &[LocalOp::Upsert(upsert)],
                ts("2023-12-01T00:00:00Z"),
            )
            .unwrap();
        let row = store
            .local_changes_by_change_id(self.syncer.server, CID)
            .unwrap()
            .remove(0);
        let versions = store.local_patch_sets(self.syncer.server, row.id).unwrap();
        (row, versions)
    }

    fn draft_with(&self, row: &LocalChange, v: &LocalPatchSet, input: DraftInput) -> Draft {
        self.syncer
            .save_draft_local(DraftSave::Create {
                change: row.gerrit_number.unwrap_or(row.number),
                patch_set: PatchSetRef {
                    number: v.number,
                    sha: v.sha.clone(),
                },
                input,
            })
            .unwrap()
            .unwrap()
    }

    /// What became of draft `d`.
    fn fate(&self, d: &Draft) -> Fate {
        match self.load_draft(d) {
            None => {
                let posts = self.http.posts();
                let found = posts.iter().find_map(|p| {
                    p["comments"].as_object()?.iter().find_map(|(path, list)| {
                        list.as_array()?
                            .iter()
                            .find(|c| c["message"] == d.message.as_str())
                            .map(|c| {
                                let mut c = c.clone();
                                c["path"] = json!(path);
                                c
                            })
                    })
                });
                Fate::Posted(found.expect("a draft that is gone was posted"))
            }
            Some(now) => match now.held_reason {
                Some(why) => Fate::Held(why),
                None => Fate::Stayed(Box::new(now)),
            },
        }
    }
}

#[derive(Debug)]
enum Fate {
    Posted(Value),
    Held(String),
    Stayed(Box<Draft>),
}

impl Fate {
    fn posted(self) -> Value {
        match self {
            Fate::Posted(c) => c,
            other => panic!("not posted: {other:?}"),
        }
    }

    fn held(self) -> String {
        match self {
            Fate::Held(why) => why,
            other => panic!("not held: {other:?}"),
        }
    }
}

/// Local version `l1` (amended, never pushed) and `s1` (pushed as PS 1,
/// seen later) of Gerrit change 7, a draft `input` on `l1`, and a run with
/// `port` (PS 1 is current): what became of the draft.
fn port_one(port: FakePort, input: DraftInput) -> (Fx, Fate) {
    let f = fx();
    let (row, v) = f.local_on(true, &[("l1", "base"), ("s1", "base")]);
    let d = f.draft_with(&row, &v[0], input);
    f.gerrit("NEW", &["s1"], json!([]));
    route_files(&f);
    f.posts_apply(json!({}), 1);
    let (_, options) = port.options();
    let report = f.run_with(&options);
    assert!(
        report.stopped.is_none() && report.errors.is_empty(),
        "{report:?}"
    );
    f.assert_nothing_local(&["l1"]);
    let fate = f.fate(&d);
    (f, fate)
}

#[test]
fn porting_moves_a_comment_on_an_unchanged_line() {
    let port = FakePort::new().map("l1", "s1", "src/a.rs", shifted("src/a.rs"));
    let (f, fate) = port_one(
        port,
        input("src/a.rs", Side::Revision, Location::Line(7), "moved"),
    );
    let c = fate.posted();
    assert_eq!(c["path"], "src/a.rs");
    assert_eq!(c["line"], 9);
    assert_eq!(c["side"], "REVISION");
    // Posted inline on PS 1, not on the local commit.
    assert!(f.http.post_targets()[0].contains("/changes/7/revisions/s1/review"));
}

#[test]
fn porting_holds_a_comment_on_a_changed_line() {
    let port = FakePort::new().map("l1", "s1", "src/a.rs", shifted("src/a.rs"));
    let (f, fate) = port_one(
        port,
        input("src/a.rs", Side::Revision, Location::Line(3), "x"),
    );
    let why = fate.held();
    assert!(why.contains("Line 3 of src/a.rs changed in PS 1"), "{why}");
    // Nothing to post.
    assert!(f.http.posts().is_empty());
}

#[test]
fn porting_holds_a_range_crossing_a_change_and_moves_one_inside_unchanged_lines() {
    let range = |start_line, end_line| {
        Location::Range(CommentRange {
            start_line,
            start_character: 1,
            end_line,
            end_character: 4,
        })
    };
    let port = || FakePort::new().map("l1", "s1", "src/a.rs", shifted("src/a.rs"));
    let (_, fate) = port_one(port(), input("src/a.rs", Side::Revision, range(2, 4), "x"));
    let why = fate.held();
    assert!(why.contains("commented lines of src/a.rs changed"), "{why}");
    let (_, fate) = port_one(port(), input("src/a.rs", Side::Revision, range(5, 6), "x"));
    let c = fate.posted();
    assert_eq!(
        c["range"],
        json!({"start_line": 7, "start_character": 1, "end_line": 8, "end_character": 4})
    );
}

#[test]
fn porting_follows_a_rename() {
    let port = FakePort::new().map(
        "l1",
        "s1",
        "src/old.rs",
        PathMap::Lines(LineMap::identity("src/new.rs", 20)),
    );
    let (_, fate) = port_one(
        port,
        input("src/old.rs", Side::Revision, Location::Line(4), "x"),
    );
    let c = fate.posted();
    assert_eq!(
        (c["path"].as_str(), c["line"].as_u64()),
        (Some("src/new.rs"), Some(4))
    );
}

#[test]
fn porting_holds_comments_on_a_deleted_file() {
    for location in [Location::Line(1), Location::File] {
        let port = FakePort::new().map("l1", "s1", "gone.rs", PathMap::Lines(LineMap::file_gone()));
        let (_, fate) = port_one(port, input("gone.rs", Side::Revision, location, "x"));
        let why = fate.held();
        assert!(why.contains("gone.rs is deleted in PS 1"), "{why}");
    }
}

#[test]
fn porting_moves_a_file_comment_on_a_file_both_delete() {
    let deleted = || {
        FakePort::new().map(
            "l1",
            "s1",
            "del.rs",
            PathMap::NotLocal(NotLocalPath::NotAFile),
        )
    };
    let (_, fate) = port_one(
        deleted(),
        input("del.rs", Side::Revision, Location::File, "x"),
    );
    let c = fate.posted();
    assert_eq!(c["path"], "del.rs");
    assert!(c.get("line").is_none());
    // PS 1 does not delete it: held.
    let port = FakePort::new().map(
        "l1",
        "s1",
        "kept.rs",
        PathMap::NotLocal(NotLocalPath::NotAFile),
    );
    let (_, fate) = port_one(port, input("kept.rs", Side::Revision, Location::File, "x"));
    let why = fate.held();
    assert!(why.contains("does not delete it"), "{why}");
    // A line comment cannot be on a file the local version deletes.
    let (_, fate) = port_one(
        deleted(),
        input("del.rs", Side::Revision, Location::Line(2), "x"),
    );
    fate.held();
}

#[test]
fn porting_always_moves_a_patch_set_level_comment() {
    // Even without any map, and without asking the checkout about it.
    let (port, options) = FakePort::new().options();
    let f = fx();
    let (row, v) = f.local_on(true, &[("l1", "base"), ("s1", "base")]);
    let d = f.draft_with(
        &row,
        &v[0],
        input(
            PATCHSET_LEVEL_PATH,
            Side::Revision,
            Location::File,
            "overall",
        ),
    );
    f.gerrit("NEW", &["s1"], json!([]));
    route_files(&f);
    f.posts_apply(json!({}), 1);
    f.run_with(&options);
    let c = f.fate(&d).posted();
    assert_eq!(c["path"], PATCHSET_LEVEL_PATH);
    assert!(port.calls.lock().unwrap().is_empty());
    // Nor Gerrit about the patch set's files.
    assert!(
        !f.http
            .routes
            .requested()
            .iter()
            .any(|r| r.contains("/files")),
        "{:?}",
        f.http.routes.requested()
    );
}

#[test]
fn porting_maps_the_commit_message() {
    let port = FakePort::new().map("l1", "s1", "/COMMIT_MSG", shifted("/COMMIT_MSG"));
    let (_, fate) = port_one(
        port,
        input("/COMMIT_MSG", Side::Revision, Location::Line(8), "x"),
    );
    let c = fate.posted();
    assert_eq!(
        (c["path"].as_str(), c["line"].as_u64()),
        (Some("/COMMIT_MSG"), Some(10))
    );
    let port = FakePort::new().map("l1", "s1", "/COMMIT_MSG", shifted("/COMMIT_MSG"));
    let (_, fate) = port_one(
        port,
        input("/COMMIT_MSG", Side::Revision, Location::Line(3), "x"),
    );
    fate.held();
}

#[test]
fn porting_moves_a_parent_side_comment_only_when_the_parent_is_unchanged() {
    // Same parent (`base` on both sides): the base side is the same.
    let (port, options) = FakePort::new().options();
    let f = fx();
    let (row, v) = f.local_on(true, &[("l1", "base"), ("s1", "base")]);
    let d = f.draft_with(
        &row,
        &v[0],
        input("src/a.rs", Side::Parent, Location::Line(5), "base"),
    );
    f.gerrit("NEW", &["s1"], json!([]));
    route_files(&f);
    f.posts_apply(json!({}), 1);
    f.run_with(&options);
    let c = f.fate(&d).posted();
    assert_eq!(
        (c["side"].as_str(), c["line"].as_u64()),
        (Some("PARENT"), Some(5))
    );
    // The same base on both sides: no line maps needed at all.
    assert!(port.calls.lock().unwrap().is_empty());

    // Rebased onto `base2`, where src/a.rs differs: held. Where it is the
    // same, the comment moves.
    for (map, moves) in [
        (shifted("src/a.rs"), false),
        (PathMap::Lines(LineMap::identity("src/a.rs", 30)), true),
    ] {
        let mut port = FakePort::new().map("base", "base2", "src/a.rs", map);
        port.commits.insert("s1".into(), vec!["base2".into()]);
        port.commits.insert("base2".into(), Vec::new());
        let (_, options) = port.options();
        let f = fx();
        let (row, v) = f.local_on(true, &[("l1", "base"), ("s1", "base2")]);
        let d = f.draft_with(
            &row,
            &v[0],
            input("src/a.rs", Side::Parent, Location::Line(5), "b"),
        );
        f.gerrit("NEW", &["s1"], json!([]));
        route_files(&f);
        f.posts_apply(json!({}), 1);
        f.run_with(&options);
        match f.fate(&d) {
            Fate::Posted(c) if moves => assert_eq!(c["line"], 5),
            Fate::Held(why) if !moves => {
                assert!(
                    why.contains("The base of src/a.rs differs in PS 1"),
                    "{why}"
                )
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn porting_holds_what_the_checkout_cannot_read() {
    let port = FakePort::new().map(
        "l1",
        "s1",
        "src/a.rs",
        PathMap::NotLocal(NotLocalPath::MissingContent),
    );
    let (_, fate) = port_one(
        port,
        input("src/a.rs", Side::Revision, Location::Line(7), "x"),
    );
    let why = fate.held();
    assert!(
        why.contains("content of src/a.rs is not in the checkout"),
        "{why}"
    );

    // The amended commit is gone from the checkout (after a `git gc`).
    let port = FakePort::new().without("l1");
    let (_, fate) = port_one(
        port,
        input("src/a.rs", Side::Revision, Location::Line(7), "x"),
    );
    let why = fate.held();
    assert!(why.contains("commit l1 is not in the checkout"), "{why}");

    // A binary file that changed, and a file PS 1 does not change (Gerrit
    // would refuse the whole post).
    let port = FakePort::new().map(
        "l1",
        "s1",
        "logo.png",
        PathMap::Lines(LineMap::opaque("logo.png")),
    );
    let (_, fate) = port_one(
        port,
        input("logo.png", Side::Revision, Location::Line(1), "x"),
    );
    let why = fate.held();
    assert!(why.contains("binary file"), "{why}");
    let port = FakePort::new().map(
        "l1",
        "s1",
        "lib.rs",
        PathMap::Lines(LineMap::identity("lib.rs", 9)),
    );
    let (_, fate) = port_one(
        port,
        input("lib.rs", Side::Revision, Location::Line(1), "x"),
    );
    let why = fate.held();
    assert!(why.contains("PS 1 does not change lib.rs"), "{why}");
}

#[test]
fn without_a_port_source_or_a_checkout_nothing_is_ported_or_held() {
    for options in [
        ConsolidateOptions::default(),
        FakePort {
            checkout: Some(HasCheckout::NotNow),
            ..FakePort::new()
        }
        .options()
        .1,
    ] {
        let f = fx();
        let (row, v) = f.local_on(true, &[("l1", "base"), ("s1", "base")]);
        let d = f.draft_with(
            &row,
            &v[0],
            input("src/a.rs", Side::Revision, Location::Line(7), "x"),
        );
        let r = f.review(&v[0], &[("Code-Review", 1)], "msg");
        f.gerrit("NEW", &["s1"], json!([]));
        route_files(&f);
        let report = f.run_with(&options);
        assert!(
            report.errors.is_empty() && report.ported.is_empty(),
            "{report:?}"
        );
        assert!(f.http.posts().is_empty());
        let Fate::Stayed(now) = f.fate(&d) else {
            panic!("moved or held");
        };
        assert_eq!(now.patch_set, v[0].number);
        let r = f.reloaded(&r);
        assert_eq!(
            (r.state, r.patch_set),
            (PendingReviewState::Pending, v[0].number)
        );
    }
}

#[test]
fn a_newer_version_that_is_not_on_gerrit_yet_waits_for_its_own_patch_set() {
    // s1 is PS 1; l2 was amended after it and not pushed (yet).
    let (_, options) = FakePort::new()
        .map("l2", "s1", "src/a.rs", shifted("src/a.rs"))
        .options();
    let f = fx();
    let (row, v) = f.local_on(true, &[("s1", "base"), ("l2", "base")]);
    let d = f.draft_with(
        &row,
        &v[1],
        input("src/a.rs", Side::Revision, Location::Line(7), "x"),
    );
    f.gerrit("NEW", &["s1"], json!([]));
    route_files(&f);
    let report = f.run_with(&options);
    assert!(report.ported.is_empty(), "{report:?}");
    assert!(matches!(f.fate(&d), Fate::Stayed(_)));
    assert!(f.http.posts().is_empty());
}

#[test]
fn the_port_goes_to_the_current_patch_set_when_the_checkout_has_it() {
    let port = FakePort::new().map("l1", "s2", "src/a.rs", shifted("src/a.rs"));
    let (_, options) = port.options();
    let f = fx();
    let (row, v) = f.local_on(true, &[("l1", "base"), ("s1", "base")]);
    let d = f.draft_with(
        &row,
        &v[0],
        input("src/a.rs", Side::Revision, Location::Line(7), "x"),
    );
    f.gerrit("NEW", &["s1", "s2"], json!([]));
    route_files(&f);
    f.posts_apply(json!({}), 2);
    let report = f.run_with(&options);
    assert_eq!(f.fate(&d).posted()["line"], 9);
    assert!(f.http.post_targets()[0].contains("/revisions/s2/review"));
    assert_eq!(report.ported.len(), 1);
    assert_eq!(report.ported[0].to, Some(PatchSetNumber(2)));
}

#[test]
fn the_port_goes_to_the_newest_patch_set_in_the_checkout_and_its_votes_wait() {
    // PS 2 was uploaded elsewhere and is not fetched: PS 1 it is.
    let port = FakePort::new()
        .without("s2")
        .map("l1", "s1", "src/a.rs", shifted("src/a.rs"));
    let (_, options) = port.options();
    let f = fx();
    let (row, v) = f.local_on(true, &[("l1", "base"), ("s1", "base")]);
    let d = f.draft_with(
        &row,
        &v[0],
        input("src/a.rs", Side::Revision, Location::Line(7), "x"),
    );
    let r = f.review_with(
        &v[0],
        &[("Code-Review", 2)],
        "msg",
        LocalApproval::Follow,
        ts(PURELY_LOCAL),
    );
    f.gerrit("NEW", &["s1", "s2"], json!([]));
    route_files(&f);
    f.posts_apply(json!({}), 2);
    let options = ConsolidateOptions {
        local_approval: BTreeMap::from([("p".to_owned(), LocalApproval::Follow)]),
        ..options
    };
    f.run_with(&options);
    assert_eq!(f.fate(&d).posted()["line"], 9);
    assert!(f.http.post_targets()[0].contains("/revisions/s1/review"));
    assert!(f.http.posts()[0].get("labels").is_none());
    // Follow, but PS 1 is not current: held, publishable.
    let r = f.reloaded(&r);
    assert_eq!(
        (r.state, r.patch_set),
        (PendingReviewState::Held, PatchSetNumber(1))
    );
    assert!(r.drop_only.is_empty());
}

#[test]
fn with_no_patch_set_in_the_checkout_everything_is_held() {
    let port = FakePort::new().without("s1").without("s2");
    let (_, options) = port.options();
    let f = fx();
    // Only the amended version; the change vanished from the checkout.
    let (row, v) = f.local_on(true, &[("l1", "base")]);
    let d = f.draft_with(
        &row,
        &v[0],
        input("src/a.rs", Side::Revision, Location::Line(7), "x"),
    );
    let r = f.review(&v[0], &[("Code-Review", 2), ("Verified", -1)], "msg");
    f.syncer
        .store()
        .set_local_change_state(f.syncer.server, row.id, LocalChangeState::Vanished)
        .unwrap();
    f.gerrit("NEW", &["s1", "s2"], json!([]));
    route_files(&f);
    let report = f.run_with(&options);
    assert!(f.http.posts().is_empty());
    let why = f.fate(&d).held();
    assert_eq!(
        why,
        "No patch set of change 7 is in the checkout, so this could not be carried over from \
         the local version."
    );
    let r = f.reloaded(&r);
    assert_eq!(
        (r.state, r.patch_set),
        (PendingReviewState::Held, v[0].number)
    );
    // Not purely local (given now): the approval only drops.
    assert_eq!(r.drop_only, BTreeSet::from(["Code-Review".to_owned()]));
    let reason = r.held_reason.unwrap();
    assert!(
        reason.contains("Verified -1: publish onto the current patch set"),
        "{reason}"
    );
    assert_eq!(report.ported.len(), 1);
    assert_eq!(
        (report.ported[0].to, report.ported[0].held_reviews),
        (None, 1)
    );
    // Shown on the Gerrit change's held card.
    let items = f
        .syncer
        .store()
        .held_items(f.syncer.server, CHANGE)
        .unwrap();
    assert_eq!((items.drafts.len(), items.reviews.len()), (1, 1));
    assert!(report.changed_gerrit_changes().contains(&CHANGE));
}

#[test]
fn a_vanished_change_is_ported() {
    let port = FakePort::new().map("l1", "s1", "src/a.rs", shifted("src/a.rs"));
    let (_, options) = port.options();
    let f = fx();
    let (row, v) = f.local_on(true, &[("l1", "base")]);
    let d = f.draft_with(
        &row,
        &v[0],
        input("src/a.rs", Side::Revision, Location::Line(7), "x"),
    );
    f.syncer
        .store()
        .set_local_change_state(f.syncer.server, row.id, LocalChangeState::Vanished)
        .unwrap();
    f.gerrit("NEW", &["s1"], json!([]));
    route_files(&f);
    f.posts_apply(json!({}), 1);
    f.run_with(&options);
    assert_eq!(f.fate(&d).posted()["line"], 9);
}

#[test]
fn follow_casts_the_votes_even_when_some_comments_were_held() {
    let port = FakePort::new().map("l1", "s1", "src/a.rs", shifted("src/a.rs"));
    let (_, options) = port.options();
    let options = ConsolidateOptions {
        local_approval: BTreeMap::from([("p".to_owned(), LocalApproval::Follow)]),
        ..options
    };
    let f = fx();
    let (row, v) = f.local_on(true, &[("l1", "base"), ("s1", "base")]);
    let held = f.draft_with(
        &row,
        &v[0],
        input("src/a.rs", Side::Revision, Location::Line(3), "held"),
    );
    let r = f.review_with(
        &v[0],
        &[("Code-Review", 2)],
        "LGTM",
        LocalApproval::Follow,
        ts(PURELY_LOCAL),
    );
    f.gerrit("NEW", &["s1"], json!([]));
    route_files(&f);
    f.posts_apply(json!({"Code-Review": 2}), 1);
    f.run_with(&options);
    f.fate(&held).held();
    let posts = f.http.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["labels"], json!({"Code-Review": 2}));
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Sent);
}

#[test]
fn an_amended_version_is_ported_once_a_later_version_is_pushed() {
    // End to end: the user committed l1 and reviewed it locally, then
    // amended it to s1 (seen by the scanner) and pushed s1 as PS 1.
    let port = FakePort::new()
        .map("l1", "s1", "src/a.rs", shifted("src/a.rs"))
        .map(
            "l1",
            "s1",
            "/COMMIT_MSG",
            PathMap::Lines(LineMap::identity("/COMMIT_MSG", 12)),
        );
    let (port, options) = port.options();
    for (strategy, voted_at, cast) in [
        (LocalApproval::Follow, PURELY_LOCAL, true),
        (LocalApproval::Follow, STRICT, false),
        (LocalApproval::Exact, PURELY_LOCAL, false),
    ] {
        port.calls.lock().unwrap().clear();
        let options = ConsolidateOptions {
            local_approval: BTreeMap::from([("p".to_owned(), strategy)]),
            ..options.clone()
        };
        let f = fx();
        let (row, v) = f.local_on(false, &[("l1", "base"), ("s1", "base")]);
        let moved = f.draft_with(
            &row,
            &v[0],
            input("src/a.rs", Side::Revision, Location::Line(7), "moved"),
        );
        let held = f.draft_with(
            &row,
            &v[0],
            input("src/a.rs", Side::Revision, Location::Line(3), "held"),
        );
        let msg = f.draft_with(
            &row,
            &v[0],
            input("/COMMIT_MSG", Side::Revision, Location::Line(6), "subject"),
        );
        let r = f.review_with(
            &v[0],
            &[("Code-Review", 2), ("Verified", -1)],
            "LGTM",
            strategy,
            ts(voted_at),
        );
        f.query_finds(&[(7, "main", "NEW")]);
        f.gerrit("NEW", &["s1"], json!([]));
        route_files(&f);
        f.posts_apply(json!({"Code-Review": 2, "Verified": -1}), 1);
        let report = f.run_with(&options);
        assert!(
            report.stopped.is_none() && report.errors.is_empty(),
            "{report:?}"
        );
        assert_eq!(report.found.len(), 1);
        // One call for the pair of commits, with every path needed.
        assert_eq!(
            *port.calls.lock().unwrap(),
            [(
                "l1".to_owned(),
                "s1".to_owned(),
                vec!["/COMMIT_MSG".to_owned(), "src/a.rs".to_owned()]
            )]
        );
        // One post on PS 1: the ported comments and the message.
        let posts = f.http.posts();
        assert_eq!(posts.len(), 1, "{posts:?}");
        assert!(f.http.post_targets()[0].contains("/changes/7/revisions/s1/review"));
        assert_eq!(f.fate(&moved).posted()["line"], 9);
        assert_eq!(f.fate(&msg).posted()["line"], 6);
        assert_eq!(
            posts[0]["comments"][PATCHSET_LEVEL_PATH][0]["message"],
            "LGTM"
        );
        let why = f.fate(&held).held();
        assert!(why.contains("Line 3 of src/a.rs changed in PS 1"), "{why}");
        let r = f.reloaded(&r);
        if cast {
            assert_eq!(
                posts[0]["labels"],
                json!({"Code-Review": 2, "Verified": -1})
            );
            assert_eq!(r.state, PendingReviewState::Sent);
        } else {
            assert!(posts[0].get("labels").is_none());
            assert_eq!(r.state, PendingReviewState::Held);
            assert_eq!(r.drop_only, BTreeSet::from(["Code-Review".to_owned()]));
        }
        // The held draft keeps the change open; it shows on the held card.
        assert_eq!(f.row(&row).state, LocalChangeState::Local);
        let items = f
            .syncer
            .store()
            .held_items(f.syncer.server, CHANGE)
            .unwrap();
        assert_eq!(items.drafts.len(), 1);
        assert_eq!(report.ported.len(), 1);
        assert_eq!(report.ported[0].held.len(), 1);
        f.assert_nothing_local(&["l1"]);
        // A second run has nothing more to do.
        let again = f.run_with(&options);
        assert!(
            again.ported.is_empty() && f.http.posts().len() == 1,
            "{again:?}"
        );
    }
}

#[test]
fn a_version_on_no_named_branch_is_ported() {
    // s1 is PS 1; l2 was amended after it, commented on, and then the
    // branch was reset back to s1: l2 will never reach Gerrit.
    let mut port = FakePort::new().map("l2", "s1", "src/a.rs", shifted("src/a.rs"));
    port.commits.insert("l2".into(), vec!["base".into()]);
    let (_, options) = port.options();
    let f = fx();
    let (row, v) = f.local_on(true, &[("s1", "base"), ("l2", "base")]);
    let d = f.draft_with(
        &row,
        &v[1],
        input("src/a.rs", Side::Revision, Location::Line(7), "x"),
    );
    f.gerrit("NEW", &["s1"], json!([]));
    route_files(&f);
    f.posts_apply(json!({}), 1);
    // While l2 is on a branch it waits for its own patch set.
    f.run_with(&options);
    assert!(matches!(f.fate(&d), Fate::Stayed(_)));
    // A complete scan no longer sees it.
    f.syncer
        .store()
        .mark_local_versions_on_branch(f.syncer.server, "p", &["s1".into()])
        .unwrap();
    let report = f.run_with(&options);
    assert_eq!(report.ported.len(), 1, "{report:?}");
    assert_eq!(f.fate(&d).posted()["line"], 9);
    assert!(f.http.post_targets()[0].contains("/revisions/s1/review"));
    f.assert_nothing_local(&["l2"]);
    // Every version is done with (s1 on Gerrit, l2 ported) and nothing is
    // unsent: a tombstone, which keeps its Gerrit change for redirects.
    assert_eq!(report.consolidated, [(row.id, CHANGE)]);
    let now = f.row(&row);
    assert_eq!(
        (now.state, now.gerrit_number),
        (LocalChangeState::Consolidated, Some(CHANGE))
    );
}

#[test]
fn a_project_without_a_configured_checkout_has_its_versions_held() {
    let (_, options) = FakePort {
        checkout: Some(HasCheckout::NotConfigured),
        ..FakePort::new()
    }
    .options();
    let f = fx();
    let (row, v) = f.local_on(true, &[("l1", "base"), ("s1", "base")]);
    let d = f.draft_with(
        &row,
        &v[0],
        input("src/a.rs", Side::Revision, Location::Line(7), "x"),
    );
    let r = f.review(&v[0], &[("Code-Review", 1)], "msg");
    f.gerrit("NEW", &["s1"], json!([]));
    let report = f.run_with(&options);
    assert!(f.http.posts().is_empty());
    let why = f.fate(&d).held();
    assert!(
        why.starts_with("No checkout is configured for project p"),
        "{why}"
    );
    let r = f.reloaded(&r);
    assert_eq!(r.state, PendingReviewState::Held);
    assert!(r.drop_only.is_empty());
    assert_eq!(report.ported.len(), 1);
    assert_eq!(report.ported[0].to, None);
}

#[test]
fn a_checkout_that_cannot_be_used_now_is_not_asked_about_on_every_run() {
    // Opening it failed: nothing is held, nothing fetched for porting.
    let (_, options) = FakePort {
        checkout: Some(HasCheckout::NotNow),
        ..FakePort::new()
    }
    .options();
    let f = fx();
    let (row, v) = f.local_on(true, &[("l1", "base"), ("s1", "base")]);
    let d = f.draft_with(
        &row,
        &v[0],
        input("src/a.rs", Side::Revision, Location::Line(7), "x"),
    );
    f.gerrit("NEW", &["s1"], json!([]));
    route_files(&f);
    // The first run fetches the change once, to map s1 to PS 1.
    let report = f.run_with(&options);
    assert!(report.errors.is_empty(), "{report:?}");
    let fetches = f.detail_fetches();
    let report = f.run_with(&options);
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(f.detail_fetches(), fetches);
    assert!(matches!(f.fate(&d), Fate::Stayed(_)));
}

#[test]
fn git_failing_leaves_the_version_for_a_later_run() {
    let port = FakePort {
        git_fails: true,
        ..FakePort::new().map("l1", "s1", "src/a.rs", shifted("src/a.rs"))
    };
    let (_, options) = port.options();
    let f = fx();
    let (row, v) = f.local_on(true, &[("l1", "base"), ("s1", "base")]);
    let d = f.draft_with(
        &row,
        &v[0],
        input("src/a.rs", Side::Revision, Location::Line(7), "x"),
    );
    f.gerrit("NEW", &["s1"], json!([]));
    route_files(&f);
    f.posts_apply(json!({}), 1);
    let report = f.run_with(&options);
    assert_eq!(report.errors.len(), 1, "{report:?}");
    assert!(report.errors[0].1.contains("git cat-file failed"));
    assert!(matches!(f.fate(&d), Fate::Stayed(_)), "not held");
    let fetches = f.detail_fetches();
    // The next run does not try again (nor fetch the change for it)...
    let report = f.run_with(&options);
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(f.detail_fetches(), fetches);
    // ...until the slow re-check has passed; then it ports.
    let later = f.syncer.clone().with_consolidation_timing(eager());
    let (_, options) = FakePort::new()
        .map("l1", "s1", "src/a.rs", shifted("src/a.rs"))
        .options();
    block_on(later.consolidate(&options));
    assert_eq!(f.fate(&d).posted()["line"], 9);
}

#[test]
fn a_draft_written_during_a_run_is_ported_by_the_next() {
    let (_, options) = FakePort::new()
        .map("l1", "s1", "src/a.rs", shifted("src/a.rs"))
        .options();
    let f = fx();
    let (row, v) = f.local_on(true, &[("l1", "base"), ("s1", "base")]);
    let first = f.draft_with(
        &row,
        &v[0],
        input("src/a.rs", Side::Revision, Location::Line(7), "first"),
    );
    f.gerrit("NEW", &["s1"], json!([]));
    f.posts_apply(json!({}), 1);
    // While the run asks for the patch set's files, the user writes
    // another draft and saves a review on the amended version.
    let (syncer, version) = (f.syncer.clone(), v[0].clone());
    let written: Arc<Mutex<Option<(Draft, PendingReview)>>> = Arc::default();
    let out = written.clone();
    f.http.respond("/files", move |_| {
        let mut out = out.lock().unwrap();
        if out.is_none() {
            let d = syncer
                .save_draft_local(DraftSave::Create {
                    change: CHANGE,
                    patch_set: PatchSetRef {
                        number: version.number,
                        sha: version.sha.clone(),
                    },
                    input: input("src/a.rs", Side::Revision, Location::Line(8), "second"),
                })
                .unwrap()
                .unwrap();
            let r = syncer
                .create_pending_review(&NewPendingReview {
                    patch_set: version.number,
                    labels: BTreeMap::new(),
                    strategy: LocalApproval::Exact,
                    message: "written meanwhile".into(),
                    unresolved: false,
                    created: Timestamp::now(),
                    replaces_draft: None,
                })
                .unwrap();
            *out = Some((d, r));
        }
        json!({"src/a.rs": {"lines_inserted": 2}})
    });
    f.run_with(&options);
    assert_eq!(f.fate(&first).posted()["line"], 9);
    let (second, review) = written.lock().unwrap().clone().unwrap();
    // Not in the run's snapshot: it stayed where it was.
    assert!(matches!(f.fate(&second), Fate::Stayed(_)));
    assert_eq!(f.reloaded(&review).patch_set, v[0].number);
    let report = f.run_with(&options);
    assert!(report.stopped.is_none(), "{report:?}");
    assert_eq!(f.fate(&second).posted()["line"], 10);
    let review = f.reloaded(&review);
    assert_eq!(
        (review.state, review.patch_set),
        (PendingReviewState::Sent, PatchSetNumber(1))
    );
}

#[test]
fn ported_comments_interrupted_before_their_post_are_posted_later() {
    let setup = || {
        let f = fx();
        let (row, v) = f.local_on(true, &[("l1", "base"), ("s1", "base")]);
        let d = f.draft_with(
            &row,
            &v[0],
            input("src/a.rs", Side::Revision, Location::Line(7), "x"),
        );
        let r = f.review(&v[0], &[], "msg");
        f.gerrit("NEW", &["s1"], json!([]));
        route_files(&f);
        f.posts_apply(json!({}), 1);
        (f, d, r)
    };
    let (_, options) = FakePort::new()
        .map("l1", "s1", "src/a.rs", shifted("src/a.rs"))
        .options();

    // Stopped after the rewrite, before the post was claimed.
    let (f, d, r) = setup();
    let stopped = ConsolidateOptions {
        stop: StopFlag::default(),
        ..options.clone()
    };
    stopped.stop.stop();
    let report = f.run_with(&stopped);
    assert!(matches!(report.stopped, Some(Error::Stopped)), "{report:?}");
    assert!(f.http.posts().is_empty());
    let moved = f.load_draft(&d).unwrap();
    assert_eq!(
        (moved.patch_set, moved.location),
        (PatchSetNumber(1), Location::Line(9))
    );
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Pending);
    f.run_with(&options);
    assert_eq!(f.fate(&d).posted()["line"], 9);
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Sent);

    // The post was lost on the way: recovery posts it again, as ported.
    let (f, d, r) = setup();
    f.http
        .queue("POST /changes/7/revisions/", 0, "connection reset");
    assert!(f.run_with(&options).stopped.is_some());
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Sending);
    f.run_with(&options);
    let posts = f.http.posts();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[0], posts[1]);
    assert!(f.http.post_targets()[1].contains("/revisions/s1/review"));
    assert_eq!(f.fate(&d).posted()["line"], 9);
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Sent);
    f.assert_nothing_local(&["l1"]);
}

#[test]
fn port_draft_holds_replies_merges_and_a_renamed_base() {
    let f = fx();
    let (row, v) = f.local_on(true, &[("l1", "base")]);
    let mut reply = input("src/a.rs", Side::Revision, Location::Line(7), "reply");
    reply.in_reply_to = Some("thread".into());
    let mut d = f.draft_with(
        &row,
        &v[0],
        input("src/a.rs", Side::Revision, Location::Line(7), "x"),
    );
    let maps: port::SideMaps = Ok(PortMaps::from([(
        "src/a.rs".to_owned(),
        PathMap::Lines(LineMap::identity("src/a.rs", 20)),
    )]));
    let files = [model::FileEntry {
        path: "src/a.rs".into(),
        status: model::FileStatus::Modified,
        old_path: None,
        lines_inserted: 1,
        lines_deleted: 0,
        size_delta: 0,
        size: 0,
        binary: false,
    }];
    assert!(port::port_draft(&d, PatchSetNumber(1), &files, &maps, &maps).is_ok());
    d.in_reply_to = Some("thread".into());
    let why = port::port_draft(&d, PatchSetNumber(1), &files, &maps, &maps).unwrap_err();
    assert!(why.contains("reply"), "{why}");
    d.in_reply_to = None;

    // The base side of a file the local version renamed (the parents'
    // map says the path is not a file there).
    d.side = Side::Parent;
    let renamed: port::SideMaps = Ok(PortMaps::from([(
        "src/a.rs".to_owned(),
        PathMap::NotLocal(NotLocalPath::NotAFile),
    )]));
    let why = port::port_draft(&d, PatchSetNumber(1), &files, &maps, &renamed).unwrap_err();
    assert!(
        why.contains("is another file (renamed or added there)"),
        "{why}"
    );
    let _ = reply;
}

#[test]
fn base_side_comments_of_a_merge_are_held() {
    let mut port = FakePort::new();
    port.commits
        .insert("l1".into(), vec!["base".into(), "other".into()]);
    let (_, options) = port.options();
    let f = fx();
    let (row, v) = f.local_on(true, &[("l1", "base"), ("s1", "base")]);
    let d = f.draft_with(
        &row,
        &v[0],
        input("src/a.rs", Side::Parent, Location::Line(5), "base"),
    );
    f.gerrit("NEW", &["s1"], json!([]));
    route_files(&f);
    f.run_with(&options);
    let why = f.fate(&d).held();
    assert!(why.contains("merge commit"), "{why}");
}

mod held;
