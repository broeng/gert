use std::collections::BTreeMap;

use model::{
    Account, AccountId, ChangeDetail, ChangeNumber, ChangeStatus, ChangeSummary, Timestamp,
};
use rusqlite::Connection;

use super::*;
use crate::migrations::{MIGRATIONS, migrate, user_version};

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

fn change(n: u64, updated: &str) -> ChangeSummary {
    ChangeSummary {
        number: ChangeNumber(n),
        change_id: Some(format!("I{n}")),
        project: "p".into(),
        branch: "main".into(),
        subject: format!("change {n}"),
        status: ChangeStatus::New,
        owner: Account {
            id: AccountId(1),
            name: Some("Ann".into()),
            email: None,
            username: None,
            display_name: None,
        },
        created: ts("2024-01-01T00:00:00Z"),
        updated: ts(updated),
        insertions: 1,
        deletions: 2,
        labels: BTreeMap::new(),
        work_in_progress: false,
        is_private: false,
        unresolved_comment_count: 0,
        total_comment_count: 0,
        topic: None,
        hashtags: vec![],
        attention_set: vec![],
        submittable: None,
        mergeable: None,
    }
}

fn store() -> (Store, ServerId) {
    let mut s = Store::open_in_memory().unwrap();
    let id = s.server_id("https://g.example/").unwrap();
    (s, id)
}

const T1: &str = "2024-02-01T00:00:00Z";
const T2: &str = "2024-02-02T00:00:00Z";

#[test]
fn store_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Store>();
}

#[test]
fn server_id_is_stable_and_normalized() {
    let (mut s, id) = store();
    assert_eq!(s.server_id("https://g.example").unwrap(), id);
    assert_eq!(s.server_id("https://g.example//").unwrap(), id);
    assert_ne!(s.server_id("https://other.example").unwrap(), id);
}

#[test]
fn query_round_trip_preserves_order() {
    let (mut s, id) = store();
    assert_eq!(s.load_query(id, "q").unwrap(), None);
    let page = [change(3, T1), change(1, T1), change(2, T2)];
    s.save_query_page(id, "q", &page, true, ts(T2)).unwrap();
    let got = s.load_query(id, "q").unwrap().unwrap();
    assert_eq!(got.changes, page);
    assert!(got.more_changes);
    assert_eq!(got.fetched_at, ts(T2));
    assert_eq!(
        s.load_change_summary(id, ChangeNumber(1)).unwrap(),
        Some(page[1].clone())
    );
}

#[test]
fn save_replaces_previous_page() {
    let (mut s, id) = store();
    s.save_query_page(id, "q", &[change(1, T1), change(2, T1)], true, ts(T1))
        .unwrap();
    s.save_query_page(id, "q", &[change(2, T2)], false, ts(T2))
        .unwrap();
    let got = s.load_query(id, "q").unwrap().unwrap();
    assert_eq!(got.changes, [change(2, T2)]);
    assert!(!got.more_changes);
}

#[test]
fn append_extends_and_skips_duplicates() {
    let (mut s, id) = store();
    s.save_query_page(id, "q", &[change(1, T1), change(2, T1)], true, ts(T1))
        .unwrap();
    s.append_query_page(id, "q", &[change(2, T2), change(3, T1)], false, ts(T2))
        .unwrap();
    let got = s.load_query(id, "q").unwrap().unwrap();
    assert_eq!(got.changes, [change(1, T1), change(2, T2), change(3, T1)]);
    assert!(!got.more_changes);
    assert_eq!(got.fetched_at, ts(T2));
}

#[test]
fn append_without_cache_saves() {
    let (mut s, id) = store();
    s.append_query_page(id, "q", &[change(1, T1)], false, ts(T1))
        .unwrap();
    assert_eq!(s.load_query(id, "q").unwrap().unwrap().changes.len(), 1);
}

#[test]
fn touch_only_moves_fetched_at() {
    let (mut s, id) = store();
    s.touch_query(id, "q", ts(T2)).unwrap();
    assert_eq!(s.load_query(id, "q").unwrap(), None);
    s.save_query_page(id, "q", &[change(1, T1)], true, ts(T1))
        .unwrap();
    s.touch_query(id, "q", ts(T2)).unwrap();
    let got = s.load_query(id, "q").unwrap().unwrap();
    assert_eq!(got.changes, [change(1, T1)]);
    assert!(got.more_changes);
    assert_eq!(got.fetched_at, ts(T2));
}

#[test]
fn queries_and_servers_are_independent() {
    let (mut s, a) = store();
    let b = s.server_id("https://b.example").unwrap();
    s.save_query_page(a, "q1", &[change(1, T1)], false, ts(T1))
        .unwrap();
    s.save_query_page(a, "q2", &[change(2, T1)], false, ts(T1))
        .unwrap();
    s.save_query_page(b, "q1", &[change(1, T2)], false, ts(T1))
        .unwrap();
    assert_eq!(
        s.load_query(a, "q1").unwrap().unwrap().changes[0].updated,
        ts(T1)
    );
    assert_eq!(
        s.load_query(b, "q1").unwrap().unwrap().changes[0].updated,
        ts(T2)
    );
    assert_eq!(
        s.load_query(a, "q2").unwrap().unwrap().changes[0].number,
        ChangeNumber(2)
    );
    assert_eq!(s.load_query(b, "q2").unwrap(), None);
}

