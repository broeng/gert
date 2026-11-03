//! The relation chain of the change page: Gerrit's related changes of the
//! patch set on show, with the local commits the checkout built on it, or
//! for a local change or version the chain read from the checkout (see
//! [`crate::local_chain`]), plus the votes of its open changes.
//!
//! [`RelationChain`] loads it and says when it changed; the page draws a
//! copy of [`Related`] and keeps what belongs to drawing (scrolling, the
//! side panel). A chain handed over from the page of another change in it
//! ([`ChainHandover`]) is shown until this change's own arrives.

use std::collections::HashMap;

use gpui_kit::*;
use model::{ChangeNumber, ChangeStatus, ChangeSummary, PatchSetNumber, RelatedChange, RevisionId};
use sync::ChainCommit;

use crate::{
    change_detail,
    checkouts::Checkouts,
    local_chain,
    session::{Session, is_unauthorized},
};

#[derive(Clone)]
pub(super) struct Related {
    pub(super) sha: RevisionId,
    /// Gerrit's related changes; empty for a chain read from the checkout.
    pub(super) changes: Vec<RelatedChange>,
    /// What the chain shows, newest first.
    pub(super) rows: Vec<ChainRow>,
    /// Read from the checkout (see [`crate::local_chain`]), not Gerrit's.
    pub(super) from_git: bool,
    /// Labels of the open changes in the chain, once fetched (from the
    /// cache for a chain read from the checkout).
    pub(super) summaries: HashMap<ChangeNumber, ChangeSummary>,
    /// Taken over from the page of another change in the chain, and shown
    /// until this change's own list arrives.
    pub(super) adopted: bool,
}

/// The relation chain as a change page hands it to the page of another
/// change in that chain (Shift-J / Shift-K, a click on a row, Back), so
/// that the chain stays on screen, scrolled where it was, while the new
/// page reads its own.
pub struct ChainHandover {
    pub(super) related: Related,
    pub(super) scroll: ScrollHandle,
    pub(super) drawn: Option<bool>,
}

/// One row of the relation chain: a change of Gerrit's related changes,
/// or a commit of a chain read from the checkout.
#[derive(Clone)]
pub(super) struct ChainRow {
    /// `#123`; for a chain from the checkout also `local`, `#7 local 2`,
    /// `#7 PS 3` or a short commit id. Never a synthetic number.
    pub(super) label: String,
    pub(super) subject: String,
    /// The change on show (for a chain from the checkout: its commit).
    pub(super) this: bool,
    /// Where a click goes; `None` for this row and for a commit that is no
    /// change.
    pub(super) open: Option<ChainOpen>,
    /// What the status pills come from: Gerrit's entry, or for a Gerrit
    /// patch set in a chain from the checkout one made from its cached
    /// summary (never outdated: the pill is about Gerrit's chain).
    pub(super) related: Option<RelatedChange>,
}

/// Where a click on a relation chain row goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChainOpen {
    /// Another change, at its current patch set.
    Change(ChangeNumber),
    /// Another change at this patch set.
    At(ChangeNumber, PatchSetNumber),
    /// This change's patch set.
    Here(PatchSetNumber),
}

/// The other change a chain row leads to, if any.
pub(super) fn row_change(open: Option<ChainOpen>) -> Option<ChangeNumber> {
    match open? {
        ChainOpen::Change(n) | ChainOpen::At(n, _) => Some(n),
        ChainOpen::Here(_) => None,
    }
}

/// Where a vote moves on to in the chain `rows` (newest first): the row
/// above the change on show, its child, when it is another change (or a
/// local version of one) and open. A row with Gerrit's status must be
/// `New`; a local commit without one is open while it is on a branch.
/// `None` at the tip, for a closed child, a commit that is no change, or
/// when the change is not in the chain.
pub(super) fn advance_row(rows: &[ChainRow]) -> Option<(ChangeNumber, Option<PatchSetNumber>)> {
    let at = rows.iter().position(|r| r.this)?;
    let child = rows.get(at.checked_sub(1)?)?;
    if child
        .related
        .as_ref()
        .is_some_and(|c| c.status != Some(ChangeStatus::New))
    {
        return None;
    }
    match child.open? {
        ChainOpen::Change(n) => Some((n, None)),
        ChainOpen::At(n, ps) => Some((n, Some(ps))),
        ChainOpen::Here(_) => None,
    }
}

