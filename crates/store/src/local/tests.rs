use std::collections::BTreeMap;

use model::{
    Account, AccountId, ChangeStatus, DraftInput, LocalApproval, Location, PendingReviewState,
    Revision, RevisionKind, Side,
};

use super::*;
use crate::NewPendingReview;

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

const T0: &str = "2024-03-01T10:00:00Z";
const T1: &str = "2024-03-01T11:00:00Z";
const T2: &str = "2024-03-01T12:00:00Z";

fn setup() -> (Store, ServerId) {
    let mut s = Store::open_in_memory().unwrap();
    let id = s.server_id("https://g.example").unwrap();
    (s, id)
}

fn new_ps(sha: &str) -> NewLocalPatchSet {
    NewLocalPatchSet {
        sha: sha.into(),
        parent_sha: Some("base".into()),
        seen_at: ts(T0),
    }
}

fn upsert(change_id: &str, branch: Option<&str>, shas: &[&str]) -> LocalChangeUpsert {
    LocalChangeUpsert {
        seen: None,
        branch: branch.map(str::to_owned),
        change_id: change_id.into(),
        gerrit_number: None,
        subject: format!("Subject of {change_id}"),
        author_name: "Ann".into(),
        author_email: "ann@example.com".into(),
        updated: ts(T0),
        state: None,
        patch_sets: shas.iter().map(|s| new_ps(s)).collect(),
    }
}

/// Reconciles project `p` with one upsert and returns the row.
fn add(s: &mut Store, server: ServerId, u: LocalChangeUpsert) -> LocalChange {
    let key = (u.branch.clone(), u.change_id.clone());
    s.reconcile_local(server, "p", &[LocalOp::Upsert(u)], ts(T1))
        .unwrap();
    s.local_changes_of_project(server, "p")
        .unwrap()
        .into_iter()
        .map(|e| e.change)
        .find(|c| (c.branch.clone(), c.change_id.clone()) == key)
        .unwrap()
}

fn draft_input(message: &str) -> DraftInput {
    DraftInput {
        path: "src/a.rs".into(),
        side: Side::Revision,
        parent: None,
        location: Location::Line(7),
        in_reply_to: None,
        message: message.into(),
        unresolved: true,
    }
}

fn psref(p: &LocalPatchSet) -> PatchSetRef {
    PatchSetRef {
        number: p.number,
        sha: p.sha.clone(),
    }
}

fn gerrit_ps(n: u32) -> PatchSetRef {
    PatchSetRef {
        number: PatchSetNumber(n),
        sha: format!("gerrit{n}").as_str().into(),
    }
}

fn review(p: &LocalPatchSet) -> NewPendingReview {
    NewPendingReview {
        patch_set: p.number,
        labels: BTreeMap::from([("Code-Review".to_owned(), 2)]),
        strategy: LocalApproval::Follow,
        message: "LGTM".into(),
        unresolved: false,
        created: ts(T1),
    }
}

fn seen(c: &LocalChange) -> Seen {
    Seen {
        id: c.id,
        state: c.state,
    }
}

fn delete(c: &LocalChange) -> LocalOp {
    LocalOp::Delete {
        seen: seen(c),
        else_state: None,
    }
}

#[test]
fn reconcile_inserts_numbers_and_orders_versions() {
    let (mut s, id) = setup();
    let c = add(&mut s, id, upsert("I1", Some("main"), &["a1", "a2"]));
    assert_eq!(c.state, LocalChangeState::Checking);
    assert_eq!(c.number, c.id.number().unwrap());
    assert!(c.number.is_local() && c.is_purely_local());
    assert_eq!(
        (c.project.as_str(), c.branch.as_deref(), c.first_seen),
        ("p", Some("main"), ts(T1))
    );
    let ps = s.local_patch_sets(id, c.id).unwrap();
    let shas: Vec<_> = ps.iter().map(|p| (p.sha.as_str(), p.ordinal)).collect();
    assert_eq!(shas, [("a1", 1), ("a2", 2)]);
    assert!(
        ps.iter()
            .all(|p| p.number.is_local() && p.local_change == c.id)
    );
    assert_eq!(
        ps[0].parent_sha.as_ref().map(RevisionId::as_str),
        Some("base")
    );
}

