//! The app's themes ([`cwc`], [`solarized`]), the one in use, and
//! theme-derived colors for markers and chips.

pub mod cwc;
pub mod solarized;
mod spec;

use std::rc::Rc;

use gpui_kit::component::button::ButtonCustomVariant;
use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode};
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

/// Whether a part of the UI is light or dark, or follows the system.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorMode {
    #[default]
    System,
    Light,
    Dark,
}

impl ColorMode {
    pub const ALL: [ColorMode; 3] = [ColorMode::System, ColorMode::Light, ColorMode::Dark];

    /// The name shown in the settings.
    pub fn label(self) -> &'static str {
        match self {
            ColorMode::System => "Follow system",
            ColorMode::Light => "Light",
            ColorMode::Dark => "Dark",
        }
    }

    /// The value in the config file.
    pub fn key(self) -> &'static str {
        match self {
            ColorMode::System => "system",
            ColorMode::Light => "light",
            ColorMode::Dark => "dark",
        }
    }

    /// The mode for a config value, ignoring case and surrounding blanks.
    pub fn from_key(key: &str) -> Option<ColorMode> {
        let key = key.trim();
        Self::ALL
            .into_iter()
            .find(|m| m.key().eq_ignore_ascii_case(key))
    }

    /// The next mode in [`Self::ALL`], wrapping around.
    pub fn next(self) -> ColorMode {
        let at = Self::ALL.iter().position(|m| *m == self).unwrap_or(0);
        Self::ALL[(at + 1) % Self::ALL.len()]
    }

    /// Whether this mode is dark under the given system appearance.
    fn is_dark(self, system: WindowAppearance) -> bool {
        match self {
            ColorMode::System => ThemeMode::from(system).is_dark(),
            ColorMode::Light => false,
            ColorMode::Dark => true,
        }
    }
}

/// The light/dark preference of the UI and of the diff view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColorModes {
    pub ui: ColorMode,
    pub diff: ColorMode,
}

impl Global for ColorModes {}

/// The preferences in use (following the system before [`set_color_modes`]).
pub fn color_modes(cx: &App) -> ColorModes {
    cx.try_global::<ColorModes>().copied().unwrap_or_default()
}

/// Makes `modes` the preference and applies it to `window`.
pub fn set_color_modes(modes: ColorModes, window: &mut Window, cx: &mut App) {
    cx.set_global(modes);
    sync_appearance(window, cx);
}

/// Applies the UI's mode: the system appearance if it follows the system,
/// otherwise the chosen one. Call it when the system appearance changes.
/// The window is redrawn either way, as the diff view may follow the system
/// while the rest of the UI does not.
pub fn sync_appearance(window: &mut Window, cx: &mut App) {
    match color_modes(cx).ui {
        ColorMode::System => Theme::sync_system_appearance(Some(&mut *window), cx),
        ColorMode::Light => Theme::change(ThemeMode::Light, Some(&mut *window), cx),
        ColorMode::Dark => Theme::change(ThemeMode::Dark, Some(&mut *window), cx),
    }
    window.refresh();
}

