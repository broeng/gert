//! Local changes: which of a checkout's commits are the user's unpushed
//! changes, and what that means for the store's local change rows.
//!
//! [`plan_reconcile`] is pure: it takes one look at a project's checkout
//! (a [`LocalScan`], read by `local-git` in `ui`), the cached Gerrit
//! changes with the scan's Change-Ids and the store's rows for the project,
//! and returns the [`LocalOp`]s that bring the rows up to date.
//! [`Syncer::reconcile_local`](crate::Syncer::reconcile_local) loads those
//! inputs and applies the plan in one store transaction.
//!
//! # Rules
//!
//! * Only the user's own commits count: merges, commits without a
//!   Change-Id and commits whose author email is not one of the given
//!   emails (compared trimmed and case-insensitively) are skipped.
//! * A commit belongs to the change keyed by (project, target branch,
//!   Change-Id); the target branch is [`BranchTargets::target_for`] its
//!   branches, `None` when unknown.
//! * A commit that is a patch set of a cached Gerrit change is on Gerrit,
//!   not local. A local patch set with that commit is mapped to the Gerrit
//!   patch set ([`LocalOp::MapPatchSet`]).
//! * A Change-Id of an open cached Gerrit change (on the target branch, or
//!   the only one when the branch is unknown) makes the commits local patch
//!   sets of that change (`gerrit_number`). One of a merged or abandoned
//!   change is skipped; an existing row with unsent data moves to
//!   `Closed`, one without is deleted. Any other Change-Id is a local
//!   change of its own. New rows start as `Checking` either way, until a
//!   lookup on Gerrit confirms them.
//! * Every commit of a change that the row does not have yet becomes its
//!   next local patch set (oldest first); known ones are kept. The row's
//!   subject, author and time are those of its newest commit in the scan.
//! * Vanishing is per change, never per version, and only on a complete
//!   scan ([`LocalScan::is_complete`]): a change none of whose local
//!   commits is on a named branch any more is deleted, unless it has unsent
//!   data, then it becomes `Vanished` (the store re-checks unsent data when
//!   it applies the plan, see [`LocalOp::Delete`]). A change whose only
//!   commits left on the branches are Gerrit patch sets is deleted without
//!   unsent data and otherwise left to consolidation.
//! * A `Vanished` change whose commits are back goes back to `Checking`.
//! * Consolidated rows are tombstones: their known commits never become
//!   local again, under any key. They go when none of their commits, and
//!   no commit with their key, is on a branch any more. A *new* commit
//!   with a tombstone's key (the change amended after the push) is a new
//!   local patch set of its Gerrit change, and the row goes back to
//!   `Checking`.
//! * A `Closed` row with unsent data stays as it is, wherever its commits
//!   are, until consolidation or the user deals with its held data.
//! * On a complete scan, a row whose key the scan no longer produces but
//!   whose Change-Id it does (the target branch changed) is the same
//!   change: it is re-keyed, or merged ([`LocalOp::Merge`]) into the row
//!   that already has the new key. Closed rows and tombstones are not.
//!
//! Every op on a row names it with the state the plan saw ([`Seen`]), so a
//! plan computed before a consolidation changed a row cannot undo it: the
//! store skips such an op as stale.

use std::collections::{HashMap, HashSet};

use model::{
    BranchTargets, ChangeStatus, LocalChangeState, LocalCommit, LocalScan, PatchSetNumber,
    RevisionId, Timestamp,
};
use store::{
    CachedGerritChange, LocalChangeEntry, LocalChangeUpsert, LocalOp, NewLocalPatchSet, Seen,
};

/// A local change's key within a project: target branch and Change-Id.
type Key = (Option<String>, String);

fn key_of(e: &LocalChangeEntry) -> Key {
    (e.change.branch.clone(), e.change.change_id.clone())
}

fn seen(e: &LocalChangeEntry) -> Seen {
    Seen {
        id: e.change.id,
        state: e.change.state,
    }
}

fn normalize(email: &str) -> String {
    email.trim().to_lowercase()
}

/// The commits of one key, newest first.
struct Group<'a> {
    key: Key,
    /// Every commit of the key that passed the filters.
    all: Vec<&'a LocalCommit>,
    /// Those that are neither Gerrit patch sets nor versions of a
    /// tombstone.
    local: Vec<&'a LocalCommit>,
}

/// The cached Gerrit change that commits with `key` belong to: the one on
/// the target branch, or, with the branch unknown, the only one with the
/// Change-Id.
fn gerrit_change<'a>(
    cached: &'a [CachedGerritChange],
    key: &Key,
) -> Option<&'a CachedGerritChange> {
    let mut hits = cached
        .iter()
        .filter(|c| c.summary.change_id.as_deref() == Some(key.1.as_str()));
    match &key.0 {
        Some(branch) => hits.find(|c| c.summary.branch == *branch),
        None => {
            let first = hits.next()?;
            hits.next().is_none().then_some(first)
        }
    }
}

