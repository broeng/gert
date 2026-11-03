//! The reviewed marks on their way to Gerrit.
//!
//! A mark request writes the mark into the cache at once (the file list
//! shows it), then goes to Gerrit. Several requests for one mark can be
//! open together (`r` pressed again before the answer), and their answers
//! come back in any order. [`MarkLedger`] numbers the requests in the order
//! they are sent, the order Gerrit applies them in as far as we can tell,
//! and once none for a mark is open says what Gerrit has: the value of the
//! latest request sent that worked, else the mark from before them all.
//! The cache then holds that ([`MarkSettled`]); while requests are open it
//! holds the value last asked for, also over a list fetched meanwhile.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
};

use model::{ChangeNumber, RevisionId};
use store::{ServerId, Store};

/// What a mark came to once no request for it is open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkSettled {
    /// The mark as Gerrit has it, as far as we know. The cache holds it.
    pub reviewed: bool,
    /// The last request sent failed and the mark went back: the value it
    /// asked for and why it failed.
    pub refused: Option<(bool, String)>,
}

/// The mark ledgers of every server, so that the syncers made for one
/// server one after another (a new login) share its requests on their way.
/// Cheap to clone; give every syncer the same one
/// ([`super::Syncer::with_marks`]).
#[derive(Clone, Default)]
pub struct MarkLedgers(Arc<Mutex<HashMap<ServerId, Shared>>>);

pub(super) type Shared = Arc<Mutex<MarkLedger>>;

impl MarkLedgers {
    pub(super) fn for_server(&self, server: ServerId) -> Shared {
        let mut all = self.0.lock().unwrap_or_else(|e| e.into_inner());
        all.entry(server).or_default().clone()
    }
}

pub(super) fn lock(ledger: &Shared) -> MutexGuard<'_, MarkLedger> {
    ledger.lock().unwrap_or_else(|e| e.into_inner())
}

/// The requests for the marks of one server that are on their way.
#[derive(Default)]
pub(super) struct MarkLedger {
    sent: u64,
    open: HashMap<(RevisionId, String), Open>,
}

struct Open {
    /// The value last asked for, and its request's number.
    asked: bool,
    last: u64,
    /// How many requests for the mark are open.
    count: u32,
    /// The mark as Gerrit has it, as far as we know: the mark before the
    /// first of these requests, then the value of the latest sent that
    /// worked (`confirmed_by`, 0 for none).
    confirmed: bool,
    confirmed_by: u64,
    /// Why the last request sent failed, if it did.
    refused: Option<String>,
}

impl MarkLedger {
    /// A request asking for `reviewed` on the mark of `path` in patch set
    /// `revision` starts; `before` is the mark Gerrit had, as far as we
    /// know, if no request for it is open. Returns the request's number.
    pub(super) fn begin(
        &mut self,
        revision: &RevisionId,
        path: &str,
        reviewed: bool,
        before: bool,
    ) -> u64 {
        self.sent += 1;
        let number = self.sent;
        let open = self
            .open
            .entry((revision.clone(), path.to_owned()))
            .or_insert(Open {
                asked: reviewed,
                last: number,
                count: 0,
                confirmed: before,
                confirmed_by: 0,
                refused: None,
            });
        open.asked = reviewed;
        open.last = number;
        open.count += 1;
        open.refused = None;
        number
    }

    /// Request `number` for the mark of `path` in `revision` ended: `Ok`
    /// when it worked, else why not. Returns what the mark came to when no
    /// request for it is open any more.
    pub(super) fn end(
        &mut self,
        revision: &RevisionId,
        path: &str,
        number: u64,
        reviewed: bool,
        result: Result<(), String>,
    ) -> Option<MarkSettled> {
        let slot = (revision.clone(), path.to_owned());
        let open = self.open.get_mut(&slot)?;
        open.count -= 1;
        match result {
            Ok(()) if number > open.confirmed_by => {
                open.confirmed = reviewed;
                open.confirmed_by = number;
            }
            Ok(()) => {}
            Err(e) if number == open.last => open.refused = Some(e),
            Err(_) => {}
        }
        if open.count > 0 {
            return None;
        }
        let done = self.open.remove(&slot)?;
        let back = done.confirmed != done.asked;
        Some(MarkSettled {
            reviewed: done.confirmed,
            refused: done.refused.filter(|_| back).map(|e| (done.asked, e)),
        })
    }

    /// The marks of `revision` on their way: each path with the value last
    /// asked for.
    pub(super) fn asked(&self, revision: &RevisionId) -> Vec<(String, bool)> {
        self.open
            .iter()
            .filter(|((r, _), _)| r == revision)
            .map(|((_, path), open)| (path.clone(), open.asked))
            .collect()
    }
}

/// A request in the ledger until it ends: [`OpenMark::end`] settles it,
/// and if the request is dropped before that (its future cancelled), it
/// ends as failed, so that the mark does not stay open for good.
pub(super) struct OpenMark {
    ledger: Shared,
    store: Arc<Mutex<Store>>,
    server: ServerId,
    change: ChangeNumber,
    revision: RevisionId,
    path: String,
    reviewed: bool,
    number: u64,
    open: bool,
}

