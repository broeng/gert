//! Reconciling a checkout's local commits with the store (see
//! [`crate::local`] for the rules).

use std::{
    collections::{BTreeSet, HashMap, hash_map::Entry},
    hash::{DefaultHasher, Hash, Hasher},
};

use jiff::Timestamp;
use model::{
    ChangeNumber, ChangeStatus, ChangeSummary, CommitInfo, LocalChange, LocalChangeState,
    LocalPatchSet, LocalScan, PatchSetNumber, RevisionId,
};
use store::{BorrowedLabels, LocalChangeEntry, ReconcileReport, ServerId, Store};

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
        versions_of_gerrit_change(&self.store(), self.server, number)
    }

    /// The version of Gerrit change `number` that only the checkout has and
    /// that a Submit would leave behind: what the Submit warning asks
    /// about. Of the versions [`Syncer::local_patch_sets_of`] lists, only
    /// those of rows still in the checkout count (checking, local or
    /// ambiguous; not vanished or closed). Then:
    ///
    /// * With `on_branches`, the commits of a fresh scan of the checkout
    ///   (newest first, as [`LocalScan::commits`]), the version of those
    ///   that comes first: the one nearest a branch tip. A version no
    ///   longer on a named branch does not count.
    /// * Without (the scan did not answer), the newest version first seen
    ///   after the newest version of its row that reached Gerrit, or the
    ///   newest of a row none of whose versions did. Both times are ours,
    ///   so no server clock is involved: a patch set made on Gerrit after
    ///   the version (a rebase, a co-author's upload) does not hide it.
    ///
    /// `None` when there is none. Blocking (SQLite).
    pub fn newer_local_version(
        &self,
        number: ChangeNumber,
        on_branches: Option<&[RevisionId]>,
    ) -> Result<Option<LocalPatchSet>, Error> {
        if number.is_local() {
            return Ok(None);
        }
        let store = self.store();
        newer_version_in(&store, self.server, number, on_branches)
    }

    /// [`Syncer::newer_local_version`] of Gerrit change `number` and of
    /// each open change below its patch set `revision` in the cached
    /// relation chain, which a Submit takes along: the ones that have such
    /// a version, this change first, then down the chain. Without a cached
    /// chain only this change is looked at. Blocking (SQLite).
    pub fn newer_local_versions(
        &self,
        number: ChangeNumber,
        revision: &RevisionId,
        on_branches: Option<&[RevisionId]>,
    ) -> Result<Vec<(ChangeNumber, LocalPatchSet)>, Error> {
        if number.is_local() {
            return Ok(Vec::new());
        }
        let store = self.store();
        let chain = store
            .load_related(self.server, number, revision)?
            .map(|c| c.changes)
            .unwrap_or_default();
        let below = chain
            .iter()
            .skip_while(|c| c.number != number)
            .skip(1)
            .filter(|c| matches!(c.status, Some(ChangeStatus::New)))
            .map(|c| c.number);
        let mut out = Vec::new();
        for n in std::iter::once(number).chain(below) {
            if let Some(v) = newer_version_in(&store, self.server, n, on_branches)? {
                out.push((n, v));
            }
        }
        Ok(out)
    }

    /// What each of `commits` (a relation chain read from the checkout of
    /// `project`) is, as far as the store knows, in the same order:
    ///
    /// * a patch set of a cached Gerrit change (its detail lists the
    ///   commit, or a local version of it was mapped to that patch set);
    /// * a local version of a Gerrit change, by its stable name (as
    ///   [`Syncer::local_patch_sets_of`] gives it);
    /// * a version of a local change;
    /// * none of these ([`ChainCommit::Unknown`]): a commit by someone
    ///   else, without a Change-Id, of a change not cached, or an old
    ///   version of a change that was consolidated.
    ///
    /// No network. Blocking (SQLite).
    pub fn classify_chain(
        &self,
        project: &str,
        commits: &[CommitInfo],
    ) -> Result<Vec<ChainCommit>, Error> {
        let store = self.store();
        let ids: BTreeSet<String> = commits.iter().filter_map(|c| c.change_id.clone()).collect();
        let ids: Vec<String> = ids.into_iter().collect();
        let cached = store.cached_changes_by_change_id(self.server, project, &ids)?;
        let gerrit = |change: ChangeNumber, patch_set| {
            let found = cached.iter().find(|g| g.summary.number == change);
            ChainCommit::Gerrit {
                change,
                patch_set,
                current: found.and_then(|g| g.current),
                summary: found.map(|g| Box::new(g.summary.clone())),
            }
        };
        let mut versions: HashMap<ChangeNumber, Vec<LocalPatchSet>> = HashMap::new();
        let mut out = Vec::with_capacity(commits.len());
        for c in commits {
            let on_gerrit = cached.iter().find_map(|g| {
                let ps = g.patch_sets.as_ref()?.iter().find(|p| p.sha == c.sha)?;
                Some((g.summary.number, ps.number))
            });
            if let Some((change, patch_set)) = on_gerrit {
                out.push(gerrit(change, patch_set));
                continue;
            }
            let mut found = ChainCommit::Unknown;
            for v in store.local_patch_sets_by_sha(self.server, &c.sha)? {
                let Some(row) = store.local_change(self.server, v.local_change)? else {
                    continue;
                };
                if row.project != project {
                    continue;
                }
                found = match (row.gerrit_number, v.gerrit_patch_set) {
                    (Some(change), Some(patch_set)) => gerrit(change, patch_set),
                    (Some(change), None) => {
                        let of = match versions.entry(change) {
                            Entry::Occupied(e) => e.into_mut(),
                            Entry::Vacant(e) => {
                                e.insert(versions_of_gerrit_change(&store, self.server, change)?)
                            }
                        };
                        // Left out when its row was consolidated.
                        match of.iter().find(|w| w.id == v.id) {
                            Some(w) => ChainCommit::LocalVersion {
                                change,
                                patch_set: w.number,
                                ordinal: w.ordinal,
                            },
                            None => continue,
                        }
                    }
                    (None, _) => {
                        // The change page shows the newest version unless
                        // told otherwise.
                        let all = store.local_patch_sets(self.server, row.id)?;
                        ChainCommit::Local {
                            change: row.number,
                            patch_set: v.number,
                            ordinal: v.ordinal,
                            newest: all.last().is_some_and(|w| w.id == v.id),
                        }
                    }
                };
                break;
            }
            out.push(found);
        }
        Ok(out)
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

/// What a commit of a relation chain read from the checkout is (see
/// [`Syncer::classify_chain`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainCommit {
    /// Version `ordinal` ("local 2") of local change `change`; `newest`
    /// when it is the change's newest version (the one its page shows by
    /// default).
    Local {
        change: ChangeNumber,
        patch_set: PatchSetNumber,
        ordinal: u32,
        newest: bool,
    },
    /// Local version `ordinal` of Gerrit change `change`: not a patch set
    /// on Gerrit.
    LocalVersion {
        change: ChangeNumber,
        patch_set: PatchSetNumber,
        ordinal: u32,
    },
    /// Patch set `patch_set` of Gerrit change `change`, with the change's
    /// current patch set and summary when they are cached.
    Gerrit {
        change: ChangeNumber,
        patch_set: PatchSetNumber,
        current: Option<PatchSetNumber>,
        summary: Option<Box<ChangeSummary>>,
    },
    /// Nothing the store knows.
    Unknown,
}

