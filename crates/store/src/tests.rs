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
