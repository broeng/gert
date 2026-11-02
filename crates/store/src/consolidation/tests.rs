use std::collections::BTreeMap;

use model::{CommentRange, DraftInput, LocalApproval, LocalPatchSet, PATCHSET_LEVEL_PATH};

use super::*;
use crate::{LocalChangeUpsert, LocalOp, NewLocalPatchSet, NewPendingReview};

fn ts(s: &str) -> model::Timestamp {
    s.parse().unwrap()
}

const T0: &str = "2024-03-01T10:00:00Z";
const T1: &str = "2024-03-01T11:00:00Z";

/// A local change `IL` in project `p` with versions v1 and v2.
fn setup() -> (Store, ServerId, LocalChange, LocalPatchSet, LocalPatchSet) {
    let mut s = Store::open_in_memory().unwrap();
    let id = s.server_id("https://g.example").unwrap();
    let c = add(&mut s, id, "IL", None);
    let mut ps = s.local_patch_sets(id, c.id).unwrap();
    let v2 = ps.remove(1);
    let v1 = ps.remove(0);
    (s, id, c, v1, v2)
}

fn add(s: &mut Store, id: ServerId, change_id: &str, gerrit: Option<u64>) -> LocalChange {
    let u = LocalChangeUpsert {
        seen: None,
        branch: Some("main".into()),
        change_id: change_id.into(),
        gerrit_number: gerrit.map(ChangeNumber),
        subject: "s".into(),
        author_name: "A".into(),
        author_email: "a@x".into(),
        updated: ts(T0),
        state: None,
        patch_sets: ["v1", "v2"]
            .iter()
            .map(|v| NewLocalPatchSet {
                sha: format!("{change_id}-{v}").as_str().into(),
                parent_sha: None,
                seen_at: ts(T0),
            })
            .collect(),
    };
    s.reconcile_local(id, "p", &[LocalOp::Upsert(u)], ts(T0))
        .unwrap();
    s.local_changes_by_change_id(id, change_id)
        .unwrap()
        .remove(0)
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

fn draft(s: &mut Store, id: ServerId, c: &LocalChange, p: &LocalPatchSet, msg: &str) -> Draft {
    s.create_draft(
        id,
        c.number,
        &psref(p),
        &DraftInput {
            path: "src/a.rs".into(),
            side: Side::Revision,
            parent: None,
            location: Location::Line(7),
            in_reply_to: None,
            message: msg.into(),
            unresolved: true,
        },
        ts(T1),
    )
    .unwrap()
}

fn review(s: &mut Store, id: ServerId, p: &LocalPatchSet) -> PendingReview {
    s.create_pending_review(
        id,
        &NewPendingReview {
            patch_set: p.number,
            labels: BTreeMap::from([("Code-Review".to_owned(), 2)]),
            strategy: LocalApproval::Follow,
            message: "LGTM".into(),
            unresolved: false,
            created: ts(T1),
        },
    )
    .unwrap()
}

fn anchor() -> DraftAnchor {
    DraftAnchor {
        path: "src/renamed.rs".into(),
        side: Side::Revision,
        parent: None,
        location: Location::Range(CommentRange {
            start_line: 10,
            start_character: 0,
            end_line: 11,
            end_character: 4,
        }),
    }
}

fn rewrite(c: &LocalChange, moves: Vec<PatchSetMove>) -> ConsolidationRewrite {
    ConsolidationRewrite {
        local_change: c.id,
        gerrit_number: ChangeNumber(42),
        moves,
        keep: vec![],
        review_states: vec![],
        state: None,
        else_state: None,
    }
}

#[test]
fn a_rewrite_moves_reserves_and_holds_what_it_is_told() {
    use PendingReviewState::*;
    let (mut s, id, c, v1, v2) = setup();
    let exact = draft(&mut s, id, &c, &v2, "exact");
    let ported = draft(&mut s, id, &c, &v1, "ported");
    let held = draft(&mut s, id, &c, &v1, "held");
    let r1 = review(&mut s, id, &v1);
    let r2 = review(&mut s, id, &v2);
    let mut rw = rewrite(
        &c,
        vec![
            // v2 was pushed as patch set 2; v1 never reached Gerrit and is
            // ported onto patch set 2 as well.
            PatchSetMove {
                from: v2.number,
                to: gerrit_ps(2),
                exact: true,
                drafts: vec![(exact.local_id, None)],
                reviews: vec![r2.id],
            },
            PatchSetMove {
                from: v1.number,
                to: gerrit_ps(2),
                exact: false,
                drafts: vec![(ported.local_id, Some(anchor()))],
                reviews: vec![r1.id],
            },
        ],
    );
    rw.keep = vec![(held.local_id, "line changed".into())];
    rw.review_states = vec![ReviewStateChange {
        review: r1.id,
        from: vec![Pending],
        state: Held,
        note: Some("approval not on this commit".into()),
    }];
    rw.state = Some(LocalChangeState::Consolidated);
    rw.else_state = Some(LocalChangeState::Local);
    let report = s.rewrite_local_change(id, &rw).unwrap().unwrap();
    // Drafts: exact, ported and the two message drafts.
    assert_eq!(
        (
            report.drafts_moved,
            report.reviews_moved,
            report.drafts_held,
            report.patch_sets_mapped,
            report.review_states,
            report.skipped
        ),
        (4, 2, 1, 1, 1, 0)
    );
    // Reserved drafts and unsent reviews are left: not consolidated, and
    // the fallback state applies.
    assert_eq!(report.state, LocalChangeState::Local);
    assert_eq!(
        s.local_change(id, c.id).unwrap().unwrap().gerrit_number,
        Some(ChangeNumber(42))
    );

    let d = s.load_draft(id, exact.local_id).unwrap().unwrap();
    assert_eq!(
        (d.change, d.patch_set, d.revision.as_str(), d.location),
        (
            ChangeNumber(42),
            PatchSetNumber(2),
            "gerrit2",
            Location::Line(7)
        )
    );
    assert_eq!(d.reserved_by, Some(c.id));
    assert!(!d.is_local() && !d.is_ordinary());
    let d = s.load_draft(id, ported.local_id).unwrap().unwrap();
    assert_eq!(
        (d.change, d.path.as_str(), d.location),
        (ChangeNumber(42), "src/renamed.rs", anchor().location)
    );
    // The held draft stays on its local patch set, findable by the change.
    let d = s.load_draft(id, held.local_id).unwrap().unwrap();
    assert_eq!(
        (d.change, d.patch_set, d.held_reason.as_deref()),
        (ChangeNumber(42), v1.number, Some("line changed"))
    );
    assert!(!d.is_ordinary());

    let r1 = s.load_pending_review(id, r1.id).unwrap().unwrap();
    assert_eq!(
        (r1.change, r1.patch_set, r1.revision.as_str(), r1.state),
        (ChangeNumber(42), PatchSetNumber(2), "gerrit2", Held)
    );
    // The votes still say what they were given on.
    assert_eq!(r1.given_on_sha, v1.sha);
    let msg = s
        .load_draft(id, r1.message_draft.unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            msg.change,
            msg.revision.as_str(),
            msg.path.as_str(),
            msg.reserved_by
        ),
        (ChangeNumber(42), "gerrit2", PATCHSET_LEVEL_PATH, Some(c.id))
    );
    let ps = s.local_patch_sets(id, c.id).unwrap();
    assert_eq!(
        (ps[0].gerrit_patch_set, ps[1].gerrit_patch_set),
        (None, Some(PatchSetNumber(2)))
    );

    // None of it enters the ordinary draft paths.
    assert!(s.pending_drafts(id).unwrap().is_empty());
    assert!(s.draft_counts(id).unwrap().is_empty());
    assert_eq!(s.local_draft_count(id).unwrap(), 5);
    assert_eq!(
        s.remove_synced_drafts(id, ChangeNumber(42), None).unwrap(),
        0
    );
    // The post sends the reserved drafts and both messages.
    let post: Vec<LocalDraftId> = s
        .consolidation_drafts(id, c.id)
        .unwrap()
        .iter()
        .map(|d| d.local_id)
        .collect();
    assert_eq!(
        post,
        [
            exact.local_id,
            ported.local_id,
            r1.message_draft.unwrap(),
            r2.message_draft.unwrap()
        ]
    );
    let held_items = s.held_items(id, ChangeNumber(42)).unwrap();
    assert_eq!(held_items.drafts.len(), 1);
    assert_eq!(held_items.drafts[0].local_id, held.local_id);
    assert_eq!(
        held_items.reviews.iter().map(|r| r.id).collect::<Vec<_>>(),
        [r1.id]
    );
}