/// Where the chain comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChainSource {
    /// Gerrit's related changes, with the local commits built on them.
    Gerrit,
    /// The checkout: a local change, or a local version as the target.
    Checkout,
}

pub(super) enum ChainEvent {
    /// The chain changed; `moved` when its rows came or went (so the row
    /// of the change on show may have moved), not for the same rows again
    /// or their votes.
    Changed { moved: bool },
    /// The server answered 401: back to the login flow.
    Unauthorized,
}

pub(super) struct RelationChain {
    session: Session,
    checkouts: Option<Entity<Checkouts>>,
    /// The change of the page.
    number: ChangeNumber,
    /// Its project and subject, from the detail on show.
    project: String,
    subject: String,
    /// The commit whose chain is wanted: the patch set on show.
    wanted: Option<RevisionId>,
    related: Option<Related>,
    related_task: Option<Task<()>>,
    /// The local commits the checkout built on the Gerrit patch set on
    /// show, for its relation chain, and the patch set's commit.
    local_stack: Option<(RevisionId, local_chain::Stack)>,
    stack_task: Option<Task<()>>,
    summaries_task: Option<Task<()>>,
}

impl EventEmitter<ChainEvent> for RelationChain {}

impl RelationChain {
    pub(super) fn new(session: Session, number: ChangeNumber) -> Self {
        Self {
            session,
            checkouts: None,
            number,
            project: String::new(),
            subject: String::new(),
            wanted: None,
            related: None,
            related_task: None,
            local_stack: None,
            stack_task: None,
            summaries_task: None,
        }
    }

    pub(super) fn set_checkouts(&mut self, checkouts: Option<Entity<Checkouts>>) {
        self.checkouts = checkouts;
    }

    /// The chain on show, if any.
    pub(super) fn related(&self) -> Option<&Related> {
        self.related.as_ref()
    }

    /// Shows `chain`, handed over by the page of another change in it,
    /// until this change's own arrives.
    pub(super) fn adopt(&mut self, chain: Related) {
        self.related = Some(chain);
    }

    /// Loads the chain of commit `sha` (the patch set on show) of
    /// `project` from `source`, from the cache and, for Gerrit's, also from
    /// the server if `network`. What belonged to another commit goes,
    /// except an adopted chain, which stays until this change's own list
    /// replaces it.
    pub(super) fn show(
        &mut self,
        sha: Option<RevisionId>,
        source: ChainSource,
        network: bool,
        project: &str,
        subject: &str,
        cx: &mut Context<Self>,
    ) {
        project.clone_into(&mut self.project);
        subject.clone_into(&mut self.subject);
        self.wanted = sha.clone();
        if self
            .related
            .as_ref()
            .is_some_and(|r| !r.adopted && Some(&r.sha) != sha.as_ref())
        {
            self.related = None;
        }
        if self.local_stack.as_ref().map(|s| &s.0) != sha.as_ref() {
            self.local_stack = None;
            self.stack_task = None;
        }
        let Some(sha) = sha else {
            return;
        };
        match source {
            ChainSource::Checkout => self.load_git_chain(sha, cx),
            ChainSource::Gerrit => {
                self.load_related(sha.clone(), network, cx);
                self.load_stack(sha, cx);
            }
        }
    }

    fn load_related(&mut self, sha: RevisionId, network: bool, cx: &mut Context<Self>) {
        let syncer = self.session.syncer().clone();
        let sync = network && self.session.can_sync();
        let number = self.number;
        self.related_task = Some(cx.spawn(async move |this, cx| {
            let cached = {
                let (syncer, sha) = (syncer.clone(), sha.clone());
                cx.background_spawn(async move { syncer.cached_related(number, &sha) })
                    .await
            };
            this.update(cx, |this, cx| match cached {
                Ok(Some(c)) => this.apply_related(&sha, c.changes, cx),
                Ok(None) => {}
                Err(e) => log::warn!("reading cached related changes: {e}"),
            })
            .ok();
            if !sync {
                return;
            }
            let fetched = {
                let sha = sha.clone();
                cx.background_spawn(async move { syncer.refresh_related(number, &sha).await })
                    .await
            };
            this.update(cx, |this, cx| match fetched {
                Ok(changes) => this.apply_related(&sha, changes, cx),
                Err(e) if is_unauthorized(&e) => cx.emit(ChainEvent::Unauthorized),
                Err(e) => log::warn!("fetching related changes of {number}: {e}"),
            })
            .ok();
        }));
    }

