use std::collections::BTreeMap;

use model::{
    Account, AccountId, ChangeNumber, ChangeSummary, CommitInfo, DraftInput, LocalChangeId,
    Location, PatchSetRef, Side, Signature, Upstream,
};
use store::{ReconcileReport, ServerId, Store};

use super::*;

const ME: &str = "me@example.com";

fn t(secs: i64) -> Timestamp {
    Timestamp::from_second(1_700_000_000 + secs).unwrap()
}

fn now() -> Timestamp {
    t(1_000_000)
}

/// A commit by me, with Change-Id `change_id`, on `branches`.
fn commit(sha: &str, change_id: Option<&str>, branches: &[&str]) -> LocalCommit {
    let sig = |time| Signature {
        name: "Me".into(),
        email: ME.into(),
        time,
        tz: 0,
    };
    LocalCommit {
        commit: CommitInfo {
            sha: sha.into(),
            parents: vec![format!("{sha}^").as_str().into()],
            author: sig(t(0)),
            committer: sig(t(sha.len() as i64)),
            message: format!("Subject {sha}\n\nChange-Id: {change_id:?}\n"),
            change_id: change_id.map(str::to_owned),
        },
        branches: branches.iter().map(|b| (*b).to_owned()).collect(),
    }
}

/// `topic` tracks `origin/main`, `fix` tracks `origin/stable`, other
/// branches go to `origin`'s default, `main`.
fn targets() -> BranchTargets {
    let up = |branch: &str| Upstream {
        remote: "origin".into(),
        branch: branch.into(),
    };
    BranchTargets {
        upstreams: BTreeMap::from([("topic".into(), up("main")), ("fix".into(), up("stable"))]),
        defaults: BTreeMap::from([("origin".into(), "main".into())]),
    }
}

/// A complete scan, newest commit first.
fn scan(commits: Vec<LocalCommit>) -> LocalScan {
    LocalScan {
        fingerprint: 1,
        commits,
        truncated: false,
        no_remotes: false,
        targets: targets(),
    }
}

fn summary(n: u64, change_id: &str, branch: &str, status: ChangeStatus) -> ChangeSummary {
    ChangeSummary {
        number: ChangeNumber(n),
        change_id: Some(change_id.into()),
        project: "p".into(),
        branch: branch.into(),
        subject: "s".into(),
        status,
        owner: Account {
            id: AccountId(1),
            name: None,
            email: None,
            username: None,
            display_name: None,
        },
        created: t(0),
        updated: t(0),
        insertions: 0,
        deletions: 0,
        labels: BTreeMap::new(),
        work_in_progress: false,
        is_private: false,
        unresolved_comment_count: 0,
        total_comment_count: 0,
        topic: None,
        hashtags: vec![],
        attention_set: vec![],
        submittable: None,
        mergeable: None,
    }
}

/// The store and the cache a plan is computed from.
struct H {
    store: Store,
    server: ServerId,
    cached: Vec<CachedGerritChange>,
    emails: Vec<String>,
}

impl H {
    fn new() -> Self {
        let mut store = Store::open_in_memory().unwrap();
        let server = store.server_id("https://g.example").unwrap();
        Self {
            store,
            server,
            cached: Vec::new(),
            emails: vec![ME.into()],
        }
    }

    /// Caches Gerrit change `n`; `shas` are its patch sets 1, 2, ... (no
    /// patch sets known when `None`: only the summary is cached).
    fn gerrit(
        &mut self,
        n: u64,
        change_id: &str,
        branch: &str,
        status: ChangeStatus,
        shas: Option<&[&str]>,
    ) {
        let patch_sets = shas.map(|shas| {
            shas.iter()
                .zip(1..)
                .map(|(sha, i)| PatchSetRef {
                    number: PatchSetNumber(i),
                    sha: (*sha).into(),
                })
                .collect::<Vec<_>>()
        });
        self.cached.push(CachedGerritChange {
            summary: summary(n, change_id, branch, status),
            current: patch_sets.as_ref().and_then(|p| p.last().map(|p| p.number)),
            patch_sets,
        });
    }

    fn entries(&self) -> Vec<LocalChangeEntry> {
        self.store
            .local_changes_of_project(self.server, "p")
            .unwrap()
    }

    fn plan(&self, scan: &LocalScan) -> Vec<LocalOp> {
        plan_reconcile(scan, &self.emails, &self.cached, &self.entries(), now())
    }

