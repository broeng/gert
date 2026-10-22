//! Cards: a titled box drawn row by row, so it can sit in a virtual list or a
//! scroll container where every row is a separate child.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The title bar at the top of a card.
pub(crate) fn title_bar(cx: &App) -> Div {
    let theme = cx.theme();
    div().px_3().py_2().rounded_t_md().bg(theme.secondary)
}

/// A separator line above a row that is not the first of its card.
pub(crate) fn row_separator(cx: &App) -> Div {
    div()
        .border_t_1()
        .border_color(cx.theme().border.opacity(0.6))
}

/// One row of a card: side borders all the way, the top border and rounded
/// corners on the `first` row, the bottom ones on the `last`.
pub(crate) fn frame(row: impl IntoElement, first: bool, last: bool, cx: &App) -> Div {
    let theme = cx.theme();
    let card = div()
        .w_full()
        .border_x_1()
        .border_color(theme.border)
        .bg(theme.secondary.opacity(0.3))
        .when(first, |d| d.border_t_1().rounded_t_md())
        .when(last, |d| d.border_b_1().rounded_b_md())
        .child(row);
    div().w_full().px_4().when(first, |d| d.pt_3()).child(card)
}
