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
    // v4 -> v5: small key/value state of the application itself (the last
    // active server), not tied to a server row.
    "
    CREATE TABLE app_state (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    ",
    // v5 -> v6: servers get the slug of their `[[servers]]` config entry, and
    // `base_url` stops being unique: two entries may share a URL (different
    // users) and each needs its own cache rows. SQLite cannot drop a
    // constraint, so the table is rebuilt; `migrate` turns foreign keys off
    // around this (dropping the parent would cascade-delete the cache
    // otherwise). `slug` is NULL for rows from before slugs; the first
    // server with that URL adopts them (`Store::server_for_slug`).
    "
    CREATE TABLE servers_new (
        id       INTEGER PRIMARY KEY,
        base_url TEXT NOT NULL,
        slug     TEXT UNIQUE
    );
    INSERT INTO servers_new (id, base_url) SELECT id, base_url FROM servers;
    DROP TABLE servers;
    ALTER TABLE servers_new RENAME TO servers;
    CREATE INDEX servers_by_url ON servers (base_url);
    ",
    // v6 -> v7: notifications. `notified_events` is the ledger of event keys
    // that were already reported, so a restart or a re-detected change never
    // notifies twice (`events.rs`); `seen_ms` is Unix milliseconds, for
    // pruning. `servers.watched_since` (Unix milliseconds) is NULL until the
    // first watch pass has recorded a baseline for the server.
    "
    CREATE TABLE notified_events (
        server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        key       TEXT NOT NULL,
        seen_ms   INTEGER NOT NULL,
        PRIMARY KEY (server_id, key)
    );
    CREATE INDEX notified_events_seen ON notified_events (seen_ms);
    ALTER TABLE servers ADD COLUMN watched_since INTEGER;
    ",
    // v7 -> v8: `pending_events` holds notification events that were
    // detected (and recorded in the ledger) but not yet acknowledged as
    // delivered, so a restart does not lose them. `event` is the JSON of a
    // `model::Event`, `created_ms` Unix milliseconds.
    "
    CREATE TABLE pending_events (
        server_id  INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        key        TEXT NOT NULL,
        event      TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        PRIMARY KEY (server_id, key)
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
    // Table rebuilds need foreign keys off, and the pragma is a no-op inside
    // a transaction. Restore it whatever happens.
    let foreign_keys: bool = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
    if foreign_keys {
        conn.execute_batch("PRAGMA foreign_keys = OFF")?;
    }
    let result = run_steps(conn, &steps[found as usize..], supported);
    if foreign_keys {
        conn.execute_batch("PRAGMA foreign_keys = ON")?;
    }
    result
}

fn run_steps(conn: &mut Connection, steps: &[&str], supported: u32) -> Result<(), Error> {
    let tx = conn.transaction()?;
    for step in steps {
        tx.execute_batch(step)?;
    }
    // A rebuild must not leave dangling references.
    let dangling = tx
        .prepare("PRAGMA foreign_key_check")?
        .query_map([], |r| r.get::<_, String>(0))?
        .next()
        .transpose()?;
    if let Some(table) = dangling {
        return Err(Error::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
            Some(format!("migration left dangling references in {table}")),
        )));
    }
    // PRAGMA does not accept bound parameters.
    tx.execute_batch(&format!("PRAGMA user_version = {supported}"))?;
    tx.commit()?;
    Ok(())
}
