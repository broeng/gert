//! The notification rule editor: a modal form for one rule, opened from the
//! Notifications tab of the settings page (`Enter` or `a` there).
//!
//! The form has the name, the event, the scope (my changes, reviewer, CC,
//! attention set, starred, any, or one of the saved queries), the servers
//! (tick boxes, none ticked: all), for vote rules the labels and the values
//! as tick boxes, the actors (`from`), and the bots and enabled switches.
//! The fields an event does not use are not shown.
//!
//! The labels and values offered are what the cache has seen (see
//! [`store::Store::label_values`], read on the background executor when the
//! dialog opens, so the form is usable at once with the fallbacks of
//! [`catalog`]); a label or value the rule already has is always offered. A
//! rule has one list of values for all of its labels, so with several
//! labels ticked the values offered are the union of their ranges.
//!
//! `Tab` / `Shift-Tab` cycle the controls (the dialog does it itself so the
//! focus stays inside), `Left` / `Right` change the event or the scope,
//! `Space` ticks, `Ctrl-Enter` saves, `Esc` discards. Saving writes the whole
//! list through `Backend::save_notifications` on the background executor;
//! when it fails the dialog says so and keeps the form.

pub mod actions;
mod catalog;
mod form;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use store::ServerId;
use sync::notify::{NotificationSettings, Rule, RuleEvent, Scope};

use crate::{
    pages::settings::with_query,
    reply::{Look, check, chip},
    session::{Env, ServerKey},
};
use actions::*;
use form::{EVENTS, Form};

/// What the dialog tells the shell.
pub enum NotifyRuleEvent {
    /// The user closed it without saving.
    Close,
    /// The rule at `index` was written; `settings` is what is now in
    /// effect.
    Saved {
        settings: NotificationSettings,
        index: usize,
    },
}

/// What the dialog is opened from.
#[derive(Clone, Debug)]
pub struct NotifyRuleContext {
    /// The settings as they are now.
    pub settings: NotificationSettings,
    /// The rule to edit; one past the end is a new rule.
    pub index: usize,
    /// The configured servers with their names, for the server ticks. The
    /// field is offered when there are several.
    pub servers: Vec<(ServerKey, String)>,
    /// The cache ids of those servers, where the labels are looked up.
    pub server_ids: Vec<ServerId>,
    /// The names of the saved queries of all servers, for the scope.
    pub queries: Vec<String>,
}

