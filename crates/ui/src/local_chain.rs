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
//! commit on the local branches that contain it.

use gpui_kit::{AppContext as _, AsyncApp, Entity};
use local_git::{Local, Series};
use model::{CommitInfo, RevisionId};
use sync::{ChainCommit, Syncer};

use crate::checkouts::{CheckoutState, Checkouts};

/// Commits read on each side of the commit, at most.
const LIMIT: usize = 25;

/// One commit of a chain read from the checkout and what it is.
#[derive(Clone, Debug)]
pub struct ChainEntry {
    pub commit: CommitInfo,
    pub kind: ChainCommit,
}

/// The chain of `sha` in the checkout of `project`, newest first: what was
/// built on it, the commit itself, then what it was built on (see the
/// module comment). `None` when there is no open checkout, it does not
/// have the commit, or git or the store failed (logged). Git and SQLite
/// run on the background executor.
pub async fn read(
    checkouts: Option<Entity<Checkouts>>,
    project: String,
    sha: RevisionId,
    syncer: Syncer,
    cx: &mut AsyncApp,
) -> Option<Vec<ChainEntry>> {
    let checkout = Checkouts::opened(&checkouts?, &project, cx).await?;
    let CheckoutState::Open(repo) = checkout.state else {
        return None;
    };
    cx.background_spawn(async move {
        let series = match repo.series(&sha, LIMIT) {
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
            ancestors,
            ..
        } = series;
        let commits: Vec<CommitInfo> = descendants.into_iter().chain(ancestors).collect();
        let kinds = match syncer.classify_chain(&project, &commits) {
            Ok(k) => k,
            Err(e) => {
                log::warn!("naming the relation chain of {sha}: {e}");
                return None;
            }
        };
        Some(cut(&sha, commits.into_iter().zip(kinds).collect()))
    })
    .await
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
