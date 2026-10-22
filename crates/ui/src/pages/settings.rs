//! The settings page. It edits the saved queries of the dashboard; more
//! sections (startup page, review flow) are meant to follow below them.
//!
//! Every committed change is written straight away through the backend
//! (the config file), off the main thread. The list shown is the one the
//! backend returns, so what is on screen is what is on disk.

use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::{
    actions::*,
    card,
    hint::Hint,
    session::{Env, SavedQuery},
};

/// What the page tells the shell.
pub enum SettingsEvent {
    /// The saved queries were written; this is the list now in effect.
    QueriesSaved(Vec<SavedQuery>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Browsing,
    /// Editing row `index`; `index == len` is a new row being added.
    Editing {
        index: usize,
    },
    /// Asking whether to delete row `index`.
    Confirming(usize),
}

// ---- pure list logic ----

/// The query for the typed name and text, or why it cannot be saved.
pub(crate) fn validate(name: &str, query: &str) -> Result<SavedQuery, &'static str> {
    let (name, query) = (name.trim(), query.trim());
    if name.is_empty() {
        Err("Give the query a name.")
    } else if query.is_empty() {
        Err("Enter the query text.")
    } else {
        Ok(SavedQuery {
            name: name.to_owned(),
            query: query.to_owned(),
        })
    }
}

/// `list` with row `index` moved by `delta` places, and where it ended up;
/// `None` at the ends of the list.
pub(crate) fn moved(
    list: &[SavedQuery],
    index: usize,
    delta: isize,
) -> Option<(Vec<SavedQuery>, usize)> {
    let to = index
        .checked_add_signed(delta)
        .filter(|t| *t < list.len())?;
    if index >= list.len() {
        return None;
    }
    let mut out = list.to_vec();
    let item = out.remove(index);
    out.insert(to, item);
    Some((out, to))
}

/// `list` with row `index` replaced by `q`, or `q` appended when `index` is
/// past the end.
pub(crate) fn with_query(list: &[SavedQuery], index: usize, q: SavedQuery) -> Vec<SavedQuery> {
    let mut out = list.to_vec();
    match out.get_mut(index) {
        Some(slot) => *slot = q,
        None => out.push(q),
    }
    out
}

/// `list` without row `index`.
pub(crate) fn without(list: &[SavedQuery], index: usize) -> Vec<SavedQuery> {
    let mut out = list.to_vec();
    if index < out.len() {
        out.remove(index);
    }
    out
}

pub struct SettingsPage {
    env: Env,
    queries: Vec<SavedQuery>,
    cursor: usize,
    mode: Mode,
    name: Entity<InputState>,
    query: Entity<InputState>,
    /// Why the row being edited cannot be saved.
    invalid: Option<&'static str>,
    /// A save is in flight: keys that change the list wait.
    busy: bool,
    hint: Hint,
    focus: FocusHandle,
    scroll: ScrollHandle,
    task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<SettingsEvent> for SettingsPage {}

impl Focusable for SettingsPage {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl SettingsPage {
    pub fn new(
        env: Env,
        queries: Vec<SavedQuery>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Name"));
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Query, e.g. is:open"));
        let subs = [&name, &query]
            .into_iter()
            .map(|input| {
                cx.subscribe_in(input, window, |this, _, ev: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        this.commit_edit(window, cx);
                    }
                })
            })
            .collect();
        Self {
            env,
            queries,
            cursor: 0,
            mode: Mode::Browsing,
            name,
            query,
            invalid: None,
            busy: false,
            hint: Hint::default(),
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            task: None,
            _subs: subs,
        }
    }

    #[cfg(test)]
    pub(crate) fn queries_for_test(&self) -> &[SavedQuery] {
        &self.queries
    }

    #[cfg(test)]
    pub(crate) fn inputs_for_test(&self) -> (Entity<InputState>, Entity<InputState>) {
        (self.name.clone(), self.query.clone())
    }

    #[cfg(test)]
    pub(crate) fn hint_for_test(&self) -> Option<(String, bool)> {
        self.hint.current().map(|(t, e)| (t.to_owned(), e))
    }

    #[cfg(test)]
    pub(crate) fn editing_for_test(&self) -> bool {
        matches!(self.mode, Mode::Editing { .. })
    }

