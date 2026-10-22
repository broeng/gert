//! The change action menu (`m`): submit, rebase, abandon, restore, work in
//! progress / ready for review and private, each as a second step with a
//! confirmation where it needs one.
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
//!    `b` rebase, `a` abandon, `r` restore, `w` WIP / ready, `p` private)
//!    picks. The mnemonics work only here.
//! 2. A confirmation for submit, rebase, abandon, restore and WIP, with an
//!    optional message (abandon, restore, WIP) or base (rebase). `Ctrl-Enter`
//!    (`Cmd-Enter`) confirms, `Esc` cancels. Ready and the private toggles
//!    run right from the menu.
//! 3. Running: the request goes out on the background executor and `Esc`
//!    does not cancel it. A refused action (403, 409, ...) goes back to the
//!    step it came from with the server's message; 401 goes to the login
//!    flow; nothing is queued, an action needs a connection.
//!
//! The data layer (`Syncer::submit` and friends) fetches the change again
//! after the server accepted an action; the page takes that detail.

pub mod actions;
#[cfg(test)]
mod tests;

use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use model::{ChangeDetail, ChangeNumber, SubmitRequirementStatus, action_id};

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
}

/// The outcome of an action the server accepted.
pub struct Done {
    pub kind: Kind,
    /// The change as the server has it now; `None` if only fetching it
    /// failed (the action went through).
    pub detail: Option<ChangeDetail>,
}

impl Done {
    /// The note for the page.
    pub fn note(&self) -> String {
        self.kind.past().to_owned()
    }
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
}

