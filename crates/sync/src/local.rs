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
//!   `Closed`, with that change's number so its held data shows there and
//!   is looked up, and one without is deleted. Any other Change-Id is a local
//!   change of its own. New rows start as `Checking` either way, until a
//!   lookup on Gerrit confirms them.
//! * Every commit of a change that the row does not have yet becomes its
//!   next local patch set (oldest first); known ones are kept. The row's
//!   subject, author and time are those of its newest commit in the scan.
//! * The HEAD rule: only commits the checked-out branch reaches
//!   ([`LocalCommit::on_head`]) make a new row. Other branches may still
//!   have what a rebase moved away from (an earlier batch, a backup, a
//!   squashed `fixup!`); their commits still add versions to rows that
//!   exist and keep rows from vanishing. On a complete scan with HEAD on a
//!   branch, a row (not a tombstone) without unsent data that HEAD reaches
//!   nothing of (no version, no commit with its key) is deleted; one with
//!   unsent data stays, and the store does not list it while HEAD reaches
//!   none of its versions ([`store::Store::mark_local_versions_on_head`]).
//!   While HEAD is detached (a rebase in progress, say) no row is made,
//!   and none goes by this rule.
//! * Vanishing is per change, never per version, only on a complete scan
//!   ([`LocalScan::is_complete`]) and never without emails. Whether a
//!   row's commits are still there goes by the whole scan, not by the
//!   filters: a row is there while any commit of the scan has its key or
//!   is one of its versions, so commits that stop matching the emails do
//!   not make it vanish. A change that is not there any more is deleted,
//!   or becomes `Vanished` if it has unsent data (`SetState` when the
//!   snapshot shows unsent data, so the scan settles at once; otherwise
//!   `Delete` with `else_state`, which the store re-checks, see
//!   [`LocalOp::Delete`]). A change whose only commits left are Gerrit
//!   patch sets is left to consolidation if it has unsent data; without,
//!   it becomes a tombstone if it has a Gerrit change (so pages naming it
//!   still redirect), and is deleted otherwise.
//! * A `Vanished` change whose commits are back goes back to `Checking`.
//! * Consolidated rows are tombstones: their known commits never become
//!   local again, under any key. They keep their Gerrit change, so a page
//!   (or history entry) still naming their synthetic numbers redirects to
//!   the Gerrit change. They go when none of their commits, and no commit
//!   with their key, is on a branch any more (on a complete scan), and
//!   [`TOMBSTONE_AGE`] has passed since they left the branches (or since
//!   the tombstone was made, if later): navigation history lives
//!   only as long as the app runs, so a tombstone that old (or one made
//!   before its time was recorded, in an earlier run) is named by no page
//!   a user is likely to go back to. A tombstone still on a branch stays,
//!   however old, since the scan needs it to not make the change local
//!   again. With the option off, tombstones go at the same age. A *new* commit
//!   with a tombstone's key (the change amended after the push) is a new
//!   local patch set of its Gerrit change, and the row goes back to
//!   `Checking`.
//! * A `Closed` row (or a tombstone) with unsent data stays as it is,
//!   wherever its commits are, until consolidation or the user deals with
//!   its data. Without unsent data it goes with its commits.
//! * On a complete scan, a row whose key the scan no longer produces but
//!   whose Change-Id it does (the target branch changed) is the same
//!   change: it is re-keyed, or merged ([`LocalOp::Merge`]) into the row
//!   that already has the new key. Closed rows and tombstones are not. A
//!   truncated scan does neither, and gives a commit that a row of the
//!   change already has no second row.
//!
//! Every op on a row names it with the state the plan saw ([`Seen`]), so a
//! plan computed before a consolidation changed a row cannot undo it: the
//! store skips such an op as stale.

use std::collections::{BTreeSet, HashMap, HashSet};

use jiff::SignedDuration;
use model::{
    BranchTargets, ChangeStatus, LocalChangeState, LocalCommit, LocalScan, PatchSetNumber,
    RevisionId, Timestamp,
};
use store::{
    CachedGerritChange, LocalChangeEntry, LocalChangeUpsert, LocalOp, NewLocalPatchSet, Seen,
};

