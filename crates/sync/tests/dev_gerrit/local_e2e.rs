//! Local changes end to end: a real clone of `demo/scheduler`, commits made
//! and amended with git, scanned with `local-git`, reviewed locally,
//! pushed to the dev Gerrit with `git push origin HEAD:refs/for/main`, and
//! consolidated, with a [`PortSource`] over the clone as the app's
//! checkouts give one. Every change is new (a file of its own, a fresh
//! Change-Id) and owned by alice; nothing else on the server is touched.
//!
//! The scenarios: (a) a review of the very commit that is pushed is sent
//! as given, under every `local_approval`; (b) a review of a version
//! amended before the push is ported, its comment on a changed line held,
//! its +2 cast or held by strategy, and what `exact-or-manual` held is
//! published from the held card (`exact`'s may only be dropped); a
//! `follow` review is purely local when a lookup after it did not find the
//! change or it was saved offline, else the strict rule holds it; (c) a +2
//! on a local patch set of a
//! Gerrit change is cast only when that commit is pushed as it is; (d) our
//! line maps agree with Gerrit's own porting of drafts.
//!
//! What the server did, as checked here (Gerrit 3.14.4):
//!
//! * A push of a commit with a `Change-Id` trailer to `refs/for/main`
//!   creates the change; its commit becomes patch set 1 with the very SHA,
//!   which is what makes an exact match. The clone's `origin/main` does not
//!   move, so the scanner keeps seeing the commit as local.
//! * The comments of a consolidation post carry the post's tag, as its
//!   change message does.
//! * A query for a Change-Id Gerrit does not have answers with an empty
//!   list: the "not found" that proves the votes saved before it purely
//!   local. The app runs that lookup right after a review is saved; the
//!   tests call it the same way.

use std::{path::Path, process::Command};

use local_git::{Local, NotLocal, Repo};
use model::{LocalApproval, LocalChangeState, PendingReviewState, RevisionId};
use store::LocalChangeEntry;
use sync::{HasCheckout, NotLocalPath, PathMap, PortCommit, PortError, PortMaps, PortSource};

use super::*;

const PROJECT: &str = "demo/scheduler";
const BRANCH: &str = "main";
const APP: &str = "e2etest";

/// A clone of `demo/scheduler`, committed to as alice.
struct Clone {
    dir: tempfile::TempDir,
    repo: Repo,
}

/// Environment variables that would point git at another repository: set
/// when the tests run from a git hook, they would make the commits and
/// pushes below go to that repository (gert's own). The same list as
/// local-git's.
const REMOVED_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
];

/// Runs git in `dir` as alice (author and committer, which Gerrit checks
/// on push) without the developer's own git configuration, and never on
/// a repository the environment names.
fn git_in(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    for var in REMOVED_ENV {
        cmd.env_remove(var);
    }
    let out = cmd
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Alice")
        .env("GIT_AUTHOR_EMAIL", "alice@example.com")
        .env("GIT_COMMITTER_NAME", "Alice")
        .env("GIT_COMMITTER_EMAIL", "alice@example.com")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

impl Clone {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let authority = base_url().trim_start_matches("http://").to_owned();
        let url = format!(
            "http://alice:alice-dev-password@{}/a/{PROJECT}",
            authority.trim_end_matches('/')
        );
        let path = dir.path().to_str().unwrap().to_owned();
        // From the temporary directory's parent, never from the working
        // directory (which may be a repository).
        let parent = dir.path().parent().unwrap();
        git_in(parent, &["clone", "-q", &url, &path]);
        git_in(dir.path(), &["config", "user.name", "Alice"]);
        git_in(dir.path(), &["config", "user.email", "alice@example.com"]);
        let repo = Repo::open(dir.path()).expect("open clone");
        Self { dir, repo }
    }

    fn git(&self, args: &[&str]) -> String {
        git_in(self.dir.path(), args)
    }

    /// Starts branch `name` at `origin/main`, tracking it (which makes
    /// `main` the target branch).
    fn branch(&self, name: &str) {
        self.git(&["checkout", "-q", "-b", name, &format!("origin/{BRANCH}")]);
    }

    fn switch(&self, name: &str) {
        self.git(&["checkout", "-q", name]);
    }

    /// Writes `path` and commits it with `subject` and `change_id`.
    fn commit(&self, path: &str, content: &str, subject: &str, change_id: &str) -> RevisionId {
        self.write(path, content);
        self.git(&["add", path]);
        let message =
            format!("{subject}\n\nEnd-to-end test of local changes.\n\nChange-Id: {change_id}\n");
        self.git(&["commit", "-q", "-m", &message]);
        self.head()
    }

    /// Rewrites `path` and amends HEAD, keeping its message.
    fn amend(&self, path: &str, content: &str) -> RevisionId {
        self.write(path, content);
        self.git(&["add", path]);
        self.git(&["commit", "-q", "--amend", "--no-edit"]);
        self.head()
    }

    fn write(&self, path: &str, content: &str) {
        let full = self.dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }

    fn head(&self) -> RevisionId {
        RevisionId::from(self.git(&["rev-parse", "HEAD"]))
    }

    /// `git push origin HEAD:refs/for/main`.
    fn push(&self) {
        self.git(&["push", "-q", "origin", &format!("HEAD:refs/for/{BRANCH}")]);
    }
}

