use rusqlite::Connection;

use crate::Error;

/// Ordered schema steps. Step `i` upgrades `user_version` `i` to `i + 1`.
/// Never edit a released step; append a new one.
///
/// Every cached row carries `server_id`, so several servers can share one
/// database. Write-side tables (an offline outbox, drafts) can be added as
/// later steps without touching these.
pub(crate) const MIGRATIONS: &[&str] = &[
    // v0 -> v1
    "
    CREATE TABLE servers (
        id       INTEGER PRIMARY KEY,
        base_url TEXT NOT NULL UNIQUE
    );

    CREATE TABLE changes (
        server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        number    INTEGER NOT NULL,
        updated   TEXT NOT NULL,
        json      TEXT NOT NULL,
        PRIMARY KEY (server_id, number)
    );

    CREATE TABLE query_results (
        server_id    INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        query        TEXT NOT NULL,
        more_changes INTEGER NOT NULL,
        fetched_at   TEXT NOT NULL,
        PRIMARY KEY (server_id, query)
    );

    CREATE TABLE query_result_items (
        server_id INTEGER NOT NULL,
        query     TEXT NOT NULL,
        position  INTEGER NOT NULL,
        number    INTEGER NOT NULL,
        PRIMARY KEY (server_id, query, position),
        FOREIGN KEY (server_id, query)
            REFERENCES query_results(server_id, query) ON DELETE CASCADE
    );

    CREATE TABLE change_details (
        server_id  INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        number     INTEGER NOT NULL,
        json       TEXT NOT NULL,
        fetched_at TEXT NOT NULL,
        PRIMARY KEY (server_id, number)
    );
    ",
    // v1 -> v2: caches for the change and diff views. `base_sha` is the empty
    // string when comparing against the parent commit. `fetched_ms` is Unix
    // milliseconds; `size` is the byte length of a diff's JSON.
    "
    CREATE TABLE file_lists (
        server_id     INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        change_number INTEGER NOT NULL,
        base_sha      TEXT NOT NULL,
        target_sha    TEXT NOT NULL,
        files_json    TEXT NOT NULL,
        reviewed_json TEXT,
        fetched_ms    INTEGER NOT NULL,
        PRIMARY KEY (server_id, change_number, base_sha, target_sha)
    );
    CREATE INDEX file_lists_fetched ON file_lists (fetched_ms);

    CREATE TABLE diffs (
        server_id     INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        change_number INTEGER NOT NULL,
        base_sha      TEXT NOT NULL,
        target_sha    TEXT NOT NULL,
        path          TEXT NOT NULL,
        whitespace    TEXT NOT NULL,
        context       TEXT NOT NULL,
        json          TEXT NOT NULL,
        size          INTEGER NOT NULL,
        fetched_ms    INTEGER NOT NULL,
        PRIMARY KEY (server_id, change_number, base_sha, target_sha, path, whitespace, context)
    );
    CREATE INDEX diffs_fetched ON diffs (fetched_ms);

    CREATE TABLE related_changes (
        server_id     INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        change_number INTEGER NOT NULL,
        revision_sha  TEXT NOT NULL,
        json          TEXT NOT NULL,
        fetched_ms    INTEGER NOT NULL,
        PRIMARY KEY (server_id, change_number, revision_sha)
    );
    CREATE INDEX related_changes_fetched ON related_changes (fetched_ms);

    CREATE TABLE diff_preferences (
        server_id  INTEGER PRIMARY KEY REFERENCES servers(id) ON DELETE CASCADE,
        json       TEXT NOT NULL,
        fetched_ms INTEGER NOT NULL
    );
    ",
];

pub(crate) fn user_version(conn: &Connection) -> Result<u32, Error> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Brings the database up to the last of `steps` in one transaction.
/// Fails without touching the database if it is already newer.
pub(crate) fn migrate(conn: &mut Connection, steps: &[&str]) -> Result<(), Error> {
    let supported = u32::try_from(steps.len()).unwrap_or(u32::MAX);
    let found = user_version(conn)?;
    if found > supported {
        return Err(Error::SchemaTooNew { found, supported });
    }
    if found == supported {
        return Ok(());
    }
    let tx = conn.transaction()?;
    for step in &steps[found as usize..] {
        tx.execute_batch(step)?;
    }
    // PRAGMA does not accept bound parameters.
    tx.execute_batch(&format!("PRAGMA user_version = {supported}"))?;
    tx.commit()?;
    Ok(())
}
