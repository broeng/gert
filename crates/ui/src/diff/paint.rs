//! Colors, metrics and text styling of the diff view, derived from the
//! active theme.

use std::ops::Range;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::*;

use super::layout::{Cell, LineKind};

/// Height of a code row for a font size. Other rows (comment threads) have
/// their own, measured height; this is also the hint for rows not yet
/// measured.
pub fn row_height(font_size: Pixels) -> Pixels {
    (font_size * 1.55).round()
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub muted_fg: Hsla,
    pub gutter_fg: Hsla,
    pub divider: Hsla,
    pub ruler: Hsla,
    pub filler_bg: Hsla,
    pub skip_bg: Hsla,
    pub cursor_bg: Hsla,
    pub cursor_bar: Hsla,
    removed: Hsla,
    added: Hsla,
    rebase: Hsla,
    removed_mark: Hsla,
    added_mark: Hsla,
    alpha_row: f32,
    alpha_gutter: f32,
}

impl Palette {
    pub fn new(focused: bool, cx: &App) -> Self {
        let t = cx.theme();
        let dark = t.is_dark();
        let (alpha_row, alpha_mark, alpha_gutter) = if dark {
            (0.14, 0.38, 0.24)
        } else {
            (0.10, 0.30, 0.20)
        };
        let (cursor, cursor_alpha) = if focused {
            (t.primary, if dark { 0.16 } else { 0.12 })
        } else {
            (t.muted_foreground, 0.12)
        };
        Self {
            muted_fg: t.muted_foreground,
            gutter_fg: t.muted_foreground.opacity(0.8),
            divider: t.border,
            ruler: t.border.opacity(0.7),
            filler_bg: t.muted_foreground.opacity(0.06),
            skip_bg: t.muted_foreground.opacity(0.10),
            cursor_bg: cursor.opacity(cursor_alpha),
            cursor_bar: cursor,
            removed: t.danger,
            added: t.success,
            rebase: t.muted_foreground,
            removed_mark: t.danger.opacity(alpha_mark),
            added_mark: t.success.opacity(alpha_mark),
            alpha_row,
            alpha_gutter,
        }
    }

    fn tint(&self, kind: LineKind) -> Option<Hsla> {
        match kind {
            LineKind::Common => None,
            LineKind::Removed => Some(self.removed),
            LineKind::Added => Some(self.added),
        }
    }

    pub fn line_bg(&self, kind: LineKind, due_to_rebase: bool) -> Hsla {
        if due_to_rebase && kind != LineKind::Common {
            // Changes that only exist because of the rebase: a neutral tint.
            return self.rebase.opacity(0.08);
        }
        self.tint(kind)
            .map_or_else(|| hsla(0., 0., 0., 0.), |c| c.opacity(self.alpha_row))
    }

    pub fn gutter_bg(&self, kind: LineKind) -> Hsla {
        self.tint(kind).map_or_else(
            || self.skip_bg.opacity(0.5),
            |c| c.opacity(self.alpha_gutter),
        )
    }

    fn mark_bg(&self, kind: LineKind) -> Hsla {
        match kind {
            LineKind::Removed => self.removed_mark,
            _ => self.added_mark,
        }
    }
}

/// Everything the row builders need, computed once per rendered slice.
pub struct Style {
    pub palette: Palette,
    pub row_height: Pixels,
    pub old_gutter: Pixels,
    pub new_gutter: Pixels,
    pub hide_numbers: bool,
    /// Left offset of the line-length marker inside a text pane.
    pub ruler: Option<Pixels>,
}

impl Style {
    pub fn new(
        family: Option<SharedString>,
        line_length: u32,
        hide_numbers: bool,
        focused: bool,
        (max_old, max_new): (u32, u32),
        window: &Window,
        cx: &App,
    ) -> Self {
        let t = cx.theme();
        let size = t.mono_font_size;
        let family = family.unwrap_or_else(|| t.mono_font_family.clone());
        let font_id = window.text_system().resolve_font(&font(family));
        let char_w = window
            .text_system()
            .advance(font_id, size, 'm')
            .map_or(size * 0.6, |s| s.width);
        let gutter = |max: u32| char_w * (digits(max).max(3) as f32) + px(8. + 6.);
        Self {
            palette: Palette::new(focused, cx),
            row_height: row_height(size),
            old_gutter: gutter(max_old),
            new_gutter: gutter(max_new),
            hide_numbers,
            ruler: (line_length > 0).then(|| char_w * line_length as f32),
        }
    }
}

fn digits(mut n: u32) -> usize {
    let mut d = 1;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

/// Highlight runs of a cell: intraline marks, and dimmed tab markers.
/// Sorted and non-overlapping, as `StyledText` wants.
pub fn line_highlights(cell: &Cell, p: &Palette) -> Vec<(Range<usize>, HighlightStyle)> {
    let mark = (cell.kind != LineKind::Common && !cell.due_to_rebase).then(|| HighlightStyle {
        background_color: Some(p.mark_bg(cell.kind)),
        ..Default::default()
    });
    let tab = HighlightStyle {
        color: Some(p.gutter_fg),
        ..Default::default()
    };
    let marks: &[Range<usize>] = if mark.is_some() { &cell.marks } else { &[] };
    if cell.tabs.is_empty() {
        return marks
            .iter()
            .map(|r| (r.clone(), mark.unwrap_or_default()))
            .collect();
    }
    // Split at every boundary so a tab marker inside a mark gets both.
    let mut cuts: Vec<usize> = marks
        .iter()
        .chain(&cell.tabs)
        .flat_map(|r| [r.start, r.end])
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    let covers = |ranges: &[Range<usize>], a: usize| ranges.iter().any(|r| r.contains(&a));
    cuts.windows(2)
        .filter_map(|w| {
            let (a, b) = (w[0], w[1]);
            let in_mark = covers(marks, a);
            let in_tab = covers(&cell.tabs, a);
            let mut style = HighlightStyle::default();
            if in_mark {
                style.background_color = mark.and_then(|m| m.background_color);
            }
            if in_tab {
                style.color = tab.color;
            }
            (in_mark || in_tab).then_some((a..b, style))
        })
        .collect()
}
