//! The reply dialog (`a`): a message, label votes, the drafts that will be
//! published, and attention set changes, sent as one review.
//!
//! The dialog is a modal overlay owned by the shell (the pages ask for it
//! with an event), so the change page and the diff page share it. It takes
//! the keyboard: while it is open no page key fires, and the shell's own
//! single-letter keys are off too (`!ReplyDialog` in their context).
//!
//! # The message is a draft
//!
//! The dialog's text is the patch-set level draft on the current patch set
//! that is not a reply, going through the same [`DraftOutbox`] as every
//! other draft. So the change page's "Patchset comments" shows it, it
//! survives going offline or quitting, and no second place has to agree
//! with it. It is read when the dialog opens (and adopted if it arrives
//! while the text is still untouched). It is saved when the dialog closes,
//! and, locally, right before the review is sent; never while typing, which
//! keeps one writer and no race with the upload. A cleared text deletes
//! the draft; an empty draft is never created. `ReviewInput::message` stays
//! empty: `PUBLISH_ALL_REVISIONS` turns the draft into the change message.
//!
//! The "Resolved" box is that draft's `unresolved` flag, inverted. It starts
//! ticked, so the message does not become a to-do for the owner unless the
//! sender unticks it.
//!
//! # Sending
//!
//! Sending runs on the background executor: the text is written to the
//! store, `Syncer::publish_review` uploads what is pending and posts
//! `POST .../review` to the current patch set with only the votes that
//! changed, and the attention set operations that differ from what the
//! server has. The controls are off while it runs and Esc does not cancel
//! it. The review needs a connection, and there is no queue: when it cannot
//! be sent the dialog says so and keeps everything; the text is a draft in
//! the store anyway.
//!
//! # Keys
//!
//! `Ctrl-Enter` (`Cmd-Enter` on macOS) sends from anywhere in the dialog,
//! `Esc` closes it, `Tab` / `Shift-Tab` move between the controls (the
//! dialog does the cycling itself, so focus never leaves it, and so the
//! message field gives Tab up instead of indenting), `Space` / `Enter`
//! press the focused control, `Left` / `Right` pick the previous / next
//! value of the focused vote. Tab stops once per vote row, on the value the
//! review would leave (the user's current vote, or 0).

pub mod actions;
pub(crate) mod plan;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use model::{
    ChangeDetail, ChangeNumber, DraftInput, Location, PATCHSET_LEVEL_PATH, PatchSetNumber,
    PatchSetRef, Side, Thread,
};
use sync::{DraftSave, PublishOutcome};

use crate::{
    comments::ChangeComments,
    outbox::DraftOutbox,
    servers::ServerBadge,
    session::{Session, is_unauthorized},
};
use actions::*;
use plan::{DraftItem, Person, VoteRow, vote_text};

/// What the dialog tells the shell.
pub enum ReplyEvent {
    /// The user closed it (their text is saved as a draft).
    Close,
    /// The server answered 401: back to the login flow.
    Unauthorized,
    /// The review was posted.
    Sent(Box<Sent>),
}

/// The outcome of a posted review.
pub struct Sent {
    /// The change as the server has it now, if refreshing worked.
    pub detail: Option<ChangeDetail>,
    /// Votes the server ignored (it answers 200 for those).
    pub unapplied: Vec<(String, i32)>,
    /// A part of the request that failed although the review was posted.
    pub warning: Option<String>,
    /// The votes that were cast and took effect; empty for a reply that
    /// changed none.
    pub voted: Vec<(String, i32)>,
}

impl Sent {
    /// The note for the page and whether it is a warning.
    pub fn note(&self) -> (String, bool) {
        if !self.unapplied.is_empty() {
            let votes: Vec<_> = self
                .unapplied
                .iter()
                .map(|(label, value)| format!("{label} {}", vote_text(*value)))
                .collect();
            return (
                format!(
                    "Reply sent, but the server did not apply your vote: {}",
                    votes.join(", ")
                ),
                true,
            );
        }
        match &self.warning {
            Some(w) => (format!("Reply sent, but: {w}"), true),
            None => ("Reply sent".to_owned(), false),
        }
    }
}

/// Why the reply dialog cannot open now, as a note for the user.
pub fn unavailable(session: &Session) -> Option<&'static str> {
    if !session.can_comment() {
        Some("Sign in to reply")
    } else if !session.can_upload() {
        Some("Still signing in, try again in a moment")
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Editing,
    Sending,
}