    fn apply(&mut self, ops: &[LocalOp]) -> ReconcileReport {
        self.store
            .reconcile_local(self.server, "p", ops, now())
            .unwrap()
    }

    fn run(&mut self, scan: &LocalScan) -> ReconcileReport {
        let ops = self.plan(scan);
        self.apply(&ops)
    }

    /// Every row of the project as `branch change-id state [versions]`,
    /// sorted; a version mapped to a Gerrit patch set shows as `sha=psN`,
    /// a Gerrit change number as `g42`.
    fn rows(&self) -> Vec<String> {
        let mut rows: Vec<String> = self
            .entries()
            .iter()
            .map(|e| {
                let c = &e.change;
                let versions: Vec<String> = e
                    .patch_sets
                    .iter()
                    .map(|p| match p.gerrit_patch_set {
                        Some(n) => format!("{}=ps{n}", p.sha),
                        None => p.sha.to_string(),
                    })
                    .collect();
                format!(
                    "{} {} {}{} [{}]",
                    c.branch.as_deref().unwrap_or("-"),
                    c.change_id,
                    c.state.as_str(),
                    c.gerrit_number
                        .map(|n| format!(" g{n}"))
                        .unwrap_or_default(),
                    versions.join(" ")
                )
            })
            .collect();
        rows.sort();
        rows
    }

    fn entry(&self, change_id: &str) -> LocalChangeEntry {
        self.entries()
            .into_iter()
            .find(|e| e.change.change_id == change_id)
            .unwrap()
    }

    /// Gives a local change unsent data: a draft on its first version.
    fn add_draft(&mut self, change_id: &str) {
        let e = self.entry(change_id);
        let p = &e.patch_sets[0];
        let change = e.change.gerrit_number.unwrap_or(e.change.number);
        self.store
            .create_draft(
                self.server,
                change,
                &PatchSetRef {
                    number: p.number,
                    sha: p.sha.clone(),
                },
                &DraftInput {
                    path: "a.rs".into(),
                    side: Side::Revision,
                    parent: None,
                    location: Location::Line(1),
                    in_reply_to: None,
                    message: "unsent".into(),
                    unresolved: true,
                },
                now(),
            )
            .unwrap();
    }

    fn set_state(&mut self, change_id: &str, state: LocalChangeState) {
        let id = self.entry(change_id).change.id;
        assert_eq!(
            self.store
                .set_local_change_state(self.server, id, state)
                .unwrap(),
            Some(state)
        );
    }
}

fn s(rows: &[&str]) -> Vec<String> {
    rows.iter().map(|r| (*r).to_owned()).collect()
}

#[test]
fn new_commits_become_checking_changes() {
    let mut h = H::new();
    let first = scan(vec![
        commit("c2", Some("I2"), &["fix"]),
        commit("c1", Some("I1"), &["topic"]),
        commit("c3", Some("I3"), &["wip"]),
    ]);
    let report = h.run(&first);
    assert_eq!(report.inserted.len(), 3);
    assert!(report.is_settled());
    assert_eq!(
        h.rows(),
        s(&[
            "main I1 checking [c1]",
            "main I3 checking [c3]",
            "stable I2 checking [c2]",
        ])
    );
    let e = h.entry("I1");
    assert_eq!(e.change.subject, "Subject c1");
    assert_eq!(
        (
            e.change.author_name.as_str(),
            e.change.author_email.as_str()
        ),
        ("Me", ME)
    );
    assert_eq!(e.change.updated, t(2));
    assert_eq!(e.patch_sets[0].seen_at, now());
    assert_eq!(e.patch_sets[0].parent_sha, Some("c1^".into()));
    // Nothing changed: an empty plan.
    assert_eq!(h.plan(&first), []);
}

#[test]
fn the_target_branch_is_unknown_without_upstreams_or_defaults() {
    let mut h = H::new();
    let mut sc = scan(vec![commit("c1", Some("I1"), &["topic"])]);
    sc.targets = BranchTargets::default();
    h.run(&sc);
    assert_eq!(h.rows(), s(&["- I1 checking [c1]"]));
}

