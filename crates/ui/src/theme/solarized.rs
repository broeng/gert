//! The Solarized palette as solarized-emacs (bbatsov/solarized-emacs) uses
//! it, and its mapping onto gpui-kit's theme tokens.
//!
//! Like solarized-emacs' `solarized-with-color-variables`, each variant swaps
//! the base tones so one mapping serves both: `base03` is always the
//! background, `base02` the highlight (hl-line, mode line), `base01` the
//! shadow (comments, secondary text), `base0` the body text and `base1` the
//! emphasized text. The accents are shared; the `-1bg`/`-2bg` tints (diff
//! rows and their refined parts) are per variant, copied from
//! `solarized-palettes.el`, except the brighter added-row green.

use gpui_kit::component::{ThemeConfig, ThemeMode};
use gpui_kit::{Hsla, rgb};
use serde_json::json;

const S_BASE03: u32 = 0x002b36;
const S_BASE02: u32 = 0x073642;
const S_BASE01: u32 = 0x586e75;
const S_BASE00: u32 = 0x657b83;
const S_BASE0: u32 = 0x839496;
const S_BASE1: u32 = 0x93a1a1;
const S_BASE2: u32 = 0xeee8d5;
const S_BASE3: u32 = 0xfdf6e3;

pub const YELLOW: u32 = 0xb58900;
const ORANGE: u32 = 0xcb4b16;
pub const RED: u32 = 0xdc322f;
const MAGENTA: u32 = 0xd33682;
const VIOLET: u32 = 0x6c71c4;
pub const BLUE: u32 = 0x268bd2;
/// solarized-emacs' `blue-d`, the same in both variants.
pub const BLUE_D: u32 = 0x0061a8;
const CYAN: u32 = 0x2aa198;
pub const GREEN: u32 = 0x859900;

/// Text on an accent background (buttons, chips): the light base in both
/// variants, as Solarized's accents are mid-tones.
pub const ON_ACCENT: u32 = S_BASE3;

/// One variant of the palette, with the base tones swapped for it.
pub struct Solarized {
    pub mode: ThemeMode,
    pub base03: u32,
    pub base02: u32,
    pub base01: u32,
    pub base00: u32,
    pub base0: u32,
    pub base1: u32,
    /// `s-line`: thin lines (window dividers, borders).
    pub line: u32,
    pub red_1bg: u32,
    pub red_2bg: u32,
    /// Added diff rows, and their gutters and refined parts. Not
    /// solarized-emacs' green-1bg/-2bg, which read as dimmed olive: a more
    /// saturated, glowing take on Solarized green (user preference).
    pub added_bg: u32,
    pub added_mark: u32,
    pub yellow_1bg: u32,
    pub yellow_2bg: u32,
}

pub const DARK: Solarized = Solarized {
    mode: ThemeMode::Dark,
    base03: S_BASE03,
    base02: S_BASE02,
    base01: S_BASE01,
    base00: S_BASE00,
    base0: S_BASE0,
    base1: S_BASE1,
    line: 0x284b54,
    red_1bg: 0x2d2c31,
    red_2bg: 0x532725,
    added_bg: 0x2d5514,
    added_mark: 0x3f7010,
    yellow_1bg: 0x273532,
    yellow_2bg: 0x433e20,
};

pub const LIGHT: Solarized = Solarized {
    mode: ThemeMode::Light,
    base03: S_BASE3,
    base02: S_BASE2,
    base01: S_BASE1,
    base00: S_BASE0,
    base0: S_BASE00,
    base1: S_BASE01,
    line: 0xcccec4,
    red_1bg: 0xffdec8,
    red_2bg: 0xffb9a1,
    added_bg: 0xe4f2b4,
    added_mark: 0xc8e46e,
    yellow_1bg: 0xf8e8c6,
    yellow_2bg: 0xf1d49b,
};

