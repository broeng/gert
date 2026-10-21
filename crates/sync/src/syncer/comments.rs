//! Comments, drafts, reviews and reviewed marks.
//!
//! # Drafts: store first, upload second
//!
//! Every draft operation writes the local database first, so the UI can
//! show the result at once and editing works offline, then tries to upload.
//! A transient failure (no network, a 502/503/504) leaves the draft pending
//! with `last_error` set; [`Syncer::retry_pending_drafts`] uploads what is
//! pending, oldest first. The UI should call it on startup and after every
//! successful poll (the connection is evidently back), and may call it from
//! a timer. A rejection by the server (403, 409, 400, 404, other statuses)
//! marks the draft `Failed` with the server's message; failed drafts are not
//! retried automatically ([`Syncer::retry_failed_draft`] does it on demand).
//! A 401 stops everything and is returned as an error
//! ([`crate::Error::is_unauthorized`]); the drafts stay pending.
//!
//! # Publishing needs the server
//!
//! [`Syncer::publish_review`] has no outbox. `PUBLISH_ALL_REVISIONS`
//! publishes the drafts the *server* has, so before posting, the change's
//! pending drafts are uploaded, and if any draft is still not on the server
//! (offline, failed, or a delete not yet confirmed) nothing is posted and
//! [`crate::Error::DraftsNotUploaded`] lists them.
//!
//! # Reviewed marks
//!
//! [`Syncer::set_reviewed`] updates the cached file lists first (the file
//! list shows the new state at once), then sends the request. If it fails,
//! for any reason including being offline, the cache is put back and the
//! error returned: marks are cheap to set again, so there is no queue for
//! them.

use futures::future::join;
use gerrit_api::Error as ApiError;
use model::{
    ChangeDetail, ChangeNumber, Comment, Draft, DraftHandling, DraftInput, DraftSync, LocalDraftId,
    PatchSetRef, ReviewInput, ReviewResult, RevisionId, Thread, Timestamp, build_threads,
};
use store::{DeleteOutcome, DraftCounts, DraftFailure};

use super::Syncer;
use crate::{Error, PendingDraft};

/// The cached comments of a change: what to build threads from.
#[derive(Clone, Debug, PartialEq)]
pub struct CommentsView {
    pub comments: Vec<Comment>,
    /// The user's drafts, except those being deleted.
    pub drafts: Vec<Draft>,
    /// When the published comments were last fetched; `None` if never.
    pub fetched_at: Option<Timestamp>,
}

impl CommentsView {
    /// The threads, sorted for a change-level list.
    pub fn threads(&self) -> Vec<Thread> {
        build_threads(&self.comments, &self.drafts)
    }
}

/// A draft to write locally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DraftSave {
    /// A new draft on a patch set.
    Create {
        change: ChangeNumber,
        patch_set: PatchSetRef,
        input: DraftInput,
    },
    /// New text (and resolution) for an existing draft.
    Update {
        id: LocalDraftId,
        message: String,
        unresolved: bool,
    },
}

/// What [`Syncer::retry_pending_drafts`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RetryReport {
    /// Drafts now on the server.
    pub synced: usize,
    /// Drafts whose deletion the server confirmed (or that had vanished).
    pub deleted: usize,
    /// Drafts the server refused; they are `Failed` now.
    pub rejected: usize,
    /// Drafts still waiting. When non-zero the retry stopped at the first
    /// transient failure (probably offline) instead of trying the rest.
    pub still_pending: usize,
}

/// What [`Syncer::publish_review`] achieved.
#[derive(Clone, Debug, PartialEq)]
pub struct PublishOutcome {
    /// The server's answer; check [`ReviewResult::error`].
    pub result: ReviewResult,
    /// The change as it is now, or `None` if refreshing it failed (the
    /// review itself was posted).
    pub detail: Option<ChangeDetail>,
    /// The comments as they are now, or `None` if refreshing failed.
    pub comments: Option<CommentsView>,
    /// Votes the server ignored (a label it does not have, a value the
    /// label does not allow): Gerrit answers 200 for those. The review
    /// itself was posted. Check [`ReviewInput`]'s permitted values before
    /// sending to avoid it.
    pub unapplied_labels: Vec<(String, i32)>,
}

