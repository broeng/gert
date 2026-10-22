//! The Manage filters dialog (`f m` on the change page): the file filter
//! rules as a list on the left (add, delete, reorder, on or off by default)
//! and the form of the selected rule on the right.
//!
//! The dialog is a modal overlay owned by the shell, like the reply dialog
//! and the action menu. While it is open no page key fires.
//!
//! # Modes
//!
//! - *Browsing* (the list has the keyboard): `j` / `k` move, `a` adds,
//!   `Enter` / `e` edits, `d` deletes, `Shift-J` / `Shift-K` reorder, `Space`
//!   switches the rule on or off by default. The form shows the selected
//!   rule and is shielded from the mouse; a click on it starts editing.
//! - *Editing* (the form): `Tab` / `Shift-Tab` cycle the controls (the
//!   dialog does the cycling itself, so focus never leaves it), `Ctrl-Enter`
//!   saves, `Esc` leaves the form and drops the edits.
//! - *Confirming* a delete: `y` / `Enter` or `n` / `Esc`.
//!
//! `Esc` in the list closes the dialog.
//!
//! # Saving
//!
//! Every committed change (the form, a delete, a reorder, the default
//! switch) writes the whole list through `Backend::save_filters` on the
//! background executor. The list that comes back is what is on screen and
//! what [`FileFilters`] gets, so the pages restyle at once and what is shown
//! is what is on disk. When the write fails the dialog says so and keeps its
//! state. Keys that change the list wait while a write is under way.

pub mod actions;
mod form;
#[cfg(test)]
mod tests;

use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::color_picker::{ColorPickerEvent, ColorPickerState, ColorSelect};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::{
    filters::{FileFilter, format_hex_color, parse_hex_color},
    pages::settings::{moved, with_query, without},
    reply::{Look, check, chip},
    session::Env,
    settings::FileFilters,
};
use actions::*;
use form::{Form, Kind, Preview};

/// What the dialog tells the shell.
pub enum FiltersEvent {
    /// The user closed it.
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Browsing,
    /// Editing the rule at `index`; `index == len` is a new rule.
    Editing {
        index: usize,
    },
    /// Asking whether to delete the rule at the index.
    Confirming(usize),
}

/// Which rule to put the cursor on after a save.
enum Select {
    Name(String),
    Index(usize),
}

/// What the dialog was opened from.
#[derive(Clone, Debug, Default)]
pub struct FiltersContext {
    /// The project of the change: new rules are scoped to it, and "Only this
    /// project" fills it in.
    pub project: Option<String>,
    /// The files of the change, for the match preview.
    pub paths: Option<Vec<String>>,
    /// The name of the rule to put the cursor on first.
    pub select: Option<String>,
    /// Start on a new rule, in the form.
    pub start_adding: bool,
}

