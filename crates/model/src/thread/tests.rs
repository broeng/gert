use super::*;
use crate::{AccountId, ChangeNumber, CommentRange, DraftSync, FileStatus, RevisionId};

fn ts(minute: u32) -> Timestamp {
    format!("2024-03-01T10:{minute:02}:00Z").parse().unwrap()
}

fn comment(id: &str, reply_to: Option<&str>, minute: u32) -> Comment {
    Comment {
        id: id.into(),
        path: "src/a.rs".into(),
        patch_set: PatchSetNumber(2),
        side: Side::Revision,
        parent: None,
        location: Location::Line(10),
        in_reply_to: reply_to.map(Into::into),
        message: format!("message {id}"),
        author: Some(Account {
            id: AccountId(1),
            name: Some("Ann".into()),
            email: None,
            username: None,
            display_name: None,
        }),
        updated: ts(minute),
        unresolved: false,
        commit_id: None,
        tag: None,
        change_message_id: None,
        robot_id: None,
        context: vec![],
    }
}

fn draft(local: i64, reply_to: Option<&str>, minute: u32) -> Draft {
    Draft {
        local_id: LocalDraftId(local),
        server_id: None,
        change: ChangeNumber(1),
        patch_set: PatchSetNumber(2),
        revision: RevisionId::from("bbb"),
        path: "src/a.rs".into(),
        side: Side::Revision,
        parent: None,
        location: Location::Line(10),
        in_reply_to: reply_to.map(Into::into),
        message: format!("draft {local}"),
        unresolved: false,
        updated: ts(minute),
        sync: DraftSync::PendingCreate,
        attempts: 0,
        last_error: None,
        version: 0,
    }
}

fn ids(t: &Thread) -> Vec<String> {
    t.entries
        .iter()
        .map(|e| match e {
            Entry::Published(c) => c.id.clone(),
            Entry::Draft(d) => format!("d{}", d.local_id.0),
        })
        .collect()
}

#[test]
fn replies_form_one_thread_in_time_order_whatever_the_input_order() {
    let cs = [
        comment("c3", Some("c2"), 3),
        comment("c1", None, 1),
        comment("c4", Some("c1"), 4), // a second reply to the root
        comment("c2", Some("c1"), 2),
    ];
    let threads = build_threads(&cs, &[]);
    assert_eq!(threads.len(), 1);
    assert_eq!(ids(&threads[0]), ["c1", "c2", "c3", "c4"]);
    assert_eq!(threads[0].id, ThreadId::Comment("c1".into()));
    assert!(!threads[0].orphaned);
    assert_eq!(threads[0].reply_target(), Some("c4"));
    assert_eq!(threads[0].comment_count(), 4);
}

#[test]
fn state_is_the_last_entry_and_a_draft_counts() {
    let mut c1 = comment("c1", None, 1);
    c1.unresolved = true;
    let c2 = comment("c2", Some("c1"), 2); // resolves
    let t = &build_threads(&[c1.clone(), c2.clone()], &[])[0];
    assert!(!t.is_unresolved());

    // Only the root: unresolved.
    assert!(build_threads(&[c1.clone()], &[])[0].is_unresolved());

    // A newer draft reply reopens a resolved thread ...
    let mut reopen = draft(1, Some("c2"), 5);
    reopen.unresolved = true;
    let t = &build_threads(&[c1.clone(), c2.clone()], &[reopen])[0];
    assert!(t.is_unresolved() && t.has_draft());
    assert_eq!(ids(t), ["c1", "c2", "d1"]);
    assert_eq!(t.reply_target(), Some("c2"));
    // ... and a resolving draft closes an unresolved one.
    let t = &build_threads(&[c1], &[draft(2, Some("c1"), 5)])[0];
    assert!(!t.is_unresolved());
}

#[test]
fn drafts_attach_to_the_thread_of_the_comment_they_answer() {
    let cs = [comment("c1", None, 1), comment("c2", Some("c1"), 2)];
    // Replying to the middle of a chain still joins the one thread.
    let threads = build_threads(&cs, &[draft(7, Some("c1"), 9)]);
    assert_eq!(threads.len(), 1);
    assert_eq!(ids(&threads[0]), ["c1", "c2", "d7"]);
    assert_eq!(
        threads[0].draft().map(|d| d.local_id),
        Some(LocalDraftId(7))
    );
    assert_eq!(
        (threads[0].comment_count(), threads[0].draft_count()),
        (2, 1)
    );
}

