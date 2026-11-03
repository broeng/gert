use std::collections::BTreeMap;

use model::{CommentRange, DraftInput, LocalApproval, LocalPatchSet, PATCHSET_LEVEL_PATH};

use super::*;
use crate::{LocalChangeUpsert, LocalOp, NewLocalPatchSet, NewPendingReview};

fn ts(s: &str) -> model::Timestamp {
    s.parse().unwrap()
}

const T0: &str = "2024-03-01T10:00:00Z";

/// The claimant of the posts in these tests.
fn who() -> Claimant<'static> {
    Claimant {
        owner: "run-1",
        at: ts(T1),
    }
}
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
            replaces_draft: None,
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
    // A review the run did not move (created after its snapshot).
    let r3 = review(&mut s, id, &v1);
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
    rw.review_states = vec![ReviewStateChange::new(
        r1.id,
        &[Pending],
        Held,
        Some("approval not on this commit".into()),
    )];
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
    assert_eq!(s.local_draft_count(id).unwrap(), 6);
    assert_eq!(
        s.remove_synced_drafts(id, ChangeNumber(42), None).unwrap(),
        0
    );
    // The post sends the reserved drafts and the moved reviews' messages,
    // not the message of the review that stayed.
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
    assert!(!post.contains(&r3.message_draft.unwrap()));
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
    // It stays on its local patch set; with the change found on Gerrit,
    // that is a local patch set of the Gerrit change now.
    let d = s.load_draft(id, late.local_id).unwrap().unwrap();
    assert_eq!(
        (d.change, d.patch_set, d.reserved_by),
        (ChangeNumber(42), v2.number, None)
    );
    let r = s.load_pending_review(id, late_review.id).unwrap().unwrap();
    assert_eq!((r.change, r.patch_set), (ChangeNumber(42), v2.number));
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
    // Nothing moved to a Gerrit patch set. The other change's draft stays
    // on its number; this change's drafts are on its Gerrit change now.
    let now = s.load_draft(id, foreign.local_id).unwrap().unwrap();
    assert_eq!(
        (now.change, now.patch_set),
        (foreign.change, foreign.patch_set)
    );
    for d in [&unanchored, &wrong_ps] {
        let now = s.load_draft(id, d.local_id).unwrap().unwrap();
        assert_eq!((now.change, now.patch_set), (ChangeNumber(42), d.patch_set));
        assert!(now.reserved_by.is_none());
    }
    // Review states only touch this change's reviews.
    let mut rw = rewrite(&c, vec![]);
    rw.review_states = vec![ReviewStateChange::new(
        foreign_review.id,
        &[PendingReviewState::Pending],
        PendingReviewState::Held,
        None,
    )];
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
    assert_eq!(s.finish_local_post(id, c.id, "n", &[], &[]).unwrap(), None);
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
    let claim = s
        .claim_local_post(
            id,
            c.id,
            Some((r.id, &[Pending][..])),
            PatchSetNumber(1),
            "n",
            &who(),
        )
        .unwrap()
        .unwrap();
    let claimed = claim.review.unwrap();
    let post: Vec<(LocalDraftId, u32)> = claim
        .drafts
        .iter()
        .map(|d| (d.local_id, d.version))
        .collect();
    assert_eq!(post.len(), 3);
    // While the post is in flight its drafts can be neither edited nor
    // deleted.
    assert!(matches!(
        s.edit_draft(id, b.local_id, "b2", true, ts(T1)),
        Err(Error::DraftBeingSent)
    ));
    assert_eq!(
        s.delete_draft(id, b.local_id).unwrap(),
        crate::DeleteOutcome::BeingSent
    );
    // As if `b` had changed meanwhile: it stays for the next post.
    s.conn
        .execute(
            "UPDATE drafts SET version = version + 1 WHERE id = ?1",
            [b.local_id.0],
        )
        .unwrap();
    let edited = s.load_draft(id, b.local_id).unwrap().unwrap();
    assert_eq!(edited.reserved_by, Some(c.id));
    let done = vec![ReviewStateChange::new(claimed.id, &[Sending], Sent, None)];
    let report = s
        .finish_local_post(id, c.id, "n", &post, &done)
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
    // `b` is free for the next post.
    assert_eq!(posting_nonce(&s, b.local_id), None);
    assert!(s.unfinished_local_posts(id).unwrap().is_empty());
    // Not done while `b` waits; done once it went too.
    assert_eq!(
        s.set_local_change_state(id, c.id, LocalChangeState::Consolidated)
            .unwrap(),
        Some(LocalChangeState::Checking)
    );
    s.finish_local_post(id, c.id, "n", &[(b.local_id, edited.version)], &[])
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

