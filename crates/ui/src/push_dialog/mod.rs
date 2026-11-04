//! The push dialog: sends a local version to Gerrit for review, opened
//! from the change page's Push button or `l u` when a change's newest
//! version is a local one.
//!
//! The app never writes git configuration and never makes up a refspec:
//! the dialog offers the push rules of the checkout's remotes that push
//! `HEAD` ([`remotes`]), the most Gerrit-like first, and pushes the commit
//! with the chosen rule, `git push <remote> <sha><rule after HEAD>`
//! ([`local_git::push`]). Remotes without such a rule are named, with how
//! to add one. Under the choice it lists the commits the push sends (those
//! no remote-tracking branch of the remote has) and the command.
//!
//! A push that went through closes the dialog with
//! [`PushEvent::Pushed`]; the shell then asks for a consolidation run,
//! which finds the commits on Gerrit and sends the local reviews. A push
//! git or Gerrit refused keeps the dialog open with git's output. `Up` /
//! `Down` choose, `Enter` pushes, `Esc` closes.

pub mod actions;
pub mod remotes;

use std::time::Duration;

use gpui_kit::component::{ActiveTheme as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use local_git::{
    Repo,
    push::{Outgoing, PushOutcome},
};
use model::RevisionId;

use crate::{
    checkouts::Checkouts,
    reply::{Look, chip},
};
use actions::*;
use remotes::Choice;

/// How long git may take for a push, after which it is stopped: a remote
/// that does not answer, or a passphrase prompt nobody sees.
const PUSH_TIMEOUT: Duration = Duration::from_secs(180);

/// How many of the commits a push sends are listed.
const OUTGOING_LIMIT: usize = 20;

/// What the dialog tells the shell.
pub enum PushEvent {
    /// Closed without pushing.
    Close,
    /// The push went through; the note says what Gerrit made of it.
    Pushed(String),
}

/// What to push, from the change page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushRequest {
    pub project: String,
    /// The commit of the local version.
    pub sha: RevisionId,
    /// How the page names the version: `local 2`.
    pub version: String,
    pub subject: String,
    /// The change's target branch, if known.
    pub branch: Option<String>,
}

enum Load {
    Reading,
    Ready,
    Failed(String),
}

pub struct PushDialog {
    request: PushRequest,
    server_url: String,
    repo: Option<Repo>,
    load: Load,
    choices: Vec<Choice>,
    /// Remotes without a push rule for `HEAD`.
    without: Vec<String>,
    selected: usize,
    /// The commits a push to `outgoing.0` sends.
    outgoing: Option<(String, Result<Outgoing, String>)>,
    /// What git said about the last push, when it did not go through.
    failure: Option<String>,
    focus: FocusHandle,
    cancel_focus: FocusHandle,
    push_focus: FocusHandle,
    load_task: Option<Task<()>>,
    outgoing_task: Option<Task<()>>,
    push_task: Option<Task<()>>,
}

impl EventEmitter<PushEvent> for PushDialog {}