pub struct NotifyRuleDialog {
    env: Env,
    settings: NotificationSettings,
    index: usize,
    /// The rule the form was loaded from.
    baseline: Rule,
    form: Form,
    /// The servers to offer: the configured ones, then any the rule names
    /// that is not configured (so that it can be seen and dropped).
    choices: Vec<(ServerKey, String)>,
    scopes: Vec<Scope>,
    /// What the cache has seen; `None` until it has been read.
    seen: Option<BTreeMap<String, BTreeSet<i32>>>,
    /// The labels offered with their values, from `seen` and the rule.
    offered: Vec<(String, Vec<i32>)>,
    /// The value ticks offered now.
    shown: Vec<i32>,
    /// A save was tried: show the problem even for an untouched form.
    attempted: bool,
    /// The last write failed.
    error: Option<String>,
    busy: bool,
    name: Entity<InputState>,
    from: Entity<InputState>,
    focus: FocusHandle,
    event_focus: FocusHandle,
    scope_focus: FocusHandle,
    server_focus: Vec<FocusHandle>,
    any_label_focus: FocusHandle,
    label_focus: Vec<FocusHandle>,
    any_value_focus: FocusHandle,
    value_focus: Vec<FocusHandle>,
    bots_focus: FocusHandle,
    enabled_focus: FocusHandle,
    save_focus: FocusHandle,
    cancel_focus: FocusHandle,
    scroll: ScrollHandle,
    task: Option<Task<()>>,
    catalog_task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<NotifyRuleEvent> for NotifyRuleDialog {}

impl Focusable for NotifyRuleDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl NotifyRuleDialog {
    /// Opens the dialog with the focus on the name.
    pub fn new(
        env: Env,
        context: NotifyRuleContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let baseline = context
            .settings
            .rules
            .get(context.index)
            .cloned()
            .unwrap_or_else(form::new_rule);
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Name (optional)"));
        let from = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Names, usernames or emails, separated by commas; empty: anyone")
        });
        let subs = vec![
            cx.subscribe_in(
                &name,
                window,
                |this, _, ev: &InputEvent, window, cx| match ev {
                    InputEvent::PressEnter { .. } => this.save_form(window, cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &from,
                window,
                |this, _, ev: &InputEvent, window, cx| match ev {
                    InputEvent::PressEnter { .. } => this.save_form(window, cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            ),
        ];
        let mut choices = context.servers.clone();
        for slug in &baseline.servers {
            if !choices.iter().any(|(k, _)| k.as_str() == slug) {
                choices.push((
                    ServerKey::new(slug.clone()),
                    format!("{slug} (not configured)"),
                ));
            }
        }
        let form = Form::from_rule(&baseline);
        let scopes = form::scope_choices(&context.queries, &baseline.scope);
        let mut this = Self {
            env,
            settings: context.settings,
            index: context.index,
            server_focus: choices.iter().map(|_| cx.focus_handle()).collect(),
            choices,
            scopes,
            form,
            baseline,
            seen: None,
            offered: Vec::new(),
            shown: Vec::new(),
            attempted: false,
            error: None,
            busy: false,
            name,
            from,
            focus: cx.focus_handle(),
            event_focus: cx.focus_handle(),
            scope_focus: cx.focus_handle(),
            any_label_focus: cx.focus_handle(),
            label_focus: Vec::new(),
            any_value_focus: cx.focus_handle(),
            value_focus: Vec::new(),
            bots_focus: cx.focus_handle(),
            enabled_focus: cx.focus_handle(),
            save_focus: cx.focus_handle(),
            cancel_focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            task: None,
            catalog_task: None,
            _subs: subs,
        };
        let (rule_name, rule_from) = (
            this.baseline.name.clone().unwrap_or_default(),
            this.baseline.from.join(", "),
        );
        this.name
            .update(cx, |s, cx| s.set_value(rule_name, window, cx));
        this.from
            .update(cx, |s, cx| s.set_value(rule_from, window, cx));
        this.refresh(cx);
        this.load_catalog(context.server_ids, cx);
        let handle = this.name.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        this
    }

    /// Reads the labels the cache has seen, in the background.
    fn load_catalog(&mut self, servers: Vec<ServerId>, cx: &mut Context<Self>) {
        let store = self.env.store.clone();
        self.catalog_task = Some(cx.spawn(async move |this, cx| {
            let seen = cx
                .background_spawn(async move {
                    let store = store.lock().map_err(|_| "the cache is locked".to_owned())?;
                    store.label_values(&servers).map_err(|e| e.to_string())
                })
                .await;
            this.update(cx, |this, cx| {
                match seen {
                    Ok(seen) => this.seen = Some(seen),
                    // The fallbacks stay; the user can still tick the usual
                    // labels, so this is only worth a note in the log.
                    Err(e) => log::warn!("reading the cached labels failed: {e}"),
                }
                this.refresh(cx);
            })
            .ok();
        }));
    }

    #[cfg(test)]
    pub(crate) fn rule_for_test(&self, cx: &App) -> Rule {
        self.read_rule(cx)
    }

    /// The label names offered, in order.
    #[cfg(test)]
    pub(crate) fn labels_for_test(&self) -> Vec<String> {
        self.offered.iter().map(|(n, _)| n.clone()).collect()
    }

    /// The value ticks offered now.
    #[cfg(test)]
    pub(crate) fn values_for_test(&self) -> Vec<i32> {
        self.shown.clone()
    }

    #[cfg(test)]
    pub(crate) fn problem_for_test(&self) -> Option<&'static str> {
        self.shown_problem()
    }

    #[cfg(test)]
    pub(crate) fn error_for_test(&self) -> Option<&str> {
        self.error.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn index_for_test(&self) -> usize {
        self.index
    }

    #[cfg(test)]
    pub(crate) fn name_input_for_test(&self) -> Entity<InputState> {
        self.name.clone()
    }

    #[cfg(test)]
    pub(crate) fn actors_input_for_test(&self) -> Entity<InputState> {
        self.from.clone()
    }

    #[cfg(test)]
    pub(crate) fn scopes_for_test(&self) -> Vec<Scope> {
        self.scopes.clone()
    }

    /// Whether the labels and values are shown (vote rules), the server
    /// ticks, and the ticks of "any label" and "any value".
    #[cfg(test)]
    pub(crate) fn fields_for_test(&self) -> (bool, bool) {
        (self.form.is_vote(), self.offers_servers())
    }

    #[cfg(test)]
    pub(crate) fn set_event_for_test(&mut self, event: RuleEvent, cx: &mut Context<Self>) {
        self.set_event(event, cx);
    }

    #[cfg(test)]
    pub(crate) fn toggle_server_for_test(&mut self, slug: &str, cx: &mut Context<Self>) {
        self.toggle_server(slug, cx);
    }

    #[cfg(test)]
    pub(crate) fn toggle_label_for_test(&mut self, label: &str, cx: &mut Context<Self>) {
        self.toggle_label(label, cx);
    }

    #[cfg(test)]
    pub(crate) fn toggle_value_for_test(&mut self, value: i32, cx: &mut Context<Self>) {
        self.toggle_value(value, cx);
    }

    #[cfg(test)]
    pub(crate) fn toggle_any_label_for_test(&mut self, cx: &mut Context<Self>) {
        self.toggle_any_label(cx);
    }

    #[cfg(test)]
    pub(crate) fn toggle_any_value_for_test(&mut self, cx: &mut Context<Self>) {
        self.toggle_any_value(cx);
    }

    /// Whether the form has the servers field: only with several servers
    /// (or a rule that names one that is gone).
    fn offers_servers(&self) -> bool {
        self.choices.len() > 1
    }

    // ---- the form ----

    /// The rule the form makes now.
    fn read_rule(&self, cx: &App) -> Rule {
        self.form.build(
            &self.baseline,
            self.name.read(cx).value().as_ref(),
            self.from.read(cx).value().as_ref(),
        )
    }

    /// Works out the labels and values on offer and makes the tick boxes
    /// focusable. Runs when the form or the cache result changes, not when
    /// it is drawn.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let keep: Vec<String> = self
            .baseline
            .labels
            .iter()
            .chain(&self.form.labels)
            .cloned()
            .collect();
        self.offered = catalog::offered_labels(self.seen.as_ref(), &keep);
        self.shown = catalog::shown_values(
            &self.offered,
            self.form.any_label,
            &self.form.labels,
            &self.form.values,
        );
        while self.label_focus.len() < self.offered.len() {
            self.label_focus.push(cx.focus_handle());
        }
        while self.value_focus.len() < self.shown.len() {
            self.value_focus.push(cx.focus_handle());
        }
        cx.notify();
    }

    /// The problem to show: not before a save was tried.
    fn shown_problem(&self) -> Option<&'static str> {
        if self.attempted {
            self.form.problem()
        } else {
            None
        }
    }

    fn set_event(&mut self, event: RuleEvent, cx: &mut Context<Self>) {
        self.form.event = event;
        self.refresh(cx);
    }

    fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        self.form.scope = scope;
        self.refresh(cx);
    }