#[test]
fn a_draft_without_parent_is_a_new_thread_with_its_own_anchor() {
    let mut d = draft(3, None, 5);
    d.path = "src/b.rs".into();
    d.location = Location::File;
    let threads = build_threads(&[comment("c1", None, 1)], &[d]);
    assert_eq!(threads.len(), 2);
    let t = &threads[1];
    assert_eq!(t.id, ThreadId::Draft(LocalDraftId(3)));
    assert_eq!((t.path.as_str(), t.location), ("src/b.rs", Location::File));
    assert_eq!(t.reply_target(), None);
    assert!(!t.orphaned);
}

#[test]
fn deleted_drafts_are_not_in_threads() {
    let mut d = draft(1, Some("c1"), 5);
    d.sync = DraftSync::PendingDelete;
    let threads = build_threads(&[comment("c1", None, 1)], &[d.clone()]);
    assert!(!threads[0].has_draft());
    assert!(build_threads(&[], &[d]).is_empty());
}

#[test]
fn orphans_start_a_thread_at_the_topmost_known_comment() {
    // c2 answers c1, c1 answers a comment we lack.
    let cs = [comment("c2", Some("c1"), 2), comment("c1", Some("gone"), 1)];
    let threads = build_threads(&cs, &[]);
    assert_eq!(threads.len(), 1);
    assert!(threads[0].orphaned);
    assert_eq!(threads[0].id, ThreadId::Comment("c1".into()));
    assert_eq!(ids(&threads[0]), ["c1", "c2"]);
}

#[test]
fn drafts_answering_a_missing_comment_are_grouped_and_orphaned() {
    let threads = build_threads(&[], &[draft(1, Some("gone"), 2), draft(2, Some("gone"), 3)]);
    assert_eq!(threads.len(), 1);
    assert!(threads[0].orphaned);
    assert_eq!(ids(&threads[0]), ["d1", "d2"]);
}

#[test]
fn reply_cycles_do_not_loop() {
    let cs = [comment("a", Some("b"), 1), comment("b", Some("a"), 2)];
    let threads = build_threads(&cs, &[]);
    let total: usize = threads.iter().map(|t| t.entries.len()).sum();
    assert_eq!(total, 2);
}

#[test]
fn equal_times_put_published_before_drafts() {
    let threads = build_threads(
        &[comment("c1", None, 1), comment("c2", Some("c1"), 5)],
        &[draft(1, Some("c1"), 5)],
    );
    assert_eq!(ids(&threads[0]), ["c1", "c2", "d1"]);
}

#[test]
fn threads_sort_by_file_patch_set_and_line() {
    let mut a = comment("a", None, 5);
    a.path = "z.rs".into();
    let mut b = comment("b", None, 6);
    b.path = "/COMMIT_MSG".into();
    b.location = Location::Line(9);
    let mut c = comment("c", None, 7);
    c.location = Location::Line(2);
    let mut d = comment("d", None, 8);
    d.location = Location::File;
    let mut e = comment("e", None, 9);
    e.patch_set = PatchSetNumber(1);
    e.location = Location::Line(50);
    let threads = build_threads(&[a, b, c, d, e], &[]);
    let order: Vec<_> = threads
        .iter()
        .map(|t| t.entries[0].message().to_owned())
        .collect();
    assert_eq!(
        order,
        [
            "message b",
            "message e",
            "message d",
            "message c",
            "message a"
        ]
    );
}

fn thread_on(patch_set: u32, side: Side, parent: Option<u32>, path: &str) -> Thread {
    let mut c = comment("c", None, 1);
    c.patch_set = PatchSetNumber(patch_set);
    c.side = side;
    c.parent = parent;
    c.path = path.into();
    build_threads(&[c], &[]).remove(0)
}

fn pair(base: Option<u32>, target: u32) -> DiffPair {
    DiffPair {
        base: base.map(PatchSetNumber),
        target: PatchSetNumber(target),
    }
}

#[test]
fn visibility_of_revision_side_comments() {
    let t = thread_on(2, Side::Revision, None, "f");
    let side = |p| t.side_in(p, "f", None);
    // Target of the diff: right side, with any base.
    assert_eq!(side(pair(None, 2)), Some(DiffSide::Right));
    assert_eq!(side(pair(Some(1), 2)), Some(DiffSide::Right));
    // Base of the diff: left side.
    assert_eq!(side(pair(Some(2), 3)), Some(DiffSide::Left));
    // Neither.
    assert_eq!(side(pair(None, 3)), None);
    assert_eq!(side(pair(Some(1), 3)), None);
    assert_eq!(t.side_in(pair(None, 2), "other", None), None);
}