pub struct FiltersDialog {
    env: Env,
    filters: Entity<FileFilters>,
    project: Option<String>,
    paths: Option<Vec<String>>,
    /// The rules as saved (what the backend returned last).
    rules: Vec<FileFilter>,
    cursor: usize,
    mode: Mode,
    /// The form: the effect, color, bold and default live here, the texts
    /// in the fields.
    form: Form,
    /// The rule the form was loaded from (a blank one for a new rule).
    baseline: FileFilter,
    /// Why the form cannot be saved, for the rule it holds now.
    problem: Option<String>,
    preview: Option<Preview>,
    dirty: bool,
    /// A save was tried: show the problem even for an untouched form.
    attempted: bool,
    /// The last write failed.
    error: Option<String>,
    /// A write is in flight.
    busy: bool,
    name: Entity<InputState>,
    globs: Entity<TextareaState>,
    projects: Entity<TextareaState>,
    picker: Entity<ColorPickerState>,
    focus: FocusHandle,
    only_focus: FocusHandle,
    action_focus: FocusHandle,
    bold_focus: FocusHandle,
    enabled_focus: FocusHandle,
    save_focus: FocusHandle,
    cancel_focus: FocusHandle,
    list_scroll: ScrollHandle,
    form_scroll: ScrollHandle,
    task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<FiltersEvent> for FiltersDialog {}

impl Focusable for FiltersDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl FiltersDialog {
    /// Opens the dialog with the focus in it.
    pub fn new(
        env: Env,
        filters: Entity<FileFilters>,
        context: FiltersContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let rules = filters.read(cx).rules().to_vec();
        let cursor = context
            .select
            .as_ref()
            .and_then(|n| rules.iter().position(|r| &r.name == n))
            .unwrap_or(0);
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Name of the button"));
        let globs = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 6)
                .placeholder("One glob per line, e.g. *.pb.go or gen/**")
        });
        let projects = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 4)
                .placeholder("One project per line; empty: all projects")
        });
        let picker = cx.new(|cx| ColorPickerState::new(window, cx));
        let subs = vec![
            cx.subscribe_in(
                &name,
                window,
                |this, _, ev: &InputEvent, window, cx| match ev {
                    InputEvent::Change => this.refresh(cx),
                    InputEvent::PressEnter { .. } => this.save_form(window, cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(&globs, window, |this, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.refresh(cx)
                }
            }),
            cx.subscribe_in(&projects, window, |this, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.refresh(cx)
                }
            }),
            cx.subscribe_in(&picker, window, |this, _, ev: &ColorPickerEvent, _, cx| {
                let ColorPickerEvent::Change(color) = ev;
                // Through the hex form, so a color equals what is saved.
                this.form.color = color.and_then(|c| parse_hex_color(&format_hex_color(c.into())));
                this.refresh(cx);
            }),
        ];
        let blank = form::new_rule(context.project.as_deref());
        let mut this = Self {
            env,
            filters,
            project: context.project,
            paths: context.paths,
            rules,
            cursor,
            mode: Mode::Browsing,
            form: Form::from_filter(&blank),
            baseline: blank,
            problem: None,
            preview: None,
            dirty: false,
            attempted: false,
            error: None,
            busy: false,
            name,
            globs,
            projects,
            picker,
            focus: cx.focus_handle(),
            only_focus: cx.focus_handle(),
            action_focus: cx.focus_handle(),
            bold_focus: cx.focus_handle(),
            enabled_focus: cx.focus_handle(),
            save_focus: cx.focus_handle(),
            cancel_focus: cx.focus_handle(),
            list_scroll: ScrollHandle::new(),
            form_scroll: ScrollHandle::new(),
            task: None,
            _subs: subs,
        };
        this.load_selected(window, cx);
        window.focus(&this.focus, cx);
        if context.start_adding {
            this.start_edit(this.rules.len(), window, cx);
        }
        this
    }

    #[cfg(test)]
    pub(crate) fn rules_for_test(&self) -> &[FileFilter] {
        &self.rules
    }

    #[cfg(test)]
    pub(crate) fn cursor_for_test(&self) -> usize {
        self.cursor
    }

    /// "browsing", "editing", "adding" or "confirming".
    #[cfg(test)]
    pub(crate) fn mode_for_test(&self) -> &'static str {
        match self.mode {
            Mode::Browsing => "browsing",
            Mode::Editing { index } if index >= self.rules.len() => "adding",
            Mode::Editing { .. } => "editing",
            Mode::Confirming(_) => "confirming",
        }
    }

    #[cfg(test)]
    pub(crate) fn error_for_test(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The problem of the form, if it is showing.
    #[cfg(test)]
    pub(crate) fn problem_for_test(&self) -> Option<&str> {
        self.shown_problem()
    }

    #[cfg(test)]
    pub(crate) fn preview_for_test(&self) -> Option<String> {
        self.preview.as_ref().map(Preview::text)
    }

    #[cfg(test)]
    pub(crate) fn form_for_test(&self, cx: &App) -> Form {
        self.read_form(cx)
    }

    #[cfg(test)]
    pub(crate) fn picker_for_test(&self) -> Entity<ColorPickerState> {
        self.picker.clone()
    }

    // ---- the form ----

    /// The form as it is now: the fields' texts and the other controls.
    fn read_form(&self, cx: &App) -> Form {
        Form {
            name: self.name.read(cx).value().to_string(),
            globs: self.globs.read(cx).value().to_string(),
            projects: self.projects.read(cx).value().to_string(),
            ..self.form.clone()
        }
    }

    /// Puts `rule` into the form.
    fn load(&mut self, rule: &FileFilter, window: &mut Window, cx: &mut Context<Self>) {
        self.form = Form::from_filter(rule);
        self.baseline = rule.clone();
        let form = self.form.clone();
        self.name
            .update(cx, |s, cx| s.set_value(form.name.clone(), window, cx));
        self.globs
            .update(cx, |s, cx| s.set_value(form.globs.clone(), window, cx));
        self.projects
            .update(cx, |s, cx| s.set_value(form.projects.clone(), window, cx));
        self.picker.update(cx, |p, cx| match form.color {
            Some(c) => p.set_value(c, window, cx),
            None => p.clear_value(window, cx),
        });
        self.refresh(cx);
    }

    /// Shows the rule under the cursor in the form.
    fn load_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rule = self
            .rules
            .get(self.cursor)
            .cloned()
            .unwrap_or_else(|| form::new_rule(self.project.as_deref()));
        self.load(&rule, window, cx);
    }

    /// Works out what the form says about itself: whether it can be saved,
    /// whether it changed, and what it matches. Runs when the form changes,
    /// not when it is drawn.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let rule = self.read_form(cx).build();
        let index = match self.mode {
            Mode::Editing { index } => index,
            _ => self.cursor,
        };
        self.problem = form::problem(&rule, &self.rules, index);
        self.dirty = rule != self.baseline;
        self.preview = match (&self.project, &self.paths) {
            (Some(project), Some(paths)) => Some(form::preview(&rule, project, paths)),
            _ => None,
        };
        cx.notify();
    }

    /// The problem to show: not before the form was touched.
    fn shown_problem(&self) -> Option<&str> {
        if self.dirty || self.attempted {
            self.problem.as_deref()
        } else {
            None
        }
    }

    // ---- moving about ----

    fn can_change(&self) -> bool {
        !self.busy
    }

    fn select(&mut self, to: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Browsing || self.rules.is_empty() {
            return;
        }
        self.cursor = to.min(self.rules.len() - 1);
        self.error = None;
        self.load_selected(window, cx);
    }

    fn down(&mut self, _: &RuleDown, window: &mut Window, cx: &mut Context<Self>) {
        self.select(self.cursor + 1, window, cx);
    }

    fn up(&mut self, _: &RuleUp, window: &mut Window, cx: &mut Context<Self>) {
        self.select(self.cursor.saturating_sub(1), window, cx);
    }

    // ---- editing ----

    fn edit(&mut self, _: &RuleEdit, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode == Mode::Browsing && self.can_change() {
            self.start_edit(self.cursor, window, cx);
        }
    }

    fn add(&mut self, _: &RuleAdd, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode == Mode::Browsing && self.can_change() {
            self.start_edit(self.rules.len(), window, cx);
        }
    }

    /// Starts editing the rule at `index`; one past the end starts a new
    /// rule, scoped to the change's project.
    fn start_edit(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let rule = self
            .rules
            .get(index)
            .cloned()
            .unwrap_or_else(|| form::new_rule(self.project.as_deref()));
        self.cursor = index;
        self.mode = Mode::Editing { index };
        self.attempted = false;
        self.error = None;
        self.load(&rule, window, cx);
        let handle = self.name.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// Back to the list, dropping what was typed.
    fn leave_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = Mode::Browsing;
        self.cursor = self.cursor.min(self.rules.len().saturating_sub(1));
        self.attempted = false;
        self.load_selected(window, cx);
        window.focus(&self.focus, cx);
    }

    fn close(&mut self, _: &CloseFilters, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        match self.mode {
            Mode::Browsing => cx.emit(FiltersEvent::Close),
            Mode::Editing { .. } => self.leave_edit(window, cx),
            Mode::Confirming(_) => {
                self.mode = Mode::Browsing;
                cx.notify();
            }
        }
    }

    fn save_action(&mut self, _: &SaveFilter, window: &mut Window, cx: &mut Context<Self>) {
        self.save_form(window, cx);
    }

    /// Writes the form's rule in place of the one being edited, or at the
    /// end for a new one.
    fn save_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Mode::Editing { index } = self.mode else {
            return;
        };
        if self.busy {
            return;
        }
        self.attempted = true;
        self.refresh(cx);
        if self.problem.is_some() {
            return;
        }
        let rule = self.read_form(cx).build();
        let select = Select::Name(rule.name.clone());
        let list = with_query(&self.rules, index, rule);
        self.persist(list, select, window, cx);
    }

    // ---- tab order ----

    fn tab_order(&self, cx: &App) -> Vec<FocusHandle> {
        let mut v = vec![
            self.name.read(cx).focus_handle(cx),
            self.globs.read(cx).focus_handle(cx),
            self.projects.read(cx).focus_handle(cx),
        ];
        if self.project.is_some() {
            v.push(self.only_focus.clone());
        }
        v.push(self.action_focus.clone());
        if self.form.kind == Kind::Highlight {
            v.push(self.picker.focus_handle(cx));
            v.push(self.bold_focus.clone());
        }
        v.push(self.enabled_focus.clone());
        v.push(self.save_focus.clone());
        v.push(self.cancel_focus.clone());
        v
    }

    fn move_focus(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.mode, Mode::Editing { .. }) {
            return;
        }
        let order = self.tab_order(cx);
        let len = order.len() as isize;
        let next = match order.iter().position(|h| h.contains_focused(window, cx)) {
            Some(i) => (i as isize + delta).rem_euclid(len) as usize,
            None if delta > 0 => 0,
            None => order.len() - 1,
        };
        window.focus(&order[next], cx);
    }

    fn next_field(&mut self, _: &FilterNextField, window: &mut Window, cx: &mut Context<Self>) {
        self.move_focus(1, window, cx);
    }

    fn prev_field(&mut self, _: &FilterPrevField, window: &mut Window, cx: &mut Context<Self>) {
        self.move_focus(-1, window, cx);
    }

    // ---- the controls ----

    fn set_kind(&mut self, kind: Kind, cx: &mut Context<Self>) {
        self.form.kind = kind;
        self.refresh(cx);
    }

    fn action_prev(&mut self, _: &ActionPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.set_kind(self.form.kind.step(-1), cx);
    }

    fn action_next(&mut self, _: &ActionNext, _: &mut Window, cx: &mut Context<Self>) {
        self.set_kind(self.form.kind.step(1), cx);
    }

    /// Space on the color field (Enter is gpui-kit's own).
    fn color_open(&mut self, _: &ColorOpen, _: &mut Window, cx: &mut Context<Self>) {
        self.picker.update(cx, |p, cx| p.toggle_open(cx));
    }

    fn only_this_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(project) = self.project.clone() {
            self.projects
                .update(cx, |s, cx| s.set_value(project, window, cx));
        }
    }

    fn no_color(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.form.color = None;
        self.picker.update(cx, |p, cx| p.clear_value(window, cx));
        self.refresh(cx);
    }

    // ---- delete and reorder ----

    fn delete(&mut self, _: &RuleDelete, _: &mut Window, cx: &mut Context<Self>) {
        if self.can_change() && self.mode == Mode::Browsing && self.cursor < self.rules.len() {
            self.mode = Mode::Confirming(self.cursor);
            cx.notify();
        }
    }

    fn delete_yes(&mut self, _: &RuleDeleteYes, window: &mut Window, cx: &mut Context<Self>) {
        if let Mode::Confirming(i) = self.mode
            && self.can_change()
        {
            let list = without(&self.rules, i);
            self.persist(list, Select::Index(i), window, cx);
        }
    }

    fn delete_no(&mut self, _: &RuleDeleteNo, _: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.mode, Mode::Confirming(_)) && !self.busy {
            self.mode = Mode::Browsing;
            cx.notify();
        }
    }

    fn move_down(&mut self, _: &RuleMoveDown, window: &mut Window, cx: &mut Context<Self>) {
        self.move_by(1, window, cx);
    }

    fn move_up(&mut self, _: &RuleMoveUp, window: &mut Window, cx: &mut Context<Self>) {
        self.move_by(-1, window, cx);
    }

    fn move_by(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_change() || self.mode != Mode::Browsing {
            return;
        }
        if let Some((list, to)) = moved(&self.rules, self.cursor, delta) {
            self.persist(list, Select::Index(to), window, cx);
        }
    }

    fn toggle_default(
        &mut self,
        _: &RuleToggleDefault,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_change() || self.mode != Mode::Browsing {
            return;
        }
        let mut list = self.rules.clone();
        if let Some(rule) = list.get_mut(self.cursor) {
            rule.enabled = !rule.enabled;
            let at = self.cursor;
            self.persist(list, Select::Index(at), window, cx);
        }
    }

    // ---- saving ----

    /// Writes `list` in the background. On success it becomes the list on
    /// screen and in the pages' shared state, and the cursor goes to
    /// `select`; on failure nothing changes but the message.
    fn persist(
        &mut self,
        list: Vec<FileFilter>,
        select: Select,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy = true;
        self.error = None;
        let backend = self.env.backend.clone();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { backend.save_filters(&list) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(saved) => {
                        this.filters
                            .update(cx, |f, cx| f.set_rules(saved.clone(), cx));
                        this.rules = saved;
                        this.cursor = match select {
                            Select::Name(name) => this.rules.iter().position(|r| r.name == name),
                            Select::Index(i) => Some(i),
                        }
                        .unwrap_or(0)
                        .min(this.rules.len().saturating_sub(1));
                        this.mode = Mode::Browsing;
                        this.attempted = false;
                        this.load_selected(window, cx);
                        window.focus(&this.focus, cx);
                    }
                    Err(e) => {
                        if matches!(this.mode, Mode::Confirming(_)) {
                            this.mode = Mode::Browsing;
                        }
                        this.error = Some(format!("Could not save the filters: {e}"));
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // ---- rendering ----

    fn key_context(&self) -> &'static str {
        match self.mode {
            Mode::Browsing => "FiltersDialog FiltersBrowsing",
            Mode::Editing { .. } => "FiltersDialog FiltersEditing",
            Mode::Confirming(_) => "FiltersDialog FiltersConfirming",
        }
    }

    fn render_row(&self, i: usize, rule: &FileFilter, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let selected = i == self.cursor;
        let row = v_flex()
            .id(("filter-row", i))
            .px_3()
            .py_1p5()
            .when(selected, |d| d.bg(theme.list_active))
            .on_click(cx.listener(move |this, _, window, cx| this.select(i, window, cx)));
        if self.mode == Mode::Confirming(i) {
            return row
                .gap_1()
                .child(
                    div()
                        .text_color(theme.danger)
                        .child(format!("Delete \u{201c}{}\u{201d}?  y / n", rule.name)),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("confirm-yes")
                                .label("Yes (y)")
                                .small()
                                .on_click(cx.listener(|this, _, w, cx| {
                                    this.delete_yes(&RuleDeleteYes, w, cx)
                                })),
                        )
                        .child(Button::new("confirm-no").label("No (n)").small().on_click(
                            cx.listener(|this, _, w, cx| this.delete_no(&RuleDeleteNo, w, cx)),
                        )),
                )
                .into_any_element();
        }
        row.child(
            h_flex()
                .gap_2()
                .items_center()
                .child(div().flex_1().min_w_0().truncate().child(rule.name.clone()))
                .when(!rule.enabled, |d| {
                    d.child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("off by default"),
                    )
                }),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .truncate()
                .child(form::summary(&rule.action)),
        )
        .into_any_element()
    }

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let adding = matches!(self.mode, Mode::Editing { index } if index >= self.rules.len());
        let mut rows = v_flex().w_full();
        for (i, rule) in self.rules.iter().enumerate() {
            rows = rows.child(self.render_row(i, rule, cx));
        }
        if adding {
            rows = rows.child(
                v_flex()
                    .id("filter-row-new")
                    .px_3()
                    .py_1p5()
                    .bg(theme.list_active)
                    .child("New filter")
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("not saved yet"),
                    ),
            );
        }
        if self.rules.is_empty() && !adding {
            rows = rows.child(
                div()
                    .px_3()
                    .py_2()
                    .text_color(theme.muted_foreground)
                    .child("No filters yet. Press a to add one."),
            );
        }
        v_flex()
            .w(px(280.))
            .flex_none()
            .h_full()
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .font_semibold()
                    .child("File filters"),
            )
            .child(
                div()
                    .id("filter-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.list_scroll)
                    .child(rows),
            )
            .child(
                v_flex()
                    .px_3()
                    .py_2()
                    .gap_1()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("a add \u{00b7} d delete \u{00b7} e edit")
                    .child("Shift-J / Shift-K reorder \u{00b7} Space on / off"),
            )
    }

    fn label(&self, text: &'static str, cx: &App) -> Div {
        div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(text)
    }

    /// One of the Hide / Dim / Highlight choices.
    fn render_kind(&self, kind: Kind, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = cx.theme().clone();
        let on = self.form.kind == kind;
        let hover = theme.list_hover;
        div()
            .id(kind.label())
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
            .child(kind.label())
            .on_click(cx.listener(move |this, _, _, cx| this.set_kind(kind, cx)))
    }

    fn render_form(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let editing = matches!(self.mode, Mode::Editing { .. });
        let action_ring = if self.action_focus.is_focused(window) {
            theme.ring
        } else {
            theme.transparent
        };
        let kinds: Vec<_> = Kind::ALL.iter().map(|k| self.render_kind(*k, cx)).collect();
        let dim_save = self.busy || self.problem.is_some();
        let project = self.project.clone();

        let highlight_row = (self.form.kind == Kind::Highlight).then(|| {
            h_flex()
                .gap_3()
                .items_center()
                .child(
                    div().w(px(150.)).child(
                        ColorSelect::new(&self.picker)
                            .placeholder("No color")
                            .small(),
                    ),
                )
                .child(
                    Button::new("no-color")
                        .label("No color")
                        .small()
                        .outline()
                        .disabled(self.form.color.is_none())
                        .on_click(cx.listener(|this, _, w, cx| this.no_color(w, cx))),
                )
                .child(
                    check(
                        "filter-bold",
                        "Bold",
                        self.form.bold,
                        &self.bold_focus,
                        false,
                        window,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.form.bold = !this.form.bold;
                        this.refresh(cx);
                    })),
                )
        });

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
                    .child(self.label("Globs, one per line", cx))
                    .child(Textarea::new(&self.globs).small()),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(self.label("Projects, one per line (empty: all projects)", cx))
                    .child(Textarea::new(&self.projects).small())
                    .when(project.is_some(), |d| {
                        d.child(
                            h_flex().child(
                                chip(
                                    "only-this-project",
                                    "Only this project",
                                    &self.only_focus,
                                    Look::Plain,
                                    false,
                                    window,
                                    cx,
                                )
                                .on_click(
                                    cx.listener(|this, _, w, cx| this.only_this_project(w, cx)),
                                ),
                            ),
                        )
                    }),
            )
            .child(
                v_flex().gap_1().child(self.label("Action", cx)).child(
                    h_flex()
                        .id("filter-action")
                        .key_context(ACTION_GROUP)
                        .track_focus(&self.action_focus)
                        .on_action(cx.listener(Self::action_prev))
                        .on_action(cx.listener(Self::action_next))
                        .gap_1()
                        .p_0p5()
                        .rounded_md()
                        .border_1()
                        .border_color(action_ring)
                        .self_start()
                        .children(kinds),
                ),
            )
            .children(highlight_row)
            .child(
                check(
                    "filter-enabled",
                    "On by default",
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
                })),
            )
            .when_some(self.preview.as_ref(), |d, p| {
                d.child(div().text_color(muted).child(p.text()))
            })
            .when_some(self.shown_problem(), |d, why| {
                d.child(div().text_color(theme.danger).child(why.to_owned()))
            })
            .when_some(self.error.clone(), |d, e| {
                d.child(div().text_color(theme.danger).child(e))
            });

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
                    .text_color(muted)
                    .child(if editing {
                        "Tab next \u{00b7} Esc discard"
                    } else {
                        "Enter edit \u{00b7} Esc close"
                    }),
            )
            .when(editing, |d| {
                d.child(
                    chip(
                        "filter-cancel",
                        "Cancel",
                        &self.cancel_focus,
                        Look::Plain,
                        self.busy,
                        window,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, w, cx| {
                        if !this.busy {
                            this.leave_edit(w, cx)
                        }
                    })),
                )
                .child(
                    chip(
                        "filter-save",
                        if self.busy { "Saving\u{2026}" } else { "Save" },
                        &self.save_focus,
                        Look::Primary,
                        dim_save,
                        window,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, w, cx| this.save_form(w, cx))),
                )
            });

        let shield = !editing && !self.rules.is_empty();
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("filter-form")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.form_scroll)
                            .when(shield, |d| d.opacity(0.75))
                            .child(body),
                    )
                    // Browsing: the form is shown, not used. A click on it
                    // starts editing.
                    .when(shield, |d| {
                        d.child(
                            div()
                                .id("filter-form-shield")
                                .absolute()
                                .inset_0()
                                .occlude()
                                .cursor_pointer()
                                .on_click(
                                    cx.listener(|this, _, w, cx| this.edit(&RuleEdit, w, cx)),
                                ),
                        )
                    }),
            )
            .child(footer)
            .into_any_element()
    }
}