#[test]
fn reconcile_updates_deletes_and_counts_in_one_transaction() {
    let (mut s, id) = setup();
    let a = add(&mut s, id, upsert("IA", Some("main"), &["a1"]));
    let b = add(&mut s, id, upsert("IB", Some("main"), &["b1"]));
    let unchanged = add(&mut s, id, upsert("IC", None, &["c1"]));

    let mut amend = upsert("IA", Some("main"), &["a1", "a2"]);
    amend.subject = "Amended".into();
    amend.updated = ts(T2);
    let ops = [
        LocalOp::Upsert(amend),
        LocalOp::Upsert(upsert("IC", None, &["c1"])),
        LocalOp::Upsert(upsert("ID", None, &["d1"])),
        delete(&b),
        LocalOp::SetState {
            seen: seen(&unchanged),
            state: LocalChangeState::Local,
        },
    ];
    let r = s.reconcile_local(id, "p", &ops, ts(T2)).unwrap();
    assert_eq!(r.inserted.len(), 1);
    assert_eq!(r.updated, [a.id, unchanged.id]);
    assert_eq!(r.deleted, [b.id]);
    assert_eq!((r.patch_sets_added, r.stale), (2, 0));
    assert!(r.kept.is_empty() && r.changed());
    let a = s.local_change(id, a.id).unwrap().unwrap();
    assert_eq!((a.subject.as_str(), a.updated), ("Amended", ts(T2)));
    // The first-seen time stays.
    assert_eq!(a.first_seen, ts(T1));
    assert_eq!(s.local_patch_sets(id, a.id).unwrap().len(), 2);
    assert_eq!(s.local_change(id, b.id).unwrap(), None);
    assert_eq!(s.local_patch_sets(id, b.id).unwrap(), []);
    // Applying the same plan again changes nothing.
    let again = [LocalOp::Upsert(upsert("ID", None, &["d1"]))];
    let r = s.reconcile_local(id, "p", &again, ts(T2)).unwrap();
    assert!(!r.changed(), "{r:?}");
    // Ops on a row that is gone are skipped as stale.
    let r = s.reconcile_local(id, "p", &[delete(&b)], ts(T2)).unwrap();
    assert_eq!((r.stale, r.changed()), (1, false));
}

#[test]
fn a_plan_naming_local_targets_or_a_removed_server_writes_nothing() {
    let (mut s, id) = setup();
    let a = add(&mut s, id, upsert("IA", None, &["a1"]));
    let a1 = s.local_patch_sets(id, a.id).unwrap().remove(0);
    let mut onto_local = upsert("IZ", None, &["z1"]);
    onto_local.gerrit_number = Some(a.number);
    let map_local = LocalOp::MapPatchSet {
        patch_set: a1.id,
        gerrit_patch_set: a1.number,
    };
    for ops in [
        vec![
            LocalOp::Upsert(upsert("IY", None, &[])),
            LocalOp::Upsert(onto_local),
        ],
        vec![LocalOp::Upsert(upsert("IY", None, &[])), map_local],
    ] {
        assert!(matches!(
            s.reconcile_local(id, "p", &ops, ts(T2)),
            Err(Error::LocalTarget)
        ));
    }
    assert_eq!(s.local_changes(id).unwrap().len(), 1);
    // A removed server is reported, not silently recreated.
    s.delete_server(id).unwrap();
    assert!(matches!(
        s.reconcile_local(id, "p", &[LocalOp::Upsert(upsert("IY", None, &[]))], ts(T2)),
        Err(Error::NoSuchServer)
    ));
}