    fn step_event(&mut self, delta: isize, cx: &mut Context<Self>) {
        let at = EVENTS.iter().position(|e| *e == self.form.event);
        let to = (at.unwrap_or(0) as isize + delta).rem_euclid(EVENTS.len() as isize);
        self.set_event(EVENTS[to as usize], cx);
    }

    fn step_scope(&mut self, delta: isize, cx: &mut Context<Self>) {
        let at = self.scopes.iter().position(|s| *s == self.form.scope);
        let to = (at.unwrap_or(0) as isize + delta).rem_euclid(self.scopes.len() as isize);
        let scope = self.scopes[to as usize].clone();
        self.set_scope(scope, cx);
    }

    fn toggle_server(&mut self, slug: &str, cx: &mut Context<Self>) {
        if let Some(at) = self.form.servers.iter().position(|k| k == slug) {
            self.form.servers.remove(at);
        } else {
            self.form.servers.push(slug.to_owned());
            // Config order, whichever way they were ticked.
            let order = |k: &String| self.choices.iter().position(|(c, _)| c.as_str() == k);
            let mut servers = std::mem::take(&mut self.form.servers);
            servers.sort_by_key(order);
            self.form.servers = servers;
        }
        self.refresh(cx);
    }

    fn toggle_any_label(&mut self, cx: &mut Context<Self>) {
        self.form.any_label = !self.form.any_label;
        self.refresh(cx);
    }