/// The clone as consolidation's port source, as the app's checkouts are:
/// a copy of `ui::local_port::CheckoutPorts` for one checkout, which must
/// map every answer as that does.
struct Ports(Repo);

impl PortSource for Ports {
    fn has_checkout(&self, project: &str) -> HasCheckout {
        if project == PROJECT {
            HasCheckout::Yes
        } else {
            HasCheckout::NotConfigured
        }
    }

    fn commit(&self, _: &str, sha: &RevisionId) -> Result<Option<PortCommit>, PortError> {
        match self.0.commit(sha) {
            Ok(Local::Available(c)) => Ok(Some(PortCommit { parents: c.parents })),
            Ok(Local::NotLocal(_)) => Ok(None),
            Err(e) => Err(PortError::Git(e.to_string())),
        }
    }

    fn line_maps(
        &self,
        _: &str,
        from: &RevisionId,
        to: &RevisionId,
        paths: &[String],
    ) -> Result<PortMaps, PortError> {
        let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
        match self.0.line_maps(from, to, &paths) {
            Ok(Local::Available(maps)) => Ok(maps
                .into_iter()
                .map(|(path, map)| {
                    let map = match map {
                        Local::Available(map) => PathMap::Lines(map),
                        Local::NotLocal(NotLocal::NotAFile(_)) => {
                            PathMap::NotLocal(NotLocalPath::NotAFile)
                        }
                        Local::NotLocal(NotLocal::MissingContent(_)) => {
                            PathMap::NotLocal(NotLocalPath::MissingContent)
                        }
                        Local::NotLocal(why) => {
                            PathMap::NotLocal(NotLocalPath::Other(why.to_string()))
                        }
                    };
                    (path, map)
                })
                .collect()),
            Ok(Local::NotLocal(NotLocal::MissingCommit(sha))) => {
                Err(PortError::MissingCommit(sha.into()))
            }
            Ok(Local::NotLocal(why)) => Err(PortError::Git(why.to_string())),
            Err(e) => Err(PortError::Git(e.to_string())),
        }
    }
}

/// A fresh, well-formed Change-Id.
fn change_id() -> String {
    use std::sync::atomic::AtomicU32;
    static N: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = N.fetch_add(1, Ordering::SeqCst);
    format!(
        "I{:032x}{:04x}{:04x}",
        nanos,
        std::process::id() & 0xffff,
        n
    )
}

/// A file name of its own for one change.
fn unique_path(tag: &str) -> String {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros();
    format!("e2e/{stamp}-{tag}.txt")
}

/// Lines `one` to `six`, with `four` replaced by `four_as`.
fn six_lines(four_as: &str) -> String {
    format!("one\ntwo\nthree\n{four_as}\nfive\nsix\n")
}

impl Fx {
    /// Alice's syncer with the app name of these tests, the account
    /// fetched (as the shell does before it scans or consolidates).
    fn e2e() -> (Self, Syncer) {
        let f = alice();
        let syncer = f.syncer.clone().with_app_name(APP);
        block_on(syncer.account_self()).expect("account");
        (f, syncer)
    }

    /// One scan of the clone, reconciled as the scanner does: the
    /// account's email and the clone's author emails.
    fn scan(&self, clone: &Clone) -> store::ReconcileReport {
        let scan = clone.repo.scan(200).expect("scan");
        assert!(scan.is_complete(), "{scan:?}");
        let mut emails: Vec<String> = self
            .syncer
            .account_email()
            .unwrap()
            .into_iter()
            .chain(clone.repo.author_emails().unwrap())
            .map(|e| e.trim().to_lowercase())
            .collect();
        emails.sort_unstable();
        emails.dedup();
        self.syncer
            .reconcile_local(PROJECT, &scan, &emails, true)
            .expect("reconcile")
    }

