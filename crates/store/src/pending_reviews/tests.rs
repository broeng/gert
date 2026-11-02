use model::{DraftInput, LocalChange, LocalPatchSet, Location};

use super::*;
use crate::{DeleteOutcome, LocalChangeUpsert, LocalOp, NewLocalPatchSet};

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

const T0: &str = "2024-03-01T10:00:00Z";
const T1: &str = "2024-03-01T11:00:00Z";
const T2: &str = "2024-03-01T12:00:00Z";

struct Fx {
    s: Store,
    id: ServerId,
    change: LocalChange,
    versions: Vec<LocalPatchSet>,
}

/// A local change in project `p` with `shas` as its versions; with a
/// Gerrit number, local patch sets of that Gerrit change.
fn add_change(s: &mut Store, id: ServerId, change_id: &str, gerrit: Option<u64>) -> LocalChange {
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
        patch_sets: ["1", "2"]
            .iter()
            .map(|n| NewLocalPatchSet {
                sha: format!("{change_id}-v{n}").as_str().into(),
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

fn fx() -> Fx {
    let mut s = Store::open_in_memory().unwrap();
    let id = s.server_id("https://g.example").unwrap();
    let change = add_change(&mut s, id, "I1", None);
    let versions = s.local_patch_sets(id, change.id).unwrap();
    Fx {
        s,
        id,
        change,
        versions,
    }
}

fn labels(cr: i32) -> BTreeMap<String, i32> {
    BTreeMap::from([("Code-Review".to_owned(), cr)])
}

fn new_review(patch_set: PatchSetNumber, message: &str) -> NewPendingReview {
    NewPendingReview {
        patch_set,
        labels: labels(2),
        strategy: LocalApproval::Follow,
        message: message.into(),
        unresolved: false,
        created: ts(T0),
    }
}

fn edit(cr: i32, message: &str) -> PendingReviewEdit {
    PendingReviewEdit {
        labels: labels(cr),
        strategy: LocalApproval::Exact,
        message: message.into(),
        unresolved: true,
    }
}

#[test]
fn a_review_takes_its_place_from_the_patch_set_and_seals_its_message() {
    let Fx {
        mut s,
        id,
        change,
        versions,
    } = fx();
    let v1 = &versions[0];
    let r = s
        .create_pending_review(id, &new_review(v1.number, "Looks good"))
        .unwrap();
    assert_eq!(r.state, PendingReviewState::Pending);
    assert_eq!(
        (r.local_change, r.change, r.patch_set),
        (change.id, change.number, v1.number)
    );
    assert_eq!((&r.revision, &r.given_on_sha), (&v1.sha, &v1.sha));
    assert_eq!(
        (r.labels.clone(), r.strategy),
        (labels(2), LocalApproval::Follow)
    );
    assert_eq!(
        (r.created, r.voted_at, r.nonce.clone()),
        (ts(T0), ts(T0), None)
    );
    assert_eq!(r.message.as_deref(), Some("Looks good"));
    // The message is a sealed patch-set-level draft on the same numbers.
    let d = s.load_draft(id, r.message_draft.unwrap()).unwrap().unwrap();
    assert_eq!(d.pending_review, Some(r.id));
    assert!(d.is_sealed() && d.is_local() && !d.is_ordinary());
    assert_eq!(
        (d.path.as_str(), d.location, d.unresolved),
        (PATCHSET_LEVEL_PATH, Location::File, false)
    );
    assert_eq!(
        (d.change, d.patch_set, &d.revision),
        (r.change, r.patch_set, &v1.sha)
    );
    assert_eq!(s.load_pending_review(id, r.id).unwrap(), Some(r.clone()));
    // A blank message makes no draft.
    let quiet = s
        .create_pending_review(id, &new_review(v1.number, "  \n"))
        .unwrap();
    assert_eq!((quiet.message, quiet.message_draft), (None, None));
    assert!(quiet.id > r.id);
}

#[test]
fn a_review_on_a_local_patch_set_of_a_gerrit_change_goes_to_that_change() {
    let Fx { mut s, id, .. } = fx();
    let g = add_change(&mut s, id, "IG", Some(7));
    let v = s.local_patch_sets(id, g.id).unwrap().remove(0);
    let r = s
        .create_pending_review(id, &new_review(v.number, "m"))
        .unwrap();
    assert_eq!((r.local_change, r.change), (g.id, ChangeNumber(7)));
}

#[test]
fn only_known_local_patch_sets_get_pending_reviews() {
    let Fx {
        mut s, id, change, ..
    } = fx();
    let other = s.server_id("https://b.example").unwrap();
    let v = s.local_patch_sets(id, change.id).unwrap().remove(0);
    for (server, ps) in [
        (id, PatchSetNumber(3)),
        (id, PatchSetNumber::local(999).unwrap()),
        (other, v.number),
    ] {
        assert!(matches!(
            s.create_pending_review(server, &new_review(ps, "m")),
            Err(Error::NotLocal)
        ));
    }
    assert!(s.pending_reviews(id, change.number).unwrap().is_empty());
    assert_eq!(s.local_draft_count(id).unwrap(), 0);
}

#[test]
fn editing_changes_votes_and_message_while_pending() {
    let Fx {
        mut s,
        id,
        versions,
        ..
    } = fx();
    let r = s
        .create_pending_review(id, &new_review(versions[0].number, "first"))
        .unwrap();
    let draft = r.message_draft.unwrap();
    // A message-only edit is not a new vote.
    let e = s
        .update_pending_review(id, r.id, &edit(2, "second"), ts(T1))
        .unwrap()
        .unwrap();
    assert_eq!(
        (e.strategy, e.voted_at, e.message.as_deref()),
        (LocalApproval::Follow, ts(T0), Some("second"))
    );
    let d = s.load_draft(id, draft).unwrap().unwrap();
    assert_eq!((d.version, d.unresolved), (1, true));
    // Changing the labels is: the mode and the time follow.
    let e = s
        .update_pending_review(id, r.id, &edit(1, "second"), ts(T2))
        .unwrap()
        .unwrap();
    assert_eq!(
        (e.labels.clone(), e.strategy, e.voted_at, e.created),
        (labels(1), LocalApproval::Exact, ts(T2), ts(T0))
    );
    // Clearing the message removes its draft; writing one again adds one.
    let e = s
        .update_pending_review(id, r.id, &edit(1, ""), ts(T2))
        .unwrap()
        .unwrap();
    assert_eq!((e.message, e.message_draft), (None, None));
    assert_eq!(s.load_draft(id, draft).unwrap(), None);
    let e = s
        .update_pending_review(id, r.id, &edit(1, "third"), ts(T2))
        .unwrap()
        .unwrap();
    assert_eq!(e.message.as_deref(), Some("third"));
    // Once claimed for sending it can no longer change.
    s.mark_pending_review_sending(id, r.id, &[PendingReviewState::Pending], "n1")
        .unwrap()
        .unwrap();
    assert_eq!(
        s.update_pending_review(id, r.id, &edit(0, "late"), ts(T2))
            .unwrap(),
        None
    );
    assert_eq!(
        s.update_pending_review(id, PendingReviewId(99), &edit(0, "x"), ts(T2))
            .unwrap(),
        None
    );
}

#[test]
fn sealed_messages_stay_out_of_the_draft_paths() {
    let Fx {
        mut s,
        id,
        versions,
        ..
    } = fx();
    let r = s
        .create_pending_review(id, &new_review(versions[0].number, "msg"))
        .unwrap();
    let d = r.message_draft.unwrap();
    assert_eq!(s.edit_draft(id, d, "hijack", true, ts(T1)).unwrap(), None);
    assert_eq!(s.delete_draft(id, d).unwrap(), DeleteOutcome::Sealed);
    // An upload that somehow happened is not recorded: the caller deletes
    // the server's copy, and the draft never gets a server id.
    assert_eq!(
        s.record_draft_uploaded(id, d, 0, "srv", ts(T1)).unwrap(),
        None
    );
    let d = s.load_draft(id, d).unwrap().unwrap();
    assert_eq!((d.message.as_str(), d.server_id), ("msg", None));
    assert!(s.pending_drafts(id).unwrap().is_empty());
}

#[test]
fn cancelling_a_pending_review_keeps_inline_drafts_and_unseals_the_message() {
    let Fx {
        mut s,
        id,
        change,
        versions,
        ..
    } = fx();
    let v1 = &versions[0];
    let inline = s
        .create_draft(
            id,
            change.number,
            &model::PatchSetRef {
                number: v1.number,
                sha: v1.sha.clone(),
            },
            &DraftInput {
                path: "f".into(),
                side: Side::Revision,
                parent: None,
                location: Location::Line(3),
                in_reply_to: None,
                message: "nit".into(),
                unresolved: true,
            },
            ts(T0),
        )
        .unwrap();
    let r = s
        .create_pending_review(id, &new_review(v1.number, "msg"))
        .unwrap();
    assert!(s.cancel_pending_review(id, r.id).unwrap());
    assert_eq!(s.load_pending_review(id, r.id).unwrap(), None);
    assert!(s.load_draft(id, inline.local_id).unwrap().is_some());
    let message = s.load_draft(id, r.message_draft.unwrap()).unwrap().unwrap();
    assert_eq!(
        (message.message.as_str(), message.pending_review),
        ("msg", None)
    );
    assert!(!s.cancel_pending_review(id, r.id).unwrap());
    // Only a pending review can be cancelled.
    let r2 = s
        .create_pending_review(id, &new_review(v1.number, "two"))
        .unwrap();
    s.set_pending_review_state(
        id,
        r2.id,
        &[PendingReviewState::Pending],
        PendingReviewState::Held,
        Some("x"),
    )
    .unwrap();
    assert!(!s.cancel_pending_review(id, r2.id).unwrap());
}

#[test]
fn dropping_a_held_or_failed_review_discards_its_message() {
    use PendingReviewState::*;
    let Fx {
        mut s,
        id,
        versions,
        ..
    } = fx();
    let v1 = versions[0].number;
    let pending = s.create_pending_review(id, &new_review(v1, "p")).unwrap();
    assert!(!s.drop_pending_review(id, pending.id).unwrap());
    for state in [Held, Failed] {
        let r = s.create_pending_review(id, &new_review(v1, "m")).unwrap();
        s.set_pending_review_state(id, r.id, &[Pending], state, Some("why"))
            .unwrap();
        assert!(s.drop_pending_review(id, r.id).unwrap());
        assert_eq!(s.load_pending_review(id, r.id).unwrap(), None);
        assert_eq!(s.load_draft(id, r.message_draft.unwrap()).unwrap(), None);
    }
    assert!(s.load_pending_review(id, pending.id).unwrap().is_some());
}

#[test]
fn claiming_for_sending_is_a_compare_and_set_that_returns_the_claim() {
    use PendingReviewState::*;
    let Fx {
        mut s,
        id,
        versions,
        ..
    } = fx();
    let r = s
        .create_pending_review(id, &new_review(versions[0].number, "m"))
        .unwrap();
    let claimed = s
        .mark_pending_review_sending(id, r.id, &[Pending], "first")
        .unwrap()
        .unwrap();
    assert_eq!(
        (claimed.state, claimed.nonce.as_deref(), claimed.labels),
        (Sending, Some("first"), labels(2))
    );
    // A second run that read the review as pending loses the race, and a
    // review being sent is never claimed again.
    assert_eq!(
        s.mark_pending_review_sending(id, r.id, &[Pending], "second")
            .unwrap(),
        None
    );
    assert_eq!(
        s.mark_pending_review_sending(id, r.id, &[Sending], "third")
            .unwrap(),
        None
    );
    // A failed one can be claimed for a retry, with a new nonce.
    s.set_pending_review_state(id, r.id, &[Sending], Failed, Some("500"))
        .unwrap();
    assert_eq!(
        s.mark_pending_review_sending(id, r.id, &[Pending], "x")
            .unwrap(),
        None
    );
    let retry = s
        .mark_pending_review_sending(id, r.id, &[Held, Failed], "retry")
        .unwrap()
        .unwrap();
    assert_eq!((retry.nonce.as_deref(), retry.error), (Some("retry"), None));
    assert_eq!(
        s.mark_pending_review_sending(id, PendingReviewId(99), &[Pending], "x")
            .unwrap(),
        None
    );
}

#[test]
fn transitions_check_the_state_they_start_from_and_keep_notes() {
    use PendingReviewState::*;
    let Fx {
        mut s,
        id,
        versions,
        ..
    } = fx();
    let r = s
        .create_pending_review(id, &new_review(versions[0].number, "m"))
        .unwrap();
    assert!(
        !s.set_pending_review_state(id, r.id, &[Sending], Sent, None)
            .unwrap()
    );
    assert!(
        s.set_pending_review_state(id, r.id, &[Pending], Held, Some("not exact"))
            .unwrap()
    );
    let held = s.load_pending_review(id, r.id).unwrap().unwrap();
    assert_eq!(
        (held.state, held.held_reason.as_deref(), held.error),
        (Held, Some("not exact"), None)
    );
    assert!(
        s.set_pending_review_state(id, r.id, &[Held], Failed, Some("403"))
            .unwrap()
    );
    let failed = s.load_pending_review(id, r.id).unwrap().unwrap();
    assert_eq!(
        (failed.state, failed.error.as_deref(), failed.held_reason),
        (Failed, Some("403"), None)
    );
    assert!(
        s.set_pending_review_state(id, r.id, &[Failed], Sent, Some("ignored"))
            .unwrap()
    );
    let sent = s.load_pending_review(id, r.id).unwrap().unwrap();
    assert_eq!(
        (sent.state, sent.error.clone(), sent.held_reason.clone()),
        (Sent, None, None)
    );
    assert!(!sent.is_unsent());
    assert!(
        !s.set_pending_review_state(id, PendingReviewId(99), &[Pending], Sent, None)
            .unwrap()
    );
}

#[test]
fn a_superseded_label_is_removed_without_revoting() {
    use PendingReviewState::*;
    let Fx {
        mut s,
        id,
        versions,
        ..
    } = fx();
    let mut input = new_review(versions[0].number, "m");
    input.labels.insert("Verified".into(), 1);
    let r = s.create_pending_review(id, &input).unwrap();
    s.set_pending_review_state(id, r.id, &[Pending], Held, Some("x"))
        .unwrap();
    let r2 = s
        .remove_review_label(id, r.id, "Code-Review")
        .unwrap()
        .unwrap();
    assert_eq!(r2.labels, BTreeMap::from([("Verified".to_owned(), 1)]));
    assert_eq!(
        (r2.voted_at, r2.strategy, r2.state),
        (r.voted_at, r.strategy, Held)
    );
    assert_eq!(
        s.load_pending_review(id, r.id).unwrap().unwrap().labels,
        r2.labels
    );
    // Unknown labels change nothing; a review being sent is left alone.
    s.remove_review_label(id, r.id, "Nope").unwrap().unwrap();
    s.mark_pending_review_sending(id, r.id, &[Held], "n")
        .unwrap()
        .unwrap();
    assert_eq!(s.remove_review_label(id, r.id, "Verified").unwrap(), None);
}

#[test]
fn reviews_list_per_change_in_creation_order() {
    let Fx {
        mut s,
        id,
        change,
        versions,
    } = fx();
    let g = add_change(&mut s, id, "IG", Some(7));
    let gv = s.local_patch_sets(id, g.id).unwrap().remove(0);
    let a = s
        .create_pending_review(id, &new_review(versions[0].number, "a"))
        .unwrap();
    let on_gerrit = s
        .create_pending_review(id, &new_review(gv.number, "g"))
        .unwrap();
    let b = s
        .create_pending_review(id, &new_review(versions[1].number, "b"))
        .unwrap();
    let ids = |v: Vec<PendingReview>| v.into_iter().map(|r| r.id).collect::<Vec<_>>();
    assert_eq!(
        ids(s.pending_reviews(id, change.number).unwrap()),
        [a.id, b.id]
    );
    assert_eq!(
        ids(s.pending_reviews(id, ChangeNumber(7)).unwrap()),
        [on_gerrit.id]
    );
    assert_eq!(
        ids(s.pending_reviews_of_local_change(id, change.id).unwrap()),
        [a.id, b.id]
    );
    let other = s.server_id("https://b.example").unwrap();
    assert_eq!(s.load_pending_review(other, a.id).unwrap(), None);
}

#[test]
fn an_unknown_strategy_reads_as_the_strictest() {
    let Fx {
        mut s,
        id,
        versions,
        ..
    } = fx();
    let r = s
        .create_pending_review(id, &new_review(versions[0].number, "m"))
        .unwrap();
    s.conn
        .execute(
            "UPDATE pending_reviews SET strategy = 'yolo' WHERE id = ?1",
            [r.id.0],
        )
        .unwrap();
    assert_eq!(
        s.load_pending_review(id, r.id).unwrap().unwrap().strategy,
        LocalApproval::Exact
    );
}
