//! The reviewers step: add a reviewer or CC with autocomplete, move people
//! between the two lists and remove them.
//!
//! Typing in the field asks the server for suggestions, after a pause of
//! [`DEBOUNCE`] and on the background executor. Starting a new query drops
//! the one before it, and an answer is used only if the field still holds
//! the text it was for. A blank field asks for nothing. People already on
//! the change are not suggested. The step stays open after each change, so
//! several people can be added in a row.

use std::time::Duration;

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use model::{Account, AddReviewerResult, ChangeDetail, ReviewerState, SuggestedReviewer};

use super::actions::*;
use super::edit::{Edit, Outcome};
use super::{ActionDialog, ActionEvent, Done, Kind, Step};
use crate::session::is_unauthorized;

/// How long the field must be quiet before the server is asked.
pub(super) const DEBOUNCE: Duration = Duration::from_millis(200);

/// A group the server wants confirmed; the next Enter on the same entry
/// sends `confirmed`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Pending {
    input: String,
    state: ReviewerState,
}

pub(super) struct Reviewers {
    pub input: Entity<InputState>,
    pub suggestions: Vec<SuggestedReviewer>,
    pub selected: usize,
    pub pending: Option<Pending>,
    /// The debounce and the request; replacing it drops the older query.
    pub task: Option<Task<()>>,
    /// One focus handle per person listed (reviewers, then CCs).
    pub rows: Vec<FocusHandle>,
    /// Something was added or removed since the step opened.
    pub changed: bool,
    /// `ChangeDetail` is what the server has now (not patched locally).
    pub fresh: bool,
    /// A question or a result for the user (not an error).
    pub notice: Option<String>,
    /// Why there are no suggestions, if the request failed.
    pub unavailable: Option<String>,
    /// Empty the field in the next frame (`set_value` needs the window).
    pub clear_input: bool,
    /// An edit started from a list row: the focus goes back to the list
    /// when it is done, to this row.
    pub list_focus: Option<ListFocus>,
}

/// Where the focus goes in the list after an edit started from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ListFocus {
    /// The row at this position, or the last one if the list got shorter
    /// (after a removal: the row that moved up, else the one above).
    Row(usize),
    /// The row of this person (after a move between reviewers and CC).
    Person(String),
}

impl Reviewers {
    pub fn new(window: &mut Window, cx: &mut Context<ActionDialog>) -> Self {
        Self {
            input: cx.new(|cx| {
                InputState::new(window, cx).placeholder("Add a reviewer: name, email or group")
            }),
            suggestions: Vec::new(),
            selected: 0,
            pending: None,
            task: None,
            rows: Vec::new(),
            changed: false,
            fresh: true,
            notice: None,
            unavailable: None,
            clear_input: false,
            list_focus: None,
        }
    }

    fn reset_suggestions(&mut self) {
        self.suggestions.clear();
        self.selected = 0;
        self.pending = None;
        self.task = None;
        self.unavailable = None;
    }
}

impl Reviewers {
    /// The row to focus after a list edit, if the edit started there and a
    /// row is left.
    pub(super) fn take_list_focus(&mut self, detail: &ChangeDetail) -> Option<FocusHandle> {
        let target = self.list_focus.take()?;
        let ix = match target {
            ListFocus::Row(ix) => ix,
            ListFocus::Person(id) => people(detail)
                .iter()
                .position(|(a, _)| a.id.to_string() == id)?,
        };
        self.rows
            .get(ix.min(self.rows.len().checked_sub(1)?))
            .cloned()
    }
}

/// The people of the change, reviewers first.
fn people(detail: &ChangeDetail) -> Vec<(&Account, ReviewerState)> {
    detail
        .reviewers
        .iter()
        .map(|a| (a, ReviewerState::Reviewer))
        .chain(detail.cc.iter().map(|a| (a, ReviewerState::Cc)))
        .collect()
}

/// What the page would show after the add, for when fetching the change
/// again failed.
fn apply_added(detail: &mut ChangeDetail, result: &AddReviewerResult) {
    for a in result.reviewers.iter().chain(&result.ccs) {
        detail.reviewers.retain(|r| r.id != a.id);
        detail.cc.retain(|r| r.id != a.id);
    }
    detail.reviewers.extend(result.reviewers.iter().cloned());
    detail.cc.extend(result.ccs.iter().cloned());
}

