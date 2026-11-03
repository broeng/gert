//! Consolidation of one server's local changes: sending their review data
//! to Gerrit once Gerrit has them ([`Syncer::consolidate`]), driven from
//! the UI.
//!
//! One [`Consolidator`] entity per signed-in server, on its registry entry
//! next to its scanner. It runs on the background executor and never
//! blocks the UI thread; what a run did goes to the shell as
//! [`ConsolidationEvent::Ran`], and the shell has the pages follow (see
//! `shell`'s `consolidated`).
//!
//! # When
//!
//! The shell asks for a run ([`Consolidator::request`]) while the server
//! is online, on the poll cadence: after every refresh or poll of the
//! server that worked, after a scan round that changed local change rows,
//! and after the user saved a local review. An anonymous session, or one
//! whose password is still being read, never runs
//! ([`Consolidator::set_session`]).
//!
//! # One run at a time
//!
//! A run is never started while the previous run of the server is still
//! alive in this process: the post deadline of `sync` is a monotonic timer
//! that stops while the machine sleeps, while claims and the lease go by
//! the wall clock, so a second run could take a post still in flight for
//! a lost one (see `sync`'s `consolidate` module). So there is one task per
//! server, it is never dropped while the server is configured (a new
//! session only changes the syncer of the *next* run), and requests while
//! it runs collapse into one more run after it.
//!
//! # Redirects
//!
//! Once a local change is on Gerrit, its synthetic number leads to the
//! Gerrit change, and a local version that became a Gerrit patch set
//! leads to that patch set ([`redirect`]). The shell checks the page on
//! screen after every run that touched something, and every change or diff
//! page it opens whose route names a local number, so history entries
//! follow when they are visited. The entry is rewritten in place
//! (`NavState::redirect`): Back and Forward stay as they were.
//!
//! # Failures
//!
//! A run that ended early ([`ConsolidateReport::stopped`]: offline, the
//! login refused, the store failing, a post timed out) counts as a failure,
//! and requests are ignored until the poll interval has passed, doubled
//! per failure in a row, like the other polls back off. A run that found
//! another one holding the lease is no failure. A new session may try at
//! once.

use std::{sync::Arc, time::Instant};

use gpui_kit::{AppContext as _, Context, Entity, EventEmitter, Task};
use model::{ChangeNumber, LocalChangeId, PatchSetNumber};
use sync::{ConsolidateOptions, ConsolidateReport, PollPolicy, PollState, PostOutcome, Syncer};

use crate::{
    checkouts::Checkouts,
    nav::{ChangeView, DiffRoute, Route},
    session::Session,
};

/// What a [`Consolidator`] tells the shell.
#[derive(Clone)]
pub enum ConsolidationEvent {
    /// A run ended; the report says what it did.
    Ran(Arc<ConsolidateReport>),
}

/// Runs consolidation for one server. See the module comment.
pub struct Consolidator {
    checkouts: Entity<Checkouts>,
    /// `None`: no session, an anonymous one, or the password is not read
    /// yet; nothing runs.
    syncer: Option<Syncer>,
    /// The run going on, if any. Never dropped while it lives (see the
    /// module comment).
    task: Option<Task<()>>,
    /// A request came while a run was going on.
    again: bool,
    /// Runs in a row that ended early, and when the next may start.
    failures: u32,
    retry_at: Option<Instant>,
    /// Runs started so far, for the tests.
    runs: usize,
}

impl EventEmitter<ConsolidationEvent> for Consolidator {}

impl Consolidator {
    pub fn new(checkouts: Entity<Checkouts>) -> Self {
        Self {
            checkouts,
            syncer: None,
            task: None,
            again: false,
            failures: 0,
            retry_at: None,
            runs: 0,
        }
    }

    /// Takes the server's current session for the next run. A run going on
    /// keeps its own (see the module comment); a signed-in session may try
    /// at once.
    pub fn set_session(&mut self, session: Option<&Session>) {
        self.syncer = session
            .filter(|s| s.can_sync() && s.username.is_some())
            .map(|s| s.syncer().clone());
        if self.syncer.is_some() {
            self.failures = 0;
            self.retry_at = None;
        }
    }

    /// Asks for a run (see the module comment). Never blocks.
    pub fn request(&mut self, cx: &mut Context<Self>) {
        let Some(syncer) = self.syncer.clone() else {
            return;
        };
        if self.task.is_some() {
            self.again = true;
            return;
        }
        if self.retry_at.is_some_and(|t| Instant::now() < t) {
            return;
        }
        let options = options(self.checkouts.read(cx));
        self.runs += 1;
        self.task = Some(cx.spawn(async move |this, cx| {
            let report = cx
                .background_spawn(async move { syncer.consolidate(&options).await })
                .await;
            this.update(cx, |this, cx| this.finished(report, cx)).ok();
        }));
    }

