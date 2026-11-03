use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use gerrit_api::{Auth, GerritClient, fake::FakeHttpClient};
use model::{
    DraftInput, LocalApproval, Location, PATCHSET_LEVEL_PATH, PatchSetRef, PendingReviewState, Side,
};
use store::Store;

use super::*;
use crate::{DraftSave, syncer::tests::add_local_version};

fn syncer() -> (Syncer, FakeHttpClient) {
    let http = FakeHttpClient::new();
    let auth = Auth::Basic {
        username: "u".into(),
        password: "p".into(),
    };
    let client = GerritClient::new(Arc::new(http.clone()), "https://g.example", auth).unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let server = store.server_id("https://g.example").unwrap();
    (
        Syncer::new(client, Arc::new(Mutex::new(store)), server),
        http,
    )
}

fn review(patch_set: model::PatchSetNumber, message: &str) -> NewPendingReview {
    NewPendingReview {
        patch_set,
        labels: BTreeMap::from([("Code-Review".to_owned(), 2)]),
        strategy: LocalApproval::ExactOrManual,
        message: message.into(),
        unresolved: false,
        created: Timestamp::now(),
        offline: false,
        replaces_draft: None,
    }
}

#[test]
fn pending_reviews_are_written_and_listed_without_the_network() {
    let (syncer, http) = syncer();
    let (change, version) = add_local_version(&syncer, None, "localsha");
    let first = syncer
        .create_pending_review(&review(version.number, "first"))
        .unwrap();
    let second = syncer
        .create_pending_review(&review(version.number, ""))
        .unwrap();
    let listed = syncer.pending_reviews(change.number).unwrap();
    assert_eq!(listed, [first.clone(), second.clone()]);

    let edit = PendingReviewEdit {
        labels: BTreeMap::from([("Code-Review".to_owned(), 1)]),
        strategy: LocalApproval::Follow,
        message: "edited".into(),
        unresolved: true,
        offline: false,
    };
    let edited = syncer
        .update_pending_review(first.id, &edit)
        .unwrap()
        .unwrap();
    assert_eq!(
        (edited.message.as_deref(), edited.unresolved),
        (Some("edited"), true)
    );
    let voted = syncer
        .set_pending_review_label(first.id, "Verified", 1, LocalApproval::Exact)
        .unwrap()
        .unwrap();
    assert_eq!(voted.labels.get("Verified"), Some(&1));
    assert_eq!(voted.message.as_deref(), Some("edited"));
    assert!(syncer.cancel_pending_review(second.id).unwrap());
    // Only held and failed reviews can be dropped.
    assert!(!syncer.drop_pending_review(first.id).unwrap());
    syncer
        .store()
        .set_pending_review_state(
            syncer.server,
            first.id,
            &[PendingReviewState::Pending],
            PendingReviewState::Held,
            Some("amended"),
        )
        .unwrap();
    assert!(syncer.drop_pending_review(first.id).unwrap());
    assert!(syncer.pending_reviews(change.number).unwrap().is_empty());
    assert!(http.requests().is_empty());
}

#[test]
fn the_comments_view_leaves_sealed_messages_out() {
    let (syncer, _) = syncer();
    let (change, version) = add_local_version(&syncer, None, "localsha");
    let on = PatchSetRef {
        number: version.number,
        sha: version.sha.clone(),
    };
    let comment = syncer
        .save_draft_local(DraftSave::Create {
            change: change.number,
            patch_set: on,
            input: DraftInput {
                path: PATCHSET_LEVEL_PATH.into(),
                side: Side::Revision,
                parent: None,
                location: Location::File,
                in_reply_to: None,
                message: "an ordinary local draft".into(),
                unresolved: false,
            },
        })
        .unwrap()
        .unwrap();
    let r = syncer
        .create_pending_review(&review(version.number, "sealed"))
        .unwrap();
    let view = syncer.cached_comments(change.number).unwrap();
    assert_eq!(
        view.drafts.iter().map(|d| d.local_id).collect::<Vec<_>>(),
        [comment.local_id]
    );
    // Cancelling unseals it: an ordinary local draft, in the view again.
    syncer.cancel_pending_review(r.id).unwrap();
    let view = syncer.cached_comments(change.number).unwrap();
    assert!(
        view.drafts
            .iter()
            .any(|d| Some(d.local_id) == r.message_draft)
    );
}