impl Render for FiltersDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let show_form = !self.rules.is_empty() || matches!(self.mode, Mode::Editing { .. });
        let form = if show_form {
            self.render_form(window, cx)
        } else {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child("Press a to add a filter.")
                .into_any_element()
        };
        div()
            .id("filters-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(crate::theme::modal_backdrop(cx))
            .child(
                v_flex()
                    .id("filters-dialog")
                    .key_context(self.key_context())
                    .track_focus(&self.focus)
                    .on_action(cx.listener(Self::close))
                    .on_action(cx.listener(Self::save_action))
                    .on_action(cx.listener(Self::next_field))
                    .on_action(cx.listener(Self::prev_field))
                    .on_action(cx.listener(Self::down))
                    .on_action(cx.listener(Self::up))
                    .on_action(cx.listener(Self::edit))
                    .on_action(cx.listener(Self::add))
                    .on_action(cx.listener(Self::delete))
                    .on_action(cx.listener(Self::delete_yes))
                    .on_action(cx.listener(Self::delete_no))
                    .on_action(cx.listener(Self::move_down))
                    .on_action(cx.listener(Self::move_up))
                    .on_action(cx.listener(Self::toggle_default))
                    .on_action(cx.listener(Self::color_open))
                    .w(px(900.))
                    .h(px(600.))
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
                            .child(
                                div()
                                    .text_base()
                                    .font_semibold()
                                    .child("Manage file filters"),
                            )
                            .child(div().flex_1())
                            .when_some(self.project.clone(), |d, project| {
                                d.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(project),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .min_h_0()
                            .child(self.render_list(cx))
                            .child(form),
                    ),
            )
    }
}