#[test]
fn change_detail_round_trip_updates_summary() {
    let (mut s, id) = store();
    s.save_query_page(id, "q", &[change(5, T1)], false, ts(T1))
        .unwrap();
    assert_eq!(s.load_change_detail(id, ChangeNumber(5)).unwrap(), None);
    let detail = ChangeDetail {
        summary: change(5, T2),
        current_revision: Some("abc".into()),
        current_patch_set: Some(model::PatchSetNumber(3)),
        commit_message: Some("msg".into()),
        reviewers: vec![],
        cc: vec![],
        revisions: vec![],
        messages: vec![],
        submit_requirements: vec![],
        votes: Default::default(),
        attention: vec![],
        permitted_labels: Default::default(),
        removable_reviewers: Default::default(),
        label_values: Default::default(),
        change_actions: [(
            "abandon".to_owned(),
            model::ActionInfo {
                method: Some("POST".into()),
                enabled: true,
                ..Default::default()
            },
        )]
        .into(),
        revision_actions: Default::default(),
    };
    s.save_change_detail(id, &detail, ts(T2)).unwrap();
    let got = s.load_change_detail(id, ChangeNumber(5)).unwrap().unwrap();
    assert_eq!(got.detail, detail);
    assert_eq!(got.fetched_at, ts(T2));
    // The list row sees the newer summary.
    assert_eq!(
        s.load_query(id, "q").unwrap().unwrap().changes[0].updated,
        ts(T2)
    );
}

/// Applies only the first `version` steps, like an older release would have.
#[test]
fn a_summary_without_mergeable_keeps_the_known_one_until_updated() {
    let (mut s, id) = store();
    let mergeable = |s: &Store| s.load_query(id, "q").unwrap().unwrap().changes[0].mergeable;
    let mut known = change(5, T1);
    known.mergeable = Some(false);
    s.save_query_page(id, "q", &[known], false, ts(T1)).unwrap();
    s.save_query_page(id, "q", &[change(5, T1)], false, ts(T1))
        .unwrap();
    assert_eq!(mergeable(&s), Some(false));
    let mut answered = change(5, T1);
    answered.mergeable = Some(true);
    s.save_query_page(id, "q", &[answered], false, ts(T1))
        .unwrap();
    assert_eq!(mergeable(&s), Some(true));
    // A newer change may merge differently.
    s.save_query_page(id, "q", &[change(5, T2)], false, ts(T2))
        .unwrap();
    assert_eq!(mergeable(&s), None);
}

fn db_at_version(version: usize) -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    migrate(&mut conn, &MIGRATIONS[..version]).unwrap();
    assert_eq!(user_version(&conn).unwrap() as usize, version);
    conn
}

#[test]
fn migrates_from_every_previous_version() {
    for from in 0..=MIGRATIONS.len() {
        let mut conn = db_at_version(from);
        migrate(&mut conn, MIGRATIONS).unwrap();
        assert_eq!(user_version(&conn).unwrap() as usize, MIGRATIONS.len());
        let mut s = Store { conn };
        // The migrated schema is usable.
        let id = s.server_id("https://g.example").unwrap();
        s.save_query_page(id, "q", &[change(1, T1)], false, ts(T1))
            .unwrap();
        assert!(s.load_query(id, "q").unwrap().is_some());
    }
}

#[test]
fn v3_drops_cached_diffs_and_rekeys_them_without_context() {
    let mut conn = db_at_version(2);
    conn.execute(
        "INSERT INTO servers (id, base_url) VALUES (1, 'https://x')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO diffs (server_id, change_number, base_sha, target_sha, path, whitespace,
                            context, json, size, fetched_ms)
         VALUES (1, 5, '', 'bbb', 'f', 'IGNORE_NONE', '10', '{}', 2, 0)",
        [],
    )
    .unwrap();
    migrate(&mut conn, MIGRATIONS).unwrap();
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    // A cache: the old rows are gone, the server and the rest stay.
    assert_eq!(count("SELECT COUNT(*) FROM diffs"), 0);
    assert_eq!(count("SELECT COUNT(*) FROM servers"), 1);
    assert!(conn.prepare("SELECT context FROM diffs").is_err());
    // One row per comparison and whitespace option now.
    let insert = "INSERT INTO diffs (server_id, change_number, base_sha, target_sha, path,
                      whitespace, json, size, fetched_ms) VALUES (1, 5, '', 'bbb', 'f', 'W', '{}', 2, 0)";
    conn.execute(insert, []).unwrap();
    assert!(conn.execute(insert, []).is_err());
}

#[test]
fn migration_preserves_existing_data() {
    // Data written at the latest-but-one version survives a further step.
    let mut conn = db_at_version(MIGRATIONS.len());
    conn.execute("INSERT INTO servers (base_url) VALUES ('https://x')", [])
        .unwrap();
    let extra = "CREATE TABLE outbox_probe (id INTEGER PRIMARY KEY)";
    let mut steps: Vec<&str> = MIGRATIONS.to_vec();
    steps.push(extra);
    migrate(&mut conn, &steps).unwrap();
    assert_eq!(user_version(&conn).unwrap() as usize, steps.len());
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM servers", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn failed_migration_rolls_back() {
    let mut conn = db_at_version(0);
    let steps = ["CREATE TABLE a (x)", "THIS IS NOT SQL"];
    assert!(migrate(&mut conn, &steps).is_err());
    assert_eq!(user_version(&conn).unwrap(), 0);
    assert!(conn.prepare("SELECT * FROM a").is_err());
}

