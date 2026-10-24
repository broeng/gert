use model::{ChangeNumber, Event, EventKind, Timestamp};
use rusqlite::Connection;

use super::*;
use crate::migrations::{MIGRATIONS, migrate, user_version};

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

fn store() -> (Store, ServerId) {
    let mut s = Store::open_in_memory().unwrap();
    let id = s.server_id("https://g.example/").unwrap();
    (s, id)
}

const T1: &str = "2024-02-01T00:00:00Z";
const T2: &str = "2024-02-02T00:00:00Z";
const T3: &str = "2024-02-03T00:00:00Z";
const NOW: &str = "2024-02-04T00:00:00Z";

fn item(server: ServerId, change: u64, at: &str) -> NewInboxItem {
    NewInboxItem {
        event: Event {
            server,
            change: ChangeNumber(change),
            project: "p".into(),
            subject: "s".into(),
            owner: None,
            actor: None,
            at: ts(at),
            kind: EventKind::Merged,
            subject_account: None,
        },
        rule_title: "Merged".into(),
        mention: false,
        shown_on_os: true,
    }
}

fn changes(items: &[InboxItem]) -> Vec<u64> {
    items.iter().map(|i| i.event.change.0).collect()
}

#[test]
fn items_come_back_newest_first_with_their_data() {
    let (mut s, id) = store();
    let mut quiet = item(id, 2, T3);
    quiet.shown_on_os = false;
    quiet.mention = true;
    let added = s
        .add_inbox(
            id,
            &[item(id, 1, T1), quiet.clone(), item(id, 3, T2)],
            ts(NOW),
        )
        .unwrap();
    assert_eq!(added, 3);
    let items = s.inbox_items(None, 10).unwrap();
    assert_eq!(changes(&items), [2, 3, 1]);
    assert_eq!(items[0].event, quiet.event);
    assert_eq!(items[0].created_at, ts(T3));
    assert_eq!(items[0].rule_title, "Merged");
    assert!(items[0].mention && !items[0].shown_on_os);
    assert_eq!(items[0].read_at, None);
    assert_eq!(changes(&s.inbox_items(None, 2).unwrap()), [2, 3]);
}

#[test]
fn a_replay_adds_nothing_and_keeps_the_read_state() {
    let (mut s, id) = store();
    let a = item(id, 1, T1);
    assert_eq!(
        s.add_inbox(id, &[a.clone(), a.clone()], ts(NOW)).unwrap(),
        1
    );
    let first = s.inbox_items(None, 10).unwrap()[0].id;
    s.mark_inbox_read(&[first], ts(NOW)).unwrap();
    assert_eq!(s.add_inbox(id, &[a], ts(NOW)).unwrap(), 0);
    let items = s.inbox_items(None, 10).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].read_at, Some(ts(NOW)));
}

#[test]
fn read_and_unread_marks_and_counts_follow_each_server() {
    let (mut s, a) = store();
    let b = s.server_id("https://other.example/").unwrap();
    s.add_inbox(a, &[item(a, 1, T1), item(a, 2, T2)], ts(NOW))
        .unwrap();
    s.add_inbox(b, &[item(b, 1, T1)], ts(NOW)).unwrap();
    let counts = s.inbox_unread_counts().unwrap();
    assert_eq!((counts[&a], counts[&b]), (2, 1));

    let items = s.inbox_items(Some(&[a]), 10).unwrap();
    assert_eq!(items.len(), 2);
    s.mark_inbox_read(&[items[0].id], ts(NOW)).unwrap();
    assert_eq!(s.inbox_unread_counts().unwrap()[&a], 1);
    s.mark_inbox_unread(&[items[0].id]).unwrap();
    assert_eq!(s.inbox_unread_counts().unwrap()[&a], 2);

    assert_eq!(s.mark_all_inbox_read(Some(a), ts(NOW)).unwrap(), 2);
    let counts = s.inbox_unread_counts().unwrap();
    assert!(!counts.contains_key(&a));
    assert_eq!(counts[&b], 1);
    assert_eq!(s.mark_all_inbox_read(None, ts(NOW)).unwrap(), 1);
    assert!(s.inbox_unread_counts().unwrap().is_empty());
}

#[test]
fn deleting_one_item_or_every_read_one() {
    let (mut s, id) = store();
    s.add_inbox(
        id,
        &[item(id, 1, T1), item(id, 2, T2), item(id, 3, T3)],
        ts(NOW),
    )
    .unwrap();
    let items = s.inbox_items(None, 10).unwrap();
    s.delete_inbox(&[items[0].id]).unwrap();
    assert_eq!(changes(&s.inbox_items(None, 10).unwrap()), [2, 1]);
    s.mark_inbox_read(&[items[1].id], ts(NOW)).unwrap();
    assert_eq!(s.delete_read_inbox(Some(id)).unwrap(), 1);
    assert_eq!(changes(&s.inbox_items(None, 10).unwrap()), [1]);
}