/// The local patch sets of Gerrit change `number` that are not on Gerrit,
/// with their stable names (see [`Syncer::local_patch_sets_of`]).
fn versions_of_gerrit_change(
    store: &Store,
    server: ServerId,
    number: ChangeNumber,
) -> Result<Vec<LocalPatchSet>, Error> {
    let rows = store.local_changes_of_gerrit_change(server, number)?;
    let several = rows.len() > 1;
    let mut all = Vec::new();
    for row in rows {
        let shown = row.state != LocalChangeState::Consolidated;
        for v in store.local_patch_sets(server, row.id)? {
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

/// See [`Syncer::newer_local_version`].
fn newer_version_in(
    store: &Store,
    server: ServerId,
    number: ChangeNumber,
    on_branches: Option<&[RevisionId]>,
) -> Result<Option<LocalPatchSet>, Error> {
    // Each present row with the time its newest version reached Gerrit.
    let mut present: HashMap<_, Option<Timestamp>> = HashMap::new();
    for row in store.local_changes_of_gerrit_change(server, number)? {
        if !matches!(
            row.state,
            LocalChangeState::Checking | LocalChangeState::Local | LocalChangeState::Ambiguous
        ) {
            continue;
        }
        let pushed = store
            .local_patch_sets(server, row.id)?
            .into_iter()
            .filter(|v| v.gerrit_patch_set.is_some())
            .map(|v| v.seen_at)
            .max();
        present.insert(row.id, pushed);
    }
    let candidates: Vec<LocalPatchSet> = versions_of_gerrit_change(store, server, number)?
        .into_iter()
        .filter(|v| present.contains_key(&v.local_change))
        .collect();
    Ok(match on_branches {
        Some(shas) => shas
            .iter()
            .find_map(|sha| candidates.iter().find(|v| v.sha == *sha))
            .cloned(),
        None => candidates.into_iter().rfind(|v| {
            present
                .get(&v.local_change)
                .copied()
                .flatten()
                .is_none_or(|pushed| v.seen_at > pushed)
        }),
    })
}

/// `ids` and the rows' Change-Ids, sorted, each once.
fn union(ids: Vec<String>, entries: &[LocalChangeEntry]) -> Vec<String> {
    let mut ids: BTreeSet<String> = ids.into_iter().collect();
    ids.extend(entries.iter().map(|e| e.change.change_id.clone()));
    ids.into_iter().collect()
}

#[cfg(test)]
mod tests;
