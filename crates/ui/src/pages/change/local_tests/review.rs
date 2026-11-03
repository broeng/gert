//! Reviewing local versions: drafts that wait for their version, pending
//! reviews from the reply dialog and quick votes, and the page's list of
//! pending reviews. The fake server panics on any synthetic number, and
//! the tests check that local work sends nothing at all.

use model::{Entry, PATCHSET_LEVEL_PATH};

use super::*;

const SAVE: &str = if cfg!(target_os = "macos") {
    "cmd-enter"
} else {
    "ctrl-enter"
};

impl Fixture {
    fn type_text(&self, cx: &mut TestAppContext, text: &str) {
        cx.update_window(self.window, |_, window, cx| window.input(text, cx))
            .expect("input");
        cx.run_until_parked();
    }

    /// Presses the keys of a chord (`"l e"`) one after the other.
    pub(super) fn chord(&self, cx: &mut TestAppContext, keys: &str) {
        for k in keys.split(' ') {
            self.press(cx, k);
        }
    }

    /// Every draft of the page's change in the store (sealed ones too).
    fn stored_drafts(&self, number: ChangeNumber) -> Vec<model::Draft> {
        self.store
            .lock()
            .expect("store")
            .load_drafts(SERVER, number)
            .expect("drafts")
    }
}

