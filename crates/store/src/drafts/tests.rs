use model::{Account, AccountId, CommentRange, FileEntry, FileStatus, FilesKey, PatchSetNumber};

use super::*;

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

const T0: &str = "2024-03-01T10:00:00Z";
const T1: &str = "2024-03-01T11:00:00Z";
const T2: &str = "2024-03-01T12:00:00Z";
const T3: &str = "2024-03-01T13:00:00Z";

fn setup() -> (Store, ServerId) {
    let mut s = Store::open_in_memory().unwrap();
    let id = s.server_id("https://g.example").unwrap();
    (s, id)
}

const CHANGE: ChangeNumber = ChangeNumber(5);

fn ps(n: u32) -> PatchSetRef {
    PatchSetRef {
        number: PatchSetNumber(n),
        sha: format!("sha{n}").as_str().into(),
    }
}

fn input(message: &str) -> DraftInput {
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

fn comment(id: &str, message: &str) -> Comment {
    Comment {
        id: id.into(),
        path: "src/a.rs".into(),
        patch_set: PatchSetNumber(2),
        side: Side::Revision,
        parent: None,
        location: Location::Line(7),
        in_reply_to: None,
        message: message.into(),
        author: Some(Account {
            id: AccountId(1),
            name: None,
            email: None,
            username: None,
            display_name: None,
        }),
        updated: ts(T1),
        unresolved: false,
        commit_id: None,
        tag: None,
        change_message_id: None,
        robot_id: None,
        context: vec![],
    }
}

fn sync_of(s: &Store, server: ServerId, id: LocalDraftId) -> DraftSync {
    s.load_draft(server, id).unwrap().unwrap().sync
}

#[test]
fn comments_round_trip_and_are_server_scoped() {
    let (mut s, a) = setup();
    let b = s.server_id("https://b.example").unwrap();
    assert_eq!(s.load_comments(a, CHANGE).unwrap(), None);
    let cs = vec![comment("c1", "hi"), comment("c2", "there")];
    s.save_comments(a, CHANGE, &cs, ts(T1)).unwrap();
    let got = s.load_comments(a, CHANGE).unwrap().unwrap();
    assert_eq!((got.comments.clone(), got.fetched_at), (cs, ts(T1)));
    assert_eq!(s.load_comments(b, CHANGE).unwrap(), None);
    // Replaced, not appended.
    s.save_comments(a, CHANGE, &[comment("c3", "x")], ts(T2))
        .unwrap();
    assert_eq!(
        s.load_comments(a, CHANGE).unwrap().unwrap().comments.len(),
        1
    );
}

#[test]
fn a_new_draft_is_pending_create_and_keeps_its_anchor() {
    let (mut s, id) = setup();
    let mut inp = input("first");
    inp.location = Location::Range(CommentRange {
        start_line: 3,
        start_character: 1,
        end_line: 5,
        end_character: 9,
    });
    inp.side = Side::Parent;
    inp.parent = Some(2);
    inp.in_reply_to = Some("c1".into());
    let d = s.create_draft(id, CHANGE, &ps(2), &inp, ts(T0)).unwrap();
    assert_eq!(d.sync, DraftSync::PendingCreate);
    assert_eq!(d.server_id, None);
    assert_eq!(
        (d.patch_set, d.revision.as_str()),
        (PatchSetNumber(2), "sha2")
    );
    assert_eq!(d.input(), inp);
    assert_eq!((d.attempts, d.version, d.updated), (0, 0, ts(T0)));
    assert_eq!(s.load_draft(id, d.local_id).unwrap(), Some(d.clone()));
    assert_eq!(s.load_drafts(id, CHANGE).unwrap().len(), 1);
    assert!(s.load_drafts(id, ChangeNumber(6)).unwrap().is_empty());
    // File-level and line anchors round trip too.
    let mut f = input("file");
    f.location = Location::File;
    let d2 = s.create_draft(id, CHANGE, &ps(2), &f, ts(T0)).unwrap();
    assert_eq!(d2.location, Location::File);
    assert!(d2.local_id > d.local_id);
}

#[test]
fn upload_success_syncs_and_editing_makes_it_pending_again() {
    let (mut s, id) = setup();
    let d = s
        .create_draft(id, CHANGE, &ps(2), &input("a"), ts(T0))
        .unwrap();
    let d = s
        .record_draft_uploaded(id, d.local_id, d.version, "srv1", ts(T1))
        .unwrap()
        .unwrap();
    assert_eq!(d.sync, DraftSync::Synced);
    assert_eq!(d.server_id.as_deref(), Some("srv1"));

    let d = s
        .edit_draft(id, d.local_id, "b", false, ts(T2))
        .unwrap()
        .unwrap();
    assert_eq!(d.sync, DraftSync::PendingUpdate);
    assert_eq!(
        (d.message.as_str(), d.unresolved, d.version),
        ("b", false, 1)
    );
    assert_eq!(d.server_id.as_deref(), Some("srv1"));
    assert_eq!(d.updated, ts(T2));

    // Editing a draft that was never uploaded keeps it a create.
    let n = s
        .create_draft(id, CHANGE, &ps(2), &input("n"), ts(T0))
        .unwrap();
    let n = s
        .edit_draft(id, n.local_id, "n2", true, ts(T1))
        .unwrap()
        .unwrap();
    assert_eq!(n.sync, DraftSync::PendingCreate);
    assert!(
        s.edit_draft(id, LocalDraftId(999), "x", true, ts(T1))
            .unwrap()
            .is_none()
    );
}

#[test]
fn transient_failures_keep_the_draft_pending_and_count_attempts() {
    let (mut s, id) = setup();
    let d = s
        .create_draft(id, CHANGE, &ps(2), &input("a"), ts(T0))
        .unwrap();
    let f = DraftFailure::Transient("offline".into());
    let d = s
        .record_draft_failure(id, d.local_id, d.version, &f, ts(T1))
        .unwrap()
        .unwrap();
    assert_eq!(d.sync, DraftSync::PendingCreate);
    assert_eq!((d.attempts, d.last_error.as_deref()), (1, Some("offline")));
    let d = s
        .record_draft_failure(id, d.local_id, d.version, &f, ts(T2))
        .unwrap()
        .unwrap();
    assert_eq!(d.attempts, 2);
    // Success clears the bookkeeping.
    let d = s
        .record_draft_uploaded(id, d.local_id, d.version, "srv", ts(T3))
        .unwrap()
        .unwrap();
    assert_eq!((d.attempts, d.last_error), (0, None));
}

#[test]
fn rejections_fail_the_draft_with_its_operation_until_edited_or_retried() {
    let (mut s, id) = setup();
    let d = s
        .create_draft(id, CHANGE, &ps(2), &input("a"), ts(T0))
        .unwrap();
    let rejected = DraftFailure::Rejected {
        error: "change is closed".into(),
        retryable: false,
    };
    let d = s
        .record_draft_failure(id, d.local_id, d.version, &rejected, ts(T1))
        .unwrap()
        .unwrap();
    assert_eq!(
        d.sync,
        DraftSync::Failed {
            op: DraftOp::Create,
            error: "change is closed".into(),
            retryable: false
        }
    );
    assert!(d.is_visible());
    assert!(
        s.pending_drafts(id).unwrap().is_empty(),
        "no automatic retry"
    );
    assert_eq!(s.unsynced_drafts(id, CHANGE).unwrap().len(), 1);

    // A manual retry makes it pending again for the same operation ...
    let r = s.retry_failed_draft(id, d.local_id).unwrap().unwrap();
    assert_eq!((r.sync.clone(), r.attempts), (DraftSync::PendingCreate, 0));
    // ... and so does editing.
    s.record_draft_failure(id, d.local_id, d.version, &rejected, ts(T1))
        .unwrap();
    let e = s
        .edit_draft(id, d.local_id, "fixed", true, ts(T2))
        .unwrap()
        .unwrap();
    assert_eq!(e.sync, DraftSync::PendingCreate);
    // Retrying something that did not fail changes nothing.
    let same = s.retry_failed_draft(id, d.local_id).unwrap().unwrap();
    assert_eq!(same.sync, DraftSync::PendingCreate);
}

#[test]
fn a_failure_of_an_outdated_version_is_ignored() {
    let (mut s, id) = setup();
    let d = s
        .create_draft(id, CHANGE, &ps(2), &input("a"), ts(T0))
        .unwrap();
    // The user edits while the upload of version 0 is in flight ...
    s.edit_draft(id, d.local_id, "b", true, ts(T1)).unwrap();
    let rejected = DraftFailure::Rejected {
        error: "no".into(),
        retryable: false,
    };
    let after = s
        .record_draft_failure(id, d.local_id, 0, &rejected, ts(T2))
        .unwrap()
        .unwrap();
    assert_eq!(after.sync, DraftSync::PendingCreate);
}

#[test]
fn an_upload_of_an_outdated_version_stays_pending_as_an_update() {
    let (mut s, id) = setup();
    let d = s
        .create_draft(id, CHANGE, &ps(2), &input("a"), ts(T0))
        .unwrap();
    s.edit_draft(id, d.local_id, "b", true, ts(T1)).unwrap();
    // The create of version 0 succeeds after the edit.
    let after = s
        .record_draft_uploaded(id, d.local_id, 0, "srv", ts(T2))
        .unwrap()
        .unwrap();
    assert_eq!(after.sync, DraftSync::PendingUpdate);
    assert_eq!(after.server_id.as_deref(), Some("srv"));
    assert_eq!(after.message, "b");
}

#[test]
fn deleting_removes_unuploaded_drafts_and_marks_uploaded_ones() {
    let (mut s, id) = setup();
    let a = s
        .create_draft(id, CHANGE, &ps(2), &input("a"), ts(T0))
        .unwrap();
    assert_eq!(
        s.delete_draft(id, a.local_id).unwrap(),
        DeleteOutcome::Removed
    );
    assert!(s.load_draft(id, a.local_id).unwrap().is_none());
    assert_eq!(
        s.delete_draft(id, a.local_id).unwrap(),
        DeleteOutcome::NotFound
    );

    let b = s
        .create_draft(id, CHANGE, &ps(2), &input("b"), ts(T0))
        .unwrap();
    s.record_draft_uploaded(id, b.local_id, 0, "srv", ts(T1))
        .unwrap();
    assert_eq!(
        s.delete_draft(id, b.local_id).unwrap(),
        DeleteOutcome::Pending
    );
    let b2 = s.load_draft(id, b.local_id).unwrap().unwrap();
    assert_eq!(b2.sync, DraftSync::PendingDelete);
    assert!(!b2.is_visible());
    assert_eq!(s.pending_drafts(id).unwrap().len(), 1);
    assert!(s.unsynced_drafts(id, CHANGE).unwrap().is_empty());
    // A deleted draft cannot be edited.
    assert!(
        s.edit_draft(id, b.local_id, "x", true, ts(T2))
            .unwrap()
            .is_none()
    );
    // Confirmation removes it; confirming twice is harmless.
    assert!(s.record_draft_deleted(id, b.local_id).unwrap());
    assert!(!s.record_draft_deleted(id, b.local_id).unwrap());
    assert!(s.load_drafts(id, CHANGE).unwrap().is_empty());
}

#[test]
fn a_failed_delete_can_be_edited_back_to_life_or_retried() {
    let (mut s, id) = setup();
    let d = s
        .create_draft(id, CHANGE, &ps(2), &input("a"), ts(T0))
        .unwrap();
    s.record_draft_uploaded(id, d.local_id, 0, "srv", ts(T1))
        .unwrap();
    s.delete_draft(id, d.local_id).unwrap();
    let v = s.load_draft(id, d.local_id).unwrap().unwrap().version;
    let rejected = DraftFailure::Rejected {
        error: "forbidden".into(),
        retryable: true,
    };
    let f = s
        .record_draft_failure(id, d.local_id, v, &rejected, ts(T2))
        .unwrap()
        .unwrap();
    assert!(matches!(
        f.sync,
        DraftSync::Failed {
            op: DraftOp::Delete,
            retryable: true,
            ..
        }
    ));
    assert!(f.is_visible());
    let r = s.retry_failed_draft(id, d.local_id).unwrap().unwrap();
    assert_eq!(r.sync, DraftSync::PendingDelete);
    s.record_draft_failure(id, d.local_id, v, &rejected, ts(T2))
        .unwrap();
    let e = s
        .edit_draft(id, d.local_id, "keep it", true, ts(T3))
        .unwrap()
        .unwrap();
    assert_eq!(e.sync, DraftSync::PendingUpdate);
}

#[test]
fn an_upload_that_finishes_after_a_local_delete_is_reported() {
    let (mut s, id) = setup();
    let d = s
        .create_draft(id, CHANGE, &ps(2), &input("a"), ts(T0))
        .unwrap();
    s.delete_draft(id, d.local_id).unwrap(); // row removed
    // The create that was in flight completes: nobody wants the draft.
    assert!(
        s.record_draft_uploaded(id, d.local_id, 0, "srv", ts(T1))
            .unwrap()
            .is_none()
    );
}

#[test]
fn an_uploaded_draft_deleted_in_flight_gets_its_server_id_for_the_delete() {
    let (mut s, id) = setup();
    let d = s
        .create_draft(id, CHANGE, &ps(2), &input("a"), ts(T0))
        .unwrap();
    s.record_draft_uploaded(id, d.local_id, 0, "srv", ts(T1))
        .unwrap();
    let e = s
        .edit_draft(id, d.local_id, "b", true, ts(T2))
        .unwrap()
        .unwrap();
    s.delete_draft(id, d.local_id).unwrap();
    // The update that was in flight finishes: the delete stays pending.
    let after = s
        .record_draft_uploaded(id, d.local_id, e.version, "srv", ts(T3))
        .unwrap()
        .unwrap();
    assert_eq!(after.sync, DraftSync::PendingDelete);
    assert_eq!(after.server_id.as_deref(), Some("srv"));
}

#[test]
fn pending_drafts_are_oldest_first_and_server_scoped() {
    let (mut s, a) = setup();
    let b = s.server_id("https://b.example").unwrap();
    let d1 = s
        .create_draft(a, CHANGE, &ps(2), &input("1"), ts(T0))
        .unwrap();
    let d2 = s
        .create_draft(a, ChangeNumber(9), &ps(1), &input("2"), ts(T1))
        .unwrap();
    s.create_draft(b, CHANGE, &ps(2), &input("other"), ts(T1))
        .unwrap();
    let d3 = s
        .create_draft(a, CHANGE, &ps(2), &input("3"), ts(T2))
        .unwrap();
    s.record_draft_uploaded(a, d2.local_id, 0, "srv", ts(T3))
        .unwrap();
    let ids: Vec<_> = s
        .pending_drafts(a)
        .unwrap()
        .iter()
        .map(|d| d.local_id)
        .collect();
    assert_eq!(ids, [d1.local_id, d3.local_id]);
    assert_eq!(s.pending_drafts(b).unwrap().len(), 1);
    // A draft of another server cannot be loaded through this one.
    let other = s.pending_drafts(b).unwrap()[0].local_id;
    assert!(s.load_draft(a, other).unwrap().is_none());
}

#[test]
fn restart_as_create_forgets_the_server_id() {
    let (mut s, id) = setup();
    let d = s
        .create_draft(id, CHANGE, &ps(2), &input("a"), ts(T0))
        .unwrap();
    s.record_draft_uploaded(id, d.local_id, 0, "srv", ts(T1))
        .unwrap();
    s.edit_draft(id, d.local_id, "b", true, ts(T2)).unwrap();
    let r = s.restart_draft_as_create(id, d.local_id).unwrap().unwrap();
    assert_eq!(
        (r.sync, r.server_id, r.message.as_str()),
        (DraftSync::PendingCreate, None, "b")
    );
}

// ---- merge ----

fn synced(s: &mut Store, id: ServerId, server_id: &str, message: &str, at: &str) -> LocalDraftId {
    let d = s
        .create_draft(id, CHANGE, &ps(2), &input(message), ts(at))
        .unwrap();
    s.record_draft_uploaded(id, d.local_id, 0, server_id, ts(at))
        .unwrap();
    d.local_id
}

fn merge(s: &mut Store, id: ServerId, server: &[Comment], started: &str) -> MergeReport {
    s.merge_server_drafts(id, CHANGE, server, &[ps(2)], ts(started), ts(T3))
        .unwrap()
}

#[test]
fn merge_adds_unknown_server_drafts_as_synced() {
    let (mut s, id) = setup();
    let mut c = comment("srv1", "from the web");
    c.in_reply_to = Some("c0".into());
    c.unresolved = true;
    let r = merge(&mut s, id, &[c], T3);
    assert_eq!((r.added, r.changed()), (1, true));
    let d = &s.load_drafts(id, CHANGE).unwrap()[0];
    assert_eq!(d.sync, DraftSync::Synced);
    assert_eq!(d.server_id.as_deref(), Some("srv1"));
    assert_eq!(d.revision.as_str(), "sha2", "taken from the patch set list");
    assert_eq!(d.in_reply_to.as_deref(), Some("c0"));
    assert!(d.unresolved && d.message == "from the web");
    // Merging the same list again changes nothing.
    let mut again = comment("srv1", "from the web");
    again.in_reply_to = Some("c0".into());
    again.unresolved = true;
    assert!(!merge(&mut s, id, &[again], T3).changed());
}

#[test]
fn merge_uses_commit_id_for_unknown_patch_sets_and_skips_without() {
    let (mut s, id) = setup();
    let mut known = comment("a", "x");
    known.patch_set = PatchSetNumber(9);
    known.commit_id = Some("sha9".into());
    let mut unknown = comment("b", "y");
    unknown.patch_set = PatchSetNumber(8);
    let r = merge(&mut s, id, &[known, unknown], T3);
    assert_eq!((r.added, r.skipped), (1, 1));
    assert_eq!(
        s.load_drafts(id, CHANGE).unwrap()[0].revision.as_str(),
        "sha9"
    );
}

#[test]
fn merge_overwrites_synced_rows_with_the_servers_copy() {
    let (mut s, id) = setup();
    let local = synced(&mut s, id, "srv1", "old text", T0);
    let mut c = comment("srv1", "edited in the web UI");
    c.unresolved = true;
    let r = merge(&mut s, id, &[c.clone()], T3);
    assert_eq!((r.updated, r.added), (1, 0));
    let d = s.load_draft(id, local).unwrap().unwrap();
    assert_eq!(
        (d.message.as_str(), d.sync.clone()),
        ("edited in the web UI", DraftSync::Synced)
    );
    assert!(d.unresolved);
    assert!(!merge(&mut s, id, &[c], T3).changed());
}

#[test]
fn merge_keeps_local_pending_edits() {
    let (mut s, id) = setup();
    let local = synced(&mut s, id, "srv1", "base", T0);
    s.edit_draft(id, local, "my pending edit", true, ts(T2))
        .unwrap();
    let r = merge(&mut s, id, &[comment("srv1", "server copy")], T3);
    assert!(!r.changed());
    let d = s.load_draft(id, local).unwrap().unwrap();
    assert_eq!(
        (d.message.as_str(), d.sync),
        ("my pending edit", DraftSync::PendingUpdate)
    );
}

#[test]
fn merge_keeps_pending_deletes_and_failed_drafts_that_the_server_still_has() {
    let (mut s, id) = setup();
    let del = synced(&mut s, id, "srv1", "a", T0);
    s.delete_draft(id, del).unwrap();
    let failed = synced(&mut s, id, "srv2", "b", T0);
    s.edit_draft(id, failed, "b2", true, ts(T1)).unwrap();
    let v = s.load_draft(id, failed).unwrap().unwrap().version;
    s.record_draft_failure(
        id,
        failed,
        v,
        &DraftFailure::Rejected {
            error: "no".into(),
            retryable: false,
        },
        ts(T2),
    )
    .unwrap();
    let r = merge(
        &mut s,
        id,
        &[comment("srv1", "a"), comment("srv2", "b")],
        T3,
    );
    assert!(!r.changed());
    assert_eq!(sync_of(&s, id, del), DraftSync::PendingDelete);
    assert!(matches!(sync_of(&s, id, failed), DraftSync::Failed { .. }));
}

#[test]
fn merge_removes_synced_drafts_the_server_lost() {
    let (mut s, id) = setup();
    let gone = synced(&mut s, id, "srv1", "published from the web UI", T0);
    let kept = synced(&mut s, id, "srv2", "still there", T0);
    let r = merge(&mut s, id, &[comment("srv2", "still there")], T3);
    assert_eq!(r.removed, 1);
    assert!(s.load_draft(id, gone).unwrap().is_none());
    assert!(s.load_draft(id, kept).unwrap().is_some());
}

#[test]
fn merge_does_not_remove_a_draft_uploaded_after_the_fetch_began() {
    let (mut s, id) = setup();
    // Synced at T2; the fetch that produced the list began at T1.
    let late = synced(&mut s, id, "srv1", "just uploaded", T2);
    let r = merge(&mut s, id, &[], T1);
    assert!(!r.changed());
    assert!(s.load_draft(id, late).unwrap().is_some());
    // A fetch that began later does remove it.
    let r = merge(&mut s, id, &[], T3);
    assert_eq!(r.removed, 1);
}

#[test]
fn merge_confirms_deletes_and_recreates_edited_drafts_the_server_lost() {
    let (mut s, id) = setup();
    let del = synced(&mut s, id, "srv1", "a", T0);
    s.delete_draft(id, del).unwrap();
    let edited = synced(&mut s, id, "srv2", "b", T0);
    s.edit_draft(id, edited, "b, edited offline", true, ts(T1))
        .unwrap();
    let failed = synced(&mut s, id, "srv3", "c", T0);
    s.edit_draft(id, failed, "c2", true, ts(T1)).unwrap();
    let v = s.load_draft(id, failed).unwrap().unwrap().version;
    s.record_draft_failure(
        id,
        failed,
        v,
        &DraftFailure::Rejected {
            error: "no".into(),
            retryable: false,
        },
        ts(T2),
    )
    .unwrap();
    let new = s
        .create_draft(id, CHANGE, &ps(2), &input("never uploaded"), ts(T1))
        .unwrap();

    let r = merge(&mut s, id, &[], T3);
    assert_eq!((r.removed, r.recreated), (1, 1));
    assert!(s.load_draft(id, del).unwrap().is_none());
    let e = s.load_draft(id, edited).unwrap().unwrap();
    assert_eq!(
        (e.sync, e.server_id, e.message.as_str()),
        (DraftSync::PendingCreate, None, "b, edited offline")
    );
    assert!(matches!(sync_of(&s, id, failed), DraftSync::Failed { .. }));
    assert_eq!(sync_of(&s, id, new.local_id), DraftSync::PendingCreate);
}

#[test]
fn merge_is_scoped_to_one_change_and_server() {
    let (mut s, id) = setup();
    let other_server = s.server_id("https://b.example").unwrap();
    let a = synced(&mut s, id, "srv1", "a", T0);
    let other_change = s
        .create_draft(id, ChangeNumber(6), &ps(2), &input("c6"), ts(T0))
        .unwrap();
    s.record_draft_uploaded(id, other_change.local_id, 0, "srv9", ts(T0))
        .unwrap();
    let b = s
        .create_draft(other_server, CHANGE, &ps(2), &input("b"), ts(T0))
        .unwrap();
    s.record_draft_uploaded(other_server, b.local_id, 0, "srv1", ts(T0))
        .unwrap();
    // The same server draft id on two servers is two drafts.
    let r = merge(&mut s, id, &[], T3);
    assert_eq!(r.removed, 1);
    assert!(s.load_draft(id, a).unwrap().is_none());
    assert!(s.load_draft(id, other_change.local_id).unwrap().is_some());
    assert!(s.load_draft(other_server, b.local_id).unwrap().is_some());
}

// ---- reviewed marks ----

fn file(path: &str) -> FileEntry {
    FileEntry {
        path: path.into(),
        status: FileStatus::Modified,
        old_path: None,
        lines_inserted: 0,
        lines_deleted: 0,
        size_delta: 0,
        size: 0,
        binary: false,
    }
}

fn key(base: Option<u32>, target: u32) -> FilesKey {
    FilesKey {
        change: CHANGE,
        base: base.map(ps),
        target: ps(target),
    }
}

#[test]
fn reviewed_marks_are_set_in_every_list_of_the_patch_set() {
    let (mut s, id) = setup();
    let files = [file("a"), file("b")];
    s.save_file_list(id, &key(None, 2), &files, Some(&["a".to_owned()]), ts(T0))
        .unwrap();
    s.save_file_list(id, &key(Some(1), 2), &files, Some(&[]), ts(T0))
        .unwrap();
    s.save_file_list(id, &key(None, 3), &files, Some(&["a".to_owned()]), ts(T0))
        .unwrap();
    let reviewed = |s: &Store, k: &FilesKey| s.load_file_list(id, k).unwrap().unwrap().reviewed;

    let n = s
        .set_reviewed_path(id, CHANGE, &ps(2).sha, "b", true)
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(
        reviewed(&s, &key(None, 2)),
        Some(vec!["a".into(), "b".into()])
    );
    assert_eq!(reviewed(&s, &key(Some(1), 2)), Some(vec!["b".into()]));
    // Another patch set is untouched.
    assert_eq!(reviewed(&s, &key(None, 3)), Some(vec!["a".into()]));

    // Idempotent, and clearing works.
    assert_eq!(
        s.set_reviewed_path(id, CHANGE, &ps(2).sha, "b", true)
            .unwrap(),
        0
    );
    assert_eq!(
        s.set_reviewed_path(id, CHANGE, &ps(2).sha, "a", false)
            .unwrap(),
        1
    );
    assert_eq!(reviewed(&s, &key(None, 2)), Some(vec!["b".into()]));
    assert_eq!(
        s.set_reviewed_path(id, CHANGE, &ps(2).sha, "zzz", false)
            .unwrap(),
        0
    );
}

#[test]
fn reviewed_marks_of_a_list_without_marks_start_empty() {
    let (mut s, id) = setup();
    s.save_file_list(id, &key(None, 2), &[file("a")], None, ts(T0))
        .unwrap();
    assert_eq!(
        s.set_reviewed_path(id, CHANGE, &ps(2).sha, "a", true)
            .unwrap(),
        1
    );
    let got = s.load_file_list(id, &key(None, 2)).unwrap().unwrap();
    assert_eq!(got.reviewed, Some(vec!["a".to_owned()]));
    // No list cached: nothing to update.
    assert_eq!(
        s.set_reviewed_path(id, CHANGE, &ps(7).sha, "a", true)
            .unwrap(),
        0
    );
}
