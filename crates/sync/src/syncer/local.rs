//! Reconciling a checkout's local commits with the store (see
//! [`crate::local`] for the rules).

use std::{
    collections::BTreeSet,
    hash::{DefaultHasher, Hash, Hasher},
};

use jiff::Timestamp;
use model::{ChangeNumber, LocalChange, LocalChangeState, LocalPatchSet, LocalScan};
use store::{BorrowedLabels, LocalChangeEntry, ReconcileReport, Store};

use super::Syncer;
use crate::{
    Error,
    local::{plan_disabled, plan_reconcile, scan_change_ids},
};

impl Syncer {
    /// Brings the local change rows of `project` up to date with `scan`, a
    /// look at the project's checkout: loads the rows and the cached Gerrit
    /// changes with the scan's and the rows' Change-Ids, plans with
    /// [`plan_reconcile`] (or [`plan_disabled`] when the project's
    /// `local_changes` option is off, `enabled == false`; the scan is not
    /// used then) and applies the plan in one store transaction. No
    /// network. `author_emails` are the signed-in account's email and the
    /// checkout's author emails.
    ///
    /// The store lock is held from loading to applying, so nothing in this
    /// process changes the rows in between. A scanner that wants to skip
    /// the next scan when nothing changed uses
    /// [`Syncer::reconcile_local_keyed`] instead.
    ///
    /// Blocking (SQLite): call it from a background executor.
    pub fn reconcile_local(
        &self,
        project: &str,
        scan: &LocalScan,
        author_emails: &[String],
        enabled: bool,
    ) -> Result<ReconcileReport, Error> {
        let mut store = self.store();
        self.reconcile_in(&mut store, project, scan, author_emails, enabled)
    }

    /// [`Syncer::reconcile_local`] with the option on, and the
    /// [`local_generation`](Syncer::local_generation) of the result, read
    /// under the same store lock right after the plan is applied. The
    /// caller may skip the next scan of the project while the refs, the
    /// author emails and this generation stay the same, but only when the
    /// report [`is_settled`](ReconcileReport::is_settled): otherwise (a
    /// row kept for its unsent data, a stale op, a re-key conflict) the
    /// next scan has to run anyway.
    pub fn reconcile_local_keyed(
        &self,
        project: &str,
        scan: &LocalScan,
        author_emails: &[String],
    ) -> Result<(ReconcileReport, u64), Error> {
        let mut store = self.store();
        let report = self.reconcile_in(&mut store, project, scan, author_emails, true)?;
        let generation = self.generation_in(&store, project, &scan_change_ids(scan))?;
        Ok((report, generation))
    }

    fn reconcile_in(
        &self,
        store: &mut Store,
        project: &str,
        scan: &LocalScan,
        author_emails: &[String],
        enabled: bool,
    ) -> Result<ReconcileReport, Error> {
        let now = Timestamp::now();
        let entries = store.local_changes_of_project(self.server, project)?;
        let ops = if enabled {
            let change_ids = union(scan_change_ids(scan), &entries);
            let cached = store.cached_changes_by_change_id(self.server, project, &change_ids)?;
            plan_reconcile(scan, author_emails, &cached, &entries, now)
        } else {
            plan_disabled(&entries)
        };
        Ok(store.reconcile_local(self.server, project, &ops, now)?)
    }

    /// A hash of everything [`Syncer::reconcile_local`] reads from the
    /// store for `project` besides the scan, given the Change-Ids of the
    /// scan's commits ([`scan_change_ids`](crate::local::scan_change_ids)):
    ///
    /// * the project's local change rows, each with its id, key (branch
    ///   and Change-Id), state, Gerrit change, whether it has unsent data,
    ///   and its versions (commit and Gerrit patch set);
    /// * what the classification uses of the cached Gerrit changes with
    ///   the scan's or the rows' Change-Ids: number, branch, status, and,
    ///   when the detail is cached, the patch sets with their commits and
    ///   the current one (not `updated`, which moves with every comment).
    ///
    /// So a reconcile of an unchanged scan does the same as the last one
    /// while this stays equal: a pushed commit is recognised as soon as
    /// its change is cached, and a vanished row whose last unsent draft
    /// is gone is looked at again, though no local ref moved. Stable
    /// within one process run only. Blocking (SQLite).
    pub fn local_generation(
        &self,
        project: &str,
        scan_change_ids: &[String],
    ) -> Result<u64, Error> {
        let store = self.store();
        self.generation_in(&store, project, scan_change_ids)
    }