    fn apply_related(
        &mut self,
        sha: &RevisionId,
        changes: Vec<RelatedChange>,
        cx: &mut Context<Self>,
    ) {
        if self.wanted.as_ref() != Some(sha) {
            return;
        }
        let rows = self.gerrit_chain_rows(sha, &changes);
        // Rows that come and go move the current one; a refresh of the
        // same rows leaves the chain where the user scrolled it.
        let moved = self.related.as_ref().is_none_or(|r| {
            !r.rows
                .iter()
                .map(|row| (&row.label, row.this))
                .eq(rows.iter().map(|row| (&row.label, row.this)))
        });
        let summaries = match self.related.take() {
            Some(r) if &r.sha == sha && !r.adopted && !r.from_git => r.summaries,
            // The previous page's votes stay for the changes still at the
            // same patch set, until this chain's own arrive.
            Some(r) if r.adopted => {
                let newest = |list: &[RelatedChange]| -> HashMap<_, _> {
                    list.iter()
                        .map(|c| (c.number, c.current_patch_set.unwrap_or(c.patch_set)))
                        .collect()
                };
                let (old, new) = (newest(&r.changes), newest(&changes));
                let mut kept = r.summaries;
                kept.retain(|n, _| old.get(n).is_some_and(|ps| new.get(n) == Some(ps)));
                kept
            }
            _ => HashMap::new(),
        };
        let open: Vec<_> = changes
            .iter()
            .filter(|c| matches!(c.status, Some(ChangeStatus::New)))
            .map(|c| c.number)
            .collect();
        self.related = Some(Related {
            sha: sha.clone(),
            changes,
            rows,
            from_git: false,
            summaries,
            adopted: false,
        });
        cx.emit(ChainEvent::Changed { moved });
        if self.session.can_sync() {
            self.load_related_summaries(sha.clone(), open, cx);
        }
    }

    /// The rows of Gerrit's relation chain `changes` of patch set `sha`,
    /// with the local commits the checkout built on it (once read, see
    /// [`RelationChain::load_stack`]) above it, or above the highest change
    /// of the chain that they are built on. Without a chain from Gerrit
    /// but with local commits, the chain is them and this change.
    ///
    /// A list cannot show a tree: when what was built on the patch set
    /// forks (one branch with a pushed change, another with a local one),
    /// the local commits are one block, in git's topological order (each
    /// line together, newest first), above the highest change they are
    /// built on.
    fn gerrit_chain_rows(&self, sha: &RevisionId, changes: &[RelatedChange]) -> Vec<ChainRow> {
        let mut rows: Vec<ChainRow> = changes
            .iter()
            .map(|c| ChainRow {
                label: change_detail::change_label(c.number),
                subject: super::view::related_subject(c),
                this: c.number == self.number,
                open: (c.number != self.number).then_some(ChainOpen::Change(c.number)),
                related: Some(c.clone()),
            })
            .collect();
        let Some((_, stack)) = self.local_stack.as_ref().filter(|(at, _)| at == sha) else {
            return rows;
        };
        // A commit Gerrit's chain has is Gerrit's row, though the store did
        // not know it yet (just pushed, its change not cached): not shown
        // twice, and the local commits built on it go above that row.
        let in_chain: HashMap<&RevisionId, ChangeNumber> = changes
            .iter()
            .filter_map(|c| Some((c.commit.as_ref()?.sha.as_ref()?, c.number)))
            .collect();
        let mut below = stack.on_gerrit.clone();
        let entries: Vec<&local_chain::ChainEntry> = stack
            .entries
            .iter()
            .filter(|e| match in_chain.get(&e.commit.sha) {
                Some(n) => {
                    below.push(*n);
                    false
                }
                None => true,
            })
            .collect();
        if entries.is_empty() {
            return rows;
        }
        if rows.is_empty() {
            rows.push(ChainRow {
                label: change_detail::change_label(self.number),
                subject: self.subject.clone(),
                this: true,
                open: None,
                related: None,
            });
        }
        let at = rows
            .iter()
            .position(|r| {
                r.this
                    || r.related
                        .as_ref()
                        .is_some_and(|c| below.contains(&c.number))
            })
            .unwrap_or(0);
        let mut ignored = HashMap::new();
        let local: Vec<ChainRow> = entries
            .into_iter()
            .map(|e| self.git_chain_row(sha, e, &mut ignored))
            .collect();
        rows.splice(at..at, local);
        rows
    }