    fn toggle_label(&mut self, label: &str, cx: &mut Context<Self>) {
        if !self.form.labels.remove(label) {
            self.form.labels.insert(label.to_owned());
        }
        self.refresh(cx);
    }

    fn toggle_any_value(&mut self, cx: &mut Context<Self>) {
        self.form.any_value = !self.form.any_value;
        self.refresh(cx);
    }

    fn toggle_value(&mut self, value: i32, cx: &mut Context<Self>) {
        if !self.form.values.remove(&value) {
            self.form.values.insert(value);
        }
        self.refresh(cx);
    }

    // ---- keys ----

    fn close(&mut self, _: &CloseRule, _: &mut Window, cx: &mut Context<Self>) {
        if !self.busy {
            cx.emit(NotifyRuleEvent::Close);
        }
    }

    fn save_action(&mut self, _: &SaveRule, window: &mut Window, cx: &mut Context<Self>) {
        self.save_form(window, cx);
    }

    fn tab_order(&self, cx: &App) -> Vec<FocusHandle> {
        let mut v = vec![
            self.name.read(cx).focus_handle(cx),
            self.event_focus.clone(),
            self.scope_focus.clone(),
        ];
        if self.form.is_vote() {
            v.push(self.any_label_focus.clone());
            if !self.form.any_label {
                v.extend(self.label_focus.iter().take(self.offered.len()).cloned());
            }
            v.push(self.any_value_focus.clone());
            if !self.form.any_value {
                v.extend(self.value_focus.iter().take(self.shown.len()).cloned());
            }
        }
        if self.offers_servers() {
            v.extend(self.server_focus.iter().cloned());
        }
        v.push(self.from.read(cx).focus_handle(cx));
        v.push(self.bots_focus.clone());
        v.push(self.enabled_focus.clone());
        v.push(self.save_focus.clone());
        v.push(self.cancel_focus.clone());
        v
    }

    fn move_focus(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let order = self.tab_order(cx);
        let len = order.len() as isize;
        let next = match order.iter().position(|h| h.contains_focused(window, cx)) {
            Some(i) => (i as isize + delta).rem_euclid(len) as usize,
            None if delta > 0 => 0,
            None => order.len() - 1,
        };
        window.focus(&order[next], cx);
    }

    fn next_field(&mut self, _: &RuleNextField, window: &mut Window, cx: &mut Context<Self>) {
        self.move_focus(1, window, cx);
    }

    fn prev_field(&mut self, _: &RulePrevField, window: &mut Window, cx: &mut Context<Self>) {
        self.move_focus(-1, window, cx);
    }

    // ---- saving ----