/// What an upload attempt of one draft concluded.
enum Step {
    Uploaded(String),
    Deleted,
}

/// How a failed request is recorded on the draft.
fn failure_for(e: &ApiError) -> DraftFailure {
    if e.is_retryable() {
        return DraftFailure::Transient(e.to_string());
    }
    let message = |m: &str| {
        if m.is_empty() {
            e.to_string()
        } else {
            m.to_owned()
        }
    };
    match e {
        ApiError::Forbidden { message: m }
        | ApiError::BadRequest { message: m }
        | ApiError::Conflict { message: m } => DraftFailure::Rejected {
            error: message(m),
            retryable: false,
        },
        // The change or patch set is gone.
        ApiError::NotFound | ApiError::InvalidUrl(_) => DraftFailure::Rejected {
            error: e.to_string(),
            retryable: false,
        },
        // A 500 or an unreadable answer: unknown, so let the user retry.
        _ => DraftFailure::Rejected {
            error: e.to_string(),
            retryable: true,
        },
    }
}

impl Syncer {
    // ---- reading ----

    fn comments_view(&self) -> impl Fn(&store::Store, ChangeNumber) -> Result<CommentsView, Error> {
        let server = self.server;
        move |store, change| {
            let cached = store.load_comments(server, change)?;
            let drafts = store
                .load_drafts(server, change)?
                .into_iter()
                .filter(Draft::is_visible)
                .collect();
            Ok(CommentsView {
                fetched_at: cached.as_ref().map(|c| c.fetched_at),
                comments: cached.map(|c| c.comments).unwrap_or_default(),
                drafts,
            })
        }
    }

    /// The cached published comments and the local drafts of a change.
    pub fn cached_comments(&self, change: ChangeNumber) -> Result<CommentsView, Error> {
        (self.comments_view())(&self.store(), change)
    }

    /// Fetches the published comments and the user's drafts, caches them,
    /// and returns the result. The server's draft list is merged into the
    /// local drafts without touching pending local work (see the rules in
    /// `store`). If only the draft request fails (other than with a 401),
    /// the comments are still cached and the drafts stay as they were.
    pub async fn refresh_comments(&self, change: ChangeNumber) -> Result<CommentsView, Error> {
        let _guard = self.drafts_lock.lock().await;
        let started = Timestamp::now();
        let (comments, drafts) = join(
            self.client.get_comments(change),
            self.client.get_drafts(change),
        )
        .await;
        let comments = comments?;
        let drafts = match drafts {
            Ok(d) => Some(d),
            Err(e) if e.is_unauthorized() => return Err(e.into()),
            Err(e) => {
                log::warn!("drafts of change {change} unavailable: {e}");
                None
            }
        };
        // A draft is stored with its patch set's commit. The server does not
        // always send it, then the change's patch set list tells; fetch
        // that first if it is not cached or too old to know the patch set.
        let patch_sets = |this: &Self| -> Result<Vec<PatchSetRef>, Error> {
            Ok(this
                .store()
                .load_change_detail(this.server, change)?
                .map(|c| c.detail.revisions.iter().map(|r| r.as_ref()).collect())
                .unwrap_or_default())
        };
        let mut known = patch_sets(self)?;
        let unplaced = |known: &[PatchSetRef]| {
            drafts
                .iter()
                .flatten()
                .any(|d| d.commit_id.is_none() && !known.iter().any(|p| p.number == d.patch_set))
        };
        if unplaced(&known) {
            match self.refresh_change(change).await {
                Ok(_) => known = patch_sets(self)?,
                Err(e) if e.is_unauthorized() => return Err(e),
                Err(e) => log::warn!("patch sets of change {change} unavailable: {e}"),
            }
        }
        let now = Timestamp::now();
        let mut store = self.store();
        store.save_comments(self.server, change, &comments, now)?;
        if let Some(drafts) = drafts
            && self.client.is_authenticated()
        {
            store.merge_server_drafts(self.server, change, &drafts, &known, started, now)?;
        }
        (self.comments_view())(&store, change)
    }

