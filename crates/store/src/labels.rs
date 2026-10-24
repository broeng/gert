//! The vote labels and values the cache has seen, for the notification
//! rule editor.
//!
//! Gerrit has no endpoint for "all labels of the server" short of one
//! project's config, so the editor offers what the cached changes carry:
//! the values the user may give (`permitted_labels`), the values the label
//! defines (`label_values`), the values reviewers gave (`votes`), and the
//! label names of the list rows. This reads JSON in SQLite and never
//! deserializes a whole change, and it looks at the most recent rows only,
//! so it stays quick on a big cache.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{params_from_iter, types::Value};

use crate::{Error, ServerId, Store};

/// How many cached change details are read, newest first.
const DETAILS_READ: usize = 500;
/// How many cached list rows are read for label names, newest first.
const SUMMARIES_READ: usize = 2_000;

impl Store {
    /// The vote labels seen on the cached changes of `servers`, each with
    /// the values seen for it. A label seen on a list row only has no
    /// values.
    pub fn label_values(
        &self,
        servers: &[ServerId],
    ) -> Result<BTreeMap<String, BTreeSet<i32>>, Error> {
        let mut out: BTreeMap<String, BTreeSet<i32>> = BTreeMap::new();
        if servers.is_empty() {
            return Ok(out);
        }
        let marks = vec!["?"; servers.len()].join(", ");
        let ids = || params_from_iter(servers.iter().map(|s| Value::Integer(s.0)));
        // Values: a label object whose values are numbers (permitted), or
        // objects with a `value` (label_values and votes).
        let details = format!(
            "SELECT json FROM change_details WHERE server_id IN ({marks})
             ORDER BY fetched_at DESC LIMIT {DETAILS_READ}"
        );
        let queries = [
            // permitted_labels: {"Code-Review": [-1, 0, 1]}
            format!(
                "SELECT l.key, v.value FROM ({details}) d,
                   json_each(d.json, '$.permitted_labels') l, json_each(l.value) v
                 WHERE v.type = 'integer'"
            ),
            // label_values: {"Code-Review": [{"value": -1, "description": ""}]},
            // votes: {"Code-Review": [{"account": {}, "value": -1}]}
            format!(
                "SELECT l.key, json_extract(v.value, '$.value') FROM ({details}) d,
                   json_each(d.json, '$.label_values') l, json_each(l.value) v"
            ),
            format!(
                "SELECT l.key, json_extract(v.value, '$.value') FROM ({details}) d,
                   json_each(d.json, '$.votes') l, json_each(l.value) v"
            ),
        ];
        for sql in queries {
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(ids(), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?))
            })?;
            for row in rows {
                let (label, value) = row?;
                let seen = out.entry(label).or_default();
                if let Some(v) = value.and_then(|v| i32::try_from(v).ok()) {
                    seen.insert(v);
                }
            }
        }
        // Names from the list rows: `labels` of a summary, value flags only.
        let mut stmt = self.conn.prepare(&format!(
            "SELECT DISTINCT l.key FROM (SELECT json FROM changes
               WHERE server_id IN ({marks}) ORDER BY updated DESC LIMIT {SUMMARIES_READ}) c,
               json_each(c.json, '$.labels') l"
        ))?;
        for name in stmt.query_map(ids(), |r| r.get::<_, String>(0))? {
            out.entry(name?).or_default();
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
