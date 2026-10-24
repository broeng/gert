//! The Notifications tab of the settings page: the master switch and the
//! global options of the desktop notifications, the group window and the
//! quiet hours.
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
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::*;
use sync::notify::{NotificationSettings, QuietHours};

use crate::actions::*;

use super::{Mode, SettingsEvent, SettingsPage};

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

    pub(super) fn last_notification_row(&self) -> usize {
        self.quiet_row()
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
        }
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
        self.busy = true;
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
                        cx.emit(SettingsEvent::NotificationsSaved(saved));
                    }
                    Err(e) => {
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
        ]
    }
}
