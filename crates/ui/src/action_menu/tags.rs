//! The topic and hashtag steps.
//!
//! Topic: one line, prefilled; Enter saves, an empty line clears it.
//! Hashtags: the current ones as chips (Delete or a click marks one for
//! removal, again to take it back) and a field for new ones, separated by
//! commas or spaces. Enter applies both in one request.

use std::collections::HashSet;

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::actions::*;
use super::edit::Edit;
use super::{ActionDialog, Step};

pub(super) struct Tags {
    pub topic: Entity<InputState>,
    pub add: Entity<InputState>,
    /// One focus handle per current hashtag.
    pub chips: Vec<FocusHandle>,
    /// Current hashtags marked for removal.
    pub removed: HashSet<String>,
}

impl Tags {
    pub fn new(window: &mut Window, cx: &mut Context<ActionDialog>) -> Self {
        Self {
            topic: cx.new(|cx| InputState::new(window, cx).placeholder("Topic (empty clears it)")),
            add: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Add hashtags, separated by commas or spaces")
            }),
            chips: Vec::new(),
            removed: HashSet::new(),
        }
    }
}

/// Splits what was typed into hashtags: commas and white space separate
/// them, a leading `#` is dropped, duplicates are kept once.
pub(super) fn parse_hashtags(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in text.split(|c: char| c == ',' || c.is_whitespace()) {
        let t = t.trim().trim_start_matches('#');
        if !t.is_empty() && !out.iter().any(|o| o == t) {
            out.push(t.to_owned());
        }
    }
    out
}

impl ActionDialog {
    pub(super) fn enter_topic(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.step = Step::Topic;
        self.error = None;
        let current = self.detail.summary.topic.clone().unwrap_or_default();
        self.tags
            .topic
            .update(cx, |i, cx| i.set_value(current, window, cx));
        self.refocus_step(cx);
        cx.notify();
    }

    pub(super) fn enter_hashtags(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.step = Step::Hashtags;
        self.error = None;
        self.tags.removed.clear();
        self.tags
            .add
            .update(cx, |i, cx| i.set_value("", window, cx));
        self.tags.chips = (0..self.detail.summary.hashtags.len())
            .map(|_| cx.focus_handle())
            .collect();
        self.refocus_step(cx);
        cx.notify();
    }

    pub(super) fn save_topic(&mut self, cx: &mut Context<Self>) {
        if self.step != Step::Topic || self.running() {
            return;
        }
        let text = self.tags.topic.read(cx).value().trim().to_owned();
        if text == self.detail.summary.topic.as_deref().unwrap_or_default() {
            cx.emit(super::ActionEvent::Close);
            return;
        }
        self.run_edit(Edit::Topic((!text.is_empty()).then_some(text)), cx);
    }

    pub(super) fn save_hashtags(&mut self, cx: &mut Context<Self>) {
        if self.step != Step::Hashtags || self.running() {
            return;
        }
        let have = &self.detail.summary.hashtags;
        let add: Vec<String> = parse_hashtags(self.tags.add.read(cx).value().as_ref())
            .into_iter()
            .filter(|t| !have.contains(t))
            .collect();
        let remove: Vec<String> = have
            .iter()
            .filter(|t| self.tags.removed.contains(*t))
            .cloned()
            .collect();
        if add.is_empty() && remove.is_empty() {
            cx.emit(super::ActionEvent::Close);
            return;
        }
        self.run_edit(Edit::Hashtags { add, remove }, cx);
    }

    fn toggle_chip(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.step != Step::Hashtags || self.running() {
            return;
        }
        let Some(tag) = self.detail.summary.hashtags.get(ix).cloned() else {
            return;
        };
        if !self.tags.removed.remove(&tag) {
            self.tags.removed.insert(tag);
        }
        cx.notify();
    }

    pub(super) fn render_topic(&self) -> AnyElement {
        v_flex()
            .gap_1()
            .px_4()
            .py_2()
            .child(Input::new(&self.tags.topic).small())
            .into_any_element()
    }

    pub(super) fn render_hashtags(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let chips: Vec<_> =
            self.detail
                .summary
                .hashtags
                .iter()
                .enumerate()
                .filter_map(|(ix, tag)| {
                    let handle = self.tags.chips.get(ix)?.clone();
                    let gone = self.tags.removed.contains(tag);
                    let focused = handle.is_focused(window);
                    Some(
                        h_flex()
                            .id(("hashtag", ix))
                            .key_context(HASHTAG_CHIP)
                            .track_focus(&handle)
                            .on_action(cx.listener(move |this, _: &RemoveItem, _, cx| {
                                this.toggle_chip(ix, cx)
                            }))
                            .gap_1()
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .border_1()
                            .border_color(if focused { theme.ring } else { theme.border })
                            .cursor_pointer()
                            .when(gone, |d| d.opacity(0.5).line_through())
                            .child(format!("#{tag}"))
                            .child(div().text_color(theme.muted_foreground).child(if gone {
                                "\u{21b6}"
                            } else {
                                "\u{00d7}"
                            }))
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle_chip(ix, cx)))
                            .into_any_element(),
                    )
                })
                .collect();
        let none = chips.is_empty();
        v_flex()
            .gap_1()
            .px_4()
            .py_2()
            .child(Input::new(&self.tags.add).small())
            .child(
                div()
                    .pt_2()
                    .text_xs()
                    .font_semibold()
                    .text_color(theme.muted_foreground)
                    .child("CURRENT HASHTAGS"),
            )
            .child(h_flex().flex_wrap().gap_1().children(chips))
            .when(none, |d| {
                d.child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child("No hashtags yet."),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::parse_hashtags;

    #[test]
    fn hashtags_split_on_commas_and_spaces() {
        assert_eq!(
            parse_hashtags(" #a, b  c,,#a\td "),
            ["a", "b", "c", "d"].map(String::from)
        );
        assert!(parse_hashtags("  , ").is_empty());
    }
}