    /// Whether a key may change the list now.
    fn can_change(&self) -> bool {
        !self.busy
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.cursor + 1, cx);
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.cursor.saturating_sub(1), cx);
    }

    fn select(&mut self, to: usize, cx: &mut Context<Self>) {
        if self.mode == Mode::Browsing && !self.queries.is_empty() {
            self.cursor = to.min(self.queries.len() - 1);
            cx.notify();
        }
    }

    // ---- editing ----

    fn edit_query(&mut self, _: &EditQuery, window: &mut Window, cx: &mut Context<Self>) {
        if self.can_change() && self.cursor < self.queries.len() {
            self.start_edit(self.cursor, window, cx);
        }
    }

    fn add_query(&mut self, _: &AddQuery, window: &mut Window, cx: &mut Context<Self>) {
        if self.can_change() {
            self.start_edit(self.queries.len(), window, cx);
        }
    }

    fn start_edit(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (name, query) = match self.queries.get(index) {
            Some(q) => (q.name.clone(), q.query.clone()),
            None => (String::new(), String::new()),
        };
        self.name.update(cx, |s, cx| s.set_value(name, window, cx));
        self.query
            .update(cx, |s, cx| s.set_value(query, window, cx));
        self.cursor = index;
        self.mode = Mode::Editing { index };
        self.invalid = None;
        self.name.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn cancel_edit(&mut self, _: &CancelEdit, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.mode, Mode::Editing { .. }) && !self.busy {
            self.leave_edit(window, cx);
        }
    }

    fn leave_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = Mode::Browsing;
        self.invalid = None;
        self.cursor = self.cursor.min(self.queries.len().saturating_sub(1));
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Tab / Shift-Tab between the two fields.
    fn other_field(&mut self, _: &NextField, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.mode, Mode::Editing { .. }) {
            return;
        }
        let on_name = self.name.focus_handle(cx).is_focused(window);
        let target = if on_name { &self.query } else { &self.name };
        target.update(cx, |s, cx| s.focus(window, cx));
    }

    fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Mode::Editing { index } = self.mode else {
            return;
        };
        if self.busy {
            return;
        }
        let name = self.name.read(cx).value().to_string();
        let text = self.query.read(cx).value().to_string();
        match validate(&name, &text) {
            Ok(q) => {
                self.invalid = None;
                let list = with_query(&self.queries, index, q);
                self.save(list, index, window, cx);
            }
            Err(why) => {
                self.invalid = Some(why);
                cx.notify();
            }
        }
    }

    // ---- delete ----

    fn delete_query(&mut self, _: &DeleteQuery, _: &mut Window, cx: &mut Context<Self>) {
        if self.can_change() && self.mode == Mode::Browsing && self.cursor < self.queries.len() {
            self.mode = Mode::Confirming(self.cursor);
            cx.notify();
        }
    }

    fn confirm_delete(&mut self, _: &ConfirmDelete, window: &mut Window, cx: &mut Context<Self>) {
        if let Mode::Confirming(i) = self.mode
            && self.can_change()
        {
            let list = without(&self.queries, i);
            self.save(list, i, window, cx);
        }
    }

    fn cancel_delete(&mut self, _: &CancelDelete, _: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.mode, Mode::Confirming(_)) && !self.busy {
            self.mode = Mode::Browsing;
            cx.notify();
        }
    }

    // ---- reorder ----

    fn move_down(&mut self, _: &MoveQueryDown, window: &mut Window, cx: &mut Context<Self>) {
        self.move_by(1, window, cx);
    }

    fn move_up(&mut self, _: &MoveQueryUp, window: &mut Window, cx: &mut Context<Self>) {
        self.move_by(-1, window, cx);
    }

    fn move_by(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_change() || self.mode != Mode::Browsing {
            return;
        }
        if let Some((list, to)) = moved(&self.queries, self.cursor, delta) {
            self.save(list, to, window, cx);
        }
    }

    // ---- saving ----

    /// Writes `list` in the background. On success it becomes the list on
    /// screen, the cursor goes to `cursor` and the shell is told; on failure
    /// nothing changes but a hint.
    fn save(
        &mut self,
        list: Vec<SavedQuery>,
        cursor: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy = true;
        let backend = self.env.backend.clone();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { backend.save_queries(&list) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(saved) => {
                        this.queries = saved.clone();
                        this.cursor = cursor.min(saved.len().saturating_sub(1));
                        this.leave_edit(window, cx);
                        cx.emit(SettingsEvent::QueriesSaved(saved));
                    }
                    Err(e) => {
                        if matches!(this.mode, Mode::Confirming(_)) {
                            this.mode = Mode::Browsing;
                        }
                        this.hint
                            .say(format!("Could not save the queries: {e}"), true, cx, |p| {
                                &mut p.hint
                            });
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
            Mode::Browsing => "SettingsPage Browsing",
            Mode::Editing { .. } => "SettingsPage Editing",
            Mode::Confirming(_) => "SettingsPage Confirming",
        }
    }

    fn render_row(&self, i: usize, q: &SavedQuery, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let selected = i == self.cursor;
        let buttons = h_flex()
            .gap_1()
            .flex_none()
            .child(
                Button::new(("q-up", i))
                    .label("\u{2191}")
                    .small()
                    .disabled(i == 0 || self.busy)
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.cursor = i;
                        this.move_up(&MoveQueryUp, w, cx)
                    })),
            )
            .child(
                Button::new(("q-down", i))
                    .label("\u{2193}")
                    .small()
                    .disabled(i + 1 >= self.queries.len() || self.busy)
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.cursor = i;
                        this.move_down(&MoveQueryDown, w, cx)
                    })),
            )
            .child(
                Button::new(("q-edit", i))
                    .label("Edit")
                    .small()
                    .disabled(self.busy)
                    .on_click(cx.listener(move |this, _, w, cx| {
                        if this.mode == Mode::Browsing && this.can_change() {
                            this.start_edit(i, w, cx);
                        }
                    })),
            )
            .child(
                Button::new(("q-delete", i))
                    .label("Delete")
                    .small()
                    .disabled(self.busy)
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.cursor = i;
                        this.delete_query(&DeleteQuery, w, cx)
                    })),
            );
        let row = h_flex()
            .id(("query-row", i))
            .px_3()
            .py_1p5()
            .gap_3()
            .items_center()
            .when(selected, |d| d.bg(theme.list_active))
            .on_click(cx.listener(move |this, _, _, cx| this.select(i, cx)));
        match self.mode {
            Mode::Editing { index } if index == i => self.render_editor(row, cx),
            Mode::Confirming(index) if index == i => row
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(theme.danger)
                        .child(format!("Delete \u{201c}{}\u{201d}?  y / n", q.name)),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("confirm-yes")
                                .label("Yes (y)")
                                .small()
                                .on_click(cx.listener(|this, _, w, cx| {
                                    this.confirm_delete(&ConfirmDelete, w, cx)
                                })),
                        )
                        .child(Button::new("confirm-no").label("No (n)").small().on_click(
                            cx.listener(|this, _, w, cx| this.cancel_delete(&CancelDelete, w, cx)),
                        )),
                )
                .into_any_element(),
            _ => row
                .child(
                    div()
                        .w(px(200.))
                        .flex_none()
                        .truncate()
                        .child(q.name.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .child(q.query.clone()),
                )
                .child(buttons)
                .into_any_element(),
        }
    }

    /// The row in edit mode: two inputs, and why it cannot be saved yet.
    fn render_editor(&self, row: Stateful<Div>, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        row.child(
            div()
                .w(px(200.))
                .flex_none()
                .child(Input::new(&self.name).small()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(Input::new(&self.query).small()),
        )
        .child(
            h_flex()
                .gap_1()
                .flex_none()
                .child(
                    Button::new("edit-save")
                        .label("Save")
                        .small()
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _, w, cx| this.commit_edit(w, cx))),
                )
                .child(
                    Button::new("edit-cancel").label("Cancel").small().on_click(
                        cx.listener(|this, _, w, cx| this.cancel_edit(&CancelEdit, w, cx)),
                    ),
                ),
        )
        .children(
            self.invalid
                .map(|why| div().text_sm().text_color(theme.danger).child(why)),
        )
        .into_any_element()
    }

    fn render_queries(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let adding = matches!(self.mode, Mode::Editing { index } if index >= self.queries.len());
        let n = self.queries.len();
        let mut body = v_flex().w_full().child(card::frame(
            card::title_bar(cx)
                .child(div().font_semibold().child("Saved queries"))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Each one is a section of the dashboard, in this order."),
                ),
            true,
            n == 0 && !adding,
            cx,
        ));
        if n == 0 && !adding {
            body = body.child(card::frame(
                card::row_separator(cx)
                    .px_3()
                    .py_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("No saved queries. Press a to add one."),
                false,
                true,
                cx,
            ));
        }
        for i in 0..n {
            let q = self.queries[i].clone();
            let row = self.render_row(i, &q, cx);
            body = body.child(card::frame(
                card::row_separator(cx).child(row),
                false,
                i + 1 == n && !adding,
                cx,
            ));
        }
        if adding {
            let row = h_flex()
                .id("query-row-new")
                .px_3()
                .py_1p5()
                .gap_3()
                .items_center()
                .bg(theme.list_active);
            let row = self.render_editor(row, cx);
            body = body.child(card::frame(
                card::row_separator(cx).child(row),
                false,
                true,
                cx,
            ));
        }
        v_flex()
            .gap_2()
            .child(body)
            .child(
                div().px_4().child(
                    Button::new("add-query")
                        .label("Add a query (a)")
                        .small()
                        .disabled(self.busy || self.mode != Mode::Browsing)
                        .on_click(cx.listener(|this, _, w, cx| this.add_query(&AddQuery, w, cx))),
                ),
            )
            .into_any_element()
    }

    fn render_hint(&self, cx: &App) -> Option<impl IntoElement> {
        let (text, error) = self.hint.current()?;
        let theme = cx.theme();
        Some(
            div()
                .px_4()
                .py_1()
                .text_xs()
                .text_color(if error {
                    theme.danger
                } else {
                    theme.foreground
                })
                .child(text.to_owned()),
        )
    }
}

