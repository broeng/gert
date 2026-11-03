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
/// connection. Status 0 is a transport error.
#[derive(Clone, Default)]
struct Http {
    routes: RouteHttpClient,
    queued: Arc<Mutex<Vec<Queued>>>,
}

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
        async move {
            let routed = routed.await;
            match over {
                Some((0, why)) => Err(HttpError(why)),
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

fn fx() -> Fx {
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
    let mut store = Store::open_in_memory().unwrap();
    let server = store.server_id(BASE).unwrap();
    store
        .set_account(server, AccountId(ME), Some("me"), Some("me@example.com"))
        .unwrap();
    Fx {
        http,
        syncer: Syncer::new(client, Arc::new(Mutex::new(store)), server),
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
        block_on(self.syncer.consolidate(options)).unwrap()
    }

    /// Gerrit's change 7 with `status`, patch sets `shas` (1, 2, ...), the
    /// last one current, and `messages`.
    fn gerrit(&self, status: &str, shas: &[&str], messages: Value) {
        self.http.routes.route_json(
            "/changes/7?",
            &change_json(status, shas, messages).to_string(),
        );
    }

    fn query_finds(&self, hits: &[(u64, &str, &str)]) {
        let list: Vec<Value> = hits
            .iter()
            .map(|(n, branch, status)| summary_json(*n, branch, status))
            .collect();
        self.http
            .routes
            .route_json("/changes/?q=", &Value::Array(list).to_string());
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
fn not_found_makes_checking_local_and_leaves_vanished_alone() {
    let f = fx();
    let (checking, _) = f.local(CID, Some("main"), None, &["l1"]);
    let other = "Iffffffffffffffffffffffffffffffffffffffff";
    let (vanished, v) = f.local(other, Some("main"), None, &["l2"]);
    // Kept while vanished: it has a draft.
    f.draft(&vanished, &v[0], "keep me");
    f.syncer
        .store()
        .set_local_change_state(f.syncer.server, vanished.id, LocalChangeState::Vanished)
        .unwrap();
    f.query_finds(&[]);
    let report = f.run();
    assert!(report.ran && report.stopped.is_none(), "{report:?}");
    assert_eq!(report.not_found.len(), 2);
    assert_eq!(f.row(&checking).state, LocalChangeState::Local);
    assert_eq!(f.row(&vanished).state, LocalChangeState::Vanished);
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
    f.assert_nothing_local(&["l1", "l2"]);
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
    assert_eq!(f.row(&row).state, LocalChangeState::Checking);
    assert!(f.http.routes.requested().is_empty());
}

// ---- closed ----

#[test]
fn a_closed_change_holds_everything_and_a_reopened_one_is_sent() {
    let f = fx();
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
    // Held as it is: nothing moved to the patch set or was reserved.
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Pending);
    let held = f.load_draft(&d).unwrap();
    assert_eq!((held.change, held.patch_set), (CHANGE, v[0].number));
    assert!(held.reserved_by.is_none());

    // Checked again on the next run: still closed, nothing new.
    let report = f.run();
    assert!(report.closed.is_empty() && f.http.posts().is_empty());
    assert_eq!(f.row(&row).state, LocalChangeState::Closed);

    // Restored on Gerrit: normal handling, sent.
    f.gerrit("NEW", &["s1"], json!([]));
    f.posts_apply(json!({"Code-Review": 2}), 1);
    let report = f.run();
    assert_eq!(report.reopened, [(row.id, CHANGE)]);
    assert_eq!(f.http.posts().len(), 1);
    assert_eq!(f.reloaded(&r).state, PendingReviewState::Sent);
    assert_eq!(f.row(&row).state, LocalChangeState::Consolidated);
    f.assert_nothing_local(&[]);
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
    block_on(syncer.consolidate(&ConsolidateOptions::default())).unwrap();
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
    // The project's setting now is stricter (no entry: exact): drop only.
    let reason = held_reason(
        &row,
        &model_detail(&["s1", "s2"]),
        PatchSetNumber(1),
        Some(&r),
        &r.labels,
        &ConsolidateOptions::default(),
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
fn votes_that_arrive_after_a_new_patch_set_are_failed() {
    let f = fx();
    let (_, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let r = f.review(&v[0], &[("Code-Review", 2)], "");
    f.gerrit("NEW", &["s1"], json!([]));
    // Applied, but PS 2 was uploaded in between.
    f.posts_apply(json!({"Code-Review": 2}), 2);
    f.run();
    let r = f.reloaded(&r);
    assert_eq!(r.state, PendingReviewState::Failed);
    assert!(r.error.unwrap().contains("PS 1 was no longer current"));
    assert_eq!(r.labels.len(), 1);
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
    let d = f.load_draft(&d).unwrap();
    assert!(matches!(d.sync, DraftSync::Failed { .. }));
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
    let guard = f.syncer.consolidate_lock.try_lock().unwrap();
    let report = f.run();
    assert!(!report.ran);
    assert!(f.http.routes.requested().is_empty());
    drop(guard);
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
    let report = block_on(syncer.consolidate(&ConsolidateOptions::default())).unwrap();
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
