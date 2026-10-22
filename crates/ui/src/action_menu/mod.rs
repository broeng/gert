//! The change action menu (`m`; `shift-a` on the diff page): submit, rebase,
//! abandon, restore, work in progress / ready for review, private, revert
//! and cherry-pick, each as a second step with a confirmation where it
//! needs one.
//!
//! Like the reply dialog it is a modal overlay owned by the shell: the page
//! asks for it with an event and takes the result back with `action_done`.
//! It takes the keyboard (the shell's single-letter keys are off while it
//! is open, `!ActionDialog` in their context) and it cycles Tab itself.
//!
//! # What is offered
//!
//! Only what the server put in the change's `actions` or the current
//! revision's: Gerrit leaves out what the user may not do at all, so those
//! are not shown. An action present with `enabled: false` (rebase at the
//! branch tip, submit while a requirement is open) is shown greyed out with
//! the server's `title` as the reason, and does nothing.
//!
//! # Steps
//!
//! 1. The menu: `j`/`k`/arrows move, `Enter` or the mnemonic (`s` submit,
//!    `b` rebase, `a` abandon, `r` restore, `w` WIP / ready, `p` private,
//!    `e` revert, `c` cherry-pick) picks. The mnemonics work only here.
//! 2. A confirmation for submit, rebase, abandon, restore and WIP, with an
//!    optional message (abandon, restore, WIP) or base and options (rebase:
//!    allow conflicts, and on behalf of the uploader where the server offers
//!    it; Tab to a checkbox, Space switches it). `Ctrl-Enter`
//!    (`Cmd-Enter`) confirms, `Esc` goes back to the menu (and from the
//!    menu closes the dialog). Ready and the private toggles run right
//!    from the menu. Revert asks for the new change's commit message
//!    (prefilled like Gerrit's default) and cherry-pick for the destination
//!    branch (a plain field) and the message (prefilled with the change's);
//!    both open the new change when the server made it.
//! 3. Reviewers (`v`), topic (`t`) and hashtags (`h`) are steps of their own
//!    with text fields: see `reviewers` and `tags`. The reviewers step stays
//!    open after each change; `Esc` goes back to the menu, and closing the
//!    menu gives the page the new detail.
//! 4. Running: the request goes out on the background executor and `Esc`
//!    does not cancel it. A refused action (403, 409, ...) goes back to the
//!    step it came from with the server's message; 401 goes to the login
//!    flow; nothing is queued, an action needs a connection.
//!
//! The data layer (`Syncer::submit` and friends) fetches the change again
//! after the server accepted an action; the page takes that detail.

pub mod actions;
mod edit;
mod reviewers;
mod tags;
#[cfg(test)]
mod tests;

use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use model::{
    ChangeDetail, ChangeNumber, REBASE_ON_BEHALF_OF_UPLOADER, RebaseOptions, ReviewerState,
    RevisionId, SubmitRequirementStatus, action_id,
};

use reviewers::Reviewers;
use tags::Tags;

use crate::{
    reply::{Look, chip},
    session::{Session, is_unauthorized},
};
use actions::*;

/// What the dialog tells the shell.
pub enum ActionEvent {
    /// The user closed it.
    Close,
    /// The server answered 401: back to the login flow.
    Unauthorized,
    /// The server did it.
    Done(Box<Done>),
    /// The server refused an action and the change was fetched again: this
    /// is how the server has it now. The dialog stays open.
    Refreshed(Box<ChangeDetail>),
}

/// The outcome of an action the server accepted.
pub struct Done {
    pub kind: Kind,
    /// The change as the server has it now; `None` if only fetching it
    /// failed (the action went through).
    pub detail: Option<ChangeDetail>,
    /// For revert and cherry-pick: the new change, which `detail` is then
    /// the detail of. The shell opens it instead of updating the page.
    pub opened: Option<ChangeNumber>,
}

impl Done {
    /// The note for the page.
    pub fn note(&self) -> String {
        match self.opened {
            Some(n) => format!("{} as change #{n}", self.kind.past()),
            None => self.kind.past().to_owned(),
        }
    }
}

/// What the user typed for an action.
#[derive(Default)]
struct Args {
    message: Option<String>,
    base: Option<String>,
    destination: Option<String>,
}

/// Why the menu cannot open now, as a note for the user.
pub fn unavailable(session: &Session) -> Option<&'static str> {
    if !session.can_comment() {
        Some("Sign in to change this change")
    } else if !session.can_upload() {
        Some("Still signing in, try again in a moment")
    } else {
        None
    }
}

/// An action of the menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Submit,
    Rebase,
    Abandon,
    Restore,
    Wip,
    Ready,
    Private,
    Unprivate,
    Revert,
    CherryPick,
    /// Steps with their own fields, not run from the menu.
    Reviewers,
    Topic,
    Hashtags,
}

impl Kind {
    const ALL: [Kind; 13] = [
        Kind::Submit,
        Kind::Rebase,
        Kind::Abandon,
        Kind::Restore,
        Kind::Wip,
        Kind::Ready,
        Kind::Private,
        Kind::Unprivate,
        Kind::Revert,
        Kind::CherryPick,
        Kind::Reviewers,
        Kind::Topic,
        Kind::Hashtags,
    ];