    fn generation_in(
        &self,
        store: &Store,
        project: &str,
        scan_change_ids: &[String],
    ) -> Result<u64, Error> {
        let entries = store.local_changes_of_project(self.server, project)?;
        let change_ids = union(scan_change_ids.to_vec(), &entries);
        let cached = store.cached_change_keys(self.server, project, &change_ids)?;
        let mut hasher = DefaultHasher::new();
        for e in &entries {
            let c = &e.change;
            (c.id, &c.branch, &c.change_id, c.state, c.gerrit_number).hash(&mut hasher);
            e.has_unsent.hash(&mut hasher);
            for p in &e.patch_sets {
                (&p.sha, p.gerrit_patch_set).hash(&mut hasher);
            }
        }
        cached.hash(&mut hasher);
        Ok(hasher.finish())
    }

    /// The local changes the dashboard lists (see
    /// [`store::Store::dashboard_local_changes`]), the most recently
    /// updated first. Blocking (SQLite).
    pub fn dashboard_local_changes(&self) -> Result<Vec<LocalChange>, Error> {
        Ok(self.store().dashboard_local_changes(self.server)?)
    }

    /// The local change with synthetic number `number` and its versions in
    /// the order they were first seen; `None` for a server's change number
    /// or a row that is gone. Blocking (SQLite).
    pub fn local_change(
        &self,
        number: ChangeNumber,
    ) -> Result<Option<(LocalChange, Vec<LocalPatchSet>)>, Error> {
        let store = self.store();
        let Some(change) = store.local_change_by_number(self.server, number)? else {
            return Ok(None);
        };
        let versions = store.local_patch_sets(self.server, change.id)?;
        Ok(Some((change, versions)))
    }

    /// The versions of Gerrit change `number` that exist only in the
    /// checkout: the local patch sets of its local change rows that are not
    /// (yet) a Gerrit patch set, in the order they were first seen. Rows
    /// that were consolidated are left out (what was on their versions has
    /// moved to the Gerrit change). Empty for a local change's number.
    ///
    /// `ordinal` names the version ("local 2") and never changes, also
    /// when an earlier version reaches Gerrit and drops out: with one row
    /// it is the row's own; with several (the target branch changed, and
    /// so the key) all versions of all the rows, pushed and consolidated
    /// ones included, are numbered in the order (first seen, row id) from
    /// 1. Blocking (SQLite).
    pub fn local_patch_sets_of(&self, number: ChangeNumber) -> Result<Vec<LocalPatchSet>, Error> {
        if number.is_local() {
            return Ok(Vec::new());
        }
        let store = self.store();
        let rows = store.local_changes_of_gerrit_change(self.server, number)?;
        let several = rows.len() > 1;
        let mut all = Vec::new();
        for row in rows {
            let shown = row.state != LocalChangeState::Consolidated;
            for v in store.local_patch_sets(self.server, row.id)? {
                all.push((shown, v));
            }
        }
        all.sort_by_key(|(_, v)| (v.seen_at, v.id));
        Ok(all
            .into_iter()
            .zip(1..)
            .filter_map(|((shown, mut v), n)| {
                if several {
                    v.ordinal = n;
                }
                (shown && v.gerrit_patch_set.is_none()).then_some(v)
            })
            .collect())
    }

    /// The labels a local change of `project` borrows (see
    /// [`store::Store::labels_to_borrow`]): those of the newest cached
    /// change of the same branch, else of the project. `None` when no
    /// cached change of the project has labels. Blocking (SQLite).
    pub fn labels_to_borrow(
        &self,
        project: &str,
        branch: Option<&str>,
    ) -> Result<Option<BorrowedLabels>, Error> {
        let store = self.store();
        if let Some(branch) = branch
            && let Some(labels) = store.labels_to_borrow(self.server, project, Some(branch))?
        {
            return Ok(Some(labels));
        }
        Ok(store.labels_to_borrow(self.server, project, None)?)
    }

    /// The projects that have local change rows on this server, in any
    /// state, sorted and each once: those a scanner has to reconcile with
    /// the option off when the project no longer has it. Blocking
    /// (SQLite).
    pub fn local_change_projects(&self) -> Result<Vec<String>, Error> {
        let rows = self.store().local_changes(self.server)?;
        let projects: BTreeSet<String> = rows.into_iter().map(|c| c.project).collect();
        Ok(projects.into_iter().collect())
    }
}

/// `ids` and the rows' Change-Ids, sorted, each once.
fn union(ids: Vec<String>, entries: &[LocalChangeEntry]) -> Vec<String> {
    let mut ids: BTreeSet<String> = ids.into_iter().collect();
    ids.extend(entries.iter().map(|e| e.change.change_id.clone()));
    ids.into_iter().collect()
}

#[cfg(test)]
mod tests;