#[test]
fn ops_on_rows_that_changed_since_the_snapshot_are_stale() {
    let (mut s, id) = setup();
    let a = add(&mut s, id, upsert("IA", None, &["a1"]));
    let snapshot = seen(&a);
    // Consolidation moves the row on meanwhile.
    s.set_local_change_state(id, a.id, LocalChangeState::Local)
        .unwrap();
    let mut amend = upsert("IA", None, &["a2"]);
    amend.seen = Some(snapshot);
    let ops = [
        LocalOp::Upsert(amend),
        LocalOp::SetState {
            seen: snapshot,
            state: LocalChangeState::Vanished,
        },
        LocalOp::Delete {
            seen: snapshot,
            else_state: None,
        },
    ];
    let r = s.reconcile_local(id, "p", &ops, ts(T2)).unwrap();
    assert_eq!((r.stale, r.changed(), r.is_settled()), (3, false, false));
    let now = s.local_change(id, a.id).unwrap().unwrap();
    assert_eq!(now.state, LocalChangeState::Local);
    assert_eq!(s.local_patch_sets(id, a.id).unwrap().len(), 1);
}

#[test]
fn an_upsert_whose_row_is_gone_still_records_the_commits() {
    let (mut s, id) = setup();
    let a = add(&mut s, id, upsert("IA", None, &["a1"]));
    s.delete_local_change(id, a.id).unwrap();
    let mut u = upsert("IA", None, &["a1", "a2"]);
    u.seen = Some(seen(&a));
    let r = s
        .reconcile_local(id, "p", &[LocalOp::Upsert(u)], ts(T2))
        .unwrap();
    assert_eq!((r.inserted.len(), r.patch_sets_added, r.stale), (1, 2, 0));
    assert_ne!(r.inserted[0], a.id);
}

#[test]
fn a_refused_delete_can_leave_the_change_vanished() {
    let (mut s, id) = setup();
    let c = add(&mut s, id, upsert("IL", None, &["l1"]));
    let l1 = s.local_patch_sets(id, c.id).unwrap().remove(0);
    s.create_draft(id, c.number, &psref(&l1), &draft_input("x"), ts(T1))
        .unwrap();
    let op = LocalOp::Delete {
        seen: seen(&c),
        else_state: Some(LocalChangeState::Vanished),
    };
    let r = s.reconcile_local(id, "p", &[op], ts(T2)).unwrap();
    assert_eq!(
        (r.kept.clone(), r.updated.clone()),
        (vec![c.id], vec![c.id])
    );
    assert!(!r.is_settled());
    assert_eq!(
        s.local_change(id, c.id).unwrap().unwrap().state,
        LocalChangeState::Vanished
    );
}

#[test]
fn a_row_can_be_rekeyed_to_another_branch() {
    let (mut s, id) = setup();
    let a = add(&mut s, id, upsert("IA", None, &["a1"]));
    let mut rekey = upsert("IA", Some("main"), &[]);
    rekey.seen = Some(seen(&a));
    let r = s
        .reconcile_local(id, "p", &[LocalOp::Upsert(rekey)], ts(T2))
        .unwrap();
    assert_eq!(r.updated, [a.id]);
    let a2 = s.local_change(id, a.id).unwrap().unwrap();
    assert_eq!((a2.number, a2.branch.as_deref()), (a.number, Some("main")));
}

#[test]
fn the_key_has_one_row_per_unknown_branch() {
    let (mut s, id) = setup();
    let a = add(&mut s, id, upsert("IA", None, &["a1"]));
    let again = add(&mut s, id, upsert("IA", None, &["a2"]));
    assert_eq!(a.id, again.id);
    let main = add(&mut s, id, upsert("IA", Some("main"), &["a1"]));
    assert_ne!(main.id, a.id);
    assert_eq!(s.local_changes_by_change_id(id, "IA").unwrap().len(), 2);
    // Another project is another key.
    s.reconcile_local(id, "q", &[LocalOp::Upsert(upsert("IA", None, &[]))], ts(T1))
        .unwrap();
    assert_eq!(s.local_changes_by_change_id(id, "IA").unwrap().len(), 3);
    // And the same commit may be a version in both projects.
    assert_eq!(
        s.local_patch_sets_by_sha(id, &"a1".into()).unwrap().len(),
        2
    );
}