#[test]
fn newer_database_is_rejected_untouched() {
    let dir = std::env::temp_dir().join(format!("store-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("cache.db");
    let future = MIGRATIONS.len() + 1;
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(&format!("PRAGMA user_version = {future}"))
            .unwrap();
    }
    match Store::open(&path) {
        Err(Error::SchemaTooNew { found, supported }) => {
            assert_eq!(found as usize, future);
            assert_eq!(supported as usize, MIGRATIONS.len());
        }
        other => panic!("expected SchemaTooNew, got {:?}", other.map(|_| ())),
    }
    let conn = Connection::open(&path).unwrap();
    assert_eq!(user_version(&conn).unwrap() as usize, future);
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "delete", "journal mode must be left alone");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn file_database_uses_wal_and_reopens() {
    let dir = std::env::temp_dir().join(format!("store-wal-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("cache.db");
    let id = {
        let mut s = Store::open(&path).unwrap();
        let id = s.server_id("https://g.example").unwrap();
        s.save_query_page(id, "q", &[change(1, T1)], false, ts(T1))
            .unwrap();
        let mode: String = s
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        id
    };
    let s = Store::open(&path).unwrap();
    assert_eq!(s.load_query(id, "q").unwrap().unwrap().changes.len(), 1);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn v4_adds_comments_and_drafts_and_keeps_existing_data() {
    let mut conn = db_at_version(3);
    conn.execute(
        "INSERT INTO servers (id, base_url) VALUES (1, 'https://x')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO file_lists (server_id, change_number, base_sha, target_sha, files_json,
                                 reviewed_json, fetched_ms)
         VALUES (1, 5, '', 'bbb', '[]', '[\"f\"]', 0)",
        [],
    )
    .unwrap();
    migrate(&mut conn, MIGRATIONS).unwrap();
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM file_lists"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM comments"), 0);
    assert_eq!(count("SELECT COUNT(*) FROM drafts"), 0);
    // Drafts get local ids, defaults for the bookkeeping columns, and a
    // server draft id is unique within a change but may be NULL repeatedly.
    let insert = "INSERT INTO drafts (server_id, change_number, patch_set, revision_sha,
                      server_draft_id, path, side, message, unresolved, updated_ms, state)
                  VALUES (1, 5, 1, 'aaa', ?1, 'f', 'REVISION', 'm', 1, 0, 'synced')";
    conn.execute(insert, [None::<String>]).unwrap();
    conn.execute(insert, [None::<String>]).unwrap();
    conn.execute(insert, ["srv1"]).unwrap();
    assert!(conn.execute(insert, ["srv1"]).is_err());
    assert_eq!(count("SELECT MAX(attempts) + MAX(version) FROM drafts"), 0);
    // Removing the server removes its drafts and comments.
    conn.execute("DELETE FROM servers", []).unwrap();
    assert_eq!(count("SELECT COUNT(*) FROM drafts"), 0);
}

// ---- several servers in one database ----

/// Every table that has a `server_id` column: what a purge must empty.
fn server_tables(conn: &Connection) -> Vec<String> {
    let names: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    names
        .into_iter()
        .filter(|t| {
            conn.prepare(&format!("PRAGMA table_info({t})"))
                .unwrap()
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .any(|c| c.unwrap() == "server_id")
        })
        .collect()
}

fn rows_of(conn: &Connection, table: &str, server: ServerId) -> i64 {
    conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE server_id = ?1"),
        [server.0],
        |r| r.get(0),
    )
    .unwrap()
}

fn ps(n: u32, sha: &str) -> model::PatchSetRef {
    model::PatchSetRef {
        number: model::PatchSetNumber(n),
        sha: sha.into(),
    }
}

fn empty_diff() -> model::Diff {
    model::Diff {
        meta_a: None,
        meta_b: None,
        change_type: None,
        intraline_status: None,
        diff_header: vec![],
        chunks: vec![],
        binary: false,
    }
}

/// Fills every per-server table for change 7, the way a session would.
/// `tag` makes the content differ between servers.
fn fill_server(s: &mut Store, id: ServerId, tag: &str) {
    let files_key = model::FilesKey {
        change: ChangeNumber(7),
        base: None,
        target: ps(2, "bbb"),
    };
    let diff_key = model::DiffKey {
        change: ChangeNumber(7),
        base: None,
        target: ps(2, "bbb"),
        path: "f".into(),
        whitespace: model::Whitespace::IgnoreNone,
    };
    let mut c = change(7, T1);
    c.subject = tag.to_owned();
    s.save_query_page(id, "q", &[c.clone()], false, ts(T1))
        .unwrap();
    let detail = ChangeDetail {
        summary: c,
        current_revision: Some("bbb".into()),
        current_patch_set: Some(model::PatchSetNumber(2)),
        commit_message: Some(tag.to_owned()),
        reviewers: vec![],
        cc: vec![],
        revisions: vec![],
        messages: vec![],
        submit_requirements: vec![],
        votes: Default::default(),
        attention: vec![],
        permitted_labels: Default::default(),
        removable_reviewers: Default::default(),
        label_values: Default::default(),
        change_actions: Default::default(),
        revision_actions: Default::default(),
    };
    s.save_change_detail(id, &detail, ts(T1)).unwrap();
    let file = model::FileEntry {
        path: tag.to_owned(),
        status: model::FileStatus::Modified,
        old_path: None,
        lines_inserted: 1,
        lines_deleted: 0,
        size_delta: 0,
        size: 1,
        binary: false,
    };
    s.save_file_list(id, &files_key, &[file], Some(&[tag.to_owned()]), ts(T1))
        .unwrap();
    s.save_diff(id, &diff_key, &empty_diff(), ts(T1)).unwrap();
    s.save_comments(id, ChangeNumber(7), &[], ts(T1)).unwrap();
    s.save_related(id, ChangeNumber(7), &"bbb".into(), &[], ts(T1))
        .unwrap();
    s.save_diff_preferences(id, &model::DiffPreferences::default(), ts(T1))
        .unwrap();
    s.create_draft(
        id,
        ChangeNumber(7),
        &ps(2, "bbb"),
        &model::DraftInput {
            path: "f".into(),
            side: model::Side::Revision,
            parent: None,
            location: model::Location::Line(1),
            in_reply_to: None,
            message: tag.to_owned(),
            unresolved: true,
        },
        ts(T1),
    )
    .unwrap();
}