#[test]
fn an_amend_is_a_second_local_patch_set() {
    let mut h = H::new();
    h.run(&scan(vec![commit("c1", Some("I1"), &["topic"])]));
    let report = h.run(&scan(vec![commit("c1b", Some("I1"), &["topic"])]));
    assert_eq!((report.patch_sets_added, report.updated.len()), (1, 1));
    assert_eq!(h.rows(), s(&["main I1 checking [c1 c1b]"]));
    let e = h.entry("I1");
    let ordinals: Vec<u32> = e.patch_sets.iter().map(|p| p.ordinal).collect();
    assert_eq!(ordinals, [1, 2]);
    assert_eq!(e.change.subject, "Subject c1b");

    // Two new versions in one scan (on two branches) are numbered oldest
    // first.
    let mut h = H::new();
    h.run(&scan(vec![
        commit("new", Some("I1"), &["topic"]),
        commit("old", Some("I1"), &["topic-old"]),
    ]));
    assert_eq!(h.rows(), s(&["main I1 checking [old new]"]));
}

#[test]
fn a_pushed_commit_maps_to_its_gerrit_patch_set() {
    // Without unsent data the local change goes: it is on Gerrit now.
    let mut h = H::new();
    let sc = scan(vec![commit("c1", Some("I1"), &["topic"])]);
    h.run(&sc);
    h.gerrit(42, "I1", "main", ChangeStatus::New, Some(&["c1"]));
    let ops = h.plan(&sc);
    let e = h.entry("I1");
    assert_eq!(
        ops,
        [
            LocalOp::MapPatchSet {
                patch_set: e.patch_sets[0].id,
                gerrit_patch_set: PatchSetNumber(1),
            },
            LocalOp::Delete {
                seen: seen(&e),
                else_state: None,
            },
        ]
    );
    let report = h.apply(&ops);
    assert!(report.is_settled());
    assert_eq!(h.rows(), Vec::<String>::new());
    // A rescan leaves it on Gerrit.
    assert_eq!(h.plan(&sc), []);

    // With unsent data it stays for consolidation, mapped, so the SHA may
    // be named in requests.
    let mut h = H::new();
    h.run(&sc);
    h.add_draft("I1");
    h.gerrit(42, "I1", "main", ChangeStatus::New, Some(&["c1"]));
    let report = h.run(&sc);
    assert!(report.is_settled());
    assert_eq!(report.patch_sets_mapped, 1);
    assert_eq!(h.rows(), s(&["main I1 checking [c1=ps1]"]));
    assert!(!h.store.is_local_sha(h.server, &"c1".into()).unwrap());
    assert_eq!(h.plan(&sc), []);
}

#[test]
fn a_version_is_mapped_even_when_it_left_the_branches() {
    let mut h = H::new();
    h.run(&scan(vec![commit("c1", Some("I1"), &["topic"])]));
    h.run(&scan(vec![commit("c2", Some("I1"), &["topic"])]));
    // c1 was pushed before the amend.
    h.gerrit(42, "I1", "main", ChangeStatus::New, Some(&["c1"]));
    h.run(&scan(vec![commit("c2", Some("I1"), &["topic"])]));
    assert_eq!(h.rows(), s(&["main I1 checking g42 [c1=ps1 c2]"]));
}

#[test]
fn a_known_open_change_id_gives_local_patch_sets_of_the_gerrit_change() {
    let sc = scan(vec![commit("c1", Some("I1"), &["topic"])]);
    for (cached, expected) in [
        // Open on the target branch, the commit not one of its patch sets.
        (
            vec![("main", ChangeStatus::New, Some(&["g1"][..]))],
            "main I1 checking g42 [c1]",
        ),
        // Only the summary is cached.
        (
            vec![("main", ChangeStatus::New, None)],
            "main I1 checking g42 [c1]",
        ),
        // A change on another branch is another change.
        (
            vec![("stable", ChangeStatus::New, Some(&["g1"][..]))],
            "main I1 checking [c1]",
        ),
        // A status this build does not know counts as open.
        (
            vec![("main", ChangeStatus::Unknown("NEWER".into()), None)],
            "main I1 checking g42 [c1]",
        ),
    ] {
        let mut h = H::new();
        for (branch, status, shas) in cached {
            h.gerrit(42, "I1", branch, status, shas);
        }
        h.run(&sc);
        assert_eq!(h.rows(), s(&[expected]));
    }

    // With the target branch unknown, only a single cached change counts.
    let mut unknown = sc.clone();
    unknown.targets = BranchTargets::default();
    let mut h = H::new();
    h.gerrit(42, "I1", "main", ChangeStatus::New, None);
    h.run(&unknown);
    assert_eq!(h.rows(), s(&["- I1 checking g42 [c1]"]));
    let mut h = H::new();
    h.gerrit(42, "I1", "main", ChangeStatus::New, None);
    h.gerrit(43, "I1", "stable", ChangeStatus::New, None);
    h.run(&unknown);
    assert_eq!(h.rows(), s(&["- I1 checking [c1]"]));

    // A local change that turns out to be on Gerrit (pushed, then amended)
    // becomes local patch sets of it.
    let mut h = H::new();
    h.run(&sc);
    h.gerrit(42, "I1", "main", ChangeStatus::New, Some(&["g1"]));
    h.run(&sc);
    assert_eq!(h.rows(), s(&["main I1 checking g42 [c1]"]));
}