    /// The id of the action in Gerrit's `actions` maps.
    fn id(self) -> &'static str {
        match self {
            Kind::Submit => action_id::SUBMIT,
            Kind::Rebase => action_id::REBASE,
            Kind::Abandon => action_id::ABANDON,
            Kind::Restore => action_id::RESTORE,
            Kind::Wip => action_id::WIP,
            Kind::Ready => action_id::READY,
            Kind::Private => action_id::PRIVATE,
            Kind::Unprivate => action_id::PRIVATE_DELETE,
            Kind::Revert => action_id::REVERT,
            Kind::CherryPick => action_id::CHERRYPICK,
            Kind::Reviewers => "",
            Kind::Topic => action_id::TOPIC,
            Kind::Hashtags => action_id::HASHTAGS,
        }
    }

    /// The label when the server gives none.
    fn label(self) -> &'static str {
        match self {
            Kind::Submit => "Submit",
            Kind::Rebase => "Rebase",
            Kind::Abandon => "Abandon",
            Kind::Restore => "Restore",
            Kind::Wip => "Mark work in progress",
            Kind::Ready => "Ready for review",
            Kind::Private => "Mark private",
            Kind::Unprivate => "Unmark private",
            Kind::Revert => "Revert",
            Kind::CherryPick => "Cherry-pick",
            Kind::Reviewers => "Reviewers and CC",
            Kind::Topic => "Edit topic",
            Kind::Hashtags => "Edit hashtags",
        }
    }

    /// The mnemonic key, valid in the menu only.
    fn mnemonic(self) -> char {
        match self {
            Kind::Submit => 's',
            Kind::Rebase => 'b',
            Kind::Abandon => 'a',
            Kind::Restore => 'r',
            Kind::Wip | Kind::Ready => 'w',
            Kind::Private | Kind::Unprivate => 'p',
            Kind::Revert => 'e',
            Kind::CherryPick => 'c',
            Kind::Reviewers => 'v',
            Kind::Topic => 't',
            Kind::Hashtags => 'h',
        }
    }

    /// What the page says afterwards.
    fn past(self) -> &'static str {
        match self {
            Kind::Submit => "Submitted",
            Kind::Rebase => "Rebased",
            Kind::Abandon => "Abandoned",
            Kind::Restore => "Restored",
            Kind::Wip => "Marked work in progress",
            Kind::Ready => "Marked ready for review",
            Kind::Private => "Marked private",
            Kind::Unprivate => "Marked not private",
            Kind::Revert => "Revert created",
            Kind::CherryPick => "Cherry-pick created",
            Kind::Reviewers => "Reviewers changed",
            Kind::Topic => "Topic changed",
            Kind::Hashtags => "Hashtags changed",
        }
    }

    /// For "Not <done>" in errors.
    fn not_done(self) -> &'static str {
        match self {
            Kind::Submit => "Not submitted",
            Kind::Rebase => "Not rebased",
            Kind::Abandon => "Not abandoned",
            Kind::Restore => "Not restored",
            Kind::Wip => "Not marked work in progress",
            Kind::Ready => "Not marked ready for review",
            Kind::Private => "Not marked private",
            Kind::Unprivate => "Not marked not private",
            Kind::Revert => "Not reverted",
            Kind::CherryPick => "Not cherry-picked",
            Kind::Reviewers => "Reviewers not changed",
            Kind::Topic => "Topic not changed",
            Kind::Hashtags => "Hashtags not changed",
        }
    }

    /// Whether a confirmation comes first. Ready and the private toggles
    /// are cheap to undo and run from the menu.
    fn confirms(self) -> bool {
        !matches!(
            self,
            Kind::Ready
                | Kind::Private
                | Kind::Unprivate
                | Kind::Reviewers
                | Kind::Topic
                | Kind::Hashtags
        )
    }

    /// Whether the confirmation has a message field.
    fn takes_message(self) -> bool {
        matches!(self, Kind::Abandon | Kind::Restore | Kind::Wip)
    }

    /// Sends the action. The detail is the changed change's, or for revert
    /// and cherry-pick the new one, whose number is the second part.
    async fn send(
        self,
        syncer: &sync::Syncer,
        change: ChangeNumber,
        revision: Option<RevisionId>,
        args: Args,
        rebase: RebaseOptions,
    ) -> Result<(Option<ChangeDetail>, Option<ChangeNumber>), sync::Error> {
        let message = args.message.as_deref();
        let plain = |d| Ok((d, None));
        let created = |c: sync::NewChange| Ok((c.detail, Some(c.number)));
        match self {
            Kind::Submit => plain(syncer.submit(change).await?),
            Kind::Rebase => plain(syncer.rebase(change, args.base.as_deref(), rebase).await?),
            Kind::Abandon => plain(syncer.abandon(change, message).await?),
            Kind::Restore => plain(syncer.restore(change, message).await?),
            Kind::Wip => plain(syncer.set_work_in_progress(change, true, message).await?),
            Kind::Ready => plain(syncer.set_work_in_progress(change, false, None).await?),
            Kind::Private => plain(syncer.set_private(change, true).await?),
            Kind::Unprivate => plain(syncer.set_private(change, false).await?),
            Kind::Revert => created(syncer.revert(change, message).await?),
            Kind::CherryPick => {
                // `confirm` checked both.
                let (Some(revision), Some(destination)) = (revision, args.destination) else {
                    return plain(None);
                };
                created(
                    syncer
                        .cherry_pick(change, &revision, &destination, message)
                        .await?,
                )
            }
            // Steps: their edits go through `edit::Edit`.
            Kind::Reviewers | Kind::Topic | Kind::Hashtags => plain(None),
        }
    }
}

/// One line of the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuEntry {
    pub kind: Kind,
    pub label: String,
    pub enabled: bool,
    /// The server's tooltip: why it is disabled.
    pub reason: Option<String>,
}

