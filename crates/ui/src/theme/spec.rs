//! A theme as a small set of roles, and how the roles map onto gpui-kit's
//! theme tokens. Each theme ([`super::solarized`], [`super::cwc`]) fills in
//! one [`ThemeSpec`] per mode; the mapping to tokens is shared.

use gpui_kit::component::{ThemeConfig, ThemeMode};
use serde_json::json;

/// How a syntax token is drawn.
#[derive(Clone, Copy)]
pub struct Syntax {
    pub color: u32,
    pub bold: bool,
    pub italic: bool,
}

pub const fn fg(color: u32) -> Syntax {
    Syntax {
        color,
        bold: false,
        italic: false,
    }
}

pub const fn bold(color: u32) -> Syntax {
    Syntax {
        color,
        bold: true,
        italic: false,
    }
}

pub const fn italic(color: u32) -> Syntax {
    Syntax {
        color,
        bold: false,
        italic: true,
    }
}

/// Colors the app paints itself, outside gpui-kit's tokens.
#[derive(Clone, Copy)]
pub struct ExtraColors {
    /// Changed diff rows, and their gutters and refined (intraline) parts.
    pub removed_row: u32,
    pub removed_mark: u32,
    pub added_row: u32,
    pub added_mark: u32,
    /// The text a comment thread is anchored at.
    pub comment_mark: u32,
    /// The commit message card on the change page.
    pub commit_card: u32,
}

/// One theme in one mode. Colors are `0xrrggbb`.
pub struct ThemeSpec {
    pub name: &'static str,
    pub dark: bool,
    pub background: u32,
    /// Body text.
    pub foreground: u32,
    /// Emphasized text: popovers, secondary buttons, headings.
    pub emphasis: u32,
    /// Secondary text: dates, hints, column heads.
    pub muted_foreground: u32,
    /// Raised or highlighted surfaces: hovered rows, card heads, popovers,
    /// the title bar, secondary buttons.
    pub surface: u32,
    /// A hovered secondary button.
    pub surface_hover: u32,
    /// Thin lines: borders, dividers.
    pub border: u32,
    /// Input borders and the scrollbar thumb, which must stay visible.
    pub control: u32,
    pub control_hover: u32,
    pub primary: u32,
    pub primary_active: u32,
    /// Text on accent backgrounds (primary, success, danger, ...).
    pub on_accent: u32,
    pub success: u32,
    pub danger: u32,
    pub warning: u32,
    pub info: u32,
    pub link: u32,
    /// Text selection, drawn at 30% (gpui-kit's cap).
    pub selection: u32,
    pub red: u32,
    pub green: u32,
    pub yellow: u32,
    pub blue: u32,
    pub magenta: u32,
    pub cyan: u32,
    pub syntax: Vec<(&'static str, Syntax)>,
    pub extra: ExtraColors,
}

impl ThemeSpec {
    pub fn config(&self) -> Result<ThemeConfig, serde_json::Error> {
        serde_json::from_value(self.config_json())
    }