    /// Reads the local commits the checkout built on Gerrit patch set
    /// `sha` (see [`local_chain::read_stack`]) and puts them in its
    /// relation chain; until they arrive, the ones read before stay. No
    /// network; nothing when the project does not have the
    /// `local_changes` option or no open checkout.
    fn load_stack(&mut self, sha: RevisionId, cx: &mut Context<Self>) {
        let project = self.project.clone();
        let (syncer, checkouts) = (self.session.syncer().clone(), self.checkouts.clone());
        self.stack_task = Some(cx.spawn(async move |this, cx| {
            let read = local_chain::read_stack(checkouts, project, sha.clone(), syncer, cx).await;
            if let Some(stack) = read {
                this.update(cx, |this, cx| this.apply_stack(sha, stack, cx))
                    .ok();
            }
        }));
    }

    fn apply_stack(&mut self, sha: RevisionId, stack: local_chain::Stack, cx: &mut Context<Self>) {
        if self.wanted.as_ref() != Some(&sha) {
            return;
        }
        // Gerrit's chain on show, if any, is drawn again without the local
        // commits that went.
        let gerrit = self
            .related
            .as_ref()
            .is_some_and(|r| r.sha == sha && !r.from_git);
        let empty = stack.entries.is_empty();
        self.local_stack = Some((sha.clone(), stack));
        // A chain handed over by the previous page stays until Gerrit's own
        // list arrives, which takes the stack in.
        let adopted = self.related.as_ref().is_some_and(|r| r.adopted);
        if !gerrit && (empty || adopted) {
            return;
        }
        let old = self.related.take();
        let old_rows: Vec<(String, bool)> = old
            .as_ref()
            .map(|r| {
                r.rows
                    .iter()
                    .map(|row| (row.label.clone(), row.this))
                    .collect()
            })
            .unwrap_or_default();
        let (changes, summaries) = match old {
            Some(r) if r.sha == sha && !r.from_git => (r.changes, r.summaries),
            _ => (Vec::new(), HashMap::new()),
        };
        let rows = self.gerrit_chain_rows(&sha, &changes);
        // The same rows again stay where the user scrolled them.
        let moved = !old_rows
            .iter()
            .map(|(label, this)| (label, *this))
            .eq(rows.iter().map(|row| (&row.label, row.this)));
        self.related = Some(Related {
            sha,
            changes,
            rows,
            from_git: false,
            summaries,
            adopted: false,
        });
        cx.emit(ChainEvent::Changed { moved });
    }

    /// Reads the relation chain of the commit `sha` from the checkout and
    /// names its commits from the store (see [`crate::local_chain`]). No
    /// network.
    fn load_git_chain(&mut self, sha: RevisionId, cx: &mut Context<Self>) {
        let project = self.project.clone();
        let (syncer, checkouts) = (self.session.syncer().clone(), self.checkouts.clone());
        self.related_task = Some(cx.spawn(async move |this, cx| {
            let read = local_chain::read(checkouts, project, sha.clone(), syncer, cx).await;
            if let Some(chain) = read {
                this.update(cx, |this, cx| this.apply_git_chain(&sha, chain, cx))
                    .ok();
            }
        }));
    }

