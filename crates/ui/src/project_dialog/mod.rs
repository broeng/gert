//! The project checkout dialog: a modal form to add or edit the local git
//! checkout of one Gerrit project, opened from the Projects tab of the
//! settings page (`a`, `Enter`) and from the gear next to the project on the
//! change page.
//!
//! The form has the project name, the directory, and a Browse button that
//! opens the system's folder picker. The directory is kept the way it was
//! typed: `~` is not expanded here (the UI knows no home directory), the
//! backend expands it when it saves and when the checkout is opened. There
//! is no live check of the directory; the Projects tab shows whether it is a
//! git repository once the entry is saved.
//!
//! Saving does not write anything itself: the dialog emits
//! [`ProjectEvent::Save`] with the whole new list and the shell hands it to
//! [`crate::shell::Shell::save_projects`]. `Tab` / `Shift-Tab` cycle the
//! controls, `Ctrl-Enter` (or `Enter` in a field) saves, `Esc` discards.

pub mod actions;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::*;

use crate::{
    reply::{Look, chip},
    session::ProjectSettings,
};
use actions::*;

/// What the dialog tells the shell.
pub enum ProjectEvent {
    /// The user closed it without saving.
    Close,
    /// Save this list (the configured projects with the change made).
    Save(Vec<ProjectSettings>),
}

/// What the dialog is opened from.
#[derive(Clone, Debug, Default)]
pub struct ProjectContext {
    /// The projects configured on the server now.
    pub projects: Vec<ProjectSettings>,
    /// The project to edit; `None` adds a new one.
    pub edit: Option<String>,
    /// The name to start a new entry with (the change's project).
    pub name: Option<String>,
}

/// The list with the entry at `at` (`None`: a new one at the end) set to
/// `name` and `path`, or why that cannot be saved. The name and the path
/// are trimmed; the name must be new on this server, the path is kept as
/// written.
pub(crate) fn with_project(
    projects: &[ProjectSettings],
    at: Option<usize>,
    name: &str,
    path: &str,
) -> Result<Vec<ProjectSettings>, &'static str> {
    let (name, path) = (name.trim(), path.trim());
    if name.is_empty() {
        return Err("Enter the name of the Gerrit project.");
    }
    if projects
        .iter()
        .enumerate()
        .any(|(i, p)| Some(i) != at && p.name == name)
    {
        return Err("This project already has a checkout.");
    }
    if path.is_empty() {
        return Err("Enter the directory of the checkout.");
    }
    let entry = ProjectSettings::new(name, path);
    let mut list = projects.to_vec();
    match at.and_then(|i| list.get_mut(i)) {
        Some(slot) => *slot = entry,
        None => list.push(entry),
    }
    Ok(list)
}

pub struct ProjectDialog {
    projects: Vec<ProjectSettings>,
    /// The index of the entry being edited; `None` for a new one.
    at: Option<usize>,
    /// A save was tried: show the problem even for an untouched form.
    attempted: bool,
    name: Entity<InputState>,
    path: Entity<InputState>,
    focus: FocusHandle,
    browse_focus: FocusHandle,
    save_focus: FocusHandle,
    cancel_focus: FocusHandle,
    picking: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<ProjectEvent> for ProjectDialog {}

impl Focusable for ProjectDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ProjectDialog {
    /// Opens the dialog with the focus on the name (on the directory when
    /// the name is given already).
    pub fn new(context: ProjectContext, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let at = context
            .edit
            .as_ref()
            .and_then(|n| context.projects.iter().position(|p| p.name == *n));
        let (name, path) = match at.and_then(|i| context.projects.get(i)) {
            Some(p) => (p.name.clone(), p.written.clone()),
            None => (context.name.clone().unwrap_or_default(), String::new()),
        };
        let name_input = cx.new(|cx| InputState::new(window, cx).placeholder("demo/project"));
        let path_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("~/src/project or /path/to/repo"));
        let subs = [&name_input, &path_input]
            .into_iter()
            .map(|input| {
                cx.subscribe_in(
                    input,
                    window,
                    |this, _, ev: &InputEvent, window, cx| match ev {
                        InputEvent::PressEnter { .. } => this.save(window, cx),
                        InputEvent::Change => cx.notify(),
                        _ => {}
                    },
                )
            })
            .collect();
        let focus_path = !name.is_empty() && path.is_empty();
        name_input.update(cx, |s, cx| s.set_value(name, window, cx));
        path_input.update(cx, |s, cx| s.set_value(path, window, cx));
        let this = Self {
            projects: context.projects,
            at,
            attempted: false,
            name: name_input,
            path: path_input,
            focus: cx.focus_handle(),
            browse_focus: cx.focus_handle(),
            save_focus: cx.focus_handle(),
            cancel_focus: cx.focus_handle(),
            picking: None,
            _subs: subs,
        };
        let first = if focus_path { &this.path } else { &this.name };
        let handle = first.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        this
    }

    #[cfg(test)]
    pub(crate) fn name_input_for_test(&self) -> Entity<InputState> {
        self.name.clone()
    }

    #[cfg(test)]
    pub(crate) fn path_input_for_test(&self) -> Entity<InputState> {
        self.path.clone()
    }