#[test]
fn ids_and_numbers_are_never_reused() {
    let (mut s, id) = setup();
    let a = add(&mut s, id, upsert("IA", None, &["a1"]));
    let a_ps = s.local_patch_sets(id, a.id).unwrap()[0].clone();
    assert!(s.delete_local_change(id, a.id).unwrap());
    let b = add(&mut s, id, upsert("IA", None, &["a1"]));
    let b_ps = s.local_patch_sets(id, b.id).unwrap()[0].clone();
    assert!(b.id > a.id && b.number != a.number);
    assert!(b_ps.id > a_ps.id && b_ps.number != a_ps.number);
    // The ordinal restarts: it counts versions within one change.
    assert_eq!(b_ps.ordinal, 1);
    // Not even after the server is gone and the tables were emptied.
    s.delete_server(id).unwrap();
    let id = s.server_id("https://g.example").unwrap();
    let c = add(&mut s, id, upsert("IA", None, &["a1"]));
    assert!(c.id > b.id);
    assert!(s.local_patch_sets(id, c.id).unwrap()[0].id > b_ps.id);
}

#[test]
fn lookups_by_number_sha_change_id_and_gerrit_change() {
    let (mut s, id) = setup();
    let local = add(&mut s, id, upsert("IL", Some("main"), &["l1"]));
    let mut of_gerrit = upsert("IG", Some("main"), &["g-local"]);
    of_gerrit.gerrit_number = Some(ChangeNumber(7));
    of_gerrit.state = Some(LocalChangeState::Local);
    let of_gerrit = add(&mut s, id, of_gerrit);

    assert_eq!(
        s.local_change_by_number(id, local.number).unwrap(),
        Some(local.clone())
    );
    assert_eq!(s.local_change_by_number(id, ChangeNumber(7)).unwrap(), None);
    let ps = s.local_patch_sets(id, of_gerrit.id).unwrap();
    assert_eq!(
        s.local_patch_set(id, ps[0].number).unwrap(),
        Some(ps[0].clone())
    );
    assert_eq!(s.local_patch_set(id, PatchSetNumber(1)).unwrap(), None);
    assert_eq!(
        s.local_patch_sets_by_sha(id, &"g-local".into()).unwrap(),
        ps
    );
    assert_eq!(
        s.local_changes_by_change_id(id, "IL").unwrap(),
        std::slice::from_ref(&local)
    );
    assert_eq!(
        s.local_changes_of_gerrit_change(id, ChangeNumber(7))
            .unwrap(),
        std::slice::from_ref(&of_gerrit)
    );
    assert_eq!(
        s.local_patch_sets_of_gerrit_change(id, ChangeNumber(7))
            .unwrap(),
        ps
    );
    // The dashboard lists changes of their own only.
    assert_eq!(
        s.dashboard_local_changes(id).unwrap(),
        std::slice::from_ref(&local)
    );
    // Another server sees none of it.
    let other = s.server_id("https://b.example").unwrap();
    assert_eq!(s.local_change(other, local.id).unwrap(), None);
    assert!(!s.is_local_sha(other, &"l1".into()).unwrap());

    // The guard: a local commit, until it is mapped to a Gerrit patch set.
    assert!(s.is_local_sha(id, &"g-local".into()).unwrap());
    assert!(!s.is_local_sha(id, &"elsewhere".into()).unwrap());
    let r = s
        .reconcile_local(
            id,
            "p",
            &[LocalOp::MapPatchSet {
                patch_set: ps[0].id,
                gerrit_patch_set: PatchSetNumber(4),
            }],
            ts(T2),
        )
        .unwrap();
    assert_eq!(r.patch_sets_mapped, 1);
    assert!(!s.is_local_sha(id, &"g-local".into()).unwrap());
    assert_eq!(
        s.local_patch_set(id, ps[0].number)
            .unwrap()
            .unwrap()
            .gerrit_patch_set,
        Some(PatchSetNumber(4))
    );
    // A row of another project is out of reach of this project's plan.
    let r = s
        .reconcile_local(
            id,
            "q",
            &[
                LocalOp::MapPatchSet {
                    patch_set: ps[0].id,
                    gerrit_patch_set: PatchSetNumber(5),
                },
                delete(&local),
            ],
            ts(T2),
        )
        .unwrap();
    assert_eq!((r.patch_sets_mapped, r.stale), (0, 2));
    assert!(s.local_change(id, local.id).unwrap().is_some());
}