#[test]
fn two_servers_with_the_same_numbers_and_queries_do_not_mix() {
    let (mut s, a) = store();
    let b = s.server_id("https://b.example").unwrap();
    fill_server(&mut s, a, "from-a");
    fill_server(&mut s, b, "from-b");

    for (id, tag) in [(a, "from-a"), (b, "from-b")] {
        let n = ChangeNumber(7);
        assert_eq!(
            s.load_query(id, "q").unwrap().unwrap().changes[0].subject,
            tag
        );
        assert_eq!(s.load_change_summary(id, n).unwrap().unwrap().subject, tag);
        assert_eq!(
            s.load_change_detail(id, n)
                .unwrap()
                .unwrap()
                .detail
                .commit_message,
            Some(tag.to_owned())
        );
        let files_key = model::FilesKey {
            change: n,
            base: None,
            target: ps(2, "bbb"),
        };
        let list = s.load_file_list(id, &files_key).unwrap().unwrap();
        assert_eq!(list.files[0].path, tag);
        assert_eq!(list.reviewed, Some(vec![tag.to_owned()]));
        let drafts = s.load_drafts(id, n).unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].message, tag);
        assert_eq!(s.pending_drafts(id).unwrap().len(), 1);
    }

    // A reviewed mark on one server leaves the other's alone.
    let n = ChangeNumber(7);
    s.set_reviewed_path(a, n, &"bbb".into(), "extra", true)
        .unwrap();
    let files_key = model::FilesKey {
        change: n,
        base: None,
        target: ps(2, "bbb"),
    };
    assert_eq!(
        s.load_file_list(a, &files_key).unwrap().unwrap().reviewed,
        Some(vec!["from-a".to_owned(), "extra".to_owned()])
    );
    assert_eq!(
        s.load_file_list(b, &files_key).unwrap().unwrap().reviewed,
        Some(vec!["from-b".to_owned()])
    );
}

#[test]
fn delete_server_purges_only_that_server() {
    let (mut s, a) = store();
    let b = s.server_id("https://b.example").unwrap();
    fill_server(&mut s, a, "from-a");
    fill_server(&mut s, b, "from-b");
    let tables = server_tables(&s.conn);
    // The test would silently cover nothing if the schema probe broke.
    for t in [
        "changes",
        "query_results",
        "query_result_items",
        "change_details",
        "file_lists",
        "diffs",
        "related_changes",
        "diff_preferences",
        "comments",
        "drafts",
    ] {
        assert!(tables.iter().any(|x| x == t), "{t} has no server_id");
        assert!(rows_of(&s.conn, t, a) > 0, "{t} was not filled for a");
        assert!(rows_of(&s.conn, t, b) > 0, "{t} was not filled for b");
    }
    let before: Vec<i64> = tables.iter().map(|t| rows_of(&s.conn, t, b)).collect();

    s.delete_server(a).unwrap();

    for t in &tables {
        assert_eq!(rows_of(&s.conn, t, a), 0, "{t} kept rows of the server");
    }
    let after: Vec<i64> = tables.iter().map(|t| rows_of(&s.conn, t, b)).collect();
    assert_eq!(before, after, "the other server lost rows");
    assert!(s.load_query(b, "q").unwrap().is_some());
    // Deleting twice, or an unknown id, is harmless.
    s.delete_server(a).unwrap();
    s.delete_server(ServerId(9999)).unwrap();
}

#[test]
fn delete_server_by_slug_purges_the_slugged_row_only() {
    let (mut s, a) = store();
    let work = s.server_for_slug("work", "https://w.example").unwrap();
    let other = s.server_for_slug("other", "https://w.example").unwrap();
    fill_server(&mut s, work, "from-work");
    fill_server(&mut s, other, "from-other");
    fill_server(&mut s, a, "from-a");
    assert!(s.delete_server_by_slug("work").unwrap());
    assert!(s.load_query(work, "q").unwrap().is_none());
    assert!(s.load_query(other, "q").unwrap().is_some());
    assert!(s.load_query(a, "q").unwrap().is_some());
    assert!(!s.delete_server_by_slug("work").unwrap());
}