    #[cfg(test)]
    pub(crate) fn problem_for_test(&self, cx: &App) -> Option<&'static str> {
        self.problem(cx)
    }

    /// Whether this edits an entry (else it adds one).
    #[cfg(test)]
    pub(crate) fn is_edit_for_test(&self) -> bool {
        self.at.is_some()
    }

    /// The list the form makes now, or why not.
    fn built(&self, cx: &App) -> Result<Vec<ProjectSettings>, &'static str> {
        with_project(
            &self.projects,
            self.at,
            self.name.read(cx).value().as_ref(),
            self.path.read(cx).value().as_ref(),
        )
    }

    /// The problem to show: not before a save was tried.
    fn problem(&self, cx: &App) -> Option<&'static str> {
        if self.attempted {
            self.built(cx).err()
        } else {
            None
        }
    }

    // ---- keys ----

    fn close(&mut self, _: &CloseProject, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ProjectEvent::Close);
    }

    fn save_action(&mut self, _: &SaveProject, window: &mut Window, cx: &mut Context<Self>) {
        self.save(window, cx);
    }

    fn tab_order(&self, cx: &App) -> Vec<FocusHandle> {
        vec![
            self.name.read(cx).focus_handle(cx),
            self.path.read(cx).focus_handle(cx),
            self.browse_focus.clone(),
            self.save_focus.clone(),
            self.cancel_focus.clone(),
        ]
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

    fn next_field(&mut self, _: &ProjectNextField, window: &mut Window, cx: &mut Context<Self>) {
        self.move_focus(1, window, cx);
    }

    fn prev_field(&mut self, _: &ProjectPrevField, window: &mut Window, cx: &mut Context<Self>) {
        self.move_focus(-1, window, cx);
    }

    // ---- actions ----

    fn save(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.attempted = true;
        match self.built(cx) {
            Ok(list) => cx.emit(ProjectEvent::Save(list)),
            Err(_) => cx.notify(),
        }
    }

    /// Opens the system's folder picker and puts the folder in the path
    /// field. The picker runs outside the app, so this only waits for it.
    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose the checkout".into()),
        });
        self.picking = Some(cx.spawn_in(window, async move |this, cx| {
            // A cancelled picker, or one that could not be opened, changes
            // nothing.
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                let text = dir.to_string_lossy().into_owned();
                this.path.update(cx, |s, cx| s.set_value(text, window, cx));
                cx.notify();
            })
            .ok();
        }));
    }

    // ---- rendering ----

    fn label(&self, text: &'static str, cx: &App) -> Div {
        div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(text)
    }
}

impl Render for ProjectDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let title = if self.at.is_some() {
            "Edit project checkout"
        } else {
            "New project checkout"
        };
        let body = v_flex()
            .gap_3()
            .p_4()
            .child(
                v_flex()
                    .gap_1()
                    .child(self.label("Gerrit project", cx))
                    .child(Input::new(&self.name).small()),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(self.label("Checkout directory (~ is the home directory)", cx))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(Input::new(&self.path).small()),
                            )
                            .child(
                                chip(
                                    "project-browse",
                                    "Browse\u{2026}",
                                    &self.browse_focus,
                                    Look::Plain,
                                    false,
                                    window,
                                    cx,
                                )
                                .on_click(cx.listener(|this, _, w, cx| this.browse(w, cx))),
                            ),
                    ),
            )
            .child(div().text_xs().text_color(theme.muted_foreground).child(
                "Diffs and file lists of changes whose commits are in the checkout \
                         are read from it instead of the server.",
            ))
            .children(
                self.problem(cx)
                    .map(|why| div().text_color(theme.danger).child(why)),
            );
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
                    .child("Tab next \u{00b7} Enter saves \u{00b7} Esc discards"),
            )
            .child(
                chip(
                    "project-cancel",
                    "Cancel",
                    &self.cancel_focus,
                    Look::Plain,
                    false,
                    window,
                    cx,
                )
                .on_click(cx.listener(|this, _, w, cx| this.close(&CloseProject, w, cx))),
            )
            .child(
                chip(
                    "project-save",
                    "Save",
                    &self.save_focus,
                    Look::Primary,
                    false,
                    window,
                    cx,
                )
                .on_click(cx.listener(|this, _, w, cx| this.save(w, cx))),
            );
        div()
            .id("project-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(crate::theme::modal_backdrop(cx))
            .child(
                v_flex()
                    .id("project-dialog")
                    .key_context(PROJECT_DIALOG)
                    .track_focus(&self.focus)
                    .on_action(cx.listener(Self::close))
                    .on_action(cx.listener(Self::save_action))
                    .on_action(cx.listener(Self::next_field))
                    .on_action(cx.listener(Self::prev_field))
                    .w(px(560.))
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
                    .child(body)
                    .child(footer),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{ProjectSettings, with_project};

    fn p(name: &str, path: &str) -> ProjectSettings {
        ProjectSettings::new(name, path)
    }

    #[test]
    fn a_new_entry_needs_a_free_name_and_a_path() {
        let list = vec![p("a", "/a")];
        assert!(with_project(&list, None, " ", "/x").is_err());
        assert!(with_project(&list, None, "b", " ").is_err());
        assert!(with_project(&list, None, "a", "/x").is_err());
        assert_eq!(
            with_project(&list, None, " b ", " ~/b ").unwrap(),
            vec![p("a", "/a"), p("b", "~/b")]
        );
    }

    #[test]
    fn an_edit_replaces_in_place_and_may_keep_its_own_name() {
        let list = vec![p("a", "/a"), p("b", "/b")];
        assert_eq!(
            with_project(&list, Some(0), "a", "/z").unwrap(),
            vec![p("a", "/z"), p("b", "/b")]
        );
        assert_eq!(
            with_project(&list, Some(0), "c", "/a").unwrap(),
            vec![p("c", "/a"), p("b", "/b")]
        );
        assert!(with_project(&list, Some(0), "b", "/a").is_err());
    }
}