#[test]
fn a_closed_change_id_is_skipped_unless_a_row_has_unsent_data() {
    let sc = scan(vec![commit("c1", Some("I1"), &["topic"])]);
    for status in [ChangeStatus::Merged, ChangeStatus::Abandoned] {
        // Nothing new for a closed change.
        let mut h = H::new();
        h.gerrit(42, "I1", "main", status.clone(), Some(&["g1"]));
        assert_eq!(h.plan(&sc), []);

        // An existing row without unsent data goes.
        let mut h = H::new();
        h.run(&sc);
        h.gerrit(42, "I1", "main", status.clone(), Some(&["g1"]));
        assert!(h.run(&sc).is_settled());
        assert_eq!(h.rows(), Vec::<String>::new());

        // One with unsent data is closed, and stays so.
        let mut h = H::new();
        h.run(&sc);
        h.add_draft("I1");
        h.gerrit(42, "I1", "main", status, Some(&["g1"]));
        let e = h.entry("I1");
        let ops = h.plan(&sc);
        assert!(matches!(
            &ops[..],
            [LocalOp::Upsert(u)] if u.seen == Some(seen(&e))
                && u.state == Some(LocalChangeState::Closed)
                && u.gerrit_number == Some(ChangeNumber(42))
                && u.patch_sets.is_empty()
        ));
        assert!(h.run(&sc).is_settled());
        // Closed with its Gerrit change, where its held data shows.
        assert_eq!(h.rows(), s(&["main I1 closed g42 [c1]"]));
        assert_eq!(
            h.store
                .local_changes_of_gerrit_change(h.server, ChangeNumber(42))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            h.store
                .load_drafts(h.server, ChangeNumber(42))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(h.plan(&sc), []);
        // Gone from the branches it stays too.
        assert_eq!(h.plan(&scan(vec![])), []);
    }
}

#[test]
fn only_my_own_single_parent_commits_with_a_change_id_count() {
    let mut other = commit("o1", Some("IO"), &["topic"]);
    other.commit.author.email = "someone@example.com".into();
    let mut merge = commit("m1", Some("IM"), &["topic"]);
    merge.commit.parents.push("p2".into());
    let mut shouting = commit("u1", Some("IU"), &["topic"]);
    shouting.commit.author.email = "  ME@Example.COM ".into();
    let sc = scan(vec![
        other,
        merge,
        commit("n1", None, &["topic"]),
        shouting,
        commit("c1", Some("I1"), &["topic"]),
    ]);
    let mut h = H::new();
    h.run(&sc);
    assert_eq!(
        h.rows(),
        s(&["main I1 checking [c1]", "main IU checking [u1]"])
    );
    // The checkout's own email counts as well as the account's, also
    // written differently.
    let mut h = H::new();
    h.emails = vec!["me@other.example".into(), " SOMEONE@example.com".into()];
    h.run(&sc);
    assert_eq!(h.rows(), s(&["main IO checking [o1]"]));
    // Without emails nothing is mine.
    let mut h = H::new();
    h.emails = vec![" ".into()];
    assert_eq!(h.plan(&sc), []);
}

#[test]
fn a_change_vanishes_only_when_all_its_commits_left_on_a_complete_scan() {
    let mut h = H::new();
    h.run(&scan(vec![commit("c1", Some("I1"), &["topic"])]));
    h.run(&scan(vec![commit("c1b", Some("I1"), &["topic"])]));
    // The amend took c1 off the branch: the change lives on, with both
    // versions.
    assert_eq!(h.rows(), s(&["main I1 checking [c1 c1b]"]));

    // Truncated or remote-less scans never vanish anything.
    let mut truncated = scan(vec![]);
    truncated.truncated = true;
    let mut no_remotes = scan(vec![]);
    no_remotes.no_remotes = true;
    for sc in [&truncated, &no_remotes] {
        assert_eq!(h.plan(sc), []);
    }

    let e = h.entry("I1");
    let ops = h.plan(&scan(vec![]));
    assert_eq!(
        ops,
        [LocalOp::Delete {
            seen: seen(&e),
            else_state: Some(LocalChangeState::Vanished),
        }]
    );
    let report = h.apply(&ops);
    assert_eq!(report.deleted, [e.change.id]);
    assert!(report.is_settled());
    assert_eq!(h.rows(), Vec::<String>::new());
}

#[test]
fn a_vanished_change_with_unsent_data_stays_and_comes_back() {
    let mut h = H::new();
    let sc = scan(vec![commit("c1", Some("I1"), &["topic"])]);
    h.run(&sc);
    h.add_draft("I1");
    let e = h.entry("I1");
    // Known to have unsent data: marked vanished at once, and settled.
    let ops = h.plan(&scan(vec![]));
    assert_eq!(
        ops,
        [LocalOp::SetState {
            seen: seen(&e),
            state: LocalChangeState::Vanished,
        }]
    );
    assert!(h.apply(&ops).is_settled());
    assert_eq!(h.rows(), s(&["main I1 vanished [c1]"]));
    // The next scan of the same refs has nothing more to do.
    assert_eq!(h.plan(&scan(vec![])), []);
    // Back on a branch: checking again.
    assert!(h.run(&sc).is_settled());
    assert_eq!(h.rows(), s(&["main I1 checking [c1]"]));
}

#[test]
fn a_tombstone_is_never_resurrected() {
    let mut h = H::new();
    let sc = scan(vec![commit("c1", Some("I1"), &["topic"])]);
    h.run(&sc);
    h.set_state("I1", LocalChangeState::Consolidated);
    // Its commit still on the branch, whatever the cache says.
    assert_eq!(h.plan(&sc), []);
    h.gerrit(42, "I1", "main", ChangeStatus::New, None);
    assert_eq!(h.plan(&sc), []);
    h.gerrit(42, "I1", "main", ChangeStatus::Merged, None);
    assert_eq!(h.plan(&sc), []);
    // Truncated: it may still be there.
    let mut truncated = scan(vec![]);
    truncated.truncated = true;
    assert_eq!(h.plan(&truncated), []);
    // Its commit on another branch with another target keeps it too.
    assert_eq!(h.plan(&scan(vec![commit("c1", Some("I1"), &["fix"])])), []);
    // Gone from the branches, but young: it stays for its redirects.
    assert_eq!(h.plan(&scan(vec![])), []);
    // Old enough: the tombstone goes.
    let e = h.entry("I1");
    let mut entries = h.entries();
    age_tombstones(&mut entries);
    assert_eq!(
        plan_reconcile(&scan(vec![]), &h.emails, &h.cached, &entries, now()),
        [LocalOp::Delete {
            seen: seen(&e),
            else_state: None,
        }]
    );
    // Still on a branch, however old: kept, so it is not made local again.
    assert_eq!(
        plan_reconcile(&sc, &h.emails, &h.cached, &entries, now()),
        []
    );
}

/// Makes the tombstones of `entries` older than [`TOMBSTONE_AGE`] at
/// `now()`.
fn age_tombstones(entries: &mut [LocalChangeEntry]) {
    for e in entries {
        if e.change.state == LocalChangeState::Consolidated {
            e.consolidated_at = Some(
                now()
                    .checked_sub(TOMBSTONE_AGE + jiff::SignedDuration::from_secs(1))
                    .unwrap(),
            );
        }
    }
}

#[test]
fn tombstones_come_of_age_after_a_week() {
    let mut h = H::new();
    h.run(&scan(vec![commit("c1", Some("I1"), &["topic"])]));
    h.set_state("I1", LocalChangeState::Consolidated);
    let mut e = h.entry("I1");
    assert!(e.consolidated_at.is_some());
    let day = jiff::SignedDuration::from_hours(24);
    e.consolidated_at = Some(now().checked_sub(day * 6).unwrap());
    assert!(!tombstone_due(&e, now()));
    e.consolidated_at = Some(now().checked_sub(day * 7).unwrap());
    assert!(tombstone_due(&e, now()));
    // Made before its time was recorded (in an earlier run).
    e.consolidated_at = None;
    assert!(tombstone_due(&e, now()));
}

#[test]
fn a_tombstone_takes_an_amend_after_the_push_as_a_new_local_patch_set() {
    // Consolidation found c1 as patch set 1 of change 42 and left a
    // tombstone.
    let mut h2 = H::new();
    h2.gerrit(42, "I1", "main", ChangeStatus::New, Some(&["c1"]));
    h2.store
        .reconcile_local(
            h2.server,
            "p",
            &[LocalOp::Upsert(LocalChangeUpsert {
                seen: None,
                branch: Some("main".into()),
                change_id: "I1".into(),
                gerrit_number: Some(ChangeNumber(42)),
                subject: "Subject c1".into(),
                author_name: "Me".into(),
                author_email: ME.into(),
                updated: t(2),
                state: Some(LocalChangeState::Consolidated),
                patch_sets: vec![NewLocalPatchSet {
                    sha: "c1".into(),
                    parent_sha: None,
                    seen_at: now(),
                }],
            })],
            now(),
        )
        .unwrap();
    h2.run(&scan(vec![commit("c1", Some("I1"), &["topic"])]));
    assert_eq!(h2.rows(), s(&["main I1 consolidated g42 [c1=ps1]"]));
    // The user amends: a local patch set of change 42 again.
    let report = h2.run(&scan(vec![commit("c2", Some("I1"), &["topic"])]));
    assert!(report.is_settled());
    assert_eq!(h2.rows(), s(&["main I1 checking g42 [c1=ps1 c2]"]));
}

#[test]
fn a_changed_target_branch_rekeys_the_change() {
    let mut h = H::new();
    h.run(&scan(vec![commit("c1", Some("I1"), &["topic"])]));
    h.add_draft("I1");
    let before = h.entry("I1");
    // `topic` now tracks stable.
    let mut sc = scan(vec![commit("c1", Some("I1"), &["topic"])]);
    sc.targets.upstreams.get_mut("topic").unwrap().branch = "stable".into();
    // Not on a truncated scan: the old key may still be on a branch, and
    // the commit gets no second row either.
    let mut truncated = sc.clone();
    truncated.truncated = true;
    assert_eq!(h.plan(&truncated), []);

    let ops = h.plan(&sc);
    assert!(
        matches!(&ops[..], [LocalOp::Upsert(u)] if u.seen == Some(seen(&before)) && u.branch.as_deref() == Some("stable") && u.patch_sets.is_empty())
    );
    assert!(h.apply(&ops).is_settled());
    assert_eq!(h.rows(), s(&["stable I1 checking [c1]"]));
    assert_eq!(h.entry("I1").change.id, before.change.id);
}

#[test]
fn a_rekey_onto_an_existing_row_merges_them() {
    let mut h = H::new();
    // A truncated scan saw only the newer version, under its new target,
    // and made a second row for it.
    h.run(&scan(vec![commit("c1", Some("I1"), &["topic"])]));
    h.add_draft("I1");
    let mut truncated = scan(vec![commit("c2", Some("I1"), &["fix"])]);
    truncated.truncated = true;
    h.run(&truncated);
    assert_eq!(
        h.rows(),
        s(&["main I1 checking [c1]", "stable I1 checking [c2]"])
    );
    let entries = h.entries();
    let main = entries
        .iter()
        .find(|e| e.change.branch.as_deref() == Some("main"))
        .unwrap();
    let stable = entries
        .iter()
        .find(|e| e.change.branch.as_deref() == Some("stable"))
        .unwrap();

    // The complete scan sees both versions headed for stable.
    let sc = scan(vec![
        commit("c2", Some("I1"), &["fix"]),
        commit("c1", Some("I1"), &["fix-old"]),
    ]);
    let mut sc = sc;
    sc.targets.upstreams.insert(
        "fix-old".into(),
        Upstream {
            remote: "origin".into(),
            branch: "stable".into(),
        },
    );
    let ops = h.plan(&sc);
    assert_eq!(
        ops.last(),
        Some(&LocalOp::Merge {
            from: seen(main),
            into: seen(stable),
        })
    );
    let report = h.apply(&ops);
    assert!(report.is_settled());
    assert_eq!(report.merged, [(main.change.id, stable.change.id)]);
    assert_eq!(h.rows(), s(&["stable I1 checking [c2 c1]"]));
    // The draft followed.
    assert!(h.entry("I1").has_unsent);
}

#[test]
fn a_vanished_row_merged_into_its_new_key_comes_back_checking() {
    let mut h = H::new();
    h.run(&scan(vec![commit("c1", Some("I1"), &["topic"])]));
    h.add_draft("I1");
    h.run(&scan(vec![]));
    let old = h.entry("I1");
    assert_eq!(old.change.state, LocalChangeState::Vanished);
    // c1 is back, now headed for stable.
    let report = h.run(&scan(vec![commit("c1", Some("I1"), &["fix"])]));
    assert!(report.is_settled());
    assert_eq!(h.rows(), s(&["stable I1 checking [c1]"]));
    assert_eq!(h.entry("I1").change.id, old.change.id);
}

#[test]
fn the_option_off_removes_what_has_no_unsent_data_and_adds_nothing() {
    let mut h = H::new();
    h.run(&scan(vec![
        commit("a1", Some("IA"), &["topic"]),
        commit("b1", Some("IB"), &["topic"]),
        commit("t1", Some("IT"), &["topic"]),
    ]));
    h.add_draft("IB");
    h.set_state("IT", LocalChangeState::Consolidated);
    let mut entries = h.entries();
    // A young tombstone stays for its redirects.
    let young: Vec<LocalChangeId> = plan_disabled(&entries, now())
        .iter()
        .map(|op| match op {
            LocalOp::Delete { seen, .. } => seen.id,
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(young, [h.entry("IA").change.id]);
    age_tombstones(&mut entries);
    let ops = plan_disabled(&entries, now());
    let deleted: Vec<LocalChangeId> = ops
        .iter()
        .map(|op| match op {
            LocalOp::Delete {
                seen,
                else_state: None,
            } => seen.id,
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(deleted.len(), 2);
    assert!(h.apply(&ops).is_settled());
    assert_eq!(h.rows(), s(&["main IB checking [b1]"]));
}

#[test]
fn a_plan_older_than_the_store_is_applied_only_where_it_still_fits() {
    let mut h = H::new();
    h.run(&scan(vec![
        commit("a1", Some("IA"), &["topic"]),
        commit("b1", Some("IB"), &["topic"]),
    ]));
    // Both vanish; meanwhile consolidation looked IA up (now local) and the
    // user commented on IB.
    let ops = h.plan(&scan(vec![]));
    assert_eq!(ops.len(), 2);
    h.set_state("IA", LocalChangeState::Local);
    h.add_draft("IB");
    let report = h.apply(&ops);
    assert_eq!(report.stale, 1);
    assert_eq!(report.kept, [h.entry("IB").change.id]);
    assert!(!report.is_settled());
    assert_eq!(
        h.rows(),
        s(&["main IA local [a1]", "main IB vanished [b1]"])
    );

    // An amend planned before a consolidation changed the row's state is
    // stale too: the new version waits for the next scan.
    let mut h = H::new();
    h.run(&scan(vec![commit("c1", Some("I1"), &["topic"])]));
    let ops = h.plan(&scan(vec![commit("c2", Some("I1"), &["topic"])]));
    h.set_state("I1", LocalChangeState::Local);
    let report = h.apply(&ops);
    assert_eq!((report.stale, report.patch_sets_added), (1, 0));
    assert_eq!(h.rows(), s(&["main I1 local [c1]"]));
    // The rescan picks it up.
    assert!(
        h.run(&scan(vec![commit("c2", Some("I1"), &["topic"])]))
            .is_settled()
    );
    assert_eq!(h.rows(), s(&["main I1 local [c1 c2]"]));
}

#[test]
fn a_change_with_only_pushed_commits_left_waits_for_consolidation() {
    let mut h = H::new();
    let sc = scan(vec![commit("c1", Some("I1"), &["topic"])]);
    h.run(&sc);
    h.set_state("I1", LocalChangeState::Local);
    h.add_draft("I1");
    h.gerrit(42, "I1", "main", ChangeStatus::New, Some(&["c1"]));
    // Mapped, not vanished, not deleted.
    let report = h.run(&sc);
    assert!(report.is_settled());
    assert_eq!(h.rows(), s(&["main I1 local [c1=ps1]"]));
    // Off the branches as well: vanished, for consolidation to decide.
    h.run(&scan(vec![]));
    assert_eq!(h.rows(), s(&["main I1 vanished [c1=ps1]"]));
}

#[test]
fn a_pushed_version_of_a_gerrit_change_leaves_a_tombstone() {
    // c1 is a local patch set of change 42 (which has only PS 1, b1).
    let mut h = H::new();
    h.gerrit(42, "I1", "main", ChangeStatus::New, Some(&["b1"]));
    let sc = scan(vec![commit("c1", Some("I1"), &["topic"])]);
    h.run(&sc);
    assert_eq!(h.rows(), s(&["main I1 checking g42 [c1]"]));
    // Pushed as PS 2: nothing is unsent, so it becomes a tombstone that
    // keeps its Gerrit change, for the pages that name its numbers.
    h.cached.clear();
    h.gerrit(42, "I1", "main", ChangeStatus::New, Some(&["b1", "c1"]));
    let report = h.run(&sc);
    assert!(report.is_settled(), "{report:?}");
    assert_eq!(h.rows(), s(&["main I1 consolidated g42 [c1=ps2]"]));
    assert_eq!(h.plan(&sc), []);
}

#[test]
fn commits_that_stop_passing_the_filters_are_still_there() {
    let mut h = H::new();
    h.run(&scan(vec![
        commit("c2", Some("I2"), &["topic"]),
        commit("c1", Some("I1"), &["topic"]),
    ]));
    h.add_draft("I1");
    // The emails changed (or the account's is not loaded yet): the commits
    // no longer count as mine, but they are still on the branches.
    let mut other = commit("c1", Some("I1"), &["topic"]);
    other.commit.author.email = "old@example.com".into();
    let mut amended = commit("c2b", Some("I2"), &["topic"]);
    amended.commit.author.email = "old@example.com".into();
    let sc = scan(vec![amended, other]);
    assert_eq!(h.plan(&sc), []);
    // Without any email nothing vanishes, whatever the scan.
    h.emails.clear();
    assert_eq!(h.plan(&scan(vec![])), []);
    h.emails = vec![" ".into()];
    assert_eq!(h.plan(&scan(vec![])), []);
    assert_eq!(
        h.rows(),
        s(&["main I1 checking [c1]", "main I2 checking [c2]"])
    );
}

#[test]
fn a_tombstone_with_unsent_data_is_left_alone() {
    let mut h = H::new();
    h.run(&scan(vec![commit("c1", Some("I1"), &["topic"])]));
    h.set_state("I1", LocalChangeState::Consolidated);
    // A draft reached it late.
    h.add_draft("I1");
    assert_eq!(h.plan(&scan(vec![])), []);
}

#[test]
fn a_closed_row_without_unsent_data_goes_with_its_commits() {
    let mut h = H::new();
    let sc = scan(vec![commit("c1", Some("I1"), &["topic"])]);
    h.run(&sc);
    h.add_draft("I1");
    h.gerrit(42, "I1", "main", ChangeStatus::Merged, None);
    h.run(&sc);
    assert_eq!(h.rows(), s(&["main I1 closed g42 [c1]"]));
    // The user dropped the held draft and the change left the cache.
    let e = h.entry("I1");
    let drafts = h.store.load_drafts(h.server, ChangeNumber(42)).unwrap();
    for d in drafts {
        h.store.delete_draft(h.server, d.local_id).unwrap();
    }
    assert!(!h.entry("I1").has_unsent);
    h.cached.clear();
    let report = h.run(&scan(vec![]));
    assert_eq!(report.deleted, [e.change.id]);
}

#[test]
fn a_truncated_scan_does_not_copy_a_rekeyed_commit() {
    let mut h = H::new();
    h.run(&scan(vec![commit("c1", Some("I1"), &["topic"])]));
    // `topic` now tracks stable, seen by a truncated scan.
    let mut sc = scan(vec![commit("c1", Some("I1"), &["topic"])]);
    sc.targets.upstreams.get_mut("topic").unwrap().branch = "stable".into();
    sc.truncated = true;
    assert_eq!(h.plan(&sc), []);
    // The complete scan re-keys it.
    sc.truncated = false;
    h.run(&sc);
    assert_eq!(h.rows(), s(&["stable I1 checking [c1]"]));
}
