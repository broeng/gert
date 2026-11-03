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
    assert!(f.http.requested().is_empty(), "{:?}", f.http.requested());
}

#[gpui_kit::test]
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
    // PS 3 shown again: a quick vote goes to Gerrit as always.
    f.pick(cx, |s, d| s.set_target(d, PatchSetNumber(3)));
    cx.update(|cx| assert!(!f.page.read(cx).quick_buttons_for_test(cx)[0].applied));
    assert!(
        !f.http.requested().iter().any(|r| r.contains("/review")),
        "{:?}",
        f.http.requested()
    );
    f.assert_nothing_local_sent(&[&ck.local]);
}
