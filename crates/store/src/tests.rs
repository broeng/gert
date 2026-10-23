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
