use model::Timestamp;
use rusqlite::Connection;

use super::*;
use crate::migrations::{MIGRATIONS, migrate, user_version};

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

fn keys(k: &[&str]) -> Vec<String> {
    k.iter().map(|s| (*s).to_owned()).collect()
}

fn store() -> (Store, ServerId) {
    let mut s = Store::open_in_memory().unwrap();
    let id = s.server_id("https://g.example/").unwrap();
    (s, id)
}

const T1: &str = "2024-02-01T00:00:00Z";
const T2: &str = "2024-02-02T00:00:00Z";
const T3: &str = "2024-02-03T00:00:00Z";

#[test]
fn a_key_is_new_once() {
    let (mut s, id) = store();
    assert_eq!(
        s.record_events(id, &keys(&["a", "b"]), ts(T1)).unwrap(),
        [true, true]
    );
    assert_eq!(
        s.record_events(id, &keys(&["b", "c", "a"]), ts(T2))
            .unwrap(),
        [false, true, false]
    );
    assert!(s.record_events(id, &[], ts(T2)).unwrap().is_empty());
}

#[test]
fn a_repeat_inside_one_call_is_not_new() {
    let (mut s, id) = store();
    assert_eq!(
        s.record_events(id, &keys(&["a", "a"]), ts(T1)).unwrap(),
        [true, false]
    );
}

#[test]
fn servers_have_their_own_ledger() {
    let (mut s, a) = store();
    let b = s.server_id("https://other.example/").unwrap();
    s.record_events(a, &keys(&["k"]), ts(T1)).unwrap();
    assert_eq!(s.record_events(b, &keys(&["k"]), ts(T1)).unwrap(), [true]);
}

#[test]
fn pruning_forgets_old_keys_only() {
    let (mut s, id) = store();
    s.record_events(id, &keys(&["old"]), ts(T1)).unwrap();
    s.record_events(id, &keys(&["new"]), ts(T3)).unwrap();
    // A repeat does not refresh the time of "old".
    s.record_events(id, &keys(&["old"]), ts(T3)).unwrap();
    assert_eq!(s.prune_events(ts(T2)).unwrap(), 1);
    assert_eq!(
        s.record_events(id, &keys(&["old", "new"]), ts(T3)).unwrap(),
        [true, false]
    );
    assert_eq!(s.prune_events(ts(T1)).unwrap(), 0);
}

#[test]
fn watched_since_is_set_once() {
    let (mut s, id) = store();
    assert_eq!(s.watched_since(id).unwrap(), None);
    s.mark_watched(id, ts(T1)).unwrap();
    s.mark_watched(id, ts(T2)).unwrap();
    assert_eq!(s.watched_since(id).unwrap(), Some(ts(T1)));
    let other = s.server_id("https://other.example/").unwrap();
    assert_eq!(s.watched_since(other).unwrap(), None);
    // Unknown servers are not an error.
    assert_eq!(s.watched_since(ServerId(999)).unwrap(), None);
    s.mark_watched(ServerId(999), ts(T1)).unwrap();
}

#[test]
fn deleting_a_server_removes_its_ledger_and_marker() {
    let (mut s, id) = store();
    s.record_events(id, &keys(&["k"]), ts(T1)).unwrap();
    s.mark_watched(id, ts(T1)).unwrap();
    s.delete_server(id).unwrap();
    let count: i64 = s
        .conn
        .query_row("SELECT COUNT(*) FROM notified_events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    // A server with the same URL starts unwatched again.
    let again = s.server_id("https://g.example/").unwrap();
    assert_eq!(s.watched_since(again).unwrap(), None);
}

#[test]
fn v7_adds_the_ledger_and_keeps_existing_data() {
    let mut conn = Connection::open_in_memory().unwrap();
    migrate(&mut conn, &MIGRATIONS[..6]).unwrap();
    conn.execute(
        "INSERT INTO servers (id, base_url, slug) VALUES (1, 'https://x', 'x')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO comments (server_id, change_number, json, fetched_ms) VALUES (1, 5, '[]', 0)",
        [],
    )
    .unwrap();
    migrate(&mut conn, MIGRATIONS).unwrap();
    assert_eq!(user_version(&conn).unwrap() as usize, MIGRATIONS.len());
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM servers"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM comments"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM notified_events"), 0);
    let mut s = Store { conn };
    // Existing servers start unwatched.
    assert_eq!(s.watched_since(ServerId(1)).unwrap(), None);
    assert_eq!(
        s.record_events(ServerId(1), &keys(&["k"]), ts(T1)).unwrap(),
        [true]
    );
}

fn event(change: u64, at: &str) -> Event {
    Event {
        server: ServerId(0),
        change: model::ChangeNumber(change),
        project: "p".into(),
        subject: "s".into(),
        owner: None,
        actor: None,
        at: ts(at),
        kind: model::EventKind::Merged,
        subject_account: None,
    }
}

#[test]
fn queued_events_are_pending_until_acknowledged() {
    let (mut s, id) = store();
    let (a, b) = (event(1, T2), event(2, T1));
    assert_eq!(
        s.queue_events(id, &[a.clone(), b.clone()], ts(T3)).unwrap(),
        2
    );
    // Oldest event first.
    assert_eq!(s.pending_events(id).unwrap(), [b.clone(), a.clone()]);
    s.ack_events(id, &[b.key()]).unwrap();
    assert_eq!(s.pending_events(id).unwrap(), std::slice::from_ref(&a));
    // Acknowledged events are in the ledger, so they are not queued again.
    assert_eq!(s.queue_events(id, &[a, b], ts(T3)).unwrap(), 0);
    assert_eq!(s.pending_events(id).unwrap().len(), 1);
}

#[test]
fn queueing_feeds_the_ledger_and_ignores_repeats() {
    let (mut s, id) = store();
    let e = event(1, T1);
    assert_eq!(
        s.queue_events(id, &[e.clone(), e.clone()], ts(T2)).unwrap(),
        1
    );
    assert_eq!(s.record_events(id, &[e.key()], ts(T2)).unwrap(), [false]);
}

#[test]
fn pending_events_are_per_server_and_pruned() {
    let (mut s, a) = store();
    let b = s.server_id("https://other.example/").unwrap();
    s.queue_events(a, &[event(1, T1)], ts(T1)).unwrap();
    assert!(s.pending_events(b).unwrap().is_empty());
    s.prune_events(ts(T2)).unwrap();
    assert!(s.pending_events(a).unwrap().is_empty());
}

#[test]
fn v8_adds_pending_events_to_a_v7_database() {
    let mut conn = Connection::open_in_memory().unwrap();
    migrate(&mut conn, &MIGRATIONS[..7]).unwrap();
    assert_eq!(user_version(&conn).unwrap(), 7);
    conn.execute(
        "INSERT INTO servers (id, base_url) VALUES (1, 'https://x')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO notified_events (server_id, key, seen_ms) VALUES (1, 'k', 0)",
        [],
    )
    .unwrap();
    migrate(&mut conn, MIGRATIONS).unwrap();
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM pending_events"), 0);
    assert_eq!(count("SELECT COUNT(*) FROM notified_events"), 1);
    conn.execute(
        "INSERT INTO pending_events (server_id, key, event, created_ms) VALUES (1, 'k', '{}', 0)",
        [],
    )
    .unwrap();
    // Removing the server removes its pending events.
    conn.execute_batch("PRAGMA foreign_keys = ON; DELETE FROM servers WHERE id = 1")
        .unwrap();
    assert_eq!(count("SELECT COUNT(*) FROM pending_events"), 0);
}