#[test]
fn the_dashboard_follows_the_state() {
    let (mut s, id) = setup();
    let mut ids = Vec::new();
    for (i, state) in LocalChangeState::ALL.into_iter().enumerate() {
        let mut u = upsert(&format!("I{i}"), None, &[]);
        u.state = Some(state);
        u.updated = ts(T0) + jiff::SignedDuration::from_secs(i64::try_from(i).unwrap());
        ids.push((state, add(&mut s, id, u).id));
    }
    let listed: Vec<_> = s
        .dashboard_local_changes(id)
        .unwrap()
        .into_iter()
        .map(|c| c.state)
        .collect();
    // Newest first.
    use LocalChangeState::*;
    assert_eq!(listed, [Vanished, Ambiguous, Local, Checking]);
    assert_eq!(s.local_changes(id).unwrap().len(), 6);
}

#[test]
fn unsent_data_is_drafts_on_local_numbers_and_unsent_reviews() {
    let (mut s, id) = setup();
    let local = add(&mut s, id, upsert("IL", None, &["l1"]));
    let mut g = upsert("IG", None, &["g1"]);
    g.gerrit_number = Some(ChangeNumber(7));
    let g = add(&mut s, id, g);
    let l1 = s.local_patch_sets(id, local.id).unwrap()[0].clone();
    let g1 = s.local_patch_sets(id, g.id).unwrap()[0].clone();
    assert!(!s.has_unsent(id, local.id).unwrap());
    assert!(!s.has_unsent(id, g.id).unwrap());

    // A normal draft on the Gerrit change is not this change's data.
    s.create_draft(
        id,
        ChangeNumber(7),
        &gerrit_ps(1),
        &draft_input("x"),
        ts(T1),
    )
    .unwrap();
    assert!(!s.has_unsent(id, g.id).unwrap());
    // A draft on its local patch set is.
    let d = s
        .create_draft(id, ChangeNumber(7), &psref(&g1), &draft_input("x"), ts(T1))
        .unwrap();
    assert!(s.has_unsent(id, g.id).unwrap());
    s.delete_draft(id, d.local_id).unwrap();
    assert!(!s.has_unsent(id, g.id).unwrap());

    // A draft on the synthetic number.
    let d = s
        .create_draft(id, local.number, &psref(&l1), &draft_input("x"), ts(T1))
        .unwrap();
    assert!(s.has_unsent(id, local.id).unwrap());
    s.delete_draft(id, d.local_id).unwrap();

    // A pending review, until it is sent.
    let mut r = review(&l1);
    r.message = String::new();
    let r = s.create_pending_review(id, &r).unwrap();
    assert!(s.has_unsent(id, local.id).unwrap());
    let entry = &s.local_changes_of_project(id, "p").unwrap();
    assert!(
        entry
            .iter()
            .find(|e| e.change.id == local.id)
            .unwrap()
            .has_unsent
    );
    s.set_pending_review_state(
        id,
        r.id,
        &[PendingReviewState::Pending],
        PendingReviewState::Sent,
        None,
    )
    .unwrap();
    assert!(!s.has_unsent(id, local.id).unwrap());
    assert!(!s.has_unsent(id, LocalChangeId(999)).unwrap());
}