    /// Writes the list with this rule in place of the one being edited (at
    /// the end for a new one). On success the shell hears [`NotifyRuleEvent::
    /// Saved`]; on failure nothing changes but the message.
    fn save_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.attempted = true;
        if self.form.problem().is_some() {
            return cx.notify();
        }
        let rule = self.read_rule(cx);
        let settings = NotificationSettings {
            rules: with_query(&self.settings.rules, self.index, rule),
            ..self.settings.clone()
        };
        let index = self.index.min(settings.rules.len().saturating_sub(1));
        self.busy = true;
        self.error = None;
        let backend = self.env.backend.clone();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { backend.save_notifications(&settings) })
                .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(saved) => cx.emit(NotifyRuleEvent::Saved {
                        settings: saved,
                        index,
                    }),
                    Err(e) => this.error = Some(format!("Could not save the rule: {e}")),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // ---- rendering ----

    fn label(&self, text: &'static str, cx: &App) -> Div {
        div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(text)
    }

    /// One of the choices of the event or scope group.
    fn render_choice(
        &self,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        on: bool,
        cx: &App,
    ) -> Stateful<Div> {
        let theme = cx.theme();
        let hover = theme.list_hover;
        div()
            .id(id)
            .px_2()
            .py_0p5()
            .rounded_md()
            .border_1()
            .border_color(if on { theme.primary } else { theme.border })
            .bg(if on { theme.primary } else { theme.transparent })
            .text_color(if on {
                theme.primary_foreground
            } else {
                theme.foreground
            })
            .cursor_pointer()
            .when(!on, |d| d.hover(move |d| d.bg(hover)))
            .child(label.into())
    }

    fn render_events(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let ring = if self.event_focus.is_focused(window) {
            cx.theme().ring
        } else {
            cx.theme().transparent
        };
        let chips: Vec<_> = EVENTS
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let event = *e;
                self.render_choice(
                    ("notify-event", i),
                    form::event_label(event),
                    self.form.event == event,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.set_event(event, cx)))
            })
            .collect();
        h_flex()
            .id("notify-events")
            .key_context(CHOICE_GROUP)
            .track_focus(&self.event_focus)
            .on_action(cx.listener(|this, _: &ChoicePrev, _, cx| this.step_event(-1, cx)))
            .on_action(cx.listener(|this, _: &ChoiceNext, _, cx| this.step_event(1, cx)))
            .flex_wrap()
            .gap_1()
            .p_0p5()
            .rounded_md()
            .border_1()
            .border_color(ring)
            .children(chips)
            .into_any_element()
    }

    fn render_scopes(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let ring = if self.scope_focus.is_focused(window) {
            cx.theme().ring
        } else {
            cx.theme().transparent
        };
        let chips: Vec<_> = self
            .scopes
            .iter()
            .enumerate()
            .map(|(i, scope)| {
                let target = scope.clone();
                self.render_choice(
                    ("notify-scope", i),
                    form::scope_label(scope),
                    self.form.scope == *scope,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.set_scope(target.clone(), cx)))
            })
            .collect();
        h_flex()
            .id("notify-scopes")
            .key_context(CHOICE_GROUP)
            .track_focus(&self.scope_focus)
            .on_action(cx.listener(|this, _: &ChoicePrev, _, cx| this.step_scope(-1, cx)))
            .on_action(cx.listener(|this, _: &ChoiceNext, _, cx| this.step_scope(1, cx)))
            .flex_wrap()
            .gap_1()
            .p_0p5()
            .rounded_md()
            .border_1()
            .border_color(ring)
            .children(chips)
            .into_any_element()
    }

    /// The labels and values of a vote rule, as tick boxes.
    fn render_votes(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let any_label = check(
            "notify-any-label",
            "Any label",
            self.form.any_label,
            &self.any_label_focus,
            false,
            window,
            cx,
        )
        .on_click(cx.listener(|this, _, _, cx| this.toggle_any_label(cx)));
        let labels: Vec<_> = if self.form.any_label {
            Vec::new()
        } else {
            self.offered
                .iter()
                .zip(&self.label_focus)
                .enumerate()
                .map(|(i, ((name, _), focus))| {
                    let target = name.clone();
                    check(
                        ("notify-label", i),
                        name.clone(),
                        self.form.labels.contains(name),
                        focus,
                        false,
                        window,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_label(&target, cx)))
                })
                .collect()
        };
        let any_value = check(
            "notify-any-value",
            "Any value",
            self.form.any_value,
            &self.any_value_focus,
            false,
            window,
            cx,
        )
        .on_click(cx.listener(|this, _, _, cx| this.toggle_any_value(cx)));
        let values: Vec<_> = if self.form.any_value {
            Vec::new()
        } else {
            self.shown
                .iter()
                .zip(&self.value_focus)
                .enumerate()
                .map(|(i, (value, focus))| {
                    let value = *value;
                    check(
                        ("notify-value", i),
                        if value == 0 {
                            "0".to_owned()
                        } else {
                            format!("{value:+}")
                        },
                        self.form.values.contains(&value),
                        focus,
                        false,
                        window,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_value(value, cx)))
                })
                .collect()
        };
        let muted = cx.theme().muted_foreground;
        v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(self.label("Labels: a vote on any ticked label counts", cx))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_3()
                            .child(any_label)
                            .children(labels),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(self.label(
                        "Values: one list for all labels, a vote with a ticked value counts \
                         (0 is a retracted vote)",
                        cx,
                    ))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_3()
                            .child(any_value)
                            .children(values),
                    )
                    .child(div().text_xs().text_color(muted).child(
                        "Offered from the changes in the cache; with several labels ticked, \
                         the values of all of them. Nothing cached: Code-Review -2 to +2, \
                         Verified -1 to +1.",
                    )),
            )
            .into_any_element()
    }

    fn render_form(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let servers = self.offers_servers().then(|| {
            let checks: Vec<_> = self
                .choices
                .iter()
                .zip(&self.server_focus)
                .enumerate()
                .map(|(i, ((key, name), focus))| {
                    let slug = key.to_string();
                    check(
                        ("notify-server", i),
                        name.clone(),
                        self.form.servers.contains(&slug),
                        focus,
                        false,
                        window,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_server(&slug, cx)))
                })
                .collect();
            v_flex()
                .gap_1()
                .child(self.label("Servers (none ticked: all servers)", cx))
                .child(h_flex().flex_wrap().gap_3().children(checks))
        });
        let vote = self.form.is_vote().then(|| self.render_votes(window, cx));
        let events = self.render_events(window, cx);
        let scopes = self.render_scopes(window, cx);
        let bots = check(
            "notify-bots",
            "Ignore bots",
            self.form.exclude_bots,
            &self.bots_focus,
            false,
            window,
            cx,
        )
        .self_start()
        .on_click(cx.listener(|this, _, _, cx| {
            this.form.exclude_bots = !this.form.exclude_bots;
            this.refresh(cx);
        }));
        let enabled = check(
            "notify-enabled",
            "On",
            self.form.enabled,
            &self.enabled_focus,
            false,
            window,
            cx,
        )
        .self_start()
        .on_click(cx.listener(|this, _, _, cx| {
            this.form.enabled = !this.form.enabled;
            this.refresh(cx);
        }));
        let body = v_flex()
            .gap_3()
            .p_4()
            .child(
                v_flex()
                    .gap_1()
                    .child(self.label("Name", cx))
                    .child(Input::new(&self.name).small()),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(self.label("Event (Left / Right)", cx))
                    .child(events),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(self.label("Changes (Left / Right)", cx))
                    .child(scopes),
            )
            .children(vote)
            .children(servers)
            .child(
                v_flex()
                    .gap_1()
                    .child(self.label("From (empty: anyone)", cx))
                    .child(Input::new(&self.from).small()),
            )
            .child(bots)
            .child(enabled)
            .when_some(self.shown_problem(), |d, why| {
                d.child(div().text_color(theme.danger).child(why))
            })
            .when_some(self.error.clone(), |d, e| {
                d.child(div().text_color(theme.danger).child(e))
            });
        let dim_save = self.busy;
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
                    .child("Tab next \u{00b7} Space ticks \u{00b7} Esc discards"),
            )
            .child(
                chip(
                    "notify-cancel",
                    "Cancel",
                    &self.cancel_focus,
                    Look::Plain,
                    self.busy,
                    window,
                    cx,
                )
                .on_click(cx.listener(|this, _, w, cx| this.close(&CloseRule, w, cx))),
            )
            .child(
                chip(
                    "notify-save",
                    if self.busy { "Saving\u{2026}" } else { "Save" },
                    &self.save_focus,
                    Look::Primary,
                    dim_save,
                    window,
                    cx,
                )
                .on_click(cx.listener(|this, _, w, cx| this.save_form(w, cx))),
            );
        v_flex()
            .size_full()
            .child(
                div().relative().flex_1().min_h_0().child(
                    div()
                        .id("notify-rule-form")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll)
                        .child(body),
                ),
            )
            .child(footer)
            .into_any_element()
    }
}

impl Render for NotifyRuleDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let title = if self.index < self.settings.rules.len() {
            "Edit notification rule"
        } else {
            "New notification rule"
        };
        let form = self.render_form(window, cx);
        div()
            .id("notify-rule-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(crate::theme::modal_backdrop(cx))
            .child(
                v_flex()
                    .id("notify-rule-dialog")
                    .key_context(NOTIFY_RULE_DIALOG)
                    .track_focus(&self.focus)
                    .on_action(cx.listener(Self::close))
                    .on_action(cx.listener(Self::save_action))
                    .on_action(cx.listener(Self::next_field))
                    .on_action(cx.listener(Self::prev_field))
                    .w(px(680.))
                    .h(px(620.))
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
                            .child(div().text_base().font_semibold().child(title)),
                    )
                    .child(div().flex_1().min_h_0().child(form)),
            )
    }
}
