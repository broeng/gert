//! Reconciling a checkout's local commits with the store (see
//! [`crate::local`] for the rules).

use jiff::Timestamp;
use model::LocalScan;
use store::ReconcileReport;

use super::Syncer;
use crate::{
    Error,
    local::{plan_disabled, plan_reconcile},
};

impl Syncer {
    /// Brings the local change rows of `project` up to date with `scan`, a
    /// look at the project's checkout: loads the rows and the cached Gerrit
    /// changes with the scan's Change-Ids, plans with
    /// [`plan_reconcile`] (or [`plan_disabled`] when the project's
    /// `local_changes` option is off, `enabled == false`; the scan is not
    /// used then) and applies the plan in one store transaction. No
    /// network. `author_emails` are the signed-in account's email and the
    /// checkout's author emails.
    ///
    /// The store lock is held from loading to applying, so nothing in this
    /// process changes the rows in between. Still, the caller must record
    /// the scan's refs fingerprint as done only when the report
    /// [`is_settled`](ReconcileReport::is_settled): otherwise (a row kept
    /// for its unsent data, a stale op, a re-key conflict) the next scan
    /// has to run even if the refs did not move.
    ///
    /// Blocking (SQLite): call it from a background executor.
    pub fn reconcile_local(
        &self,
        project: &str,
        scan: &LocalScan,
        author_emails: &[String],
        enabled: bool,
    ) -> Result<ReconcileReport, Error> {
        let now = Timestamp::now();
        let mut store = self.store();
        let entries = store.local_changes_of_project(self.server, project)?;
        let ops = if enabled {
            let mut change_ids: Vec<String> = scan
                .commits
                .iter()
                .filter_map(|c| c.commit.change_id.clone())
                .chain(entries.iter().map(|e| e.change.change_id.clone()))
                .collect();
            change_ids.sort_unstable();
            change_ids.dedup();
            let cached = store.cached_changes_by_change_id(self.server, project, &change_ids)?;
            plan_reconcile(scan, author_emails, &cached, &entries, now)
        } else {
            plan_disabled(&entries)
        };
        Ok(store.reconcile_local(self.server, project, &ops, now)?)
    }
}

#[cfg(test)]
mod tests;
