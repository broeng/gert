//! The Projects tab of the settings page: the local git checkouts of the
//! active server's projects (`[[servers.projects]]`), with whether each one
//! is usable.
//!
//! The rows are what the server's [`Checkouts`] holds, in config order, and
//! the page observes the entity: the status of a row follows while the
//! background opens finish, and a save made by the shell shows at once.
//! Showing the tab opens every checkout that has not been tried and tries
//! the failed ones again ([`Checkouts::recheck`]), so fixing a path or
//! running `git init` in a terminal and coming back to the tab is enough.
//!
//! The page does not edit anything itself. A checkout is added and edited in
//! the project dialog, which the shell opens when the page emits
//! [`SettingsEvent::EditProject`]; a delete is confirmed here and the page
//! then emits [`SettingsEvent::SaveProjects`] with the shorter list. Both go
//! through `Shell::save_projects`.

use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::{
    actions::*,
    card,
    checkouts::{Checkout, CheckoutState},
    session::ProjectSettings,
};

use super::{Mode, SettingsEvent, SettingsPage, without};

/// The status words of a checkout: the text, and how to color it (`Some(true)`
/// good, `Some(false)` bad, `None` still being looked at).
pub(crate) fn status_text(checkout: &Checkout) -> (String, Option<bool>) {
    match &checkout.state {
        CheckoutState::Opening => ("Checking\u{2026}".to_owned(), None),
        CheckoutState::Open(_) => ("Git checkout".to_owned(), Some(true)),
        CheckoutState::Failed(e) => (e.to_string(), Some(false)),
    }
}

impl SettingsPage {
    /// The first project row (the empty-state row when there are none).
    pub(super) fn projects_row(&self) -> usize {
        self.last_notification_row() + 1
    }

    pub(super) fn last_project_row(&self) -> usize {
        self.projects_row() + self.project_names.len().max(1) - 1
    }

    pub(super) fn in_projects(&self) -> bool {
        self.cursor >= self.projects_row()
    }

    /// The project the cursor is on, if it is on one.
    fn project_under_cursor(&self) -> Option<usize> {
        self.cursor
            .checked_sub(self.projects_row())
            .filter(|i| *i < self.project_names.len())
    }

    #[cfg(test)]
    pub(crate) fn project_names_for_test(&self) -> &[String] {
        &self.project_names
    }

    /// Puts the cursor on the checkout of project `name`, if there is one
    /// (the shell does this when the gear on the change page led here).
    #[allow(dead_code)] // The gear on the change page uses it.
    pub(crate) fn select_project(&mut self, name: &str, cx: &mut Context<Self>) {
        if let Some(i) = self.project_names.iter().position(|n| n == name) {
            self.cursor = self.projects_row() + i;
            cx.notify();
        }
    }

    /// The tab was shown: open what has not been tried, try failures again.
    pub(super) fn recheck_projects(&mut self, cx: &mut Context<Self>) {
        if let Some(c) = &self.checkouts {
            c.update(cx, |c, cx| c.recheck(cx));
        }
    }

    /// The checkouts changed (an open finished, or the shell saved): list
    /// them again and keep the cursor on its project, or clamp it when that
    /// one is gone.
    pub(super) fn follow_projects(&mut self, cx: &mut Context<Self>) {
        let names: Vec<String> = self
            .checkouts
            .as_ref()
            .map(|c| c.read(cx).projects().map(|p| p.name.clone()).collect())
            .unwrap_or_default();
        let on = self
            .project_under_cursor()
            .and_then(|old| names.iter().position(|n| *n == self.project_names[old]));
        if names != self.project_names && matches!(self.mode, Mode::ConfirmingProject(_)) {
            // The list moved under the question.
            self.mode = Mode::Browsing;
        }
        self.project_names = names;
        if let Some(at) = on {
            self.cursor = self.projects_row() + at;
        }
        if self.tab == super::SettingsTab::Projects {
            self.cursor = self.in_tab(self.cursor);
        }
        cx.notify();
    }

    // ---- keys ----

    /// `Enter` / `e`: the dialog on the project under the cursor (a new one
    /// on the empty-state row).
    pub(super) fn edit_project(&mut self, cx: &mut Context<Self>) {
        if self.mode != Mode::Browsing || !self.can_change() || self.checkouts.is_none() {
            return;
        }
        let name = self
            .project_under_cursor()
            .and_then(|i| self.project_names.get(i).cloned());
        cx.emit(SettingsEvent::EditProject { name });
    }

    /// `a`: the dialog on a new project.
    pub(super) fn add_project(&mut self, cx: &mut Context<Self>) {
        if self.mode != Mode::Browsing || !self.can_change() || self.checkouts.is_none() {
            return;
        }
        cx.emit(SettingsEvent::EditProject { name: None });
    }

