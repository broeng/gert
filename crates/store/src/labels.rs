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

use model::LabelValue;
use rusqlite::{OptionalExtension as _, params, params_from_iter, types::Value};

use crate::{Error, ServerId, Store};

/// The vote labels of one cached change, lent to a local change (which
/// has none of its own) for its reply dialog.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BorrowedLabels {
    /// The values the user may give per label (`permitted_labels`).
    pub permitted: BTreeMap<String, Vec<i32>>,
    /// What each value of a label means (`label_values`).
    pub values: BTreeMap<String, Vec<LabelValue>>,
}

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

    /// The labels of the newest cached change detail of `project` (and of
    /// `branch`, when given) that defines labels: open changes first,
    /// since a closed change may only permit post-submit votes, then by
    /// `updated`. `None` when no cached detail of the project has labels.
    /// Local changes are never cached, so they never lend labels.
    pub fn labels_to_borrow(
        &self,
        server: ServerId,
        project: &str,
        branch: Option<&str>,
    ) -> Result<Option<BorrowedLabels>, Error> {
        let row: Option<(Option<String>, String)> = self
            .conn
            .prepare_cached(
                "SELECT json_extract(d.json, '$.permitted_labels'),
                        json_extract(d.json, '$.label_values')
                 FROM change_details d
                 JOIN changes c ON c.server_id = d.server_id AND c.number = d.number
                 WHERE d.server_id = ?1
                   AND json_extract(c.json, '$.project') = ?2
                   AND (?3 IS NULL OR json_extract(c.json, '$.branch') = ?3)
                   AND json_type(d.json, '$.label_values') = 'object'
                   AND json_extract(d.json, '$.label_values') != '{}'
                 ORDER BY json_extract(c.json, '$.status') = 'New' DESC, c.updated DESC
                 LIMIT 1",
            )?
            .query_row(params![server.0, project, branch], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let Some((permitted, values)) = row else {
            return Ok(None);
        };
        Ok(Some(BorrowedLabels {
            permitted: permitted
                .map(|p| serde_json::from_str(&p))
                .transpose()?
                .unwrap_or_default(),
            values: serde_json::from_str(&values)?,
        }))
    }
}

#[cfg(test)]
mod tests;
