use model::{
    ChangedChunk, DiffChunk, DiffContext, DiffViewMode, FileStatus, PatchSetNumber, Whitespace,
};

use super::*;
use crate::{
    Store,
    migrations::{MIGRATIONS, migrate, user_version},
};

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

const T0: &str = "2024-02-01T00:00:00Z";
const T1: &str = "2024-02-02T00:00:00Z";

fn store() -> (Store, ServerId) {
    let mut s = Store::open_in_memory().unwrap();
    let id = s.server_id("https://g.example/").unwrap();
    (s, id)
}

fn ps(n: u32, sha: &str) -> PatchSetRef {
    PatchSetRef {
        number: PatchSetNumber(n),
        sha: sha.into(),
    }
}

fn files_key() -> FilesKey {
    FilesKey {
        change: ChangeNumber(7),
        base: None,
        target: ps(2, "bbb"),
    }
}

fn diff_key() -> DiffKey {
    DiffKey {
        change: ChangeNumber(7),
        base: Some(ps(1, "aaa")),
        target: ps(2, "bbb"),
        path: "src/main.rs".into(),
        whitespace: Whitespace::IgnoreNone,
        context: DiffContext::Lines(10),
    }
}

fn file(path: &str) -> FileEntry {
    FileEntry {
        path: path.into(),
        status: FileStatus::Modified,
        old_path: None,
        lines_inserted: 3,
        lines_deleted: 1,
        size_delta: 20,
        size: 200,
        binary: false,
    }
}

fn diff(text: &str) -> Diff {
    Diff {
        meta_a: None,
        meta_b: None,
        change_type: None,
        intraline_status: None,
        diff_header: vec![],
        chunks: vec![
            DiffChunk::Common {
                lines: vec!["same".into()],
            },
            DiffChunk::Changed(ChangedChunk {
                a: vec!["old".into()],
                b: vec![text.into()],
                ..Default::default()
            }),
            DiffChunk::Skip { lines: 5 },
        ],
        binary: false,
    }
}

#[test]
fn file_list_round_trip_and_reviewed_marks() {
    let (mut s, id) = store();
    let key = files_key();
    assert_eq!(s.load_file_list(id, &key).unwrap(), None);

    let files = [file("/COMMIT_MSG"), file("a.rs")];
    s.save_file_list(id, &key, &files, None, ts(T0)).unwrap();
    let got = s.load_file_list(id, &key).unwrap().unwrap();
    assert_eq!(got.files, files);
    assert_eq!(got.reviewed, None);
    assert_eq!(got.fetched_at, ts(T0));

    let reviewed = vec!["a.rs".to_owned()];
    s.save_file_list(id, &key, &files, Some(&reviewed), ts(T1))
        .unwrap();
    let got = s.load_file_list(id, &key).unwrap().unwrap();
    assert_eq!(got.reviewed.as_deref(), Some(&reviewed[..]));
    assert_eq!(got.fetched_at, ts(T1));

    // A later refresh that could not fetch the marks keeps the known ones;
    // one that fetched an empty set clears them.
    s.save_file_list(id, &key, &files[..1], None, ts(T1))
        .unwrap();
    let got = s.load_file_list(id, &key).unwrap().unwrap();
    assert_eq!(got.files.len(), 1);
    assert_eq!(got.reviewed.as_deref(), Some(&reviewed[..]));
    s.save_file_list(id, &key, &files, Some(&[]), ts(T1))
        .unwrap();
    let got = s.load_file_list(id, &key).unwrap().unwrap();
    assert_eq!(got.reviewed, Some(vec![]));
}