/// A local change in the checking state, its page open, signed in.
fn local_change(cx: &mut TestAppContext, ck: &Checkout) -> (Fixture, ChangeNumber) {
    let store = store_with(
        &[row(
            "I1",
            None,
            LocalChangeState::Checking,
            &[&ck.local],
            &ck.base,
        )],
        |_| {},
    );
    let number = store
        .local_changes_by_change_id(SERVER, "I1")
        .expect("rows")[0]
        .number;
    (page(cx, store, number, ck, |_| {}), number)
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_draft_on_a_local_change_is_saved_and_never_uploaded(cx: &mut TestAppContext) {
    let ck = checkout();
    let (f, number) = local_change(cx, &ck);
    let version = f.local_patch_set(cx, 1);
    // `c`: a patch-set level comment on the current version, local 1.
    f.press(cx, "c");
    f.type_text(cx, "worth a second look");
    f.press(cx, SAVE);
    let drafts = f.stored_drafts(number);
    assert_eq!(drafts.len(), 1);
    let d = &drafts[0];
    assert_eq!(
        (d.path.as_str(), d.patch_set, d.revision.as_str()),
        (PATCHSET_LEVEL_PATH, version, ck.local.as_str())
    );
    assert!(d.is_local() && !d.is_ordinary());
    // It is in the threads, as a draft that waits for its version.
    cx.update(|cx| {
        let threads = f.page.read(cx).threads_for_test();
        let shown: Vec<_> = threads
            .iter()
            .flat_map(|t| &t.entries)
            .filter_map(|e| match e {
                Entry::Draft(d) => crate::comments::local_note(d),
                Entry::Published(_) => None,
            })
            .collect();
        assert_eq!(shown, [crate::comments::LOCAL_DRAFT]);
    });
    // Editing it is local too, and so is the retry of everything pending.
    let id = d.local_id;
    f.page.update(cx, |p, cx| {
        p.outbox.update(cx, |o, cx| {
            o.save(
                sync::DraftSave::Update {
                    id,
                    message: "worth another look".into(),
                    unresolved: true,
                },
                cx,
            );
            o.retry_everything(cx);
        })
    });
    cx.run_until_parked();
    assert_eq!(f.stored_drafts(number)[0].message, "worth another look");
    // The title bar does not count it as waiting for the upload.
    cx.update(|cx| assert!(f.page.read(cx).outbox.read(cx).counts().is_empty()));
    assert!(f.http.requested().is_empty(), "{:?}", f.http.requested());
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_reply_to_a_gerrit_thread_while_a_local_version_is_shown_uploads_normally(
    cx: &mut TestAppContext,
) {
    let ck = checkout();
    let store = store_with(
        &[row(
            "I7",
            Some(ChangeNumber(7)),
            LocalChangeState::Local,
            &[&ck.local],
            &ck.base,
        )],
        seed_gerrit_change,
    );
    let f = page(cx, store, ChangeNumber(7), &ck, |http| {
        http.route_json(
            "/changes/7/comments",
            r#"{"/PATCHSET_LEVEL": [{"id":"p1","patch_set":3,"message":"overall","updated":"2024-06-15 10:02:00.000000000","unresolved":true}]}"#,
        );
        http.route(
            "/revisions/sha3/drafts",
            201,
            ")]}'\n{\"id\":\"srv1\",\"patch_set\":3,\"path\":\"/PATCHSET_LEVEL\",\"in_reply_to\":\"p1\",\"updated\":\"2024-06-15 10:05:00.000000000\",\"message\":\"Done\"}",
        );
    });
    let local = f.local_patch_set(cx, 1);
    f.pick(cx, |s, d| s.set_target(d, local));
    // The thread is on PS 3; "Done" on its card answers it there (O2).
    f.press(cx, "shift-n");
    f.press(cx, "x");
    f.press(cx, "d");
    let drafts = f.stored_drafts(ChangeNumber(7));
    assert_eq!(drafts.len(), 1, "{drafts:?}");
    let d = &drafts[0];
    assert_eq!(
        (d.patch_set, d.revision.as_str(), d.in_reply_to.as_deref()),
        (PatchSetNumber(3), "sha3", Some("p1"))
    );
    assert!(d.is_ordinary());
    assert_eq!(d.server_id.as_deref(), Some("srv1"));
    assert!(
        f.http
            .requested()
            .iter()
            .any(|r| r.contains("/changes/7/revisions/sha3/drafts")),
        "{:?}",
        f.http.requested()
    );
    f.assert_nothing_local_sent(&[&ck.local]);
}

impl Fixture {
    fn reviews(&self, change: ChangeNumber) -> Vec<model::PendingReview> {
        self.store
            .lock()
            .expect("store")
            .pending_reviews(SERVER, change)
            .expect("reviews")
    }
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_quick_vote_on_a_local_version_goes_into_its_pending_review(cx: &mut TestAppContext) {
    let ck = checkout();
    let (f, number) = local_change(cx, &ck);
    let version = f.local_patch_set(cx, 1);
    let buttons =
        |cx: &mut TestAppContext| cx.update(|cx| f.page.read(cx).quick_buttons_for_test(cx));
    assert_eq!(buttons(cx)[0].text(), "Code-Review +2");
    assert!(!buttons(cx)[0].applied);
    f.press(cx, "shift-v");
    assert_eq!(
        f.hint(cx).as_deref(),
        Some("Saved Code-Review +2 locally; sent when local 1 is on Gerrit")
    );
    let reviews = f.reviews(number);
    assert_eq!(reviews.len(), 1);
    let r = &reviews[0];
    assert_eq!(
        (r.patch_set, &r.labels, r.message.as_deref()),
        (version, &[("Code-Review".to_owned(), 2)].into(), None)
    );
    // The project's mode, from its checkout entry (the default).
    assert_eq!(r.strategy, model::LocalApproval::Exact);
    // The vote is a box where Gerrit's votes are, with a local pill.
    let mut visual = gpui_kit::VisualTestContext::from_window(f.window, cx);
    visual.run_until_parked();
    assert!(visual.debug_bounds("local-vote-Code-Review").is_some());
    // The button knows the vote now; pressing again changes nothing.
    assert!(buttons(cx)[0].applied);
    f.press(cx, "shift-v");
    assert_eq!(f.reviews(number).len(), 1);

    // After a reply with a message, a vote joins that review.
    f.store
        .lock()
        .expect("store")
        .cancel_pending_review(SERVER, r.id)
        .expect("cancel");
    let replied = f
        .store
        .lock()
        .expect("store")
        .create_pending_review(
            SERVER,
            &store::NewPendingReview {
                patch_set: version,
                labels: [("Code-Review".to_owned(), 1)].into(),
                strategy: model::LocalApproval::Follow,
                message: "Nearly".into(),
                unresolved: true,
                created: Timestamp::now(),
                offline: false,
                replaces_draft: None,
            },
        )
        .expect("review");
    f.page.update(cx, |p, cx| {
        p.outbox.update(cx, |o, cx| o.published(number, cx))
    });
    cx.run_until_parked();
    assert!(!buttons(cx)[0].applied, "+1 is the vote now");
    f.press(cx, "shift-v");
    let reviews = f.reviews(number);
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].id, replied.id);
    assert_eq!(reviews[0].labels, [("Code-Review".to_owned(), 2)].into());
    assert_eq!(
        (reviews[0].message.as_deref(), reviews[0].unresolved),
        (Some("Nearly"), true)
    );

    // The box's trash button takes the vote back; the message stays.
    f.page
        .update(cx, |p, cx| p.withdraw_local_vote("Code-Review".into(), cx));
    cx.run_until_parked();
    assert_eq!(
        f.hint(cx).as_deref(),
        Some("Removed your local Code-Review vote")
    );
    let reviews = f.reviews(number);
    assert_eq!(reviews.len(), 1);
    assert!(reviews[0].labels.is_empty());
    assert_eq!(reviews[0].message.as_deref(), Some("Nearly"));
    assert!(!buttons(cx)[0].applied);
    let mut visual = gpui_kit::VisualTestContext::from_window(f.window, cx);
    visual.run_until_parked();
    assert!(visual.debug_bounds("local-vote-Code-Review").is_none());
    assert!(f.http.requested().is_empty(), "{:?}", f.http.requested());
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_quick_vote_on_a_local_patch_set_of_a_gerrit_change_stays_local(cx: &mut TestAppContext) {
    let ck = checkout();
    let store = store_with(
        &[row(
            "I7",
            Some(ChangeNumber(7)),
            LocalChangeState::Local,
            &[&ck.local],
            &ck.base,
        )],
        seed_gerrit_change,
    );
    let project = ProjectSettings {
        local_approval: model::LocalApproval::Follow,
        ..ck.project()
    };
    let f = page_in(cx, store, ChangeNumber(7), project, |_| {});
    let local = f.local_patch_set(cx, 1);
    f.pick(cx, |s, d| s.set_target(d, local));
    // Change 7 lets the user give Code-Review +1.
    f.press(cx, "shift-v");
    let reviews = f.reviews(ChangeNumber(7));
    assert_eq!(reviews.len(), 1, "{:?}", f.hint(cx));
    assert_eq!(reviews[0].labels, [("Code-Review".to_owned(), 1)].into());
    assert_eq!(reviews[0].patch_set, local);
    assert_eq!(reviews[0].strategy, model::LocalApproval::Follow);
    // A box with a local pill next to Gerrit's votes, while the local
    // version is shown.
    let local_box = |cx: &mut TestAppContext| {
        let mut visual = gpui_kit::VisualTestContext::from_window(f.window, cx);
        visual.run_until_parked();
        visual.debug_bounds("local-vote-Code-Review").is_some()
    };
    assert!(local_box(cx));
    // PS 3 shown again: a quick vote goes to Gerrit as always.
    f.pick(cx, |s, d| s.set_target(d, PatchSetNumber(3)));
    assert!(!local_box(cx), "the local vote is not on PS 3");
    cx.update(|cx| assert!(!f.page.read(cx).quick_buttons_for_test(cx)[0].applied));
    assert!(
        !f.http.requested().iter().any(|r| r.contains("/review")),
        "{:?}",
        f.http.requested()
    );
    f.assert_nothing_local_sent(&[&ck.local]);
}

/// Local change I1 amended once, on a page with the project in mode
/// `approval`, and a way to save a pending review (Follow) on a version.
fn amended(
    cx: &mut TestAppContext,
    ck: &Checkout,
    approval: model::LocalApproval,
) -> (Fixture, ChangeNumber, String) {
    ck.repo
        .git(&["commit", "-q", "--amend", "-m", "Amended\n\nChange-Id: I1"]);
    let amended = ck.repo.git(&["rev-parse", "HEAD"]);
    let store = store_with(
        &[row(
            "I1",
            None,
            LocalChangeState::Local,
            &[&ck.local, &amended],
            &ck.base,
        )],
        |_| {},
    );
    let number = store
        .local_changes_by_change_id(SERVER, "I1")
        .expect("rows")[0]
        .number;
    let project = ProjectSettings {
        local_approval: approval,
        ..ck.project()
    };
    (page_in(cx, store, number, project, |_| {}), number, amended)
}

impl Fixture {
    /// Saves a pending review (mode `follow`) with `labels` on `version`
    /// and lets the page read it.
    fn save_review(
        &self,
        cx: &mut TestAppContext,
        number: ChangeNumber,
        version: PatchSetNumber,
        labels: &[(&str, i32)],
    ) {
        self.store
            .lock()
            .expect("store")
            .create_pending_review(
                SERVER,
                &store::NewPendingReview {
                    patch_set: version,
                    labels: labels.iter().map(|(l, v)| ((*l).to_owned(), *v)).collect(),
                    strategy: model::LocalApproval::Follow,
                    message: String::new(),
                    unresolved: false,
                    created: Timestamp::now(),
                    offline: false,
                    replaces_draft: None,
                },
            )
            .expect("review");
        self.page.update(cx, |p, cx| {
            p.outbox.update(cx, |o, cx| o.published(number, cx))
        });
        cx.run_until_parked();
    }
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_vote_on_an_older_local_version_carries_over_to_the_newer_one(cx: &mut TestAppContext) {
    let ck = checkout();
    let (f, number, amended) = amended(cx, &ck, model::LocalApproval::Exact);
    let (v1, v2) = (f.local_patch_set(cx, 1), f.local_patch_set(cx, 2));
    f.save_review(cx, number, v1, &[("Code-Review", 2), ("Verified", 1)]);
    let boxes = |cx: &mut TestAppContext| cx.update(|cx| f.page.read(cx).local_votes_for_test());
    let vote = |label: &str, value, version| (label.to_owned(), value, version);
    // Local 2, the newest, is shown: local 1's votes carry over.
    assert_eq!(
        boxes(cx),
        [vote("Code-Review", 2, v1), vote("Verified", 1, v1)]
    );
    // The project holds votes of an amended commit (`exact`, stricter
    // than the review's `follow`): the +2 button is still to press.
    let buttons =
        |cx: &mut TestAppContext| cx.update(|cx| f.page.read(cx).quick_buttons_for_test(cx));
    assert_eq!(buttons(cx)[0].text(), "Code-Review +2");
    assert!(!buttons(cx)[0].applied);
    // A vote on local 2 wins over the carried one, and does not go back
    // to local 1.
    f.save_review(cx, number, v2, &[("Code-Review", 1)]);
    assert_eq!(
        boxes(cx),
        [vote("Code-Review", 1, v2), vote("Verified", 1, v1)]
    );
    f.pick(cx, |s, d| s.set_target(d, v1));
    assert_eq!(
        boxes(cx),
        [vote("Code-Review", 2, v1), vote("Verified", 1, v1)]
    );

    // The trash on local 2 takes the vote back on both versions.
    f.pick(cx, |s, d| s.set_target(d, v2));
    f.page
        .update(cx, |p, cx| p.withdraw_local_vote("Code-Review".into(), cx));
    cx.run_until_parked();
    assert_eq!(boxes(cx), [vote("Verified", 1, v1)]);
    let left: Vec<_> = f.reviews(number).into_iter().map(|r| r.labels).collect();
    assert_eq!(left, [[("Verified".to_owned(), 1)].into()]);
    f.assert_nothing_local_sent(&[&ck.local, &amended]);
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_carried_vote_that_follows_the_amended_commit_counts_as_given(cx: &mut TestAppContext) {
    let ck = checkout();
    let (f, number, amended) = amended(cx, &ck, model::LocalApproval::Follow);
    let v1 = f.local_patch_set(cx, 1);
    f.save_review(cx, number, v1, &[("Code-Review", 2)]);
    let buttons =
        |cx: &mut TestAppContext| cx.update(|cx| f.page.read(cx).quick_buttons_for_test(cx));
    assert_eq!(buttons(cx)[0].text(), "Code-Review +2");
    assert!(buttons(cx)[0].applied);
    f.press(cx, "shift-v");
    assert_eq!(
        f.hint(cx).as_deref(),
        Some("You already voted Code-Review +2")
    );
    assert_eq!(f.reviews(number).len(), 1, "no new review");
    f.assert_nothing_local_sent(&[&ck.local, &amended]);
}

/// Pending reviews on `version`: one per entry of `states`, each with a
/// message naming its state, the held and failed ones with their reason.
fn seed_reviews(store: &mut Store, version: PatchSetNumber, states: &[model::PendingReviewState]) {
    use model::PendingReviewState as S;
    for (i, state) in states.iter().enumerate() {
        let r = store
            .create_pending_review(
                SERVER,
                &store::NewPendingReview {
                    patch_set: version,
                    labels: [("Code-Review".to_owned(), 1 + i as i32 % 2)].into(),
                    strategy: model::LocalApproval::Exact,
                    message: format!("review {i}, {}", state.as_str()),
                    unresolved: false,
                    created: at(50 + i as u32),
                    offline: false,
                    replaces_draft: None,
                },
            )
            .expect("review");
        let note = match state {
            S::Held => Some("amended before upload"),
            S::Failed => Some("403 not allowed"),
            _ => None,
        };
        if *state != S::Pending {
            store
                .set_pending_review_state(SERVER, r.id, &[S::Pending], *state, note)
                .expect("state");
        }
    }
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn the_local_reviews_card_lists_waiting_reviews_and_its_keys_edit_and_cancel(
    cx: &mut TestAppContext,
) {
    use model::PendingReviewState as S;
    let ck = checkout();
    let mut store = store_with(
        &[row(
            "I1",
            None,
            LocalChangeState::Local,
            &[&ck.local],
            &ck.base,
        )],
        |_| {},
    );
    let row = store
        .local_changes_by_change_id(SERVER, "I1")
        .expect("rows")
        .remove(0);
    let version = store
        .local_patch_sets(SERVER, row.id)
        .expect("versions")
        .remove(0)
        .number;
    seed_reviews(&mut store, version, &[S::Pending, S::Held, S::Failed]);
    let number = row.number;
    let f = page(cx, store, number, &ck, |_| {});
    // The waiting review here; the held and failed ones in the held card.
    cx.update(|cx| {
        let p = f.page.read(cx);
        assert_eq!(
            &p.blocks_for_test()[1..3],
            [Block::PendingReviews, Block::Held]
        );
        assert_eq!(p.listed_reviews_for_test().len(), 1);
    });
    let mut visual = gpui_kit::VisualTestContext::from_window(f.window, cx);
    visual.run_until_parked();
    assert!(visual.debug_bounds("pending-review-0: pending").is_some());
    assert!(visual.debug_bounds("pending-review-1: held").is_none());

    // Edit opens the dialog on it, cancel unseals its message.
    f.chord(cx, "l e");
    let first = f.reviews(number)[0].clone();
    assert_eq!(
        f.events.lock().expect("log").last().cloned(),
        Some(format!("reply edit {}", first.id.0))
    );
    f.chord(cx, "l c");
    assert_eq!(
        f.hint(cx).as_deref(),
        Some("Review cancelled; its message is kept as a draft")
    );
    assert_eq!(f.reviews(number).len(), 2);
    let drafts = f.stored_drafts(number);
    assert_eq!(drafts.len(), 3, "two sealed, one unsealed");
    let kept = drafts
        .iter()
        .find(|d| Some(d.local_id) == first.message_draft)
        .expect("the message");
    assert!(!kept.is_sealed() && kept.is_local());
    // Nothing waits: no card, and the keys say so; the held card stays.
    cx.update(|cx| {
        let p = f.page.read(cx);
        assert_eq!(p.listed_reviews_for_test().len(), 0);
        assert!(!p.blocks_for_test().contains(&Block::PendingReviews));
        assert!(p.blocks_for_test().contains(&Block::Held));
    });
    f.chord(cx, "l e");
    assert_eq!(f.hint(cx).as_deref(), Some("No local reviews waiting here"));
    assert!(f.http.requested().is_empty(), "{:?}", f.http.requested());
}

#[gpui_kit::test]
fn a_gerrit_change_lists_the_reviews_of_its_local_versions(cx: &mut TestAppContext) {
    let ck = checkout();
    let mut store = store_with(
        &[row(
            "I7",
            Some(ChangeNumber(7)),
            LocalChangeState::Local,
            &[&ck.local],
            &ck.base,
        )],
        seed_gerrit_change,
    );
    let row = store
        .local_changes_by_change_id(SERVER, "I7")
        .expect("rows")
        .remove(0);
    let version = store
        .local_patch_sets(SERVER, row.id)
        .expect("versions")
        .remove(0)
        .number;
    seed_reviews(&mut store, version, &[model::PendingReviewState::Pending]);
    let f = page(cx, store, ChangeNumber(7), &ck, |_| {});
    cx.update(|cx| {
        let p = f.page.read(cx);
        assert!(p.blocks_for_test().contains(&Block::PendingReviews));
        // It is on the local version, which the row names.
        let reviews = p.listed_reviews_for_test();
        assert_eq!(reviews.len(), 1);
        let label = crate::change_detail::patch_set_label(
            &p.detail().expect("detail").revisions,
            reviews[0].patch_set,
        );
        assert_eq!(label, "local 1");
    });
    // Edit opens the dialog on it even while PS 3 is shown.
    f.chord(cx, "l e");
    assert!(
        f.events
            .lock()
            .expect("log")
            .iter()
            .any(|e| e.starts_with("reply edit"))
    );
    f.assert_nothing_local_sent(&[&ck.local]);
}

/// Change 7 as [`seed_gerrit_change`] has it, with the user's Code-Review
/// +1 (the highest they may give) on PS 3.
fn seed_with_my_vote(store: &mut Store) {
    seed_gerrit_change(store);
    let mut d = store
        .load_change_detail(SERVER, ChangeNumber(7))
        .expect("store")
        .expect("detail")
        .detail;
    let mut me = crate::testing::account(9, "u");
    me.username = Some("u".into());
    d.votes = [(
        "Code-Review".to_owned(),
        vec![model::LabelVote {
            account: me,
            value: 1,
            date: None,
        }],
    )]
    .into();
    store
        .save_change_detail(SERVER, &d, Timestamp::now())
        .expect("detail");
}

#[gpui_kit::test]
fn a_vote_on_gerrit_does_not_count_as_the_vote_on_a_local_version(cx: &mut TestAppContext) {
    let ck = checkout();
    let store = store_with(
        &[row(
            "I7",
            Some(ChangeNumber(7)),
            LocalChangeState::Local,
            &[&ck.local],
            &ck.base,
        )],
        seed_with_my_vote,
    );
    let f = page(cx, store, ChangeNumber(7), &ck, |_| {});
    let buttons =
        |cx: &mut TestAppContext| cx.update(|cx| f.page.read(cx).quick_buttons_for_test(cx));
    // On PS 3 the +1 is given already.
    assert!(buttons(cx)[0].applied);
    // On the local version it is not: the vote on Gerrit is on PS 3.
    let local = f.local_patch_set(cx, 1);
    f.pick(cx, |s, d| s.set_target(d, local));
    assert!(!buttons(cx)[0].applied);
    f.press(cx, "shift-v");
    let reviews = f.reviews(ChangeNumber(7));
    assert_eq!(reviews.len(), 1, "{:?}", f.hint(cx));
    assert_eq!(reviews[0].labels, [("Code-Review".to_owned(), 1)].into());
    assert!(buttons(cx)[0].applied);
    f.assert_nothing_local_sent(&[&ck.local]);
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn c_on_a_local_version_of_a_gerrit_change_writes_a_local_draft(cx: &mut TestAppContext) {
    let ck = checkout();
    let store = store_with(
        &[row(
            "I7",
            Some(ChangeNumber(7)),
            LocalChangeState::Local,
            &[&ck.local],
            &ck.base,
        )],
        seed_gerrit_change,
    );
    let f = page(cx, store, ChangeNumber(7), &ck, |_| {});
    let local = f.local_patch_set(cx, 1);
    f.pick(cx, |s, d| s.set_target(d, local));
    f.press(cx, "c");
    f.type_text(cx, "about local 1");
    f.press(cx, SAVE);
    let drafts = f.stored_drafts(ChangeNumber(7));
    assert_eq!(drafts.len(), 1);
    assert_eq!(
        (drafts[0].patch_set, drafts[0].path.as_str()),
        (local, PATCHSET_LEVEL_PATH)
    );
    assert!(drafts[0].is_local());
    // Not a Gerrit draft: nothing is uploaded.
    assert!(
        !f.http.requested().iter().any(|r| r.contains("/revisions/")),
        "{:?}",
        f.http.requested()
    );
    f.assert_nothing_local_sent(&[&ck.local]);
}