pub struct ReplyDialog {
    session: Session,
    outbox: Entity<DraftOutbox>,
    comments: Entity<ChangeComments>,
    detail: ChangeDetail,
    number: ChangeNumber,
    /// The signed-in user's login name, to find their vote and account.
    me: Option<String>,
    /// The patch set the review goes to: the current one.
    current: Option<PatchSetRef>,
    input: Entity<TextareaState>,
    /// The text last put into the field by the dialog itself; the text is
    /// untouched while the field still holds it.
    baseline: String,
    resolved: bool,
    threads: Arc<Vec<Thread>>,
    votes: Vec<VoteRow>,
    people: Vec<Person>,
    phase: Phase,
    error: Option<String>,
    send_task: Option<Task<()>>,
    scroll: ScrollHandle,
    focus: FocusHandle,
    input_focus: FocusHandle,
    resolved_focus: FocusHandle,
    vote_focus: Vec<Vec<FocusHandle>>,
    people_focus: Vec<FocusHandle>,
    upload_focus: FocusHandle,
    cancel_focus: FocusHandle,
    send_focus: FocusHandle,
    _subs: Vec<Subscription>,
    /// The server this writes to, named when there are several.
    server_badge: Option<ServerBadge>,
}

impl EventEmitter<ReplyEvent> for ReplyDialog {}

impl Focusable for ReplyDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.input_focus.clone()
    }
}

impl ReplyDialog {
    /// Names the server the review goes to in the dialog (several servers only).
    pub fn set_server_badge(&mut self, badge: Option<ServerBadge>, cx: &mut Context<Self>) {
        if self.server_badge != badge {
            self.server_badge = badge;
            cx.notify();
        }
    }

    #[cfg(test)]
    pub(crate) fn server_badge_for_test(&self) -> Option<&ServerBadge> {
        self.server_badge.as_ref()
    }

