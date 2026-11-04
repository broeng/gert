//! The relation chain of a commit that only the checkout has: a local
//! change, or a local version of a Gerrit change. Gerrit's related-changes
//! API does not know the commit, so the chain is read from git
//! ([`local_git::Repo::series`]) and each commit is named from the store
//! ([`Syncer::classify_chain`]).
//!
//! The series goes down the first parents until a remote-tracking branch
//! has the commit (merged history is no part of a chain), and stops after
//! the first Gerrit patch set: the chain below it is that change's own,
//! which its page shows from Gerrit. Up, it takes what was built on the
//! commit on the checked-out branch, or when that does not have it on the
//! local branches that do, keeping only the newest version of each change
//! there ([`drop_older_versions`]): a branch left behind by a rebase still
//! has the older ones.
//!
//! The chain of a Gerrit patch set is Gerrit's, but the checkout may have
//! local work built on it: [`read_stack`] reads that, for the change page
//! to put above the patch set in Gerrit's chain.

use std::collections::HashMap;

use gpui_kit::{AppContext as _, AsyncApp, Entity};
use local_git::{Local, Series};
use model::{ChangeNumber, CommitInfo, RevisionId};
use sync::{ChainCommit, Syncer};

use crate::checkouts::{CheckoutState, Checkouts};

/// Commits read on each side of the commit, at most. The chain shows all
/// of them: the walk already ends at history a remote-tracking branch has
/// (and the chain at the first Gerrit patch set below), so this only
/// guards against a checkout without remote-tracking branches, whose
/// whole history would otherwise be read.
const GUARD: usize = 1000;

/// A chain read from the checkout.
#[derive(Clone, Debug, Default)]
pub struct Chain {
    /// Newest first.
    pub entries: Vec<ChainEntry>,
}

/// One commit of a chain read from the checkout and what it is.
#[derive(Clone, Debug)]
pub struct ChainEntry {
    pub commit: CommitInfo,
    pub kind: ChainCommit,
}

/// The chain of `sha` in the checkout of `project`, newest first: what was
/// built on it, the commit itself, then what it was built on (see the
/// module comment), and whether a limit left commits out. `None` when there is no open checkout, it does not
/// have the commit, or git or the store failed (logged). Git and SQLite
/// run on the background executor.
pub async fn read(
    checkouts: Option<Entity<Checkouts>>,
    project: String,
    sha: RevisionId,
    syncer: Syncer,
    cx: &mut AsyncApp,
) -> Option<Chain> {
    let checkout = Checkouts::opened(&checkouts?, &project, cx).await?;
    let CheckoutState::Open(repo) = checkout.state else {
        return None;
    };
    cx.background_spawn(async move {
        let series = match repo.series(&sha, GUARD) {
            Ok(Local::Available(s)) => s,
            Ok(Local::NotLocal(why)) => {
                log::debug!("no local relation chain for {sha}: {why}");
                return None;
            }
            Err(e) => {
                log::warn!("reading the relation chain of {sha}: {e}");
                return None;
            }
        };
        let Series {
            descendants,
            more_descendants,
            ancestors,
            more_ancestors,
        } = series;
        if more_descendants || more_ancestors {
            log::debug!("the relation chain of {sha} has more than {GUARD} commits on a side");
        }
        let above = descendants.len();
        let commits: Vec<CommitInfo> = descendants.into_iter().chain(ancestors).collect();
        let kinds = match syncer.classify_chain(&project, &commits) {
            Ok(k) => k,
            Err(e) => {
                log::warn!("naming the relation chain of {sha}: {e}");
                return None;
            }
        };
        let mut entries: Vec<_> = commits.into_iter().zip(kinds).collect();
        let below = entries.split_off(above);
        drop_older_versions(&mut entries);
        entries.extend(below);
        let entries = cut(&sha, entries);
        Some(Chain { entries })
    })
    .await
}

/// The local commits the checkout built on a Gerrit patch set, for the
/// relation chain Gerrit gives of it (see [`read_stack`]).
#[derive(Clone, Debug, Default)]
pub struct Stack {
    /// The local changes and local versions of Gerrit changes among the
    /// commits built on the patch set, newest first and never a parent
    /// before its children. Where those commits fork onto several
    /// branches the list keeps each line together (git's topological
    /// order); it cannot show the tree.
    pub entries: Vec<ChainEntry>,
    /// The Gerrit changes whose patch sets are among those commits: the
    /// local ones go above them.
    pub on_gerrit: Vec<ChangeNumber>,
}