#[test]
fn a_draft_written_after_the_snapshot_stays_and_keeps_the_change_open() {
    let (mut s, id, c, _v1, v2) = setup();
    let listed = draft(&mut s, id, &c, &v2, "listed");
    // Written while the run was computing its plan.
    let late = draft(&mut s, id, &c, &v2, "late");
    let late_review = review(&mut s, id, &v2);
    let mut rw = rewrite(
        &c,
        vec![PatchSetMove {
            from: v2.number,
            to: gerrit_ps(1),
            exact: true,
            drafts: vec![(listed.local_id, None)],
            reviews: vec![],
        }],
    );
    rw.state = Some(LocalChangeState::Consolidated);
    let report = s.rewrite_local_change(id, &rw).unwrap().unwrap();
    assert_eq!(report.drafts_moved, 1);
    assert_eq!(report.state, LocalChangeState::Checking);
    let d = s.load_draft(id, late.local_id).unwrap().unwrap();
    assert_eq!(
        (d.change, d.patch_set, d.reserved_by),
        (c.number, v2.number, None)
    );
    let r = s.load_pending_review(id, late_review.id).unwrap().unwrap();
    assert_eq!(r.patch_set, v2.number);
    assert!(s.has_unsent(id, c.id).unwrap());
}

#[test]
fn moves_that_do_not_fit_are_skipped() {
    let (mut s, id, c, v1, v2) = setup();
    let other = add(&mut s, id, "IO", None);
    let o1 = s.local_patch_sets(id, other.id).unwrap().remove(0);
    let foreign = draft(&mut s, id, &other, &o1, "foreign");
    let unanchored = draft(&mut s, id, &c, &v1, "no anchor");
    let wrong_ps = draft(&mut s, id, &c, &v2, "on v2");
    let sending = review(&mut s, id, &v1);
    s.mark_pending_review_sending(id, sending.id, &[PendingReviewState::Pending], "n")
        .unwrap()
        .unwrap();
    let foreign_review = review(&mut s, id, &o1);
    let rw = rewrite(
        &c,
        vec![
            PatchSetMove {
                from: v1.number,
                to: gerrit_ps(2),
                exact: false,
                drafts: vec![
                    (unanchored.local_id, None),
                    (wrong_ps.local_id, Some(anchor())),
                    (foreign.local_id, Some(anchor())),
                    // A sealed message moves only with its review.
                    (sending.message_draft.unwrap(), Some(anchor())),
                ],
                reviews: vec![sending.id, foreign_review.id],
            },
            // Not a version of this change.
            PatchSetMove {
                from: o1.number,
                to: gerrit_ps(2),
                exact: true,
                drafts: vec![(foreign.local_id, None)],
                reviews: vec![],
            },
        ],
    );
    let report = s.rewrite_local_change(id, &rw).unwrap().unwrap();
    assert_eq!(
        (report.drafts_moved, report.reviews_moved, report.skipped),
        (0, 0, 7)
    );
    for d in [&foreign, &unanchored, &wrong_ps] {
        let now = s.load_draft(id, d.local_id).unwrap().unwrap();
        assert_eq!((now.change, now.patch_set), (d.change, d.patch_set));
    }
    // Review states only touch this change's reviews.
    let mut rw = rewrite(&c, vec![]);
    rw.review_states = vec![ReviewStateChange {
        review: foreign_review.id,
        from: vec![PendingReviewState::Pending],
        state: PendingReviewState::Held,
        note: None,
    }];
    let report = s.rewrite_local_change(id, &rw).unwrap().unwrap();
    assert_eq!(report.review_states, 0);
}