    /// The local change row with `change_id`.
    fn entry(&self, change_id: &str) -> LocalChangeEntry {
        self.store
            .lock()
            .unwrap()
            .local_changes_of_project(self.server, PROJECT)
            .unwrap()
            .into_iter()
            .find(|e| e.change.change_id == change_id)
            .unwrap_or_else(|| panic!("no local change {change_id}"))
    }

    /// The local version with commit `sha` of the row with `change_id`.
    fn version(&self, change_id: &str, sha: &RevisionId) -> model::LocalPatchSet {
        self.entry(change_id)
            .patch_sets
            .into_iter()
            .find(|v| v.sha == *sha)
            .unwrap_or_else(|| panic!("no local version {sha}"))
    }

    /// A local inline comment on `line` of `path` in local version `v` of
    /// the change shown as `change` (its synthetic number, or the Gerrit
    /// change it is a local patch set of).
    fn comment(
        &self,
        change: ChangeNumber,
        v: &model::LocalPatchSet,
        path: &str,
        line: u32,
        message: &str,
    ) {
        self.syncer
            .save_draft_local(DraftSave::Create {
                change,
                patch_set: PatchSetRef {
                    number: v.number,
                    sha: v.sha.clone(),
                },
                input: DraftInput {
                    path: path.into(),
                    location: Location::Line(line),
                    ..input(message)
                },
            })
            .unwrap()
            .unwrap();
    }

    /// A local review of version `v` as the reply dialog saves it, with the
    /// server reachable or not (`offline`).
    fn review(
        &self,
        v: &model::LocalPatchSet,
        labels: &[(&str, i32)],
        strategy: LocalApproval,
        message: &str,
        offline: bool,
    ) -> model::PendingReview {
        self.syncer
            .create_pending_review(&store::NewPendingReview {
                patch_set: v.number,
                labels: labels.iter().map(|(l, v)| ((*l).to_owned(), *v)).collect(),
                strategy,
                message: message.into(),
                unresolved: false,
                created: model::Timestamp::now(),
                offline,
                replaces_draft: None,
            })
            .unwrap()
    }
}

/// One consolidation run over the clone, with the project's
/// `local_approval` = `setting`; it must not stop early or skip a change.
fn consolidate(syncer: &Syncer, clone: &Clone, setting: LocalApproval) -> sync::ConsolidateReport {
    let options = sync::ConsolidateOptions {
        local_approval: [(PROJECT.to_owned(), setting)].into(),
        port: Some(Arc::new(Ports(clone.repo.clone()))),
        ..sync::ConsolidateOptions::default()
    };
    let report = block_on(syncer.consolidate(&options));
    println!("consolidation: {report:#?}");
    assert!(report.ran, "{report:?}");
    assert!(
        report.stopped.is_none() && report.errors.is_empty(),
        "{report:?}"
    );
    report
}

/// The Gerrit change with `change_id` (fresh, so in this project only).
fn gerrit_change(change_id: &str) -> ChangeNumber {
    let found = get(&format!("/changes/?q=change:{change_id}"));
    let found = found.as_array().unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["project"], PROJECT);
    ChangeNumber(found[0]["_number"].as_u64().unwrap())
}

/// The SHA of patch set `ps` of change `n`.
fn patch_set_sha(n: ChangeNumber, ps: u64) -> RevisionId {
    let c = get(&format!("/changes/{n}?o=ALL_REVISIONS"));
    let (sha, _) = c["revisions"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, r)| r["_number"] == ps)
        .unwrap_or_else(|| panic!("no PS {ps} on {n}: {c}"));
    RevisionId::from(sha.clone())
}

/// Alice's votes on change `n`, as label and value (zero votes left out).
fn alice_votes(n: ChangeNumber) -> Vec<(String, i64)> {
    let me = get("/accounts/self")["_account_id"].clone();
    let labels = get(&format!("/changes/{n}?o=DETAILED_LABELS"))["labels"].clone();
    let mut out: Vec<(String, i64)> = labels
        .as_object()
        .unwrap()
        .iter()
        .filter_map(|(label, l)| {
            l["all"]
                .as_array()?
                .iter()
                .find(|v| v["_account_id"] == me)
                .and_then(|v| v["value"].as_i64())
                .filter(|v| *v != 0)
                .map(|v| (label.clone(), v))
        })
        .collect();
    out.sort();
    out
}