    // ---- drafts ----

    /// Writes a draft to the local database as pending, without any
    /// network. `None` when updating a draft that no longer exists (or is
    /// being deleted).
    pub fn save_draft_local(&self, save: DraftSave) -> Result<Option<Draft>, Error> {
        let now = Timestamp::now();
        let mut store = self.store();
        Ok(match save {
            DraftSave::Create {
                change,
                patch_set,
                input,
            } => Some(store.create_draft(self.server, change, &patch_set, &input, now)?),
            DraftSave::Update {
                id,
                message,
                unresolved,
            } => store.edit_draft(self.server, id, &message, unresolved, now)?,
        })
    }

    /// [`Self::save_draft_local`], then an upload attempt. Returns the
    /// draft in its state after the attempt: `Synced`, still pending
    /// (`last_error` says why; it is uploaded by a later retry), or
    /// `Failed`. Only a 401 (and store errors) are returned as errors.
    pub async fn save_draft(&self, save: DraftSave) -> Result<Option<Draft>, Error> {
        let Some(draft) = self.save_draft_local(save)? else {
            return Ok(None);
        };
        self.upload_draft(draft.local_id).await
    }

    /// Deletes a draft locally; one the server has stays hidden and pending
    /// until the deletion is confirmed. No network.
    pub fn delete_draft_local(&self, id: LocalDraftId) -> Result<DeleteOutcome, Error> {
        Ok(self.store().delete_draft(self.server, id)?)
    }

    /// [`Self::delete_draft_local`], then an attempt to delete it on the
    /// server.
    pub async fn delete_draft(&self, id: LocalDraftId) -> Result<DeleteOutcome, Error> {
        let outcome = self.delete_draft_local(id)?;
        if outcome == DeleteOutcome::Pending {
            self.upload_draft(id).await?;
        }
        Ok(outcome)
    }

    /// How many drafts are not on the server, over all changes: pending
    /// (retried by [`Self::retry_pending_drafts`]) and failed (only a
    /// manual retry or an edit moves them). No network.
    pub fn draft_counts(&self) -> Result<DraftCounts, Error> {
        Ok(self.store().draft_counts(self.server)?)
    }

    /// Tries to bring one draft up to date on the server. `Ok(None)` if the
    /// draft is gone (deleted). Does nothing for `Synced` and `Failed`
    /// drafts.
    pub async fn upload_draft(&self, id: LocalDraftId) -> Result<Option<Draft>, Error> {
        let _guard = self.drafts_lock.lock().await;
        self.upload_locked(id).await
    }

    /// Puts a `Failed` draft back to pending and tries again.
    pub async fn retry_failed_draft(&self, id: LocalDraftId) -> Result<Option<Draft>, Error> {
        if self.store().retry_failed_draft(self.server, id)?.is_none() {
            return Ok(None);
        }
        self.upload_draft(id).await
    }

    /// Uploads every pending draft of the server, oldest first. Stops at
    /// the first transient failure (everything after it would fail too) and
    /// at a 401, which is returned as an error.
    pub async fn retry_pending_drafts(&self) -> Result<RetryReport, Error> {
        let _guard = self.drafts_lock.lock().await;
        let pending = self.store().pending_drafts(self.server)?;
        let total = pending.len();
        let mut report = RetryReport::default();
        for (i, d) in pending.into_iter().enumerate() {
            match self.upload_locked(d.local_id).await? {
                None => report.deleted += 1,
                Some(d) => match d.sync {
                    DraftSync::Synced => report.synced += 1,
                    DraftSync::Failed { .. } => report.rejected += 1,
                    _ => {
                        report.still_pending = total - i;
                        break;
                    }
                },
            }
        }
        Ok(report)
    }