impl Focusable for PushDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl PushDialog {
    /// Opens the dialog and reads the remotes of the project's checkout.
    pub fn new(
        request: PushRequest,
        checkouts: Entity<Checkouts>,
        server_url: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let project = request.project.clone();
        let load_task = cx.spawn(async move |this, cx| {
            let checkout = Checkouts::opened(&checkouts, &project, cx).await;
            let repo = match checkout {
                None => Err(format!("{project} has no local checkout.")),
                Some(c) => match (c.repo(), c.error()) {
                    (Some(repo), _) => Ok(repo.clone()),
                    (None, Some(e)) => Err(format!("The checkout cannot be used: {e}.")),
                    (None, None) => Err("The checkout is not open.".to_owned()),
                },
            };
            let read = match repo {
                Ok(repo) => {
                    cx.background_spawn(async move {
                        let remotes = repo.remotes();
                        remotes.map(|r| (repo, r)).map_err(|e| e.to_string())
                    })
                    .await
                }
                Err(e) => Err(e),
            };
            this.update(cx, |this, cx| this.loaded(read, cx)).ok();
        });
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self {
            request,
            server_url,
            repo: None,
            load: Load::Reading,
            choices: Vec::new(),
            without: Vec::new(),
            selected: 0,
            outgoing: None,
            failure: None,
            focus,
            cancel_focus: cx.focus_handle(),
            push_focus: cx.focus_handle(),
            load_task: Some(load_task),
            outgoing_task: None,
            push_task: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn choices_for_test(&self) -> (&[Choice], &[String], usize) {
        (&self.choices, &self.without, self.selected)
    }

    #[cfg(test)]
    pub(crate) fn failure_for_test(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn outgoing_for_test(&self) -> Option<&Outgoing> {
        self.outgoing.as_ref().and_then(|(_, o)| o.as_ref().ok())
    }

    fn loaded(
        &mut self,
        read: Result<(Repo, Vec<local_git::push::Remote>), String>,
        cx: &mut Context<Self>,
    ) {
        self.load_task = None;
        match read {
            Ok((repo, remotes)) => {
                let r = &self.request;
                (self.choices, self.without) = remotes::choices(
                    &remotes,
                    &r.sha,
                    &self.server_url,
                    &r.project,
                    r.branch.as_deref(),
                );
                self.repo = Some(repo);
                self.load = Load::Ready;
                self.load_outgoing(cx);
            }
            Err(e) => self.load = Load::Failed(e),
        }
        cx.notify();
    }

    /// Lists what a push to the chosen remote sends, unless known.
    fn load_outgoing(&mut self, cx: &mut Context<Self>) {
        let (Some(repo), Some(choice)) = (self.repo.clone(), self.choices.get(self.selected))
        else {
            return;
        };
        let remote = choice.remote.clone();
        if self.outgoing.as_ref().is_some_and(|(r, _)| *r == remote) {
            return;
        }
        let sha = self.request.sha.clone();
        self.outgoing_task = Some(cx.spawn(async move |this, cx| {
            let read = {
                let remote = remote.clone();
                cx.background_spawn(async move {
                    repo.outgoing(&sha, &remote, OUTGOING_LIMIT)
                        .map_err(|e| e.to_string())
                })
                .await
            };
            this.update(cx, |this, cx| {
                this.outgoing = Some((remote, read));
                cx.notify();
            })
            .ok();
        }));
    }

    fn pushing(&self) -> bool {
        self.push_task.is_some()
    }

    // ---- keys ----

    fn close(&mut self, _: &ClosePush, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PushEvent::Close);
    }

    fn push_action(&mut self, _: &Push, _: &mut Window, cx: &mut Context<Self>) {
        self.push(cx);
    }

    fn prev(&mut self, _: &PushChoicePrev, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected.saturating_sub(1), cx);
    }

    fn next(&mut self, _: &PushChoiceNext, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected + 1, cx);
    }

    fn select(&mut self, at: usize, cx: &mut Context<Self>) {
        if self.pushing() || self.choices.is_empty() {
            return;
        }
        self.selected = at.min(self.choices.len() - 1);
        self.load_outgoing(cx);
        cx.notify();
    }

    // ---- pushing ----

    /// Pushes with the chosen rule.
    pub(crate) fn push(&mut self, cx: &mut Context<Self>) {
        if self.pushing() {
            return;
        }
        let (Some(repo), Some(choice)) = (self.repo.clone(), self.choices.get(self.selected))
        else {
            return;
        };
        let (remote, rule) = (choice.remote.clone(), choice.rule.clone());
        let sha = self.request.sha.clone();
        self.failure = None;
        self.push_task = Some(cx.spawn(async move |this, cx| {
            let pushed = {
                let remote = remote.clone();
                cx.background_spawn(async move {
                    repo.push(&remote, &sha, &rule, PUSH_TIMEOUT)
                        .map_err(|e| e.to_string())
                })
                .await
            };
            this.update(cx, |this, cx| this.pushed(&remote, pushed, cx))
                .ok();
        }));
        cx.notify();
    }

