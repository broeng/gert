//! The Notifications tab of the settings page: the master switch and the
//! global options of the desktop notifications, the group window, the quiet
//! hours, the rules and the test notification.
//!
//! The rules are listed with a short description each. A config file with
//! no rules gets the defaults ([`default_rules`]); the page shows them
//! marked as defaults, and the first change to the list writes all of them
//! as rules of the file. Deleting the last rule writes an empty list, which
//! the file loader reads as "the defaults" again, and so does the page. A
//! rule is added and edited in the rule editor, a dialog the shell opens
//! when the page emits [`SettingsEvent::EditNotificationRule`].
//!
//! Every change writes the whole `[notifications]` section through
//! `Backend::save_notifications` and the page then shows what comes back,
//! so what is on screen is what is on disk. The shell hears about it
//! ([`SettingsEvent::NotificationsSaved`]) and hands the watches the new
//! settings.

use std::time::Duration;

use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::Input;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use sync::notify::{NotificationSettings, QuietHours, Rule, default_rules};

use crate::{actions::*, card};

use super::{Mode, ServerRow, SettingsEvent, SettingsPage, moved, without};

/// The longest group window the input accepts: a day.
const MAX_GROUP_SECS: u64 = 86_400;

/// The on or off options, in the order of their rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flag {
    Enabled,
    OnlyWhenUnfocused,
    SuppressCurrentChange,
    IgnoreOwnActions,
}

impl Flag {
    const ALL: [Flag; 4] = [
        Flag::Enabled,
        Flag::OnlyWhenUnfocused,
        Flag::SuppressCurrentChange,
        Flag::IgnoreOwnActions,
    ];

    fn label(self) -> &'static str {
        match self {
            Flag::Enabled => "Desktop notifications",
            Flag::OnlyWhenUnfocused => "Only while the window is in the background",
            Flag::SuppressCurrentChange => "Not for the change on screen",
            Flag::IgnoreOwnActions => "Ignore my own actions",
        }
    }

    fn explanation(self) -> &'static str {
        match self {
            Flag::Enabled => "The master switch: off, nothing is watched for and nothing is shown.",
            Flag::OnlyWhenUnfocused => {
                "No notification while this window has the focus, you see the news anyway."
            }
            Flag::SuppressCurrentChange => {
                "No notification about the change you are looking at in the window."
            }
            Flag::IgnoreOwnActions => {
                "Votes, comments and so on that you made yourself, here or in the web UI, stay quiet."
            }
        }
    }

    fn get(self, s: &NotificationSettings) -> bool {
        match self {
            Flag::Enabled => s.enabled,
            Flag::OnlyWhenUnfocused => s.only_when_unfocused,
            Flag::SuppressCurrentChange => s.suppress_current_change,
            Flag::IgnoreOwnActions => s.ignore_own_actions,
        }
    }

    fn set(self, s: &mut NotificationSettings, on: bool) {
        match self {
            Flag::Enabled => s.enabled = on,
            Flag::OnlyWhenUnfocused => s.only_when_unfocused = on,
            Flag::SuppressCurrentChange => s.suppress_current_change = on,
            Flag::IgnoreOwnActions => s.ignore_own_actions = on,
        }
    }
}

/// The group window the typed text means, or why it is no good.
pub(crate) fn parse_group_window(text: &str) -> Result<Duration, &'static str> {
    match text.trim().parse::<u64>() {
        Ok(secs) if secs <= MAX_GROUP_SECS => Ok(Duration::from_secs(secs)),
        _ => Err("Enter whole seconds, for example 20 (0: every event on its own)."),
    }
}

/// The quiet hours the typed text means (`None` for empty), or why it is
/// no good.
pub(crate) fn parse_quiet_hours(text: &str) -> Result<Option<QuietHours>, &'static str> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    QuietHours::parse(text).map(Some).map_err(
        |_| "Enter the hours as 22:00-07:00 (start-end, 24-hour clock), or nothing for none.",
    )
}

/// How the group window reads in its row.
fn group_window_text(window: Duration) -> String {
    match window.as_secs() {
        0 => "off, every event on its own".to_owned(),
        1 => "1 second".to_owned(),
        n => format!("{n} seconds"),
    }
}