impl OpenMark {
    /// Enters a request asking for `reviewed` into `ledger` (already
    /// locked as `marks`); `before` is the mark the cache has.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn begin(
        ledger: &Shared,
        marks: &mut MarkLedger,
        store: &Arc<Mutex<Store>>,
        server: ServerId,
        change: ChangeNumber,
        revision: &RevisionId,
        path: &str,
        reviewed: bool,
        before: bool,
    ) -> Self {
        let number = marks.begin(revision, path, reviewed, before);
        Self {
            ledger: ledger.clone(),
            store: store.clone(),
            server,
            change,
            revision: revision.clone(),
            path: path.to_owned(),
            reviewed,
            number,
            open: true,
        }
    }

    /// The request ended: `Ok` when it worked, else why not. When it was
    /// the last open one for the mark, the cache gets what it settled to.
    pub(super) fn end(mut self, result: Result<(), String>) -> Option<MarkSettled> {
        self.open = false;
        self.settle(result)
    }

    fn settle(&self, result: Result<(), String>) -> Option<MarkSettled> {
        let mut marks = lock(&self.ledger);
        let settled = marks.end(
            &self.revision,
            &self.path,
            self.number,
            self.reviewed,
            result,
        );
        if let Some(s) = &settled {
            let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = store.set_reviewed_path(
                self.server,
                self.change,
                &self.revision,
                &self.path,
                s.reviewed,
            ) {
                log::warn!("settling the reviewed mark of {}: {e}", self.path);
            }
        }
        settled
    }
}

impl Drop for OpenMark {
    fn drop(&mut self) {
        if self.open {
            self.settle(Err("the request was cancelled".into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rev() -> RevisionId {
        "sha2".into()
    }

    #[test]
    fn one_request_settles_on_its_answer() {
        let mut l = MarkLedger::default();
        let n = l.begin(&rev(), "a", true, false);
        let settled = l.end(&rev(), "a", n, true, Ok(()));
        assert_eq!(
            settled,
            Some(MarkSettled {
                reviewed: true,
                refused: None
            })
        );
        assert!(l.asked(&rev()).is_empty());
        let n = l.begin(&rev(), "a", false, true);
        let settled = l.end(&rev(), "a", n, false, Err("no".into()));
        assert_eq!(
            settled,
            Some(MarkSettled {
                reviewed: true,
                refused: Some((false, "no".into())),
            })
        );
    }

    #[test]
    fn the_latest_request_sent_that_worked_wins_in_any_answer_order() {
        let mut l = MarkLedger::default();
        let on = l.begin(&rev(), "a", true, false);
        let off = l.begin(&rev(), "a", false, false);
        assert_eq!(l.asked(&rev()), [("a".to_owned(), false)]);
        // Off's answer first.
        assert_eq!(l.end(&rev(), "a", off, false, Ok(())), None);
        let settled = l.end(&rev(), "a", on, true, Ok(()));
        assert_eq!(
            settled,
            Some(MarkSettled {
                reviewed: false,
                refused: None
            })
        );
    }

    #[test]
    fn a_late_failure_of_an_earlier_request_changes_nothing() {
        let mut l = MarkLedger::default();
        let on = l.begin(&rev(), "a", true, false);
        let off = l.begin(&rev(), "a", false, false);
        let again = l.begin(&rev(), "a", true, false);
        assert_eq!(l.end(&rev(), "a", off, false, Ok(())), None);
        assert_eq!(l.end(&rev(), "a", again, true, Ok(())), None);
        let settled = l.end(&rev(), "a", on, true, Err("late".into()));
        assert_eq!(
            settled,
            Some(MarkSettled {
                reviewed: true,
                refused: None
            })
        );
    }

    #[test]
    fn a_failed_last_request_goes_back_to_the_latest_that_worked() {
        let mut l = MarkLedger::default();
        // On (refused), off (works), on (refused): Gerrit has it off.
        let first = l.begin(&rev(), "a", true, false);
        let off = l.begin(&rev(), "a", false, false);
        let last = l.begin(&rev(), "a", true, false);
        assert_eq!(l.end(&rev(), "a", last, true, Err("no".into())), None);
        assert_eq!(l.end(&rev(), "a", first, true, Err("no".into())), None);
        let settled = l.end(&rev(), "a", off, false, Ok(()));
        assert_eq!(
            settled,
            Some(MarkSettled {
                reviewed: false,
                refused: Some((true, "no".into())),
            })
        );
    }

    #[test]
    fn when_every_request_fails_the_mark_from_before_comes_back() {
        let mut l = MarkLedger::default();
        let on = l.begin(&rev(), "a", true, false);
        let off = l.begin(&rev(), "a", false, false);
        assert_eq!(l.end(&rev(), "a", off, false, Err("b".into())), None);
        let settled = l.end(&rev(), "a", on, true, Err("a".into()));
        // The last asked for off, which the mark is: nothing went back.
        assert_eq!(
            settled,
            Some(MarkSettled {
                reviewed: false,
                refused: None
            })
        );
        let on = l.begin(&rev(), "a", true, false);
        let settled = l.end(&rev(), "a", on, true, Err("no".into()));
        assert_eq!(
            settled,
            Some(MarkSettled {
                reviewed: false,
                refused: Some((true, "no".into())),
            })
        );
    }

    #[test]
    fn syncers_of_one_server_share_its_ledger() {
        let all = MarkLedgers::default();
        let (a, b) = (all.for_server(ServerId(1)), all.for_server(ServerId(1)));
        lock(&a).begin(&rev(), "a", true, false);
        assert_eq!(lock(&b).asked(&rev()), [("a".to_owned(), true)]);
        assert!(lock(&all.for_server(ServerId(2))).asked(&rev()).is_empty());
    }

    #[test]
    fn marks_are_kept_apart_by_patch_set_and_path() {
        let mut l = MarkLedger::default();
        l.begin(&rev(), "a", true, false);
        l.begin(&rev(), "b", false, true);
        l.begin(&"sha3".into(), "a", false, true);
        let mut asked = l.asked(&rev());
        asked.sort();
        assert_eq!(asked, [("a".to_owned(), true), ("b".to_owned(), false)]);
    }
}