/// Merged or abandoned. A status this build does not know counts as open,
/// so its commits stay visible.
fn is_closed(status: &ChangeStatus) -> bool {
    matches!(status, ChangeStatus::Merged | ChangeStatus::Abandoned)
}

/// Splits the user's commits of `scan` by key.
fn groups<'a>(
    commits: &[&'a LocalCommit],
    targets: &BranchTargets,
    not_local: impl Fn(&RevisionId) -> bool,
) -> Vec<Group<'a>> {
    let mut out: Vec<Group<'a>> = Vec::new();
    let mut index: HashMap<Key, usize> = HashMap::new();
    for c in commits {
        let Some(change_id) = c.commit.change_id.clone() else {
            continue;
        };
        let key = (
            targets.target_for(&c.branches).map(str::to_owned),
            change_id,
        );
        let i = *index.entry(key.clone()).or_insert_with(|| {
            out.push(Group {
                key,
                all: Vec::new(),
                local: Vec::new(),
            });
            out.len() - 1
        });
        out[i].all.push(c);
        if !not_local(&c.commit.sha) {
            out[i].local.push(c);
        }
    }
    out
}

/// The reconcile plan for one project: see the module documentation for
/// the rules. `author_emails` are the user's emails (the account's and the
/// checkout's); `cached` the cached Gerrit changes of the project with the
/// Change-Ids of the scan and of `entries`; `entries` the store's rows for
/// the project ([`store::Store::local_changes_of_project`]); `now` the
/// time new versions are first seen.
pub fn plan_reconcile(
    scan: &LocalScan,
    author_emails: &[String],
    cached: &[CachedGerritChange],
    entries: &[LocalChangeEntry],
    now: Timestamp,
) -> Vec<LocalOp> {
    let emails: HashSet<String> = author_emails
        .iter()
        .map(|e| normalize(e))
        .filter(|e| !e.is_empty())
        .collect();
    let commits: Vec<&LocalCommit> = scan
        .commits
        .iter()
        .filter(|c| {
            !c.is_merge()
                && c.commit.change_id.is_some()
                && emails.contains(&normalize(&c.commit.author.email))
        })
        .collect();
    let on_gerrit: HashMap<&RevisionId, PatchSetNumber> = cached
        .iter()
        .flat_map(|c| c.patch_sets.iter().flatten())
        .filter(|p| !p.number.is_local())
        .map(|p| (&p.sha, p.number))
        .collect();
    // A consolidated version stays consolidated, also when its commit now
    // heads for another branch.
    let consolidated: HashSet<&RevisionId> = entries
        .iter()
        .filter(|e| e.change.state == LocalChangeState::Consolidated)
        .flat_map(|e| e.patch_sets.iter().map(|p| &p.sha))
        .collect();
    let complete = scan.is_complete();
    let groups = groups(&commits, &scan.targets, |sha| {
        on_gerrit.contains_key(sha) || consolidated.contains(sha)
    });
    let keys_all: HashSet<&Key> = groups.iter().map(|g| &g.key).collect();
    let scan_shas: HashSet<&RevisionId> = scan.commits.iter().map(|c| &c.commit.sha).collect();
    let by_key: HashMap<Key, &LocalChangeEntry> = entries.iter().map(|e| (key_of(e), e)).collect();

    let mut ops = Vec::new();
    // Versions that turned out to be Gerrit patch sets, whether or not
    // their commits are still on a branch.
    for p in entries.iter().flat_map(|e| &e.patch_sets) {
        if let Some(&n) = on_gerrit.get(&p.sha)
            && p.gerrit_patch_set != Some(n)
        {
            ops.push(LocalOp::MapPatchSet {
                patch_set: p.id,
                gerrit_patch_set: n,
            });
        }
    }

    // Rows the groups took care of; the rest are checked for vanishing.
    let mut handled = HashSet::new();
    for g in &groups {
        if g.local.is_empty() {
            continue;
        }
        let gerrit = gerrit_change(cached, &g.key);
        let exact = by_key.get(&g.key).copied();
        if gerrit.is_some_and(|c| is_closed(&c.summary.status)) {
            let Some(e) = exact else { continue };
            handled.insert(e.change.id);
            match e.change.state {
                LocalChangeState::Consolidated => {}
                LocalChangeState::Closed if e.has_unsent => {}
                LocalChangeState::Closed => ops.push(LocalOp::Delete {
                    seen: seen(e),
                    else_state: None,
                }),
                _ if e.has_unsent => ops.push(LocalOp::SetState {
                    seen: seen(e),
                    state: LocalChangeState::Closed,
                }),
                _ => ops.push(LocalOp::Delete {
                    seen: seen(e),
                    else_state: Some(LocalChangeState::Closed),
                }),
            }
            continue;
        }

        // Rows of the same change under a key the scan no longer produces.
        let mut orphans: Vec<&LocalChangeEntry> = if complete {
            entries
                .iter()
                .filter(|e| {
                    e.change.change_id == g.key.1
                        && !keys_all.contains(&key_of(e))
                        && !matches!(
                            e.change.state,
                            LocalChangeState::Consolidated | LocalChangeState::Closed
                        )
                        && !handled.contains(&e.change.id)
                })
                .collect()
        } else {
            Vec::new()
        };
        let target = match exact {
            Some(e) => Some(e),
            None if orphans.is_empty() => None,
            None => {
                // Prefer the row that has one of the commits.
                let shas: HashSet<&RevisionId> = g.all.iter().map(|c| &c.commit.sha).collect();
                let i = orphans
                    .iter()
                    .position(|e| e.patch_sets.iter().any(|p| shas.contains(&p.sha)))
                    .unwrap_or(0);
                Some(orphans.remove(i))
            }
        };
        let known: HashSet<&RevisionId> = target
            .map(|e| e.patch_sets.iter().map(|p| &p.sha).collect())
            .unwrap_or_default();
        let new: Vec<NewLocalPatchSet> = g
            .local
            .iter()
            .rev()
            .filter(|c| !known.contains(&c.commit.sha))
            .map(|c| NewLocalPatchSet {
                sha: c.commit.sha.clone(),
                parent_sha: c.commit.parents.first().cloned(),
                seen_at: now,
            })
            .collect();
        if let Some(e) = target {
            handled.insert(e.change.id);
            if e.change.state == LocalChangeState::Consolidated && new.is_empty() {
                // A tombstone whose commits are still there.
                continue;
            }
        }
        let state = target.and_then(|e| match e.change.state {
            LocalChangeState::Vanished | LocalChangeState::Consolidated => {
                Some(LocalChangeState::Checking)
            }
            _ => None,
        });
        let newest = &g.local[0].commit;
        let upsert = LocalChangeUpsert {
            seen: target.map(seen),
            branch: g.key.0.clone(),
            change_id: g.key.1.clone(),
            gerrit_number: gerrit.map(|c| c.summary.number),
            subject: newest.subject(),
            author_name: newest.author.name.clone(),
            author_email: newest.author.email.clone(),
            updated: newest.committer.time,
            state,
            patch_sets: new,
        };
        let Some(e) = target else {
            ops.push(LocalOp::Upsert(upsert));
            continue;
        };
        let c = &e.change;
        let unchanged = c.branch == upsert.branch
            && c.change_id == upsert.change_id
            && upsert
                .gerrit_number
                .is_none_or(|n| c.gerrit_number == Some(n))
            && c.subject == upsert.subject
            && c.author_name == upsert.author_name
            && c.author_email == upsert.author_email
            && c.updated == upsert.updated
            && upsert.state.is_none()
            && upsert.patch_sets.is_empty();
        if !unchanged {
            ops.push(LocalOp::Upsert(upsert));
        }
        let into = Seen {
            id: c.id,
            state: state.unwrap_or(c.state),
        };
        for from in orphans {
            handled.insert(from.change.id);
            ops.push(LocalOp::Merge {
                from: seen(from),
                into,
            });
        }
    }

    if !complete {
        return ops;
    }
    for e in entries.iter().filter(|e| !handled.contains(&e.change.id)) {
        let key = key_of(e);
        let on_branches = keys_all.contains(&key);
        let delete = |else_state| LocalOp::Delete {
            seen: seen(e),
            else_state,
        };
        match e.change.state {
            LocalChangeState::Consolidated => {
                let present =
                    on_branches || e.patch_sets.iter().any(|p| scan_shas.contains(&p.sha));
                if !present {
                    ops.push(delete(None));
                }
            }
            // Kept for their unsent data until consolidation decides.
            LocalChangeState::Closed | LocalChangeState::Vanished if e.has_unsent => {}
            // Only commits that are Gerrit patch sets are left: with unsent
            // data that is for consolidation to send.
            _ if on_branches => {
                if !e.has_unsent {
                    ops.push(delete(None));
                }
            }
            LocalChangeState::Closed => ops.push(delete(None)),
            _ => ops.push(delete(Some(LocalChangeState::Vanished))),
        }
    }
    ops
}

/// The plan for a project whose `local_changes` option is off: every row
/// without unsent data goes (the store keeps a row that has some by the
/// time the plan is applied, in its state), and nothing new is picked up.
/// Rows with unsent data stay and still consolidate, so turning the option
/// off never loses a review.
pub fn plan_disabled(entries: &[LocalChangeEntry]) -> Vec<LocalOp> {
    entries
        .iter()
        .filter(|e| !e.has_unsent)
        .map(|e| LocalOp::Delete {
            seen: seen(e),
            else_state: None,
        })
        .collect()
}

#[cfg(test)]
mod tests;
