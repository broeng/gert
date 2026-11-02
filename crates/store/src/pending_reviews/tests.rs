use model::{DraftInput, Location};

use super::*;

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

const T0: &str = "2024-03-01T10:00:00Z";
const T1: &str = "2024-03-01T11:00:00Z";

fn setup() -> (Store, ServerId) {
    let mut s = Store::open_in_memory().unwrap();
    let id = s.server_id("https://g.example").unwrap();
    (s, id)
}

fn local_change() -> ChangeNumber {
    ChangeNumber::local(1).unwrap()
}

fn local_ps(id: i64) -> PatchSetRef {
    PatchSetRef {
        number: PatchSetNumber::local(id).unwrap(),
        sha: format!("local{id}").as_str().into(),
    }
}

fn labels(cr: i32) -> BTreeMap<String, i32> {
    BTreeMap::from([("Code-Review".to_owned(), cr)])
}

fn new_review(message: &str) -> NewPendingReview {
    NewPendingReview {
        local_change: None,
        change: local_change(),
        patch_set: local_ps(1),
        labels: labels(2),
        strategy: LocalApproval::Follow,
        message: message.into(),
        created: ts(T0),
    }
}

#[test]
fn a_review_keeps_its_votes_strategy_and_sealed_message() {
    let (mut s, id) = setup();
    let r = s
        .create_pending_review(id, &new_review("Looks good"))
        .unwrap();
    assert_eq!(r.state, PendingReviewState::Pending);
    assert_eq!(
        (r.change, r.patch_set, r.given_on_sha.as_str()),
        (local_change(), local_ps(1).number, "local1")
    );
    assert_eq!(r.labels, labels(2));
    assert_eq!(r.strategy, LocalApproval::Follow);
    assert_eq!((r.created, r.nonce.clone()), (ts(T0), None));
    assert_eq!(r.message.as_deref(), Some("Looks good"));
    // The message is a sealed patch-set-level draft on the same numbers.
    let d = s.load_draft(id, r.message_draft.unwrap()).unwrap().unwrap();
    assert_eq!(d.pending_review, Some(r.id));
    assert!(d.is_sealed() && d.is_local());
    assert_eq!(
        (d.path.as_str(), d.location, d.unresolved),
        (PATCHSET_LEVEL_PATH, Location::File, false)
    );
    assert_eq!((d.change, d.patch_set), (r.change, r.patch_set));
    assert_eq!(s.load_pending_review(id, r.id).unwrap(), Some(r.clone()));
    // A blank message makes no draft.
    let quiet = s.create_pending_review(id, &new_review("  \n")).unwrap();
    assert_eq!((quiet.message, quiet.message_draft), (None, None));
    assert!(quiet.id > r.id);
}

#[test]
fn only_local_patch_sets_get_pending_reviews() {
    let (mut s, id) = setup();
    let mut input = new_review("m");
    input.patch_set = PatchSetRef {
        number: PatchSetNumber(3),
        sha: "gerrit3".into(),
    };
    assert!(matches!(
        s.create_pending_review(id, &input),
        Err(Error::NotLocal)
    ));
    assert!(s.pending_reviews(id, local_change()).unwrap().is_empty());
    assert_eq!(s.local_draft_count(id).unwrap(), 0);
}

#[test]
fn editing_replaces_votes_and_message_while_pending() {
    let (mut s, id) = setup();
    let r = s.create_pending_review(id, &new_review("first")).unwrap();
    let draft = r.message_draft.unwrap();
    let edit = |message: &str| PendingReviewEdit {
        labels: labels(1),
        strategy: LocalApproval::Exact,
        message: message.into(),
    };
    let e = s
        .update_pending_review(id, r.id, &edit("second"), ts(T1))
        .unwrap()
        .unwrap();
    assert_eq!(
        (e.labels.clone(), e.strategy),
        (labels(1), LocalApproval::Exact)
    );
    // Same draft, new text; the review keeps its creation time.
    assert_eq!(
        (e.message_draft, e.message.as_deref(), e.created),
        (Some(draft), Some("second"), ts(T0))
    );
    assert_eq!(s.load_draft(id, draft).unwrap().unwrap().version, 1);
    // Clearing the message removes its draft; writing one again adds one.
    let e = s
        .update_pending_review(id, r.id, &edit(""), ts(T1))
        .unwrap()
        .unwrap();
    assert_eq!((e.message, e.message_draft), (None, None));
    assert_eq!(s.load_draft(id, draft).unwrap(), None);
    let e = s
        .update_pending_review(id, r.id, &edit("third"), ts(T1))
        .unwrap()
        .unwrap();
    assert_eq!(e.message.as_deref(), Some("third"));
    // Once claimed for sending it can no longer change.
    assert!(s.mark_pending_review_sending(id, r.id, "n1").unwrap());
    assert_eq!(
        s.update_pending_review(id, r.id, &edit("late"), ts(T1))
            .unwrap(),
        None
    );
    assert_eq!(
        s.load_pending_review(id, r.id)
            .unwrap()
            .unwrap()
            .message
            .as_deref(),
        Some("third")
    );
    assert_eq!(
        s.update_pending_review(id, PendingReviewId(99), &edit("x"), ts(T1))
            .unwrap(),
        None
    );
}