    /// Puts every failed draft that may work on a second try (a 500, an
    /// unreadable answer) back to pending, then uploads everything pending
    /// like [`Self::retry_pending_drafts`]. Drafts the server refused for
    /// good (403, 409, 400) stay failed: they have to be edited or deleted.
    pub async fn retry_failed_drafts(&self) -> Result<RetryReport, Error> {
        {
            let mut store = self.store();
            for d in store.failed_drafts(self.server)? {
                if matches!(
                    d.sync,
                    DraftSync::Failed {
                        retryable: true,
                        ..
                    }
                ) {
                    store.retry_failed_draft(self.server, d.local_id)?;
                }
            }
        }
        self.retry_pending_drafts().await
    }

    /// One upload attempt; the caller holds `drafts_lock`.
    async fn upload_locked(&self, id: LocalDraftId) -> Result<Option<Draft>, Error> {
        // An update whose draft is gone from the server is created anew,
        // once.
        for _ in 0..2 {
            let Some(d) = self.store().load_draft(self.server, id)? else {
                return Ok(None);
            };
            let sent = match (&d.sync, d.server_id.as_deref()) {
                (DraftSync::PendingCreate, _) | (DraftSync::PendingUpdate, None) => self
                    .client
                    .create_draft(d.change, &d.revision, &d.input())
                    .await
                    .map(|c| Step::Uploaded(c.id)),
                (DraftSync::PendingUpdate, Some(sid)) => self
                    .client
                    .update_draft(d.change, &d.revision, sid, &d.input())
                    .await
                    .map(|c| Step::Uploaded(c.id)),
                (DraftSync::PendingDelete, Some(sid)) => self
                    .client
                    .delete_draft(d.change, &d.revision, sid)
                    .await
                    .map(|()| Step::Deleted),
                (DraftSync::PendingDelete, None) => Ok(Step::Deleted),
                (DraftSync::Synced | DraftSync::Failed { .. }, _) => return Ok(Some(d)),
            };
            let now = Timestamp::now();
            let failure = match sent {
                Ok(Step::Uploaded(server_draft_id)) => {
                    let stored = self.store().record_draft_uploaded(
                        self.server,
                        id,
                        d.version,
                        &server_draft_id,
                        now,
                    )?;
                    if stored.is_none() {
                        // Deleted locally while the request was in flight:
                        // take the copy we just made back.
                        if let Err(e) = self
                            .client
                            .delete_draft(d.change, &d.revision, &server_draft_id)
                            .await
                        {
                            log::warn!("removing the orphaned draft {server_draft_id}: {e}");
                        }
                    }
                    return Ok(stored);
                }
                Ok(Step::Deleted) | Err(ApiError::NotFound)
                    if d.sync == DraftSync::PendingDelete =>
                {
                    self.store().record_draft_deleted(self.server, id)?;
                    return Ok(None);
                }
                Ok(Step::Deleted) => return Ok(Some(d)),
                Err(ApiError::NotFound)
                    if d.sync == DraftSync::PendingUpdate && d.server_id.is_some() =>
                {
                    self.store().restart_draft_as_create(self.server, id)?;
                    continue;
                }
                Err(e) if e.is_unauthorized() => return Err(e.into()),
                Err(e) => failure_for(&e),
            };
            return Ok(self.store().record_draft_failure(
                self.server,
                id,
                d.version,
                &failure,
                now,
            )?);
        }
        Ok(self.store().load_draft(self.server, id)?)
    }

    // ---- publishing ----