/// What the rule applies to besides what [`Rule::describe`] says: the
/// servers it is limited to, by name. `servers` names the configured ones.
pub(crate) fn rule_text(rule: &Rule, servers: &[ServerRow]) -> String {
    let mut text = rule.describe();
    if !rule.servers.is_empty() {
        let names: Vec<String> = rule
            .servers
            .iter()
            .map(|k| {
                servers
                    .iter()
                    .find(|s| s.key.as_str() == k)
                    .map_or_else(|| k.clone(), |s| s.name.clone())
            })
            .collect();
        text.push_str(" (only ");
        text.push_str(&names.join(", "));
        text.push(')');
    }
    text
}

impl SettingsPage {
    /// The first notification row: the master switch.
    pub(super) fn notify_row(&self) -> usize {
        self.servers_row() + self.servers.len().max(1)
    }

    fn flag_row(&self, flag: Flag) -> usize {
        self.notify_row() + Flag::ALL.iter().position(|f| *f == flag).unwrap_or(0)
    }

    fn group_row(&self) -> usize {
        self.notify_row() + Flag::ALL.len()
    }

    fn quiet_row(&self) -> usize {
        self.group_row() + 1
    }

    /// The first rule row (the empty-state row when there are no rules).
    fn rules_row(&self) -> usize {
        self.quiet_row() + 1
    }

    fn reset_row(&self) -> usize {
        self.rules_row() + self.notify.rules.len().max(1)
    }

    fn test_row(&self) -> usize {
        self.reset_row() + 1
    }

    pub(super) fn last_notification_row(&self) -> usize {
        self.test_row()
    }

    /// The rule the cursor is on, if it is on one.
    fn rule_under_cursor(&self) -> Option<usize> {
        self.cursor
            .checked_sub(self.rules_row())
            .filter(|i| *i < self.notify.rules.len())
    }

    /// Whether the rules are the defaults, so that the file may have none.
    fn rules_are_defaults(&self) -> bool {
        self.notify.rules == default_rules()
    }

    #[cfg(test)]
    pub(crate) fn notify_for_test(&self) -> &NotificationSettings {
        &self.notify
    }

    #[cfg(test)]
    pub(crate) fn notify_input_for_test(&self) -> Entity<gpui_kit::component::input::InputState> {
        self.notify_input.clone()
    }

    #[cfg(test)]
    pub(crate) fn notify_row_for_test(&self) -> usize {
        self.notify_row()
    }

    #[cfg(test)]
    pub(crate) fn rules_are_defaults_for_test(&self) -> bool {
        self.rules_are_defaults()
    }

    // ---- keys ----

