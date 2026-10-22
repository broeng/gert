//! The writes of the reviewer, topic and hashtag steps: one `Edit` per
//! request, run on the background executor like the menu's actions.
//!
//! A refused edit (403, 409, 400) keeps the step open with the server's
//! message, 401 goes to the login flow and a transport error says nothing
//! was changed, the same as for the actions of the menu. Adding or removing
//! a reviewer keeps the step open (several people in a row); a topic or
//! hashtag edit closes the dialog.

use gpui_kit::*;
use model::{AddReviewerResult, ChangeDetail, ChangeNumber, ReviewerState};

use super::{ActionDialog, ActionEvent, Done, Kind, Step, describe};
use crate::session::is_unauthorized;

/// One write of the steps.
#[derive(Clone, Debug)]
pub(super) enum Edit {
    AddReviewer {
        input: String,
        state: ReviewerState,
        confirmed: bool,
    },
    RemoveReviewer(String),
    Topic(Option<String>),
    Hashtags {
        add: Vec<String>,
        remove: Vec<String>,
    },
}

/// What the server answered.
pub(super) struct Outcome {
    /// The answer to an add (which may be "no" with a 200).
    pub added: Option<AddReviewerResult>,
    /// The change as the server has it now; `None` if only fetching it
    /// failed.
    pub detail: Option<ChangeDetail>,
}

impl Edit {
    pub fn kind(&self) -> Kind {
        match self {
            Edit::AddReviewer { .. } | Edit::RemoveReviewer(_) => Kind::Reviewers,
            Edit::Topic(_) => Kind::Topic,
            Edit::Hashtags { .. } => Kind::Hashtags,
        }
    }

    async fn send(
        self,
        syncer: &sync::Syncer,
        change: ChangeNumber,
    ) -> Result<Outcome, sync::Error> {
        match self {
            Edit::AddReviewer {
                input,
                state,
                confirmed,
            } => {
                let r = syncer
                    .add_reviewer(change, &input, state, confirmed)
                    .await?;
                Ok(Outcome {
                    added: Some(r.result),
                    detail: r.detail,
                })
            }
            Edit::RemoveReviewer(who) => Ok(Outcome {
                added: None,
                detail: syncer.remove_reviewer(change, &who).await?,
            }),
            Edit::Topic(topic) => Ok(Outcome {
                added: None,
                detail: syncer.set_topic(change, topic.as_deref()).await?,
            }),
            Edit::Hashtags { add, remove } => Ok(Outcome {
                added: None,
                detail: syncer.edit_hashtags(change, &add, &remove).await?,
            }),
        }
    }
}

impl ActionDialog {
    pub(super) fn run_edit(&mut self, edit: Edit, cx: &mut Context<Self>) {
        if self.running() {
            return;
        }
        let syncer = self.session.syncer().clone();
        let change = self.number;
        self.busy = true;
        self.error = None;
        self.rev.notice = None;
        self.set_fields_disabled(true, cx);
        cx.notify();
        self.task = Some(cx.spawn(async move |this, cx| {
            let sent = edit.clone();
            let result = cx
                .background_spawn(async move { sent.send(&syncer, change).await })
                .await;
            this.update(cx, |this, cx| this.finished_edit(edit, result, cx))
                .ok();
        }));
    }

    fn finished_edit(
        &mut self,
        edit: Edit,
        result: Result<Outcome, sync::Error>,
        cx: &mut Context<Self>,
    ) {
        self.busy = false;
        self.set_fields_disabled(false, cx);
        let kind = edit.kind();
        match result {
            Err(e) if is_unauthorized(&e) => cx.emit(ActionEvent::Unauthorized),
            Err(e) => {
                self.error = Some(describe(kind, &e));
                self.refocus_step(cx);
            }
            Ok(out) => match edit {
                Edit::AddReviewer { input, state, .. } => {
                    self.reviewer_added(input, state, out, cx)
                }
                Edit::RemoveReviewer(who) => self.reviewer_removed(&who, out, cx),
                Edit::Topic(_) | Edit::Hashtags { .. } => {
                    cx.emit(ActionEvent::Done(Box::new(Done {
                        kind,
                        detail: out.detail,
                    })))
                }
            },
        }
        cx.notify();
    }

    fn set_fields_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        self.message
            .update(cx, |i, cx| i.set_disabled(disabled, cx));
        self.base.update(cx, |i, cx| i.set_disabled(disabled, cx));
        self.rev
            .input
            .update(cx, |i, cx| i.set_disabled(disabled, cx));
        self.tags
            .topic
            .update(cx, |i, cx| i.set_disabled(disabled, cx));
        self.tags
            .add
            .update(cx, |i, cx| i.set_disabled(disabled, cx));
    }

    /// Puts the focus back in the step's text field.
    pub(super) fn refocus_step(&mut self, cx: &App) {
        self.pending_focus = match self.step {
            Step::Reviewers => Some(self.rev.input.read(cx).focus_handle(cx)),
            Step::Topic => Some(self.tags.topic.read(cx).focus_handle(cx)),
            Step::Hashtags => Some(self.tags.add.read(cx).focus_handle(cx)),
            _ => None,
        };
    }
}
