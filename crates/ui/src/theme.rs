//! The app's themes ([`cwc`], [`solarized`]), the one in use, and
//! theme-derived colors for markers and chips.

pub mod cwc;
pub mod solarized;
mod spec;

use std::rc::Rc;

use gpui_kit::component::button::ButtonCustomVariant;
use gpui_kit::component::{ActiveTheme as _, Theme};
use gpui_kit::*;

use crate::{labels::Mark, row::Chip};
use spec::{ExtraColors, ThemeSpec};

/// A theme the user can pick. Each has a light and a dark variant; the
/// system appearance picks between them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeName {
    #[default]
    Cwc,
    Solarized,
}

impl ThemeName {
    pub const ALL: [ThemeName; 2] = [ThemeName::Cwc, ThemeName::Solarized];

    /// The name shown in the settings.
    pub fn label(self) -> &'static str {
        match self {
            ThemeName::Cwc => "CWC",
            ThemeName::Solarized => "Solarized",
        }
    }

    /// The value in the config file.
    pub fn key(self) -> &'static str {
        match self {
            ThemeName::Cwc => "cwc",
            ThemeName::Solarized => "solarized",
        }
    }

    /// The theme for a config value, ignoring case and surrounding blanks.
    pub fn from_key(key: &str) -> Option<ThemeName> {
        let key = key.trim();
        Self::ALL
            .into_iter()
            .find(|t| t.key().eq_ignore_ascii_case(key))
    }

    /// The next theme in [`Self::ALL`], wrapping around.
    pub fn next(self) -> ThemeName {
        let at = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(at + 1) % Self::ALL.len()]
    }

    fn spec(self, dark: bool) -> ThemeSpec {
        match self {
            ThemeName::Cwc => cwc::spec(dark),
            ThemeName::Solarized => solarized::spec(dark),
        }
    }
}

/// The theme in use, with the extra colors of both its variants.
struct Current {
    name: ThemeName,
    light: ExtraColors,
    dark: ExtraColors,
}

impl Global for Current {}

/// Makes `name` the theme: replaces gpui-kit's light and dark themes with
/// its variants and applies the one for the current mode. Following the
/// system appearance (`Theme::sync_system_appearance`) then switches
/// between the two.
pub fn set(name: ThemeName, cx: &mut App) {
    let (light, dark) = (name.spec(false), name.spec(true));
    let configs = (light.config(), dark.config());
    let (Ok(light_config), Ok(dark_config)) = configs else {
        // A test checks every theme parses; this is the fallback anyway.
        log::error!(
            "the {} theme does not parse; keeping the current one",
            name.label()
        );
        return;
    };
    cx.set_global(Current {
        name,
        light: light.extra,
        dark: dark.extra,
    });
    Theme::update(cx, |theme| {
        theme.light_theme = Rc::new(light_config);
        theme.dark_theme = Rc::new(dark_config);
    });
    let mode = cx.theme().mode;
    Theme::change(mode, None, cx);
}

/// The theme in use (the default before [`set`]).
pub fn current(cx: &App) -> ThemeName {
    cx.try_global::<Current>()
        .map_or_else(ThemeName::default, |c| c.name)
}

/// The extra colors of the theme and mode in use.
fn extra(cx: &App) -> ExtraColors {
    let dark = cx.theme().is_dark();
    match cx.try_global::<Current>() {
        Some(c) if dark => c.dark,
        Some(c) => c.light,
        None => ThemeName::default().spec(dark).extra,
    }
}

/// Diff colors outside gpui-kit's tokens, for the theme and mode in use.
pub struct DiffColors {
    pub removed_row: Hsla,
    pub removed_mark: Hsla,
    pub added_row: Hsla,
    pub added_mark: Hsla,
    pub comment_mark: Hsla,
}

pub fn diff_colors(cx: &App) -> DiffColors {
    let e = extra(cx);
    DiffColors {
        removed_row: color(e.removed_row),
        removed_mark: color(e.removed_mark),
        added_row: color(e.added_row),
        added_mark: color(e.added_mark),
        comment_mark: color(e.comment_mark),
    }
}

fn color(c: u32) -> Hsla {
    rgb(c).into()
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

/// Background of the commit message card: a faint tint of the theme's, so
/// it stands out from the columns beside it.
pub fn commit_card_background(cx: &App) -> Hsla {
    color(extra(cx).commit_card)
}

/// Style of the quick-vote buttons, in the theme's primary color. The
/// button paints its idle background at a fifth of `color`, so it shows a
/// faint tint. A `strong` button gets the full color as its idle background
/// from the caller, and hovers a little lighter. On a light page the faint
/// tint is too pale for the light accent text, so there the other buttons
/// use the darker primary for text and a stronger tint on hover; on a dark
/// page the tint is dark and they turn the full color on hover. A `reject`
/// button (a negative vote) does all that in the theme's danger color.
pub fn vote_button(strong: bool, reject: bool, cx: &App) -> ButtonCustomVariant {
    let theme = cx.theme();
    let (primary, primary_foreground, primary_active) = if reject {
        (theme.danger, theme.danger_foreground, theme.danger_active)
    } else {
        (
            theme.primary,
            theme.primary_foreground,
            theme.primary_active,
        )
    };
    let (hover, foreground) = if strong {
        (
            hsla(primary.h, primary.s, primary.l + 0.06, 1.),
            primary_foreground,
        )
    } else if theme.is_dark() {
        (primary, primary_foreground)
    } else {
        (primary.opacity(0.35), primary_active)
    };
    ButtonCustomVariant::new(cx)
        .color(primary)
        .hover(hover)
        .active(primary_active)
        .foreground(foreground)
}

/// The scrim behind the modal dialogs (reply, change actions, filters).
/// The theme's `overlay` token is 5% black, which hardly shows on a dark
/// page, so the dialogs use this instead.
pub fn modal_backdrop(cx: &App) -> Hsla {
    let alpha = if cx.theme().is_dark() { 0.55 } else { 0.3 };
    hsla(0., 0., 0., alpha)
}

#[cfg(test)]
mod tests {
    use super::ThemeName;

    #[test]
    fn config_keys_round_trip() {
        for t in ThemeName::ALL {
            assert_eq!(ThemeName::from_key(t.key()), Some(t));
        }
        assert_eq!(
            ThemeName::from_key(" Solarized "),
            Some(ThemeName::Solarized)
        );
        assert_eq!(ThemeName::from_key("zenburn"), None);
    }

    #[test]
    fn next_cycles_through_all() {
        assert_eq!(ThemeName::Cwc.next(), ThemeName::Solarized);
        assert_eq!(ThemeName::Solarized.next(), ThemeName::Cwc);
    }
}