#[test]
fn file_lists_are_keyed_by_change_base_and_target_sha() {
    let (mut s, id) = store();
    let key = files_key();
    s.save_file_list(id, &key, &[file("x")], None, ts(T0))
        .unwrap();

    let other_base = FilesKey {
        base: Some(ps(1, "aaa")),
        ..key.clone()
    };
    let other_target = FilesKey {
        target: ps(3, "ccc"),
        ..key.clone()
    };
    // Same patch set number, different commit (change re-imported).
    let same_number_new_sha = FilesKey {
        target: ps(2, "zzz"),
        ..key.clone()
    };
    let other_change = FilesKey {
        change: ChangeNumber(8),
        ..key.clone()
    };
    for k in [
        &other_base,
        &other_target,
        &same_number_new_sha,
        &other_change,
    ] {
        assert_eq!(s.load_file_list(id, k).unwrap(), None, "{k:?}");
    }
    s.save_file_list(id, &other_base, &[file("y")], None, ts(T0))
        .unwrap();
    assert_eq!(
        s.load_file_list(id, &key).unwrap().unwrap().files[0].path,
        "x"
    );
    assert_eq!(
        s.load_file_list(id, &other_base).unwrap().unwrap().files[0].path,
        "y"
    );
}

#[test]
fn diff_round_trip() {
    let (mut s, id) = store();
    let key = diff_key();
    assert_eq!(s.load_diff(id, &key).unwrap(), None);
    assert!(!s.has_diff(id, &key).unwrap());
    s.save_diff(id, &key, &diff("new"), ts(T0)).unwrap();
    assert!(s.has_diff(id, &key).unwrap());
    let got = s.load_diff(id, &key).unwrap().unwrap();
    assert_eq!(got.diff, diff("new"));
    assert_eq!(got.fetched_at, ts(T0));

    // Saving again replaces.
    s.save_diff(id, &key, &diff("newer"), ts(T1)).unwrap();
    let got = s.load_diff(id, &key).unwrap().unwrap();
    assert_eq!(got.diff, diff("newer"));
    assert_eq!(got.fetched_at, ts(T1));
}

#[test]
fn every_part_of_the_diff_key_matters() {
    let (mut s, id) = store();
    let key = diff_key();
    s.save_diff(id, &key, &diff("x"), ts(T0)).unwrap();
    let variants = [
        DiffKey {
            base: None,
            ..key.clone()
        },
        DiffKey {
            base: Some(ps(1, "other")),
            ..key.clone()
        },
        DiffKey {
            target: ps(2, "other"),
            ..key.clone()
        },
        DiffKey {
            path: "src/other.rs".into(),
            ..key.clone()
        },
        DiffKey {
            whitespace: Whitespace::IgnoreAll,
            ..key.clone()
        },
        DiffKey {
            context: DiffContext::All,
            ..key.clone()
        },
        DiffKey {
            context: DiffContext::Lines(3),
            ..key.clone()
        },
        DiffKey {
            change: ChangeNumber(8),
            ..key.clone()
        },
    ];
    for v in &variants {
        assert!(!s.has_diff(id, v).unwrap(), "{v:?}");
        assert_eq!(s.load_diff(id, v).unwrap(), None, "{v:?}");
    }
    // The patch set number is not part of the key, only the SHA.
    let renumbered = DiffKey {
        target: ps(9, "bbb"),
        ..key.clone()
    };
    assert!(s.has_diff(id, &renumbered).unwrap());
}

#[test]
fn caches_are_per_server() {
    let (mut s, a) = store();
    let b = s.server_id("https://other.example").unwrap();
    s.save_diff(a, &diff_key(), &diff("x"), ts(T0)).unwrap();
    s.save_file_list(a, &files_key(), &[file("x")], None, ts(T0))
        .unwrap();
    s.save_related(a, ChangeNumber(7), &"bbb".into(), &[], ts(T0))
        .unwrap();
    s.save_diff_preferences(a, &DiffPreferences::default(), ts(T0))
        .unwrap();
    assert_eq!(s.load_diff(b, &diff_key()).unwrap(), None);
    assert_eq!(s.load_file_list(b, &files_key()).unwrap(), None);
    assert_eq!(
        s.load_related(b, ChangeNumber(7), &"bbb".into()).unwrap(),
        None
    );
    assert_eq!(s.load_diff_preferences(b).unwrap(), None);
}