impl Kind {
    const ALL: [Kind; 8] = [
        Kind::Submit,
        Kind::Rebase,
        Kind::Abandon,
        Kind::Restore,
        Kind::Wip,
        Kind::Ready,
        Kind::Private,
        Kind::Unprivate,
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
        }
    }

    /// Whether a confirmation comes first. Ready and the private toggles
    /// are cheap to undo and run from the menu.
    fn confirms(self) -> bool {
        !matches!(self, Kind::Ready | Kind::Private | Kind::Unprivate)
    }

    /// Whether the confirmation has a message field.
    fn takes_message(self) -> bool {
        matches!(self, Kind::Abandon | Kind::Restore | Kind::Wip)
    }

    async fn send(
        self,
        syncer: &sync::Syncer,
        change: ChangeNumber,
        message: Option<&str>,
        base: Option<&str>,
    ) -> Result<Option<ChangeDetail>, sync::Error> {
        match self {
            Kind::Submit => syncer.submit(change).await,
            Kind::Rebase => syncer.rebase(change, base).await,
            Kind::Abandon => syncer.abandon(change, message).await,
            Kind::Restore => syncer.restore(change, message).await,
            Kind::Wip => syncer.set_work_in_progress(change, true, message).await,
            Kind::Ready => syncer.set_work_in_progress(change, false, None).await,
            Kind::Private => syncer.set_private(change, true).await,
            Kind::Unprivate => syncer.set_private(change, false).await,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Menu,
    Confirm(Kind),
    Running(Kind),
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
        let subs = vec![
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
        matches!(self.step, Step::Running(_))
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
        if kind.confirms() {
            self.step = Step::Confirm(kind);
            let target = match kind {
                k if k.takes_message() => self.message.read(cx).focus_handle(cx),
                Kind::Rebase => self.base.read(cx).focus_handle(cx),
                _ => self.confirm_focus.clone(),
            };
            self.pending_focus = Some(target);
            cx.notify();
        } else {
            self.run(kind, None, None, window, cx);
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
    fn pick_private(&mut self, _: &PickPrivate, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_key('p', window, cx);
    }

    // ---- closing, focus ----

    fn close_action(&mut self, _: &CloseMenu, _: &mut Window, cx: &mut Context<Self>) {
        self.close(cx);
    }

    /// Closes the dialog. Not while an action runs: the request is under
    /// way and cannot be taken back.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if !self.running() {
            cx.emit(ActionEvent::Close);
        }
    }

    fn tab_order(&self, cx: &App) -> Vec<FocusHandle> {
        let Step::Confirm(kind) = self.step else {
            return Vec::new();
        };
        let mut v = Vec::new();
        if kind.takes_message() {
            v.push(self.message.read(cx).focus_handle(cx));
        }
        if kind == Kind::Rebase {
            v.push(self.base.read(cx).focus_handle(cx));
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
        let Step::Confirm(kind) = self.step else {
            return;
        };
        let text = |s: &str| {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_owned())
        };
        let message = kind
            .takes_message()
            .then(|| text(self.message.read(cx).value().as_ref()))
            .flatten();
        let base = (kind == Kind::Rebase)
            .then(|| text(self.base.read(cx).value().as_ref()))
            .flatten();
        self.run(kind, message, base, window, cx);
    }

    fn run(
        &mut self,
        kind: Kind,
        message: Option<String>,
        base: Option<String>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.running() {
            return;
        }
        let from = self.step;
        let syncer = self.session.syncer().clone();
        let change = self.number;
        self.step = Step::Running(kind);
        self.error = None;
        self.message.update(cx, |i, cx| i.set_disabled(true, cx));
        self.base.update(cx, |i, cx| i.set_disabled(true, cx));
        cx.notify();
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    kind.send(&syncer, change, message.as_deref(), base.as_deref())
                        .await
                })
                .await;
            this.update(cx, |this, cx| this.finished(kind, from, result, cx))
                .ok();
        }));
    }

    fn finished(
        &mut self,
        kind: Kind,
        from: Step,
        result: Result<Option<ChangeDetail>, sync::Error>,
        cx: &mut Context<Self>,
    ) {
        self.message.update(cx, |i, cx| i.set_disabled(false, cx));
        self.base.update(cx, |i, cx| i.set_disabled(false, cx));
        match result {
            Ok(detail) => cx.emit(ActionEvent::Done(Box::new(Done { kind, detail }))),
            Err(e) if is_unauthorized(&e) => cx.emit(ActionEvent::Unauthorized),
            Err(e) => {
                self.step = from;
                self.error = Some(describe(kind, &e));
                if let Step::Confirm(_) = from {
                    self.pending_focus = Some(self.confirm_focus.clone());
                }
            }
        }
        cx.notify();
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
    pub(crate) fn message_focus_for_test(&self, cx: &App) -> FocusHandle {
        self.message.read(cx).focus_handle(cx)
    }

    #[cfg(test)]
    pub(crate) fn base_focus_for_test(&self, cx: &App) -> FocusHandle {
        self.base.read(cx).focus_handle(cx)
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

    fn render_confirm(&self, kind: Kind, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let intro = match kind {
            Kind::Submit => "Submit this change to its branch?",
            Kind::Rebase => "Rebase this change onto the tip of its branch, or onto a base.",
            Kind::Abandon => "Abandon this change?",
            Kind::Restore => "Restore this abandoned change?",
            Kind::Wip => "Mark this change work in progress?",
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
            .when(kind == Kind::Rebase, |d| {
                d.child(Input::new(&self.base).small()).child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Leave the base empty to rebase onto the branch tip."),
                )
            })
            .into_any_element()
    }
}

impl Render for ActionDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(handle) = self.pending_focus.take() {
            window.defer(cx, move |window, cx| window.focus(&handle, cx));
        }
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let subject = self.detail.summary.subject.clone();
        let step = self.step;
        let (title, confirm_label) = match step {
            Step::Menu => ("Change actions".to_owned(), None),
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
            "Cmd-Enter confirms, Esc cancels"
        } else {
            "Ctrl-Enter confirms, Esc cancels"
        };
        let body = match step {
            Step::Menu => self.render_menu(window, cx),
            Step::Confirm(k) | Step::Running(k) => self.render_confirm(k, cx),
        };
        let footer = match (step, confirm_label) {
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
                    .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
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
                                d.child(div().pt_2().text_xs().text_color(theme.danger).child(e))
                            })
                            .child(footer),
                    ),
            )
    }
}
