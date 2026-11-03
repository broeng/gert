//! Where pages and dialogs get a [`ChangeDetail`] from: the one place that
//! adds what exists only in the local checkout.
//!
//! * A **Gerrit change**'s detail, cached or fresh, gets the change's local
//!   patch sets (versions in the checkout that are not a Gerrit patch set)
//!   as extra [`Revision`]s with their synthetic numbers and commits. They
//!   are told apart by [`PatchSetNumber::is_local`]; Gerrit's fields they
//!   cannot have (kind, uploader, ref, description) stay `None`, and
//!   `created` is when the version was first seen in the checkout.
//! * A **local change** has no Gerrit detail: [`cached`] synthesises one
//!   from its store row and versions (subject, project, branch, Change-Id,
//!   owner, status open, its versions as the revisions, the newest one
//!   current, and labels borrowed from a cached change), and git fills in
//!   the commits ([`load_commits`], [`apply_commits`]): message, author,
//!   committer, parents and dates. Nothing else: no reviewers, votes,
//!   requirements, actions or messages.
//!
//! The result is for the views only. The store never sees it:
//! `Syncer::refresh_change` stores what Gerrit sent before anything here
//! runs, so `sync::diff_change` never takes a local patch set for a new
//! one.
//!
//! **Patch set order.** A detail's `revisions` is the order the pages work
//! in (by position, never by comparing numbers): Gerrit's patch sets by
//! number, and each local one after every Gerrit patch set created at or
//! before the moment the version was first seen, i.e. after the patch set
//! that was current then (and after the local versions seen before it). A
//! Gerrit patch set uploaded later comes after it. In practice local
//! versions are newer than everything on Gerrit, so they are appended.

use std::collections::{BTreeMap, HashMap, HashSet};

use gpui_kit::{AppContext as _, AsyncApp, Entity};
use local_git::{Local, Repo};
use model::{
    Account, AccountId, ChangeDetail, ChangeNumber, ChangeStatus, ChangeSummary, Commit,
    CommitInfo, CommitParent, GitPerson, LabelValue, LocalChange, LocalPatchSet, PatchSetNumber,
    Revision, RevisionId, Signature, Timestamp,
};
use store::BorrowedLabels;
use sync::Syncer;

use crate::checkouts::{CheckoutState, Checkouts};

/// What [`cached`] found for a change.
#[derive(Clone, Debug)]
pub struct Provided {
    pub detail: ChangeDetail,
    /// When Gerrit's part was fetched; `None` for a local change.
    pub fetched_at: Option<Timestamp>,
    /// The store row of a local change. Code that matches branches or
    /// builds lookup keys must read its `branch` (`None` when the target
    /// branch is unknown), never the synthesised `summary.branch`, which is
    /// an empty placeholder then.
    pub local: Option<LocalChange>,
}

/// The detail of `number` from the store, merged or synthesised (see the
/// module comment), without the commits git adds. No network, no git.
/// `None` when nothing is cached (a Gerrit change) or the row is gone (a
/// local change). Blocking (SQLite): call from the background executor.
pub fn cached(syncer: &Syncer, number: ChangeNumber) -> Result<Option<Provided>, sync::Error> {
    if number.is_local() {
        let Some((row, versions)) = syncer.local_change(number)? else {
            return Ok(None);
        };
        let labels = syncer.labels_to_borrow(&row.project, row.branch.as_deref())?;
        let account = syncer.account()?;
        let detail = synthesise(&row, &versions, labels, account);
        return Ok(Some(Provided {
            detail,
            fetched_at: None,
            local: Some(row),
        }));
    }
    let Some(c) = syncer.cached_change(number)? else {
        return Ok(None);
    };
    Ok(Some(Provided {
        detail: merged(syncer, c.detail)?,
        fetched_at: Some(c.fetched_at),
        local: None,
    }))
}

/// The local patch sets of Gerrit change `number` as revisions, for
/// [`merge`]. Empty for a local change. Blocking (SQLite).
pub fn local_revisions(
    syncer: &Syncer,
    number: ChangeNumber,
) -> Result<Vec<Revision>, sync::Error> {
    Ok(syncer
        .local_patch_sets_of(number)?
        .iter()
        .map(local_revision)
        .collect())
}