#[test]
fn a_rewrite_refuses_local_targets_and_another_gerrit_change() {
    let (mut s, id, c, v1, _v2) = setup();
    let mut onto_local = rewrite(
        &c,
        vec![PatchSetMove {
            from: v1.number,
            to: psref(&v1),
            exact: true,
            drafts: vec![],
            reviews: vec![],
        }],
    );
    assert!(matches!(
        s.rewrite_local_change(id, &onto_local),
        Err(Error::LocalTarget)
    ));
    onto_local.moves.clear();
    onto_local.gerrit_number = c.number;
    assert!(matches!(
        s.rewrite_local_change(id, &onto_local),
        Err(Error::LocalTarget)
    ));
    s.rewrite_local_change(id, &rewrite(&c, vec![]))
        .unwrap()
        .unwrap();
    let mut other = rewrite(&c, vec![]);
    other.gerrit_number = ChangeNumber(43);
    assert!(matches!(
        s.rewrite_local_change(id, &other),
        Err(Error::OtherGerritChange(ChangeNumber(42)))
    ));
    // The server is removed while consolidation runs: nothing to write,
    // and the caller learns that it should stop.
    s.delete_server(id).unwrap();
    assert_eq!(
        s.rewrite_local_change(id, &rewrite(&c, vec![])).unwrap(),
        None
    );
    assert_eq!(s.finish_local_post(id, c.id, &[], &[]).unwrap(), None);
}