    fn finished(&mut self, report: ConsolidateReport, cx: &mut Context<Self>) {
        self.task = None;
        if failed(&report) {
            self.failures = self.failures.saturating_add(1);
            let state = PollState {
                last_attempt: None,
                window_focused: true,
                visible: true,
                consecutive_failures: self.failures,
            };
            self.retry_at = Some(Instant::now() + PollPolicy::default().interval(&state));
            // The next request after the wait runs; one asked for meanwhile
            // would only fail the same way.
            self.again = false;
        } else if report.ran {
            self.failures = 0;
            self.retry_at = None;
        }
        cx.emit(ConsolidationEvent::Ran(Arc::new(report)));
        if std::mem::take(&mut self.again) {
            self.request(cx);
        }
    }

    #[cfg(test)]
    pub(crate) fn runs(&self) -> usize {
        self.runs
    }

    #[cfg(test)]
    pub(crate) fn is_running(&self) -> bool {
        self.task.is_some()
    }

    /// Whether requests wait for a back-off after failures.
    #[cfg(test)]
    pub(crate) fn backing_off(&self) -> bool {
        self.retry_at.is_some_and(|t| Instant::now() < t)
    }

    /// Lets the next request run at once, as if the back-off had passed.
    #[cfg(test)]
    pub(crate) fn end_backoff(&mut self) {
        self.retry_at = None;
    }
}

/// What a run takes from the checkouts: each configured project's
/// `local_approval` (also of projects whose local changes option is off
/// now: their rows with unsent data still consolidate). A project without
/// an entry is left out, which `sync` takes as `exact`, the strictest.
pub fn options(checkouts: &Checkouts) -> ConsolidateOptions {
    ConsolidateOptions {
        local_approval: checkouts
            .projects()
            .map(|p| (p.name.clone(), p.local_approval))
            .collect(),
        ..Default::default()
    }
}

/// Whether a run ended early. Losing the lease to another run is no
/// failure: that run does the work.
fn failed(report: &ConsolidateReport) -> bool {
    report
        .stopped
        .as_ref()
        .is_some_and(|e| !matches!(e, sync::Error::LeaseLost))
}

/// The synthetic number of local change `id`.
fn local_number(id: LocalChangeId) -> Option<ChangeNumber> {
    id.number()
}

/// Every change a run touched, by Gerrit number and by synthetic number:
/// the pages, drafts and reviews of these are read again.
pub fn affected(report: &ConsolidateReport) -> Vec<ChangeNumber> {
    let mut out = report.changed_gerrit_changes();
    out.extend(report.found.iter().map(|f| f.local_number));
    let rows = report
        .posts
        .iter()
        .map(|p| p.local_change)
        .chain(report.closed.iter().map(|c| c.0))
        .chain(report.reopened.iter().map(|c| c.0))
        .chain(report.consolidated.iter().map(|c| c.0))
        .chain(report.not_found.iter().copied())
        .chain(report.ambiguous.iter().copied());
    out.extend(rows.filter_map(local_number));
    out.into_iter().collect()
}

/// Whether a run changed the state of local change rows (found, closed,
/// reopened, consolidated, ambiguous) or sent something: the scanner then
/// looks again, since what it reconciles against moved.
pub fn rows_changed(report: &ConsolidateReport) -> bool {
    !(report.found.is_empty()
        && report.closed.is_empty()
        && report.reopened.is_empty()
        && report.consolidated.is_empty()
        && report.ambiguous.is_empty()
        && report.posts.is_empty())
}

