//! The scanner of one server's checkouts for local changes: the user's own
//! unpushed commits in the projects that have the `local_changes` option.
//!
//! One [`LocalScanner`] entity per server, on its registry entry next to its
//! [`Checkouts`]. Only signed-in servers get one ([`LocalScanner::set_session`]
//! takes no anonymous session). The git calls and the store writes run on
//! the background executor; the classification is `sync`'s
//! ([`sync::Syncer::reconcile_local`]).
//!
//! # When
//!
//! [`LocalScanner::request`] asks for a round. The shell asks when the
//! server becomes active and after each background poll of an inactive
//! server; the dashboard asks when it is shown and on its poll tick; the
//! scanner asks itself whenever its [`Checkouts`] notify (a checkout
//! finished opening, the projects were saved), and the shell asks again
//! when it learns the account's email. A request while a round runs is remembered and runs once
//! that one is done, so there is never more than one scan of a project at a
//! time. Requests are cheap: a round skips every project whose skip key is
//! unchanged after one `for-each-ref`.
//!
//! # A round
//!
//! For every project with the option on and an open checkout:
//!
//! 1. [`Repo::refs_fingerprint`] (one git call), the author emails (the
//!    checkout's `git config`, and the account's as the cache has it,
//!    [`Syncer::account_email`]) and the generation
//!    ([`sync::Syncer::local_generation`]: the project's rows and the
//!    cached Gerrit changes with the last scan's or the rows' Change-Ids);
//!    with the author emails and the enabled flag that is the skip key.
//!    Equal to the one recorded: nothing to do.
//! 2. Otherwise [`Repo::scan`] and the
//!    reconcile ([`sync::Syncer::reconcile_local_keyed`], which also
//!    returns the generation after its writes). The key is recorded only
//!    when the report
//!    [`is_settled`](store::ReconcileReport::is_settled), so a reconcile
//!    that the store did not take as planned runs again next time.
//!
//! Projects that have rows in the store but no longer have the option (or
//! no entry at all) are reconciled with the option off, which removes the
//! rows without unsent data. A project whose checkout is still opening or
//! failed to open is left alone: its rows stay as they are.
//!
//! When a round changed anything, the scanner emits
//! [`LocalScanEvent::Changed`] and the dashboard reads its section again.
//! Git and store errors are logged and never fail a page.

use std::collections::{HashMap, HashSet};

use gpui_kit::{AppContext as _, Context, Entity, EventEmitter, Subscription, Task};
use local_git::Repo;
use sync::{Syncer, local::scan_change_ids};

use crate::{checkouts::Checkouts, session::Session};

/// How many of a checkout's newest local commits a scan looks at (the
/// user's decision: 200 per project).
pub const SCAN_LIMIT: usize = 200;

/// What the scanner tells its observers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalScanEvent {
    /// A round changed local change rows: lists showing them read them
    /// again.
    Changed,
}

/// Everything a project's scan depends on. Equal keys mean a new scan would
/// change nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SkipKey {
    /// [`Repo::refs_fingerprint`].
    refs: u64,
    /// The author emails, normalized and sorted.
    emails: Vec<String>,
    enabled: bool,
    /// [`Syncer::local_generation`] of `Memo::change_ids`: the project's
    /// rows and the cached Gerrit changes the reconcile reads.
    generation: u64,
}

/// What the last settled scan of a project left for the next one.
#[derive(Clone, Debug)]
struct Memo {
    key: SkipKey,
    /// The Change-Ids of that scan's commits, sorted, each once: the input
    /// of the next pre-check's cache generation.
    change_ids: Vec<String>,
}

/// What one round needs, gathered on the main thread.
struct Round {
    syncer: Syncer,
    /// How many commits a scan looks at ([`SCAN_LIMIT`]).
    limit: usize,
    /// The projects with the option on and an open checkout.
    open: Vec<(String, Repo)>,
    /// Every project with the option on, open or not.
    enabled: HashSet<String>,
    memos: HashMap<String, Memo>,
}

/// What a round did.
struct Ran {
    memos: HashMap<String, Memo>,
    changed: bool,
    full_scans: usize,
}

/// The scanner of one server. See the module comment.
pub struct LocalScanner {
    checkouts: Entity<Checkouts>,
    /// `None`: no session, or an anonymous one; nothing is scanned.
    syncer: Option<Syncer>,
    username: Option<String>,
    memos: HashMap<String, Memo>,
    /// [`SCAN_LIMIT`]; smaller in tests.
    limit: usize,
    /// Bumped when the memos are dropped, so that a round that started
    /// before does not bring them back.
    epoch: u64,
    task: Option<Task<()>>,
    /// A request came while a round ran.
    again: bool,
    /// Full scans (`Repo::scan`) done so far, for the tests.
    full_scans: usize,
    _checkouts: Subscription,
}

impl EventEmitter<LocalScanEvent> for LocalScanner {}

impl LocalScanner {
    pub fn new(checkouts: Entity<Checkouts>, cx: &mut Context<Self>) -> Self {
        let sub = cx.observe(&checkouts, |this, _, cx| this.request(cx));
        Self {
            checkouts,
            syncer: None,
            username: None,
            memos: HashMap::new(),
            limit: SCAN_LIMIT,
            epoch: 0,
            task: None,
            again: false,
            full_scans: 0,
            _checkouts: sub,
        }
    }

    /// Takes the server's current session. An anonymous session, or none,
    /// stops scanning. Another account forgets what was known about the
    /// previous one.
    pub fn set_session(&mut self, session: Option<&Session>) {
        let session = session.filter(|s| s.username.is_some());
        let username = session.and_then(|s| s.username.clone());
        if username != self.username {
            self.username = username;
            self.memos.clear();
            self.epoch += 1;
        }
        self.syncer = session.map(|s| s.syncer().clone());
    }