#[test]
fn set_server_url_keeps_the_id_and_the_cache() {
    let (mut s, a) = store();
    let b = s.server_id("https://b.example").unwrap();
    fill_server(&mut s, a, "from-a");
    s.set_server_url(a, "https://moved.example/").unwrap();
    assert_eq!(s.server_id("https://moved.example").unwrap(), a);
    assert!(s.load_query(a, "q").unwrap().is_some());
    // The same URL again is fine; another row's URL is not.
    s.set_server_url(a, "https://moved.example").unwrap();
    assert!(matches!(
        s.set_server_url(a, "https://b.example"),
        Err(Error::ServerUrlTaken(_))
    ));
    assert!(matches!(
        s.set_server_url(ServerId(9999), "https://nowhere.example"),
        Err(Error::NoSuchServer)
    ));
    // Nothing changed by the failures.
    assert_eq!(s.server_id("https://b.example").unwrap(), b);
}

#[test]
fn app_state_round_trip() {
    let (mut s, _) = store();
    assert_eq!(s.app_state("active_server").unwrap(), None);
    s.set_app_state("active_server", "work").unwrap();
    s.set_app_state("other", "x").unwrap();
    s.set_app_state("active_server", "home").unwrap();
    assert_eq!(
        s.app_state("active_server").unwrap().as_deref(),
        Some("home")
    );
    assert_eq!(s.app_state("other").unwrap().as_deref(), Some("x"));
}

#[test]
fn v5_adds_app_state_and_keeps_existing_data() {
    let mut conn = db_at_version(4);
    conn.execute(
        "INSERT INTO servers (id, base_url) VALUES (1, 'https://x')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO comments (server_id, change_number, json, fetched_ms) VALUES (1, 5, '[]', 0)",
        [],
    )
    .unwrap();
    migrate(&mut conn, MIGRATIONS).unwrap();
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM servers"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM comments"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM app_state"), 0);
    conn.execute("INSERT INTO app_state (key, value) VALUES ('k', 'v')", [])
        .unwrap();
}

#[test]
fn v6_rebuilds_servers_without_losing_the_cache() {
    let mut conn = db_at_version(5);
    // The app opens with foreign keys on; a naive rebuild would cascade.
    conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
    conn.execute(
        "INSERT INTO servers (id, base_url) VALUES (3, 'https://x'), (4, 'https://y')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO comments (server_id, change_number, json, fetched_ms)
         VALUES (3, 5, '[]', 0), (4, 5, '[]', 0)",
        [],
    )
    .unwrap();
    migrate(&mut conn, MIGRATIONS).unwrap();
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM servers"), 2);
    assert_eq!(count("SELECT COUNT(*) FROM comments"), 2);
    assert_eq!(count("SELECT COUNT(*) FROM servers WHERE slug IS NULL"), 2);
    // Ids survive, and foreign keys are back on and point at the new table.
    assert_eq!(count("PRAGMA foreign_keys"), 1);
    assert_eq!(
        count("SELECT base_url = 'https://y' FROM servers WHERE id = 4"),
        1
    );
    assert!(
        conn.execute(
            "INSERT INTO comments (server_id, change_number, json, fetched_ms)
             VALUES (99, 1, '[]', 0)",
            [],
        )
        .is_err()
    );
    conn.execute("DELETE FROM servers WHERE id = 3", [])
        .unwrap();
    assert_eq!(count("SELECT COUNT(*) FROM comments"), 1);
    // A URL may be shared now; a slug may not.
    conn.execute(
        "INSERT INTO servers (base_url, slug) VALUES ('https://y', 'a'), ('https://y', 'b')",
        [],
    )
    .unwrap();
    assert!(
        conn.execute(
            "INSERT INTO servers (base_url, slug) VALUES ('https://z', 'a')",
            []
        )
        .is_err()
    );
}

#[test]
fn v6_from_every_older_version_keeps_servers() {
    for from in 1..MIGRATIONS.len() {
        let mut conn = db_at_version(from);
        conn.execute("INSERT INTO servers (base_url) VALUES ('https://x')", [])
            .unwrap();
        migrate(&mut conn, MIGRATIONS).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM servers", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "from v{from}");
    }
}

#[test]
fn slugs_give_servers_with_the_same_url_their_own_rows() {
    let mut s = Store::open_in_memory().unwrap();
    let a = s.server_for_slug("work", "https://g.example/").unwrap();
    let b = s.server_for_slug("work-bot", "https://g.example").unwrap();
    assert_ne!(a, b);
    assert_eq!(s.server_for_slug("work", "https://g.example").unwrap(), a);
    assert_eq!(
        s.server_for_slug("work-bot", "https://g.example").unwrap(),
        b
    );
    fill_server(&mut s, a, "from-a");
    fill_server(&mut s, b, "from-b");
    assert_eq!(
        s.load_query(a, "q").unwrap().unwrap().changes[0].subject,
        "from-a"
    );
    assert_eq!(
        s.load_query(b, "q").unwrap().unwrap().changes[0].subject,
        "from-b"
    );
    s.delete_server(b).unwrap();
    assert!(s.load_query(a, "q").unwrap().is_some());
    assert_eq!(s.load_query(b, "q").unwrap(), None);
}

#[test]
fn the_first_slug_adopts_an_existing_cache() {
    let (mut s, old) = store();
    s.save_query_page(old, "q", &[change(1, T1)], false, ts(T1))
        .unwrap();
    // Only the first entry with the URL adopts the unslugged rows.
    let first = s.server_for_slug("one", "https://g.example").unwrap();
    assert_eq!(first, old);
    assert!(s.load_query(first, "q").unwrap().is_some());
    let second = s.server_for_slug("two", "https://g.example").unwrap();
    assert_ne!(second, old);
    assert_eq!(s.load_query(second, "q").unwrap(), None);
    // A login by URL after that finds the oldest claimed row, and one that
    // is new stays unclaimed for the entry the login creates.
    assert_eq!(s.server_id("https://g.example").unwrap(), first);
    let fresh = s.server_id("https://new.example").unwrap();
    assert_eq!(
        s.server_for_slug("new", "https://new.example").unwrap(),
        fresh
    );
}