/// The label of a suggestion and a second line for it.
fn describe_suggestion(s: &SuggestedReviewer) -> (String, Option<String>) {
    match s {
        SuggestedReviewer::Account(a) => {
            let sub = a
                .email
                .clone()
                .or_else(|| a.username.clone())
                .filter(|x| *x != a.display());
            (a.display(), sub)
        }
        SuggestedReviewer::Group { group, count, .. } => {
            let name = group.name.clone().unwrap_or_else(|| group.id.clone());
            let sub = match count {
                Some(1) => "Group, 1 member".to_owned(),
                Some(n) => format!("Group, {n} members"),
                None => "Group".to_owned(),
            };
            (name, Some(sub))
        }
    }
}

impl ActionDialog {
    pub(super) fn enter_reviewers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.step = Step::Reviewers;
        self.error = None;
        self.rev.notice = None;
        self.rev
            .input
            .update(cx, |i, cx| i.set_value("", window, cx));
        self.rev.reset_suggestions();
        self.sync_rows(cx);
        self.refocus_step(cx);
        cx.notify();
    }

    pub(super) fn sync_rows(&mut self, cx: &mut Context<Self>) {
        let n = people(&self.detail).len();
        self.rev.rows = (0..n).map(|_| cx.focus_handle()).collect();
    }

    // ---- suggestions ----

    /// The field changed: ask again after the pause.
    pub(super) fn query_changed(&mut self, cx: &mut Context<Self>) {
        if self.step != Step::Reviewers || self.running() {
            return;
        }
        let query = self.rev.input.read(cx).value().trim().to_owned();
        self.rev.reset_suggestions();
        self.rev.notice = None;
        self.error = None;
        cx.notify();
        if query.is_empty() {
            return;
        }
        let syncer = self.session.syncer().clone();
        let change = self.number;
        self.rev.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let asked = query.clone();
            let result = cx
                .background_spawn(async move { syncer.suggest_reviewers(change, &asked).await })
                .await;
            this.update(cx, |this, cx| this.suggested(&query, result, cx))
                .ok();
        }));
    }

    pub(super) fn suggested(
        &mut self,
        query: &str,
        result: Result<Vec<SuggestedReviewer>, sync::Error>,
        cx: &mut Context<Self>,
    ) {
        // Dropped with the task normally; this is for a change of the
        // field that did not go through `query_changed`.
        if self.step != Step::Reviewers || self.rev.input.read(cx).value().trim() != query {
            return;
        }
        match result {
            Ok(list) => {
                let on_change: Vec<_> = people(&self.detail).iter().map(|(a, _)| a.id).collect();
                self.rev.suggestions = list
                    .into_iter()
                    .filter(|s| match s {
                        SuggestedReviewer::Account(a) => !on_change.contains(&a.id),
                        SuggestedReviewer::Group { .. } => true,
                    })
                    .collect();
                self.rev.selected = 0;
                self.rev.unavailable = None;
            }
            Err(e) if is_unauthorized(&e) => cx.emit(ActionEvent::Unauthorized),
            Err(e) => {
                self.rev.suggestions.clear();
                self.rev.unavailable = Some(format!("No suggestions: {e}"));
            }
        }
        cx.notify();
    }

    fn move_suggestion(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.rev.suggestions.len() as isize;
        if self.step != Step::Reviewers || len == 0 {
            return;
        }
        self.rev.selected = (self.rev.selected as isize + delta).rem_euclid(len) as usize;
        self.rev.pending = None;
        self.rev.notice = None;
        cx.notify();
    }

    pub(super) fn suggest_down(&mut self, _: &SuggestDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_suggestion(1, cx);
    }

    pub(super) fn suggest_up(&mut self, _: &SuggestUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_suggestion(-1, cx);
    }

    // ---- adding ----

    /// Enter (reviewer) or Shift-Enter (CC) in the field: adds the selected
    /// suggestion, or what was typed if nothing is suggested (an email
    /// address the server does not know as a suggestion).
    pub(super) fn add_current(&mut self, state: ReviewerState, cx: &mut Context<Self>) {
        if self.step != Step::Reviewers || self.running() {
            return;
        }
        self.rev.list_focus = None;
        let typed = self.rev.input.read(cx).value().trim().to_owned();
        let input = match self.rev.suggestions.get(self.rev.selected) {
            Some(s) => s.reviewer_input(),
            None if !typed.is_empty() => typed,
            None => return,
        };
        let confirmed = self
            .rev
            .pending
            .as_ref()
            .is_some_and(|p| p.input == input && p.state == state);
        self.run_edit(
            Edit::AddReviewer {
                input,
                state,
                confirmed,
            },
            cx,
        );
    }

    pub(super) fn reviewer_added(
        &mut self,
        input: String,
        state: ReviewerState,
        out: Outcome,
        cx: &mut Context<Self>,
    ) {
        let Some(result) = out.added else { return };
        if result.is_added() {
            self.took_effect(out.detail, |d| apply_added(d, &result));
            self.rev.reset_suggestions();
            self.rev.clear_input = true;
            self.sync_rows(cx);
            self.refocus_step(cx);
        } else if result.confirm {
            let question = result
                .error
                .as_deref()
                .map(str::trim)
                .filter(|q| !q.is_empty())
                .unwrap_or("This group is large.");
            self.rev.notice = Some(format!("{question}\nPress Enter again to add them."));
            self.rev.pending = Some(Pending { input, state });
            self.refocus_step(cx);
        } else {
            let why = result.error.as_deref().map(str::trim).unwrap_or_default();
            self.error = Some(if why.is_empty() {
                "Not added: the server did not add anybody.".to_owned()
            } else {
                format!("Not added: {why}")
            });
            self.refocus_step(cx);
        }
    }

    /// Takes the detail the server returned, or applies `patch` to ours if
    /// fetching it failed after the write went through.
    fn took_effect(&mut self, detail: Option<ChangeDetail>, patch: impl FnOnce(&mut ChangeDetail)) {
        self.rev.changed = true;
        match detail {
            Some(d) => {
                self.detail = d;
                self.rev.fresh = true;
            }
            None => {
                patch(&mut self.detail);
                self.rev.fresh = false;
            }
        }
    }

    // ---- the lists ----

    fn person_at(&self, ix: usize) -> Option<(Account, ReviewerState)> {
        people(&self.detail)
            .into_iter()
            .nth(ix)
            .map(|(a, s)| (a.clone(), s))
    }

    pub(super) fn remove_person(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.step != Step::Reviewers || self.running() {
            return;
        }
        if let Some((a, _)) = self.person_at(ix) {
            self.rev.list_focus = Some(ListFocus::Row(ix));
            self.run_edit(Edit::RemoveReviewer(a.id.to_string()), cx);
        }
    }

    /// Reviewer to CC and back.
    pub(super) fn move_person(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.step != Step::Reviewers || self.running() {
            return;
        }
        if let Some((a, state)) = self.person_at(ix) {
            let to = match state {
                ReviewerState::Reviewer => ReviewerState::Cc,
                ReviewerState::Cc => ReviewerState::Reviewer,
            };
            self.rev.list_focus = Some(ListFocus::Person(a.id.to_string()));
            self.run_edit(
                Edit::AddReviewer {
                    input: a.id.to_string(),
                    state: to,
                    confirmed: false,
                },
                cx,
            );
        }
    }

    pub(super) fn reviewer_removed(&mut self, who: &str, out: Outcome, cx: &mut Context<Self>) {
        self.took_effect(out.detail, |d| {
            d.reviewers.retain(|a| a.id.to_string() != who);
            d.cc.retain(|a| a.id.to_string() != who);
        });
        self.sync_rows(cx);
        self.refocus_step(cx);
    }

    fn remove_row(&mut self, ix: usize, _: &RemoveItem, _: &mut Window, cx: &mut Context<Self>) {
        self.remove_person(ix, cx);
    }

    /// Esc: closes, and tells the page about the changes made.
    pub(super) fn close_reviewers(&mut self, cx: &mut Context<Self>) {
        if self.rev.changed {
            cx.emit(ActionEvent::Done(Box::new(Done {
                kind: Kind::Reviewers,
                detail: self.rev.fresh.then(|| self.detail.clone()),
            })));
        } else {
            cx.emit(ActionEvent::Close);
        }
    }

    /// The frame-bound housekeeping of the step.
    pub(super) fn reviewers_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.rev.clear_input) {
            let input = self.rev.input.clone();
            window.defer(cx, move |window, cx| {
                input.update(cx, |i, cx| i.set_value("", window, cx));
            });
        }
    }

    // ---- rendering ----

    /// The person's current votes, `CR +2  V +1`, coloured by sign.
    fn vote_chips(
        &self,
        account: model::AccountId,
        theme: &gpui_kit::component::Theme,
    ) -> Option<Div> {
        let votes = crate::labels::votes_of(&self.detail, account);
        if votes.is_empty() {
            return None;
        }
        Some(
            h_flex()
                .flex_none()
                .gap_2()
                .text_xs()
                .children(votes.into_iter().map(|(label, value)| {
                    let color = if value > 0 {
                        theme.success
                    } else {
                        theme.danger
                    };
                    div()
                        .text_color(color)
                        .child(format!("{label} {}", crate::reply::plan::vote_text(value)))
                })),
        )
    }

    pub(super) fn render_reviewers(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let suggestions: Vec<_> = self
            .rev
            .suggestions
            .iter()
            .enumerate()
            .map(|(ix, s)| {
                let (label, sub) = describe_suggestion(s);
                let hover = theme.list_hover;
                v_flex()
                    .id(("suggestion", ix))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .when(ix == self.rev.selected, |d| d.bg(theme.list_active))
                    .hover(move |d| d.bg(hover))
                    .child(div().truncate().child(label))
                    .when_some(sub, |d, sub| {
                        d.child(div().text_xs().truncate().text_color(muted).child(sub))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.rev.selected = ix;
                        this.add_current(ReviewerState::Reviewer, cx);
                    }))
                    .into_any_element()
            })
            .collect();
        let rows: Vec<_> = people(&self.detail)
            .into_iter()
            .enumerate()
            .filter_map(|(ix, (a, state))| {
                let handle = self.rev.rows.get(ix)?.clone();
                let focused = handle.is_focused(window);
                let other = match state {
                    ReviewerState::Reviewer => "To CC",
                    ReviewerState::Cc => "To reviewers",
                };
                Some(
                    h_flex()
                        .id(("person", ix))
                        .key_context(REVIEWER_ROW)
                        .track_focus(&handle)
                        .on_action(cx.listener(move |this, a: &RemoveItem, w, cx| {
                            this.remove_row(ix, a, w, cx)
                        }))
                        .on_action(
                            cx.listener(move |this, _: &MoveItem, _, cx| this.move_person(ix, cx)),
                        )
                        .gap_2()
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .border_1()
                        .border_color(if focused {
                            theme.ring
                        } else {
                            theme.transparent
                        })
                        .when(state == ReviewerState::Cc, |d| {
                            d.child(div().text_xs().text_color(muted).child("CC"))
                        })
                        .child(div().min_w_0().flex_1().truncate().child(a.display()))
                        .children(self.vote_chips(a.id, &theme))
                        .when_some(a.email.clone(), |d, e| {
                            d.child(
                                div()
                                    .text_xs()
                                    .truncate()
                                    .max_w_48()
                                    .text_color(muted)
                                    .child(e),
                            )
                        })
                        .child(
                            div()
                                .id(("person-move", ix))
                                .text_xs()
                                .text_color(muted)
                                .cursor_pointer()
                                .child(other)
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.move_person(ix, cx)),
                                ),
                        )
                        .child(
                            div()
                                .id(("person-remove", ix))
                                .text_xs()
                                .text_color(theme.danger)
                                .cursor_pointer()
                                .child("Remove")
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.remove_person(ix, cx)),
                                ),
                        )
                        .into_any_element(),
                )
            })
            .collect();
        let none = rows.is_empty();
        v_flex()
            .gap_1()
            .px_4()
            .py_2()
            .child(Input::new(&self.rev.input).small())
            .when(!suggestions.is_empty(), |d| {
                d.child(v_flex().gap_0p5().children(suggestions))
            })
            .when_some(self.rev.unavailable.clone(), |d, why| {
                d.child(div().text_xs().text_color(muted).child(why))
            })
            .when_some(self.rev.notice.clone(), |d, n| {
                d.child(lines(&n, theme.warning))
            })
            .child(
                div()
                    .pt_2()
                    .text_xs()
                    .font_semibold()
                    .text_color(muted)
                    .child("REVIEWERS AND CC"),
            )
            .children(rows)
            .when(none, |d| {
                d.child(div().text_color(muted).child("Nobody yet."))
            })
            .into_any_element()
    }
}

/// Text with line breaks, one element per line.
pub(super) fn lines(text: &str, color: Hsla) -> Div {
    v_flex()
        .text_xs()
        .text_color(color)
        .children(text.lines().map(|l| div().child(l.to_owned())))
}