#[test]
fn a_change_with_unsent_data_is_neither_deleted_nor_consolidated_by_a_plan() {
    let (mut s, id) = setup();
    let c = add(&mut s, id, upsert("IL", None, &["l1"]));
    let l1 = s.local_patch_sets(id, c.id).unwrap()[0].clone();
    s.create_draft(id, c.number, &psref(&l1), &draft_input("x"), ts(T1))
        .unwrap();
    let ops = [
        LocalOp::SetState {
            seen: seen(&c),
            state: LocalChangeState::Consolidated,
        },
        delete(&c),
    ];
    let r = s.reconcile_local(id, "p", &ops, ts(T2)).unwrap();
    assert_eq!(r.kept, [c.id, c.id]);
    assert!(r.deleted.is_empty());
    let kept = s.local_change(id, c.id).unwrap().unwrap();
    assert_eq!(kept.state, LocalChangeState::Checking);
    // Outside a plan the same guard holds for Consolidated, while other
    // states are set.
    assert_eq!(
        s.set_local_change_state(id, c.id, LocalChangeState::Consolidated)
            .unwrap(),
        Some(LocalChangeState::Checking)
    );
    assert_eq!(
        s.set_local_change_state(id, c.id, LocalChangeState::Vanished)
            .unwrap(),
        Some(LocalChangeState::Vanished)
    );
    assert_eq!(
        s.set_local_change_state(id, LocalChangeId(999), LocalChangeState::Local)
            .unwrap(),
        None
    );
}

#[test]
fn deleting_a_local_change_removes_its_drafts_reviews_and_versions_only() {
    let (mut s, id) = setup();
    let local = add(&mut s, id, upsert("IL", None, &["l1"]));
    let other = add(&mut s, id, upsert("IO", None, &["o1"]));
    let mut g = upsert("IG", None, &["g1"]);
    g.gerrit_number = Some(ChangeNumber(7));
    let g = add(&mut s, id, g);
    let l1 = s.local_patch_sets(id, local.id).unwrap()[0].clone();
    let o1 = s.local_patch_sets(id, other.id).unwrap()[0].clone();
    let g1 = s.local_patch_sets(id, g.id).unwrap()[0].clone();

    let on_local = s
        .create_draft(id, local.number, &psref(&l1), &draft_input("l"), ts(T1))
        .unwrap();
    let on_other = s
        .create_draft(id, other.number, &psref(&o1), &draft_input("o"), ts(T1))
        .unwrap();
    let on_g1 = s
        .create_draft(id, ChangeNumber(7), &psref(&g1), &draft_input("g"), ts(T1))
        .unwrap();
    let on_gerrit = s
        .create_draft(
            id,
            ChangeNumber(7),
            &gerrit_ps(2),
            &draft_input("n"),
            ts(T1),
        )
        .unwrap();
    let r_local = s.create_pending_review(id, &review(&l1)).unwrap();
    let r_g = s.create_pending_review(id, &review(&g1)).unwrap();
    let r_other = s.create_pending_review(id, &review(&o1)).unwrap();

    assert!(s.delete_local_change(id, local.id).unwrap());
    assert_eq!(s.load_draft(id, on_local.local_id).unwrap(), None);
    assert_eq!(
        s.load_draft(id, r_local.message_draft.unwrap()).unwrap(),
        None
    );
    assert_eq!(s.load_pending_review(id, r_local.id).unwrap(), None);
    assert_eq!(s.local_patch_set(id, l1.number).unwrap(), None);

    assert!(s.delete_local_change(id, g.id).unwrap());
    // Its local patch set's drafts go; the Gerrit change's own stay.
    assert_eq!(s.load_draft(id, on_g1.local_id).unwrap(), None);
    assert_eq!(s.load_draft(id, r_g.message_draft.unwrap()).unwrap(), None);
    assert!(s.load_draft(id, on_gerrit.local_id).unwrap().is_some());

    // The other local change is untouched.
    assert!(s.load_draft(id, on_other.local_id).unwrap().is_some());
    assert!(s.load_pending_review(id, r_other.id).unwrap().is_some());
    assert!(
        s.load_draft(id, r_other.message_draft.unwrap())
            .unwrap()
            .is_some()
    );
    assert_eq!(s.local_patch_sets(id, other.id).unwrap(), [o1]);
    assert!(!s.delete_local_change(id, local.id).unwrap());
}