/// What the page of `change` (a Gerrit or a synthetic number) says about
/// a run, and whether it is an error: a refusal or failed votes first,
/// then held votes, then what was sent, then a change found closed or
/// found at all. `None` when the run did nothing about the change.
pub fn note(report: &ConsolidateReport, change: ChangeNumber) -> Option<(String, bool)> {
    let gerrit = report
        .found
        .iter()
        .find(|f| f.local_number == change)
        .map_or(change, |f| f.gerrit);
    let concerns =
        |row: LocalChangeId, n: ChangeNumber| n == gerrit || local_number(row) == Some(change);
    let posts: Vec<_> = report
        .posts
        .iter()
        .filter(|p| concerns(p.local_change, p.change))
        .collect();
    let refused = posts.iter().find_map(|p| match &p.outcome {
        PostOutcome::Refused { error } => Some(error),
        _ => None,
    });
    if let Some(error) = refused {
        return Some((format!("Gerrit refused your local review: {error}"), true));
    }
    let failed = posts.iter().find_map(|p| match &p.outcome {
        PostOutcome::VotesFailed { error, .. } => Some(error),
        _ => None,
    });
    if let Some(error) = failed {
        return Some((
            format!("Your local review was sent to Gerrit, but some votes did not apply: {error}"),
            true,
        ));
    }
    let held = posts.iter().find_map(|p| match &p.outcome {
        PostOutcome::VotesHeld { reason, .. } => Some(reason),
        _ => None,
    });
    if let Some(reason) = held {
        return Some((
            format!("Your local review was sent to Gerrit; some votes were held: {reason}"),
            false,
        ));
    }
    if posts.iter().any(|p| p.review.is_some()) {
        return Some(("Your local review was sent to Gerrit".to_owned(), false));
    }
    if !posts.is_empty() {
        return Some(("Your local comments were sent to Gerrit".to_owned(), false));
    }
    if report.closed.iter().any(|c| concerns(c.0, c.1)) {
        return Some((
            "This change is closed on Gerrit; your local review is held".to_owned(),
            false,
        ));
    }
    if report.found.iter().any(|f| f.local_number == change) {
        return Some(("This change is on Gerrit now".to_owned(), false));
    }
    None
}

/// Where a change or diff page leads now, with the change page state to
/// open it with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redirect {
    pub route: Route,
    pub view: Option<ChangeView>,
}

/// Whether `route` (with the change page state `view`) names a local
/// change or version: only those can lead elsewhere.
pub fn names_local(route: &Route, view: Option<&ChangeView>) -> bool {
    let in_view = view.is_some_and(|v| {
        v.base.is_some_and(PatchSetNumber::is_local)
            || v.target.is_some_and(PatchSetNumber::is_local)
    });
    match route {
        Route::Change(n) => n.is_local() || in_view,
        Route::Diff(d) => {
            d.change.is_local()
                || d.target.is_local()
                || d.base.is_some_and(PatchSetNumber::is_local)
        }
        _ => false,
    }
}

/// Where `route` leads now that Gerrit has some of what it names (see the
/// module comment): a local change found on Gerrit leads to the Gerrit
/// change, and each local version that is a Gerrit patch set by now to
/// that patch set; a version that is not stays (the Gerrit change lists it
/// among its local patch sets). `view` is the change page state of a
/// change route. `None` when nothing changed. Blocking (SQLite): call it
/// from the background executor.
pub fn redirect(
    syncer: &Syncer,
    route: &Route,
    view: Option<&ChangeView>,
) -> Result<Option<Redirect>, sync::Error> {
    if !names_local(route, view) {
        return Ok(None);
    }
    let change = match route {
        Route::Change(n) => *n,
        Route::Diff(d) => d.change,
        _ => return Ok(None),
    };
    let change = if change.is_local() {
        match syncer
            .local_change(change)?
            .and_then(|(row, _)| row.gerrit_number)
        {
            Some(gerrit) => gerrit,
            // Not on Gerrit (yet), or the row is gone: the page stays.
            None => return Ok(None),
        }
    } else {
        change
    };
    let map = |n: PatchSetNumber| -> Result<PatchSetNumber, sync::Error> {
        if !n.is_local() {
            return Ok(n);
        }
        Ok(syncer
            .local_patch_set(n)?
            .and_then(|v| v.gerrit_patch_set)
            .unwrap_or(n))
    };
    let map_opt = |n: Option<PatchSetNumber>| n.map(map).transpose();
    let to = match route {
        Route::Change(_) => {
            let view = match view {
                Some(v) => Some(ChangeView {
                    base: map_opt(v.base)?,
                    target: map_opt(v.target)?,
                    file: v.file.clone(),
                }),
                None => None,
            };
            Redirect {
                route: Route::Change(change),
                view,
            }
        }
        Route::Diff(d) => {
            let target = map(d.target)?;
            let base = map_opt(d.base)?.filter(|b| *b != target);
            Redirect {
                route: Route::Diff(DiffRoute {
                    change,
                    base,
                    target,
                    path: d.path.clone(),
                    thread: d.thread.clone(),
                }),
                view: None,
            }
        }
        _ => return Ok(None),
    };
    let same = to.route == *route
        && match &to.route {
            Route::Change(_) => to.view.as_ref() == view,
            _ => true,
        };
    Ok((!same).then_some(to))
}

#[cfg(test)]
mod tests;