#[test]
fn cancelling_keeps_inline_drafts_and_unseals_the_message() {
    let (mut s, id) = setup();
    let inline = s
        .create_draft(
            id,
            local_change(),
            &local_ps(1),
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
    let r = s.create_pending_review(id, &new_review("msg")).unwrap();
    assert!(s.cancel_pending_review(id, r.id).unwrap());
    assert_eq!(s.load_pending_review(id, r.id).unwrap(), None);
    assert!(s.load_draft(id, inline.local_id).unwrap().is_some());
    let message = s.load_draft(id, r.message_draft.unwrap()).unwrap().unwrap();
    assert_eq!(
        (message.message.as_str(), message.pending_review),
        ("msg", None)
    );
    // Gone already, and a review being sent cannot be cancelled.
    assert!(!s.cancel_pending_review(id, r.id).unwrap());
    let r2 = s.create_pending_review(id, &new_review("two")).unwrap();
    assert!(s.mark_pending_review_sending(id, r2.id, "n").unwrap());
    assert!(!s.cancel_pending_review(id, r2.id).unwrap());
    assert!(s.load_pending_review(id, r2.id).unwrap().is_some());
}

#[test]
fn only_one_run_claims_a_review_for_sending() {
    let (mut s, id) = setup();
    let r = s.create_pending_review(id, &new_review("m")).unwrap();
    assert!(s.mark_pending_review_sending(id, r.id, "first").unwrap());
    // A second run that read the review as pending loses the race.
    assert!(!s.mark_pending_review_sending(id, r.id, "second").unwrap());
    let r = s.load_pending_review(id, r.id).unwrap().unwrap();
    assert_eq!(
        (r.state, r.nonce.as_deref()),
        (PendingReviewState::Sending, Some("first"))
    );
    assert!(
        !s.mark_pending_review_sending(id, PendingReviewId(99), "x")
            .unwrap()
    );
}

#[test]
fn transitions_check_the_state_they_start_from_and_keep_notes() {
    use PendingReviewState::*;
    let (mut s, id) = setup();
    let r = s.create_pending_review(id, &new_review("m")).unwrap();
    // Wrong starting state: nothing happens.
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
    // Held reviews cannot be claimed directly; publishing makes them
    // pending again, which clears the reason.
    assert!(!s.mark_pending_review_sending(id, r.id, "n").unwrap());
    assert!(
        s.set_pending_review_state(id, r.id, &[Held, Failed], Pending, None)
            .unwrap()
    );
    assert!(s.mark_pending_review_sending(id, r.id, "n").unwrap());
    assert!(
        s.set_pending_review_state(id, r.id, &[Sending], Failed, Some("403"))
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
fn reviews_list_per_change_in_creation_order() {
    let (mut s, id) = setup();
    let other = s.server_id("https://b.example").unwrap();
    let a = s.create_pending_review(id, &new_review("a")).unwrap();
    let mut on_gerrit = new_review("g");
    on_gerrit.change = ChangeNumber(7);
    on_gerrit.patch_set = local_ps(2);
    let g = s.create_pending_review(id, &on_gerrit).unwrap();
    let b = s.create_pending_review(id, &new_review("b")).unwrap();
    s.create_pending_review(other, &new_review("x")).unwrap();
    let ids = |v: Vec<PendingReview>| v.into_iter().map(|r| r.id).collect::<Vec<_>>();
    assert_eq!(
        ids(s.pending_reviews(id, local_change()).unwrap()),
        [a.id, b.id]
    );
    assert_eq!(ids(s.pending_reviews(id, ChangeNumber(7)).unwrap()), [g.id]);
    // A review of another server is not visible here.
    assert_eq!(s.pending_reviews(other, ChangeNumber(7)).unwrap(), []);
    assert_eq!(s.load_pending_review(other, a.id).unwrap(), None);
}

#[test]
fn an_unknown_strategy_reads_as_the_strictest() {
    let (mut s, id) = setup();
    let r = s.create_pending_review(id, &new_review("m")).unwrap();
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