#[test]
fn related_changes_round_trip() {
    let (mut s, id) = store();
    let rev = RevisionId::from("bbb");
    assert_eq!(s.load_related(id, ChangeNumber(7), &rev).unwrap(), None);
    let related = vec![RelatedChange {
        project: "p".into(),
        change_id: Some("I1".into()),
        number: ChangeNumber(8),
        patch_set: PatchSetNumber(1),
        current_patch_set: Some(PatchSetNumber(2)),
        status: Some(model::ChangeStatus::New),
        submittable: None,
        work_in_progress: false,
        commit: None,
    }];
    s.save_related(id, ChangeNumber(7), &rev, &related, ts(T0))
        .unwrap();
    let got = s.load_related(id, ChangeNumber(7), &rev).unwrap().unwrap();
    assert_eq!(got.changes, related);
    assert_eq!(got.fetched_at, ts(T0));
    // An empty list is a real answer, different from "never fetched".
    s.save_related(id, ChangeNumber(7), &rev, &[], ts(T1))
        .unwrap();
    assert!(
        s.load_related(id, ChangeNumber(7), &rev)
            .unwrap()
            .unwrap()
            .changes
            .is_empty()
    );
}

#[test]
fn diff_preferences_round_trip() {
    let (mut s, id) = store();
    assert_eq!(s.load_diff_preferences(id).unwrap(), None);
    let prefs = DiffPreferences {
        context: Some(DiffContext::All),
        ignore_whitespace: Some(Whitespace::Unknown("X".into())),
        tab_size: Some(4),
        show_tabs: Some(false),
        view_mode: Some(DiffViewMode::Unified),
        ..Default::default()
    };
    s.save_diff_preferences(id, &prefs, ts(T0)).unwrap();
    let got = s.load_diff_preferences(id).unwrap().unwrap();
    assert_eq!(got.preferences, prefs);
    s.save_diff_preferences(id, &DiffPreferences::default(), ts(T1))
        .unwrap();
    let got = s.load_diff_preferences(id).unwrap().unwrap();
    assert_eq!(got.preferences, DiffPreferences::default());
    assert_eq!(got.fetched_at, ts(T1));
}

fn key_n(n: u64) -> DiffKey {
    DiffKey {
        change: ChangeNumber(n),
        ..diff_key()
    }
}

#[test]
fn prune_drops_rows_older_than_max_age() {
    let (mut s, id) = store();
    s.save_diff(id, &key_n(1), &diff("old"), ts("2024-01-01T00:00:00Z"))
        .unwrap();
    s.save_diff(id, &key_n(2), &diff("new"), ts("2024-01-20T00:00:00Z"))
        .unwrap();
    s.save_file_list(
        id,
        &files_key(),
        &[file("a")],
        None,
        ts("2024-01-01T00:00:00Z"),
    )
    .unwrap();
    s.save_related(
        id,
        ChangeNumber(7),
        &"bbb".into(),
        &[],
        ts("2024-01-01T00:00:00Z"),
    )
    .unwrap();
    s.save_diff_preferences(id, &DiffPreferences::default(), ts("2024-01-01T00:00:00Z"))
        .unwrap();

    let policy = PrunePolicy {
        max_age: Duration::from_secs(14 * 24 * 3600),
        max_diff_bytes: u64::MAX,
    };
    let report = s.prune(&policy, ts("2024-01-25T00:00:00Z")).unwrap();
    assert_eq!(
        report,
        PruneReport {
            diffs: 1,
            file_lists: 1,
            related: 1
        }
    );
    assert!(!s.has_diff(id, &key_n(1)).unwrap());
    assert!(s.has_diff(id, &key_n(2)).unwrap());
    assert_eq!(s.load_file_list(id, &files_key()).unwrap(), None);
    // Preferences are tiny and never pruned.
    assert!(s.load_diff_preferences(id).unwrap().is_some());
    // Nothing more to do.
    assert_eq!(
        s.prune(&policy, ts("2024-01-25T00:00:00Z")).unwrap(),
        PruneReport::default()
    );
}

