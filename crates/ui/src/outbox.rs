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
use model::{
    ChangeDetail, ChangeNumber, DraftInput, Entry, LocalDraftId, PatchSetNumber, PatchSetRef,
};
use sync::{DraftCounts, DraftSave};

use crate::{
    comments::ChangeComments,
    diff::DraftRequest,
    session::{Session, is_unauthorized},
};

/// What a page says when an edit of a draft was saved as a new draft
/// because a local review post is sending the draft (or has sent it).
pub const KEPT_AS_NEW: &str =
    "Saved as a new draft: the comment you edited is being sent to Gerrit with your local review";

/// What a page says when deleting such a draft was refused.
pub const DELETE_BEING_SENT: &str = "This comment is being sent to Gerrit with your local \
     review. Once it is there it is public and cannot be deleted here";

pub enum OutboxEvent {
    /// The drafts of this change changed in the local database (a draft was
    /// saved, deleted or found uploaded or failed).
    Changed(ChangeNumber),
    /// The server answered 401 to a draft request.
    Unauthorized,
    /// An edit of a draft of `change` could not change it, because a
    /// local review post is sending the draft (`DraftBeingSent`) or has
    /// sent it (the draft is gone): the text was saved as a new ordinary
    /// draft at the same place instead (see [`DraftOutbox::request`]).
    KeptAsNew(ChangeNumber),
    /// A delete of a draft of `change` was refused for the same reason
    /// (`store::DeleteOutcome::BeingSent`).
    DeleteBeingSent(ChangeNumber),
}

pub struct DraftOutbox {
    session: Session,
    comments: HashMap<ChangeNumber, WeakEntity<ChangeComments>>,
    /// Drafts not on the server, over all changes (for the title bar).
    counts: DraftCounts,
    counts_task: Option<Task<()>>,
}

impl EventEmitter<OutboxEvent> for DraftOutbox {}

/// What an edit falls back to when its draft cannot take it (see
/// [`DraftOutbox::request`]).
struct Fallback {
    /// The change of the page that asked.
    page: ChangeNumber,
    id: LocalDraftId,
    /// The new draft at the place the page showed the draft, if it did.
    shown: Option<DraftSave>,
    message: String,
    unresolved: bool,
}

/// A new draft with `message` at `d`'s place: path, side, line or range,
/// the comment it replies to.
fn as_new(d: &model::Draft, message: &str, unresolved: bool) -> DraftSave {
    DraftSave::Create {
        change: d.change,
        patch_set: PatchSetRef {
            number: d.patch_set,
            sha: d.revision.clone(),
        },
        input: DraftInput {
            message: message.to_owned(),
            unresolved,
            ..d.input()
        },
    }
}

/// `save` with a new draft on a local version that is a Gerrit patch set
/// by now (the version was pushed, and a consolidation run mapped it) moved
/// onto that patch set of the Gerrit change: the local change is done
/// there, and the draft is an ordinary one that uploads. Other saves stay
/// as they are. Blocking (SQLite).
fn on_gerrit(syncer: &sync::Syncer, save: DraftSave) -> Result<DraftSave, sync::Error> {
    let DraftSave::Create {
        change,
        patch_set,
        input,
    } = save
    else {
        return Ok(save);
    };
    let moved = match syncer.local_patch_set(patch_set.number)? {
        Some(v) => match v.gerrit_patch_set {
            Some(gerrit_ps) => v
                .local_change
                .number()
                .map(|n| syncer.local_change(n))
                .transpose()?
                .flatten()
                .and_then(|(row, _)| row.gerrit_number)
                .map(|gerrit| (gerrit, gerrit_ps, v.sha)),
            None => None,
        },
        None => None,
    };
    Ok(match moved {
        Some((gerrit, number, sha)) => DraftSave::Create {
            change: gerrit,
            patch_set: PatchSetRef { number, sha },
            input,
        },
        None => DraftSave::Create {
            change,
            patch_set,
            input,
        },
    })
}