impl Solarized {
    pub fn for_dark(dark: bool) -> &'static Solarized {
        if dark { &DARK } else { &LIGHT }
    }

    /// The gpui-kit theme for this variant. Token choices follow the faces
    /// solarized-emacs sets: `default`, `hl-line`/`highlight` for hover and
    /// selection rows, `shadow` for muted text, `vertical-border` for borders,
    /// `company-tooltip` for popovers, `mode-line` for the title bar, and the
    /// `font-lock-*` faces for syntax. Links are blue rather than the face's
    /// yellow, since yellow already means "unresolved" and "warning" here.
    pub fn config(&self) -> Result<ThemeConfig, serde_json::Error> {
        serde_json::from_value(self.config_json())
    }

    fn config_json(&self) -> serde_json::Value {
        let a = |color: u32, alpha: u8| json!(format!("#{color:06x}{alpha:02x}"));
        let c = |color: u32| json!(hex(color));
        let style = |color: u32| json!({ "color": hex(color) });
        let bold = |color: u32| json!({ "color": hex(color), "font_weight": 700 });
        let name = match self.mode {
            ThemeMode::Dark => "Solarized Dark",
            ThemeMode::Light => "Solarized Light",
        };
        let colors = object([
            ("background", c(self.base03)),
            ("foreground", c(self.base0)),
            ("border", c(self.line)),
            ("input.border", c(self.base01)),
            ("muted.background", c(self.base02)),
            ("muted.foreground", c(self.base01)),
            ("accent.background", c(self.base02)),
            ("accent.foreground", c(self.base1)),
            ("list.hover.background", c(self.base02)),
            // gpui-kit caps the selected-row tint at 20% opacity.
            ("list.active.background", a(BLUE, 0x33)),
            ("list.active.border", c(BLUE)),
            ("list.even.background", c(self.base03)),
            ("list.head.background", c(self.base02)),
            ("popover.background", c(self.base02)),
            ("popover.foreground", c(self.base1)),
            ("primary.background", c(BLUE)),
            ("primary.active.background", c(BLUE_D)),
            ("primary.foreground", c(ON_ACCENT)),
            ("secondary.background", c(self.base02)),
            ("secondary.hover.background", c(self.line)),
            ("secondary.foreground", c(self.base1)),
            ("success.foreground", c(ON_ACCENT)),
            ("danger.background", c(RED)),
            ("danger.foreground", c(ON_ACCENT)),
            ("warning.foreground", c(ON_ACCENT)),
            ("info.background", c(BLUE)),
            ("info.foreground", c(ON_ACCENT)),
            ("link", c(BLUE)),
            ("ring", c(BLUE)),
            ("caret", c(self.base1)),
            // `region` inverts to base1; gpui-kit caps selection at 30%.
            ("selection.background", a(self.base1, 0x4d)),
            ("scrollbar.thumb.background", c(self.base01)),
            ("scrollbar.thumb.hover.background", c(self.base00)),
            ("title_bar.background", c(self.base02)),
            ("title_bar.border", c(self.line)),
            ("status_bar.background", c(self.base02)),
            ("tab_bar.background", c(self.base03)),
            ("tab.background", c(self.base02)),
            ("tab.foreground", c(self.base01)),
            ("tab.active.background", c(self.base03)),
            ("tab.active.foreground", c(self.base1)),
            ("skeleton.background", c(self.base02)),
            ("switch.background", c(self.base01)),
            ("description_list.label.background", c(self.base02)),
            ("window.border", c(self.line)),
            ("base.red", c(RED)),
            ("base.green", c(GREEN)),
            ("base.yellow", c(YELLOW)),
            ("base.blue", c(BLUE)),
            ("base.magenta", c(MAGENTA)),
            ("base.cyan", c(CYAN)),
        ]);
        let syntax = object([
            ("attribute", style(BLUE)),
            ("boolean", bold(BLUE)),
            ("comment", style(self.base01)),
            ("comment_doc", style(CYAN)),
            ("constant", bold(BLUE)),
            ("constructor", style(YELLOW)),
            ("embedded", style(self.base0)),
            ("emphasis", json!({ "font_style": "italic" })),
            ("emphasis.strong", bold(self.base1)),
            ("enum", style(YELLOW)),
            ("function", style(BLUE)),
            ("keyword", bold(GREEN)),
            ("link_text", style(YELLOW)),
            ("link_uri", style(BLUE)),
            ("preproc", style(BLUE)),
            ("string", style(CYAN)),
            ("string.escape", style(VIOLET)),
            ("string.regex", style(CYAN)),
            ("string.special", style(YELLOW)),
            ("string.special.symbol", style(VIOLET)),
            ("tag", style(BLUE)),
            ("tag.doctype", style(self.base01)),
            ("text.literal", style(CYAN)),
            ("title", bold(self.base1)),
            ("type", style(YELLOW)),
            ("variable.special", style(GREEN)),
            ("punctuation.special", style(ORANGE)),
        ]);
        // These are the field names of gpui-kit's highlighter stub, which is
        // what our build has (no `tree-sitter` feature); the full highlighter
        // spells them `editor.background` etc. The stub has no status colors.
        let mut highlight = object([
            ("editor_background", c(self.base03)),
            ("editor_foreground", c(self.base0)),
            ("editor_active_line", c(self.base02)),
            ("editor_line_number", c(self.base01)),
            ("editor_active_line_number", c(self.base0)),
            ("editor_invisible", c(self.base01)),
        ]);
        highlight["syntax"] = syntax;
        json!({
            "name": name,
            "mode": if self.mode.is_dark() { "dark" } else { "light" },
            "colors": colors,
            "highlight": highlight,
        })
    }
}

/// A JSON object of `entries`, for the theme's long flat key lists (too long
/// for one `json!`, which hits the macro recursion limit).
fn object<const N: usize>(entries: [(&str, serde_json::Value); N]) -> serde_json::Value {
    serde_json::Value::Object(entries.into_iter().map(|(k, v)| (k.into(), v)).collect())
}

fn hex(color: u32) -> String {
    format!("#{color:06x}")
}

pub fn color(c: u32) -> Hsla {
    rgb(c).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key we set must be one gpui-kit knows: serde ignores unknown
    /// keys, so a typo would silently fall back to gpui-kit's default color.
    #[test]
    fn every_key_is_known_to_gpui_kit() {
        for variant in [&DARK, &LIGHT] {
            let source = variant.config_json();
            let config = variant.config().expect("the theme parses");
            let parsed = serde_json::to_value(&config).expect("serializes");
            for path in [&["colors"][..], &["highlight"], &["highlight", "syntax"]] {
                let at = |v: &serde_json::Value| path.iter().fold(v.clone(), |v, k| v[k].clone());
                let (source, parsed) = (at(&source), at(&parsed));
                for (key, value) in source.as_object().expect("an object") {
                    if !value.is_object() || path.len() > 1 {
                        assert!(
                            !parsed[key].is_null(),
                            "{}.{key} of {:?} is not a gpui-kit key",
                            path.join("."),
                            variant.mode
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn variants_swap_the_base_tones() {
        assert_eq!((DARK.base03, LIGHT.base03), (S_BASE03, S_BASE3));
        assert_eq!((LIGHT.base0, LIGHT.base1), (S_BASE00, S_BASE01));
        assert_eq!(DARK.config().expect("parses").mode, ThemeMode::Dark);
        assert_eq!(LIGHT.config().expect("parses").mode, ThemeMode::Light);
    }
}
