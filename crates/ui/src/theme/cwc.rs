//! The CWC theme, from the colors of https://cwconsult.dk/ (a Bootstrap
//! site; `assets/stylesheets/main.css`).
//!
//! The site's brand colors: orange `#dc6b2f` (`--bs-primary`, also links and
//! focus rings), teal `#77c5d5` and pale teal `#bbdde6` (`--bs-success`,
//! `--bs-info`: used as card colors, not as "success"), slate gray
//! `#75787b` (`--bs-secondary`, rules), blue-gray `#dbe2e9` (`--bs-light`)
//! and green-black `#373a36` (`--bs-dark`). Light pages are `#1e201d` on
//! `#fbfcfc`; the site's dark mode swaps them, with Bootstrap's dark grays.
//!
//! The roles: the orange is the primary (buttons, selection, focus, links)
//! and the teal the info color. A review tool still needs a green that
//! means "approved" and a red that means "rejected", which the brand lacks,
//! so success and danger are Bootstrap's own (`#198754`, `#dc3545`; their
//! dark-mode `#75b798`, `#ea868f`). Text is softened a little from the
//! site's near-black and near-white, which suit prose more than dense lists.
//! The site's fonts (Poppins, Azeret Mono) are not used: fetching them would
//! break the no-third-party-requests rule.

use super::spec::{ExtraColors, ThemeSpec, bold, fg, italic};

const ORANGE: u32 = 0xdc6b2f;
/// `--bs-link-hover-color` in light mode.
const ORANGE_DARK: u32 = 0xb05626;
/// `--bs-link-hover-color` in dark mode.
const ORANGE_LIGHT: u32 = 0xe38959;
const TEAL: u32 = 0x77c5d5;
const SLATE: u32 = 0x75787b;

pub fn spec(dark: bool) -> ThemeSpec {
    if dark { dark_spec() } else { light_spec() }
}

fn light_spec() -> ThemeSpec {
    ThemeSpec {
        name: "CWC Light",
        dark: false,
        background: 0xfbfcfc,
        foreground: 0x2b2d2a,
        emphasis: 0x1e201d,
        muted_foreground: 0x66696b,
        surface: 0xeef1f4,
        surface_hover: 0xdbe2e9,
        border: 0xdee2e6,
        control: 0xc3c8cd,
        control_hover: SLATE,
        primary: ORANGE,
        primary_active: ORANGE_DARK,
        on_accent: 0xffffff,
        success: 0x198754,
        danger: 0xdc3545,
        warning: 0xcc9a06,
        // The brand teal is too pale for text on white; a deeper teal.
        info: 0x2f8a9c,
        link: ORANGE_DARK,
        selection: ORANGE,
        red: 0xdc3545,
        green: 0x198754,
        yellow: 0xcc9a06,
        blue: 0x2f6fae,
        magenta: 0xb52a6f,
        cyan: 0x2f8a9c,
        syntax: vec![
            ("attribute", fg(SLATE)),
            ("boolean", fg(0xb52a6f)),
            ("comment", italic(SLATE)),
            ("comment_doc", italic(0x4f7f62)),
            ("constant", fg(0xb52a6f)),
            ("constructor", fg(0x6f42c1)),
            ("emphasis.strong", bold(0x1e201d)),
            ("enum", fg(0x6f42c1)),
            ("function", fg(0x2f5f8f)),
            ("keyword", bold(ORANGE_DARK)),
            ("link_text", fg(ORANGE_DARK)),
            ("link_uri", fg(0x2f8a9c)),
            ("number", fg(0xb52a6f)),
            ("preproc", fg(0x6f42c1)),
            ("string", fg(0x23798a)),
            ("string.escape", fg(0xb52a6f)),
            ("string.regex", fg(0x23798a)),
            ("string.special", fg(0x23798a)),
            ("string.special.symbol", fg(0xb52a6f)),
            ("tag", fg(ORANGE_DARK)),
            ("tag.doctype", fg(SLATE)),
            ("text.literal", fg(0x23798a)),
            ("title", bold(0x1e201d)),
            ("type", fg(0x6f42c1)),
            ("variable.special", fg(ORANGE_DARK)),
        ],
        extra: ExtraColors {
            removed_row: 0xfbe3e5,
            removed_mark: 0xf5c2c7,
            added_row: 0xdcefe4,
            added_mark: 0xb3dbc4,
            comment_mark: 0xffe69c,
            // `--bs-success-bg-subtle`: the brand teal, faint.
            commit_card: 0xe4f3f7,
        },
    }
}

fn dark_spec() -> ThemeSpec {
    ThemeSpec {
        name: "CWC Dark",
        dark: true,
        background: 0x1e201d,
        foreground: 0xd9dcd9,
        emphasis: 0xfbfcfc,
        muted_foreground: 0x9a9d99,
        surface: 0x2a2d29,
        // `--bs-dark`.
        surface_hover: 0x373a36,
        border: 0x3e423d,
        control: 0x585c57,
        control_hover: SLATE,
        primary: ORANGE,
        primary_active: ORANGE_DARK,
        on_accent: 0xffffff,
        success: 0x75b798,
        danger: 0xea868f,
        warning: 0xffc107,
        info: TEAL,
        link: ORANGE_LIGHT,
        selection: ORANGE,
        red: 0xea868f,
        green: 0x75b798,
        yellow: 0xffc107,
        blue: 0x8bb9e8,
        magenta: 0xec8fbf,
        cyan: TEAL,
        syntax: vec![
            ("attribute", fg(0x9a9d99)),
            ("boolean", fg(0xec8fbf)),
            ("comment", italic(0x8a8d89)),
            ("comment_doc", italic(0x9fc4ae)),
            ("constant", fg(0xec8fbf)),
            ("constructor", fg(0xc4a7f5)),
            ("emphasis.strong", bold(0xfbfcfc)),
            ("enum", fg(0xc4a7f5)),
            ("function", fg(0xa9cfe0)),
            ("keyword", bold(ORANGE_LIGHT)),
            ("link_text", fg(ORANGE_LIGHT)),
            ("link_uri", fg(TEAL)),
            ("number", fg(0xec8fbf)),
            ("preproc", fg(0xc4a7f5)),
            ("string", fg(TEAL)),
            ("string.escape", fg(0xec8fbf)),
            ("string.regex", fg(TEAL)),
            ("string.special", fg(TEAL)),
            ("string.special.symbol", fg(0xec8fbf)),
            ("tag", fg(ORANGE_LIGHT)),
            ("tag.doctype", fg(0x9a9d99)),
            ("text.literal", fg(TEAL)),
            ("title", bold(0xfbfcfc)),
            ("type", fg(0xc4a7f5)),
            ("variable.special", fg(ORANGE_LIGHT)),
        ],
        extra: ExtraColors {
            removed_row: 0x3a2427,
            removed_mark: 0x5e2c33,
            added_row: 0x1f3a2c,
            added_mark: 0x2c5a40,
            comment_mark: 0x4d3b06,
            // `--bs-success-bg-subtle` (dark), lifted a little off the page.
            commit_card: 0x1f3236,
        },
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn both_variants_are_valid() {
        super::super::spec::assert_valid(&super::spec(true));
        super::super::spec::assert_valid(&super::spec(false));
    }
}