#[test]
fn finishing_the_post_removes_what_was_sent_and_then_it_consolidates() {
    use PendingReviewState::*;
    let (mut s, id, c, _v1, v2) = setup();
    let a = draft(&mut s, id, &c, &v2, "a");
    let b = draft(&mut s, id, &c, &v2, "b");
    let r = review(&mut s, id, &v2);
    let rw = rewrite(
        &c,
        vec![PatchSetMove {
            from: v2.number,
            to: gerrit_ps(1),
            exact: true,
            drafts: vec![(a.local_id, None), (b.local_id, None)],
            reviews: vec![r.id],
        }],
    );
    s.rewrite_local_change(id, &rw).unwrap().unwrap();
    let claimed = s
        .mark_pending_review_sending(id, r.id, &[Pending], "n")
        .unwrap()
        .unwrap();
    let post: Vec<(LocalDraftId, u32)> = s
        .consolidation_drafts(id, c.id)
        .unwrap()
        .iter()
        .map(|d| (d.local_id, d.version))
        .collect();
    assert_eq!(post.len(), 3);
    // `b` is edited while the post is in flight: it stays reserved.
    let edited = s
        .edit_draft(id, b.local_id, "b2", true, ts(T1))
        .unwrap()
        .unwrap();
    assert_eq!(edited.reserved_by, Some(c.id));
    let done = vec![ReviewStateChange {
        review: claimed.id,
        from: vec![Sending],
        state: Sent,
        note: None,
    }];
    let report = s
        .finish_local_post(id, c.id, &post, &done)
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            report.drafts_removed,
            report.drafts_kept,
            report.review_states
        ),
        (2, 1, 1)
    );
    assert_eq!(s.load_draft(id, a.local_id).unwrap(), None);
    assert_eq!(
        s.load_draft(id, claimed.message_draft.unwrap()).unwrap(),
        None
    );
    assert_eq!(
        s.consolidation_drafts(id, c.id)
            .unwrap()
            .iter()
            .map(|d| d.local_id)
            .collect::<Vec<_>>(),
        [b.local_id]
    );
    // Not done while `b` waits; done once it went too.
    assert_eq!(
        s.set_local_change_state(id, c.id, LocalChangeState::Consolidated)
            .unwrap(),
        Some(LocalChangeState::Checking)
    );
    s.finish_local_post(id, c.id, &[(b.local_id, edited.version)], &[])
        .unwrap()
        .unwrap();
    assert_eq!(
        s.set_local_change_state(id, c.id, LocalChangeState::Consolidated)
            .unwrap(),
        Some(LocalChangeState::Consolidated)
    );
}

#[test]
fn reserved_drafts_go_with_their_local_change() {
    let (mut s, id, c, _v1, v2) = setup();
    let ordinary = s
        .create_draft(
            id,
            ChangeNumber(42),
            &gerrit_ps(1),
            &DraftInput {
                path: "f".into(),
                side: Side::Revision,
                parent: None,
                location: Location::File,
                in_reply_to: None,
                message: "mine".into(),
                unresolved: false,
            },
            ts(T1),
        )
        .unwrap();
    let d = draft(&mut s, id, &c, &v2, "x");
    let rw = rewrite(
        &c,
        vec![PatchSetMove {
            from: v2.number,
            to: gerrit_ps(1),
            exact: true,
            drafts: vec![(d.local_id, None)],
            reviews: vec![],
        }],
    );
    s.rewrite_local_change(id, &rw).unwrap().unwrap();
    assert!(s.delete_local_change(id, c.id).unwrap());
    assert_eq!(s.load_draft(id, d.local_id).unwrap(), None);
    assert!(s.load_draft(id, ordinary.local_id).unwrap().is_some());
}

#[test]
fn the_changes_to_consolidate_are_the_open_ones_and_those_with_leftovers() {
    let (mut s, id, checking, _v1, _v2) = setup();
    let mut states = Vec::new();
    for (i, state) in LocalChangeState::ALL.into_iter().enumerate() {
        let c = add(
            &mut s,
            id,
            &format!("I{i}"),
            Some(100 + u64::try_from(i).unwrap()),
        );
        s.set_local_change_state(id, c.id, state).unwrap();
        states.push((c, state));
    }
    // A closed one with a held draft.
    let (closed, _) = states
        .iter()
        .find(|(_, s)| *s == LocalChangeState::Closed)
        .unwrap()
        .clone();
    let v = s.local_patch_sets(id, closed.id).unwrap().remove(0);
    draft(&mut s, id, &closed, &v, "held");
    let ambiguous = add(&mut s, id, "IAMB", None);
    s.set_local_change_state(id, ambiguous.id, LocalChangeState::Ambiguous)
        .unwrap();

    let mut got: Vec<LocalChangeId> = s
        .local_changes_to_consolidate(id)
        .unwrap()
        .iter()
        .map(|c| c.id)
        .collect();
    got.sort();
    let mut want: Vec<LocalChangeId> = states
        .iter()
        .filter(|(_, st)| {
            matches!(
                st,
                LocalChangeState::Checking
                    | LocalChangeState::Local
                    | LocalChangeState::Vanished
                    | LocalChangeState::Closed
            )
        })
        .map(|(c, _)| c.id)
        .chain([checking.id])
        .collect();
    want.sort();
    assert_eq!(got, want);
}