    /// Opens the dialog for `detail` with the focus in the message field.
    pub fn new(
        session: Session,
        outbox: Entity<DraftOutbox>,
        detail: ChangeDetail,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let number = detail.summary.number;
        let comments = outbox.update(cx, |o, cx| o.comments(number, cx));
        let me = session.username.clone();
        let current = detail.current().map(|r| r.as_ref());
        let threads = comments.read(cx).threads();
        let votes = plan::vote_rows(&detail, me.as_deref());
        let people = plan::people(&detail, me.as_deref(), &threads);

        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 10)
                .placeholder("Write a message to the reviewers")
        });
        let input_focus = input.read(cx).focus_handle(cx);
        let draft = current
            .as_ref()
            .and_then(|c| plan::message_draft(&threads, c.number));
        let (baseline, resolved) = match draft {
            Some(d) => (d.message.clone(), !d.unresolved),
            None => (String::new(), true),
        };
        put_text(&input, &baseline, window, cx);

        let handles = |n: usize, cx: &mut Context<Self>| -> Vec<FocusHandle> {
            (0..n).map(|_| cx.focus_handle()).collect()
        };
        let vote_focus: Vec<Vec<FocusHandle>> =
            votes.iter().map(|r| handles(r.choices.len(), cx)).collect();
        let people_focus = handles(people.len(), cx);

        let subs = vec![
            cx.subscribe_in(
                &input,
                window,
                |this, _, ev: &InputEvent, window, cx| match ev {
                    // Ctrl-Enter: the field also inserts a newline; the text
                    // is trimmed when it is used.
                    InputEvent::PressEnter {
                        secondary: true, ..
                    } => {
                        // Take back the newline the field just inserted.
                        let value = this.input.read(cx).value().to_string();
                        if let Some(kept) = value.strip_suffix('\n') {
                            let kept = kept.to_owned();
                            this.input
                                .update(cx, |i, cx| i.set_value(kept.clone(), window, cx));
                            put_text(&this.input, &kept, window, cx);
                        }
                        this.send(window, cx)
                    }
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            ),
            cx.observe_in(&comments, window, |this, comments, window, cx| {
                this.threads = comments.read(cx).threads();
                this.adopt_draft(window, cx);
                cx.notify();
            }),
        ];

        let this = Self {
            session,
            outbox,
            comments,
            detail,
            number,
            me,
            current,
            input,
            baseline,
            resolved,
            threads,
            votes,
            people,
            phase: Phase::Editing,
            error: None,
            send_task: None,
            scroll: ScrollHandle::new(),
            focus: cx.focus_handle(),
            input_focus,
            resolved_focus: cx.focus_handle(),
            vote_focus,
            people_focus,
            upload_focus: cx.focus_handle(),
            cancel_focus: cx.focus_handle(),
            send_focus: cx.focus_handle(),
            _subs: subs,
            server_badge: None,
        };
        window.focus(&this.input_focus, cx);
        this
    }

    // ---- the message draft ----

    /// The text in the field, trimmed.
    fn text(&self, cx: &App) -> String {
        self.input.read(cx).value().trim().to_owned()
    }

    /// Whether the field still holds what the dialog put there.
    fn untouched(&self, cx: &App) -> bool {
        self.input.read(cx).value().as_ref() == self.baseline
    }

    /// The message draft arrived after the dialog opened (a save that was
    /// still running when it opened): take its text if the user has not
    /// started writing.
    fn adopt_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(current) = &self.current else {
            return;
        };
        let Some(d) = plan::message_draft(&self.threads, current.number) else {
            return;
        };
        if d.message == self.baseline || !self.untouched(cx) || self.phase == Phase::Sending {
            return;
        }
        let (text, resolved) = (d.message.clone(), !d.unresolved);
        put_text(&self.input, &text, window, cx);
        self.baseline = text;
        self.resolved = resolved;
    }

    /// The draft the dialog's text belongs to, if there is one.
    fn message_draft(&self) -> Option<&model::Draft> {
        let current = self.current.as_ref()?;
        plan::message_draft(&self.threads, current.number)
    }

    /// Keeps the text as a draft: a new one, a changed one, or none
    /// (deleted when the text was cleared). The outbox does it, so the
    /// draft is stored first and uploaded afterwards, and it carries on
    /// after the dialog is gone.
    fn save_message(&mut self, cx: &mut Context<Self>) {
        let Some(patch_set) = self.current.clone() else {
            return;
        };
        let (text, unresolved, change) = (self.text(cx), !self.resolved, self.number);
        self.outbox.update(cx, |o, cx| {
            o.set_message(change, patch_set, text, unresolved, cx)
        });
    }

    /// Keeps the text as a draft. The shell calls it when it takes the
    /// dialog away without a send (a click on the title bar, say).
    pub fn keep_text(&mut self, cx: &mut Context<Self>) {
        if self.phase == Phase::Editing {
            self.save_message(cx);
        }
    }

    // ---- state of the dialog ----

    fn sending(&self) -> bool {
        self.phase == Phase::Sending
    }

    fn drafts(&self) -> Vec<DraftItem> {
        let current = self
            .current
            .as_ref()
            .map_or(PatchSetNumber(0), |c| c.number);
        let message = self.message_draft().map(|d| d.local_id);
        plan::draft_items(&self.threads, current, message)
    }

    /// Drafts the server does not have as they are; sending waits for them.
    fn unuploaded(&self) -> usize {
        self.drafts().iter().filter(|d| d.pending()).count()
    }

    fn review(&self) -> model::ReviewInput {
        let who = self
            .people
            .iter()
            .find(|p| p.me)
            .map(|p| p.account.display())
            .or_else(|| self.me.clone())
            .unwrap_or_else(|| "A reviewer".to_owned());
        let (add, remove) = plan::attention_changes(&self.people, &who);
        plan::review_input(&self.votes, add, remove)
    }

    /// Whether the review would carry anything.
    fn has_content(&self, cx: &App) -> bool {
        let review = self.review();
        !self.text(cx).is_empty()
            || !review.labels.is_empty()
            || !review.add_to_attention_set.is_empty()
            || !review.remove_from_attention_set.is_empty()
            || !self.drafts().is_empty()
    }

    fn can_send(&self, cx: &App) -> bool {
        !self.sending() && self.unuploaded() == 0 && self.current.is_some() && self.has_content(cx)
    }

    // ---- actions ----

    fn close_action(&mut self, _: &CloseReply, _: &mut Window, cx: &mut Context<Self>) {
        self.close(cx);
    }

    /// Closes the dialog and keeps the text as a draft. Not while sending:
    /// the request is under way and cannot be taken back.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if self.sending() {
            return;
        }
        self.save_message(cx);
        cx.emit(ReplyEvent::Close);
    }

    fn send_action(&mut self, _: &SendReply, window: &mut Window, cx: &mut Context<Self>) {
        self.send(window, cx);
    }

    /// The controls in tab order.
    fn tab_order(&self, cx: &App) -> Vec<FocusHandle> {
        if self.sending() {
            return Vec::new();
        }
        let mut v = vec![self.input_focus.clone(), self.resolved_focus.clone()];
        // One stop per vote row, on the value the review would leave;
        // Left / Right move from there.
        v.extend(
            self.votes
                .iter()
                .zip(&self.vote_focus)
                .filter_map(|(row, handles)| handles.get(row.focus_index()).cloned()),
        );
        if self.unuploaded() > 0 {
            v.push(self.upload_focus.clone());
        }
        v.extend(self.people_focus.iter().cloned());
        v.push(self.cancel_focus.clone());
        if self.can_send(cx) {
            v.push(self.send_focus.clone());
        }
        v
    }

    fn move_focus(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let order = self.tab_order(cx);
        if order.is_empty() {
            return;
        }
        let at = order.iter().position(|h| h.is_focused(window));
        let len = order.len() as isize;
        let next = match at {
            Some(i) => (i as isize + delta).rem_euclid(len) as usize,
            None if delta > 0 => 0,
            None => order.len() - 1,
        };
        window.focus(&order[next], cx);
    }

    fn focus_next(&mut self, _: &ReplyNext, window: &mut Window, cx: &mut Context<Self>) {
        self.move_focus(1, window, cx);
    }

    fn focus_prev(&mut self, _: &ReplyPrev, window: &mut Window, cx: &mut Context<Self>) {
        self.move_focus(-1, window, cx);
    }

    /// Left / right on a vote: the previous / next value, chosen.
    fn step_vote(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let at = self.vote_focus.iter().enumerate().find_map(|(r, row)| {
            row.iter()
                .position(|h| h.is_focused(window))
                .map(|i| (r, i))
        });
        let Some((row, ix)) = at else {
            return;
        };
        let last = self.votes[row].choices.len() as isize - 1;
        let to = (ix as isize + delta).clamp(0, last) as usize;
        self.select_vote(row, to, cx);
        window.focus(&self.vote_focus[row][to], cx);
    }

    fn vote_left(&mut self, _: &ReplyVoteLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.step_vote(-1, window, cx);
    }

    fn vote_right(&mut self, _: &ReplyVoteRight, window: &mut Window, cx: &mut Context<Self>) {
        self.step_vote(1, window, cx);
    }

    fn select_vote(&mut self, row: usize, choice: usize, cx: &mut Context<Self>) {
        if self.sending() {
            return;
        }
        if let Some(value) = self
            .votes
            .get(row)
            .and_then(|r| r.choices.get(choice))
            .map(|c| c.value)
        {
            self.votes[row].selected = value;
            self.error = None;
            cx.notify();
        }
    }

    fn set_person(&mut self, ix: usize, want: bool, cx: &mut Context<Self>) {
        if self.sending() {
            return;
        }
        if let Some(p) = self.people.get_mut(ix) {
            p.want = want;
            cx.notify();
        }
    }

    /// Uploads the drafts that are not on the server (the title bar's
    /// retry); the list follows as they arrive.
    fn upload_drafts(&mut self, cx: &mut Context<Self>) {
        self.outbox.update(cx, |o, cx| o.retry_everything(cx));
    }

    // ---- sending ----

    /// Posts the review.
    pub fn send(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.sending() {
            return;
        }
        let Some(revision) = self.current.as_ref().map(|c| c.sha.clone()) else {
            return;
        };
        let unuploaded = self.unuploaded();
        if unuploaded > 0 {
            self.error = Some(format!(
                "{unuploaded} draft comment{} not uploaded yet. Upload {} first.",
                if unuploaded == 1 { " is" } else { "s are" },
                if unuploaded == 1 { "it" } else { "them" },
            ));
            cx.notify();
            return;
        }
        if !self.has_content(cx) {
            self.error = Some("There is nothing to send yet.".into());
            cx.notify();
            return;
        }
        let (text, unresolved) = (self.text(cx), !self.resolved);
        let patch_set = self.current.clone().unwrap_or(PatchSetRef {
            number: PatchSetNumber(0),
            sha: revision.clone(),
        });
        let review = self.review();
        let syncer = self.session.syncer().clone();
        let change = self.number;
        self.phase = Phase::Sending;
        self.error = None;
        self.input.update(cx, |i, cx| i.set_disabled(true, cx));
        cx.notify();
        self.send_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    // The text is stored before anything goes out, so that a
                    // failure (offline, refused) leaves it as a draft.
                    match message_step(&syncer, change, &patch_set, &text, unresolved)? {
                        Some(MessageStep::Save(save)) => {
                            syncer.save_draft_local(save)?;
                        }
                        Some(MessageStep::Delete(id)) => {
                            syncer.delete_draft_local(id)?;
                        }
                        None => {}
                    }
                    syncer.publish_review(change, &revision, &review).await
                })
                .await;
            this.update(cx, |this, cx| this.sent(result, cx)).ok();
        }));
    }

    fn sent(&mut self, result: Result<PublishOutcome, sync::Error>, cx: &mut Context<Self>) {
        self.phase = Phase::Editing;
        self.input.update(cx, |i, cx| i.set_disabled(false, cx));
        let change = self.number;
        match result {
            Ok(outcome) => {
                // The drafts became comments and the votes and the attention
                // set moved: tell every place that shows them, once.
                match outcome.comments {
                    Some(view) => self.comments.update(cx, |c, cx| c.apply_view(view, cx)),
                    None => self.comments.update(cx, |c, cx| c.refresh(cx)),
                }
                self.outbox.update(cx, |o, cx| o.published(change, cx));
                let voted = plan::applied_votes(
                    &plan::labels_to_send(&self.votes),
                    &outcome.unapplied_labels,
                );
                cx.emit(ReplyEvent::Sent(Box::new(Sent {
                    voted,
                    detail: outcome.detail,
                    unapplied: outcome.unapplied_labels,
                    warning: outcome.result.error,
                })));
            }
            Err(e) if is_unauthorized(&e) => cx.emit(ReplyEvent::Unauthorized),
            Err(e) => {
                // The send wrote the message draft and may have uploaded
                // some: show the drafts as they are now.
                self.outbox.update(cx, |o, cx| o.published(change, cx));
                let listed: Vec<_> = self.drafts().iter().map(|d| d.id).collect();
                self.error = Some(describe(&e, &listed));
            }
        }
        cx.notify();
    }

    // ---- test access ----

    #[cfg(test)]
    pub(crate) fn error_for_test(&self) -> Option<&str> {
        self.error.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn sending_for_test(&self) -> bool {
        self.sending()
    }

    #[cfg(test)]
    pub(crate) fn text_for_test(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    #[cfg(test)]
    pub(crate) fn set_text_for_test(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.input
            .update(cx, |i, cx| i.set_value(text.to_owned(), window, cx));
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn resolved_for_test(&self) -> bool {
        self.resolved
    }

    #[cfg(test)]
    pub(crate) fn votes_for_test(&self) -> &[VoteRow] {
        &self.votes
    }

    #[cfg(test)]
    pub(crate) fn people_for_test(&self) -> &[Person] {
        &self.people
    }

    #[cfg(test)]
    pub(crate) fn drafts_for_test(&self) -> Vec<DraftItem> {
        self.drafts()
    }

    #[cfg(test)]
    pub(crate) fn can_send_for_test(&self, cx: &App) -> bool {
        self.can_send(cx)
    }

    #[cfg(test)]
    pub(crate) fn input_focus_for_test(&self) -> FocusHandle {
        self.input_focus.clone()
    }

    #[cfg(test)]
    pub(crate) fn vote_focus_for_test(&self, row: usize, choice: usize) -> FocusHandle {
        self.vote_focus[row][choice].clone()
    }

    #[cfg(test)]
    pub(crate) fn person_focus_for_test(&self, ix: usize) -> FocusHandle {
        self.people_focus[ix].clone()
    }

    #[cfg(test)]
    pub(crate) fn send_focus_for_test(&self) -> FocusHandle {
        self.send_focus.clone()
    }

    #[cfg(test)]
    pub(crate) fn cancel_focus_for_test(&self) -> FocusHandle {
        self.cancel_focus.clone()
    }

    #[cfg(test)]
    pub(crate) fn upload_focus_for_test(&self) -> FocusHandle {
        self.upload_focus.clone()
    }
}

/// What it takes to make the stored message draft hold `text`.
pub(crate) enum MessageStep {
    Save(DraftSave),
    Delete(model::LocalDraftId),
}

/// Looks the message draft up in the store (not in what a view happens to
/// hold, which may be a moment behind) and says what to do so that it holds
/// `text`: update it, delete it when the text is empty, create it when
/// there is none; nothing when it is as wanted.
pub(crate) fn message_step(
    syncer: &sync::Syncer,
    change: ChangeNumber,
    patch_set: &PatchSetRef,
    text: &str,
    unresolved: bool,
) -> Result<Option<MessageStep>, sync::Error> {
    let threads = syncer.cached_comments(change)?.threads();
    let existing = plan::message_draft(&threads, patch_set.number);
    Ok(match (existing, text.is_empty()) {
        (Some(d), true) => Some(MessageStep::Delete(d.local_id)),
        (Some(d), false) => (d.message != text || d.unresolved != unresolved).then(|| {
            MessageStep::Save(DraftSave::Update {
                id: d.local_id,
                message: text.to_owned(),
                unresolved,
            })
        }),
        (None, false) => Some(MessageStep::Save(DraftSave::Create {
            change,
            patch_set: patch_set.clone(),
            input: message_input(text.to_owned(), unresolved),
        })),
        (None, true) => None,
    })
}

/// The draft input of the dialog's message: patch-set level, no reply.
fn message_input(message: String, unresolved: bool) -> DraftInput {
    DraftInput {
        path: PATCHSET_LEVEL_PATH.to_owned(),
        // The server takes no side for a patch-set level comment, and the
        // upload leaves it out (see `draft_input` in gerrit-api).
        side: Side::Revision,
        parent: None,
        location: Location::File,
        in_reply_to: None,
        message,
        unresolved,
    }
}

/// Puts `text` into the field with the cursor at its end.
pub(crate) fn put_text(
    input: &Entity<TextareaState>,
    text: &str,
    window: &mut Window,
    cx: &mut App,
) {
    if text.is_empty() {
        return;
    }
    input.update(cx, |i, cx| {
        i.set_value(text.to_owned(), window, cx);
        let line = text.matches('\n').count();
        let column = text
            .rsplit('\n')
            .next()
            .unwrap_or("")
            .encode_utf16()
            .count();
        i.set_cursor_position(
            gpui_kit::component::input::Position::new(line as u32, column as u32),
            window,
            cx,
        );
    });
}

/// A failed send, in words for the dialog. `listed` are the drafts the
/// dialog shows in its list (with the "Upload drafts" button); the
/// message's own draft is not among them.
fn describe(e: &sync::Error, listed: &[model::LocalDraftId]) -> String {
    use gerrit_api::Error as Api;
    match e {
        sync::Error::DraftsNotUploaded(list)
            if list.iter().all(|d| !listed.contains(&d.local_id)) =>
        {
            // Only the message is missing on the server. There is no list
            // and no button for it, and Send uploads it again.
            let why = list.first().map_or("", |d| d.reason.as_str());
            format!(
                "Not sent: the message could not be uploaded ({why}). Nothing was posted; \
                 your text is kept as a draft. Send again, or wait until you are online."
            )
        }
        sync::Error::DraftsNotUploaded(list) => {
            let first = list.first().map_or(String::new(), |d| {
                format!(" ({} on patch set {}: {})", d.path, d.patch_set, d.reason)
            });
            format!(
                "Not sent: {} draft comment{} could not be uploaded{first}. \
                 Nothing was posted; try Upload drafts, or send when you are online.",
                list.len(),
                if list.len() == 1 { "" } else { "s" },
            )
        }
        sync::Error::Api(api) => match api {
            Api::Forbidden { message } => refused("not allowed (403)", message),
            Api::Conflict { message } => refused("in conflict (409)", message),
            Api::BadRequest { message } => refused("rejected (400)", message),
            api if api.is_retryable() => format!(
                "Not sent: the server cannot be reached ({api}). You may be offline. \
                 Your text is kept as a draft; send again when the connection is back."
            ),
            api => format!("Not sent: {api}"),
        },
        other => format!("Not sent: {other}"),
    }
}

fn refused(what: &str, message: &str) -> String {
    if message.trim().is_empty() {
        format!("Not sent: the server says it is {what}.")
    } else {
        format!("Not sent: {what}: {}", message.trim())
    }
}

// ---- rendering ----

/// How a [`chip`] looks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Look {
    Plain,
    /// The chosen one of a group.
    Selected,
    /// The main button.
    Primary,
}

/// A small button that takes the keyboard focus (`handle`) and is pressed
/// with Enter or Space as well as a click. The caller adds `on_click`
/// (which also runs for the keyboard) and guards what it does itself;
/// `dimmed` only changes the look. These are not the component library's
/// buttons: those keep their focus handle to themselves, and the dialog has
/// to know which control has the focus to cycle through them.
pub(crate) fn chip(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    handle: &FocusHandle,
    look: Look,
    dimmed: bool,
    window: &Window,
    cx: &App,
) -> Stateful<Div> {
    let theme = cx.theme();
    let focused = handle.is_focused(window);
    let (bg, fg, border) = match look {
        Look::Plain => (theme.transparent, theme.foreground, theme.border),
        Look::Selected | Look::Primary => (theme.primary, theme.primary_foreground, theme.primary),
    };
    let hover = theme.list_hover;
    div()
        .id(id)
        .track_focus(handle)
        .flex_none()
        .px_2()
        .py_0p5()
        .rounded_md()
        .border_2()
        .border_color(if focused { theme.ring } else { border })
        .bg(bg)
        .text_color(fg)
        .text_sm()
        .when(dimmed, |d| d.opacity(0.5))
        .when(!dimmed && look == Look::Plain, |d| {
            d.hover(move |d| d.bg(hover))
        })
        .when(!dimmed, |d| d.cursor_pointer())
        .child(label.into())
}

/// A tick box with a label, focusable like a [`chip`].
pub(crate) fn check(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    checked: bool,
    handle: &FocusHandle,
    dimmed: bool,
    window: &Window,
    cx: &App,
) -> Stateful<Div> {
    let theme = cx.theme();
    let focused = handle.is_focused(window);
    h_flex()
        .id(id)
        .track_focus(handle)
        .gap_2()
        .items_center()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(if focused {
            theme.ring
        } else {
            theme.transparent
        })
        .when(dimmed, |d| d.opacity(0.5))
        .when(!dimmed, |d| d.cursor_pointer())
        .child(
            div()
                .size_4()
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .border_1()
                .border_color(if checked { theme.primary } else { theme.border })
                .bg(if checked {
                    theme.primary
                } else {
                    theme.transparent
                })
                .text_color(theme.primary_foreground)
                .text_xs()
                .child(if checked { "\u{2713}" } else { "" }),
        )
        .child(label.into())
}

/// A tooltip for a vote value.
struct Tip(SharedString);

impl Render for Tip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .text_sm()
            .child(self.0.clone())
    }
}

