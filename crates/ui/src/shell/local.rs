//! The local changes of each server: its scanner and consolidator, and
//! what the shell does when a consolidation run or a held publish ends
//! (pages reload, local numbers redirect).

use gpui_kit::*;
use model::ChangeNumber;

use crate::{
    consolidation::{self, ConsolidationEvent, Consolidator, Request},
    local_scan::{LocalScanEvent, LocalScanner},
    servers::Connection,
    session::ServerKey,
};

use super::{Page, Shell};

impl Shell {
    /// The local change scanner of server `key`, made on first use; `None`
    /// for an anonymous server and one without a session, which are never
    /// scanned.
    pub(crate) fn local_scanner(
        &mut self,
        key: &ServerKey,
        cx: &mut Context<Self>,
    ) -> Option<Entity<LocalScanner>> {
        let entry = self.servers.get(key)?;
        if entry.setup.username.is_none() || entry.session.is_none() {
            return None;
        }
        if let Some(scanner) = &entry.local_scan {
            return Some(scanner.clone());
        }
        let checkouts = self.checkouts(key, cx)?;
        let scanner = cx.new(|cx| LocalScanner::new(checkouts, cx));
        // A round that changed local change rows may have found something
        // to send.
        let owner = key.clone();
        let sub = cx.subscribe(&scanner, move |this, _, ev: &LocalScanEvent, cx| match ev {
            LocalScanEvent::Changed => this.consolidate(&owner, Request::Now, cx),
        });
        let entry = self.servers.get_mut(key)?;
        scanner.update(cx, |s, _| s.set_session(entry.session.as_ref()));
        entry.local_scan = Some(scanner.clone());
        entry.local_scan_sub = Some(sub);
        Some(scanner)
    }

    /// Gives the consolidator of server `key` its current session, making
    /// it the first time the server is signed in (an anonymous server never
    /// gets one). It is kept from then on, also when the session goes, so
    /// that a run going on is never dropped (see `consolidation`).
    pub(super) fn sync_consolidator(
        &mut self,
        key: &ServerKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.servers.get(key) else {
            return;
        };
        let session = entry.session.clone();
        // `install_session` forgot `me` if another user signed in.
        let known = entry.me.is_some();
        if let Some(c) = entry.consolidation.clone() {
            c.update(cx, |c, cx| {
                c.set_session(session.as_ref());
                c.set_account_known(known, cx);
            });
            return;
        }
        if !session.as_ref().is_some_and(|s| s.username.is_some()) {
            return;
        }
        let Some(checkouts) = self.checkouts(key, cx) else {
            return;
        };
        let consolidator = cx.new(|cx| {
            let mut c = Consolidator::new(checkouts);
            c.set_session(session.as_ref());
            c.set_account_known(known, cx);
            c
        });
        let owner = key.clone();
        let sub = cx.subscribe_in(
            &consolidator,
            window,
            move |this, _, ev: &ConsolidationEvent, window, cx| match ev {
                ConsolidationEvent::Ran(report) => this.consolidated(&owner, report, window, cx),
            },
        );
        if let Some(entry) = self.servers.get_mut(key) {
            entry.consolidation = Some(consolidator);
            entry.consolidation_sub = Some(sub);
        }
    }

    /// The account of server `key`'s user is read (and cached): its
    /// consolidator may run.
    pub(super) fn account_known(&mut self, key: &ServerKey, cx: &mut Context<Self>) {
        if let Some(c) = self.servers.get(key).and_then(|e| e.consolidation.clone()) {
            c.update(cx, |c, cx| c.set_account_known(true, cx));
        }
    }

    /// Asks the consolidator of server `key` for a run, if the server is
    /// online (see `consolidation`): after a refresh or poll that worked, a
    /// scan round that changed rows, a saved local review.
    pub(super) fn consolidate(&mut self, key: &ServerKey, kind: Request, cx: &mut Context<Self>) {
        let Some(entry) = self.servers.get(key) else {
            return;
        };
        if entry.connection != Connection::Online {
            return;
        }
        if let Some(c) = entry.consolidation.clone() {
            c.update(cx, |c, cx| c.request(kind, cx));
        }
    }

    /// A held publish of server `key` ended (`DraftOutbox::publish_held`).
    /// The page of that change says how (it listens itself); with another
    /// page on screen it is logged. A consolidation run asked for while the
    /// publish held the lease (and found it taken) runs now, and the
    /// dashboard's local section is read again.
    pub(super) fn held_published(
        &mut self,
        key: &ServerKey,
        change: ChangeNumber,
        item: &sync::HeldItem,
        result: &Result<sync::HeldPublished, sync::Error>,
        cx: &mut Context<Self>,
    ) {
        let shown = self.current_change() == Some((key.clone(), change))
            && matches!(self.page, Page::Change(_));
        if !shown {
            match result {
                Ok(out) => log::info!(
                    "held item {item:?} of change {change} on {key} published: {:?}",
                    out.outcome
                ),
                Err(e) => {
                    log::warn!("publishing held item {item:?} of change {change} on {key}: {e}")
                }
            }
        }
        self.consolidate(key, Request::Now, cx);
        if self.nav.current_server() == Some(key)
            && let Page::List(p) = &self.page
        {
            p.update(cx, |p, cx| p.reload_local(cx));
        }
    }