impl Render for SettingsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let queries = self.render_queries(cx);
        v_flex()
            .id("settings")
            .key_context(self.key_context())
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::edit_query))
            .on_action(cx.listener(Self::add_query))
            .on_action(cx.listener(Self::delete_query))
            .on_action(cx.listener(Self::confirm_delete))
            .on_action(cx.listener(Self::cancel_delete))
            .on_action(cx.listener(Self::cancel_edit))
            .on_action(cx.listener(Self::other_field))
            .on_action(
                cx.listener(|this, _: &PrevField, w, cx| this.other_field(&NextField, w, cx)),
            )
            .on_action(cx.listener(Self::move_up))
            .on_action(cx.listener(Self::move_down))
            .size_full()
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .id("settings-scroll")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .py_4()
                            .gap_4()
                            .child(div().px_4().text_xl().font_semibold().child("Settings"))
                            .child(queries),
                    )
                    .vertical_scrollbar(&self.scroll),
            )
            .children(self.render_hint(cx))
            .child(
                div()
                    .px_4()
                    .py_1()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(
                        "j / k: select    Enter / e: edit    a: add    d: delete    \
                         Shift-J / Shift-K: move    u: back to the dashboard",
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{moved, validate, with_query, without};
    use crate::session::SavedQuery;

    fn q(n: &str) -> SavedQuery {
        SavedQuery {
            name: n.into(),
            query: format!("q:{n}"),
        }
    }

    fn names(l: &[SavedQuery]) -> Vec<&str> {
        l.iter().map(|q| q.name.as_str()).collect()
    }

    #[test]
    fn a_query_needs_a_name_and_text() {
        assert_eq!(validate("", "is:open"), Err("Give the query a name."));
        assert_eq!(validate("  ", "is:open"), Err("Give the query a name."));
        assert_eq!(validate("Open", " "), Err("Enter the query text."));
        assert_eq!(
            validate(" Open ", " is:open "),
            Ok(SavedQuery {
                name: "Open".into(),
                query: "is:open".into()
            })
        );
    }

    #[test]
    fn moving_swaps_with_the_neighbour_and_stops_at_the_ends() {
        let l = vec![q("a"), q("b"), q("c")];
        let (down, at) = moved(&l, 0, 1).unwrap();
        assert_eq!((names(&down), at), (vec!["b", "a", "c"], 1));
        let (up, at) = moved(&l, 2, -1).unwrap();
        assert_eq!((names(&up), at), (vec!["a", "c", "b"], 1));
        assert!(moved(&l, 0, -1).is_none());
        assert!(moved(&l, 2, 1).is_none());
        assert!(moved(&[], 0, 1).is_none());
    }

    #[test]
    fn replace_add_and_delete() {
        let l = vec![q("a"), q("b")];
        assert_eq!(names(&with_query(&l, 1, q("x"))), ["a", "x"]);
        assert_eq!(names(&with_query(&l, 2, q("x"))), ["a", "b", "x"]);
        assert_eq!(names(&without(&l, 0)), ["b"]);
        assert_eq!(names(&without(&l, 5)), ["a", "b"]);
        assert!(without(&without(&l, 0), 0).is_empty());
    }
}