    /// `Enter` / `e` on a notification row.
    pub(super) fn edit_notification_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_change() || self.mode != Mode::Browsing {
            return;
        }
        if self.cursor == self.group_row() {
            self.start_notification_edit(Mode::EditingGroupWindow, window, cx);
        } else if self.cursor == self.quiet_row() {
            self.start_notification_edit(Mode::EditingQuietHours, window, cx);
        } else if self.cursor >= self.rules_row() && self.cursor < self.reset_row() {
            // On the empty-state row that is a new rule.
            let index = self.rule_under_cursor().unwrap_or(self.notify.rules.len());
            cx.emit(SettingsEvent::EditNotificationRule { index });
        } else if self.cursor == self.reset_row() {
            self.ask_reset(cx);
        } else if self.cursor == self.test_row() {
            cx.emit(SettingsEvent::TestNotification);
        } else {
            self.toggle_notification_row(window, cx);
        }
    }

    /// `Space` on a notification row: flip the option, or edit the number
    /// or the hours.
    pub(super) fn toggle_notification_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_change() || self.mode != Mode::Browsing {
            return;
        }
        if let Some(flag) = Flag::ALL
            .into_iter()
            .find(|f| self.flag_row(*f) == self.cursor)
        {
            self.set_flag(flag, !flag.get(&self.notify), window, cx);
        } else if self.cursor == self.group_row() || self.cursor == self.quiet_row() {
            self.edit_notification_row(window, cx);
        } else if let Some(i) = self.rule_under_cursor() {
            let mut settings = self.notify.clone();
            settings.rules[i].enabled = !settings.rules[i].enabled;
            self.save_notifications_at(settings, Some(i), window, cx);
        }
    }

    /// `a` in the tab: a new rule, in the rule editor.
    pub(super) fn add_notification_rule(&mut self, cx: &mut Context<Self>) {
        if self.can_change() && self.mode == Mode::Browsing {
            cx.emit(SettingsEvent::EditNotificationRule {
                index: self.notify.rules.len(),
            });
        }
    }

    /// `d` on a rule: ask first.
    pub(super) fn ask_delete_rule(&mut self, cx: &mut Context<Self>) {
        if self.can_change()
            && self.mode == Mode::Browsing
            && let Some(i) = self.rule_under_cursor()
        {
            self.mode = Mode::ConfirmingRule(i);
            cx.notify();
        }
    }

    /// Asks before the rules are replaced by the defaults.
    fn ask_reset(&mut self, cx: &mut Context<Self>) {
        if self.rules_are_defaults() {
            self.hint
                .say("The rules already are the defaults.", false, cx, |p| {
                    &mut p.hint
                });
        } else {
            self.mode = Mode::ConfirmingReset;
            cx.notify();
        }
    }

    /// `y` at a question about the rules; false if none was asked.
    pub(super) fn confirm_rule_question(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match self.mode {
            Mode::ConfirmingRule(i) => {
                let rules = without(&self.notify.rules, i);
                let settings = NotificationSettings {
                    rules,
                    ..self.notify.clone()
                };
                self.save_notifications_at(settings, Some(i), window, cx);
            }
            Mode::ConfirmingReset => {
                let settings = NotificationSettings {
                    rules: default_rules(),
                    ..self.notify.clone()
                };
                self.save_notifications_at(settings, Some(0), window, cx);
            }
            _ => return false,
        }
        true
    }

    /// `Shift-J` / `Shift-K` on a rule.
    pub(super) fn move_rule(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(i) = self.rule_under_cursor()
            && let Some((rules, to)) = moved(&self.notify.rules, i, delta)
        {
            let settings = NotificationSettings {
                rules,
                ..self.notify.clone()
            };
            self.save_notifications_at(settings, Some(to), window, cx);
        }
    }

    /// `t`: show a sample notification.
    pub(super) fn ask_test_notification(&mut self, cx: &mut Context<Self>) {
        if self.mode == Mode::Browsing {
            cx.emit(SettingsEvent::TestNotification);
        }
    }

    /// The shell showed (or could not show) the sample notification.
    pub(crate) fn test_sent(&mut self, sent: bool, cx: &mut Context<Self>) {
        let (text, error) = if sent {
            (
                "Sent: check your desktop. Nothing appeared? On Linux a notification \
                 daemon must be running (your desktop's, or one like dunst or mako).",
                false,
            )
        } else {
            ("Notifications are not available in this run.", true)
        };
        self.hint.say(text, error, cx, |p| &mut p.hint);
    }

    fn set_flag(&mut self, flag: Flag, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        let mut settings = self.notify.clone();
        flag.set(&mut settings, on);
        self.save_notifications(settings, window, cx);
    }

    fn start_notification_edit(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        let text = match mode {
            Mode::EditingGroupWindow => self.notify.group_window.as_secs().to_string(),
            _ => self
                .notify
                .quiet_hours
                .map(|q| q.format())
                .unwrap_or_default(),
        };
        let placeholder = match mode {
            Mode::EditingGroupWindow => "Seconds, e.g. 20",
            _ => "22:00-07:00, or empty for none",
        };
        self.notify_input.update(cx, |s, cx| {
            s.set_placeholder(placeholder, window, cx);
            s.set_value(text, window, cx);
        });
        self.cursor = if mode == Mode::EditingGroupWindow {
            self.group_row()
        } else {
            self.quiet_row()
        };
        self.mode = mode;
        self.invalid = None;
        self.notify_input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    /// `Enter` in the group window or quiet hours input.
    pub(super) fn commit_notification_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.notify_input.read(cx).value().to_string();
        let mut settings = self.notify.clone();
        match self.mode {
            Mode::EditingGroupWindow => match parse_group_window(&text) {
                Ok(window_len) => settings.group_window = window_len,
                Err(why) => {
                    self.invalid = Some(why);
                    return cx.notify();
                }
            },
            _ => match parse_quiet_hours(&text) {
                Ok(hours) => settings.quiet_hours = hours,
                Err(why) => {
                    self.invalid = Some(why);
                    return cx.notify();
                }
            },
        }
        self.invalid = None;
        self.save_notifications(settings, window, cx);
    }

    // ---- saving ----

    /// Writes `settings` in the background. On success they become the
    /// ones on screen (what the backend returns) and the shell is told; on
    /// failure nothing changes but a hint.
    pub(super) fn save_notifications(
        &mut self,
        settings: NotificationSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_notifications_at(settings, None, window, cx);
    }

    /// Like [`Self::save_notifications`], and the cursor goes to rule
    /// `rule` of the saved list afterwards.
    pub(super) fn save_notifications_at(
        &mut self,
        settings: NotificationSettings,
        rule: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy = true;
        let emptied = settings.rules.is_empty();
        let backend = self.env.backend.clone();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { backend.save_notifications(&settings) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(saved) => {
                        this.notify = saved.clone();
                        this.leave_edit(window, cx);
                        if let Some(rule) = rule {
                            let last = this.notify.rules.len().saturating_sub(1);
                            this.cursor = this.in_tab(this.rules_row() + rule.min(last));
                        }
                        if emptied && !saved.rules.is_empty() {
                            this.hint.say(
                                "No rules left: the default rules apply again.",
                                false,
                                cx,
                                |p| &mut p.hint,
                            );
                        }
                        cx.emit(SettingsEvent::NotificationsSaved(saved));
                    }
                    Err(e) => {
                        if matches!(this.mode, Mode::ConfirmingRule(_) | Mode::ConfirmingReset) {
                            this.mode = Mode::Browsing;
                        }
                        this.hint.say(
                            format!("Could not save the notification settings: {e}"),
                            true,
                            cx,
                            |p| &mut p.hint,
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // ---- rendering ----

    fn render_flag(&self, flag: Flag, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let row = self.flag_row(flag);
        v_flex()
            .gap_0p5()
            .child(
                Checkbox::new(("notify-flag", row))
                    .label(flag.label())
                    .checked(flag.get(&self.notify))
                    .disabled(self.busy)
                    .on_click(cx.listener(move |this, checked: &bool, w, cx| {
                        this.cursor = row;
                        if this.can_change() && this.mode == Mode::Browsing {
                            this.set_flag(flag, *checked, w, cx);
                        }
                    })),
            )
            .child(
                div()
                    .pl_6()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(flag.explanation()),
            )
            .into_any_element()
    }

    /// A row with a value that is typed into an input: the label and what
    /// it does, the value (or the input while editing) and the buttons.
    fn render_typed_row(
        &self,
        id: &'static str,
        label: &'static str,
        explanation: &'static str,
        value: String,
        mode: Mode,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let editing = self.mode == mode;
        let busy = self.busy;
        let detail =
            if editing {
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(px(260.))
                            .child(Input::new(&self.notify_input).small()),
                    )
                    .child(
                        Button::new((id, 0usize))
                            .label("Save")
                            .small()
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, w, cx| this.commit_edit(w, cx))),
                    )
                    .child(Button::new((id, 1usize)).label("Cancel").small().on_click(
                        cx.listener(|this, _, w, cx| this.cancel_edit(&CancelEdit, w, cx)),
                    ))
                    .children(
                        self.invalid
                            .map(|why| div().text_sm().text_color(theme.danger).child(why)),
                    )
                    .into_any_element()
            } else {
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .font_family(theme.mono_font_family.clone())
                            .text_color(theme.muted_foreground)
                            .child(value),
                    )
                    .child(
                        Button::new((id, 2usize))
                            .label("Edit")
                            .small()
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, w, cx| {
                                if this.can_change() && this.mode == Mode::Browsing {
                                    this.start_notification_edit(mode, w, cx);
                                }
                            })),
                    )
                    .into_any_element()
            };
        v_flex()
            .w_full()
            .gap_0p5()
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(div().w(px(200.)).flex_none().child(label))
                    .child(detail),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(explanation),
            )
            .into_any_element()
    }

    fn render_rule(&self, i: usize, rule: &Rule, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let at = self.rules_row() + i;
        let selected = at == self.cursor && self.mode == Mode::Browsing;
        let len = self.notify.rules.len();
        let busy = self.busy;
        let defaults = self.rules_are_defaults();
        let row = h_flex()
            .id(("notify-rule", i))
            .px_3()
            .py_1p5()
            .gap_3()
            .items_center()
            .when(selected, |d| d.bg(theme.list_active))
            .on_click(cx.listener(move |this, _, _, cx| this.select(at, cx)));
        if self.mode == Mode::ConfirmingRule(i) {
            return row
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(theme.danger)
                        .child(format!("Delete \u{201c}{}\u{201d}?  y / n", rule.title())),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child(Button::new("rule-yes").label("Yes (y)").small().on_click(
                            cx.listener(|this, _, w, cx| {
                                this.confirm_delete(&ConfirmDelete, w, cx)
                            }),
                        ))
                        .child(Button::new("rule-no").label("No (n)").small().on_click(
                            cx.listener(|this, _, w, cx| this.cancel_delete(&CancelDelete, w, cx)),
                        )),
                )
                .into_any_element();
        }
        let dim = !rule.enabled;
        let title = rule.title();
        // A named rule shows what it does underneath; an unnamed one is
        // its description.
        let detail = rule_text(rule, &self.servers);
        let named = title != rule.describe();
        row.child(
            v_flex()
                .flex_1()
                .min_w_0()
                .when(dim, |d| d.opacity(0.55))
                .child(div().truncate().child(title))
                .when(named || !rule.servers.is_empty(), |d| {
                    d.child(
                        div()
                            .text_xs()
                            .truncate()
                            .text_color(theme.muted_foreground)
                            .child(detail),
                    )
                }),
        )
        .when(dim, |d| {
            d.child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.warning)
                    .child("off"),
            )
        })
        .when(defaults, |d| {
            d.child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("default"),
            )
        })
        .child(
            h_flex()
                .gap_1()
                .flex_none()
                .child(
                    Button::new(("rule-up", i))
                        .label("\u{2191}")
                        .small()
                        .disabled(i == 0 || busy)
                        .on_click(cx.listener(move |this, _, w, cx| {
                            this.cursor = at;
                            this.move_by(-1, w, cx)
                        })),
                )
                .child(
                    Button::new(("rule-down", i))
                        .label("\u{2193}")
                        .small()
                        .disabled(i + 1 >= len || busy)
                        .on_click(cx.listener(move |this, _, w, cx| {
                            this.cursor = at;
                            this.move_by(1, w, cx)
                        })),
                )
                .child(
                    Button::new(("rule-edit", i))
                        .label("Edit")
                        .small()
                        .disabled(busy)
                        .on_click(cx.listener(move |this, _, w, cx| {
                            this.cursor = at;
                            this.edit_notification_row(w, cx)
                        })),
                )
                .child(
                    Button::new(("rule-delete", i))
                        .label("Delete")
                        .small()
                        .disabled(busy)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.cursor = at;
                            this.ask_delete_rule(cx)
                        })),
                ),
        )
        .into_any_element()
    }

    fn render_rules(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let n = self.notify.rules.len();
        let defaults = self.rules_are_defaults();
        let browsing = self.mode == Mode::Browsing;
        let note = if defaults {
            "These are the default rules. Changing one writes all of them to the config file."
        } else {
            "Each rule says which events to tell you about. Enter edits, a adds, \
             Space switches a rule off or on."
        };
        let mut body = v_flex().w_full().child(card::frame(
            card::title_bar(cx)
                .child(div().font_semibold().child("Rules"))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(note),
                ),
            true,
            false,
            cx,
        ));
        if n == 0 {
            // Only before the first save: the defaults fill the list at once.
            let selected = self.cursor == self.rules_row() && browsing;
            body = body.child(card::frame(
                card::row_separator(cx).child(
                    div()
                        .id("notify-rule-empty")
                        .px_3()
                        .py_2()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .when(selected, |d| d.bg(theme.list_active))
                        .child("No rules: the defaults apply. Press a to add one."),
                ),
                false,
                false,
                cx,
            ));
        }
        for (i, rule) in self.notify.rules.iter().enumerate() {
            let row = self.render_rule(i, rule, cx);
            body = body.child(card::frame(
                card::row_separator(cx).child(row),
                false,
                false,
                cx,
            ));
        }
        // The reset row closes the card.
        let selected = self.cursor == self.reset_row() && browsing;
        let reset = if self.mode == Mode::ConfirmingReset {
            h_flex()
                .gap_3()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .text_color(theme.danger)
                        .child("Replace all rules by the defaults?  y / n"),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child(Button::new("reset-yes").label("Yes (y)").small().on_click(
                            cx.listener(|this, _, w, cx| {
                                this.confirm_delete(&ConfirmDelete, w, cx)
                            }),
                        ))
                        .child(Button::new("reset-no").label("No (n)").small().on_click(
                            cx.listener(|this, _, w, cx| this.cancel_delete(&CancelDelete, w, cx)),
                        )),
                )
                .into_any_element()
        } else {
            h_flex()
                .gap_3()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .text_color(theme.muted_foreground)
                        .child(if defaults {
                            "The rules are the defaults."
                        } else {
                            "Replace all rules by the defaults."
                        }),
                )
                .child(
                    Button::new("reset-rules")
                        .label("Reset to defaults")
                        .small()
                        .disabled(self.busy || defaults)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cursor = this.reset_row();
                            this.ask_reset(cx)
                        })),
                )
                .into_any_element()
        };
        body = body.child(card::frame(
            card::row_separator(cx).child(
                div()
                    .id("notify-reset")
                    .px_3()
                    .py_1p5()
                    .when(selected, |d| d.bg(theme.list_active))
                    .child(reset),
            ),
            false,
            true,
            cx,
        ));
        v_flex()
            .gap_2()
            .child(body)
            .child(
                div().px_4().child(
                    Button::new("add-notify-rule")
                        .label("Add a rule (a)")
                        .small()
                        .disabled(self.busy || !browsing)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cursor = this.rules_row();
                            this.add_notification_rule(cx)
                        })),
                ),
            )
            .into_any_element()
    }

    fn render_test(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let row = h_flex()
            .w_full()
            .gap_3()
            .items_center()
            .child(
                v_flex()
                    .flex_1()
                    .gap_0p5()
                    .child("Send a test notification")
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Shows one now, whatever the rules and the quiet hours say."),
                    ),
            )
            .child(
                Button::new("test-notification")
                    .label("Test (t)")
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.cursor = this.test_row();
                        this.ask_test_notification(cx)
                    })),
            );
        self.section(
            "Test",
            "Enter or t.",
            row,
            self.cursor == self.test_row() && self.mode == Mode::Browsing,
            cx,
        )
    }

    pub(super) fn render_notifications(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let browsing = self.mode == Mode::Browsing;
        let flags: Vec<(AnyElement, bool)> = Flag::ALL
            .into_iter()
            .map(|f| {
                (
                    self.render_flag(f, cx),
                    browsing && self.cursor == self.flag_row(f),
                )
            })
            .collect();
        let timing = vec![
            (
                self.render_typed_row(
                    "notify-group",
                    "Group window",
                    "Events closer together than this are shown as one notification.",
                    group_window_text(self.notify.group_window),
                    Mode::EditingGroupWindow,
                    cx,
                ),
                browsing && self.cursor == self.group_row(),
            ),
            (
                self.render_typed_row(
                    "notify-quiet",
                    "Quiet hours",
                    "No desktop notifications in this daily window, in local time.",
                    self.notify
                        .quiet_hours
                        .map_or_else(|| "none".to_owned(), |q| q.format()),
                    Mode::EditingQuietHours,
                    cx,
                ),
                browsing && self.cursor == self.quiet_row(),
            ),
        ];
        vec![
            self.section_rows(
                "Notifications",
                "Space toggles. A notification daemon must be running on Linux.",
                flags,
                cx,
            ),
            self.section_rows(
                "Timing",
                "Enter edits: seconds for the group window, 22:00-07:00 for the hours.",
                timing,
                cx,
            ),
            self.render_rules(cx),
            self.render_test(cx),
        ]
    }
}