/// The entries the server offers for `detail`, in menu order.
pub fn menu_entries(detail: &ChangeDetail) -> Vec<MenuEntry> {
    Kind::ALL
        .into_iter()
        .filter_map(|kind| {
            if kind == Kind::Reviewers {
                // The server decides; a refusal is shown in the step.
                return Some(MenuEntry {
                    kind,
                    label: kind.label().to_owned(),
                    enabled: true,
                    reason: None,
                });
            }
            let a = detail.action(kind.id())?;
            Some(MenuEntry {
                kind,
                label: a
                    .label
                    .as_deref()
                    .filter(|l| !l.trim().is_empty())
                    .unwrap_or(kind.label())
                    .to_owned(),
                enabled: a.enabled,
                reason: a
                    .title
                    .as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(str::to_owned),
            })
        })
        .collect()
}

/// A checkbox of the rebase confirmation.
#[derive(Clone, Copy)]
enum RebaseFlag {
    Conflicts,
    Behalf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Menu,
    Confirm(Kind),
    Running(Kind),
    Reviewers,
    Topic,
    Hashtags,
}

pub struct ActionDialog {
    session: Session,
    detail: ChangeDetail,
    number: ChangeNumber,
    entries: Vec<MenuEntry>,
    selected: usize,
    step: Step,
    error: Option<String>,
    message: Entity<TextareaState>,
    base: Entity<InputState>,
    /// Revert's and cherry-pick's commit message, and cherry-pick's
    /// destination branch.
    commit_message: Entity<TextareaState>,
    destination: Entity<InputState>,
    /// Whose prefill `commit_message` holds, so going back and in again
    /// keeps what was typed.
    prefilled: Option<Kind>,
    /// The rebase checkboxes and their focus.
    rebase: RebaseOptions,
    conflicts_focus: FocusHandle,
    behalf_focus: FocusHandle,
    rev: Reviewers,
    tags: Tags,
    /// A reviewer, topic or hashtag edit is under way.
    busy: bool,
    focus: FocusHandle,
    cancel_focus: FocusHandle,
    confirm_focus: FocusHandle,
    /// Focus to take after the next frame, when its control exists.
    pending_focus: Option<FocusHandle>,
    task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<ActionEvent> for ActionDialog {}

impl Focusable for ActionDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ActionDialog {
    /// Opens the menu for `detail` with the focus in the dialog.
    pub fn new(
        session: Session,
        detail: ChangeDetail,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let entries = menu_entries(&detail);
        let selected = entries.iter().position(|e| e.enabled).unwrap_or(0);
        let message = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 8)
                .placeholder("Message (optional)")
        });
        let base = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Base: change number or commit SHA (optional)")
        });
        let commit_message = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(6, 14)
                .placeholder("Commit message")
        });
        let destination = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Destination branch, e.g. stable-1.0")
        });
        let rev = Reviewers::new(window, cx);
        let tags = Tags::new(window, cx);
        let subs = vec![
            cx.subscribe_in(
                &rev.input,
                window,
                |this, _, ev: &InputEvent, _, cx| match ev {
                    InputEvent::Change => this.query_changed(cx),
                    InputEvent::PressEnter {
                        secondary: false,
                        shift,
                    } => this.add_current(
                        if *shift {
                            ReviewerState::Cc
                        } else {
                            ReviewerState::Reviewer
                        },
                        cx,
                    ),
                    _ => {}
                },
            ),
            cx.subscribe_in(&tags.topic, window, |this, _, ev: &InputEvent, _, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.save_topic(cx);
                }
            }),
            cx.subscribe_in(&tags.add, window, |this, _, ev: &InputEvent, _, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.save_hashtags(cx);
                }
            }),
            cx.subscribe_in(
                &message,
                window,
                |this, _, ev: &InputEvent, window, cx| match ev {
                    // Ctrl-Enter: the field also inserts a newline; the
                    // text is trimmed when it is used.
                    InputEvent::PressEnter {
                        secondary: true, ..
                    } => this.confirm(window, cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            ),
            cx.subscribe_in(&base, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.confirm(window, cx);
                }
            }),
            cx.subscribe_in(
                &destination,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        this.confirm(window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &commit_message,
                window,
                |this, _, ev: &InputEvent, window, cx| match ev {
                    InputEvent::PressEnter {
                        secondary: true, ..
                    } => this.confirm(window, cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            ),
        ];
        let this = Self {
            number: detail.summary.number,
            session,
            detail,
            entries,
            selected,
            step: Step::Menu,
            error: None,
            message,
            base,
            commit_message,
            destination,
            prefilled: None,
            rebase: RebaseOptions::default(),
            conflicts_focus: cx.focus_handle(),
            behalf_focus: cx.focus_handle(),
            rev,
            tags,
            busy: false,
            focus: cx.focus_handle(),
            cancel_focus: cx.focus_handle(),
            confirm_focus: cx.focus_handle(),
            pending_focus: None,
            task: None,
            _subs: subs,
        };
        window.focus(&this.focus, cx);
        this
    }

    fn running(&self) -> bool {
        matches!(self.step, Step::Running(_)) || self.busy
    }

    // ---- the menu ----

    fn step_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.step != Step::Menu || self.entries.is_empty() {
            return;
        }
        let len = self.entries.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
        self.error = None;
        cx.notify();
    }

    fn down(&mut self, _: &MenuDown, _: &mut Window, cx: &mut Context<Self>) {
        self.step_selection(1, cx);
    }

    fn up(&mut self, _: &MenuUp, _: &mut Window, cx: &mut Context<Self>) {
        self.step_selection(-1, cx);
    }

    fn run_selected(&mut self, _: &MenuRun, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_at(self.selected, window, cx);
    }

    /// Picks the entry whose mnemonic is `key` (`w`: WIP or ready, `p`:
    /// private or not, whichever the change offers).
    fn pick_key(&mut self, key: char, window: &mut Window, cx: &mut Context<Self>) {
        if self.step != Step::Menu {
            return;
        }
        if let Some(ix) = self.entries.iter().position(|e| e.kind.mnemonic() == key) {
            self.pick_at(ix, window, cx);
        }
    }

    /// Takes the entry: a disabled one only gets the selection (its reason
    /// is on screen).
    fn pick_at(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.step != Step::Menu {
            return;
        }
        let Some(entry) = self.entries.get(ix) else {
            return;
        };
        self.selected = ix;
        self.error = None;
        if !entry.enabled {
            cx.notify();
            return;
        }
        let kind = entry.kind;
        match kind {
            Kind::Reviewers => return self.enter_reviewers(window, cx),
            Kind::Topic => return self.enter_topic(window, cx),
            Kind::Hashtags => return self.enter_hashtags(window, cx),
            _ => {}
        }
        if kind.confirms() {
            self.step = Step::Confirm(kind);
            if matches!(kind, Kind::Revert | Kind::CherryPick) && self.prefilled != Some(kind) {
                self.prefilled = Some(kind);
                let text = self.default_message(kind);
                self.commit_message
                    .update(cx, |i, cx| i.set_value(String::new(), window, cx));
                crate::reply::put_text(&self.commit_message, &text, window, cx);
            }
            let target = match kind {
                k if k.takes_message() => self.message.read(cx).focus_handle(cx),
                Kind::Rebase => self.base.read(cx).focus_handle(cx),
                Kind::Revert => self.commit_message.read(cx).focus_handle(cx),
                Kind::CherryPick => self.destination.read(cx).focus_handle(cx),
                _ => self.confirm_focus.clone(),
            };
            self.pending_focus = Some(target);
            cx.notify();
        } else {
            self.run(kind, Args::default(), window, cx);
        }
    }

    fn pick_submit(&mut self, _: &PickSubmit, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_key('s', window, cx);
    }
    fn pick_rebase(&mut self, _: &PickRebase, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_key('b', window, cx);
    }
    fn pick_abandon(&mut self, _: &PickAbandon, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_key('a', window, cx);
    }
    fn pick_restore(&mut self, _: &PickRestore, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_key('r', window, cx);
    }
    fn pick_wip(&mut self, _: &PickWip, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_key('w', window, cx);
    }
    fn pick_revert(&mut self, _: &PickRevert, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_key('e', window, cx);
    }
    fn pick_cherry_pick(
        &mut self,
        _: &PickCherryPick,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pick_key('c', window, cx);
    }
    fn pick_private(&mut self, _: &PickPrivate, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_key('p', window, cx);
    }

    fn pick_reviewers(&mut self, _: &PickReviewers, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_key('v', window, cx);
    }
    fn pick_topic(&mut self, _: &PickTopic, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_key('t', window, cx);
    }
    fn pick_hashtags(&mut self, _: &PickHashtags, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_key('h', window, cx);
    }

    // ---- closing, focus ----

    /// Esc: from a step back to the menu, from the menu out of the dialog.
    fn close_action(&mut self, _: &CloseMenu, _: &mut Window, cx: &mut Context<Self>) {
        if self.step == Step::Menu {
            self.close(cx);
        } else {
            self.back(cx);
        }
    }

    /// Leaves a step for the menu, keeping what was typed. Not while an
    /// action runs.
    fn back(&mut self, cx: &mut Context<Self>) {
        if self.running() || self.step == Step::Menu {
            return;
        }
        self.step = Step::Menu;
        self.error = None;
        self.pending_focus = Some(self.focus.clone());
        cx.notify();
    }

    /// Closes the dialog. Not while an action runs: the request is under
    /// way and cannot be taken back. If reviewers were changed on the way
    /// the page is told, so it takes the new detail.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if self.running() {
            return;
        }
        self.close_reviewers(cx);
    }

    fn tab_order(&self, cx: &App) -> Vec<FocusHandle> {
        let mut v = Vec::new();
        match self.step {
            Step::Reviewers => {
                v.push(self.rev.input.read(cx).focus_handle(cx));
                v.extend(self.rev.rows.iter().cloned());
                v.push(self.cancel_focus.clone());
                return v;
            }
            Step::Topic => {
                v.push(self.tags.topic.read(cx).focus_handle(cx));
            }
            Step::Hashtags => {
                v.push(self.tags.add.read(cx).focus_handle(cx));
                v.extend(self.tags.chips.iter().cloned());
            }
            _ => {}
        }
        if matches!(self.step, Step::Topic | Step::Hashtags) {
            v.push(self.cancel_focus.clone());
            v.push(self.confirm_focus.clone());
            return v;
        }
        let Step::Confirm(kind) = self.step else {
            return Vec::new();
        };
        if kind.takes_message() {
            v.push(self.message.read(cx).focus_handle(cx));
        }
        match kind {
            Kind::Revert => v.push(self.commit_message.read(cx).focus_handle(cx)),
            Kind::CherryPick => {
                v.push(self.destination.read(cx).focus_handle(cx));
                v.push(self.commit_message.read(cx).focus_handle(cx));
            }
            _ => {}
        }
        if kind == Kind::Rebase {
            v.push(self.base.read(cx).focus_handle(cx));
            v.push(self.conflicts_focus.clone());
            if self.offers_behalf() {
                v.push(self.behalf_focus.clone());
            }
        }
        v.push(self.cancel_focus.clone());
        v.push(self.confirm_focus.clone());
        v
    }

    fn move_focus(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let order = self.tab_order(cx);
        if order.is_empty() {
            return;
        }
        let len = order.len() as isize;
        let next = match order.iter().position(|h| h.is_focused(window)) {
            Some(i) => (i as isize + delta).rem_euclid(len) as usize,
            None if delta > 0 => 0,
            None => order.len() - 1,
        };
        window.focus(&order[next], cx);
    }

    fn focus_next(&mut self, _: &MenuNext, window: &mut Window, cx: &mut Context<Self>) {
        self.move_focus(1, window, cx);
    }

    fn focus_prev(&mut self, _: &MenuPrev, window: &mut Window, cx: &mut Context<Self>) {
        self.move_focus(-1, window, cx);
    }

    // ---- running ----

    fn confirm_action(&mut self, _: &ConfirmAction, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm(window, cx);
    }

    /// Runs the action of the confirmation with the typed message or base.
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.step {
            Step::Topic => return self.save_topic(cx),
            Step::Hashtags => return self.save_hashtags(cx),
            _ => {}
        }
        let Step::Confirm(kind) = self.step else {
            return;
        };
        let text = |s: &str| {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_owned())
        };
        let creates = matches!(kind, Kind::Revert | Kind::CherryPick);
        let message = if creates {
            text(self.commit_message.read(cx).value().as_ref())
        } else {
            kind.takes_message()
                .then(|| text(self.message.read(cx).value().as_ref()))
                .flatten()
        };
        let base = (kind == Kind::Rebase)
            .then(|| text(self.base.read(cx).value().as_ref()))
            .flatten();
        let destination = (kind == Kind::CherryPick)
            .then(|| text(self.destination.read(cx).value().as_ref()))
            .flatten();
        if kind == Kind::CherryPick && destination.is_none() {
            self.error = Some("Enter the branch to cherry-pick to.".to_owned());
            self.pending_focus = Some(self.destination.read(cx).focus_handle(cx));
            cx.notify();
            return;
        }
        if kind == Kind::CherryPick && self.detail.current_revision.is_none() {
            self.error = Some("The change has no current patch set loaded.".to_owned());
            cx.notify();
            return;
        }
        let args = Args {
            message,
            base,
            destination,
        };
        self.run(kind, args, window, cx);
    }

    /// What Gerrit's own revert and cherry-pick dialogs start from.
    fn default_message(&self, kind: Kind) -> String {
        let summary = &self.detail.summary;
        match kind {
            Kind::Revert => {
                let sha = self
                    .detail
                    .current_revision
                    .as_ref()
                    .map_or("", |r| r.as_str());
                format!(
                    "Revert \"{}\"\n\nThis reverts commit {sha}.\n\nReason for revert: ",
                    summary.subject
                )
            }
            _ => self
                .detail
                .commit_message
                .clone()
                .unwrap_or_else(|| summary.subject.clone()),
        }
    }

    /// Whether the server lets this user rebase on behalf of the uploader.
    fn offers_behalf(&self) -> bool {
        self.detail
            .action(action_id::REBASE)
            .is_some_and(|a| a.has_option(REBASE_ON_BEHALF_OF_UPLOADER))
    }

    fn run(&mut self, kind: Kind, args: Args, _: &mut Window, cx: &mut Context<Self>) {
        if self.running() {
            return;
        }
        let from = self.step;
        let syncer = self.session.syncer().clone();
        let change = self.number;
        let revision = self.detail.current_revision.clone();
        let rebase = RebaseOptions {
            on_behalf_of_uploader: self.rebase.on_behalf_of_uploader && self.offers_behalf(),
            ..self.rebase
        };
        self.step = Step::Running(kind);
        self.error = None;
        self.set_fields_disabled(true, cx);
        cx.notify();
        self.task = Some(cx.spawn(async move |this, cx| {
            let (result, fresh) = cx
                .background_spawn(async move {
                    let result = kind.send(&syncer, change, revision, args, rebase).await;
                    // A refusal means our picture of the change is stale.
                    let fresh = match &result {
                        Err(e) => syncer.refresh_after_refusal(change, e).await,
                        Ok(_) => None,
                    };
                    (result, fresh)
                })
                .await;
            this.update(cx, |this, cx| this.finished(kind, from, result, fresh, cx))
                .ok();
        }));
    }

    fn finished(
        &mut self,
        kind: Kind,
        from: Step,
        result: Result<(Option<ChangeDetail>, Option<ChangeNumber>), sync::Error>,
        fresh: Option<ChangeDetail>,
        cx: &mut Context<Self>,
    ) {
        self.set_fields_disabled(false, cx);
        match result {
            Ok((detail, opened)) => cx.emit(ActionEvent::Done(Box::new(Done {
                kind,
                detail,
                opened,
            }))),
            Err(e) if is_unauthorized(&e) => cx.emit(ActionEvent::Unauthorized),
            Err(e) => {
                self.step = from;
                self.error = Some(describe(kind, &e));
                self.take_fresh(fresh, cx);
                if let Step::Confirm(_) = from {
                    self.pending_focus = Some(self.confirm_focus.clone());
                }
            }
        }
        cx.notify();
    }

    /// The server refused something and we fetched the change again: show
    /// the menu (what is offered, with the reasons) and the reviewers list
    /// from that, and tell the page.
    pub(super) fn take_fresh(&mut self, fresh: Option<ChangeDetail>, cx: &mut Context<Self>) {
        let Some(detail) = fresh else { return };
        let kind = self.entries.get(self.selected).map(|e| e.kind);
        self.entries = menu_entries(&detail);
        self.selected = kind
            .and_then(|k| self.entries.iter().position(|e| e.kind == k))
            .unwrap_or(0);
        self.detail = detail.clone();
        self.sync_rows(cx);
        cx.emit(ActionEvent::Refreshed(Box::new(detail)));
    }

    // ---- test access ----

    #[cfg(test)]
    pub(crate) fn selected_for_test(&self) -> usize {
        self.selected
    }

    #[cfg(test)]
    pub(crate) fn step_for_test(&self) -> String {
        format!("{:?}", self.step)
    }

    #[cfg(test)]
    pub(crate) fn error_for_test(&self) -> Option<&str> {
        self.error.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn notice_for_test(&self) -> Option<String> {
        self.rev.notice.clone()
    }

    #[cfg(test)]
    pub(crate) fn suggestions_for_test(&self) -> Vec<String> {
        self.rev
            .suggestions
            .iter()
            .map(|s| s.reviewer_input())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn suggested_for_test(
        &mut self,
        query: &str,
        list: Vec<model::SuggestedReviewer>,
        cx: &mut Context<Self>,
    ) {
        self.suggested(query, Ok(list), cx);
    }

    #[cfg(test)]
    pub(crate) fn detail_for_test(&self) -> &ChangeDetail {
        &self.detail
    }

    #[cfg(test)]
    pub(crate) fn focused_row_for_test(&self, window: &Window) -> Option<usize> {
        self.rev.rows.iter().position(|h| h.is_focused(window))
    }

    #[cfg(test)]
    pub(crate) fn message_focus_for_test(&self, cx: &App) -> FocusHandle {
        self.message.read(cx).focus_handle(cx)
    }

    #[cfg(test)]
    pub(crate) fn base_focus_for_test(&self, cx: &App) -> FocusHandle {
        self.base.read(cx).focus_handle(cx)
    }

    #[cfg(test)]
    pub(crate) fn commit_message_for_test(&self, cx: &App) -> String {
        self.commit_message.read(cx).value().to_string()
    }

    #[cfg(test)]
    pub(crate) fn rebase_options_for_test(&self) -> RebaseOptions {
        self.rebase
    }

    #[cfg(test)]
    pub(crate) fn confirm_focus_for_test(&self) -> FocusHandle {
        self.confirm_focus.clone()
    }
}

/// A failed action, in words for the dialog.
fn describe(kind: Kind, e: &sync::Error) -> String {
    use gerrit_api::Error as Api;
    let not = kind.not_done();
    match e {
        sync::Error::Api(api) => match api {
            Api::Forbidden { message } => refused(not, "not allowed (403)", message),
            // 409 messages are meant for the user: "Change is already up
            // to date.", "... could not be rebased due to a conflict ...".
            Api::Conflict { message } => refused(not, "in conflict (409)", message),
            Api::BadRequest { message } => refused(not, "rejected (400)", message),
            api if api.is_retryable() => format!(
                "{not}: the server cannot be reached ({api}). You may be offline. \
                 Nothing was changed; try again when the connection is back."
            ),
            api => format!("{not}: {api}"),
        },
        other => format!("{not}: {other}"),
    }
}

fn refused(not: &str, what: &str, message: &str) -> String {
    if message.trim().is_empty() {
        format!("{not}: the server says it is {what}.")
    } else {
        format!("{not}: {}", message.trim())
    }
}

// ---- rendering ----

impl ActionDialog {
    /// The submit requirements, one line each, for the submit confirmation.
    fn requirements(&self, cx: &App) -> Option<AnyElement> {
        let reqs = &self.detail.submit_requirements;
        if reqs.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let rows: Vec<_> = reqs
            .iter()
            .filter(|r| r.status != SubmitRequirementStatus::NotApplicable)
            .map(|r| {
                let ok = r.status.is_ok();
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .w_4()
                            .flex_none()
                            .text_color(if ok { theme.success } else { theme.danger })
                            .child(if ok { "\u{2713}" } else { "\u{2717}" }),
                    )
                    .child(div().min_w_0().truncate().child(r.name.clone()))
                    .into_any_element()
            })
            .collect();
        (!rows.is_empty()).then(|| {
            v_flex()
                .gap_0p5()
                .pt_2()
                .child(
                    div()
                        .text_xs()
                        .font_semibold()
                        .text_color(theme.muted_foreground)
                        .child("SUBMIT REQUIREMENTS"),
                )
                .children(rows)
                .into_any_element()
        })
    }

    fn render_menu(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let _ = window;
        let theme = cx.theme().clone();
        let rows: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .map(|(ix, e)| {
                let selected = ix == self.selected;
                let kind = e.kind;
                let hover = theme.list_hover;
                h_flex()
                    .id(("action-entry", ix))
                    .gap_3()
                    .px_3()
                    .py_1p5()
                    .items_start()
                    .rounded_md()
                    .when(selected, |d| d.bg(theme.list_active))
                    .when(e.enabled, |d| {
                        d.cursor_pointer().hover(move |d| d.bg(hover))
                    })
                    .when(!e.enabled, |d| d.opacity(0.5))
                    .child(
                        div()
                            .w_5()
                            .flex_none()
                            .font_semibold()
                            .text_color(theme.muted_foreground)
                            .child(kind.mnemonic().to_ascii_uppercase().to_string()),
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .flex_1()
                            .child(div().child(e.label.clone()))
                            .when_some(e.reason.clone().filter(|_| !e.enabled), |d, reason| {
                                d.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(reason),
                                )
                            }),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.pick_at(ix, window, cx);
                    }))
                    .into_any_element()
            })
            .collect();
        v_flex()
            .gap_0p5()
            .p_2()
            .children(rows)
            .when(self.entries.is_empty(), |d| {
                d.child(
                    div()
                        .p_2()
                        .text_color(theme.muted_foreground)
                        .child("Nothing to do with this change."),
                )
            })
            .into_any_element()
    }

    /// A checkbox row that Tab reaches and Space switches.
    fn option_row(
        &self,
        flag: RebaseFlag,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (id, label, handle, checked) = match flag {
            RebaseFlag::Conflicts => (
                "rebase-conflicts",
                "Allow conflicts",
                &self.conflicts_focus,
                self.rebase.allow_conflicts,
            ),
            RebaseFlag::Behalf => (
                "rebase-behalf",
                "On behalf of the uploader",
                &self.behalf_focus,
                self.rebase.on_behalf_of_uploader,
            ),
        };
        let ring = if handle.is_focused(window) {
            cx.theme().ring
        } else {
            cx.theme().transparent
        };
        h_flex()
            .id(id)
            .key_context(OPTION_ROW)
            .track_focus(handle)
            .on_action(cx.listener(move |this, _: &ToggleOption, _, cx| {
                this.toggle_flag(flag);
                cx.notify();
            }))
            .px_1()
            .rounded_md()
            .border_1()
            .border_color(ring)
            .child(
                Checkbox::new(id)
                    .label(label)
                    .checked(checked)
                    .tab_stop(false)
                    .on_click(cx.listener(move |this, _: &bool, _, cx| {
                        this.toggle_flag(flag);
                        cx.notify();
                    })),
            )
    }

    fn toggle_flag(&mut self, flag: RebaseFlag) {
        match flag {
            RebaseFlag::Conflicts => self.rebase.allow_conflicts = !self.rebase.allow_conflicts,
            RebaseFlag::Behalf => {
                self.rebase.on_behalf_of_uploader = !self.rebase.on_behalf_of_uploader
            }
        }
    }

    fn render_confirm(&self, kind: Kind, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let intro = match kind {
            Kind::Submit => "Submit this change to its branch?",
            Kind::Rebase => "Rebase this change onto the tip of its branch, or onto a base.",
            Kind::Abandon => "Abandon this change?",
            Kind::Restore => "Restore this abandoned change?",
            Kind::Wip => "Mark this change work in progress?",
            Kind::Revert => "Create a new change that reverts this one. Its commit message:",
            Kind::CherryPick => "Copy this change onto another branch as a new change.",
            _ => "",
        };
        v_flex()
            .gap_1()
            .px_4()
            .py_2()
            .child(div().child(intro))
            .when(kind == Kind::Submit, |d| {
                d.children(self.requirements(cx))
                    .when(self.detail.summary.work_in_progress, |d| {
                        d.child(
                            div()
                                .pt_2()
                                .text_xs()
                                .text_color(theme.warning)
                                .child("The change is work in progress."),
                        )
                    })
            })
            .when(kind.takes_message(), |d| {
                d.child(Textarea::new(&self.message).small())
            })
            .when(kind == Kind::CherryPick, |d| {
                d.child(Input::new(&self.destination).small()).child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!(
                            "This change is on {}. The commit message of the new change:",
                            self.detail.summary.branch
                        )),
                )
            })
            .when(matches!(kind, Kind::Revert | Kind::CherryPick), |d| {
                d.child(Textarea::new(&self.commit_message).small())
            })
            .when(kind == Kind::Rebase, |d| {
                d.child(Input::new(&self.base).small())
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Leave the base empty to rebase onto the branch tip."),
                    )
                    .child(self.option_row(RebaseFlag::Conflicts, window, cx))
                    .child(
                        div()
                            .pl_6()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(
                                "A rebase that conflicts then still creates a patch set, \
                             with conflict markers in the files, instead of failing.",
                            ),
                    )
                    .when(self.offers_behalf(), |d| {
                        d.child(self.option_row(RebaseFlag::Behalf, window, cx))
                    })
            })
            .into_any_element()
    }
}