    /// Scans look at only `limit` commits from now on.
    #[cfg(test)]
    pub(crate) fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
        self.memos.clear();
        self.epoch += 1;
    }

    #[cfg(test)]
    pub(crate) fn full_scans(&self) -> usize {
        self.full_scans
    }

    #[cfg(test)]
    pub(crate) fn is_idle(&self) -> bool {
        self.task.is_none() && !self.again
    }

    /// Asks for a round (see the module comment). Never blocks.
    pub fn request(&mut self, cx: &mut Context<Self>) {
        let Some(syncer) = self.syncer.clone() else {
            return;
        };
        if self.task.is_some() {
            self.again = true;
            return;
        }
        let (open, enabled) = self.checkouts.update(cx, |c, cx| {
            let names: Vec<String> = c.local_change_projects().map(|p| p.name.clone()).collect();
            let open: Vec<(String, Repo)> = names
                .iter()
                .filter_map(|name| {
                    let repo = c.get(name, cx)?.repo()?.clone();
                    Some((name.clone(), repo))
                })
                .collect();
            (open, names.into_iter().collect::<HashSet<_>>())
        });
        let round = Round {
            syncer,
            limit: self.limit,
            open,
            enabled,
            memos: self.memos.clone(),
        };
        let epoch = self.epoch;
        self.task = Some(cx.spawn(async move |this, cx| {
            let ran = cx.background_spawn(async move { run(round) }).await;
            this.update(cx, |this, cx| this.finished(epoch, ran, cx))
                .ok();
        }));
    }

    fn finished(&mut self, epoch: u64, ran: Ran, cx: &mut Context<Self>) {
        self.task = None;
        self.full_scans += ran.full_scans;
        if epoch == self.epoch {
            self.memos = ran.memos;
        }
        if ran.changed {
            cx.emit(LocalScanEvent::Changed);
            cx.notify();
        }
        if std::mem::take(&mut self.again) {
            self.request(cx);
        }
    }
}

/// The normalized, sorted union of the account's and the checkout's
/// emails.
fn author_emails(account: Option<&str>, repo: &[String]) -> Vec<String> {
    let mut emails: Vec<String> = account
        .into_iter()
        .chain(repo.iter().map(String::as_str))
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    emails.sort_unstable();
    emails.dedup();
    emails
}

/// One round, on the background executor.
fn run(round: Round) -> Ran {
    let Round {
        syncer,
        limit,
        open,
        enabled,
        mut memos,
    } = round;
    let mut ran = Ran {
        memos: HashMap::new(),
        changed: false,
        full_scans: 0,
    };
    let account_email = syncer.account_email().unwrap_or_else(|e| {
        log::warn!("reading the account email: {e}");
        None
    });
    for (project, repo) in &open {
        match scan_project(
            &syncer,
            project,
            repo,
            limit,
            account_email.as_deref(),
            memos.remove(project),
        ) {
            Ok(outcome) => {
                ran.changed |= outcome.changed;
                ran.full_scans += usize::from(outcome.scanned);
                if let Some(memo) = outcome.memo {
                    ran.memos.insert(project.clone(), memo);
                }
            }
            Err(e) => log::warn!("looking for local changes of {project}: {e}"),
        }
    }
    // Projects still opening (or failed) keep their memo for later.
    for (project, memo) in memos {
        if enabled.contains(&project) && !ran.memos.contains_key(&project) {
            ran.memos.insert(project, memo);
        }
    }
    match syncer.local_change_projects() {
        Ok(projects) => {
            for project in projects.iter().filter(|p| !enabled.contains(*p)) {
                match syncer.reconcile_local(project, &Default::default(), &[], false) {
                    Ok(report) => ran.changed |= report.changed(),
                    Err(e) => log::warn!("removing the local changes of {project}: {e}"),
                }
            }
        }
        Err(e) => log::warn!("listing the projects with local changes: {e}"),
    }
    ran
}

struct Outcome {
    /// The memo to keep: `None` when the reconcile did not settle (or an
    /// earlier memo is gone because the scan ran).
    memo: Option<Memo>,
    changed: bool,
    scanned: bool,
}

#[derive(Debug, thiserror::Error)]
enum ScanError {
    #[error(transparent)]
    Git(#[from] local_git::Error),
    #[error(transparent)]
    Sync(#[from] sync::Error),
}

fn scan_project(
    syncer: &Syncer,
    project: &str,
    repo: &Repo,
    limit: usize,
    account_email: Option<&str>,
    memo: Option<Memo>,
) -> Result<Outcome, ScanError> {
    let refs = repo.refs_fingerprint()?;
    let emails = author_emails(account_email, &repo.author_emails()?);
    if let Some(memo) = memo {
        let key = SkipKey {
            refs,
            emails: emails.clone(),
            enabled: true,
            generation: syncer.local_generation(project, &memo.change_ids)?,
        };
        if key == memo.key {
            return Ok(Outcome {
                memo: Some(memo),
                changed: false,
                scanned: false,
            });
        }
    }
    let scan = repo.scan(limit)?;
    let change_ids = scan_change_ids(&scan);
    // The generation is read under the reconcile's store lock, after its
    // writes: what the next pre-check compares with.
    let (report, generation) = syncer.reconcile_local_keyed(project, &scan, &emails)?;
    let memo = report.is_settled().then_some(Memo {
        key: SkipKey {
            refs: scan.fingerprint,
            emails,
            enabled: true,
            generation,
        },
        change_ids,
    });
    Ok(Outcome {
        memo,
        changed: report.changed(),
        scanned: true,
    })
}

#[cfg(test)]
mod tests;