    pub(super) fn config_json(&self) -> serde_json::Value {
        let c = |color: u32| json!(hex(color));
        let a = |color: u32, alpha: u8| json!(format!("#{color:06x}{alpha:02x}"));
        let colors = object([
            ("background", c(self.background)),
            ("foreground", c(self.foreground)),
            ("border", c(self.border)),
            ("input.border", c(self.control)),
            ("muted.background", c(self.surface)),
            ("muted.foreground", c(self.muted_foreground)),
            ("accent.background", c(self.surface)),
            ("accent.foreground", c(self.emphasis)),
            ("list.hover.background", c(self.surface)),
            // gpui-kit caps the selected-row tint at 20% opacity.
            ("list.active.background", a(self.primary, 0x33)),
            ("list.active.border", c(self.primary)),
            ("list.even.background", c(self.background)),
            ("list.head.background", c(self.surface)),
            ("popover.background", c(self.surface)),
            ("popover.foreground", c(self.emphasis)),
            ("primary.background", c(self.primary)),
            ("primary.active.background", c(self.primary_active)),
            ("primary.foreground", c(self.on_accent)),
            ("secondary.background", c(self.surface)),
            ("secondary.hover.background", c(self.surface_hover)),
            ("secondary.foreground", c(self.emphasis)),
            ("success.background", c(self.success)),
            ("success.foreground", c(self.on_accent)),
            ("danger.background", c(self.danger)),
            ("danger.foreground", c(self.on_accent)),
            ("warning.background", c(self.warning)),
            ("warning.foreground", c(self.on_accent)),
            ("info.background", c(self.info)),
            ("info.foreground", c(self.on_accent)),
            ("link", c(self.link)),
            ("ring", c(self.primary)),
            ("caret", c(self.emphasis)),
            // gpui-kit caps the selection at 30% opacity.
            ("selection.background", a(self.selection, 0x4d)),
            ("scrollbar.thumb.background", c(self.control)),
            ("scrollbar.thumb.hover.background", c(self.control_hover)),
            ("title_bar.background", c(self.surface)),
            ("title_bar.border", c(self.border)),
            ("status_bar.background", c(self.surface)),
            ("tab_bar.background", c(self.background)),
            ("tab.background", c(self.surface)),
            ("tab.foreground", c(self.muted_foreground)),
            ("tab.active.background", c(self.background)),
            ("tab.active.foreground", c(self.emphasis)),
            ("skeleton.background", c(self.surface)),
            ("switch.background", c(self.control)),
            ("description_list.label.background", c(self.surface)),
            ("window.border", c(self.border)),
            ("base.red", c(self.red)),
            ("base.green", c(self.green)),
            ("base.yellow", c(self.yellow)),
            ("base.blue", c(self.blue)),
            ("base.magenta", c(self.magenta)),
            ("base.cyan", c(self.cyan)),
        ]);
        let syntax = serde_json::Value::Object(
            self.syntax
                .iter()
                .map(|(name, s)| {
                    let mut style = json!({ "color": hex(s.color) });
                    if s.bold {
                        style["font_weight"] = json!(700);
                    }
                    if s.italic {
                        style["font_style"] = json!("italic");
                    }
                    ((*name).to_owned(), style)
                })
                .collect(),
        );
        // These are the field names of gpui-kit's highlighter stub, which is
        // what our build has (no `tree-sitter` feature); the full highlighter
        // spells them `editor.background` etc. The stub has no status colors.
        let mut highlight = object([
            ("editor_background", c(self.background)),
            ("editor_foreground", c(self.foreground)),
            ("editor_active_line", c(self.surface)),
            ("editor_line_number", c(self.muted_foreground)),
            ("editor_active_line_number", c(self.foreground)),
            ("editor_invisible", c(self.muted_foreground)),
        ]);
        highlight["syntax"] = syntax;
        let mode = if self.dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        };
        json!({
            "name": self.name,
            "mode": mode,
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

/// Checks a spec the way every theme's tests should.
#[cfg(test)]
pub(super) fn assert_valid(spec: &ThemeSpec) {
    let source = spec.config_json();
    let config = spec.config().expect("the theme parses");
    assert_eq!(config.mode.is_dark(), spec.dark);
    // serde ignores unknown keys, so a typo would silently fall back to
    // gpui-kit's default color: every key set must survive the round trip.
    let parsed = serde_json::to_value(&config).expect("serializes");
    for path in [&["colors"][..], &["highlight"], &["highlight", "syntax"]] {
        let at = |v: &serde_json::Value| path.iter().fold(v.clone(), |v, k| v[k].clone());
        let (source, parsed) = (at(&source), at(&parsed));
        for (key, value) in source.as_object().expect("an object") {
            if !value.is_object() || path.len() > 1 {
                assert!(
                    !parsed[key].is_null(),
                    "{}: {}.{key} is not a gpui-kit key",
                    spec.name,
                    path.join("."),
                );
            }
        }
    }
}