    fn pushed(
        &mut self,
        remote: &str,
        pushed: Result<PushOutcome, String>,
        cx: &mut Context<Self>,
    ) {
        self.push_task = None;
        let version = &self.request.version;
        match pushed {
            Ok(o) if o.success => {
                let note = match o.changes.len() {
                    0 => format!("Pushed {version} to {remote}."),
                    1 => format!("Pushed {version} to {remote}: 1 change on Gerrit."),
                    n => format!("Pushed {version} to {remote}: {n} changes on Gerrit."),
                };
                cx.emit(PushEvent::Pushed(note));
            }
            // Gerrit has every commit: nothing to do but send the reviews.
            Ok(o) if o.no_new_changes => cx.emit(PushEvent::Pushed(format!(
                "Gerrit has {version} already (no new changes)."
            ))),
            Ok(o) if o.output.is_empty() => self.failure = Some("git push failed.".to_owned()),
            Ok(o) => self.failure = Some(o.output),
            Err(e) => self.failure = Some(e),
        }
        cx.notify();
    }

    // ---- rendering ----

    fn label(&self, text: impl Into<SharedString>, cx: &App) -> Div {
        div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(text.into())
    }

    fn mono(&self, cx: &App) -> Div {
        div().font_family(cx.theme().mono_font_family.clone())
    }

    /// One push rule to choose: a round mark, the remote and the rule, the
    /// URL and why it ranks where it does.
    fn render_choice(&self, i: usize, c: &Choice, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = cx.theme().clone();
        let on = i == self.selected;
        let wrong_branch = match (&c.branch, &self.request.branch) {
            (Some(rule), Some(change)) if rule != change => {
                Some(format!("pushes to {rule}; the change is for {change}"))
            }
            _ => None,
        };
        h_flex()
            .id(("push-choice", i))
            .gap_2()
            .items_start()
            .px_2()
            .py_1()
            .rounded_sm()
            .when(on, |d| d.bg(theme.accent))
            .cursor_pointer()
            .hover(|d| d.bg(theme.list_hover))
            .on_click(cx.listener(move |this, _, _, cx| this.select(i, cx)))
            .child(
                div()
                    .mt_1()
                    .size_3p5()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .border_1()
                    .border_color(if on { theme.primary } else { theme.border })
                    .when(on, |d| {
                        d.child(div().size_2().rounded_full().bg(theme.primary))
                    }),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().font_semibold().child(c.remote.clone()))
                            .child(self.mono(cx).child(c.rule.clone()))
                            .when(i == 0 && c.score > 0, |d| {
                                d.child(
                                    div()
                                        .px_1()
                                        .rounded_sm()
                                        .text_xs()
                                        .text_color(theme.primary)
                                        .border_1()
                                        .border_color(theme.primary)
                                        .child("suggested"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(c.push_url.clone()),
                    )
                    .when(!c.reasons.is_empty(), |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(c.reasons.join(" \u{00b7} ")),
                        )
                    })
                    .children(
                        wrong_branch.map(|w| div().text_xs().text_color(theme.warning).child(w)),
                    ),
            )
    }

    /// The commits the chosen push sends, and the command.
    fn render_preview(&self, cx: &App) -> Option<Div> {
        let theme = cx.theme();
        let choice = self.choices.get(self.selected)?;
        let short = self.request.sha.short(12);
        let rest = choice.rule.split_once("HEAD").map_or("", |(_, r)| r);
        let force = if choice.rule.starts_with('+') {
            "+"
        } else {
            ""
        };
        let command = format!("git push {} {force}{short}{rest}", choice.remote);
        let commits = match self.outgoing.as_ref().filter(|(r, _)| *r == choice.remote) {
            None => self.label("Reading the commits\u{2026}", cx),
            Some((_, Err(e))) => div().text_xs().text_color(theme.danger).child(e.clone()),
            Some((_, Ok(o))) if o.commits.is_empty() => {
                self.label("The remote's branches have this commit already.", cx)
            }
            Some((_, Ok(o))) => {
                let n = o.commits.len();
                let head = match (n, o.more) {
                    (1, false) => "Sends 1 commit:".to_owned(),
                    (n, false) => format!("Sends {n} commits:"),
                    (n, true) => format!("Sends more than {n} commits, newest first:"),
                };
                v_flex()
                    .gap_0p5()
                    .child(self.label(head, cx))
                    .children(o.commits.iter().map(|c| {
                        h_flex()
                            .gap_2()
                            .text_xs()
                            .child(
                                self.mono(cx)
                                    .text_color(theme.muted_foreground)
                                    .child(c.sha.short(8).to_owned()),
                            )
                            .child(div().min_w_0().truncate().child(c.subject.clone()))
                    }))
            }
        };
        Some(
            v_flex().gap_2().child(commits).child(
                v_flex()
                    .gap_0p5()
                    .child(self.label("Runs in the checkout:", cx))
                    .child(self.mono(cx).text_xs().child(command)),
            ),
        )
    }