#[test]
fn prune_enforces_the_size_cap_oldest_first() {
    let (mut s, id) = store();
    let size = serde_json::to_string(&diff("0")).unwrap().len() as u64;
    // Five diffs of the same size, saved out of order of their keys.
    for (n, at) in [
        (1, "2024-01-05T00:00:00Z"),
        (2, "2024-01-03T00:00:00Z"),
        (3, "2024-01-01T00:00:00Z"),
        (4, "2024-01-04T00:00:00Z"),
        (5, "2024-01-02T00:00:00Z"),
    ] {
        s.save_diff(id, &key_n(n), &diff("0"), ts(at)).unwrap();
    }
    let policy = PrunePolicy {
        max_age: Duration::from_secs(365 * 24 * 3600),
        max_diff_bytes: size * 2 + size / 2,
    };
    let report = s.prune(&policy, ts("2024-01-06T00:00:00Z")).unwrap();
    assert_eq!(report.diffs, 3);
    // The two newest remain: #1 (Jan 5) and #4 (Jan 4).
    let kept: Vec<u64> = (1..=5)
        .filter(|n| s.has_diff(id, &key_n(*n)).unwrap())
        .collect();
    assert_eq!(kept, [1, 4]);

    // Under the cap nothing is dropped.
    let report = s.prune(&policy, ts("2024-01-06T00:00:00Z")).unwrap();
    assert_eq!(report.diffs, 0);
}

#[test]
fn default_policy_is_two_weeks_and_256_mib() {
    let p = PrunePolicy::default();
    assert_eq!(p.max_age, Duration::from_secs(14 * 86_400));
    assert_eq!(p.max_diff_bytes, 256 << 20);
}

#[test]
fn migrates_from_v1_keeping_data_and_old_change_details() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate(&mut conn, &MIGRATIONS[..1]).unwrap();
    assert_eq!(user_version(&conn).unwrap(), 1);
    conn.execute(
        "INSERT INTO servers (base_url) VALUES ('https://g.example')",
        [],
    )
    .unwrap();
    // A change detail as the v1 build wrote it: no revisions, messages,
    // submit requirements or votes.
    let old_detail = r#"{
        "summary": {
            "number": 7, "change_id": null, "project": "p", "branch": "main",
            "subject": "old", "status": "New",
            "owner": {"id": 1, "name": null, "email": null, "username": null,
                      "display_name": null},
            "created": "2024-01-01T00:00:00Z", "updated": "2024-01-01T00:00:00Z",
            "insertions": 0, "deletions": 0, "labels": {}, "work_in_progress": false,
            "is_private": false, "unresolved_comment_count": 0, "total_comment_count": 0,
            "topic": null, "hashtags": [], "attention_set": [], "submittable": null
        },
        "current_revision": "bbb", "current_patch_set": 2,
        "commit_message": "old", "reviewers": [], "cc": []
    }"#;
    conn.execute(
        "INSERT INTO change_details (server_id, number, json, fetched_at)
         VALUES (1, 7, ?1, '2024-01-01T00:00:00Z')",
        [old_detail],
    )
    .unwrap();

    migrate(&mut conn, MIGRATIONS).unwrap();
    assert_eq!(user_version(&conn).unwrap() as usize, MIGRATIONS.len());
    let mut s = Store { conn };

    let id = s.server_id("https://g.example").unwrap();
    assert_eq!(id, ServerId(1));
    let cached = s.load_change_detail(id, ChangeNumber(7)).unwrap().unwrap();
    assert_eq!(cached.detail.commit_message.as_deref(), Some("old"));
    assert!(cached.detail.revisions.is_empty());
    // The new tables work on the migrated database.
    s.save_diff(id, &diff_key(), &diff("x"), ts(T0)).unwrap();
    s.save_file_list(id, &files_key(), &[file("a")], None, ts(T0))
        .unwrap();
    assert!(s.has_diff(id, &diff_key()).unwrap());
    s.prune(&PrunePolicy::default(), ts(T0)).unwrap();
}

#[test]
fn deleting_a_server_cascades_to_the_review_caches() {
    let (mut s, id) = store();
    s.save_diff(id, &diff_key(), &diff("x"), ts(T0)).unwrap();
    s.save_file_list(id, &files_key(), &[file("a")], None, ts(T0))
        .unwrap();
    s.save_diff_preferences(id, &DiffPreferences::default(), ts(T0))
        .unwrap();
    s.conn.execute("DELETE FROM servers", []).unwrap();
    for table in ["diffs", "file_lists", "diff_preferences", "related_changes"] {
        let n: i64 = s
            .conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "{table}");
    }
}