/// What the checkout of `project` built on Gerrit patch set `sha` on its
/// local branches, as far as it is local work: local changes and local
/// versions of Gerrit changes ([`Stack`]). `None` when the project does
/// not have the `local_changes` option, there is no open checkout, it does
/// not have the commit, or git or the store failed (logged). Git and
/// SQLite run on the background executor; no network.
pub async fn read_stack(
    checkouts: Option<Entity<Checkouts>>,
    project: String,
    sha: RevisionId,
    syncer: Syncer,
    cx: &mut AsyncApp,
) -> Option<Stack> {
    let checkout = Checkouts::opened(&checkouts?, &project, cx).await?;
    if !checkout.local_changes {
        return None;
    }
    let CheckoutState::Open(repo) = checkout.state else {
        return None;
    };
    cx.background_spawn(async move {
        let series = match repo.series(&sha, GUARD) {
            Ok(Local::Available(s)) => s,
            Ok(Local::NotLocal(why)) => {
                log::debug!("no local commits on {sha}: {why}");
                return None;
            }
            Err(e) => {
                log::warn!("reading the local commits on {sha}: {e}");
                return None;
            }
        };
        let kinds = match syncer.classify_chain(&project, &series.descendants) {
            Ok(k) => k,
            Err(e) => {
                log::warn!("naming the local commits on {sha}: {e}");
                return None;
            }
        };
        if series.more_descendants {
            log::debug!("more than {GUARD} commits were built on {sha}");
        }
        let mut built_on: Vec<_> = series.descendants.into_iter().zip(kinds).collect();
        drop_older_versions(&mut built_on);
        let mut stack = Stack::default();
        for (commit, kind) in built_on {
            match kind {
                ChainCommit::Local { .. } | ChainCommit::LocalVersion { .. } => {
                    stack.entries.push(ChainEntry { commit, kind })
                }
                ChainCommit::Gerrit { change, .. } => stack.on_gerrit.push(change),
                ChainCommit::Unknown => {}
            }
        }
        Some(stack)
    })
    .await
}

/// Leaves out of `entries` (commits built on the one on show) each version
/// of a change that has a newer one among them. They come from several
/// branches only when the checked-out branch does not have the commit on
/// show (see [`local_git::Repo::series`]), and a branch a rebase left
/// behind still has the older versions. Local versions are newer than the
/// change's Gerrit patch sets; commits the store does not know all stay.
fn drop_older_versions(entries: &mut Vec<(CommitInfo, ChainCommit)>) {
    let version = |kind: &ChainCommit| match kind {
        ChainCommit::Local {
            change, ordinal, ..
        }
        | ChainCommit::LocalVersion {
            change, ordinal, ..
        } => Some((*change, (1, *ordinal))),
        ChainCommit::Gerrit {
            change, patch_set, ..
        } => Some((*change, (0, patch_set.0))),
        ChainCommit::Unknown => None,
    };
    let mut newest: HashMap<ChangeNumber, (u8, u32)> = HashMap::new();
    for (change, v) in entries.iter().filter_map(|(_, k)| version(k)) {
        let at = newest.entry(change).or_insert(v);
        *at = (*at).max(v);
    }
    entries.retain(|(_, k)| version(k).is_none_or(|(change, v)| newest.get(&change) == Some(&v)));
}

/// `entries` without what comes after the first Gerrit patch set below
/// `sha`.
fn cut(sha: &RevisionId, entries: Vec<(CommitInfo, ChainCommit)>) -> Vec<ChainEntry> {
    let at = entries.iter().position(|(c, _)| c.sha == *sha);
    let end = at
        .and_then(|at| {
            entries[at + 1..]
                .iter()
                .position(|(_, k)| matches!(k, ChainCommit::Gerrit { .. }))
                .map(|i| at + 1 + i + 1)
        })
        .unwrap_or(entries.len());
    entries
        .into_iter()
        .take(end)
        .map(|(commit, kind)| ChainEntry { commit, kind })
        .collect()
}

/// How a chain entry is named: `local` for a local change (its number is
/// synthetic), `#7 local 2` for a local version of a Gerrit change, `#7
/// PS 3` for a Gerrit patch set, and the short commit id for anything else.
pub fn label(entry: &ChainEntry) -> String {
    match &entry.kind {
        ChainCommit::Local { .. } => "local".to_owned(),
        ChainCommit::LocalVersion {
            change, ordinal, ..
        } => format!("#{change} local {ordinal}"),
        ChainCommit::Gerrit {
            change, patch_set, ..
        } => format!("#{change} PS {patch_set}"),
        ChainCommit::Unknown => entry.commit.sha.as_str().chars().take(7).collect(),
    }
}