    /// What a consolidation run of server `key` did: logged, and the pages
    /// follow. A refused login goes the usual way, and a lost connection
    /// marks the server offline until a refresh works again. The drafts and
    /// local reviews of every change it touched are read again, the
    /// dashboard's local section too, the scanner looks again when rows
    /// changed state, and the page on screen reloads the change and says
    /// what was sent if it is about it.
    fn consolidated(
        &mut self,
        key: &ServerKey,
        report: &sync::ConsolidateReport,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for (row, e) in &report.errors {
            log::warn!("consolidating local change {} of {key}: {e}", row.0);
        }
        let unauthorized = report
            .stopped
            .as_ref()
            .is_some_and(sync::Error::is_unauthorized);
        match &report.stopped {
            Some(e) if e.is_unauthorized() => {
                log::warn!("consolidating the local changes of {key}: {e}");
            }
            Some(sync::Error::LeaseLost) => {
                log::debug!("consolidation of {key}: another run took over");
            }
            Some(sync::Error::Stopped) => {
                log::debug!("consolidation of {key}: stopped for another session");
            }
            Some(e) => {
                log::warn!("consolidating the local changes of {key}: {e}");
                if matches!(e, sync::Error::Api(a) if a.is_retryable()) {
                    self.servers.set_connection(key, Connection::Offline);
                    cx.notify();
                }
            }
            None => {}
        }
        // What the run did before a refused login still shows; then the
        // login goes the usual way.
        let affected = consolidation::affected(report);
        if report.ran && !affected.is_empty() {
            if let Some(outbox) = self.servers.get(key).and_then(|e| e.outbox.clone()) {
                outbox.update(cx, |o, cx| o.consolidated(&affected, cx));
            }
            if consolidation::rows_changed(report) {
                self.scan_local(key, cx);
            }
        }
        if unauthorized {
            self.unauthorized_for(key, window, cx);
            return;
        }
        if !report.ran || self.nav.current_server() != Some(key) {
            return;
        }
        self.retry_redirect(window, cx);
        if affected.is_empty() {
            return;
        }
        let shown = self.current_change().map(|(_, n)| n);
        match &self.page {
            Page::List(p) => p.update(cx, |p, cx| p.reload_local(cx)),
            Page::Change(p) if shown.is_some_and(|n| affected.contains(&n)) => {
                p.update(cx, |p, cx| p.reload(cx))
            }
            _ => {}
        }
        let note = shown.and_then(|n| consolidation::note(report, n));
        self.follow_redirect(note, window, cx);
    }

    /// Sends the change or diff page on screen to where its local numbers
    /// lead now (see `consolidation::redirect`), rewriting its history
    /// entry in place, then says `note` on the page shown. Reads the store
    /// on the background executor; if the user moved on meanwhile, nothing
    /// happens.
    pub(super) fn follow_redirect(
        &mut self,
        note: Option<(String, bool)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let route = self.nav.current().clone();
        let server = self.nav.current_server().cloned();
        let view = match &self.page {
            Page::Change(p) => {
                let mut view = p.read(cx).view_state();
                if view.file.is_none() {
                    view.file = self.nav.change_view().and_then(|v| v.file);
                }
                Some(view)
            }
            _ => None,
        };
        let syncer = server
            .as_ref()
            .and_then(|k| self.servers.get(k))
            .and_then(|e| e.session.as_ref())
            .map(|s| s.syncer().clone())
            .filter(|_| consolidation::names_local(&route, view.as_ref()));
        let Some(syncer) = syncer else {
            if let Some((note, error)) = note {
                self.say_on_page(note, error, cx);
            }
            return;
        };
        let position = self.nav.position();
        cx.spawn_in(window, async move |this, cx| {
            let asked = route.clone();
            let found = cx
                .background_spawn(
                    async move { consolidation::redirect(&syncer, &asked, view.as_ref()) },
                )
                .await;
            this.update_in(cx, |this, window, cx| {
                let here = this.nav.position() == position
                    && this.nav.current() == &route
                    && this.nav.current_server() == server.as_ref();
                if !here {
                    return;
                }
                match found {
                    // A new page would take the open editor or dialog
                    // with it: later.
                    Ok(Some(_)) if this.busy_writing(cx) => this.redirect_pending = true,
                    Ok(Some(to)) => {
                        this.nav.redirect(to.route, to.view);
                        this.show(window, cx);
                    }
                    Ok(None) => {}
                    Err(e) => log::warn!("looking up where {route:?} leads now: {e}"),
                }
                if let Some((note, error)) = note {
                    this.say_on_page(note, error, cx);
                }
            })
            .ok();
        })
        .detach();
    }
}