/// How old a tombstone must be before the scan removes it once its commits
/// are off every named branch: long enough that no page of this run still
/// names its synthetic numbers (pages redirect as soon as consolidation
/// finds a change, and history is not kept across runs).
pub const TOMBSTONE_AGE: SignedDuration = SignedDuration::from_hours(7 * 24);

/// Whether tombstone `e` is old enough to go at `now`: its versions left
/// the named branches, and that and its becoming a tombstone, whichever is
/// later (the last time a page could name it), are [`TOMBSTONE_AGE`] ago.
/// One whose leaving is not recorded yet waits for the next complete scan,
/// unless it was also made before its time was recorded (in an earlier
/// run, which no page of this run names).
pub fn tombstone_due(e: &LocalChangeEntry, now: Timestamp) -> bool {
    let Ok(limit) = now.checked_sub(TOMBSTONE_AGE) else {
        return false;
    };
    match (e.off_branch_since, e.consolidated_at) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(off), made) => made.map_or(off, |m| m.max(off)) <= limit,
    }
}

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
    // Whether a row's commits are still there goes by the whole scan, not
    // only the commits that pass the filters: a row must not vanish because
    // its commits stopped matching the emails.
    let key_for = |c: &LocalCommit| -> Option<Key> {
        let change_id = c.commit.change_id.clone()?;
        Some((
            scan.targets.target_for(&c.branches).map(str::to_owned),
            change_id,
        ))
    };
    let scan_keys: HashSet<Key> = scan.commits.iter().filter_map(key_for).collect();
    let by_key: HashMap<Key, &LocalChangeEntry> = entries.iter().map(|e| (key_of(e), e)).collect();
    // A row that goes by the HEAD rule (see the module documentation).
    let head_rule = complete && !scan.detached_head && !emails.is_empty();
    let off_head = |e: &LocalChangeEntry| -> bool {
        if !head_rule || e.has_unsent || e.change.state == LocalChangeState::Consolidated {
            return false;
        }
        let key = key_of(e);
        let shas: HashSet<&RevisionId> = e.patch_sets.iter().map(|p| &p.sha).collect();
        !scan.commits.iter().any(|c| {
            c.on_head && (shas.contains(&c.commit.sha) || key_for(c).as_ref() == Some(&key))
        })
    };

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
            let c = &e.change;
            let number = gerrit.map(|g| g.summary.number);
            match c.state {
                LocalChangeState::Consolidated => {}
                // Closed, with the Gerrit change it belongs to, so its
                // held data shows there and is looked up.
                _ if e.has_unsent => {
                    if c.state != LocalChangeState::Closed || c.gerrit_number != number {
                        ops.push(LocalOp::Upsert(LocalChangeUpsert {
                            seen: Some(seen(e)),
                            branch: c.branch.clone(),
                            change_id: c.change_id.clone(),
                            gerrit_number: number,
                            subject: c.subject.clone(),
                            author_name: c.author_name.clone(),
                            author_email: c.author_email.clone(),
                            updated: c.updated,
                            state: Some(LocalChangeState::Closed),
                            patch_sets: Vec::new(),
                        }));
                    }
                }
                // If unsent data shows up before the plan is applied, the
                // row is kept and the next scan closes it.
                _ => ops.push(LocalOp::Delete {
                    seen: seen(e),
                    else_state: None,
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
                        && !scan_keys.contains(&key_of(e))
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
        if exact.is_none() && !complete {
            // A truncated scan cannot tell a re-key from a second copy, so
            // a commit that a row of the change already has gets no row of
            // its own: it would show twice until the next complete scan
            // merged them. A complete scan re-keys or merges.
            let has_one = entries.iter().any(|e| {
                e.change.change_id == g.key.1
                    && e.change.state != LocalChangeState::Consolidated
                    && e.patch_sets
                        .iter()
                        .any(|p| g.local.iter().any(|c| c.commit.sha == p.sha))
            });
            if has_one {
                continue;
            }
        }
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
        let group_on_head = g.all.iter().any(|c| c.on_head);
        match target {
            // Only HEAD makes new changes.
            None if !group_on_head => continue,
            Some(e) if !group_on_head && off_head(e) => {
                handled.insert(e.change.id);
                ops.push(LocalOp::Delete {
                    seen: seen(e),
                    else_state: None,
                });
                continue;
            }
            _ => {}
        }
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

    // Without emails no commit is the user's, which says nothing about
    // whether the rows' commits are still there.
    if !complete || emails.is_empty() {
        return ops;
    }
    for e in entries.iter().filter(|e| !handled.contains(&e.change.id)) {
        let key = key_of(e);
        // The scan's commits of this row: with its key, or one of its
        // versions (under any key, by any author).
        let shas: HashSet<&RevisionId> = e.patch_sets.iter().map(|p| &p.sha).collect();
        let mut there = scan
            .commits
            .iter()
            .filter(|c| shas.contains(&c.commit.sha) || key_for(c).as_ref() == Some(&key))
            .peekable();
        let present = there.peek().is_some();
        let only_pushed = present && there.all(|c| on_gerrit.contains_key(&c.commit.sha));
        let delete = |else_state| LocalOp::Delete {
            seen: seen(e),
            else_state,
        };
        match e.change.state {
            // Kept for their unsent data until consolidation decides (a
            // tombstone with unsent data is one that a draft reached late).
            LocalChangeState::Closed
            | LocalChangeState::Vanished
            | LocalChangeState::Consolidated
                if e.has_unsent => {}
            LocalChangeState::Consolidated => {
                if !present && tombstone_due(e, now) {
                    ops.push(delete(None));
                }
            }
            // Only commits that are Gerrit patch sets are left: without
            // unsent data there is nothing local to it any more; with some,
            // that is for consolidation to send. One with a Gerrit change
            // becomes a tombstone, so pages naming it still redirect.
            _ if only_pushed => {
                if e.has_unsent {
                } else if e.change.gerrit_number.is_some() {
                    ops.push(LocalOp::SetState {
                        seen: seen(e),
                        state: LocalChangeState::Consolidated,
                    });
                } else {
                    ops.push(delete(None));
                }
            }
            // Still on a branch (also when its commits no longer pass the
            // filters): nothing to do, unless HEAD reaches none of it.
            _ if present => {
                if off_head(e) {
                    ops.push(delete(None));
                }
            }
            LocalChangeState::Closed => ops.push(delete(None)),
            // Vanished. A row known to have unsent data is marked so
            // directly, so the scan settles at once; for the others the
            // store re-checks when it applies the plan.
            _ if e.has_unsent => ops.push(LocalOp::SetState {
                seen: seen(e),
                state: LocalChangeState::Vanished,
            }),
            _ => ops.push(delete(Some(LocalChangeState::Vanished))),
        }
    }
    ops
}

/// The Change-Ids of a scan's commits, sorted, each once: what a scanner
/// keeps for [`Syncer::local_generation`](crate::Syncer::local_generation).
pub fn scan_change_ids(scan: &LocalScan) -> Vec<String> {
    let ids: BTreeSet<String> = scan
        .commits
        .iter()
        .filter_map(|c| c.commit.change_id.clone())
        .collect();
    ids.into_iter().collect()
}

/// The plan for a project whose `local_changes` option is off: every row
/// without unsent data goes (the store keeps a row that has some by the
/// time the plan is applied, in its state), and nothing new is picked up.
/// Rows with unsent data stay and still consolidate, so turning the option
/// off never loses a review. A tombstone goes only once it is
/// [`TOMBSTONE_AGE`] old at `now`, so its redirects keep working.
pub fn plan_disabled(entries: &[LocalChangeEntry], now: Timestamp) -> Vec<LocalOp> {
    entries
        .iter()
        .filter(|e| !e.has_unsent)
        .filter(|e| e.change.state != LocalChangeState::Consolidated || tombstone_due(e, now))
        .map(|e| LocalOp::Delete {
            seen: seen(e),
            else_state: None,
        })
        .collect()
}

pub mod search;

#[cfg(test)]
mod tests;