#[cfg(test)]
mod tests {
    use model::{ChangeNumber, PatchSetNumber, Signature, Timestamp};

    use super::*;

    fn commit(sha: &str) -> CommitInfo {
        let sig = Signature {
            name: "Me".into(),
            email: "me@example.com".into(),
            time: Timestamp::UNIX_EPOCH,
            tz: 0,
        };
        CommitInfo {
            sha: sha.into(),
            parents: Vec::new(),
            author: sig.clone(),
            committer: sig,
            message: format!("{sha}\n"),
            change_id: None,
        }
    }

    fn gerrit(n: u64, ps: u32) -> ChainCommit {
        ChainCommit::Gerrit {
            change: ChangeNumber(n),
            patch_set: PatchSetNumber(ps),
            current: None,
            summary: None,
        }
    }

    fn shas(entries: &[ChainEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.commit.sha.as_str()).collect()
    }

    #[test]
    fn the_chain_ends_at_the_first_gerrit_patch_set_below_the_commit() {
        let entries = vec![
            (commit("up"), gerrit(9, 1)),
            (commit("me"), ChainCommit::Unknown),
            (commit("mid"), ChainCommit::Unknown),
            (commit("g1"), gerrit(7, 2)),
            (commit("g0"), gerrit(6, 1)),
        ];
        let cut = cut(&"me".into(), entries.clone());
        // A patch set above stays; below, the first one is the last row.
        assert_eq!(shas(&cut), ["up", "me", "mid", "g1"]);
        // Without a patch set below, everything stays.
        assert_eq!(
            shas(&super::cut(&"me".into(), entries[..3].to_vec())),
            ["up", "me", "mid"]
        );
    }

    fn local(n: u64, ordinal: u32) -> ChainCommit {
        ChainCommit::Local {
            change: ChangeNumber(ChangeNumber::LOCAL_BASE + n),
            patch_set: PatchSetNumber(PatchSetNumber::LOCAL_BASE + ordinal),
            ordinal,
            newest: false,
        }
    }

    #[test]
    fn only_the_newest_version_of_a_change_stays_above_the_commit() {
        let version = ChainCommit::LocalVersion {
            change: ChangeNumber(7),
            patch_set: PatchSetNumber(PatchSetNumber::LOCAL_BASE + 1),
            ordinal: 1,
        };
        let mut entries = vec![
            (commit("w1"), local(1, 1)),
            (commit("w3"), local(1, 3)),
            (commit("x"), ChainCommit::Unknown),
            (commit("w2"), local(1, 2)),
            (commit("y"), ChainCommit::Unknown),
            (commit("g7"), gerrit(7, 2)),
            (commit("v7"), version),
            (commit("g7-old"), gerrit(7, 1)),
            (commit("g8"), gerrit(8, 1)),
        ];
        drop_older_versions(&mut entries);
        let entries: Vec<ChainEntry> = entries
            .into_iter()
            .map(|(commit, kind)| ChainEntry { commit, kind })
            .collect();
        assert_eq!(shas(&entries), ["w3", "x", "y", "v7", "g8"]);
    }

    #[test]
    fn labels_never_show_a_synthetic_number() {
        let local = ChainEntry {
            commit: commit("a"),
            kind: ChainCommit::Local {
                change: ChangeNumber(ChangeNumber::LOCAL_BASE + 1),
                patch_set: PatchSetNumber(PatchSetNumber::LOCAL_BASE + 1),
                ordinal: 1,
                newest: true,
            },
        };
        assert_eq!(label(&local), "local");
        let version = ChainEntry {
            commit: commit("b"),
            kind: ChainCommit::LocalVersion {
                change: ChangeNumber(7),
                patch_set: PatchSetNumber(PatchSetNumber::LOCAL_BASE + 3),
                ordinal: 2,
            },
        };
        assert_eq!(label(&version), "#7 local 2");
        let ps = ChainEntry {
            commit: commit("c"),
            kind: gerrit(7, 3),
        };
        assert_eq!(label(&ps), "#7 PS 3");
        let other = ChainEntry {
            commit: commit("0123456789abcdef"),
            kind: ChainCommit::Unknown,
        };
        assert_eq!(label(&other), "0123456");
    }
}
