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
