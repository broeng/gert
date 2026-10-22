//! The app's theme (Solarized, see [`solarized`]) and theme-derived colors
//! for markers and chips.

pub mod solarized;

use std::rc::Rc;

use gpui_kit::component::button::ButtonCustomVariant;
use gpui_kit::component::{ActiveTheme as _, Theme};
use gpui_kit::*;

use crate::{labels::Mark, row::Chip};
use solarized::Solarized;

/// Replaces gpui-kit's default light and dark themes with Solarized, and
/// applies the one for the current mode. Following the system appearance
/// (`Theme::sync_system_appearance`) then switches between the two.
pub fn install(cx: &mut App) {
    let (dark, light) = match (solarized::DARK.config(), solarized::LIGHT.config()) {
        (Ok(dark), Ok(light)) => (dark, light),
        (Err(err), _) | (_, Err(err)) => {
            log::error!("the Solarized theme does not parse, keeping the default: {err}");
            return;
        }
    };
    Theme::update(cx, |theme| {
        theme.dark_theme = Rc::new(dark);
        theme.light_theme = Rc::new(light);
    });
    let mode = cx.theme().mode;
    Theme::change(mode, None, cx);
}

/// The Solarized variant of the active mode.
pub fn palette(cx: &App) -> &'static Solarized {
    Solarized::for_dark(cx.theme().is_dark())
}

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

/// Background of the commit message card: Solarized's faint yellow
/// (`yellow-1bg`, the tooltip face), so it stands out from the columns
/// beside it.
pub fn commit_card_background(cx: &App) -> Hsla {
    solarized::color(palette(cx).yellow_1bg)
}

/// The blue of the quick-vote buttons: Solarized blue, which reads well on
/// light and dark backgrounds.
pub fn vote_blue() -> Hsla {
    solarized::color(solarized::BLUE)
}

/// Style of the quick-vote buttons. The button paints its idle background at
/// a fifth of `color`, so it shows a light blue that turns `vote_blue` on
/// hover. A `strong` button sets `vote_blue` as its idle background itself,
/// and hovers a little lighter.
pub fn vote_button(strong: bool, cx: &App) -> ButtonCustomVariant {
    let blue = vote_blue();
    let hover = if strong {
        hsla(blue.h, blue.s, blue.l + 0.06, 1.)
    } else {
        blue
    };
    ButtonCustomVariant::new(cx)
        .color(blue)
        .hover(hover)
        .active(solarized::color(solarized::BLUE_D))
        .foreground(solarized::color(solarized::ON_ACCENT))
}

/// The scrim behind the modal dialogs (reply, change actions, filters).
/// The theme's `overlay` token is 5% black, which hardly shows on a dark
/// page, so the dialogs use this instead.
pub fn modal_backdrop(cx: &App) -> Hsla {
    let alpha = if cx.theme().is_dark() { 0.55 } else { 0.3 };
    hsla(0., 0., 0., alpha)
}
