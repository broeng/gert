//! Theme-derived colors for markers and chips.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::*;

use crate::{labels::Mark, row::Chip};

pub fn mark_color(mark: &Mark, cx: &App) -> Hsla {
    match mark {
        Mark::Approved | Mark::Recommended(_) => cx.theme().success,
        Mark::Rejected | Mark::Disliked(_) => cx.theme().danger,
        Mark::None => cx.theme().muted_foreground,
    }
}

/// Foreground and background of a status chip.
pub fn chip_colors(chip: Chip, cx: &App) -> (Hsla, Hsla) {
    let fg = match chip {
        Chip::Merged => cx.theme().success,
        Chip::Abandoned => cx.theme().muted_foreground,
        Chip::Wip => cx.theme().warning,
        Chip::Private => cx.theme().info,
    };
    (fg, fg.opacity(0.15))
}

/// Background of the commit message card: a faded sticky-note yellow, so it
/// stands out from the columns beside it. The theme has no such token, so
/// light and dark get their own tint.
pub fn commit_card_background(cx: &App) -> Hsla {
    if cx.theme().is_dark() {
        hsla(48. / 360., 0.25, 0.19, 1.)
    } else {
        hsla(50. / 360., 0.85, 0.90, 1.)
    }
}