#[test]
fn a_slug_follows_an_edited_url() {
    let mut s = Store::open_in_memory().unwrap();
    let a = s.server_for_slug("work", "https://old.example").unwrap();
    s.save_query_page(a, "q", &[change(1, T1)], false, ts(T1))
        .unwrap();
    assert_eq!(s.server_for_slug("work", "https://new.example").unwrap(), a);
    assert!(s.load_query(a, "q").unwrap().is_some());
    assert_eq!(s.server_id("https://new.example").unwrap(), a);
}

// ---- v11: local changes ----

const INSERT_LOCAL_CHANGE: &str = "INSERT INTO local_changes (server_id, project, branch,
        change_id, subject, author_name, author_email, updated_ms, state, first_seen_ms)
    VALUES (?1, 'p', ?2, ?3, 's', 'A', 'a@x', 0, 'local', 0)";

#[test]
fn v11_adds_the_local_tables_from_every_older_version() {
    for from in 1..MIGRATIONS.len() {
        let mut conn = db_at_version(from);
        conn.execute(
            "INSERT INTO servers (id, base_url) VALUES (1, 'https://x')",
            [],
        )
        .unwrap();
        let had_drafts = from >= 4;
        if had_drafts {
            conn.execute(
                "INSERT INTO drafts (server_id, change_number, patch_set, revision_sha, path,
                     side, message, unresolved, updated_ms, state)
                 VALUES (1, 5, 1, 'aaa', 'f', 'REVISION', 'm', 1, 0, 'synced')",
                [],
            )
            .unwrap();
        }
        migrate(&mut conn, MIGRATIONS).unwrap();
        assert_eq!(user_version(&conn).unwrap() as usize, MIGRATIONS.len());
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(count("SELECT COUNT(*) FROM servers"), 1, "from v{from}");
        assert_eq!(
            count("SELECT COUNT(*) FROM drafts WHERE pending_review_id IS NULL"),
            i64::from(had_drafts),
            "from v{from}"
        );
        for t in ["local_changes", "local_patch_sets", "pending_reviews"] {
            assert_eq!(count(&format!("SELECT COUNT(*) FROM {t}")), 0, "{t}");
        }
        // The existing draft is readable through the store.
        let s = Store { conn };
        if had_drafts {
            let drafts = s.load_drafts(ServerId(1), ChangeNumber(5)).unwrap();
            assert_eq!(drafts.len(), 1);
            assert_eq!(drafts[0].pending_review, None);
        }
    }
}

#[test]
fn v11_local_change_key_has_one_row_per_unknown_branch() {
    let conn = db_at_version(MIGRATIONS.len());
    conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
    conn.execute(
        "INSERT INTO servers (id, base_url) VALUES (1, 'https://x'), (2, 'https://y')",
        [],
    )
    .unwrap();
    let insert = |conn: &Connection, server: i64, branch: Option<&str>, change_id: &str| {
        conn.execute(
            INSERT_LOCAL_CHANGE,
            rusqlite::params![server, branch, change_id],
        )
    };
    insert(&conn, 1, None, "I1").unwrap();
    // A second row without a branch for the same Change-Id is refused,
    // although UNIQUE alone would let NULLs through.
    assert!(insert(&conn, 1, None, "I1").is_err());
    insert(&conn, 1, Some("main"), "I1").unwrap();
    assert!(insert(&conn, 1, Some("main"), "I1").is_err());
    insert(&conn, 1, Some("stable"), "I1").unwrap();
    insert(&conn, 1, None, "I2").unwrap();
    insert(&conn, 2, None, "I1").unwrap();
    // Unknown servers are refused, and removing one removes its rows.
    assert!(insert(&conn, 9, None, "I1").is_err());
    conn.execute("DELETE FROM servers WHERE id = 1", [])
        .unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM local_changes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn v11_cascades_and_unseals() {
    let conn = db_at_version(11);
    conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
    conn.execute(
        "INSERT INTO servers (id, base_url) VALUES (1, 'https://x')",
        [],
    )
    .unwrap();
    conn.execute(INSERT_LOCAL_CHANGE, rusqlite::params![1, "main", "I1"])
        .unwrap();
    conn.execute_batch(
        "INSERT INTO local_patch_sets (local_change_id, server_id, sha, seen_ms, ordinal)
             VALUES (1, 1, 'aaa', 0, 1);
         INSERT INTO pending_reviews (server_id, local_change_id, change_number, patch_set,
                 given_on_sha, labels, strategy, state, created_ms)
             VALUES (1, 1, 5, 6, 'aaa', '{}', 'exact', 'pending', 0),
                    (1, NULL, 5, 6, 'aaa', '{}', 'exact', 'pending', 0);
         INSERT INTO drafts (server_id, change_number, patch_set, revision_sha, path, side,
                 message, unresolved, updated_ms, state, pending_review_id)
             VALUES (1, 5, 6, 'aaa', '/PATCHSET_LEVEL', 'REVISION', 'm', 0, 0,
                 'pending_create', 2);",
    )
    .unwrap();
    // One message draft per review.
    assert!(
        conn.execute(
            "INSERT INTO drafts (server_id, change_number, patch_set, revision_sha, path, side,
                 message, unresolved, updated_ms, state, pending_review_id)
             VALUES (1, 5, 6, 'aaa', '/PATCHSET_LEVEL', 'REVISION', 'm', 0, 0,
                 'pending_create', 2)",
            [],
        )
        .is_err()
    );
    let count =
        |conn: &Connection, sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    conn.execute("DELETE FROM local_changes", []).unwrap();
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM local_patch_sets"), 0);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM pending_reviews"), 1);
    // Removing a review unseals its message draft rather than dangling.
    conn.execute("DELETE FROM pending_reviews", []).unwrap();
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM drafts WHERE pending_review_id IS NULL"
        ),
        1
    );
}