impl DraftOutbox {
    pub fn new(session: Session) -> Self {
        Self {
            session,
            comments: HashMap::new(),
            counts: DraftCounts::default(),
            counts_task: None,
        }
    }

    /// Counts the drafts and uploads the pending ones. Call it when the
    /// session starts (login, restored credentials): drafts written last
    /// time may still be waiting.
    pub fn resume(&mut self, cx: &mut Context<Self>) {
        self.refresh_counts(cx);
        self.retry_all(cx);
    }

    /// The drafts that are not on the server.
    pub fn counts(&self) -> DraftCounts {
        self.counts
    }

    /// Reads the counts from the database; observers are notified when
    /// they changed.
    fn refresh_counts(&mut self, cx: &mut Context<Self>) {
        let syncer = self.session.syncer().clone();
        self.counts_task = Some(cx.spawn(async move |this, cx| {
            let counts = cx
                .background_spawn(async move { syncer.draft_counts() })
                .await;
            this.update(cx, |this, cx| match counts {
                Ok(counts) if counts != this.counts => {
                    this.counts = counts;
                    cx.notify();
                }
                Ok(_) => {}
                Err(e) => log::warn!("counting drafts: {e}"),
            })
            .ok();
        }));
    }

    /// A review was published: the drafts of `change` became comments (or
    /// were refused), so the listeners read them again and the counts too.
    pub fn published(&mut self, change: ChangeNumber, cx: &mut Context<Self>) {
        self.announce(change, cx);
    }

    /// A consolidation run moved or sent the drafts and local reviews of
    /// `changes` (Gerrit and synthetic numbers): the listeners read them
    /// again, and the comments of the Gerrit changes on screen are fetched
    /// again, since what was sent is published now.
    pub fn consolidated(&mut self, changes: &[ChangeNumber], cx: &mut Context<Self>) {
        for &change in changes {
            cx.emit(OutboxEvent::Changed(change));
            if change.is_local() {
                continue;
            }
            if let Some(c) = self.comments.get(&change).and_then(WeakEntity::upgrade) {
                c.update(cx, |c, cx| c.refresh(cx));
            }
        }
        self.refresh_counts(cx);
    }

