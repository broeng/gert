//! The Solarized palette as solarized-emacs (bbatsov/solarized-emacs) uses
//! it.
//!
//! Like solarized-emacs' `solarized-with-color-variables`, each variant swaps
//! the base tones so one mapping serves both: `base03` is always the
//! background, `base02` the highlight (hl-line, mode line), `base01` the
//! shadow (comments, secondary text), `base0` the body text and `base1` the
//! emphasized text. The accents are shared; the `-1bg`/`-2bg` tints (diff
//! rows and their refined parts) are per variant, copied from
//! `solarized-palettes.el`, except the added-row greens, which are a step
//! brighter than solarized-emacs' (user preference).
//!
//! Faces behind the roles: `default`, `hl-line`/`highlight` for surfaces,
//! `shadow` for muted text, `vertical-border` (`s-line`) for borders,
//! `company-tooltip` for popovers, `mode-line` for the title bar,
//! `diff-*`/`diff-refine-*` for diffs, `tooltip` for the commit card and the
//! `font-lock-*` faces for syntax. Links are blue rather than the `link`
//! face's yellow, since yellow already means "unresolved" and "warning" here.

use super::spec::{ExtraColors, ThemeSpec, bold, fg};

const S_BASE03: u32 = 0x002b36;
const S_BASE02: u32 = 0x073642;
const S_BASE01: u32 = 0x586e75;
const S_BASE00: u32 = 0x657b83;
const S_BASE0: u32 = 0x839496;
const S_BASE1: u32 = 0x93a1a1;
const S_BASE2: u32 = 0xeee8d5;
const S_BASE3: u32 = 0xfdf6e3;

const YELLOW: u32 = 0xb58900;
const ORANGE: u32 = 0xcb4b16;
const RED: u32 = 0xdc322f;
const MAGENTA: u32 = 0xd33682;
const VIOLET: u32 = 0x6c71c4;
const BLUE: u32 = 0x268bd2;
/// solarized-emacs' `blue-d`, the same in both variants.
const BLUE_D: u32 = 0x0061a8;
const CYAN: u32 = 0x2aa198;
const GREEN: u32 = 0x859900;

/// One variant of the palette, with the base tones swapped for it.
struct Solarized {
    dark: bool,
    base03: u32,
    base02: u32,
    base01: u32,
    base00: u32,
    base0: u32,
    base1: u32,
    /// `s-line`: thin lines (window dividers, borders).
    line: u32,
    red_1bg: u32,
    red_2bg: u32,
    green_1bg: u32,
    green_2bg: u32,
    yellow_1bg: u32,
    yellow_2bg: u32,
}

const DARK: Solarized = Solarized {
    dark: true,
    base03: S_BASE03,
    base02: S_BASE02,
    base01: S_BASE01,
    base00: S_BASE00,
    base0: S_BASE0,
    base1: S_BASE1,
    line: 0x284b54,
    red_1bg: 0x2d2c31,
    red_2bg: 0x532725,
    // solarized-emacs: 0x1d3732, 0x2f4321.
    green_1bg: 0x1e4034,
    green_2bg: 0x334d21,
    yellow_1bg: 0x273532,
    yellow_2bg: 0x433e20,
};

const LIGHT: Solarized = Solarized {
    dark: false,
    base03: S_BASE3,
    base02: S_BASE2,
    base01: S_BASE1,
    base00: S_BASE0,
    base0: S_BASE00,
    base1: S_BASE01,
    line: 0xcccec4,
    red_1bg: 0xffdec8,
    red_2bg: 0xffb9a1,
    // solarized-emacs: 0xefeac7, 0xdbdb9c.
    green_1bg: 0xefeec0,
    green_2bg: 0xdbde8f,
    yellow_1bg: 0xf8e8c6,
    yellow_2bg: 0xf1d49b,
};

pub fn spec(dark: bool) -> ThemeSpec {
    let p = if dark { &DARK } else { &LIGHT };
    ThemeSpec {
        name: if p.dark {
            "Solarized Dark"
        } else {
            "Solarized Light"
        },
        dark: p.dark,
        background: p.base03,
        foreground: p.base0,
        emphasis: p.base1,
        muted_foreground: p.base01,
        surface: p.base02,
        surface_hover: p.line,
        border: p.line,
        control: p.base01,
        control_hover: p.base00,
        primary: BLUE,
        primary_active: BLUE_D,
        // The light base in both variants: the accents are mid-tones.
        on_accent: S_BASE3,
        success: GREEN,
        danger: RED,
        warning: YELLOW,
        info: BLUE,
        link: BLUE,
        // `region` inverts to base1.
        selection: p.base1,
        red: RED,
        green: GREEN,
        yellow: YELLOW,
        blue: BLUE,
        magenta: MAGENTA,
        cyan: CYAN,
        syntax: vec![
            ("attribute", fg(BLUE)),
            ("boolean", bold(BLUE)),
            ("comment", fg(p.base01)),
            ("comment_doc", fg(CYAN)),
            ("constant", bold(BLUE)),
            ("constructor", fg(YELLOW)),
            ("embedded", fg(p.base0)),
            ("emphasis.strong", bold(p.base1)),
            ("enum", fg(YELLOW)),
            ("function", fg(BLUE)),
            ("keyword", bold(GREEN)),
            ("link_text", fg(YELLOW)),
            ("link_uri", fg(BLUE)),
            ("preproc", fg(BLUE)),
            ("string", fg(CYAN)),
            ("string.escape", fg(VIOLET)),
            ("string.regex", fg(CYAN)),
            ("string.special", fg(YELLOW)),
            ("string.special.symbol", fg(VIOLET)),
            ("tag", fg(BLUE)),
            ("tag.doctype", fg(p.base01)),
            ("text.literal", fg(CYAN)),
            ("title", bold(p.base1)),
            ("type", fg(YELLOW)),
            ("variable.special", fg(GREEN)),
            ("punctuation.special", fg(ORANGE)),
        ],
        extra: ExtraColors {
            removed_row: p.red_1bg,
            removed_mark: p.red_2bg,
            added_row: p.green_1bg,
            added_mark: p.green_2bg,
            comment_mark: p.yellow_2bg,
            commit_card: p.yellow_1bg,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_variants_are_valid() {
        super::super::spec::assert_valid(&spec(true));
        super::super::spec::assert_valid(&spec(false));
    }

    #[test]
    fn variants_swap_the_base_tones() {
        assert_eq!((DARK.base03, LIGHT.base03), (S_BASE03, S_BASE3));
        assert_eq!((LIGHT.base0, LIGHT.base1), (S_BASE00, S_BASE01));
    }
}
