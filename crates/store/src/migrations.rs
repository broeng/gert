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
    // v8 -> v9: `inbox` is the in-app notification inbox (`inbox.rs`): one
    // row per matched event, whether or not the desktop notification was
    // shown. `event` is the JSON of a `model::Event`, `created_ms` the time
    // of the event and `read_ms` NULL while unread (both Unix milliseconds).
    // `rule_title` is the title of the rule that fired, at match time.
    "
    CREATE TABLE inbox (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        server_id   INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        event_key   TEXT NOT NULL,
        event       TEXT NOT NULL,
        rule_title  TEXT NOT NULL,
        mention     INTEGER NOT NULL DEFAULT 0,
        shown_on_os INTEGER NOT NULL DEFAULT 0,
        created_ms  INTEGER NOT NULL,
        read_ms     INTEGER,
        UNIQUE (server_id, event_key)
    );
    CREATE INDEX inbox_by_time ON inbox (server_id, created_ms);
    CREATE INDEX inbox_unread ON inbox (server_id) WHERE read_ms IS NULL;
    ",
    // v9 -> v10: `seen_ms` is when the inbox page was opened with the row
    // in it (NULL until then). It is separate from `read_ms`: the bell
    // counts the rows that are neither read nor seen. Existing rows start
    // unseen, so the bell shows the unread ones once.
    "
    ALTER TABLE inbox ADD COLUMN seen_ms INTEGER;
    ",
    // v10 -> v11: local changes, the user's unpushed commits (`local.rs`).
    //
    // `local_changes` has one row per (server, project, target branch,
    // Change-Id); `branch` is NULL when the target is unknown. UNIQUE treats
    // NULLs as distinct, so the key is an expression index over
    // `COALESCE(branch, '')` (git branch names are never empty). `state` is
    // one of checking, local, ambiguous, vanished, closed, consolidated.
    // `local_patch_sets` holds the versions (commits) of a change;
    // `gerrit_patch_set` is the Gerrit patch set with the same commit, once
    // there is one. `pending_reviews` are votes and a message given on a
    // local patch set, waiting to be sent: `labels` is a JSON object of
    // label → vote, `strategy` the project's `local_approval` at vote time,
    // `state` one of pending, sending, held, sent, failed.
    //
    // Ids are AUTOINCREMENT so that they are never reused: the synthetic
    // change and patch set numbers are derived from them. Times are Unix
    // milliseconds. `drafts.pending_review_id` marks a pending review's
    // message draft (sealed); a review has at most one.
    "
    CREATE TABLE local_changes (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        server_id     INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        project       TEXT NOT NULL,
        branch        TEXT,
        change_id     TEXT NOT NULL,
        gerrit_number INTEGER,
        subject       TEXT NOT NULL,
        author_name   TEXT NOT NULL,
        author_email  TEXT NOT NULL,
        updated_ms    INTEGER NOT NULL,
        state         TEXT NOT NULL,
        first_seen_ms INTEGER NOT NULL
    );
    CREATE UNIQUE INDEX local_changes_key
        ON local_changes (server_id, project, COALESCE(branch, ''), change_id);
    CREATE INDEX local_changes_by_change_id ON local_changes (server_id, change_id);
    CREATE INDEX local_changes_by_gerrit ON local_changes (server_id, gerrit_number)
        WHERE gerrit_number IS NOT NULL;

    CREATE TABLE local_patch_sets (
        id               INTEGER PRIMARY KEY AUTOINCREMENT,
        local_change_id  INTEGER NOT NULL REFERENCES local_changes(id) ON DELETE CASCADE,
        server_id        INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        sha              TEXT NOT NULL,
        parent_sha       TEXT,
        seen_ms          INTEGER NOT NULL,
        ordinal          INTEGER NOT NULL,
        gerrit_patch_set INTEGER,
        UNIQUE (local_change_id, sha),
        UNIQUE (local_change_id, ordinal)
    );
    CREATE INDEX local_patch_sets_by_sha ON local_patch_sets (server_id, sha);

    CREATE TABLE pending_reviews (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        server_id       INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        local_change_id INTEGER REFERENCES local_changes(id) ON DELETE CASCADE,
        change_number   INTEGER NOT NULL,
        patch_set       INTEGER NOT NULL,
        given_on_sha    TEXT NOT NULL,
        labels          TEXT NOT NULL,
        strategy        TEXT NOT NULL,
        state           TEXT NOT NULL,
        error           TEXT,
        held_reason     TEXT,
        created_ms      INTEGER NOT NULL,
        nonce           TEXT
    );
    CREATE INDEX pending_reviews_by_change ON pending_reviews (server_id, change_number);
    CREATE INDEX pending_reviews_by_local ON pending_reviews (local_change_id);

    ALTER TABLE drafts ADD COLUMN pending_review_id INTEGER
        REFERENCES pending_reviews(id) ON DELETE SET NULL;
    CREATE UNIQUE INDEX drafts_by_pending_review ON drafts (pending_review_id)
        WHERE pending_review_id IS NOT NULL;
    ",
    // v11 -> v12: what consolidation needs to keep moved data apart.
    //
    // `drafts.reserved_by` marks a draft that consolidation moved onto the
    // Gerrit change; it is posted inline with its local change's review and
    // never uploaded as a draft. `drafts.held_reason` marks a draft of a
    // local patch set that could not be moved and waits for the user.
    //
    // `pending_reviews` is rebuilt (SQLite cannot add NOT NULL to a column):
    // `local_change_id` becomes required, taken from the review's local
    // patch set where it was NULL (16777216 is `PatchSetNumber::LOCAL_BASE`;
    // reviews that cannot be attributed are dropped, and their message
    // drafts unsealed); `revision_sha` is the
    // commit of `patch_set`, which consolidation moves, apart from
    // `given_on_sha`; `voted_at_ms` is when the labels were last set. The
    // AUTOINCREMENT counter carries over, so ids are still never reused.
    // `migrate` runs with foreign keys off; the drafts' references to
    // `pending_reviews` resolve to the new table once it is renamed.
    "
    ALTER TABLE drafts ADD COLUMN reserved_by INTEGER
        REFERENCES local_changes(id) ON DELETE CASCADE;
    ALTER TABLE drafts ADD COLUMN held_reason TEXT;
    CREATE INDEX drafts_reserved ON drafts (reserved_by) WHERE reserved_by IS NOT NULL;

    CREATE TABLE pending_reviews_new (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        server_id       INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        local_change_id INTEGER NOT NULL REFERENCES local_changes(id) ON DELETE CASCADE,
        change_number   INTEGER NOT NULL,
        patch_set       INTEGER NOT NULL,
        revision_sha    TEXT NOT NULL,
        given_on_sha    TEXT NOT NULL,
        labels          TEXT NOT NULL,
        strategy        TEXT NOT NULL,
        state           TEXT NOT NULL,
        error           TEXT,
        held_reason     TEXT,
        created_ms      INTEGER NOT NULL,
        voted_at_ms     INTEGER NOT NULL,
        nonce           TEXT
    );
    INSERT INTO pending_reviews_new (id, server_id, local_change_id, change_number,
            patch_set, revision_sha, given_on_sha, labels, strategy, state, error,
            held_reason, created_ms, voted_at_ms, nonce)
        SELECT id, server_id, owner, change_number, patch_set, given_on_sha, given_on_sha,
               labels, strategy, state, error, held_reason, created_ms, created_ms, nonce
        FROM (SELECT r.*, COALESCE(r.local_change_id,
                     (SELECT p.local_change_id FROM local_patch_sets p
                      WHERE p.server_id = r.server_id AND p.id = r.patch_set - 16777216))
                     AS owner
              FROM pending_reviews r)
        WHERE owner IS NOT NULL;
    UPDATE drafts SET pending_review_id = NULL
        WHERE pending_review_id IS NOT NULL
          AND pending_review_id NOT IN (SELECT id FROM pending_reviews_new);
    INSERT INTO sqlite_sequence (name, seq)
        SELECT 'pending_reviews_new', 0
        WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'pending_reviews_new');
    UPDATE sqlite_sequence
        SET seq = (SELECT MAX(seq) FROM sqlite_sequence
                   WHERE name IN ('pending_reviews', 'pending_reviews_new'))
        WHERE name = 'pending_reviews_new';
    DROP TABLE pending_reviews;
    ALTER TABLE pending_reviews_new RENAME TO pending_reviews;
    CREATE INDEX pending_reviews_by_change ON pending_reviews (server_id, change_number);
    CREATE INDEX pending_reviews_by_local ON pending_reviews (local_change_id);
    ",
    // v12 -> v13: a consolidation post claims its drafts.
    //
    // `drafts.posting_nonce` is set on the reserved drafts a consolidation
    // post sends when the post is claimed, with the post's nonce (also the
    // claimed review's), and cleared when the post is finished or released.
    // A crash in between leaves it, so the next run checks the Gerrit
    // change's messages for the post's tag before posting again; this
    // works for posts without a review too.
    "
    ALTER TABLE drafts ADD COLUMN posting_nonce TEXT;
    ",
    // v13 -> v14: what the local change scanner reads on every round.
    //
    // `changes_by_change_id` lets the cached Gerrit changes with given
    // Change-Ids be found without reading every cached summary
    // (`Store::cached_changes_by_change_id`, `Store::cached_change_keys`
    // use this very expression). `servers.account_email` is the signed-in
    // account's email as last fetched, so that an offline start still
    // knows which commits are the user's; NULL when not known (or the
    // account has none).
    "
    CREATE INDEX changes_by_change_id ON changes (server_id, json_extract(json, '$.change_id'));
    ALTER TABLE servers ADD COLUMN account_email TEXT;
    ",
    // v14 -> v15: the rest of the signed-in account next to its email, so
    // that a local change's owner is that account on every page, also
    // offline and before the account is fetched in this run. NULL when
    // not known.
    "
    ALTER TABLE servers ADD COLUMN account_id INTEGER;
    ALTER TABLE servers ADD COLUMN account_username TEXT;
    ",
    // v15 -> v16: consolidation runs that do not step on each other, and
    // what the held card needs.
    //
    // `consolidation_leases` lets one consolidation run per server go on at
    // a time, also across processes on one cache: a run takes the row (or
    // an expired one) and renews it before every post. `local_post_claims`
    // names the run that claimed a post and when, so that a later run only
    // recovers posts whose run is gone and that are too old to still be in
    // flight. `pending_reviews.drop_only` is a JSON array of the labels of
    // a held review whose votes may only be dropped (approvals under the
    // strict rule or `exact`); NULL means none. `local_changes.checked_ms`
    // is when consolidation last asked Gerrit about the change, so rows
    // with nothing to send are asked about rarely.
    "
    CREATE TABLE consolidation_leases (
        server_id  INTEGER PRIMARY KEY REFERENCES servers(id) ON DELETE CASCADE,
        owner      TEXT NOT NULL,
        expires_ms INTEGER NOT NULL
    );
    CREATE TABLE local_post_claims (
        server_id       INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        nonce           TEXT NOT NULL,
        local_change_id INTEGER NOT NULL REFERENCES local_changes(id) ON DELETE CASCADE,
        owner           TEXT NOT NULL,
        claimed_ms      INTEGER NOT NULL,
        PRIMARY KEY (server_id, nonce)
    );
    ALTER TABLE pending_reviews ADD COLUMN drop_only TEXT;
    ALTER TABLE local_changes ADD COLUMN checked_ms INTEGER;
    ",
    // v16 -> v17: whether a local version was on a named branch in the
    // last complete scan of its project. Consolidation ports the comments
    // of a version that is not (it was amended away, or the branch was
    // reset), since it will never reach Gerrit. Existing versions count as
    // on a branch until the next complete scan says otherwise.
    "
    ALTER TABLE local_patch_sets ADD COLUMN on_branch INTEGER NOT NULL DEFAULT 1;
    ",
    // v17 -> v18: when a local change became a tombstone (`consolidated`).
    // A tombstone keeps its Gerrit change so that pages still naming its
    // synthetic number redirect; the scan removes it once its commits are
    // off every named branch and it is old enough that no page of this run
    // can still name it (navigation history is not kept across runs).
    // NULL for other states and for tombstones made before this step,
    // which no page of a later run names.
    "
    ALTER TABLE local_changes ADD COLUMN consolidated_ms INTEGER;
    ",
    // v18 -> v19: who wrote a pending review: the signed-in account (id
    // and username) when it was created. Consolidation sends only the
    // reviews of the account it runs as and holds the others. NULL for
    // reviews written before this step (or before the account was ever
    // fetched), which are sent as before.
    "
    ALTER TABLE pending_reviews ADD COLUMN author_id INTEGER;
    ALTER TABLE pending_reviews ADD COLUMN author_name TEXT;
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
