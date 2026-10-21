//! The draft outbox: what the UI calls to save, delete and retry drafts,
//! and the place that hands out the shared per-change comments.
//!
//! Every operation writes the local database first (on the background
//! executor), tells its listeners at once so the threads show the draft in
//! its pending state, and then tries the server and tells them again with
//! the result. This is the UI side of the store-first design in
//! `sync::Syncer`; the views never wait for the network.
//!
//! A 401 becomes [`OutboxEvent::Unauthorized`] (the shell goes back to the
//! login page); a 403 and every other failure is recorded on the draft by
//! `sync` and shown in its card, nothing more.
//!
//! The outbox also owns the registry of [`ChangeComments`]: one entity per
//! change, shared by the pages that show it (the change page and the diff
//! pages of the same change), so a draft saved in one is in the other at the
//! same moment. The registry holds them weakly: the entity goes away with
//! the last page using it.

use std::collections::HashMap;

use gpui_kit::*;
use model::{ChangeDetail, ChangeNumber, LocalDraftId};
use sync::DraftSave;

use crate::{
    comments::ChangeComments,
    diff::DraftRequest,
    session::{Session, is_unauthorized},
};

pub enum OutboxEvent {
    /// The drafts of this change changed in the local database (a draft was
    /// saved, deleted or found uploaded or failed).
    Changed(ChangeNumber),
    /// The server answered 401 to a draft request.
    Unauthorized,
}

pub struct DraftOutbox {
    session: Session,
    comments: HashMap<ChangeNumber, WeakEntity<ChangeComments>>,
}

impl EventEmitter<OutboxEvent> for DraftOutbox {}

impl DraftOutbox {
    pub fn new(session: Session) -> Self {
        Self {
            session,
            comments: HashMap::new(),
        }
    }

    /// The comments of `change`, shared with every other page showing it.
    pub fn comments(
        &mut self,
        change: ChangeNumber,
        cx: &mut Context<Self>,
    ) -> Entity<ChangeComments> {
        if let Some(c) = self.comments.get(&change).and_then(WeakEntity::upgrade) {
            return c;
        }
        self.comments.retain(|_, c| c.upgrade().is_some());
        let session = self.session.clone();
        let outbox = cx.entity();
        let c = cx.new(|cx| {
            let mut c = ChangeComments::new(session, change, cx);
            c.follow(&outbox, cx);
            c
        });
        self.comments.insert(change, c.downgrade());
        c
    }

    /// Carries out what a diff view asked for. `detail` supplies the commit
    /// of the patch set a new draft goes to.
    pub fn request(
        &mut self,
        change: ChangeNumber,
        request: DraftRequest,
        detail: Option<&ChangeDetail>,
        cx: &mut Context<Self>,
    ) {
        match request {
            DraftRequest::Create { patch_set, input } => {
                let target = detail
                    .and_then(|d| d.revisions.iter().find(|r| r.number == patch_set))
                    .map(|r| r.as_ref());
                match target {
                    Some(patch_set) => self.save(
                        DraftSave::Create {
                            change,
                            patch_set,
                            input,
                        },
                        cx,
                    ),
                    None => {
                        log::warn!("no patch set {patch_set} in change {change}: draft dropped")
                    }
                }
            }
            DraftRequest::Update {
                id,
                message,
                unresolved,
            } => self.save(
                DraftSave::Update {
                    id,
                    message,
                    unresolved,
                },
                cx,
            ),
            DraftRequest::Delete(id) => self.delete(change, id, cx),
            DraftRequest::Retry(id) => self.retry(change, id, cx),
        }
    }

    /// Writes the draft locally, then uploads it. Without credentials (the
    /// password is still being fetched) the draft only waits; a later retry
    /// uploads it.
    pub fn save(&mut self, save: DraftSave, cx: &mut Context<Self>) {
        let syncer = self.session.syncer().clone();
        let upload = self.session.can_sync();
        cx.spawn(async move |this, cx| {
            let local = {
                let syncer = syncer.clone();
                cx.background_spawn(async move { syncer.save_draft_local(save) })
                    .await
            };
            let draft = match local {
                Ok(Some(draft)) => draft,
                // Deleted meanwhile: nothing to save.
                Ok(None) => return,
                Err(e) => return log::warn!("saving a draft: {e}"),
            };
            let (change, id) = (draft.change, draft.local_id);
            this.update(cx, |_, cx| cx.emit(OutboxEvent::Changed(change)))
                .ok();
            if !upload {
                return;
            }
            let uploaded = cx
                .background_spawn(async move { syncer.upload_draft(id).await })
                .await;
            this.update(cx, |this, cx| {
                this.finished(change, uploaded.map(|_| ()), cx)
            })
            .ok();
        })
        .detach();
    }

