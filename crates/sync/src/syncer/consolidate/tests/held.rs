//! The held card's actions: dropping and publishing held items.

use super::*;
use crate::{HeldItem, HeldPublished};

/// A local patch set of change 7 that became PS 1 while PS 2 is current:
/// its comments and message were posted, its votes held (Verified +1, an
/// approval, drop-only; Code-Review +1 publishable).
fn held_votes(f: &Fx) -> (LocalChange, PendingReview) {
    let (row, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let r = f.review(&v[0], &[("Code-Review", 1), ("Verified", 1)], "LGTM");
    f.gerrit("NEW", &["s1", "s2"], json!([]));
    f.posts_apply(json!({}), 2);
    f.run();
    let r = f.reloaded(&r);
    assert_eq!(r.state, PendingReviewState::Held);
    assert_eq!(r.drop_only, ["Verified".to_owned()].into());
    (row, r)
}

/// A local change with versions l1 (never on Gerrit) and s1, found merged
/// as change 7 with s1 as PS 1: its review on s1 (Code-Review +2, "LGTM")
/// and its comment on l1 are held.
fn held_closed(f: &Fx) -> (LocalChange, PendingReview, Draft) {
    let (row, v) = f.local(CID, Some("main"), None, &["l1", "s1"]);
    let d = f.draft(&row, &v[0], "a comment");
    let r = f.review(&v[1], &[("Code-Review", 2)], "LGTM");
    f.query_finds(&[(7, "main", "MERGED")]);
    f.gerrit("MERGED", &["s1"], json!([]));
    let report = f.run();
    assert_eq!(report.closed, [(row.id, CHANGE)]);
    assert!(f.http.posts().is_empty());
    (row, f.reloaded(&r), f.load_draft(&d).unwrap())
}

fn publish(f: &Fx, onto: u32, items: &[HeldItem]) -> Result<HeldPublished, Error> {
    block_on(
        f.syncer
            .publish_held(CHANGE, PatchSetNumber(onto), items, LocalApproval::Exact),
    )
}

fn vote(r: &PendingReview, label: &str) -> HeldItem {
    HeldItem::Vote {
        review: r.id,
        label: label.into(),
    }
}

fn held(f: &Fx) -> store::HeldItems {
    f.syncer.held_items(CHANGE).unwrap()
}

#[test]
fn a_publishable_vote_is_posted_to_the_current_patch_set() {
    let f = fx();
    let (row, r) = held_votes(&f);
    let before = f.http.posts().len();
    f.posts_apply(json!({"Code-Review": 1}), 2);
    let out = publish(&f, 2, &[vote(&r, "Code-Review")]).unwrap();
    assert_eq!(
        out,
        HeldPublished {
            patch_set: PatchSetNumber(2),
            votes: vec![("Code-Review".into(), 1)],
            comments: 0,
            outcome: PostOutcome::Sent,
        }
    );
    let posts = f.http.posts();
    assert_eq!(posts.len(), before + 1);
    assert!(
        f.http.post_targets()[before].contains("/changes/7/revisions/s2/review"),
        "{:?}",
        f.http.post_targets()
    );
    let body = &posts[before];
    assert_eq!(body["labels"], json!({"Code-Review": 1}));
    assert_eq!(body["drafts"], "KEEP");
    assert_eq!(body["ignore_automatic_attention_set_rules"], true);
    assert!(body.get("comments").is_none() && body.get("notify").is_none());
    let tag = body["tag"].as_str().unwrap();
    assert!(
        tag.starts_with(&format!("client-local-{}-r", row.id.0)),
        "{tag}"
    );
    // The vote left the held list; the drop-only approval stays.
    let now = held(&f);
    assert_eq!(now.reviews.len(), 1);
    assert_eq!(
        now.reviews[0].labels,
        BTreeMap::from([("Verified".to_owned(), 1)])
    );
    assert!(
        f.syncer
            .store()
            .unfinished_local_posts(f.syncer.server)
            .unwrap()
            .is_empty()
    );
    // The held approval keeps the change open.
    assert_eq!(f.row(&row).state, LocalChangeState::Local);
    f.assert_nothing_local(&[]);
}

#[test]
fn a_drop_only_vote_is_never_published_and_can_be_dropped() {
    let f = fx();
    let (row, r) = held_votes(&f);
    let requests = f.http.routes.requested().len();
    for items in [
        vec![vote(&r, "Verified")],
        vec![vote(&r, "Code-Review"), vote(&r, "Verified")],
    ] {
        let e = publish(&f, 2, &items).unwrap_err();
        assert!(matches!(&e, Error::DropOnly(l) if l == "Verified"), "{e}");
        assert!(e.to_string().contains("exact commit"), "{e}");
    }
    // Not a single request.
    assert_eq!(f.http.routes.requested().len(), requests);
    assert_eq!(held(&f).reviews[0].labels.len(), 2);
    assert!(f.syncer.drop_held(&vote(&r, "Verified")).unwrap());
    assert!(!f.syncer.drop_held(&vote(&r, "Verified")).unwrap());
    assert!(f.syncer.drop_held(&vote(&r, "Code-Review")).unwrap());
    // Nothing is left: the review is gone, the change a tombstone.
    assert!(held(&f).reviews.is_empty());
    assert_eq!(f.row(&row).state, LocalChangeState::Consolidated);
}

#[test]
fn a_held_message_is_published_as_the_patch_set_message() {
    let f = fx();
    let (_, r, _) = held_closed(&f);
    f.posts_apply(json!({}), 1);
    let out = publish(&f, 1, &[HeldItem::Message(r.id)]).unwrap();
    assert_eq!((out.comments, out.outcome), (1, PostOutcome::Sent));
    let body = &f.http.posts()[0];
    assert!(body.get("labels").is_none(), "{body}");
    assert_eq!(
        body["comments"][PATCHSET_LEVEL_PATH],
        json!([{"message": "LGTM", "unresolved": false}])
    );
    assert_eq!(body["drafts"], "KEEP");
    // The review keeps its vote, without the message.
    let now = f.reloaded(&r);
    assert_eq!(
        (now.state, now.message_draft),
        (PendingReviewState::Held, None)
    );
    assert_eq!(now.labels.len(), 1);
}

#[test]
fn a_held_comment_is_posted_as_a_patch_set_comment_quoting_its_place() {
    let f = fx();
    let (row, _, d) = held_closed(&f);
    f.posts_apply(json!({}), 1);
    publish(&f, 1, &[HeldItem::Comment(d.local_id)]).unwrap();
    assert!(f.http.post_targets()[0].contains("/revisions/s1/review"));
    let body = &f.http.posts()[0];
    assert_eq!(
        body["comments"],
        json!({PATCHSET_LEVEL_PATH: [{
            "message": "On local 1, src/a.rs line 7: a comment",
            "unresolved": true
        }]})
    );
    assert_eq!(f.load_draft(&d), None);
    assert!(held(&f).drafts.is_empty());
    // Neither the local version's commit nor a synthetic number went out.
    f.assert_nothing_local(&["l1"]);
    assert_eq!(f.row(&row).state, LocalChangeState::Closed);
}

#[test]
fn a_post_submit_vote_goes_to_the_merged_change_and_the_last_item_settles_it() {
    let f = fx();
    let (row, r, d) = held_closed(&f);
    f.posts_apply(json!({"Code-Review": 2}), 1);
    let out = publish(&f, 1, &[vote(&r, "Code-Review"), HeldItem::Message(r.id)]).unwrap();
    assert_eq!(out.votes, [("Code-Review".to_owned(), 2)]);
    let body = &f.http.posts()[0];
    assert_eq!(body["labels"], json!({"Code-Review": 2}));
    assert_eq!(body["comments"][PATCHSET_LEVEL_PATH][0]["message"], "LGTM");
    assert!(held(&f).reviews.is_empty());
    assert_eq!(f.row(&row).state, LocalChangeState::Closed);
    assert!(f.syncer.drop_held(&HeldItem::Comment(d.local_id)).unwrap());
    // Nothing held is left: a tombstone that keeps its Gerrit change for
    // redirects.
    let now = f.row(&row);
    assert_eq!(
        (now.state, now.gerrit_number),
        (LocalChangeState::Consolidated, Some(CHANGE))
    );
}

#[test]
fn a_refused_vote_does_not_sink_the_comments() {
    let f = fx();
    let (_, r, d) = held_closed(&f);
    f.http.queue(
        "POST /changes/7/revisions/",
        409,
        "Cannot reduce vote on labels for closed change: Code-Review",
    );
    f.posts_apply(json!({}), 1);
    let out = publish(
        &f,
        1,
        &[vote(&r, "Code-Review"), HeldItem::Comment(d.local_id)],
    )
    .unwrap();
    assert!(
        matches!(&out.outcome, PostOutcome::VotesFailed { votes, .. } if votes.len() == 1),
        "{out:?}"
    );
    let posts = f.http.posts();
    assert_eq!(posts.len(), 2);
    assert!(posts[1].get("labels").is_none());
    assert_eq!(posts[1]["tag"], posts[0]["tag"]);
    assert_eq!(f.load_draft(&d), None);
    // The vote is back in the held card, failed with Gerrit's text.
    let now = held(&f);
    let failed = now
        .reviews
        .iter()
        .find(|x| x.state == PendingReviewState::Failed)
        .unwrap();
    assert_eq!(
        failed.labels,
        BTreeMap::from([("Code-Review".to_owned(), 2)])
    );
    assert!(failed.error.as_deref().unwrap().contains("closed change"));
}

#[test]
fn a_refused_publish_puts_its_items_back_with_gerrits_text() {
    let f = fx();
    let (_, _, d) = held_closed(&f);
    f.http
        .queue("POST /changes/7/revisions/", 403, "not permitted");
    let out = publish(&f, 1, &[HeldItem::Comment(d.local_id)]).unwrap();
    assert_eq!(
        out.outcome,
        PostOutcome::Refused {
            error: "not permitted".into()
        }
    );
    let now = held(&f);
    assert_eq!(now.drafts.len(), 1);
    assert_eq!(
        now.drafts[0].held_reason.as_deref(),
        Some("Gerrit refused the post: not permitted")
    );
    assert!(
        f.syncer
            .store()
            .unfinished_local_posts(f.syncer.server)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn publishing_while_a_consolidation_run_holds_the_lease_sends_nothing() {
    let f = fx();
    let (_, r) = held_votes(&f);
    let now = Timestamp::now();
    assert!(
        f.syncer
            .store()
            .take_consolidation_lease(
                f.syncer.server,
                "another run",
                now,
                now.checked_add(CLAIM_STALE).unwrap()
            )
            .unwrap()
    );
    let requests = f.http.routes.requested().len();
    let e = publish(&f, 2, &[vote(&r, "Code-Review")]).unwrap_err();
    assert!(matches!(e, Error::ConsolidationBusy), "{e}");
    assert_eq!(f.http.routes.requested().len(), requests);
    assert_eq!(held(&f).reviews[0].labels.len(), 2);
    // The run's lease is not touched; once it ends, the publish goes.
    f.syncer
        .store()
        .end_consolidation_lease(f.syncer.server, "another run")
        .unwrap();
    f.posts_apply(json!({"Code-Review": 1}), 2);
    publish(&f, 2, &[vote(&r, "Code-Review")]).unwrap();
}

#[test]
fn a_patch_set_that_is_no_longer_current_is_refused_before_posting() {
    let f = fx();
    let (_, r) = held_votes(&f);
    let posts = f.http.posts().len();
    let e = publish(&f, 1, &[vote(&r, "Code-Review")]).unwrap_err();
    assert!(
        matches!(
            e,
            Error::NotCurrent {
                asked: PatchSetNumber(1),
                current: PatchSetNumber(2)
            }
        ),
        "{e}"
    );
    assert_eq!(f.http.posts().len(), posts);
    // Items that are not held (any more) are refused too.
    let e = publish(&f, 2, &[vote(&r, "Other")]).unwrap_err();
    assert!(matches!(e, Error::HeldChanged), "{e}");
    assert_eq!(f.http.posts().len(), posts);
}

#[test]
fn a_publish_interrupted_after_its_post_is_settled_by_the_tag() {
    let f = fx_with(eager());
    let (_, r, _) = held_closed(&f);
    // No answer: the claim stays for the next consolidation run.
    f.http
        .queue("POST /changes/7/revisions/", 0, "connection reset");
    let e = publish(&f, 1, &[HeldItem::Message(r.id)]).unwrap_err();
    assert!(matches!(e, Error::Api(_)), "{e}");
    let tag = first_tag(&f);
    let unfinished = f
        .syncer
        .store()
        .unfinished_local_posts(f.syncer.server)
        .unwrap();
    assert_eq!(unfinished.len(), 1);
    // It did arrive: the change shows our message with the tag.
    f.gerrit(
        "MERGED",
        &["s1"],
        json!([{"id": "m1", "author": {"_account_id": ME}, "tag": tag,
                "message": "Patch Set 1:\n\n(1 comment)", "date": "2024-01-03 00:00:00.000000000",
                "_revision_number": 1}]),
    );
    let report = f.run();
    assert!(report.posts.iter().any(|p| p.recovered), "{report:?}");
    // Not posted a second time.
    assert_eq!(f.http.posts().len(), 1);
    assert!(
        f.syncer
            .store()
            .unfinished_local_posts(f.syncer.server)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn quotes_say_where_a_comment_was() {
    let mut d = Draft {
        local_id: LocalDraftId(1),
        server_id: None,
        change: CHANGE,
        patch_set: PatchSetNumber(1),
        revision: "s1".into(),
        path: "src/a.rs".into(),
        side: Side::Revision,
        parent: None,
        location: Location::Line(40),
        in_reply_to: None,
        message: "why?".into(),
        unresolved: true,
        updated: Timestamp::UNIX_EPOCH,
        sync: DraftSync::PendingCreate,
        attempts: 0,
        last_error: None,
        version: 0,
        pending_review: None,
        reserved_by: None,
        held_reason: Some("x".into()),
    };
    assert_eq!(
        super::super::held::quote("local 2", &d),
        "On local 2, src/a.rs line 40: why?"
    );
    d.location = Location::Range(model::CommentRange {
        start_line: 3,
        start_character: 0,
        end_line: 5,
        end_character: 1,
    });
    d.side = Side::Parent;
    assert_eq!(
        super::super::held::quote("PS 1", &d),
        "On PS 1, src/a.rs (base) lines 3-5: why?"
    );
    d.location = Location::File;
    d.path = model::COMMIT_MSG_PATH.into();
    d.side = Side::Revision;
    assert_eq!(
        super::super::held::quote("local 1", &d),
        "On local 1, the commit message: why?"
    );
    d.path = PATCHSET_LEVEL_PATH.into();
    assert_eq!(super::super::held::quote("local 1", &d), "On local 1: why?");
}

// ---- approvals that may not move ----

/// Project `p` under `follow`.
fn follow() -> ConsolidateOptions {
    ConsolidateOptions {
        local_approval: BTreeMap::from([("p".to_owned(), LocalApproval::Follow)]),
        ..ConsolidateOptions::default()
    }
}

/// The answer to a post that applies nothing.
fn applies_nothing(f: &Fx, current: u32) {
    f.posts_apply(json!({}), current);
}

#[test]
fn a_non_exact_approval_on_a_closed_change_is_drop_only() {
    // +2 given on l1, which never reached Gerrit; the change was merged
    // with another commit (s9).
    let f = fx();
    let (_, v) = f.local(CID, Some("main"), None, &["l1"]);
    let r = f.review(&v[0], &[("Code-Review", 2), ("Verified", -1)], "");
    f.query_finds(&[(7, "main", "MERGED")]);
    f.gerrit("MERGED", &["s9"], json!([]));
    f.run();
    let r = f.reloaded(&r);
    assert_eq!(r.state, PendingReviewState::Held);
    // Recorded when held, from the change's labels.
    assert_eq!(r.drop_only, ["Code-Review".to_owned()].into());
    // And judged again at publish time, also for data held before that
    // was recorded.
    f.syncer
        .store()
        .set_held_drop_only(f.syncer.server, r.id, &BTreeSet::new())
        .unwrap();
    let e = publish(&f, 1, &[vote(&r, "Code-Review")]).unwrap_err();
    assert!(
        matches!(&e, Error::DropOnly(l) if l == "Code-Review"),
        "{e}"
    );
    assert!(f.http.posts().is_empty());
    // Not an approval: publishable.
    f.posts_apply(json!({"Verified": -1}), 1);
    publish(&f, 1, &[vote(&r, "Verified")]).unwrap();
    assert_eq!(f.http.posts()[0]["labels"], json!({"Verified": -1}));
}

#[test]
fn a_failed_approval_is_drop_only_once_the_patch_set_moved_on() {
    let f = fx();
    let (_, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let r = f.review(&v[0], &[("Code-Review", 2)], "LGTM");
    f.gerrit("NEW", &["s1"], json!([]));
    f.http.queue(
        "POST /changes/7/revisions/",
        403,
        "Applying label \"Code-Review\": 2 is restricted",
    );
    applies_nothing(&f, 1);
    f.run();
    let r = f.reloaded(&r);
    assert_eq!(r.state, PendingReviewState::Failed);
    // PS 2 is uploaded: the +2 was given on PS 1.
    f.gerrit("NEW", &["s1", "s2"], json!([]));
    let posts = f.http.posts().len();
    let e = publish(&f, 2, &[vote(&r, "Code-Review")]).unwrap_err();
    assert!(matches!(&e, Error::DropOnly(_)), "{e}");
    assert_eq!(f.http.posts().len(), posts);
    // Dropping works.
    assert!(f.syncer.drop_held(&vote(&r, "Code-Review")).unwrap());
}

#[test]
fn another_accounts_approval_is_drop_only_once_moved_on() {
    let f = fx();
    let (_, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let server = f.syncer.server;
    f.syncer
        .store()
        .set_account(server, AccountId(2000), Some("other"), None)
        .unwrap();
    let r = f.review(&v[0], &[("Code-Review", 2)], "");
    f.syncer
        .store()
        .set_account(server, AccountId(ME), Some("me"), None)
        .unwrap();
    f.gerrit("NEW", &["s1"], json!([]));
    applies_nothing(&f, 1);
    f.run();
    let r = f.reloaded(&r);
    assert_eq!(r.state, PendingReviewState::Held);
    // Given on PS 1: not publishable once PS 2 is current.
    f.gerrit("NEW", &["s1", "s2"], json!([]));
    let e = publish(&f, 2, &[vote(&r, "Code-Review")]).unwrap_err();
    assert!(matches!(&e, Error::DropOnly(_)), "{e}");
    assert!(f.http.posts().is_empty());
}

#[test]
fn a_purely_local_approval_under_follow_may_still_be_published() {
    let f = fx();
    let (_, v) = f.local(CID, Some("main"), None, &["l1"]);
    // Given long before Gerrit's change was created (2024-01-01).
    let r = f.review_with(
        &v[0],
        &[("Code-Review", 2)],
        "",
        LocalApproval::Follow,
        ts("2023-06-01T00:00:00Z"),
    );
    f.query_finds(&[(7, "main", "MERGED")]);
    f.gerrit("MERGED", &["s9"], json!([]));
    f.run_with(&follow());
    let r = f.reloaded(&r);
    assert!(r.drop_only.is_empty(), "{:?}", r.drop_only);
    // `exact` now for the project: the stricter wins.
    let e = block_on(f.syncer.publish_held(
        CHANGE,
        PatchSetNumber(1),
        &[vote(&r, "Code-Review")],
        LocalApproval::Exact,
    ))
    .unwrap_err();
    assert!(matches!(&e, Error::DropOnly(_)), "{e}");
    f.posts_apply(json!({"Code-Review": 2}), 1);
    block_on(f.syncer.publish_held(
        CHANGE,
        PatchSetNumber(1),
        &[vote(&r, "Code-Review")],
        LocalApproval::Follow,
    ))
    .unwrap();
    assert_eq!(f.http.posts()[0]["labels"], json!({"Code-Review": 2}));
}

#[test]
fn a_publish_that_fails_keeps_where_its_votes_were_given() {
    let f = fx();
    let (_, v) = f.local(CID, Some("main"), None, &["l1"]);
    let r = f.review_with(
        &v[0],
        &[("Code-Review", 2)],
        "",
        LocalApproval::Follow,
        ts("2023-06-01T00:00:00Z"),
    );
    f.query_finds(&[(7, "main", "MERGED")]);
    f.gerrit("MERGED", &["s9"], json!([]));
    f.run_with(&follow());
    let r = f.reloaded(&r);
    f.http
        .queue("POST /changes/7/revisions/", 403, "not permitted");
    let out = block_on(f.syncer.publish_held(
        CHANGE,
        PatchSetNumber(1),
        &[vote(&r, "Code-Review")],
        LocalApproval::Follow,
    ))
    .unwrap();
    assert!(matches!(out.outcome, PostOutcome::Refused { .. }));
    let failed = f.syncer.held_items(CHANGE).unwrap().reviews.remove(0);
    assert_eq!(
        (
            failed.given_on_sha.clone(),
            failed.voted_at,
            failed.strategy
        ),
        (r.given_on_sha.clone(), r.voted_at, r.strategy)
    );
    // Still publishable under follow, still not under exact.
    assert!(matches!(
        block_on(f.syncer.publish_held(
            CHANGE,
            PatchSetNumber(1),
            &[vote(&failed, "Code-Review")],
            LocalApproval::Exact,
        )),
        Err(Error::DropOnly(_))
    ));
}

// ---- account switches ----

/// Signs `account` in (as the cache and the server know it).
fn sign_in(f: &Fx, account: u64, name: &str) {
    f.syncer
        .store()
        .set_account(f.syncer.server, AccountId(account), Some(name), None)
        .unwrap();
    f.http.routes.route_json(
        "/accounts/self",
        &json!({"_account_id": account, "username": name}).to_string(),
    );
}

#[test]
fn a_comment_only_post_that_arrived_before_a_switch_is_not_posted_again() {
    let f = fx();
    sign_in(&f, 2000, "other");
    let (row, v) = f.local(CID, Some("main"), Some(CHANGE), &["s1"]);
    let d = f.draft(&row, &v[0], "a comment");
    f.gerrit("NEW", &["s1"], json!([]));
    // The other account's run posts; the answer is lost.
    f.http
        .queue("POST /changes/7/revisions/", 0, "connection reset");
    f.run();
    assert_eq!(f.http.posts().len(), 1);
    let tag = first_tag(&f);
    // It did arrive, by the other account.
    f.gerrit(
        "NEW",
        &["s1"],
        json!([{"id": "m1", "author": {"_account_id": 2000}, "tag": tag,
                "message": "Patch Set 1:\n\n(1 comment)",
                "date": "2024-01-03 00:00:00.000000000", "_revision_number": 1}]),
    );
    f.http.routes.route_json(
        "/changes/7/comments",
        &json!({"src/a.rs": [{"id": "c1", "line": 7, "message": "a comment",
                 "patch_set": 1, "tag": tag, "author": {"_account_id": 2000},
                 "updated": "2024-01-03 00:00:00.000000000"}]})
        .to_string(),
    );
    sign_in(&f, ME, "me");
    let report = f.run();
    assert!(report.posts.iter().any(|p| p.recovered), "{report:?}");
    assert_eq!(f.http.posts().len(), 1, "not posted again");
    assert_eq!(f.load_draft(&d), None);
    assert!(
        f.syncer
            .store()
            .unfinished_local_posts(f.syncer.server)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_held_publish_interrupted_across_a_switch_is_settled_not_offered_again() {
    let f = fx();
    let (_, r, _) = held_closed(&f);
    sign_in(&f, 2000, "other");
    f.http
        .queue("POST /changes/7/revisions/", 0, "connection reset");
    assert!(publish(&f, 1, &[HeldItem::Message(r.id)]).is_err());
    let tag = first_tag(&f);
    // The new review is the publisher's.
    let sending = f
        .syncer
        .pending_reviews(CHANGE)
        .unwrap()
        .into_iter()
        .find(|x| x.state == PendingReviewState::Sending)
        .unwrap();
    assert_eq!(sending.author, Some(AccountId(2000)));
    f.gerrit(
        "MERGED",
        &["s1"],
        json!([{"id": "m1", "author": {"_account_id": 2000}, "tag": tag,
                "message": "Patch Set 1:\n\n(1 comment)",
                "date": "2024-01-03 00:00:00.000000000", "_revision_number": 1}]),
    );
    sign_in(&f, ME, "me");
    f.run();
    assert_eq!(f.http.posts().len(), 1, "not posted again");
    assert_eq!(
        f.reloaded(&sending).state,
        PendingReviewState::Sent,
        "settled as sent"
    );
    // Not offered again: only the vote of the original review is held.
    let held = f.syncer.held_items(CHANGE).unwrap();
    assert_eq!(held.reviews.len(), 1);
    assert_eq!(held.reviews[0].id, r.id);
    assert_eq!(held.reviews[0].message, None);
}