// ---- v12: reserved and held drafts, pending reviews rebuilt ----

#[test]
fn v12_from_every_older_version_keeps_drafts_ordinary() {
    for from in 1..MIGRATIONS.len() {
        let mut conn = db_at_version(from);
        conn.execute(
            "INSERT INTO servers (id, base_url) VALUES (1, 'https://x')",
            [],
        )
        .unwrap();
        if from >= 4 {
            conn.execute(
                "INSERT INTO drafts (server_id, change_number, patch_set, revision_sha, path,
                     side, message, unresolved, updated_ms, state)
                 VALUES (1, 5, 1, 'aaa', 'f', 'REVISION', 'm', 1, 0, 'pending_create')",
                [],
            )
            .unwrap();
        }
        migrate(&mut conn, MIGRATIONS).unwrap();
        assert_eq!(user_version(&conn).unwrap() as usize, MIGRATIONS.len());
        let s = Store { conn };
        // An existing draft is still uploaded as before.
        assert_eq!(
            s.pending_drafts(ServerId(1)).unwrap().len(),
            usize::from(from >= 4),
            "from v{from}"
        );
        if let Some(d) = s.pending_drafts(ServerId(1)).unwrap().first() {
            assert!(d.is_ordinary());
            assert_eq!((d.reserved_by, d.held_reason.clone()), (None, None));
        }
        assert!(
            s.pending_reviews(ServerId(1), ChangeNumber(5))
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn v12_rebuilds_pending_reviews_with_an_owner_and_keeps_the_counter() {
    let mut conn = db_at_version(11);
    conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
    let ps1 = model::PatchSetNumber::local(1).unwrap().0;
    let ps9 = model::PatchSetNumber::local(9).unwrap().0;
    conn.execute(
        "INSERT INTO servers (id, base_url) VALUES (1, 'https://x')",
        [],
    )
    .unwrap();
    conn.execute(INSERT_LOCAL_CHANGE, rusqlite::params![1, "main", "I1"])
        .unwrap();
    conn.execute_batch(
        "INSERT INTO local_patch_sets (local_change_id, server_id, sha, seen_ms, ordinal)
             VALUES (1, 1, 'aaa', 0, 1);",
    )
    .unwrap();
    let insert_review = |owner: Option<i64>, ps: u32| {
        conn.execute(
            "INSERT INTO pending_reviews (server_id, local_change_id, change_number, patch_set,
                 given_on_sha, labels, strategy, state, held_reason, created_ms, nonce)
             VALUES (1, ?1, 5, ?2, 'aaa', '{\"Code-Review\":2}', 'follow', 'held', 'why',
                 7, 'n')",
            rusqlite::params![owner, ps],
        )
        .unwrap();
        conn.last_insert_rowid()
    };
    let with_owner = insert_review(Some(1), ps1);
    let derived = insert_review(None, ps1);
    let orphan = insert_review(None, ps9);
    let removed = insert_review(Some(1), ps1);
    conn.execute("DELETE FROM pending_reviews WHERE id = ?1", [removed])
        .unwrap();
    conn.execute(
        "INSERT INTO drafts (server_id, change_number, patch_set, revision_sha, path, side,
             message, unresolved, updated_ms, state, pending_review_id)
         VALUES (1, 5, ?1, 'aaa', '/PATCHSET_LEVEL', 'REVISION', 'm', 0, 0, 'pending_create',
             ?2), (1, 5, ?1, 'aaa', '/PATCHSET_LEVEL', 'REVISION', 'o', 0, 0,
             'pending_create', ?3)",
        rusqlite::params![ps1, derived, orphan],
    )
    .unwrap();

    migrate(&mut conn, MIGRATIONS).unwrap();
    assert_eq!(user_version(&conn).unwrap() as usize, MIGRATIONS.len());
    let count =
        |conn: &Connection, sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count(&conn, "PRAGMA foreign_keys"), 1);
    // local_change_id is required now.
    assert!(
        conn.execute(
            "INSERT INTO pending_reviews (server_id, change_number, patch_set, revision_sha,
                 given_on_sha, labels, strategy, state, created_ms, voted_at_ms)
             VALUES (1, 5, 1, 'a', 'a', '{}', 'exact', 'pending', 0, 0)",
            [],
        )
        .is_err()
    );
    let s = Store { conn };
    let server = ServerId(1);
    let reviews = s.pending_reviews(server, ChangeNumber(5)).unwrap();
    let ids: Vec<i64> = reviews.iter().map(|r| r.id.0).collect();
    assert_eq!(ids, [with_owner, derived]);
    for r in &reviews {
        assert_eq!(r.local_change, model::LocalChangeId(1));
        assert_eq!(
            (r.revision.as_str(), r.given_on_sha.as_str()),
            ("aaa", "aaa")
        );
        assert_eq!((r.created, r.voted_at), (r.created, r.created));
        assert_eq!(r.held_reason.as_deref(), Some("why"));
        assert_eq!(r.nonce.as_deref(), Some("n"));
    }
    assert_eq!(reviews[1].message.as_deref(), Some("m"));
    // The orphan's message draft was unsealed, not left dangling.
    let orphan_msg = s
        .load_drafts(server, ChangeNumber(5))
        .unwrap()
        .into_iter()
        .find(|d| d.message == "o")
        .unwrap();
    assert_eq!(orphan_msg.pending_review, None);
    // Ids above the highest ever handed out, also the deleted one.
    let mut s = s;
    let next = s
        .create_pending_review(
            server,
            &NewPendingReview {
                patch_set: model::PatchSetNumber(ps1),
                labels: Default::default(),
                strategy: model::LocalApproval::Exact,
                message: String::new(),
                unresolved: false,
                created: ts(T1),
                replaces_draft: None,
            },
        )
        .unwrap();
    assert!(next.id.0 > removed, "{} <= {removed}", next.id.0);
    // Removing the local change still takes its reviews along.
    s.conn
        .execute("DELETE FROM local_changes WHERE id = 1", [])
        .unwrap();
    assert!(
        s.pending_reviews(server, ChangeNumber(5))
            .unwrap()
            .is_empty()
    );
}