    /// Deletes the draft locally and, if the server has it, there.
    pub fn delete(&mut self, change: ChangeNumber, id: LocalDraftId, cx: &mut Context<Self>) {
        let syncer = self.session.syncer().clone();
        let upload = self.session.can_sync();
        cx.spawn(async move |this, cx| {
            let local = {
                let syncer = syncer.clone();
                cx.background_spawn(async move { syncer.delete_draft_local(id) })
                    .await
            };
            let outcome = match local {
                Ok(outcome) => outcome,
                Err(e) => return log::warn!("deleting a draft: {e}"),
            };
            this.update(cx, |_, cx| cx.emit(OutboxEvent::Changed(change)))
                .ok();
            // A draft the server never saw is gone; one it has waits for
            // the server's confirmation.
            if outcome != store::DeleteOutcome::Pending || !upload {
                return;
            }
            let deleted = cx
                .background_spawn(async move { syncer.upload_draft(id).await })
                .await;
            this.update(cx, |this, cx| {
                this.finished(change, deleted.map(|_| ()), cx)
            })
            .ok();
        })
        .detach();
    }

    /// Tries a failed draft again.
    pub fn retry(&mut self, change: ChangeNumber, id: LocalDraftId, cx: &mut Context<Self>) {
        if !self.session.can_sync() {
            return;
        }
        let syncer = self.session.syncer().clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { syncer.retry_failed_draft(id).await })
                .await;
            this.update(cx, |this, cx| this.finished(change, result.map(|_| ()), cx))
                .ok();
        })
        .detach();
    }

    /// Uploads every pending draft of the server (after the connection is
    /// back: the login, a successful poll). Failed drafts are not retried.
    /// Pages that show comments are told when something changed.
    // Called after login and polls (a later commit).
    #[allow(dead_code)]
    pub fn retry_all(&mut self, cx: &mut Context<Self>) {
        if !self.session.can_sync() {
            return;
        }
        let syncer = self.session.syncer().clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { syncer.retry_pending_drafts().await })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(report) => {
                    if report.synced + report.deleted + report.rejected > 0 {
                        this.changed_everywhere(cx);
                    }
                }
                Err(e) if is_unauthorized(&e) => cx.emit(OutboxEvent::Unauthorized),
                Err(e) => log::warn!("retrying drafts: {e}"),
            })
            .ok();
        })
        .detach();
    }

    /// The result of an upload: log or ask for a login, and tell the
    /// listeners that the draft's state moved.
    fn finished(
        &mut self,
        change: ChangeNumber,
        result: Result<(), sync::Error>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(()) => {}
            Err(e) if is_unauthorized(&e) => cx.emit(OutboxEvent::Unauthorized),
            Err(e) => log::warn!("uploading a draft of change {change}: {e}"),
        }
        cx.emit(OutboxEvent::Changed(change));
    }

    /// Any change with comments on screen may have drafts that moved.
    #[allow(dead_code)]
    fn changed_everywhere(&mut self, cx: &mut Context<Self>) {
        let live: Vec<ChangeNumber> = self
            .comments
            .iter()
            .filter(|(_, c)| c.upgrade().is_some())
            .map(|(n, _)| *n)
            .collect();
        for change in live {
            cx.emit(OutboxEvent::Changed(change));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use gerrit_api::{Auth, fake::RouteHttpClient};
    use gpui_kit::{AppContext as _, Entity, TestAppContext};
    use model::{
        ChangeNumber, DraftInput, DraftSync, Location, PatchSetNumber, PatchSetRef, Side, ThreadId,
    };
    use store::Store;
    use sync::DraftSave;

    use super::{DraftOutbox, OutboxEvent};
    use crate::{
        comments::ChangeComments,
        session::Session,
        tests::{URL, env_with, setup},
    };

    const CHANGE: ChangeNumber = ChangeNumber(7);

    fn signed_in() -> Auth {
        Auth::Basic {
            username: "u".into(),
            password: "p".into(),
        }
    }

    fn create(message: &str) -> DraftSave {
        DraftSave::Create {
            change: CHANGE,
            patch_set: PatchSetRef {
                number: PatchSetNumber(1),
                sha: "sha1".into(),
            },
            input: DraftInput {
                path: "a.rs".into(),
                side: Side::Revision,
                parent: None,
                location: Location::Line(3),
                in_reply_to: None,
                message: message.into(),
                unresolved: true,
            },
        }
    }

    fn receipt(id: &str) -> String {
        format!(
            r#")]}}'
{{"id":"{id}","patch_set":1,"path":"a.rs","line":3,"updated":"2024-06-15 10:00:00.000000000","message":"m"}}"#
        )
    }

    struct Fx {
        outbox: Entity<DraftOutbox>,
        comments: Entity<ChangeComments>,
        /// The draft sync state each `Changed` event found in the store.
        states: Arc<Mutex<Vec<String>>>,
        unauthorized: Arc<Mutex<usize>>,
        session: Session,
    }

    fn fx(cx: &mut TestAppContext, http: &RouteHttpClient) -> Fx {
        let mut store = Store::open_in_memory().expect("store");
        store.server_id(URL).expect("server");
        let env = env_with(store, http);
        let session = Session::new(&env, &setup(), Some(signed_in())).expect("session");
        let outbox = cx.new(|_| DraftOutbox::new(session.clone()));
        let comments = cx.update(|cx| outbox.update(cx, |o, cx| o.comments(CHANGE, cx)));
        let states: Arc<Mutex<Vec<String>>> = Arc::default();
        let unauthorized = Arc::new(Mutex::new(0));
        let (log, syncer, count) = (
            states.clone(),
            session.syncer().clone(),
            unauthorized.clone(),
        );
        cx.update(|cx| {
            cx.subscribe(&outbox, move |_, ev: &OutboxEvent, _| match ev {
                OutboxEvent::Changed(c) => {
                    let view = syncer.cached_comments(*c).expect("cache");
                    let text = view
                        .drafts
                        .iter()
                        .map(|d| match &d.sync {
                            DraftSync::Failed { error, .. } => format!("failed: {error}"),
                            other => format!("{other:?}"),
                        })
                        .collect::<Vec<_>>()
                        .join(",");
                    log.lock().expect("log").push(text);
                }
                OutboxEvent::Unauthorized => *count.lock().expect("count") += 1,
            })
            .detach();
        });
        cx.run_until_parked();
        Fx {
            outbox,
            comments,
            states,
            unauthorized,
            session,
        }
    }

    fn save(cx: &mut TestAppContext, fx: &Fx, message: &str) {
        let save = create(message);
        cx.update(|cx| fx.outbox.update(cx, |o, cx| o.save(save, cx)));
        cx.run_until_parked();
    }

    fn states(fx: &Fx) -> Vec<String> {
        fx.states.lock().expect("log").clone()
    }

    #[gpui_kit::test]
    fn a_saved_draft_shows_as_pending_at_once_and_then_as_synced(cx: &mut TestAppContext) {
        let http = RouteHttpClient::new();
        http.route("/revisions/sha1/drafts", 201, receipt("srv1"));
        let fx = fx(cx, &http);
        save(cx, &fx, "hello");
        assert_eq!(states(&fx), ["PendingCreate", "Synced"]);
        // The shared comments reloaded from the cache: one thread, the draft.
        cx.update(|cx| {
            let threads = fx.comments.read(cx).threads();
            assert_eq!(threads.len(), 1);
            assert!(matches!(threads[0].id, ThreadId::Draft(_)));
            assert_eq!(threads[0].draft().expect("draft").sync, DraftSync::Synced);
        });
    }

    #[gpui_kit::test]
    fn a_refused_upload_shows_the_draft_as_failed(cx: &mut TestAppContext) {
        let http = RouteHttpClient::new();
        http.route("/revisions/sha1/drafts", 400, "no empty comments");
        let fx = fx(cx, &http);
        save(cx, &fx, "hello");
        let states = states(&fx);
        assert_eq!(states[0], "PendingCreate");
        assert!(states[1].starts_with("failed: "), "{states:?}");
        cx.update(|cx| {
            let threads = fx.comments.read(cx).threads();
            assert!(matches!(
                threads[0].draft().expect("draft").sync,
                DraftSync::Failed { .. }
            ));
        });
        assert_eq!(*fx.unauthorized.lock().expect("count"), 0);
    }

    #[gpui_kit::test]
    fn a_401_asks_for_a_login_and_a_403_does_not(cx: &mut TestAppContext) {
        let http = RouteHttpClient::new();
        http.route("/revisions/sha1/drafts", 403, "forbidden");
        let forbidden = fx(cx, &http);
        save(cx, &forbidden, "hello");
        assert_eq!(*forbidden.unauthorized.lock().expect("count"), 0);
        assert!(states(&forbidden)[1].starts_with("failed: "));

        let http = RouteHttpClient::new();
        http.route("/revisions/sha1/drafts", 401, "");
        let expired = fx(cx, &http);
        save(cx, &expired, "hello");
        assert_eq!(*expired.unauthorized.lock().expect("count"), 1);
        // The draft is kept for after the login.
        assert_eq!(states(&expired)[1], "PendingCreate");
    }

    #[gpui_kit::test]
    fn deleting_removes_the_draft_and_asks_the_server_only_if_it_has_it(cx: &mut TestAppContext) {
        let http = RouteHttpClient::new();
        http.route("/revisions/sha1/drafts", 201, receipt("srv1"));
        http.route("/drafts/srv1", 204, "");
        let fx = fx(cx, &http);
        // Never uploaded (offline): only the local row goes.
        http.route("/revisions/sha1/drafts", 503, "down");
        save(cx, &fx, "offline");
        let id = cx.update(|cx| {
            fx.comments.read(cx).threads()[0]
                .draft()
                .expect("d")
                .local_id
        });
        let before = http.requested().len();
        cx.update(|cx| fx.outbox.update(cx, |o, cx| o.delete(CHANGE, id, cx)));
        cx.run_until_parked();
        assert_eq!(http.requested().len(), before);
        cx.update(|cx| assert!(fx.comments.read(cx).threads().is_empty()));

        // One the server has: deleted there too.
        http.route("/revisions/sha1/drafts", 201, receipt("srv1"));
        save(cx, &fx, "online");
        let id = cx.update(|cx| {
            fx.comments.read(cx).threads()[0]
                .draft()
                .expect("d")
                .local_id
        });
        cx.update(|cx| fx.outbox.update(cx, |o, cx| o.delete(CHANGE, id, cx)));
        cx.run_until_parked();
        assert!(http.requested().iter().any(|r| r.contains("/drafts/srv1")));
        cx.update(|cx| assert!(fx.comments.read(cx).threads().is_empty()));
        assert!(
            fx.session
                .syncer()
                .cached_comments(CHANGE)
                .expect("cache")
                .drafts
                .is_empty()
        );
    }

    #[gpui_kit::test]
    fn retry_all_uploads_what_waited_and_a_failed_draft_is_retried_by_hand(
        cx: &mut TestAppContext,
    ) {
        let http = RouteHttpClient::new();
        http.route("/revisions/sha1/drafts", 503, "down");
        let fx = fx(cx, &http);
        save(cx, &fx, "waits");
        assert_eq!(
            states(&fx).last().map(String::as_str),
            Some("PendingCreate")
        );

        http.route("/revisions/sha1/drafts", 201, receipt("srv1"));
        cx.update(|cx| fx.outbox.update(cx, |o, cx| o.retry_all(cx)));
        cx.run_until_parked();
        cx.update(|cx| {
            let threads = fx.comments.read(cx).threads();
            assert_eq!(threads[0].draft().expect("draft").sync, DraftSync::Synced);
        });

        // A rejected draft stays failed until retried by hand.
        let http = RouteHttpClient::new();
        http.route("/revisions/sha1/drafts", 400, "bad");
        let failing = fx_for(cx, &http);
        save(cx, &failing, "rejected");
        http.route("/revisions/sha1/drafts", 201, receipt("srv2"));
        cx.update(|cx| failing.outbox.update(cx, |o, cx| o.retry_all(cx)));
        cx.run_until_parked();
        let id = cx.update(|cx| {
            let t = failing.comments.read(cx).threads();
            let d = t[0].draft().expect("draft").clone();
            assert!(matches!(d.sync, DraftSync::Failed { .. }));
            d.local_id
        });
        cx.update(|cx| failing.outbox.update(cx, |o, cx| o.retry(CHANGE, id, cx)));
        cx.run_until_parked();
        cx.update(|cx| {
            let t = failing.comments.read(cx).threads();
            assert_eq!(t[0].draft().expect("draft").sync, DraftSync::Synced);
        });
    }

    fn fx_for(cx: &mut TestAppContext, http: &RouteHttpClient) -> Fx {
        fx(cx, http)
    }

    #[gpui_kit::test]
    fn one_comments_entity_per_change_while_it_is_in_use(cx: &mut TestAppContext) {
        let http = RouteHttpClient::new();
        let fx = fx(cx, &http);
        let again = cx.update(|cx| fx.outbox.update(cx, |o, cx| o.comments(CHANGE, cx)));
        assert_eq!(again.entity_id(), fx.comments.entity_id());
        let other = cx.update(|cx| {
            fx.outbox
                .update(cx, |o, cx| o.comments(ChangeNumber(8), cx))
        });
        assert_ne!(other.entity_id(), fx.comments.entity_id());
        let id = fx.comments.entity_id();
        drop((again, fx.comments));
        cx.run_until_parked();
        let fresh = cx.update(|cx| fx.outbox.update(cx, |o, cx| o.comments(CHANGE, cx)));
        assert_ne!(fresh.entity_id(), id);
    }
}