    /// Tells the listeners that the drafts of `change` changed.
    fn announce(&mut self, change: ChangeNumber, cx: &mut Context<Self>) {
        cx.emit(OutboxEvent::Changed(change));
        self.refresh_counts(cx);
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
    /// of the patch set a new draft goes to: the page's detail from
    /// [`crate::change_detail`], so a local patch set is found too.
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
                    // A local version the page's detail no longer lists
                    // (it reached Gerrit meanwhile): the store knows it.
                    None if patch_set.is_local() => {
                        self.save_on_local_version(change, patch_set, input, cx)
                    }
                    None => {
                        log::warn!("no patch set {patch_set} in change {change}: draft dropped")
                    }
                }
            }
            DraftRequest::Update {
                id,
                message,
                unresolved,
            } => {
                // Where the draft is as the pages show it: a new draft goes
                // there if this one cannot take the text any more.
                let shown = self
                    .comments
                    .get(&change)
                    .and_then(WeakEntity::upgrade)
                    .and_then(|c| {
                        c.read(cx)
                            .threads()
                            .iter()
                            .flat_map(|t| &t.entries)
                            .find_map(|e| match e {
                                Entry::Draft(d) if d.local_id == id => Some(d.clone()),
                                _ => None,
                            })
                    });
                let fallback = Fallback {
                    page: change,
                    id,
                    shown: shown.map(|d| as_new(&d, &message, unresolved)),
                    message: message.clone(),
                    unresolved,
                };
                self.save_with(
                    DraftSave::Update {
                        id,
                        message,
                        unresolved,
                    },
                    Some(fallback),
                    cx,
                )
            }
            DraftRequest::Delete(id) => self.delete(change, id, cx),
            DraftRequest::Retry(id) => self.retry(change, id, cx),
        }
    }

    /// Makes the patch-set level message draft of the reply dialog hold
    /// `text` (see `reply::message_step`): changed, created, or deleted when
    /// the text is empty. The draft is looked up in the store, not in a
    /// view, so a draft that was written a moment ago is found.
    pub fn set_message(
        &mut self,
        change: ChangeNumber,
        patch_set: model::PatchSetRef,
        text: String,
        unresolved: bool,
        cx: &mut Context<Self>,
    ) {
        use crate::reply::{MessageStep, message_step};
        let syncer = self.session.syncer().clone();
        cx.spawn(async move |this, cx| {
            let step = cx
                .background_spawn(async move {
                    message_step(&syncer, change, &patch_set, &text, unresolved)
                })
                .await;
            this.update(cx, |this, cx| match step {
                Ok(Some(MessageStep::Save(save))) => this.save(save, cx),
                Ok(Some(MessageStep::Delete(id))) => this.delete(change, id, cx),
                Ok(None) => {}
                Err(e) => log::warn!("saving the reply message of change {change}: {e}"),
            })
            .ok();
        })
        .detach();
    }

    /// Writes the draft locally, then uploads it. Without credentials (the
    /// password is still being fetched) the draft only waits; a later retry
    /// uploads it.
    pub fn save(&mut self, save: DraftSave, cx: &mut Context<Self>) {
        self.save_with(save, None, cx);
    }

    /// A new draft on local version `patch_set`, whose commit is looked up
    /// in the store.
    fn save_on_local_version(
        &mut self,
        change: ChangeNumber,
        patch_set: PatchSetNumber,
        input: DraftInput,
        cx: &mut Context<Self>,
    ) {
        let syncer = self.session.syncer().clone();
        cx.spawn(async move |this, cx| {
            let version = cx
                .background_spawn(async move { syncer.local_patch_set(patch_set) })
                .await;
            this.update(cx, |this, cx| match version {
                Ok(Some(v)) => this.save(
                    DraftSave::Create {
                        change,
                        patch_set: PatchSetRef {
                            number: patch_set,
                            sha: v.sha,
                        },
                        input,
                    },
                    cx,
                ),
                Ok(None) => log::warn!("no local patch set {patch_set}: draft dropped"),
                Err(e) => log::warn!("reading local patch set {patch_set}: {e}"),
            })
            .ok();
        })
        .detach();
    }

    /// [`Self::save`]. A new draft on a local version that is a Gerrit
    /// patch set by now goes onto that patch set of the Gerrit change
    /// ([`on_gerrit`]). `fallback` is for an edit: if the draft cannot take
    /// it, because a local review post is sending it (`DraftBeingSent`) or
    /// has sent it (the draft is gone), its text is saved as this new
    /// draft instead, and [`OutboxEvent::KeptAsNew`] names the page's
    /// change (the first part). So no text the user saved is ever lost.
    fn save_with(&mut self, save: DraftSave, fallback: Option<Fallback>, cx: &mut Context<Self>) {
        let syncer = self.session.syncer().clone();
        let upload = self.session.can_upload();
        cx.spawn(async move |this, cx| {
            let local = {
                let syncer = syncer.clone();
                cx.background_spawn(async move {
                    let save = on_gerrit(&syncer, save)?;
                    let new = match (syncer.save_draft_local(save), fallback) {
                        // Being sent: the store has the draft, at the place
                        // it is sent to.
                        (Err(sync::Error::Store(store::Error::DraftBeingSent)), Some(f)) => {
                            let stored = syncer
                                .cached_comments(f.page)?
                                .drafts
                                .into_iter()
                                .find(|d| d.local_id == f.id);
                            let new = stored
                                .map(|d| as_new(&d, &f.message, f.unresolved))
                                .or(f.shown);
                            (new, f.page)
                        }
                        // Gone (sent meanwhile): where the page showed it.
                        (Ok(None), Some(f)) => (f.shown, f.page),
                        (result, _) => return Ok::<_, sync::Error>(result?.map(|d| (d, None))),
                    };
                    let (Some(new), page) = new else {
                        log::warn!("an edit of a draft being sent could not be kept: no place");
                        return Ok(None);
                    };
                    log::info!(
                        "a draft edit came while the draft was being sent; saved as a new draft"
                    );
                    let new = on_gerrit(&syncer, new)?;
                    Ok(syncer.save_draft_local(new)?.map(|d| (d, Some(page))))
                })
                .await
            };
            let (draft, kept) = match local {
                Ok(Some(saved)) => saved,
                // Deleted meanwhile: nothing to save.
                Ok(None) => return,
                Err(e) => return log::warn!("saving a draft: {e}"),
            };
            if let Some(page) = kept {
                this.update(cx, |this, cx| {
                    this.announce(page, cx);
                    cx.emit(OutboxEvent::KeptAsNew(page));
                })
                .ok();
            }
            let (change, id) = (draft.change, draft.local_id);
            this.update(cx, |this, cx| this.announce(change, cx)).ok();
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
        let upload = self.session.can_upload();
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
            if outcome == store::DeleteOutcome::BeingSent {
                this.update(cx, |_, cx| cx.emit(OutboxEvent::DeleteBeingSent(change)))
                    .ok();
                return;
            }
            this.update(cx, |this, cx| this.announce(change, cx)).ok();
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
        if !self.session.can_upload() {
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
    pub fn retry_all(&mut self, cx: &mut Context<Self>) {
        self.retry_pending(false, cx);
    }

    /// The title bar's retry: also tries the failed drafts that may work
    /// on a second try (see `Syncer::retry_failed_drafts`).
    pub fn retry_everything(&mut self, cx: &mut Context<Self>) {
        self.retry_pending(true, cx);
    }

    fn retry_pending(&mut self, failed_too: bool, cx: &mut Context<Self>) {
        if !self.session.can_upload() {
            return;
        }
        let syncer = self.session.syncer().clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    if failed_too {
                        syncer.retry_failed_drafts().await
                    } else {
                        syncer.retry_pending_drafts().await
                    }
                })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(report) => {
                    if report.synced + report.deleted + report.rejected > 0 {
                        this.changed_everywhere(cx);
                    }
                    this.refresh_counts(cx);
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
        self.announce(change, cx);
    }

    /// Any change with comments on screen may have drafts that moved.
    fn changed_everywhere(&mut self, cx: &mut Context<Self>) {
        let live: Vec<ChangeNumber> = self
            .comments
            .iter()
            .filter(|(_, c)| c.upgrade().is_some())
            .map(|(n, _)| *n)
            .collect();
        for change in live {
            self.announce(change, cx);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
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

    pub(crate) const CHANGE: ChangeNumber = ChangeNumber(7);

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
        /// The refusals of drafts being sent, as text.
        refused: Arc<Mutex<Vec<String>>>,
        session: Session,
        store: Arc<Mutex<Store>>,
    }

    fn fx(cx: &mut TestAppContext, http: &RouteHttpClient) -> Fx {
        let mut store = Store::open_in_memory().expect("store");
        store.server_id(URL).expect("server");
        let env = env_with(store, http);
        let store = env.store.clone();
        let mut server = setup();
        server.username = Some("u".into());
        let session = Session::new(&env, &server, Some(signed_in())).expect("session");
        let outbox = cx.new(|_| DraftOutbox::new(session.clone()));
        let comments = cx.update(|cx| outbox.update(cx, |o, cx| o.comments(CHANGE, cx)));
        let states: Arc<Mutex<Vec<String>>> = Arc::default();
        let unauthorized = Arc::new(Mutex::new(0));
        let refused: Arc<Mutex<Vec<String>>> = Arc::default();
        let (log, syncer, count, refusals) = (
            states.clone(),
            session.syncer().clone(),
            unauthorized.clone(),
            refused.clone(),
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
                OutboxEvent::KeptAsNew(change) => {
                    refusals.lock().expect("log").push(format!("kept {change}"))
                }
                OutboxEvent::DeleteBeingSent(change) => refusals
                    .lock()
                    .expect("log")
                    .push(format!("delete {change}")),
            })
            .detach();
        });
        cx.run_until_parked();
        Fx {
            outbox,
            comments,
            states,
            unauthorized,
            refused,
            session,
            store,
        }
    }

    /// A draft of change 7 that a consolidation post is sending: written
    /// on a local version of the change, moved onto patch set 1 (the same
    /// commit) and claimed.
    pub(crate) fn claimed_draft(
        store: &Mutex<Store>,
        syncer: &sync::Syncer,
    ) -> (model::LocalChangeId, model::LocalDraftId) {
        use store::{
            Claimant, ConsolidationRewrite, LocalChangeUpsert, LocalOp, NewLocalPatchSet,
            PatchSetMove,
        };
        let server = store::ServerId(1);
        let now: model::Timestamp = "2024-06-15T10:00:00Z".parse().expect("time");
        let cid = "I0123456789abcdef0123456789abcdef01234567";
        let (row, version) = {
            let mut s = store.lock().expect("store");
            let upsert = LocalChangeUpsert {
                seen: None,
                branch: Some("main".into()),
                change_id: cid.into(),
                gerrit_number: Some(CHANGE),
                subject: "s".into(),
                author_name: "Me".into(),
                author_email: "me@example.com".into(),
                updated: now,
                state: None,
                patch_sets: vec![NewLocalPatchSet {
                    sha: "sha1".into(),
                    parent_sha: None,
                    seen_at: now,
                }],
            };
            s.reconcile_local(server, "p", &[LocalOp::Upsert(upsert)], now)
                .expect("reconcile");
            let row = s
                .local_changes_by_change_id(server, cid)
                .expect("rows")
                .remove(0);
            let version = s
                .local_patch_sets(server, row.id)
                .expect("versions")
                .remove(0);
            (row, version)
        };
        let draft = syncer
            .save_draft_local(DraftSave::Create {
                change: CHANGE,
                patch_set: PatchSetRef {
                    number: version.number,
                    sha: version.sha.clone(),
                },
                input: DraftInput {
                    path: "a.rs".into(),
                    side: Side::Revision,
                    parent: None,
                    location: Location::Line(3),
                    in_reply_to: None,
                    message: "as written".into(),
                    unresolved: true,
                },
            })
            .expect("draft")
            .expect("saved");
        let mut s = store.lock().expect("store");
        let to = PatchSetRef {
            number: PatchSetNumber(1),
            sha: "sha1".into(),
        };
        s.rewrite_local_change(
            server,
            &ConsolidationRewrite {
                local_change: row.id,
                gerrit_number: CHANGE,
                moves: vec![PatchSetMove {
                    from: version.number,
                    to,
                    exact: true,
                    drafts: vec![(draft.local_id, None)],
                    reviews: Vec::new(),
                }],
                keep: Vec::new(),
                review_states: Vec::new(),
                state: None,
                else_state: None,
            },
        )
        .expect("rewrite")
        .expect("rewritten");
        let by = Claimant {
            owner: "another run",
            at: now,
            account: None,
        };
        s.claim_local_post(server, row.id, None, PatchSetNumber(1), "n0", &by)
            .expect("claim")
            .expect("claimed");
        (row.id, draft.local_id)
    }

    /// The drafts of change 7 in the store, as (message, ordinary).
    fn drafts_of(fx: &Fx) -> Vec<(String, bool)> {
        let mut out: Vec<(String, bool)> = fx
            .session
            .syncer()
            .cached_comments(CHANGE)
            .expect("cache")
            .drafts
            .iter()
            .map(|d| (d.message.clone(), d.is_ordinary()))
            .collect();
        out.sort();
        out
    }

    #[gpui_kit::test]
    fn an_edit_of_a_draft_being_sent_becomes_a_new_draft(cx: &mut TestAppContext) {
        use crate::diff::DraftRequest;

        let http = RouteHttpClient::new();
        let fx = fx(cx, &http);
        let (row, id) = claimed_draft(&fx.store, fx.session.syncer());
        // The pages show the claimed draft.
        cx.update(|cx| fx.outbox.update(cx, |o, cx| o.published(CHANGE, cx)));
        cx.run_until_parked();
        let request = |cx: &mut TestAppContext, r: DraftRequest| {
            cx.update(|cx| fx.outbox.update(cx, |o, cx| o.request(CHANGE, r, None, cx)));
            cx.run_until_parked();
        };
        let edit = |message: &str| DraftRequest::Update {
            id,
            message: message.into(),
            unresolved: false,
        };
        request(cx, edit("edited"));
        request(cx, DraftRequest::Delete(id));
        assert_eq!(
            *fx.refused.lock().expect("log"),
            ["kept 7".to_owned(), "delete 7".to_owned()]
        );
        // The claimed draft is as it was; the text is a new ordinary draft
        // at its place.
        assert_eq!(
            drafts_of(&fx),
            [
                ("as written".to_owned(), false),
                ("edited".to_owned(), true)
            ]
        );
        let new = fx
            .session
            .syncer()
            .cached_comments(CHANGE)
            .expect("cache")
            .drafts
            .into_iter()
            .find(|d| d.message == "edited")
            .expect("new draft");
        assert_eq!(
            (
                new.patch_set,
                new.path.as_str(),
                new.location,
                new.unresolved
            ),
            (PatchSetNumber(1), "a.rs", Location::Line(3), false)
        );

        // The post ends and takes the claimed draft away; an edit sent
        // before the pages heard of it is kept the same way.
        let version = fx
            .store
            .lock()
            .expect("store")
            .load_draft(store::ServerId(1), id)
            .expect("load")
            .expect("draft")
            .version;
        fx.store
            .lock()
            .expect("store")
            .finish_local_post(store::ServerId(1), row, "n0", &[(id, version)], &[])
            .expect("finish")
            .expect("finished");
        request(cx, edit("edited again"));
        assert_eq!(
            drafts_of(&fx),
            [
                ("edited".to_owned(), true),
                ("edited again".to_owned(), true)
            ]
        );
        // And the threads, read again, show both.
        let shown: Vec<String> = cx.update(|cx| {
            fx.comments
                .read(cx)
                .threads()
                .iter()
                .flat_map(|t| t.draft().map(|d| d.message.clone()))
                .collect()
        });
        assert_eq!(shown.len(), 2, "{shown:?}");
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

    #[gpui_kit::test]
    fn the_counts_follow_the_drafts_and_resume_uploads_what_waited(cx: &mut TestAppContext) {
        let http = RouteHttpClient::new();
        http.route("/revisions/sha1/drafts", 503, "down");
        let fx = fx(cx, &http);
        let counts = |cx: &mut TestAppContext| cx.update(|cx| fx.outbox.read(cx).counts());
        assert!(counts(cx).is_empty());
        save(cx, &fx, "waits");
        assert_eq!((counts(cx).pending, counts(cx).failed), (1, 0));

        // The server is back: resume (the login, or a poll) uploads it.
        http.route("/revisions/sha1/drafts", 201, receipt("srv1"));
        cx.update(|cx| fx.outbox.update(cx, |o, cx| o.resume(cx)));
        cx.run_until_parked();
        assert!(counts(cx).is_empty());

        // A refusal is a failed draft; the title bar's retry tries it again
        // only if the server's answer leaves hope.
        http.route("/revisions/sha1/drafts", 500, "oops");
        save(cx, &fx, "flaky");
        assert_eq!((counts(cx).pending, counts(cx).failed), (0, 1));
        http.route("/revisions/sha1/drafts", 201, receipt("srv2"));
        cx.update(|cx| fx.outbox.update(cx, |o, cx| o.retry_everything(cx)));
        cx.run_until_parked();
        assert!(counts(cx).is_empty());
    }
}
