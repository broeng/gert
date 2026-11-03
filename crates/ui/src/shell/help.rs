//! The `?` overlay, built from the same table as the keymap.

use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::*;

use crate::actions::{self, *};

use super::Shell;

impl Shell {
    pub(super) fn render_help(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        // Two columns: the first three groups on the left, the rest on the
        // right, so that the whole overlay fits a normal window.
        let mut columns: [Vec<AnyElement>; 2] = [Vec::new(), Vec::new()];
        for (i, g) in actions::help_groups().into_iter().enumerate() {
            let rows = g
                .entries
                .into_iter()
                .filter(|e| !e.keys.is_empty())
                .map(|e| {
                    h_flex()
                        .gap_3()
                        .child(
                            div()
                                .w(px(190.))
                                .flex_none()
                                .font_family(theme.mono_font_family.clone())
                                .child(e.keys),
                        )
                        .child(div().child(e.description))
                })
                .collect::<Vec<_>>();
            columns[usize::from(i >= 3)].push(
                v_flex()
                    .gap_0p5()
                    .pb_3()
                    .child(
                        div()
                            .font_semibold()
                            .text_color(theme.muted_foreground)
                            .child(g.title),
                    )
                    .children(rows)
                    .into_any_element(),
            );
        }
        let [left, right] = columns;
        div()
            .id("help-overlay")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.overlay)
            .on_click(cx.listener(|this, _, w, cx| this.toggle_help(&ToggleHelp, w, cx)))
            .child(
                v_flex()
                    .max_h_full()
                    .max_w_full()
                    .my_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .text_color(theme.popover_foreground)
                    .text_sm()
                    .child(
                        div()
                            .px_4()
                            .pt_3()
                            .pb_1()
                            .text_base()
                            .font_semibold()
                            .child("Keyboard shortcuts"),
                    )
                    .child(
                        // Two columns of groups; the whole panel scrolls if
                        // the window is too short.
                        v_flex()
                            .relative()
                            .min_h_0()
                            .child(
                                div()
                                    .id("help-scroll")
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .track_scroll(&self.help_scroll)
                                    .px_4()
                                    .py_2()
                                    .child(
                                        h_flex()
                                            .items_start()
                                            .gap_8()
                                            .child(v_flex().children(left))
                                            .child(v_flex().children(right)),
                                    ),
                            )
                            .vertical_scrollbar(&self.help_scroll),
                    )
                    .child(
                        div()
                            .px_4()
                            .pb_3()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Press ? or Esc to close"),
                    ),
            )
    }
}