/// `fresh`, a detail from Gerrit, with the change's local patch sets.
/// Blocking (SQLite).
pub fn merged(syncer: &Syncer, fresh: ChangeDetail) -> Result<ChangeDetail, sync::Error> {
    let locals = local_revisions(syncer, fresh.summary.number)?;
    Ok(merge(fresh, locals))
}

/// `fresh` (from Gerrit, after a write) with the local patch sets `old`,
/// the detail on screen, has: for the foreground, where the store is not
/// read. The next refresh reads them from the store again.
pub fn carry_local(fresh: ChangeDetail, old: Option<&ChangeDetail>) -> ChangeDetail {
    let locals = shown_locals(old, fresh.summary.number);
    merge(fresh, locals)
}

/// The local revisions of `shown` when it is a detail of change `number`,
/// for [`carry_local`] and [`refreshed`].
pub fn shown_locals(shown: Option<&ChangeDetail>, number: ChangeNumber) -> Vec<Revision> {
    shown
        .filter(|d| d.summary.number == number)
        .map(|d| {
            d.revisions
                .iter()
                .filter(|r| r.number.is_local())
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Gerrit change `number` fetched now (`Syncer::refresh_change` stores
/// it) with its local patch sets from the store. When the store cannot
/// be read, `shown` (the local revisions of the detail on screen, from
/// [`shown_locals`]) are carried over instead, as in [`carry_local`].
/// Blocking (network, SQLite): call from the background executor.
pub async fn refreshed(
    syncer: &Syncer,
    number: ChangeNumber,
    shown: Vec<Revision>,
) -> Result<ChangeDetail, sync::Error> {
    let fresh = syncer.refresh_change(number).await?;
    Ok(match local_revisions(syncer, number) {
        Ok(locals) => merge(fresh, locals),
        Err(e) => {
            log::warn!("reading the local patch sets of {number}: {e}");
            merge(fresh, shown)
        }
    })
}

/// `detail` with `locals` (local revisions in first-seen order) placed
/// among its Gerrit patch sets (see the module comment). Local revisions
/// already in `detail` are replaced; a local version whose commit is a
/// Gerrit patch set by now is left out.
pub fn merge(mut detail: ChangeDetail, locals: Vec<Revision>) -> ChangeDetail {
    detail.revisions.retain(|r| !r.number.is_local());
    for local in locals {
        if !local.number.is_local() || detail.revisions.iter().any(|r| r.sha == local.sha) {
            continue;
        }
        let seen = local.created;
        let at = detail
            .revisions
            .iter()
            .rposition(|r| {
                r.number.is_local()
                    || match (r.created, seen) {
                        (Some(created), Some(seen)) => created <= seen,
                        _ => true,
                    }
            })
            .map_or(0, |i| i + 1);
        detail.revisions.insert(at, local);
    }
    detail
}

/// A local version as a revision: its synthetic number and commit, first
/// seen as `created`, nothing else known yet.
fn local_revision(v: &LocalPatchSet) -> Revision {
    Revision {
        number: v.number,
        sha: v.sha.clone(),
        kind: None,
        created: Some(v.seen_at),
        uploader: None,
        ref_name: None,
        description: None,
        commit: None,
        local_ordinal: Some(v.ordinal),
    }
}

/// The detail of a local change from its row and versions (see the module
/// comment). The commit message and the dates from git come later.
fn synthesise(
    row: &LocalChange,
    versions: &[LocalPatchSet],
    labels: Option<BorrowedLabels>,
    account: Option<(AccountId, Option<String>)>,
) -> ChangeDetail {
    // The owner is the signed-in account, as stored when it was last
    // fetched: discovery only takes commits whose author email is the
    // account's or one of the checkout's own author emails, so every local
    // change is the user's. Name and email are the commit author's. Before
    // the account was ever fetched, the id is 0 (no account).
    let (id, username) = account.unwrap_or((AccountId(0), None));
    let owner = Account {
        id,
        name: Some(row.author_name.clone()).filter(|n| !n.trim().is_empty()),
        email: Some(row.author_email.clone()),
        username,
        display_name: None,
    };
    let revisions: Vec<Revision> = versions.iter().map(local_revision).collect();
    let newest = revisions.last();
    let (permitted_labels, label_values) = labels_of(labels);
    ChangeDetail {
        summary: ChangeSummary {
            number: row.number,
            change_id: Some(row.change_id.clone()),
            project: row.project.clone(),
            // Empty for an unknown target branch; the page says so. Not a
            // key: see `Provided::local`.
            branch: row.branch.clone().unwrap_or_default(),
            subject: row.subject.clone(),
            status: ChangeStatus::New,
            owner,
            created: versions.first().map_or(row.first_seen, |v| v.seen_at),
            updated: row.updated,
            insertions: 0,
            deletions: 0,
            labels: BTreeMap::new(),
            work_in_progress: false,
            is_private: false,
            unresolved_comment_count: 0,
            total_comment_count: 0,
            topic: None,
            hashtags: Vec::new(),
            attention_set: Vec::new(),
            submittable: None,
            mergeable: None,
        },
        current_revision: newest.map(|r| r.sha.clone()),
        current_patch_set: newest.map(|r| r.number),
        commit_message: None,
        reviewers: Vec::new(),
        cc: Vec::new(),
        revisions,
        messages: Vec::new(),
        submit_requirements: Vec::new(),
        votes: BTreeMap::new(),
        attention: Vec::new(),
        permitted_labels,
        removable_reviewers: Vec::new(),
        label_values,
        change_actions: BTreeMap::new(),
        revision_actions: BTreeMap::new(),
    }
}

/// The labels a local change shows: the borrowed ones, else Code-Review
/// -2..+2 with Gerrit's standard texts. A closed lender permits only
/// post-submit votes (often just the maximum), and a lender may permit
/// nothing at all: then every value the label defines is offered.
fn labels_of(
    labels: Option<BorrowedLabels>,
) -> (
    BTreeMap<String, Vec<i32>>,
    BTreeMap<String, Vec<LabelValue>>,
) {
    match labels {
        Some(b) if b.open && b.permitted.values().any(|v| !v.is_empty()) => (b.permitted, b.values),
        Some(b) => {
            let permitted = b
                .values
                .iter()
                .map(|(name, values)| (name.clone(), values.iter().map(|v| v.value).collect()))
                .collect();
            (permitted, b.values)
        }
        None => default_labels(),
    }
}

/// Code-Review -2..+2 as Gerrit defines it by default.
pub fn default_labels() -> (
    BTreeMap<String, Vec<i32>>,
    BTreeMap<String, Vec<LabelValue>>,
) {
    const TEXTS: [(i32, &str); 5] = [
        (-2, "This shall not be submitted"),
        (-1, "I would prefer this is not submitted as is"),
        (0, "No score"),
        (1, "Looks good to me, but someone else must approve"),
        (2, "Looks good to me, approved"),
    ];
    let name = "Code-Review".to_owned();
    let values = TEXTS
        .iter()
        .map(|&(value, text)| LabelValue {
            value,
            description: text.to_owned(),
        })
        .collect();
    (
        BTreeMap::from([(name.clone(), TEXTS.iter().map(|t| t.0).collect())]),
        BTreeMap::from([(name, values)]),
    )
}

// ---- commits from git ----

/// Local commits read from the checkout, by SHA; `None` for one the
/// checkout does not have (so it is not asked again).
#[derive(Clone, Debug, Default)]
pub struct GitCommits {
    found: HashMap<RevisionId, Option<Commit>>,
    /// Commits git failed to read (the checkout moved, permissions): not
    /// asked again until [`GitCommits::forget_failures`], so that a broken
    /// checkout costs one git call per detail, not a loop.
    failed: HashSet<RevisionId>,
}

impl GitCommits {
    /// The local revisions of `detail` that were not looked up yet.
    pub fn wanted(&self, detail: &ChangeDetail) -> Vec<RevisionId> {
        detail
            .revisions
            .iter()
            .filter(|r| r.number.is_local() && r.commit.is_none())
            .filter(|r| !self.found.contains_key(&r.sha) && !self.failed.contains(&r.sha))
            .map(|r| r.sha.clone())
            .collect()
    }

    /// Takes in what another lookup found.
    pub fn extend(&mut self, other: GitCommits) {
        for sha in other.found.keys() {
            self.failed.remove(sha);
        }
        self.found.extend(other.found);
        self.failed.extend(other.failed);
    }

    /// Lets the commits git failed to read be asked for again: for a new
    /// detail (a refresh), so a checkout that comes back is used again.
    pub fn forget_failures(&mut self) {
        self.failed.clear();
    }

    /// Reads `shas` from `repo`. Missing commits are remembered as such,
    /// git failures as failed (logged). Blocking.
    pub fn read(repo: &Repo, shas: &[RevisionId]) -> Self {
        let mut out = Self::default();
        for sha in shas {
            match repo.commit(sha) {
                Ok(Local::Available(info)) => {
                    out.found.insert(sha.clone(), Some(commit_of(&info)));
                }
                Ok(Local::NotLocal(why)) => {
                    log::debug!("local version {sha} is not in the checkout: {why}");
                    out.found.insert(sha.clone(), None);
                }
                Err(e) => {
                    log::warn!("reading local version {sha}: {e}");
                    out.failed.insert(sha.clone());
                }
            }
        }
        out
    }
}

/// A commit read from the checkout, as Gerrit would describe it.
fn commit_of(info: &CommitInfo) -> Commit {
    let person = |s: &Signature| GitPerson {
        name: Some(s.name.clone()),
        email: Some(s.email.clone()),
        date: Some(s.time),
        tz: Some(s.tz),
    };
    Commit {
        sha: None,
        parents: info
            .parents
            .iter()
            .map(|p| CommitParent {
                sha: p.clone(),
                subject: None,
            })
            .collect(),
        author: Some(person(&info.author)),
        committer: Some(person(&info.committer)),
        subject: info.subject(),
        message: Some(info.message.clone()),
    }
}

/// Puts the commits of `git` on the local revisions of `detail`. For a
/// local change it also takes the commit message of the current (newest)
/// version, `updated` from its committer and `created` from the oldest
/// known version's author.
pub fn apply_commits(detail: &mut ChangeDetail, git: &GitCommits) {
    for r in detail.revisions.iter_mut().filter(|r| r.number.is_local()) {
        if r.commit.is_none()
            && let Some(Some(c)) = git.found.get(&r.sha)
        {
            r.commit = Some(c.clone());
        }
    }
    if !detail.summary.number.is_local() {
        return;
    }
    if let Some(c) = detail.current().and_then(|r| r.commit.clone()) {
        detail.commit_message = c.message.clone();
        if let Some(t) = c.committer.as_ref().and_then(|p| p.date) {
            detail.summary.updated = t;
        }
    }
    let first_written = detail
        .revisions
        .iter()
        .find_map(|r| r.commit.as_ref()?.author.as_ref()?.date);
    if let Some(t) = first_written {
        detail.summary.created = t;
    }
}

/// Reads the commits of `detail`'s local revisions that `known` does not
/// have from the checkout of its project, waiting for the checkout to
/// open. `None` when nothing is wanted or there is no open checkout. Git
/// runs on the background executor.
pub async fn load_commits(
    detail: &ChangeDetail,
    known: &GitCommits,
    checkouts: Option<Entity<Checkouts>>,
    cx: &mut AsyncApp,
) -> Option<GitCommits> {
    let shas = known.wanted(detail);
    if shas.is_empty() {
        return None;
    }
    let checkout = Checkouts::opened(&checkouts?, &detail.summary.project, cx).await?;
    let CheckoutState::Open(repo) = checkout.state else {
        return None;
    };
    Some(
        cx.background_spawn(async move { GitCommits::read(&repo, &shas) })
            .await,
    )
}

// ---- display ----

/// How a patch set is named on screen: `PS 3` for a Gerrit patch set,
/// `local 2` for a local version in `list` (the change's merged revisions)
/// by its stored name ([`Revision::local_ordinal`]: the second version
/// seen, also once the first is on Gerrit), plain `local` for a local
/// number `list` does not have. Never the synthetic number.
pub fn patch_set_label(list: &[Revision], number: PatchSetNumber) -> String {
    if !number.is_local() {
        return format!("PS {number}");
    }
    list.iter()
        .find(|r| r.number == number)
        .and_then(|r| r.local_ordinal)
        .map_or_else(|| "local".to_owned(), |n| format!("local {n}"))
}

/// How a change is named on screen: `#123`, or `local` for a local change
/// (its number is synthetic).
pub fn change_label(number: ChangeNumber) -> String {
    if number.is_local() {
        "local".to_owned()
    } else {
        format!("#{number}")
    }
}

/// Where patch set `number` is in `list`, the order the pages work in.
pub fn position(list: &[Revision], number: PatchSetNumber) -> Option<usize> {
    list.iter().position(|r| r.number == number)
}

#[cfg(test)]
mod tests;