fn summary(n: u64, project: &str, change_id: &str) -> ChangeSummary {
    ChangeSummary {
        number: ChangeNumber(n),
        change_id: Some(change_id.into()),
        project: project.into(),
        branch: "main".into(),
        subject: "s".into(),
        status: ChangeStatus::New,
        owner: Account {
            id: AccountId(1),
            name: None,
            email: None,
            username: None,
            display_name: None,
        },
        created: ts(T0),
        updated: ts(T0),
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

fn revision(n: u32) -> Revision {
    Revision {
        number: PatchSetNumber(n),
        sha: format!("gerrit{n}").as_str().into(),
        kind: Some(RevisionKind::Rework),
        created: None,
        uploader: None,
        ref_name: None,
        description: None,
        commit: None,
    }
}

#[test]
fn cached_gerrit_changes_are_found_by_change_id_within_a_project() {
    let (mut s, id) = setup();
    let cached = [
        summary(1, "p", "IA"),
        summary(2, "p", "IB"),
        summary(3, "q", "IA"),
        summary(4, "p", "IA"),
    ];
    s.save_query_page(id, "q", &cached, false, ts(T0)).unwrap();
    let found = |s: &Store, ids: &[&str]| -> Vec<CachedGerritChange> {
        let ids: Vec<String> = ids.iter().map(|s| (*s).to_owned()).collect();
        s.cached_changes_by_change_id(id, "p", &ids).unwrap()
    };
    let numbers =
        |v: Vec<CachedGerritChange>| -> Vec<u64> { v.iter().map(|c| c.summary.number.0).collect() };
    assert_eq!(numbers(found(&s, &["IA"])), [1, 4]);
    assert_eq!(numbers(found(&s, &["IA", "IB", "IX"])), [1, 2, 4]);
    assert_eq!(numbers(found(&s, &[])), Vec::<u64>::new());
    // Without a cached detail the patch sets are unknown.
    assert_eq!(found(&s, &["IB"])[0].patch_sets, None);
    // With one, a pushed commit's SHA maps to its patch set.
    let detail = ChangeDetail {
        summary: summary(2, "p", "IB"),
        current_revision: Some("gerrit2".into()),
        current_patch_set: Some(PatchSetNumber(2)),
        commit_message: None,
        reviewers: vec![],
        cc: vec![],
        revisions: vec![revision(1), revision(2)],
        messages: vec![],
        submit_requirements: vec![],
        votes: Default::default(),
        attention: vec![],
        permitted_labels: Default::default(),
        removable_reviewers: Default::default(),
        label_values: Default::default(),
        change_actions: Default::default(),
        revision_actions: Default::default(),
    };
    s.save_change_detail(id, &detail, ts(T0)).unwrap();
    let ib = found(&s, &["IB"]).remove(0);
    assert_eq!(ib.current, Some(PatchSetNumber(2)));
    assert_eq!(ib.patch_sets, Some(vec![gerrit_ps(1), gerrit_ps(2)]));
}

#[test]
fn a_rekey_onto_another_rows_key_is_a_conflict_that_a_merge_resolves() {
    let (mut s, id) = setup();
    // Commit v1 targets main; an amended copy v2 on a branch tracking
    // stable became its own row.
    let main = add(&mut s, id, upsert("I1", Some("main"), &["v1"]));
    let stable = add(&mut s, id, upsert("I1", Some("stable"), &["v2"]));
    let v1 = s.local_patch_sets(id, main.id).unwrap().remove(0);
    let v2 = s.local_patch_sets(id, stable.id).unwrap().remove(0);
    let on_v1 = s
        .create_draft(id, main.number, &psref(&v1), &draft_input("v1"), ts(T1))
        .unwrap();
    let r_v1 = s.create_pending_review(id, &review(&v1)).unwrap();
    // A new branch that sorts first and tracks stable now contains v1, so
    // v1's target becomes stable: re-keying `main` collides.
    let mut rekey = upsert("I1", Some("stable"), &["v1"]);
    rekey.seen = Some(seen(&main));
    let r = s
        .reconcile_local(id, "p", &[LocalOp::Upsert(rekey)], ts(T2))
        .unwrap();
    assert_eq!(r.conflicts, [main.id]);
    assert!(!r.is_settled() && !r.changed());
    assert_eq!(
        s.local_change(id, main.id)
            .unwrap()
            .unwrap()
            .branch
            .as_deref(),
        Some("main")
    );
    // The merge folds `main` into `stable`, keeping numbers.
    let op = LocalOp::Merge {
        from: seen(&main),
        into: seen(&stable),
    };
    let r = s.reconcile_local(id, "p", &[op], ts(T2)).unwrap();
    assert_eq!(r.merged, [(main.id, stable.id)]);
    assert_eq!(r.updated, [stable.id]);
    assert_eq!(s.local_change(id, main.id).unwrap(), None);
    let versions = s.local_patch_sets(id, stable.id).unwrap();
    let got: Vec<_> = versions
        .iter()
        .map(|p| (p.id, p.number, p.ordinal))
        .collect();
    assert_eq!(got, [(v2.id, v2.number, 1), (v1.id, v1.number, 2)]);
    let d = s.load_draft(id, on_v1.local_id).unwrap().unwrap();
    assert_eq!((d.change, d.patch_set), (stable.number, v1.number));
    let r_v1 = s.load_pending_review(id, r_v1.id).unwrap().unwrap();
    assert_eq!((r_v1.local_change, r_v1.change), (stable.id, stable.number));
    let msg = s
        .load_draft(id, r_v1.message_draft.unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(msg.change, stable.number);
    assert!(s.has_unsent(id, stable.id).unwrap());
}

#[test]
fn a_merge_hands_a_shared_commit_to_the_surviving_copy() {
    let (mut s, id) = setup();
    let a = add(&mut s, id, upsert("I1", None, &["shared", "a-only"]));
    let mut b = upsert("I1", Some("main"), &["shared"]);
    b.gerrit_number = Some(ChangeNumber(9));
    let b = add(&mut s, id, b);
    let a_shared = s.local_patch_sets(id, a.id).unwrap().remove(0);
    let b_shared = s.local_patch_sets(id, b.id).unwrap().remove(0);
    let d = s
        .create_draft(id, a.number, &psref(&a_shared), &draft_input("x"), ts(T1))
        .unwrap();
    let r = s.create_pending_review(id, &review(&a_shared)).unwrap();
    let op = LocalOp::Merge {
        from: seen(&a),
        into: seen(&b),
    };
    s.reconcile_local(id, "p", std::slice::from_ref(&op), ts(T2))
        .unwrap();
    let shas: Vec<_> = s
        .local_patch_sets(id, b.id)
        .unwrap()
        .into_iter()
        .map(|p| (p.sha.0, p.ordinal))
        .collect();
    assert_eq!(shas, [("shared".to_owned(), 1), ("a-only".to_owned(), 2)]);
    // On the surviving copy, posted on `into`'s Gerrit change.
    let d = s.load_draft(id, d.local_id).unwrap().unwrap();
    assert_eq!((d.change, d.patch_set), (ChangeNumber(9), b_shared.number));
    let r = s.load_pending_review(id, r.id).unwrap().unwrap();
    assert_eq!((r.change, r.patch_set), (ChangeNumber(9), b_shared.number));
    // Merging again names a row that is gone.
    let again = s.reconcile_local(id, "p", &[op], ts(T2)).unwrap();
    assert_eq!(again.stale, 1);
}