// ---- v13: consolidation posts claim their drafts ----

#[test]
fn v13_from_every_older_version_leaves_no_post_claimed() {
    for from in 1..MIGRATIONS.len() {
        let mut conn = db_at_version(from);
        conn.execute(
            "INSERT INTO servers (id, base_url) VALUES (1, 'https://x')",
            [],
        )
        .unwrap();
        if from >= 4 {
            conn.execute(
                "INSERT INTO drafts (server_id, change_number, patch_set, revision_sha, path,
                     side, message, unresolved, updated_ms, state)
                 VALUES (1, 5, 1, 'aaa', 'f', 'REVISION', 'm', 1, 0, 'pending_create')",
                [],
            )
            .unwrap();
        }
        migrate(&mut conn, MIGRATIONS).unwrap();
        assert_eq!(user_version(&conn).unwrap() as usize, MIGRATIONS.len());
        let claimed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM drafts WHERE posting_nonce IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(claimed, 0, "from v{from}");
        let s = Store { conn };
        assert!(s.unfinished_local_posts(ServerId(1)).unwrap().is_empty());
    }
}

#[test]
fn v13_keeps_a_reserved_draft_claimable() {
    let mut conn = db_at_version(12);
    conn.execute(
        "INSERT INTO servers (id, base_url) VALUES (1, 'https://x')",
        [],
    )
    .unwrap();
    conn.execute(INSERT_LOCAL_CHANGE, rusqlite::params![1, "main", "I1"])
        .unwrap();
    conn.execute(
        "INSERT INTO drafts (server_id, change_number, patch_set, revision_sha, path, side,
             message, unresolved, updated_ms, state, reserved_by)
         VALUES (1, 42, 1, 'aaa', 'f', 'REVISION', 'm', 1, 0, 'pending_create', 1)",
        [],
    )
    .unwrap();
    migrate(&mut conn, MIGRATIONS).unwrap();
    let mut s = Store { conn };
    let post = s
        .claim_local_post(
            ServerId(1),
            model::LocalChangeId(1),
            None,
            model::PatchSetNumber(1),
            "n1",
            &crate::Claimant {
                owner: "run",
                at: model::Timestamp::UNIX_EPOCH,
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(post.drafts.len(), 1);
    assert_eq!(
        s.unfinished_local_posts(ServerId(1)).unwrap(),
        [(model::LocalChangeId(1), "n1".to_owned())]
    );
}

// ---- v16: consolidation leases, claims, held markers ----

#[test]
fn v16_from_every_older_version_starts_without_leases_or_claims() {
    for from in 1..MIGRATIONS.len() {
        let mut conn = db_at_version(from);
        migrate(&mut conn, MIGRATIONS).unwrap();
        assert_eq!(user_version(&conn).unwrap() as usize, MIGRATIONS.len());
        for table in ["consolidation_leases", "local_post_claims"] {
            let n: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{table} from v{from}");
        }
        conn.query_row("SELECT COUNT(drop_only) FROM pending_reviews", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap();
        conn.query_row("SELECT COUNT(checked_ms) FROM local_changes", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap();
    }
}

// ---- v17: versions on a named branch ----

#[test]
fn v17_counts_existing_versions_as_on_a_branch() {
    let mut conn = db_at_version(16);
    conn.execute_batch(
        "INSERT INTO servers (id, base_url) VALUES (1, 'https://x');
         INSERT INTO local_changes (id, server_id, project, branch, change_id, subject,
             author_name, author_email, updated_ms, state, first_seen_ms)
         VALUES (1, 1, 'p', 'main', 'I1', 's', 'a', 'a@x', 0, 'local', 0);
         INSERT INTO local_patch_sets (local_change_id, server_id, sha, seen_ms, ordinal)
         VALUES (1, 1, 'abc', 0, 1);",
    )
    .unwrap();
    migrate(&mut conn, MIGRATIONS).unwrap();
    let s = Store { conn };
    let versions = s
        .local_patch_sets(ServerId(1), model::LocalChangeId(1))
        .unwrap();
    assert_eq!(versions.len(), 1);
    assert!(versions[0].on_branch);
}
