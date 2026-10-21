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
    // v2 -> v3: diffs are no longer keyed by `context`. The server ignores
    // that parameter and always sends the whole file, so unchanged lines are
    // folded on the client. Cached diffs are only a cache: drop them.
    "
    DROP TABLE diffs;
    CREATE TABLE diffs (
        server_id     INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        change_number INTEGER NOT NULL,
        base_sha      TEXT NOT NULL,
        target_sha    TEXT NOT NULL,
        path          TEXT NOT NULL,
        whitespace    TEXT NOT NULL,
        json          TEXT NOT NULL,
        size          INTEGER NOT NULL,
        fetched_ms    INTEGER NOT NULL,
        PRIMARY KEY (server_id, change_number, base_sha, target_sha, path, whitespace)
    );
    CREATE INDEX diffs_fetched ON diffs (fetched_ms);
    ",
    // v3 -> v4: comments and the user's drafts. See `drafts.rs`.
    //
    // `comments` holds the published comments of one change as one JSON
    // list: they are always fetched and read whole (the server has no
    // incremental endpoint worth using) and threads are built in memory, so
    // columns would only add schema churn.
    //
    // `drafts` has a column per field because the sync code queries and
    // updates drafts one by one and by state (what is pending?). `id` is the
    // local id; `server_draft_id` is NULL until the server knows the draft.
    // `state` is one of synced, pending_create, pending_update,
    // pending_delete, failed; `failed_op`, `error` and `retryable` describe
    // a failed one. `version` counts local edits. Timestamps are Unix
    // milliseconds.
    "
    CREATE TABLE comments (
        server_id     INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        change_number INTEGER NOT NULL,
        json          TEXT NOT NULL,
        fetched_ms    INTEGER NOT NULL,
        PRIMARY KEY (server_id, change_number)
    );

    CREATE TABLE drafts (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        server_id       INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        change_number   INTEGER NOT NULL,
        patch_set       INTEGER NOT NULL,
        revision_sha    TEXT NOT NULL,
        server_draft_id TEXT,
        path            TEXT NOT NULL,
        side            TEXT NOT NULL,
        parent          INTEGER,
        line            INTEGER,
        start_line      INTEGER,
        start_character INTEGER,
        end_line        INTEGER,
        end_character   INTEGER,
        in_reply_to     TEXT,
        message         TEXT NOT NULL,
        unresolved      INTEGER NOT NULL,
        updated_ms      INTEGER NOT NULL,
        state           TEXT NOT NULL,
        failed_op       TEXT,
        error           TEXT,
        retryable       INTEGER,
        attempts        INTEGER NOT NULL DEFAULT 0,
        last_error      TEXT,
        last_attempt_ms INTEGER,
        synced_ms       INTEGER,
        version         INTEGER NOT NULL DEFAULT 0
    );
    CREATE UNIQUE INDEX drafts_server_draft
        ON drafts (server_id, change_number, server_draft_id)
        WHERE server_draft_id IS NOT NULL;
    CREATE INDEX drafts_by_change ON drafts (server_id, change_number);
    CREATE INDEX drafts_by_state ON drafts (server_id, state);
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