/// The published comments on `path` of change `n`, as (patch set, line,
/// message, tag).
fn comments_on(n: ChangeNumber, path: &str) -> Vec<(u64, u64, String, String)> {
    let all = get(&format!("/changes/{n}/comments"));
    let mut out: Vec<(u64, u64, String, String)> = all[path]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            (
                c["patch_set"].as_u64().unwrap(),
                c["line"].as_u64().unwrap_or(0),
                c["message"].as_str().unwrap().to_owned(),
                c["tag"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

/// The tag of the one change message of change `n` that a consolidation
/// post of local change `row` sent.
fn post_tag(n: ChangeNumber, row: &model::LocalChange) -> String {
    let prefix = format!("{APP}-local-{}-", row.id.0);
    let messages = get(&format!("/changes/{n}?o=MESSAGES"))["messages"].clone();
    let ours: Vec<&serde_json::Value> = messages
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["tag"].as_str().is_some_and(|t| t.starts_with(&prefix)))
        .collect();
    assert_eq!(ours.len(), 1, "{messages}");
    ours[0]["tag"].as_str().unwrap().to_owned()
}

/// Nothing of local change `change_id` is left to send, and it is a
/// tombstone on Gerrit change `n`.
fn assert_consolidated(f: &Fx, change_id: &str, n: ChangeNumber) {
    let e = f.entry(change_id);
    assert_eq!(e.change.state, LocalChangeState::Consolidated, "{e:?}");
    assert_eq!(e.change.gerrit_number, Some(n));
    assert!(!e.has_unsent, "{e:?}");
    let held = f.syncer.held_items(n).unwrap();
    assert!(
        held.drafts.is_empty() && held.reviews.is_empty(),
        "{held:?}"
    );
}

// ---- (a) exact match ----

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn a_local_review_of_a_pushed_commit_is_sent_as_it_was_given() {
    let (f, syncer) = Fx::e2e();
    let clone = Clone::new();
    // One change per strategy snapshot: an exact match sends everything
    // under each, also an approval (Verified +1 is that label's maximum),
    // with the project's setting the strictest.
    let strategies = [
        LocalApproval::Follow,
        LocalApproval::ExactOrManual,
        LocalApproval::Exact,
    ];
    let mut cases = Vec::new();
    for (i, strategy) in strategies.into_iter().enumerate() {
        clone.branch(&format!("e2e-exact-{i}"));
        let path = unique_path(&format!("exact-{i}"));
        let id = change_id();
        let sha = clone.commit(&path, &six_lines("four"), &format!("E2E exact {i}"), &id);
        cases.push((strategy, path, id, sha));
    }

    // Scanned: a local change in each case, checking with Gerrit until a
    // lookup says Gerrit does not have it.
    f.scan(&clone);
    for (_, _, id, sha) in &cases {
        let e = f.entry(id);
        assert_eq!(e.change.state, LocalChangeState::Checking);
        assert!(e.change.is_purely_local());
        assert_eq!(e.change.branch.as_deref(), Some(BRANCH));
        assert_eq!(e.patch_sets.len(), 1);
        assert_eq!(e.patch_sets[0].sha, *sha);
    }
    let report = consolidate(&syncer, &clone, LocalApproval::Exact);
    assert!(report.posts.is_empty());
    for (_, _, id, _) in &cases {
        let e = f.entry(id);
        assert_eq!(e.change.state, LocalChangeState::Local, "{e:?}");
        assert!(report.not_found.contains(&e.change.id));
    }

    // Reviewed locally: a comment, a message, Code-Review +1 and the
    // approval.
    let mut reviews = Vec::new();
    for (strategy, path, id, sha) in &cases {
        let v = f.version(id, sha);
        let number = f.entry(id).change.number;
        f.comment(number, &v, path, 4, "local comment on four");
        let r = f.review(
            &v,
            &[("Code-Review", 1), ("Verified", 1)],
            *strategy,
            "local review message",
            false,
        );
        assert_eq!(r.state, PendingReviewState::Pending);
        reviews.push(r);
    }

    // Pushed, then consolidated.
    for (i, _) in cases.iter().enumerate() {
        clone.switch(&format!("e2e-exact-{i}"));
        clone.push();
    }
    let report = consolidate(&syncer, &clone, LocalApproval::Exact);
    assert_eq!(report.posts.len(), cases.len(), "{report:?}");
    assert!(
        report
            .posts
            .iter()
            .all(|p| p.outcome == sync::PostOutcome::Sent),
        "{report:?}"
    );

    for ((strategy, path, id, sha), review) in cases.iter().zip(&reviews) {
        let n = gerrit_change(id);
        assert_eq!(patch_set_sha(n, 1), *sha, "the push made PS 1 of {sha}");
        let row = f.entry(id).change;
        assert!(
            report
                .found
                .iter()
                .any(|x| x.gerrit == n && x.local_change == row.id)
        );
        let tag = post_tag(n, &row);
        assert!(
            tag.starts_with(&format!("{APP}-local-{}-r{}-", row.id.0, review.id.0)),
            "{tag}"
        );
        // The comment on its line, the message, the votes, all tagged.
        assert_eq!(
            comments_on(n, path),
            [(1, 4, "local comment on four".to_owned(), tag.clone())],
            "{strategy:?}"
        );
        let top = comments_on(n, "/PATCHSET_LEVEL");
        assert_eq!(top.len(), 1, "{top:?}");
        assert_eq!(
            (top[0].0, &top[0].2, &top[0].3),
            (1, &"local review message".to_owned(), &tag)
        );
        assert_eq!(
            alice_votes(n),
            [("Code-Review".to_owned(), 1), ("Verified".to_owned(), 1)],
            "{strategy:?}"
        );
        // Locally: sent, nothing left, a tombstone.
        assert_eq!(f.review_now(review).state, PendingReviewState::Sent);
        assert!(report.consolidated.contains(&(row.id, n)), "{report:?}");
        assert_consolidated(&f, id, n);
    }

    // A later run sends nothing again.
    let report = consolidate(&syncer, &clone, LocalApproval::Exact);
    assert!(report.posts.is_empty(), "{report:?}");
}

// ---- (b) amended before upload ----

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn a_review_of_a_version_amended_before_upload_is_ported_by_strategy() {
    let (f, syncer) = Fx::e2e();
    let clone = Clone::new();
    let strategies = [
        LocalApproval::Follow,
        LocalApproval::ExactOrManual,
        LocalApproval::Exact,
    ];
    struct Case {
        strategy: LocalApproval,
        branch: String,
        path: String,
        id: String,
        v1: RevisionId,
        review: Option<model::PendingReview>,
    }
    let mut cases: Vec<Case> = strategies
        .into_iter()
        .enumerate()
        .map(|(i, strategy)| {
            let branch = format!("e2e-amend-{i}");
            clone.branch(&branch);
            let path = unique_path(&format!("amend-{i}"));
            let id = change_id();
            let v1 = clone.commit(&path, &six_lines("four"), &format!("E2E amend {i}"), &id);
            Case {
                strategy,
                branch,
                path,
                id,
                v1,
                review: None,
            }
        })
        .collect();
    f.scan(&clone);
    consolidate(&syncer, &clone, LocalApproval::Follow);

    // Local 1: a comment on an unchanged line, one on the line the amend
    // changes, and a review with an approval, followed by the lookup a
    // saved review starts: Gerrit does not have the change, so the votes
    // are purely local.
    for c in &mut cases {
        let v = f.version(&c.id, &c.v1);
        let number = f.entry(&c.id).change.number;
        f.comment(number, &v, &c.path, 2, "on two, unchanged");
        f.comment(number, &v, &c.path, 4, "on four, which changes");
        c.review = Some(f.review(&v, &[("Code-Review", 2)], c.strategy, "looks good", false));
    }
    after_a_tick();
    consolidate(&syncer, &clone, LocalApproval::Follow);

    // Amended (line four), scanned (local 2), pushed: Gerrit's PS 1 is
    // local 2, and local 1 never reached Gerrit.
    for c in &cases {
        clone.switch(&c.branch);
        let v2 = clone.amend(&c.path, &six_lines("FOUR"));
        assert_ne!(v2, c.v1);
        clone.push();
    }
    f.scan(&clone);
    for c in &cases {
        let e = f.entry(&c.id);
        assert_eq!(e.patch_sets.len(), 2, "{e:?}");
        assert!(!e.patch_sets[0].on_branch, "local 1 was amended away");
    }

    // The project lets votes follow; each review's snapshot is stricter
    // or equal, and the stricter one rules.
    let report = consolidate(&syncer, &clone, LocalApproval::Follow);
    for c in &cases {
        let n = gerrit_change(&c.id);
        let row = f.entry(&c.id).change;
        let review = f.review_now(c.review.as_ref().unwrap());
        println!("{:?}: {review:#?}", c.strategy);
        let ported = report
            .ported
            .iter()
            .find(|p| p.local_change == row.id)
            .unwrap_or_else(|| panic!("{:?} not ported: {report:?}", c.strategy));
        assert_eq!(ported.to, Some(model::PatchSetNumber(1)));
        assert_eq!((ported.drafts, ported.reviews), (1, 1), "{ported:?}");
        assert_eq!(ported.held.len(), 1, "{ported:?}");
        println!("held comment: {}", ported.held[0].1);

        // The unchanged line's comment is on PS 1, line 2, and the
        // message went with it; the other comment is held on local 1.
        let tag = post_tag(n, &row);
        assert_eq!(
            comments_on(n, &c.path),
            [(1, 2, "on two, unchanged".to_owned(), tag.clone())]
        );
        let top = comments_on(n, "/PATCHSET_LEVEL");
        assert_eq!(top.len(), 1, "{top:?}");
        assert_eq!(top[0].2, "looks good");
        let held = f.syncer.held_items(n).unwrap();
        assert_eq!(held.drafts.len(), 1, "{held:?}");
        assert_eq!(held.drafts[0].message, "on four, which changes");
        assert!(held.drafts[0].held_reason.is_some());

        match c.strategy {
            // Cast on what reached Gerrit, though a comment was held.
            LocalApproval::Follow => {
                assert_eq!(alice_votes(n), [("Code-Review".to_owned(), 2)]);
                assert_eq!(review.state, PendingReviewState::Sent);
            }
            // Held, to publish by hand.
            LocalApproval::ExactOrManual => {
                assert_eq!(alice_votes(n), []);
                assert_eq!(review.state, PendingReviewState::Held);
                assert!(review.drop_only.is_empty(), "{review:?}");
                assert_eq!(review.labels.get("Code-Review"), Some(&2));
                // Published from the held card: the vote onto PS 1, the
                // comment quoted in a patch-set comment, never on a line.
                let items = [
                    sync::HeldItem::Vote {
                        review: review.id,
                        label: "Code-Review".into(),
                    },
                    sync::HeldItem::Comment(held.drafts[0].local_id),
                ];
                let out = block_on(syncer.publish_held(
                    n,
                    model::PatchSetNumber(1),
                    &items,
                    LocalApproval::Follow,
                ))
                .expect("published");
                println!("published: {out:?}");
                assert_eq!(out.outcome, sync::PostOutcome::Sent);
                assert_eq!(out.votes, [("Code-Review".to_owned(), 2)]);
                assert_eq!(alice_votes(n), [("Code-Review".to_owned(), 2)]);
                let top = comments_on(n, "/PATCHSET_LEVEL");
                println!("patch-set comments: {top:?}");
                assert!(
                    top.iter().any(|(ps, _, m, _)| *ps == 1
                        && m.contains("line 4")
                        && m.contains("on four, which changes")),
                    "{top:?}"
                );
                assert_eq!(comments_on(n, &c.path).len(), 1, "nothing on a line");
                let left = f.syncer.held_items(n).unwrap();
                assert!(
                    left.drafts.is_empty() && left.reviews.is_empty(),
                    "{left:?}"
                );
            }
            // Held, and the approval may only be dropped.
            LocalApproval::Exact => {
                assert_eq!(alice_votes(n), []);
                assert_eq!(review.state, PendingReviewState::Held);
                assert!(review.drop_only.contains("Code-Review"), "{review:?}");
                let vote = sync::HeldItem::Vote {
                    review: review.id,
                    label: "Code-Review".into(),
                };
                let err = block_on(syncer.publish_held(
                    n,
                    model::PatchSetNumber(1),
                    &[vote],
                    LocalApproval::Follow,
                ))
                .unwrap_err();
                assert!(matches!(err, sync::Error::DropOnly(_)), "{err}");
                assert_eq!(alice_votes(n), []);
            }
        }
        println!(
            "{:?} held reason: {:?}; row {:?}",
            c.strategy,
            review.held_reason,
            f.entry(&c.id).change.state
        );
    }
}

/// A lookup in the vote's own millisecond proves nothing (the order of
/// the two is unknown), so a test waits a moment between them.
fn after_a_tick() {
    std::thread::sleep(std::time::Duration::from_millis(5));
}

/// When a `follow` review of a version amended before the push is purely
/// local: saved, then a lookup that does not find the change, then amended
/// and pushed within seconds: cast; saved offline (no lookup at all):
/// cast; saved with Gerrit reachable but pushed before any lookup: the
/// strict rule holds the +2 drop-only, saying Gerrit could not confirm.
#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn a_follow_review_is_purely_local_after_a_lookup_or_offline() {
    let (f, syncer) = Fx::e2e();
    let clone = Clone::new();
    let mut cases = Vec::new();
    for name in ["looked-up", "offline", "unconfirmed"] {
        let branch = format!("e2e-local-{name}");
        clone.branch(&branch);
        let path = unique_path(name);
        let id = change_id();
        let v1 = clone.commit(&path, &six_lines("four"), &format!("E2E {name}"), &id);
        cases.push((name, branch, path, id, v1));
    }
    f.scan(&clone);
    consolidate(&syncer, &clone, LocalApproval::Follow);
    let mut reviews = Vec::new();
    for (name, _, path, id, v1) in &cases {
        let v = f.version(id, v1);
        f.comment(f.entry(id).change.number, &v, path, 2, "on two");
        let offline = *name == "offline";
        reviews.push(f.review(
            &v,
            &[("Code-Review", 2)],
            LocalApproval::Follow,
            name,
            offline,
        ));
        if *name == "looked-up" {
            // The lookup a saved review starts (the others are saved after
            // it, so it proves nothing about them).
            after_a_tick();
            consolidate(&syncer, &clone, LocalApproval::Follow);
        }
    }
    for (_, branch, path, _, _) in &cases {
        clone.switch(branch);
        clone.amend(path, &six_lines("FOUR"));
        clone.push();
    }
    f.scan(&clone);
    consolidate(&syncer, &clone, LocalApproval::Follow);

    for ((name, _, path, id, _), review) in cases.iter().zip(&reviews) {
        let n = gerrit_change(id);
        let review = f.review_now(review);
        println!("{name}: {review:#?}");
        assert_eq!(
            comments_on(n, path).len(),
            1,
            "{name}: the comment is ported"
        );
        if *name == "unconfirmed" {
            assert_eq!(alice_votes(n), [], "{name}");
            assert_eq!(review.state, PendingReviewState::Held);
            assert!(review.drop_only.contains("Code-Review"), "{review:?}");
            let reason = review.held_reason.unwrap();
            assert!(
                reason.contains("Gerrit could not confirm the change was not there yet"),
                "{reason}"
            );
        } else {
            assert_eq!(alice_votes(n), [("Code-Review".to_owned(), 2)], "{name}");
            assert_eq!(review.state, PendingReviewState::Sent, "{name}");
        }
    }
}

// ---- (c) local patch sets of a Gerrit change ----

#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn an_approval_on_a_local_patch_set_goes_only_onto_its_exact_commit() {
    let (f, syncer) = Fx::e2e();
    let clone = Clone::new();
    // Two changes: in "moved" the reviewed version is amended again before
    // the push; in "exact" it is pushed as it is.
    let mut cases = Vec::new();
    for name in ["moved", "exact"] {
        let branch = format!("e2e-strict-{name}");
        clone.branch(&branch);
        let path = unique_path(&format!("strict-{name}"));
        let id = change_id();
        clone.commit(
            &path,
            &six_lines("four"),
            &format!("E2E strict {name}"),
            &id,
        );
        clone.push();
        cases.push((name, branch, path, id));
    }
    // PS 1 is on Gerrit; the run finds it, and the row of the pushed
    // commit has nothing to send.
    f.scan(&clone);
    consolidate(&syncer, &clone, LocalApproval::Follow);

    // Amended locally: a local patch set of the Gerrit change, with a
    // comment on an unchanged line and a +2. The project lets purely local
    // votes follow, which does not apply here.
    let mut reviews = Vec::new();
    for (_, branch, path, id) in &cases {
        clone.switch(branch);
        let v2 = clone.amend(path, &six_lines("four, amended"));
        let n = gerrit_change(id);
        f.scan(&clone);
        let local = f.syncer.local_patch_sets_of(n).unwrap();
        let v = local
            .iter()
            .find(|v| v.sha == v2)
            .unwrap_or_else(|| panic!("{v2} is no local patch set of {n}: {local:?}"))
            .clone();
        f.comment(n, &v, path, 2, "on two, local PS");
        let r = f.review(
            &v,
            &[("Code-Review", 2)],
            LocalApproval::Follow,
            "approved locally",
            false,
        );
        reviews.push(r);
    }

    // "moved": amended once more and pushed, so PS 2 has another SHA.
    // "exact": pushed as reviewed.
    clone.switch(&cases[0].1);
    clone.amend(&cases[0].2, &six_lines("four, amended twice"));
    clone.push();
    clone.switch(&cases[1].1);
    clone.push();
    f.scan(&clone);
    let report = consolidate(&syncer, &clone, LocalApproval::Follow);

    for ((name, _, path, id), review) in cases.iter().zip(&reviews) {
        let n = gerrit_change(id);
        let review = f.review_now(review);
        println!("{name}: {review:#?}");
        // The comment on the unchanged line and the message are on PS 2
        // either way.
        let ours: Vec<_> = comments_on(n, path)
            .into_iter()
            .map(|(ps, line, m, _)| (ps, line, m))
            .collect();
        assert_eq!(ours, [(2, 2, "on two, local PS".to_owned())], "{name}");
        let top = comments_on(n, "/PATCHSET_LEVEL");
        assert_eq!(top.len(), 1, "{top:?}");
        assert_eq!((top[0].0, top[0].2.as_str()), (2, "approved locally"));
        if *name == "moved" {
            // Never cast on another commit; it may only be dropped.
            assert_eq!(alice_votes(n), [], "{report:?}");
            assert_eq!(review.state, PendingReviewState::Held);
            assert!(review.drop_only.contains("Code-Review"), "{review:?}");
            println!("held reason: {:?}", review.held_reason);
        } else {
            assert_eq!(alice_votes(n), [("Code-Review".to_owned(), 2)]);
            assert_eq!(review.state, PendingReviewState::Sent);
            assert_consolidated(&f, id, n);
        }
    }
}