    fn apply_git_chain(
        &mut self,
        sha: &RevisionId,
        chain: local_chain::Chain,
        cx: &mut Context<Self>,
    ) {
        if self.wanted.as_ref() != Some(sha) {
            return;
        }
        let mut summaries = HashMap::new();
        let rows: Vec<ChainRow> = chain
            .entries
            .iter()
            .map(|e| self.git_chain_row(sha, e, &mut summaries))
            .collect();
        // As for Gerrit's chain: the same rows again stay where the user
        // scrolled them.
        let moved = self.related.as_ref().is_none_or(|r| {
            !r.rows
                .iter()
                .map(|row| (&row.label, row.this))
                .eq(rows.iter().map(|row| (&row.label, row.this)))
        });
        self.related = Some(Related {
            sha: sha.clone(),
            changes: Vec::new(),
            rows,
            from_git: true,
            summaries,
            adopted: false,
        });
        cx.emit(ChainEvent::Changed { moved });
    }

    /// The row of a commit of a chain read from the checkout, `sha` being
    /// the commit on show: where a click goes (a version of this change is
    /// picked on this page), and for a Gerrit patch set the status pills
    /// from its cached summary, which also goes into `summaries`.
    fn git_chain_row(
        &self,
        sha: &RevisionId,
        e: &local_chain::ChainEntry,
        summaries: &mut HashMap<ChangeNumber, ChangeSummary>,
    ) -> ChainRow {
        let this = e.commit.sha == *sha;
        let (open, related) = match &e.kind {
            // An older version (something may be built on it) at that
            // version; the newest is what the page shows anyway.
            ChainCommit::Local {
                change,
                patch_set,
                newest,
                ..
            } => {
                let open = if *newest {
                    ChainOpen::Change(*change)
                } else {
                    ChainOpen::At(*change, *patch_set)
                };
                (Some(open), None)
            }
            ChainCommit::LocalVersion {
                change, patch_set, ..
            } => (Some(ChainOpen::At(*change, *patch_set)), None),
            ChainCommit::Gerrit {
                change,
                patch_set,
                current,
                summary,
            } => {
                let open = if *current == Some(*patch_set) {
                    ChainOpen::Change(*change)
                } else {
                    ChainOpen::At(*change, *patch_set)
                };
                let related = summary.as_deref().map(|s| {
                    summaries.insert(*change, s.clone());
                    RelatedChange {
                        project: s.project.clone(),
                        change_id: s.change_id.clone(),
                        number: *change,
                        patch_set: *patch_set,
                        current_patch_set: None,
                        status: Some(s.status.clone()),
                        submittable: s.submittable,
                        work_in_progress: s.work_in_progress,
                        commit: None,
                    }
                });
                (Some(open), related)
            }
            ChainCommit::Unknown => (None, None),
        };
        // A version of the change on show is picked on this page.
        let open = open
            .map(|o| match o {
                ChainOpen::Change(n) | ChainOpen::At(n, _) if n == self.number => match e.kind {
                    ChainCommit::Local { patch_set, .. }
                    | ChainCommit::LocalVersion { patch_set, .. }
                    | ChainCommit::Gerrit { patch_set, .. } => ChainOpen::Here(patch_set),
                    ChainCommit::Unknown => o,
                },
                o => o,
            })
            .filter(|_| !this);
        ChainRow {
            label: local_chain::label(e),
            subject: e.commit.subject(),
            this,
            open,
            related,
        }
    }

    /// Fetches the votes of the open changes in the chain for their markers.
    fn load_related_summaries(
        &mut self,
        sha: RevisionId,
        open: Vec<ChangeNumber>,
        cx: &mut Context<Self>,
    ) {
        if open.is_empty() {
            return;
        }
        let syncer = self.session.syncer().clone();
        self.summaries_task = Some(cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { syncer.fetch_summaries(&open).await })
                .await;
            this.update(cx, |this, cx| match fetched {
                Ok(list) => {
                    if let Some(r) = this.related.as_mut().filter(|r| r.sha == sha) {
                        r.summaries = list.into_iter().map(|s| (s.number, s)).collect();
                        cx.emit(ChainEvent::Changed { moved: false });
                    }
                }
                Err(e) => log::warn!("fetching votes of related changes: {e}"),
            })
            .ok();
        }));
    }
}