/// The theme the diff view paints with: the UI's own unless the diff has
/// the other mode, then the other variant of the theme in use.
pub fn diff_theme(window: &Window, cx: &App) -> Theme {
    let ui = cx.theme();
    let dark = color_modes(cx).diff.is_dark(window.appearance());
    if dark == ui.is_dark() {
        return ui.clone();
    }
    let config = if dark {
        ui.dark_theme.clone()
    } else {
        ui.light_theme.clone()
    };
    let mut theme = ui.clone();
    theme.apply_config(&config);
    theme
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

/// The extra colors of the theme in use, for a light or dark mode.
fn extra(dark: bool, cx: &App) -> ExtraColors {
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

/// The diff colors for the mode of `theme`.
pub fn diff_colors(theme: &Theme, cx: &App) -> DiffColors {
    let e = extra(theme.is_dark(), cx);
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
        Chip::MergeConflict => cx.theme().danger,
    };
    (fg, fg.opacity(0.15))
}

/// Background of the commit message card: a faint tint of the theme's, so
/// it stands out from the columns beside it.
pub fn commit_card_background(cx: &App) -> Hsla {
    color(extra(cx.theme().is_dark(), cx).commit_card)
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

/// How many distinct server colors the palette has; later servers reuse
/// them in order.
pub const SERVER_PALETTE_LEN: usize = 8;

/// Server colors for a light page: dark enough to read as a dot or a line
/// on white (at least 3:1) and to carry text on a faint tint.
const SERVER_PALETTE_LIGHT: [u32; SERVER_PALETTE_LEN] = [
    0x2563eb, // blue
    0xd9480f, // orange
    0x2f9e44, // green
    0xae3ec9, // purple
    0x0c8599, // teal
    0xc2255c, // pink
    0x9c6b00, // amber
    0x5c6f82, // slate
];

/// The same hues for a dark page: lighter, so they keep their contrast.
const SERVER_PALETTE_DARK: [u32; SERVER_PALETTE_LEN] = [
    0x6ea8fe, 0xff9b5e, 0x69db7c, 0xd27cf0, 0x3bc9db, 0xf783ac, 0xffd43b, 0xa0aec0,
];

/// The color of the server at `position` in the config (0-based): the
/// user's `custom` `"#rrggbb"` if it parses, else the palette entry for the
/// position, from the variant that suits `theme`. By position, so the first
/// eight servers always differ; it moves when servers are reordered, which
/// is why a color that must stay put is set in the config.
pub fn server_color(position: usize, custom: Option<&str>, theme: &Theme) -> Hsla {
    if let Some(c) = custom.and_then(parse_hex_color) {
        return c;
    }
    let palette = if theme.is_dark() {
        &SERVER_PALETTE_DARK
    } else {
        &SERVER_PALETTE_LIGHT
    };
    color(palette[position % SERVER_PALETTE_LEN])
}

/// `#rrggbb` (the `#` is optional).
pub fn parse_hex_color(text: &str) -> Option<Hsla> {
    let hex = text.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok().map(color)
}

#[cfg(test)]
mod tests {
    use super::{
        ColorMode, SERVER_PALETTE_DARK, SERVER_PALETTE_LEN, SERVER_PALETTE_LIGHT, ThemeName,
        parse_hex_color,
    };

    fn luminance(c: u32) -> f32 {
        let channel = |shift: u32| {
            let v = ((c >> shift) & 0xff) as f32 / 255.;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
    }

    fn contrast(a: u32, b: u32) -> f32 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn server_palettes_are_distinct_and_readable() {
        for (palette, page) in [
            (SERVER_PALETTE_LIGHT, 0xffffff),
            (SERVER_PALETTE_DARK, 0x1c1c1c),
        ] {
            assert_eq!(palette.len(), SERVER_PALETTE_LEN);
            let mut seen = std::collections::HashSet::new();
            for c in palette {
                assert!(seen.insert(c), "{c:06x} twice");
                assert!(contrast(c, page) >= 3., "{c:06x} on {page:06x}");
            }
        }
    }

    #[test]
    fn hex_colors_parse() {
        assert!(parse_hex_color("#2563eb").is_some());
        assert!(parse_hex_color(" 2563EB ").is_some());
        assert!(parse_hex_color("#25").is_none());
        assert!(parse_hex_color("#zzzzzz").is_none());
        assert!(parse_hex_color("red").is_none());
    }

    #[test]
    fn color_mode_keys_round_trip() {
        for m in ColorMode::ALL {
            assert_eq!(ColorMode::from_key(m.key()), Some(m));
        }
        assert_eq!(ColorMode::from_key(" Dark "), Some(ColorMode::Dark));
        assert_eq!(ColorMode::from_key("auto"), None);
        assert_eq!(ColorMode::Dark.next(), ColorMode::System);
    }

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
