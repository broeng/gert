//! The held card on a Gerrit change: what consolidation held from a local
//! review, each item with its reason, published (by the user) or dropped.

use model::{DraftInput, Location, PATCHSET_LEVEL_PATH, PendingReviewState, Side};
use serde_json::{Value, json};
use store::{ConsolidationRewrite, ReviewStateChange};

use super::*;

/// Change 7 (PS 3 current; merged if `merged`) with a local version whose
/// review (Code-Review +2, drop-only, Verified +1, message "LGTM") and
/// comment on `src/a.rs` line 7 are held. Its page, signed in; posts are
/// answered with Verified +1 applied.
fn held_page(cx: &mut TestAppContext, merged: bool) -> (Fixture, Checkout) {
    held_page_with(cx, merged, true)
}

/// Like [`held_page`]; with `recorded` false nothing marked the approval
/// drop-only when it was held (as for data held before that was
/// recorded): the card judges it from the change.
fn held_page_with(cx: &mut TestAppContext, merged: bool, recorded: bool) -> (Fixture, Checkout) {
    let ck = checkout();
    let mut store = store_with(
        &[row(
            "I7",
            Some(ChangeNumber(7)),
            LocalChangeState::Local,
            &[&ck.local],
            &ck.base,
        )],
        |s| {
            seed_gerrit_change(s);
            if merged {
                let mut d = s
                    .load_change_detail(SERVER, ChangeNumber(7))
                    .expect("load")
                    .expect("cached")
                    .detail;
                d.summary.status = model::ChangeStatus::Merged;
                s.save_change_detail(SERVER, &d, Timestamp::now())
                    .expect("save");
            }
        },
    );
    let row = store
        .local_changes_by_change_id(SERVER, "I7")
        .expect("rows")
        .remove(0);
    let version = store
        .local_patch_sets(SERVER, row.id)
        .expect("versions")
        .remove(0);
    let d = store
        .create_draft(
            SERVER,
            ChangeNumber(7),
            &PatchSetRef {
                number: version.number,
                sha: version.sha.clone(),
            },
            &DraftInput {
                path: "src/a.rs".into(),
                side: Side::Revision,
                parent: None,
                location: Location::Line(7),
                in_reply_to: None,
                message: "a comment".into(),
                unresolved: true,
            },
            at(50),
        )
        .expect("draft");
    let r = store
        .create_pending_review(
            SERVER,
            &store::NewPendingReview {
                patch_set: version.number,
                labels: [("Code-Review".to_owned(), 2), ("Verified".to_owned(), 1)].into(),
                strategy: model::LocalApproval::Exact,
                message: "LGTM".into(),
                unresolved: false,
                created: at(51),
                replaces_draft: None,
            },
        )
        .expect("review");
    let mut held = ReviewStateChange::new(
        r.id,
        &[PendingReviewState::Pending],
        PendingReviewState::Held,
        Some("Your local version never reached Gerrit".into()),
    );
    if recorded {
        held.drop_only = ["Code-Review".to_owned()].into();
    }
    store
        .rewrite_local_change(
            SERVER,
            &ConsolidationRewrite {
                local_change: row.id,
                gerrit_number: ChangeNumber(7),
                moves: Vec::new(),
                keep: vec![(d.local_id, "Line 7 of src/a.rs changed in PS 3".into())],
                review_states: vec![held],
                state: None,
                else_state: None,
            },
        )
        .expect("rewrite")
        .expect("rewritten");
    let detail = if merged {
        change_7(3).replace("\"NEW\"", "\"MERGED\"")
    } else {
        change_7(3)
    };
    // The change's labels, so the page knows which vote is an approval.
    let mut detail: Value = serde_json::from_str(&detail).expect("json");
    detail["labels"] = json!({
        "Code-Review": {"values": {"-2": "", "-1": "", " 0": "", "+1": "", "+2": ""}},
        "Verified": {"values": {"-1": "", " 0": "", "+1": "", "+2": ""}},
    });
    let detail = detail.to_string();
    let f = page(cx, store, ChangeNumber(7), &ck, |http| {
        http.route_json("/changes/7?", &detail);
        http.route_json("/accounts/self", r#"{"_account_id": 1, "username": "u"}"#);
        http.route_json(
            "POST /changes/7/revisions/",
            &json!({"labels": {"Verified": 1}, "change_info": {"current_revision_number": 3}})
                .to_string(),
        );
    });
    (f, ck)
}

impl Fixture {
    /// The bodies of the review posts so far.
    fn review_posts(&self) -> Vec<(String, Value)> {
        self.http
            .sent()
            .iter()
            .filter(|r| r.method == "POST" && r.target.ends_with("/review"))
            .map(|r| {
                (
                    r.target.clone(),
                    serde_json::from_slice(&r.body).expect("json body"),
                )
            })
            .collect()
    }

    fn held_items(&self, cx: &mut TestAppContext) -> Vec<Option<sync::HeldItem>> {
        cx.update(|cx| {
            self.page
                .read(cx)
                .held_rows()
                .iter()
                .map(|r| r.item())
                .collect()
        })
    }

    /// Neither a synthetic number nor the local commit in any request,
    /// bodies included.
    fn assert_no_local_in_bodies(&self, ck: &Checkout) {
        self.assert_nothing_local_sent(&[&ck.local]);
        let change = ChangeNumber::LOCAL_BASE.to_string();
        let patch_set = PatchSetNumber::LOCAL_BASE.to_string();
        for r in self.http.sent() {
            let body = r.body_text();
            assert!(!body.contains(&ck.local), "{body}");
            assert!(!body.contains(&change[..6]), "{body}");
            assert!(!body.contains(&patch_set[..5]), "{body}");
        }
    }
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn held_items_show_their_reasons_and_a_drop_only_vote_only_drops(cx: &mut TestAppContext) {
    let (f, ck) = held_page(cx, false);
    cx.update(|cx| {
        assert!(f.page.read(cx).blocks_for_test().contains(&Block::Held));
    });
    assert_eq!(f.held_items(cx).len(), 5);
    let mut visual = gpui_kit::VisualTestContext::from_window(f.window, cx);
    visual.run_until_parked();
    for selector in [
        "held-review-0: held: Your local version never reached Gerrit",
        "held-row-1: vote Code-Review +2",
        "held-drop-only",
        "held-row-2: vote Verified +1",
        "held-row-3: message",
        "held-row-4: comment local 1, src/a.rs line 7",
    ] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector}");
    }
    // Open change: no post-submit note.
    assert!(visual.debug_bounds("held-closed-note").is_none());

    // The first item is the drop-only approval: `w s` explains and sends
    // nothing.
    f.chord(cx, "w s");
    let hint = f.hint(cx).expect("a hint");
    assert!(
        hint.contains("only be dropped") && hint.contains("exact commit they were given on"),
        "{hint}"
    );
    assert!(f.review_posts().is_empty());
    // Dropping asks once.
    f.chord(cx, "w d");
    assert!(f.hint(cx).is_some_and(|h| h.starts_with("Drop this vote?")));
    f.chord(cx, "w d");
    assert_eq!(f.hint(cx).as_deref(), Some("Dropped"));
    let rows = f.held_items(cx);
    assert_eq!(rows.len(), 4);
    assert!(!rows.iter().any(|i| matches!(
        i,
        Some(sync::HeldItem::Vote { label, .. }) if label == "Code-Review"
    )));
    assert!(f.review_posts().is_empty());
    f.assert_no_local_in_bodies(&ck);
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_held_vote_is_published_onto_the_current_patch_set(cx: &mut TestAppContext) {
    let (f, ck) = held_page(cx, false);
    // Code-Review (drop-only), then Verified.
    f.chord(cx, "w n");
    f.chord(cx, "w s");
    assert_eq!(
        f.hint(cx).as_deref(),
        Some("Publish this vote onto PS 3? Press w s (or the button) again")
    );
    assert!(f.review_posts().is_empty(), "asked first");
    f.chord(cx, "w s");
    assert_eq!(f.hint(cx).as_deref(), Some("Vote published onto PS 3"));
    let posts = f.review_posts();
    assert_eq!(posts.len(), 1, "{posts:?}");
    let (target, body) = &posts[0];
    assert!(
        target.contains("/changes/7/revisions/sha3/review"),
        "{target}"
    );
    assert_eq!(body["labels"], json!({"Verified": 1}));
    assert_eq!(body["drafts"], "KEEP");
    assert_eq!(body["ignore_automatic_attention_set_rules"], true);
    assert!(body.get("comments").is_none(), "{body}");
    assert!(
        body["tag"].as_str().is_some_and(|t| t.contains("-local-")),
        "{body}"
    );
    // Gone from the card; the approval stays.
    assert!(!f.held_items(cx).iter().any(|i| matches!(
        i,
        Some(sync::HeldItem::Vote { label, .. }) if label == "Verified"
    )));
    f.assert_no_local_in_bodies(&ck);
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_held_comment_is_posted_quoting_its_place_and_a_message_as_the_message(
    cx: &mut TestAppContext,
) {
    let (f, ck) = held_page(cx, false);
    // Back from the first item: the comment, the last one.
    f.chord(cx, "w p");
    f.chord(cx, "w y");
    assert_eq!(
        cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
        Some("a comment")
    );
    f.chord(cx, "w s");
    f.chord(cx, "w s");
    assert_eq!(
        f.hint(cx).as_deref(),
        Some("Posted as a patch-set comment on PS 3")
    );
    let posts = f.review_posts();
    assert_eq!(
        posts[0].1["comments"],
        json!({PATCHSET_LEVEL_PATH: [{
            "message": "On local 1, src/a.rs line 7: a comment",
            "unresolved": true
        }]})
    );
    assert!(posts[0].1.get("labels").is_none());
    // The message (now the last item).
    f.chord(cx, "w p");
    f.chord(cx, "w s");
    f.chord(cx, "w s");
    assert_eq!(f.hint(cx).as_deref(), Some("Message published on PS 3"));
    let posts = f.review_posts();
    assert_eq!(posts.len(), 2);
    assert_eq!(
        posts[1].1["comments"][PATCHSET_LEVEL_PATH],
        json!([{"message": "LGTM", "unresolved": false}])
    );
    // What is left: the two votes.
    let left = f.held_items(cx);
    assert_eq!(left.iter().flatten().count(), 2, "{left:?}");
    f.assert_no_local_in_bodies(&ck);
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_merged_change_says_published_votes_are_post_submit_votes(cx: &mut TestAppContext) {
    let (f, ck) = held_page(cx, true);
    let mut visual = gpui_kit::VisualTestContext::from_window(f.window, cx);
    visual.run_until_parked();
    assert!(visual.debug_bounds("held-closed-note").is_some());
    f.chord(cx, "w n");
    f.chord(cx, "w s");
    assert_eq!(
        f.hint(cx).as_deref(),
        Some("Publish this vote onto PS 3? Press w s (or the button) again")
    );
    assert!(f.review_posts().is_empty(), "asked first");
    f.chord(cx, "w s");
    assert_eq!(f.hint(cx).as_deref(), Some("Vote published onto PS 3"));
    let posts = f.review_posts();
    assert_eq!(posts.len(), 1);
    assert!(posts[0].0.contains("/revisions/sha3/review"));
    assert_eq!(posts[0].1["labels"], json!({"Verified": 1}));
    f.assert_no_local_in_bodies(&ck);
}

#[gpui_kit::test]
fn an_approval_given_on_another_commit_is_drop_only_whatever_was_recorded(cx: &mut TestAppContext) {
    let (f, ck) = held_page_with(cx, false, false);
    let mut visual = gpui_kit::VisualTestContext::from_window(f.window, cx);
    visual.run_until_parked();
    // The +2 was given on the local version, not on PS 3: drop only.
    assert!(visual.debug_bounds("held-drop-only").is_some());
    f.chord(cx, "w s");
    assert!(f.hint(cx).is_some_and(|h| h.contains("only be dropped")));
    assert!(f.review_posts().is_empty());
    f.assert_no_local_in_bodies(&ck);
}

#[gpui_kit::test]
fn a_publish_goes_on_when_the_page_is_left(cx: &mut TestAppContext) {
    let (f, _ck) = held_page(cx, false);
    // The outbox outlives the page (the shell keeps it).
    let outbox = cx.update(|cx| f.page.read(cx).outbox.clone());
    let ended = Arc::new(Mutex::new(Vec::new()));
    let seen = ended.clone();
    let _sub = cx.update(|cx| {
        cx.subscribe(&outbox, move |_, ev: &crate::outbox::OutboxEvent, _| {
            if let crate::outbox::OutboxEvent::HeldPublished { result, .. } = ev {
                seen.lock().expect("log").push(result.is_ok());
            }
        })
    });
    f.chord(cx, "w n");
    f.chord(cx, "w s");
    // The second press, and the page is gone before anything ran.
    for key in ["w", "s"] {
        cx.update_window(f.window, |_, window, cx| window.press(key, cx))
            .expect("press");
    }
    cx.update_window(f.window, |_, window, _| window.remove_window())
        .expect("close");
    let Fixture {
        page, http, store, ..
    } = f;
    drop(page);
    cx.run_until_parked();
    assert_eq!(*ended.lock().expect("log"), [true]);
    let posted = http
        .sent()
        .iter()
        .filter(|r| r.method == "POST" && r.target.ends_with("/review"))
        .count();
    assert_eq!(posted, 1);
    // The lease is free again: consolidation does not wait for it.
    let now = Timestamp::now();
    assert!(
        store
            .lock()
            .expect("store")
            .take_consolidation_lease(
                SERVER,
                "a run",
                now,
                now.checked_add(sync::CLAIM_STALE).expect("time")
            )
            .expect("lease")
    );
}