// ---- (d) our ported anchors against Gerrit's ----

/// Gerrit ports a user's drafts from other patch sets onto a revision
/// (`ported_drafts`). For every line our line map carries over unchanged,
/// Gerrit's port lands on the same line. A comment on a changed or deleted
/// line we hold; Gerrit makes it a comment on the file (no `line`; the
/// ported draft keeps its old `patch_set`). The drafts stay on the
/// throwaway change.
#[test]
#[ignore = "needs GERT_TEST_GERRIT"]
fn ported_anchors_agree_with_gerrits_ported_drafts() {
    let clone = Clone::new();
    clone.branch("e2e-ported-drafts");
    let path = unique_path("ported-drafts");
    let id = change_id();
    let lines: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
    let v1 = clone.commit(&path, &(lines.join("\n") + "\n"), "E2E ported drafts", &id);
    clone.push();
    // Delete line 3, insert two lines after line 8, change line 12, change
    // the last line and add one after it.
    let mut v2_lines: Vec<String> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        match i + 1 {
            3 => {}
            8 => {
                v2_lines.push(l.clone());
                v2_lines.push("inserted a".into());
                v2_lines.push("inserted b".into());
            }
            12 => v2_lines.push("line 12, changed".into()),
            20 => {
                v2_lines.push("line 20, changed".into());
                v2_lines.push("line 21".into());
            }
            _ => v2_lines.push(l.clone()),
        }
    }
    let v2 = clone.amend(&path, &(v2_lines.join("\n") + "\n"));
    clone.push();
    let n = gerrit_change(&id);
    assert_eq!(patch_set_sha(n, 1), v1);
    assert_eq!(patch_set_sha(n, 2), v2);

    // A draft on every line of PS 1.
    for line in 1..=lines.len() {
        let (code, body) = raw(
            "PUT",
            &format!("/changes/{n}/revisions/1/drafts"),
            serde_json::json!({"path": path, "line": line, "message": format!("on {line}")}),
        );
        assert_eq!(code, 201, "{body}");
    }
    let ported = get(&format!("/changes/{n}/revisions/2/ported_drafts"));
    println!("ported drafts: {ported}");
    let theirs: std::collections::BTreeMap<u32, Option<u32>> = ported[&path]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let from: u32 = c["message"].as_str().unwrap()[3..].parse().unwrap();
            (from, c["line"].as_u64().map(|l| l as u32))
        })
        .collect();
    assert_eq!(theirs.len(), lines.len(), "every draft ported");

    let maps = clone
        .repo
        .line_maps(&v1, &v2, &[path.as_str()])
        .unwrap()
        .available()
        .unwrap();
    let map = maps[&path].clone().available().unwrap();
    let mut held = Vec::new();
    for line in 1..=lines.len() as u32 {
        let ours = map.map_line(line);
        assert_eq!(theirs[&line], ours, "line {line}");
        if ours.is_none() {
            held.push(line);
        }
    }
    assert_eq!(held, [3, 12, 20]);
}