    /// `d`: asks whether to remove the checkout under the cursor.
    pub(super) fn ask_delete_project(&mut self, cx: &mut Context<Self>) {
        if self.mode == Mode::Browsing
            && self.can_change()
            && let Some(i) = self.project_under_cursor()
        {
            self.mode = Mode::ConfirmingProject(i);
            cx.notify();
        }
    }

    /// `y` at the question about a project; false if none was asked.
    pub(super) fn confirm_project_question(&mut self, cx: &mut Context<Self>) -> bool {
        let Mode::ConfirmingProject(i) = self.mode else {
            return false;
        };
        self.mode = Mode::Browsing;
        if let Some(c) = &self.checkouts {
            let list: Vec<ProjectSettings> = c.read(cx).projects().cloned().collect();
            cx.emit(SettingsEvent::SaveProjects(without(&list, i)));
        }
        cx.notify();
        true
    }

    // ---- rendering ----

    fn render_project_row(
        &self,
        i: usize,
        project: &ProjectSettings,
        checkout: &Checkout,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let at = self.projects_row() + i;
        let selected = at == self.cursor;
        let idle = !self.busy && self.mode == Mode::Browsing;
        let row = h_flex()
            .id(("project-row", i))
            .px_3()
            .py_1p5()
            .gap_3()
            .items_center()
            .when(selected, |d| d.bg(theme.list_active))
            .on_click(cx.listener(move |this, _, _, cx| this.select(at, cx)));
        if self.mode == Mode::ConfirmingProject(i) {
            return row
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(theme.danger)
                        .child(format!(
                            "Remove the checkout of \u{201c}{}\u{201d}?  y / n",
                            project.name
                        )),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("project-confirm-yes")
                                .label("Yes (y)")
                                .small()
                                .on_click(cx.listener(|this, _, w, cx| {
                                    this.confirm_delete(&ConfirmDelete, w, cx)
                                })),
                        )
                        .child(
                            Button::new("project-confirm-no")
                                .label("No (n)")
                                .small()
                                .on_click(cx.listener(|this, _, w, cx| {
                                    this.cancel_delete(&CancelDelete, w, cx)
                                })),
                        ),
                )
                .into_any_element();
        }
        let (status, good) = status_text(checkout);
        let color = match good {
            Some(true) => theme.success,
            Some(false) => theme.danger,
            None => theme.muted_foreground,
        };
        let name = project.name.clone();
        row.child(
            div()
                .w(px(220.))
                .flex_none()
                .truncate()
                .child(project.name.clone()),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .truncate()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .child(project.written.clone()),
                )
                .child(
                    div()
                        .id(("project-status", i))
                        .text_xs()
                        .text_color(color)
                        .child(status),
                ),
        )
        .child(
            h_flex()
                .gap_1()
                .flex_none()
                .child(
                    Button::new(("project-edit", i))
                        .label("Edit")
                        .small()
                        .disabled(!idle)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.cursor = at;
                            cx.emit(SettingsEvent::EditProject {
                                name: Some(name.clone()),
                            });
                        })),
                )
                .child(
                    Button::new(("project-remove", i))
                        .label("Remove")
                        .small()
                        .disabled(!idle)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.cursor = at;
                            this.ask_delete_project(cx)
                        })),
                ),
        )
        .into_any_element()
    }

    pub(super) fn render_projects(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let (projects, checkouts): (Vec<ProjectSettings>, Vec<Checkout>) = self
            .checkouts
            .as_ref()
            .map(|c| {
                let c = c.read(cx);
                (c.projects().cloned().collect(), c.all())
            })
            .unwrap_or_default();
        let n = projects.len();
        let mut body = v_flex().w_full().child(card::frame(
            card::title_bar(cx)
                .child(div().font_semibold().child("Local checkouts"))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Of this server's projects. Enter edits, a adds, d removes."),
                ),
            true,
            n == 0,
            cx,
        ));
        if n == 0 {
            let selected = self.cursor == self.projects_row() && self.mode == Mode::Browsing;
            body = body.child(card::frame(
                card::row_separator(cx).child(
                    div()
                        .id("project-row-empty")
                        .px_3()
                        .py_2()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .when(selected, |d| d.bg(theme.list_active))
                        .child(
                            "No local checkouts. Diffs and file lists of changes whose commits \
                             are in the checkout are read from it instead of the server. \
                             Press a to add one.",
                        ),
                ),
                false,
                true,
                cx,
            ));
        }
        for (i, (project, checkout)) in projects.iter().zip(&checkouts).enumerate() {
            let element = self.render_project_row(i, project, checkout, cx);
            body = body.child(card::frame(
                card::row_separator(cx).child(element),
                false,
                i + 1 == n,
                cx,
            ));
        }
        v_flex()
            .gap_2()
            .child(body)
            .child(
                div().px_4().child(
                    Button::new("add-project")
                        .label("Add a checkout (a)")
                        .small()
                        .disabled(self.mode != Mode::Browsing || self.checkouts.is_none())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cursor = this.projects_row();
                            this.add_project(cx)
                        })),
                ),
            )
            .into_any_element()
    }
}