    fn render_body(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let r = &self.request;
        let body = v_flex().gap_3().p_4().child(
            div()
                .text_color(theme.muted_foreground)
                .child(format!("{} \u{2014} {}", r.version, r.subject)),
        );
        match &self.load {
            Load::Reading => body.child(self.label("Reading the remotes\u{2026}", cx)),
            Load::Failed(e) => body.child(div().text_color(theme.danger).child(e.clone())),
            Load::Ready => {
                let choices: Vec<_> = self
                    .choices
                    .iter()
                    .enumerate()
                    .map(|(i, c)| self.render_choice(i, c, cx))
                    .collect();
                let without = (!self.without.is_empty()).then(|| {
                    self.label(
                        format!(
                            "Not offered, no push rule for HEAD: {}. Add one with \
                             git config remote.<name>.push HEAD:refs/for/<branch>",
                            self.without.join(", ")
                        ),
                        cx,
                    )
                });
                let none = self.choices.is_empty().then(|| {
                    div().child(
                        "No remote of the checkout has a push rule for HEAD, such as \
                         HEAD:refs/for/main. The app pushes only by a rule you configured.",
                    )
                });
                body.child(
                    v_flex()
                        .gap_1()
                        .child(self.label("Remote and push rule", cx))
                        .child(v_flex().gap_0p5().children(choices)),
                )
                .children(none)
                .children(without)
                .children(self.render_preview(cx))
                .children(self.failure.as_ref().map(|f| {
                    v_flex()
                        .gap_1()
                        .child(div().text_color(theme.danger).child("The push failed:"))
                        .child(
                            div()
                                .id("push-output")
                                .max_h(px(160.))
                                .overflow_y_scroll()
                                .p_2()
                                .rounded_md()
                                .border_1()
                                .border_color(theme.border)
                                .text_xs()
                                .font_family(theme.mono_font_family.clone())
                                .child(f.clone()),
                        )
                }))
            }
        }
    }
}

impl Render for PushDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let can_push = !self.choices.is_empty() && !self.pushing();
        let footer = h_flex()
            .px_4()
            .py_2()
            .gap_2()
            .items_center()
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Up / Down choose \u{00b7} Enter pushes \u{00b7} Esc closes"),
            )
            .child(
                chip(
                    "push-cancel",
                    "Cancel",
                    &self.cancel_focus,
                    Look::Plain,
                    false,
                    window,
                    cx,
                )
                .on_click(cx.listener(|this, _, w, cx| this.close(&ClosePush, w, cx))),
            )
            .child(
                chip(
                    "push-go",
                    if self.pushing() {
                        "Pushing\u{2026}"
                    } else {
                        "Push"
                    },
                    &self.push_focus,
                    Look::Primary,
                    !can_push,
                    window,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| this.push(cx))),
            );
        let body = self.render_body(cx);
        div()
            .id("push-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(crate::theme::modal_backdrop(cx))
            .child(
                v_flex()
                    .id("push-dialog")
                    .key_context(PUSH_DIALOG)
                    .track_focus(&self.focus)
                    .on_action(cx.listener(Self::close))
                    .on_action(cx.listener(Self::push_action))
                    .on_action(cx.listener(Self::prev))
                    .on_action(cx.listener(Self::next))
                    .w(px(600.))
                    .max_w_full()
                    .max_h_full()
                    .my_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .text_color(theme.popover_foreground)
                    .text_sm()
                    .overflow_hidden()
                    .child(
                        h_flex()
                            .px_4()
                            .py_2()
                            .items_center()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(div().text_base().font_semibold().child("Push for review")),
                    )
                    .child(body)
                    .child(footer),
            )
    }
}

#[cfg(test)]
mod tests;