impl Render for ActionDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.reviewers_frame(window, cx);
        if let Some(handle) = self.pending_focus.take() {
            window.defer(cx, move |window, cx| window.focus(&handle, cx));
        }
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let subject = self.detail.summary.subject.clone();
        let step = self.step;
        let (title, confirm_label) = match step {
            Step::Menu => ("Change actions".to_owned(), None),
            Step::Reviewers => ("Reviewers and CC".to_owned(), None),
            Step::Topic => ("Topic".to_owned(), None),
            Step::Hashtags => ("Hashtags".to_owned(), None),
            Step::Confirm(k) | Step::Running(k) => (
                self.entries
                    .iter()
                    .find(|e| e.kind == k)
                    .map_or_else(|| k.label().to_owned(), |e| e.label.clone()),
                Some(k),
            ),
        };
        let running = self.running();
        let confirm_hint = if cfg!(target_os = "macos") {
            "Cmd-Enter confirms, Esc goes back"
        } else {
            "Ctrl-Enter confirms, Esc goes back"
        };
        let body = match step {
            Step::Menu => self.render_menu(window, cx),
            Step::Reviewers => self.render_reviewers(window, cx),
            Step::Topic => self.render_topic(),
            Step::Hashtags => self.render_hashtags(window, cx),
            Step::Confirm(k) | Step::Running(k) => self.render_confirm(k, window, cx),
        };
        let save_label = match step {
            Step::Topic => Some("Save topic"),
            Step::Hashtags => Some("Apply"),
            _ => None,
        };
        let footer = match (step, confirm_label) {
            (Step::Reviewers, _) => h_flex()
                .gap_2()
                .pt_1()
                .items_center()
                .child(div().flex_1().min_w_0().text_xs().text_color(muted).child(
                    "Enter adds a reviewer, Shift-Enter a CC, Up/Down choose. \
                     Tab to the list: Enter moves, Delete removes. Esc goes back",
                ))
                .child(
                    chip(
                        "action-close",
                        "Close",
                        &self.cancel_focus,
                        Look::Plain,
                        running,
                        window,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                ),
            (Step::Topic | Step::Hashtags, _) => h_flex()
                .gap_2()
                .pt_1()
                .items_center()
                .child(div().flex_1().min_w_0().text_xs().text_color(muted).child(
                    if step == Step::Hashtags {
                        "Enter applies, Tab to a hashtag and Delete removes it, Esc goes back"
                    } else {
                        "Enter saves, Esc goes back"
                    },
                ))
                .child(
                    chip(
                        "action-cancel",
                        "Cancel",
                        &self.cancel_focus,
                        Look::Plain,
                        running,
                        window,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.back(cx))),
                )
                .child(
                    chip(
                        "action-confirm",
                        if running {
                            "Working\u{2026}"
                        } else {
                            save_label.unwrap_or("Save")
                        },
                        &self.confirm_focus,
                        Look::Primary,
                        running,
                        window,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
                ),
            (Step::Menu, _) => h_flex().pt_1().child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("J/K move, Enter or the letter runs, Esc closes"),
            ),
            (step, Some(kind)) => h_flex()
                .gap_2()
                .pt_1()
                .items_center()
                .child(div().flex_1().min_w_0().text_xs().text_color(muted).child(
                    if matches!(step, Step::Running(_)) {
                        "Working\u{2026}"
                    } else {
                        confirm_hint
                    },
                ))
                .child(
                    chip(
                        "action-cancel",
                        "Cancel",
                        &self.cancel_focus,
                        Look::Plain,
                        running,
                        window,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.back(cx))),
                )
                .child(
                    chip(
                        "action-confirm",
                        if running {
                            "Working\u{2026}".to_owned()
                        } else {
                            self.entries
                                .iter()
                                .find(|e| e.kind == kind)
                                .map_or_else(|| kind.label().to_owned(), |e| e.label.clone())
                        },
                        &self.confirm_focus,
                        Look::Primary,
                        running,
                        window,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
                ),
            _ => h_flex(),
        };
        // Direct actions (ready, private) show "Working" in the menu.
        let working = running && !matches!(step, Step::Running(k) if k.confirms());
        let key_context = match step {
            Step::Menu => format!("{ACTION_DIALOG} {ACTION_MENU}"),
            Step::Reviewers => format!("{ACTION_DIALOG} {REVIEWERS_STEP}"),
            _ => ACTION_DIALOG.to_owned(),
        };

        div()
            .id("action-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.overlay)
            .child(
                v_flex()
                    .id("action-dialog")
                    .key_context(key_context.as_str())
                    .track_focus(&self.focus)
                    .on_action(cx.listener(Self::close_action))
                    .on_action(cx.listener(Self::confirm_action))
                    .on_action(cx.listener(Self::focus_next))
                    .on_action(cx.listener(Self::focus_prev))
                    .on_action(cx.listener(Self::down))
                    .on_action(cx.listener(Self::up))
                    .on_action(cx.listener(Self::run_selected))
                    .on_action(cx.listener(Self::pick_submit))
                    .on_action(cx.listener(Self::pick_rebase))
                    .on_action(cx.listener(Self::pick_abandon))
                    .on_action(cx.listener(Self::pick_restore))
                    .on_action(cx.listener(Self::pick_wip))
                    .on_action(cx.listener(Self::pick_private))
                    .on_action(cx.listener(Self::pick_revert))
                    .on_action(cx.listener(Self::pick_cherry_pick))
                    .on_action(cx.listener(Self::pick_reviewers))
                    .on_action(cx.listener(Self::pick_topic))
                    .on_action(cx.listener(Self::pick_hashtags))
                    .on_action(cx.listener(Self::suggest_down))
                    .on_action(cx.listener(Self::suggest_up))
                    .w(px(520.))
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
                            .child(div().text_base().font_semibold().child(title))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .truncate()
                                    .child(format!("#{} {subject}", self.number)),
                            ),
                    )
                    .child(body)
                    .child(
                        v_flex()
                            .px_4()
                            .pb_3()
                            .when(working, |d| {
                                d.child(
                                    div()
                                        .pt_1()
                                        .text_xs()
                                        .text_color(muted)
                                        .child("Working\u{2026}"),
                                )
                            })
                            .when_some(self.error.clone(), |d, e| {
                                d.child(reviewers::lines(&e, theme.danger).pt_2())
                            })
                            .child(footer),
                    ),
            )
    }
}