impl Render for ReplyDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let sending = self.sending();
        let subject = self.detail.summary.subject.clone();
        let send_hint = if cfg!(target_os = "macos") {
            "Cmd-Enter sends, Esc closes"
        } else {
            "Ctrl-Enter sends, Esc closes"
        };
        let can_send = self.can_send(cx);
        let section = |title: &'static str| {
            div()
                .pt_3()
                .text_xs()
                .font_semibold()
                .text_color(muted)
                .child(title)
        };
        let drafts = self.drafts();
        let unuploaded = drafts.iter().filter(|d| d.pending()).count();

        let message = v_flex()
            .gap_1()
            .child(Textarea::new(&self.input).small())
            .child(
                check(
                    "reply-resolved",
                    "Resolved",
                    self.resolved,
                    &self.resolved_focus,
                    sending,
                    window,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if !this.sending() {
                        this.resolved = !this.resolved;
                        cx.notify();
                    }
                })),
            );

        let votes = (!self.votes.is_empty()).then(|| {
            let rows: Vec<_> = self
                .votes
                .iter()
                .enumerate()
                .map(|(r, row)| self.vote_row(r, row, window, cx))
                .collect();
            v_flex()
                .gap_1()
                .child(section("VOTES"))
                .children(rows)
                .into_any_element()
        });

        let draft_list = (!drafts.is_empty()).then(|| {
            let rows: Vec<_> = drafts.iter().map(|d| draft_row(d, cx)).collect();
            v_flex()
                .gap_0p5()
                .child(section("DRAFTS TO PUBLISH"))
                .children(rows)
                .when(unuploaded > 0, |d| {
                    d.child(
                        h_flex()
                            .gap_2()
                            .pt_1()
                            .items_center()
                            .child(div().text_xs().text_color(theme.danger).child(format!(
                                "{unuploaded} not uploaded: Send waits until they are."
                            )))
                            .child(
                                chip(
                                    "reply-upload",
                                    "Upload drafts",
                                    &self.upload_focus,
                                    Look::Plain,
                                    sending,
                                    window,
                                    cx,
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.upload_drafts(cx))),
                            ),
                    )
                })
                .into_any_element()
        });

        let people = (!self.people.is_empty()).then(|| {
            let rows: Vec<_> = self
                .people
                .iter()
                .enumerate()
                .map(|(i, p)| self.person_row(i, p, window, cx))
                .collect();
            v_flex()
                .gap_0p5()
                .child(section("ATTENTION SET"))
                .children(rows)
                .into_any_element()
        });

        let footer = h_flex()
            .gap_2()
            .pt_3()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .text_color(muted)
                    .child(if sending {
                        "Sending\u{2026}"
                    } else {
                        send_hint
                    }),
            )
            .child(
                chip(
                    "reply-cancel",
                    "Cancel",
                    &self.cancel_focus,
                    Look::Plain,
                    sending,
                    window,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
            )
            .child(
                chip(
                    "reply-send",
                    if sending { "Sending\u{2026}" } else { "Send" },
                    &self.send_focus,
                    Look::Primary,
                    !can_send,
                    window,
                    cx,
                )
                .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
            );

        div()
            .id("reply-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(crate::theme::modal_backdrop(cx))
            .child(
                v_flex()
                    .id("reply-dialog")
                    .key_context(REPLY_DIALOG)
                    .track_focus(&self.focus)
                    .on_action(cx.listener(Self::close_action))
                    .on_action(cx.listener(Self::send_action))
                    .on_action(cx.listener(Self::focus_next))
                    .on_action(cx.listener(Self::focus_prev))
                    .w(px(660.))
                    .max_w_full()
                    .max_h_full()
                    .my_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .text_color(theme.popover_foreground)
                    .text_sm()
                    .child(
                        v_flex()
                            .px_4()
                            .pt_3()
                            .pb_1()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(div().text_base().font_semibold().child("Reply"))
                                    .children(self.server_badge.as_ref().map(|b| {
                                        h_flex()
                                            .gap_1()
                                            .items_center()
                                            .text_xs()
                                            .text_color(muted)
                                            .child("sent to")
                                            .child(b.pill(cx))
                                    })),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .truncate()
                                    .child(format!("#{} {subject}", self.number)),
                            ),
                    )
                    .child(
                        v_flex()
                            .relative()
                            .min_h_0()
                            .child(
                                div()
                                    .id("reply-scroll")
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .track_scroll(&self.scroll)
                                    .px_4()
                                    .pb_2()
                                    .child(
                                        v_flex()
                                            .gap_1()
                                            .child(message)
                                            .children(votes)
                                            .children(draft_list)
                                            .children(people),
                                    ),
                            )
                            .vertical_scrollbar(&self.scroll),
                    )
                    .child(
                        v_flex()
                            .px_4()
                            .pb_3()
                            .when_some(self.error.clone(), |d, e| {
                                d.child(div().pt_2().text_xs().text_color(theme.danger).child(e))
                            })
                            .child(footer),
                    ),
            )
    }
}