#[test]
fn removing_a_server_removes_its_inbox() {
    let (mut s, a) = store();
    let b = s.server_id("https://other.example/").unwrap();
    s.add_inbox(a, &[item(a, 1, T1)], ts(NOW)).unwrap();
    s.add_inbox(b, &[item(b, 1, T1)], ts(NOW)).unwrap();
    s.delete_server(a).unwrap();
    let items = s.inbox_items(None, 10).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].server, b);
    assert!(!s.inbox_unread_counts().unwrap().contains_key(&a));
}

#[test]
fn adding_prunes_by_age_and_by_count() {
    let (mut s, a) = store();
    let b = s.server_id("https://other.example/").unwrap();
    // 60 days before NOW is 2023-12-06; the first is older.
    s.add_inbox(
        a,
        &[
            item(a, 1, "2023-12-01T00:00:00Z"),
            item(a, 2, "2023-12-10T00:00:00Z"),
        ],
        ts(NOW),
    )
    .unwrap();
    assert_eq!(changes(&s.inbox_items(None, 10).unwrap()), [2]);

    // The cap is per server, and the newest items stay.
    let many: Vec<_> = (0..INBOX_KEEP as u64 + 20)
        .map(|n| item(b, 1000 + n, "2024-01-20T00:00:00Z"))
        .collect();
    s.add_inbox(b, &many, ts(NOW)).unwrap();
    let mine = s.inbox_items(Some(&[b]), 10_000).unwrap();
    assert_eq!(mine.len(), INBOX_KEEP);
    // Same time: the later insertions win.
    assert_eq!(mine[0].event.change.0, 1000 + INBOX_KEEP as u64 + 19);
    assert_eq!(s.inbox_items(Some(&[a]), 10).unwrap().len(), 1);
}

#[test]
fn prune_inbox_covers_every_server() {
    let (mut s, id) = store();
    s.add_inbox(id, &[item(id, 1, T1), item(id, 2, T3)], ts(T3))
        .unwrap();
    // Two months later both are old.
    assert_eq!(s.prune_inbox(ts("2024-05-01T00:00:00Z")).unwrap(), 2);
    assert!(s.inbox_items(None, 10).unwrap().is_empty());
}

#[test]
fn v9_adds_the_inbox_and_keeps_existing_data() {
    let mut conn = Connection::open_in_memory().unwrap();
    migrate(&mut conn, &MIGRATIONS[..8]).unwrap();
    assert_eq!(user_version(&conn).unwrap(), 8);
    conn.execute(
        "INSERT INTO servers (id, base_url) VALUES (1, 'https://x')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO pending_events (server_id, key, event, created_ms) VALUES (1, 'k', '{}', 0)",
        [],
    )
    .unwrap();
    migrate(&mut conn, MIGRATIONS).unwrap();
    assert_eq!(user_version(&conn).unwrap() as usize, MIGRATIONS.len());
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM inbox"), 0);
    assert_eq!(count("SELECT COUNT(*) FROM pending_events"), 1);
    let mut s = Store { conn };
    let a = item(ServerId(1), 1, T1);
    assert_eq!(s.add_inbox(ServerId(1), &[a], ts(NOW)).unwrap(), 1);
}

#[test]
fn delivering_keeps_the_items_and_acknowledges_the_events_together() {
    let (mut s, id) = store();
    let (a, b) = (item(id, 1, T1), item(id, 2, T2));
    s.queue_events(id, &[a.event.clone(), b.event.clone()], ts(NOW))
        .unwrap();
    let keys = [a.event.key(), b.event.key()];
    // `b` matched no rule: acknowledged but not kept.
    assert_eq!(
        s.deliver_events(id, std::slice::from_ref(&a), &keys, ts(NOW))
            .unwrap(),
        1
    );
    assert!(s.pending_events(id).unwrap().is_empty());
    assert_eq!(changes(&s.inbox_items(None, 10).unwrap()), [1]);
}

#[test]
fn a_failed_inbox_write_leaves_the_events_pending() {
    let (mut s, id) = store();
    let a = item(id, 1, T1);
    s.queue_events(id, std::slice::from_ref(&a.event), ts(NOW))
        .unwrap();
    s.conn
        .execute_batch(
            "CREATE TRIGGER no_inbox BEFORE INSERT ON inbox
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )
        .unwrap();
    let keys = [a.event.key()];
    assert!(
        s.deliver_events(id, std::slice::from_ref(&a), &keys, ts(NOW))
            .is_err()
    );
    assert_eq!(s.pending_events(id).unwrap().len(), 1);
    // The next attempt, once the write works, keeps and acknowledges it.
    s.conn.execute_batch("DROP TRIGGER no_inbox").unwrap();
    s.deliver_events(id, &[a], &keys, ts(NOW)).unwrap();
    assert!(s.pending_events(id).unwrap().is_empty());
    assert_eq!(s.inbox_items(None, 10).unwrap().len(), 1);
}