/// The posting nonce of a draft.
fn posting_nonce(s: &Store, id: LocalDraftId) -> Option<String> {
    s.conn
        .query_row(
            "SELECT posting_nonce FROM drafts WHERE id = ?1",
            [id.0],
            |r| r.get(0),
        )
        .unwrap()
}

/// A local change with drafts `a` (moved) and a review `r` on v2, both
/// moved onto change 42, and a review `other` on v1 that also moved.
fn moved(
    s: &mut Store,
    id: ServerId,
    c: &LocalChange,
    v1: &LocalPatchSet,
    v2: &LocalPatchSet,
) -> (Draft, PendingReview, PendingReview) {
    let a = draft(s, id, c, v2, "a");
    let r = review(s, id, v2);
    let other = review(s, id, v1);
    let rw = rewrite(
        c,
        vec![
            PatchSetMove {
                from: v2.number,
                to: gerrit_ps(1),
                exact: true,
                drafts: vec![(a.local_id, None)],
                reviews: vec![r.id],
            },
            PatchSetMove {
                from: v1.number,
                to: gerrit_ps(1),
                exact: false,
                drafts: vec![],
                reviews: vec![other.id],
            },
        ],
    );
    s.rewrite_local_change(id, &rw).unwrap().unwrap();
    (a, r, other)
}