impl ReplyDialog {
    fn vote_row(
        &self,
        r: usize,
        row: &VoteRow,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let sending = self.sending();
        let chips: Vec<_> = row
            .choices
            .iter()
            .enumerate()
            .map(|(i, choice)| {
                let text = if choice.description.is_empty() {
                    format!("{} {}", row.label, vote_text(choice.value))
                } else {
                    format!(
                        "{} {}: {}",
                        row.label,
                        vote_text(choice.value),
                        choice.description
                    )
                };
                div()
                    .id(("reply-vote-tip", r * 100 + i))
                    .tooltip(move |_, cx| {
                        let text = SharedString::from(text.clone());
                        cx.new(|_| Tip(text)).into()
                    })
                    .child(
                        chip(
                            ("reply-vote", r * 100 + i),
                            vote_text(choice.value),
                            &self.vote_focus[r][i],
                            if row.selected == choice.value {
                                Look::Selected
                            } else {
                                Look::Plain
                            },
                            sending,
                            window,
                            cx,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.select_vote(r, i, cx))),
                    )
            })
            .collect();
        let meaning = row
            .choices
            .iter()
            .find(|c| c.value == row.selected)
            .map(|c| c.description.clone())
            .filter(|d| !d.is_empty());
        let note = if row.changed() {
            format!("was {}", vote_text(row.current))
        } else if row.current != 0 {
            "your vote".to_owned()
        } else {
            String::new()
        };
        v_flex()
            .gap_0p5()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .flex_wrap()
                    .child(div().w(px(110.)).flex_none().child(row.label.clone()))
                    .child(
                        h_flex()
                            .id(("reply-vote-row", r))
                            .key_context(REPLY_VOTE)
                            .on_action(cx.listener(Self::vote_left))
                            .on_action(cx.listener(Self::vote_right))
                            .gap_1()
                            .children(chips),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(note),
                    ),
            )
            .children(meaning.map(|m| {
                div()
                    .pl(px(118.))
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(m)
            }))
            .into_any_element()
    }

    fn person_row(
        &self,
        i: usize,
        p: &Person,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let roles = p.roles();
        let status = match (p.in_set, p.want) {
            (true, true) => "stays".to_owned(),
            (true, false) => "will be removed".to_owned(),
            (false, true) => "will be added".to_owned(),
            (false, false) => String::new(),
        };
        h_flex()
            .gap_2()
            .items_center()
            .child(
                check(
                    ("reply-person", i),
                    format!("{} ({roles})", p.account.display()),
                    p.want,
                    &self.people_focus[i],
                    self.sending(),
                    window,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    let want = this.people.get(i).is_some_and(|p| !p.want);
                    this.set_person(i, want, cx)
                })),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_xs()
                    .text_color(if p.want != p.in_set {
                        theme.primary
                    } else {
                        theme.muted_foreground
                    })
                    .child(match &p.reason {
                        Some(r) if p.in_set => format!("{status} \u{b7} {r}"),
                        _ => status,
                    }),
            )
            .into_any_element()
    }
}

fn draft_row(d: &DraftItem, cx: &mut Context<ReplyDialog>) -> AnyElement {
    let theme = cx.theme().clone();
    h_flex()
        .gap_2()
        .items_baseline()
        .child(
            div()
                .flex_none()
                .max_w(px(220.))
                .truncate()
                .font_family(theme.mono_font_family.clone())
                .child(d.place.clone()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(theme.muted_foreground)
                .child(d.text.clone()),
        )
        .when(d.other_patch_set, |r| {
            r.child(
                div()
                    .flex_none()
                    .px_1()
                    .rounded_sm()
                    .text_xs()
                    .text_color(theme.warning)
                    .bg(theme.warning.opacity(0.15))
                    .child(format!("PS {}", d.patch_set)),
            )
        })
        .children(d.problem().map(|p| {
            div()
                .flex_none()
                .max_w(px(220.))
                .truncate()
                .text_xs()
                .text_color(if d.failed() {
                    theme.danger
                } else {
                    theme.muted_foreground
                })
                .child(p)
        }))
        .into_any_element()
}