    /// Posts a review to `revision` (message, votes, drafts, attention set)
    /// and refreshes the change and its comments.
    ///
    /// Needs the server: see the module documentation. Unless
    /// `review.drafts` is `Keep`, the change's pending drafts are uploaded
    /// first and the call fails with [`Error::DraftsNotUploaded`] if any
    /// draft is still not on the server. Server-side problems (a vote that
    /// is not permitted, a closed change) are `Error::Api` with the
    /// server's message; the drafts stay as they were. After a successful
    /// post the drafts that were published are forgotten locally even if
    /// the refresh afterwards fails.
    pub async fn publish_review(
        &self,
        change: ChangeNumber,
        revision: &RevisionId,
        review: &ReviewInput,
    ) -> Result<PublishOutcome, Error> {
        let result = {
            let _guard = self.drafts_lock.lock().await;
            if review.drafts != DraftHandling::Keep {
                let uploadable: Vec<LocalDraftId> = self
                    .store()
                    .load_drafts(self.server, change)?
                    .iter()
                    .filter(|d| d.sync.is_uploadable())
                    .map(|d| d.local_id)
                    .collect();
                for id in uploadable {
                    self.upload_locked(id).await?;
                }
                let blocking: Vec<PendingDraft> = self
                    .store()
                    .load_drafts(self.server, change)?
                    .into_iter()
                    .filter(|d| d.sync.is_pending())
                    .map(|d| PendingDraft {
                        reason: match &d.sync {
                            DraftSync::Failed { error, .. } => error.clone(),
                            DraftSync::PendingDelete => d
                                .last_error
                                .clone()
                                .unwrap_or_else(|| "deletion not confirmed yet".into()),
                            _ => d
                                .last_error
                                .clone()
                                .unwrap_or_else(|| "waiting to be uploaded".into()),
                        },
                        local_id: d.local_id,
                        path: d.path,
                        patch_set: d.patch_set,
                    })
                    .collect();
                if !blocking.is_empty() {
                    return Err(Error::DraftsNotUploaded(blocking));
                }
            }
            let result = self.client.post_review(change, revision, review).await?;
            // The server turned its drafts into comments.
            let only = (review.drafts == DraftHandling::Publish).then_some(revision);
            let removed = self.store().remove_synced_drafts(self.server, change, only);
            if let Err(e) = removed {
                log::warn!("forgetting published drafts of change {change}: {e}");
            }
            result
        };
        let detail = match self.refresh_change(change).await {
            Ok(d) => Some(d),
            Err(e) => {
                log::warn!("refreshing change {change} after the review: {e}");
                None
            }
        };
        let comments = match self.refresh_comments(change).await {
            Ok(c) => Some(c),
            Err(e) => {
                log::warn!("refreshing comments of change {change} after the review: {e}");
                None
            }
        };
        Ok(PublishOutcome {
            unapplied_labels: review.unapplied_labels(&result),
            result,
            detail,
            comments,
        })
    }

    // ---- reviewed marks ----

    /// Marks a file reviewed (or not): updates the cached file lists of the
    /// patch set first, then asks the server, and puts the cache back if
    /// the request fails.
    pub async fn set_reviewed(
        &self,
        change: ChangeNumber,
        revision: &RevisionId,
        path: &str,
        reviewed: bool,
    ) -> Result<(), Error> {
        let changed =
            self.store()
                .set_reviewed_path(self.server, change, revision, path, reviewed)?;
        match self
            .client
            .set_reviewed(change, revision, path, reviewed)
            .await
        {
            Ok(()) => Ok(()),
            Err(e) => {
                if changed > 0
                    && let Err(revert) = self.store().set_reviewed_path(
                        self.server,
                        change,
                        revision,
                        path,
                        !reviewed,
                    )
                {
                    log::warn!("reverting the reviewed mark of {path}: {revert}");
                }
                Err(e.into())
            }
        }
    }
}

#[cfg(test)]
mod tests;