#[test]
fn claiming_a_post_marks_its_drafts_and_review_with_one_nonce() {
    use PendingReviewState::*;
    let (mut s, id, c, v1, v2) = setup();
    let (a, r, other) = moved(&mut s, id, &c, &v1, &v2);
    let post = s
        .claim_local_post(
            id,
            c.id,
            Some((r.id, &[Pending][..])),
            PatchSetNumber(1),
            "n1",
            &who(),
        )
        .unwrap()
        .unwrap();
    let review = post.review.unwrap();
    assert_eq!(
        (review.state, review.nonce.as_deref()),
        (Sending, Some("n1"))
    );
    // Its drafts: the moved comment and its own message, not the other
    // review's message.
    let ids: Vec<LocalDraftId> = post.drafts.iter().map(|d| d.local_id).collect();
    assert_eq!(ids, [a.local_id, r.message_draft.unwrap()]);
    for id in &ids {
        assert_eq!(posting_nonce(&s, *id).as_deref(), Some("n1"));
    }
    assert_eq!(posting_nonce(&s, other.message_draft.unwrap()), None);
    assert_eq!(
        s.unfinished_local_posts(id).unwrap(),
        [(c.id, "n1".to_owned())]
    );

    // Another run cannot claim it, with or without the review; the same
    // run can (a retry after a crash).
    assert_eq!(
        s.claim_local_post(id, c.id, None, PatchSetNumber(1), "n2", &who())
            .unwrap(),
        None
    );
    assert_eq!(
        s.claim_local_post(
            id,
            c.id,
            Some((other.id, &[Pending][..])),
            PatchSetNumber(1),
            "n2",
            &who()
        )
        .unwrap(),
        None
    );
    let again = s
        .claim_local_post(
            id,
            c.id,
            Some((r.id, &[Pending][..])),
            PatchSetNumber(1),
            "n1",
            &who(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(again.drafts.len(), 2);
    assert_eq!(
        s.load_pending_review(id, other.id).unwrap().unwrap().state,
        Pending
    );

    // Released (the server refused it): claimable again.
    assert_eq!(s.release_local_post(id, c.id, "n1").unwrap(), 2);
    s.set_pending_review_state(id, r.id, &[Sending], Failed, Some("409"))
        .unwrap();
    assert!(s.unfinished_local_posts(id).unwrap().is_empty());
    let retry = s
        .claim_local_post(
            id,
            c.id,
            Some((r.id, &[Failed][..])),
            PatchSetNumber(1),
            "n3",
            &who(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(retry.review.unwrap().nonce.as_deref(), Some("n3"));
}

#[test]
fn a_claimed_post_reads_back_exactly_as_claimed() {
    use PendingReviewState::*;
    let (mut s, id, c, v1, v2) = setup();
    let (a, r, other) = moved(&mut s, id, &c, &v1, &v2);
    assert_eq!(s.local_post(id, c.id, "n1").unwrap(), None);
    let claimed = s
        .claim_local_post(
            id,
            c.id,
            Some((r.id, &[Pending][..])),
            PatchSetNumber(1),
            "n1",
            &who(),
        )
        .unwrap()
        .unwrap();
    // Reserved after the claim (by a later rewrite): not part of the post.
    let late = draft(&mut s, id, &c, &v1, "late");
    s.rewrite_local_change(
        id,
        &rewrite(
            &c,
            vec![PatchSetMove {
                from: v1.number,
                to: gerrit_ps(1),
                exact: true,
                drafts: vec![(late.local_id, None)],
                reviews: vec![],
            }],
        ),
    )
    .unwrap()
    .unwrap();
    let read = s.local_post(id, c.id, "n1").unwrap().unwrap();
    assert_eq!(read, claimed);
    assert_eq!(
        read.drafts.iter().map(|d| d.local_id).collect::<Vec<_>>(),
        [a.local_id, r.message_draft.unwrap()]
    );
    assert!(read.drafts.iter().all(|d| d.local_id != late.local_id));
    assert_eq!(s.local_post(id, c.id, "n2").unwrap(), None);
    assert_eq!(s.local_post(id, other.local_change, "n2").unwrap(), None);
}

#[test]
fn a_rewrite_brings_what_is_left_on_the_synthetic_number_to_the_gerrit_change() {
    let (mut s, id, c, v1, v2) = setup();
    let stays = draft(&mut s, id, &c, &v1, "on a version that is not on Gerrit");
    let r = review(&mut s, id, &v1);
    // Found closed: nothing moves to a Gerrit patch set.
    let mut rw = rewrite(&c, vec![]);
    rw.state = Some(LocalChangeState::Closed);
    let report = s.rewrite_local_change(id, &rw).unwrap().unwrap();
    assert_eq!(report.state, LocalChangeState::Closed);
    let d = s.load_draft(id, stays.local_id).unwrap().unwrap();
    assert_eq!((d.change, d.patch_set), (ChangeNumber(42), v1.number));
    assert!(d.reserved_by.is_none() && d.held_reason.is_none());
    let r = s.load_pending_review(id, r.id).unwrap().unwrap();
    assert_eq!(
        (r.change, r.patch_set, r.state),
        (ChangeNumber(42), v1.number, PendingReviewState::Pending)
    );
    assert!(s.load_drafts(id, c.number).unwrap().is_empty());
    assert_eq!(s.load_drafts(id, ChangeNumber(42)).unwrap().len(), 2);
    let _ = v2;
}

#[test]
fn a_comment_only_post_has_a_nonce_too() {
    let (mut s, id, c, _v1, v2) = setup();
    let a = draft(&mut s, id, &c, &v2, "a");
    let rw = rewrite(
        &c,
        vec![PatchSetMove {
            from: v2.number,
            to: gerrit_ps(1),
            exact: true,
            drafts: vec![(a.local_id, None)],
            reviews: vec![],
        }],
    );
    s.rewrite_local_change(id, &rw).unwrap().unwrap();
    let post = s
        .claim_local_post(id, c.id, None, PatchSetNumber(1), "n1", &who())
        .unwrap()
        .unwrap();
    assert_eq!((post.review, post.drafts.len()), (None, 1));
    assert_eq!(
        s.unfinished_local_posts(id).unwrap(),
        [(c.id, "n1".to_owned())]
    );
    // Finished: nothing is left claimed.
    s.finish_local_post(id, c.id, "n1", &[(a.local_id, post.drafts[0].version)], &[])
        .unwrap()
        .unwrap();
    assert!(s.unfinished_local_posts(id).unwrap().is_empty());
}

#[test]
fn claims_that_do_not_fit_write_nothing() {
    use PendingReviewState::*;
    let (mut s, id, c, v1, v2) = setup();
    let (a, r, _) = moved(&mut s, id, &c, &v1, &v2);
    let other_change = add(&mut s, id, "IO", None);
    // Not in the states named, another change's review, a gone change.
    assert_eq!(
        s.claim_local_post(
            id,
            c.id,
            Some((r.id, &[Held][..])),
            PatchSetNumber(1),
            "n",
            &who()
        )
        .unwrap(),
        None
    );
    assert_eq!(
        s.claim_local_post(
            id,
            other_change.id,
            Some((r.id, &[Pending][..])),
            PatchSetNumber(1),
            "n",
            &who()
        )
        .unwrap(),
        None
    );
    assert_eq!(
        s.claim_local_post(id, LocalChangeId(999), None, PatchSetNumber(1), "n", &who())
            .unwrap(),
        None
    );
    assert_eq!(posting_nonce(&s, a.local_id), None);
    assert!(s.unfinished_local_posts(id).unwrap().is_empty());
}

#[test]
fn one_owner_at_a_time_holds_the_lease() {
    let (mut s, id, ..) = setup();
    let (t0, t1, t2) = (ts(T0), ts(T1), ts("2024-03-01T12:00:00Z"));
    assert!(s.take_consolidation_lease(id, "a", t0, t1).unwrap());
    // Another owner waits until it expires; the holder renews.
    assert!(!s.take_consolidation_lease(id, "b", t0, t1).unwrap());
    assert!(s.take_consolidation_lease(id, "a", t0, t2).unwrap());
    assert!(!s.take_consolidation_lease(id, "b", t1, t2).unwrap());
    assert!(s.take_consolidation_lease(id, "b", t2, t2).unwrap());
    // Ending someone else's lease does nothing; ending one's own frees it.
    s.end_consolidation_lease(id, "a").unwrap();
    assert!(!s.take_consolidation_lease(id, "a", t1, t2).unwrap());
    s.end_consolidation_lease(id, "b").unwrap();
    assert!(s.take_consolidation_lease(id, "a", t1, t2).unwrap());
}

#[test]
fn only_claims_of_gone_runs_that_are_old_enough_are_recovered() {
    use PendingReviewState::*;
    let (mut s, id, c, v1, v2) = setup();
    let (_, r, _) = moved(&mut s, id, &c, &v1, &v2);
    let claimed_at = ts(T1);
    let by = Claimant {
        owner: "old-run",
        at: claimed_at,
    };
    s.claim_local_post(
        id,
        c.id,
        Some((r.id, &[Pending][..])),
        PatchSetNumber(1),
        "n1",
        &by,
    )
    .unwrap()
    .unwrap();
    let young = ts("2024-03-01T10:30:00Z");
    let old = ts("2024-03-01T11:30:00Z");
    // Still in flight: claimed after `stale_before`, by another run.
    assert!(
        s.recoverable_local_posts(id, "new-run", young)
            .unwrap()
            .is_empty()
    );
    let me = Claimant {
        owner: "new-run",
        at: ts("2024-03-01T12:00:00Z"),
    };
    assert_eq!(
        s.adopt_local_post(id, c.id, "n1", &me, young).unwrap(),
        None
    );
    // Old enough.
    assert_eq!(
        s.recoverable_local_posts(id, "new-run", old).unwrap(),
        [(c.id, "n1".to_owned())]
    );
    let post = s
        .adopt_local_post(id, c.id, "n1", &me, old)
        .unwrap()
        .unwrap();
    assert_eq!(post.review.unwrap().id, r.id);
    // Now the new run's: no other run may take it over, however old.
    let third = Claimant {
        owner: "third-run",
        at: ts("2024-03-01T12:00:00Z"),
    };
    assert!(
        s.recoverable_local_posts(id, "third-run", old)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        s.adopt_local_post(id, c.id, "n1", &third, old).unwrap(),
        None
    );
    // Finishing clears the claim.
    let sent = ReviewStateChange::new(r.id, &[Sending], Sent, None);
    s.finish_local_post(id, c.id, "n1", &[], &[sent])
        .unwrap()
        .unwrap();
    assert!(s.unfinished_local_posts(id).unwrap().is_empty());
    let left: i64 = s
        .conn
        .query_row("SELECT COUNT(*) FROM local_post_claims", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0);
}

#[test]
fn a_claim_recorded_by_no_run_is_recoverable() {
    use PendingReviewState::*;
    let (mut s, id, c, v1, v2) = setup();
    let (_, r, _) = moved(&mut s, id, &c, &v1, &v2);
    s.claim_local_post(
        id,
        c.id,
        Some((r.id, &[Pending][..])),
        PatchSetNumber(1),
        "n1",
        &who(),
    )
    .unwrap()
    .unwrap();
    // As a claim made before claims named their run (v15).
    s.conn.execute("DELETE FROM local_post_claims", []).unwrap();
    assert_eq!(
        s.recoverable_local_posts(id, "any", ts(T0)).unwrap(),
        [(c.id, "n1".to_owned())]
    );
    // Deleting the change removes its claims.
    s.claim_local_post(
        id,
        c.id,
        Some((r.id, &[Sending][..])),
        PatchSetNumber(1),
        "n1",
        &who(),
    )
    .unwrap()
    .unwrap();
    assert!(s.delete_local_change(id, c.id).unwrap());
    let left: i64 = s
        .conn
        .query_row("SELECT COUNT(*) FROM local_post_claims", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0);
}

#[test]
fn finishing_a_post_keeps_the_votes_that_did_not_reach_gerrit() {
    use PendingReviewState::*;
    let (mut s, id, c, v1, v2) = setup();
    let (a, r, _) = moved(&mut s, id, &c, &v1, &v2);
    s.set_review_label(id, r.id, "Verified", 1, LocalApproval::Exact, ts(T1))
        .unwrap()
        .unwrap();
    let post = s
        .claim_local_post(
            id,
            c.id,
            Some((r.id, &[Pending][..])),
            PatchSetNumber(1),
            "n1",
            &who(),
        )
        .unwrap()
        .unwrap();
    let posted: Vec<(LocalDraftId, u32)> = post
        .drafts
        .iter()
        .map(|d| (d.local_id, d.version))
        .collect();
    let mut held = ReviewStateChange::new(r.id, &[Sending], Held, Some("cast on PS 1".into()));
    held.labels = Some(BTreeMap::from([("Code-Review".to_owned(), 2)]));
    held.drop_only = ["Code-Review".to_owned()].into();
    s.finish_local_post(id, c.id, "n1", &posted, &[held])
        .unwrap()
        .unwrap();
    let r = s.load_pending_review(id, r.id).unwrap().unwrap();
    assert_eq!(
        (r.state, r.held_reason.as_deref()),
        (Held, Some("cast on PS 1"))
    );
    assert_eq!(r.labels, BTreeMap::from([("Code-Review".to_owned(), 2)]));
    assert_eq!(r.drop_only, ["Code-Review".to_owned()].into());
    assert_eq!(s.load_draft(id, a.local_id).unwrap(), None);
    // Claiming it again (a manual publish) clears the marker.
    let again = s
        .claim_local_post(
            id,
            c.id,
            Some((r.id, &[Held][..])),
            PatchSetNumber(1),
            "n2",
            &who(),
        )
        .unwrap()
        .unwrap();
    assert!(again.review.unwrap().drop_only.is_empty());
}

#[test]
fn abandoning_a_refused_post_fails_the_review_and_holds_its_comments_at_once() {
    use PendingReviewState::*;
    let (mut s, id, c, v1, v2) = setup();
    let (a, r, _) = moved(&mut s, id, &c, &v1, &v2);
    s.claim_local_post(
        id,
        c.id,
        Some((r.id, &[Pending][..])),
        PatchSetNumber(1),
        "n1",
        &who(),
    )
    .unwrap()
    .unwrap();
    let failed = ReviewStateChange::new(r.id, &[Sending], Failed, Some("400 bad line".into()));
    let released = s
        .abandon_local_post(
            id,
            c.id,
            "n1",
            Some(&failed),
            Some("Gerrit refused the post"),
        )
        .unwrap()
        .unwrap();
    assert_eq!(released, 2);
    let r = s.load_pending_review(id, r.id).unwrap().unwrap();
    assert_eq!(
        (r.state, r.error.as_deref()),
        (Failed, Some("400 bad line"))
    );
    let d = s.load_draft(id, a.local_id).unwrap().unwrap();
    assert_eq!(d.held_reason.as_deref(), Some("Gerrit refused the post"));
    assert_eq!(posting_nonce(&s, a.local_id), None);
    // The sealed message is not held on its own: it goes with its review.
    let msg = s.load_draft(id, r.message_draft.unwrap()).unwrap().unwrap();
    assert_eq!(msg.held_reason, None);
    assert!(s.unfinished_local_posts(id).unwrap().is_empty());
    let held = s.held_items(id, ChangeNumber(42)).unwrap();
    assert_eq!(held.drafts.len(), 1);
    assert_eq!(held.reviews.len(), 1);

    // Given up before sending: back to pending, nothing held.
    let (mut s, id, c, v1, v2) = setup();
    let (a, r, _) = moved(&mut s, id, &c, &v1, &v2);
    s.claim_local_post(
        id,
        c.id,
        Some((r.id, &[Pending][..])),
        PatchSetNumber(1),
        "n1",
        &who(),
    )
    .unwrap()
    .unwrap();
    let back = ReviewStateChange::new(r.id, &[Sending], Pending, None);
    s.abandon_local_post(id, c.id, "n1", Some(&back), None)
        .unwrap()
        .unwrap();
    assert_eq!(
        s.load_pending_review(id, r.id).unwrap().unwrap().state,
        Pending
    );
    assert_eq!(
        s.load_draft(id, a.local_id).unwrap().unwrap().held_reason,
        None
    );
    assert_eq!(
        s.abandon_local_post(id, LocalChangeId(999), "n1", None, None)
            .unwrap(),
        None
    );
}

#[test]
fn a_closed_change_holds_its_reviews_and_drafts() {
    use PendingReviewState::*;
    let (mut s, id, c, v1, v2) = setup();
    let (a, r, other) = moved(&mut s, id, &c, &v1, &v2);
    // Still on a version: a draft and a review created after the move.
    let local = draft(&mut s, id, &c, &v1, "local");
    let late = review(&mut s, id, &v1);
    let (reviews, drafts) = s
        .hold_closed_local_change(id, c.id, ChangeNumber(42), "change merged")
        .unwrap()
        .unwrap();
    assert_eq!((reviews, drafts), (3, 2));
    let row = s.local_change(id, c.id).unwrap().unwrap();
    assert_eq!(
        (row.state, row.gerrit_number),
        (LocalChangeState::Closed, Some(ChangeNumber(42)))
    );
    for x in [&r, &other, &late] {
        let now = s.load_pending_review(id, x.id).unwrap().unwrap();
        assert_eq!(
            (now.state, now.held_reason.as_deref()),
            (Held, Some("change merged"))
        );
    }
    for d in [&a, &local] {
        let now = s.load_draft(id, d.local_id).unwrap().unwrap();
        assert_eq!(now.held_reason.as_deref(), Some("change merged"));
    }
    // The review messages go with their reviews.
    let msg = s.load_draft(id, r.message_draft.unwrap()).unwrap().unwrap();
    assert_eq!(msg.held_reason, None);
    let held = s.held_items(id, ChangeNumber(42)).unwrap();
    assert_eq!((held.drafts.len(), held.reviews.len()), (2, 3));
    // Held drafts are not claimed by a post.
    let post = s
        .claim_local_post(id, c.id, None, PatchSetNumber(1), "n1", &who())
        .unwrap()
        .unwrap();
    assert!(post.drafts.is_empty());
    assert_eq!(
        s.hold_closed_local_change(id, c.id, ChangeNumber(43), "x")
            .unwrap_err()
            .to_string(),
        Error::OtherGerritChange(ChangeNumber(42)).to_string()
    );
}

#[test]
fn checked_times_are_kept_per_change() {
    let (mut s, id, c, ..) = setup();
    assert_eq!(s.local_change_checked_at(id, c.id).unwrap(), None);
    s.mark_local_changes_checked(id, &[c.id], ts(T1)).unwrap();
    assert_eq!(s.local_change_checked_at(id, c.id).unwrap(), Some(ts(T1)));
    assert_eq!(
        s.local_change_checked_at(id, LocalChangeId(999)).unwrap(),
        None
    );
}

#[test]
fn a_post_claims_only_the_drafts_of_its_revision() {
    use PendingReviewState::*;
    let (mut s, id, c, v1, v2) = setup();
    let on1 = draft(&mut s, id, &c, &v1, "on PS 1");
    let on2 = draft(&mut s, id, &c, &v2, "on PS 2");
    let r = review(&mut s, id, &v2);
    let rw = rewrite(
        &c,
        vec![
            PatchSetMove {
                from: v1.number,
                to: gerrit_ps(1),
                exact: true,
                drafts: vec![(on1.local_id, None)],
                reviews: vec![],
            },
            PatchSetMove {
                from: v2.number,
                to: gerrit_ps(2),
                exact: true,
                drafts: vec![(on2.local_id, None)],
                reviews: vec![r.id],
            },
        ],
    );
    s.rewrite_local_change(id, &rw).unwrap().unwrap();
    // A review must be claimed on its own revision.
    assert_eq!(
        s.claim_local_post(
            id,
            c.id,
            Some((r.id, &[Pending][..])),
            PatchSetNumber(1),
            "n1",
            &who()
        )
        .unwrap(),
        None
    );
    let post = s
        .claim_local_post(
            id,
            c.id,
            Some((r.id, &[Pending][..])),
            PatchSetNumber(2),
            "n1",
            &who(),
        )
        .unwrap()
        .unwrap();
    let ids: Vec<LocalDraftId> = post.drafts.iter().map(|d| d.local_id).collect();
    assert_eq!(ids, [on2.local_id, r.message_draft.unwrap()]);
    assert_eq!(posting_nonce(&s, on1.local_id), None);
    // Refused: only what the post carried is held.
    let failed = ReviewStateChange::new(r.id, &[Sending], Failed, Some("400".into()));
    s.abandon_local_post(
        id,
        c.id,
        "n1",
        Some(&failed),
        Some("Gerrit refused the post: 400"),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        s.load_draft(id, on2.local_id)
            .unwrap()
            .unwrap()
            .held_reason
            .as_deref(),
        Some("Gerrit refused the post: 400")
    );
    assert_eq!(
        s.load_draft(id, on1.local_id).unwrap().unwrap().held_reason,
        None
    );
    // PS 1's comment is still free for its own post.
    let other = s
        .claim_local_post(id, c.id, None, PatchSetNumber(1), "n2", &who())
        .unwrap()
        .unwrap();
    assert_eq!(other.drafts.len(), 1);
    assert_eq!(other.drafts[0].local_id, on1.local_id);
}

#[test]
fn a_draft_written_on_a_closed_changes_version_is_held_at_once() {
    let (mut s, id, c, v1, _v2) = setup();
    let before = draft(&mut s, id, &c, &v1, "before");
    assert_eq!(before.held_reason, None);
    s.hold_closed_local_change(id, c.id, ChangeNumber(42), "change merged")
        .unwrap()
        .unwrap();
    let after = s
        .create_draft(
            id,
            ChangeNumber(42),
            &psref(&v1),
            &DraftInput {
                path: "src/a.rs".into(),
                side: Side::Revision,
                parent: None,
                location: Location::Line(3),
                in_reply_to: None,
                message: "after".into(),
                unresolved: false,
            },
            ts(T1),
        )
        .unwrap();
    assert_eq!(
        after.held_reason.as_deref(),
        Some(Store::CLOSED_HELD_REASON)
    );
    // Restored on Gerrit: the row is open again, the draft stays held.
    s.set_local_change_state(id, c.id, LocalChangeState::Local)
        .unwrap();
    assert!(
        s.load_draft(id, after.local_id)
            .unwrap()
            .unwrap()
            .held_reason
            .is_some()
    );
    // Drafts on Gerrit's own patch sets are not affected.
    let ordinary = s
        .create_draft(
            id,
            ChangeNumber(42),
            &gerrit_ps(1),
            &DraftInput {
                path: "src/a.rs".into(),
                side: Side::Revision,
                parent: None,
                location: Location::Line(3),
                in_reply_to: None,
                message: "ordinary".into(),
                unresolved: false,
            },
            ts(T1),
        )
        .unwrap();
    assert_eq!(ordinary.held_reason, None);
}
