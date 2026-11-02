use std::collections::BTreeMap;

use model::{
    Account, AccountId, ChangeStatus, DraftInput, LocalApproval, Location, PatchSetRef,
    PendingReviewState, Side,
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
        id: None,
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

fn review(change: ChangeNumber, local: LocalChangeId, p: &LocalPatchSet) -> NewPendingReview {
    NewPendingReview {
        local_change: Some(local),
        change,
        patch_set: psref(p),
        labels: BTreeMap::from([("Code-Review".to_owned(), 2)]),
        strategy: LocalApproval::Follow,
        message: "LGTM".into(),
        created: ts(T1),
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
        LocalOp::Delete(b.id),
        LocalOp::SetState {
            change: unchanged.id,
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
    let r = s
        .reconcile_local(id, "p", &[LocalOp::Delete(b.id)], ts(T2))
        .unwrap();
    assert_eq!((r.stale, r.changed()), (1, false));
}

#[test]
fn a_failing_reconcile_writes_nothing() {
    let (mut s, id) = setup();
    let a = add(&mut s, id, upsert("IA", Some("main"), &["a1"]));
    add(&mut s, id, upsert("IA", Some("stable"), &["s1"]));
    // Re-keying `a` onto the key of the other row fails, and the insert
    // before it in the same plan is rolled back.
    let mut rekey = upsert("IA", Some("stable"), &[]);
    rekey.id = Some(a.id);
    let ops = [
        LocalOp::Upsert(upsert("IZ", None, &["z1"])),
        LocalOp::Upsert(rekey),
    ];
    assert!(s.reconcile_local(id, "p", &ops, ts(T2)).is_err());
    assert_eq!(s.local_changes(id).unwrap().len(), 2);
    assert!(s.local_changes_by_change_id(id, "IZ").unwrap().is_empty());
    // A removed server is reported, not silently recreated.
    s.delete_server(id).unwrap();
    assert!(matches!(
        s.reconcile_local(id, "p", &ops[..1], ts(T2)),
        Err(Error::NoSuchServer)
    ));
}

#[test]
fn a_row_can_be_rekeyed_to_another_branch() {
    let (mut s, id) = setup();
    let a = add(&mut s, id, upsert("IA", None, &["a1"]));
    let mut rekey = upsert("IA", Some("main"), &[]);
    rekey.id = Some(a.id);
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
                LocalOp::Delete(local.id),
            ],
            ts(T2),
        )
        .unwrap();
    assert_eq!((r.patch_sets_mapped, r.stale), (0, 1));
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
    let mut r = review(local.number, local.id, &l1);
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
            change: c.id,
            state: LocalChangeState::Consolidated,
        },
        LocalOp::Delete(c.id),
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
    let r_local = s
        .create_pending_review(id, &review(local.number, local.id, &l1))
        .unwrap();
    let r_g = s
        .create_pending_review(id, &review(ChangeNumber(7), g.id, &g1))
        .unwrap();
    let r_other = s
        .create_pending_review(id, &review(other.number, other.id, &o1))
        .unwrap();

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
    let found = |ids: &[&str]| -> Vec<u64> {
        let ids: Vec<String> = ids.iter().map(|s| (*s).to_owned()).collect();
        s.cached_changes_by_change_id(id, "p", &ids)
            .unwrap()
            .iter()
            .map(|c| c.number.0)
            .collect()
    };
    assert_eq!(found(&["IA"]), [1, 4]);
    assert_eq!(found(&["IA", "IB", "IX"]), [1, 2, 4]);
    assert_eq!(found(&[]), Vec::<u64>::new());
}