#[test]
fn visibility_of_parent_side_comments() {
    let t = thread_on(2, Side::Parent, None, "f");
    let side = |p| t.side_in(p, "f", None);
    assert_eq!(side(pair(None, 2)), Some(DiffSide::Left));
    // Against another patch set the left side is not the parent.
    assert_eq!(side(pair(Some(1), 2)), None);
    // The parent of patch set 2 is not what patch set 3 is compared to.
    assert_eq!(side(pair(None, 3)), None);
    assert_eq!(side(pair(Some(2), 3)), None);
    // Merge commits: only the first parent is diffed.
    let p1 = thread_on(2, Side::Parent, Some(1), "f");
    let p2 = thread_on(2, Side::Parent, Some(2), "f");
    assert_eq!(p1.side_in(pair(None, 2), "f", None), Some(DiffSide::Left));
    assert_eq!(p2.side_in(pair(None, 2), "f", None), None);
}

#[test]
fn left_side_comments_of_a_renamed_file_carry_the_old_path() {
    let t = thread_on(1, Side::Revision, None, "old.rs");
    assert_eq!(
        t.side_in(pair(Some(1), 2), "new.rs", Some("old.rs")),
        Some(DiffSide::Left)
    );
    // The right side is matched by the new path only.
    let r = thread_on(2, Side::Revision, None, "old.rs");
    assert_eq!(r.side_in(pair(Some(1), 2), "new.rs", Some("old.rs")), None);
}

fn file(path: &str, old: Option<&str>) -> FileEntry {
    FileEntry {
        path: path.into(),
        status: FileStatus::Modified,
        old_path: old.map(Into::into),
        lines_inserted: 0,
        lines_deleted: 0,
        size_delta: 0,
        size: 0,
        binary: false,
    }
}

#[test]
fn counts_per_file_for_a_pair() {
    // f: ps2 thread with a reply (unresolved), ps1 thread (resolved) with a
    // draft; g: only a ps3 comment; h: nothing.
    let mut root = comment("c1", None, 1);
    root.unresolved = true;
    root.path = "f".into();
    let mut reply = comment("c2", Some("c1"), 2);
    reply.path = "f".into();
    let mut reopen = comment("c3", None, 3);
    reopen.unresolved = true;
    reopen.path = "f".into();
    // Thread c1/c2 ends resolved, thread c3 unresolved.
    let mut old = comment("c4", None, 4);
    old.path = "f".into();
    old.patch_set = PatchSetNumber(1);
    let mut elsewhere = comment("c5", None, 5);
    elsewhere.path = "g".into();
    elsewhere.patch_set = PatchSetNumber(3);
    let mut d = draft(1, None, 6);
    d.path = "f".into();
    let threads = build_threads(&[root, reply, reopen, old, elsewhere], &[d]);
    let files = [file("f", None), file("g", None), file("h", None)];

    let counts = comment_counts(&threads, pair(None, 2), &files);
    assert_eq!(
        counts["f"],
        FileCommentCounts {
            comments: 3,
            unresolved: 1,
            drafts: 1,
            elsewhere: 1
        }
    );
    assert_eq!(
        counts["g"],
        FileCommentCounts {
            elsewhere: 1,
            ..Default::default()
        }
    );
    assert!(!counts.contains_key("h"));

    // Comparing 1..2 additionally shows the patch set 1 thread on the left.
    let counts = comment_counts(&threads, pair(Some(1), 2), &files);
    assert_eq!(counts["f"].comments, 4);
    assert_eq!(counts["f"].elsewhere, 0);
}

#[test]
fn counts_follow_renames_and_skip_unlisted_paths() {
    let mut left = comment("c1", None, 1);
    left.path = "old.rs".into();
    left.patch_set = PatchSetNumber(1);
    let mut psl = comment("c2", None, 2);
    psl.path = "/PATCHSET_LEVEL".into();
    psl.location = Location::File;
    let threads = build_threads(&[left, psl], &[]);
    let files = [file("new.rs", Some("old.rs"))];
    let counts = comment_counts(&threads, pair(Some(1), 2), &files);
    assert_eq!(counts.len(), 1);
    assert_eq!(counts["new.rs"].comments, 1);
}

#[test]
fn range_threads_keep_their_range() {
    let r = CommentRange {
        start_line: 3,
        start_character: 1,
        end_line: 4,
        end_character: 0,
    };
    let mut c = comment("c1", None, 1);
    c.location = Location::Range(r);
    assert_eq!(build_threads(&[c], &[])[0].location, Location::Range(r));
}
