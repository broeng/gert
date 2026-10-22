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
    /// Background of the text a comment thread is anchored at.
    comment_mark: Hsla,
    /// Comment cards: unresolved (needs attention) and resolved.
    pub unresolved: Hsla,
    pub resolved: Hsla,
    pub danger: Hsla,
    /// URLs in code lines.
    pub link: Hsla,
    pub card_fg: Hsla,
    alpha_row: f32,
    alpha_gutter: f32,
}

impl Palette {
    pub fn new(focused: bool, cx: &App) -> Self {
        let t = cx.theme();
        let dark = t.is_dark();
        let (alpha_row, alpha_mark, alpha_gutter) = if dark {
            (0.24, 0.55, 0.40)
        } else {
            (0.22, 0.50, 0.38)
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
            comment_mark: t.warning.opacity(if dark { 0.30 } else { 0.35 }),
            unresolved: t.warning,
            resolved: t.muted_foreground,
            danger: t.danger,
            link: t.info,
            card_fg: t.foreground,
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
    /// The UI font and size, for comment cards (the view's text is code).
    pub ui_family: SharedString,
    pub ui_size: Pixels,
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
            ui_family: t.font_family.clone(),
            ui_size: t.font_size,
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

/// Highlight runs of a cell: intraline marks, the text comment threads are
/// anchored at, URLs (underlined link colour) and dimmed tab markers. Sorted and non-overlapping, as
/// `StyledText` wants. Where marks and comments overlap, the mark shows.
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
    let comments = &cell.comments;
    if cell.tabs.is_empty() && comments.is_empty() && cell.urls.is_empty() {
        return marks
            .iter()
            .map(|r| (r.clone(), mark.unwrap_or_default()))
            .collect();
    }
    // Split at every boundary so a tab marker inside a mark gets both.
    let mut cuts: Vec<usize> = marks
        .iter()
        .chain(&cell.tabs)
        .chain(comments)
        .chain(&cell.urls)
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
            let in_comment = covers(comments, a);
            let in_url = covers(&cell.urls, a);
            let mut style = HighlightStyle::default();
            if in_mark {
                style.background_color = mark.and_then(|m| m.background_color);
            } else if in_comment {
                style.background_color = Some(p.comment_mark);
            }
            if in_url {
                style.color = Some(p.link);
                style.underline = Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(p.link),
                    wavy: false,
                });
            }
            if in_tab {
                style.color = tab.color;
            }
            (in_mark || in_tab || in_comment || in_url).then_some((a..b, style))
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use std::ops::Range;

    use gpui_kit::{HighlightStyle, hsla};

    use super::{Cell, LineKind, Palette, line_highlights};

    fn palette() -> Palette {
        let c = |h: f32| hsla(h, 0.5, 0.5, 1.);
        Palette {
            muted_fg: c(0.),
            gutter_fg: c(0.1),
            divider: c(0.),
            ruler: c(0.),
            filler_bg: c(0.),
            skip_bg: c(0.),
            cursor_bg: c(0.),
            cursor_bar: c(0.),
            removed: c(0.2),
            added: c(0.3),
            rebase: c(0.),
            removed_mark: c(0.4),
            added_mark: c(0.5),
            comment_mark: c(0.6),
            unresolved: c(0.),
            resolved: c(0.),
            danger: c(0.),
            link: c(0.7),
            card_fg: c(0.),
            alpha_row: 0.2,
            alpha_gutter: 0.4,
        }
    }

    fn cell(text: &str, kind: LineKind) -> Cell {
        Cell {
            number: 1,
            text: text.into(),
            marks: Vec::new(),
            tabs: Vec::new(),
            kind,
            due_to_rebase: false,
            comments: Vec::new(),
            urls: crate::links::find_urls(text),
        }
    }

    fn runs(c: &Cell) -> Vec<(Range<usize>, HighlightStyle)> {
        let out = line_highlights(c, &palette());
        let mut last = 0;
        for (r, _) in &out {
            assert!(r.start >= last && r.start < r.end, "{out:?}");
            last = r.end;
        }
        out
    }

    #[test]
    fn lines_without_urls_are_unchanged() {
        let mut c = cell("let x = 1;", LineKind::Added);
        c.marks = vec![4..5];
        let out = runs(&c);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 4..5);
        assert!(out[0].1.underline.is_none() && out[0].1.color.is_none());
        assert!(runs(&cell("plain", LineKind::Common)).is_empty());
    }

    #[test]
    fn a_url_is_underlined_in_the_link_colour() {
        let out = runs(&cell("// https://a.org/x done", LineKind::Common));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 3..18);
        assert_eq!(out[0].1.color, Some(palette().link));
        assert!(out[0].1.underline.is_some());
    }

    #[test]
    fn a_url_inside_a_mark_keeps_the_mark_background() {
        let mut c = cell("// https://a.org/x done", LineKind::Added);
        c.marks = vec![0..23];
        let out = runs(&c);
        let p = palette();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].0, 0..3);
        assert_eq!(out[0].1.background_color, Some(p.mark_bg(LineKind::Added)));
        assert_eq!(out[0].1.color, None);
        assert_eq!(out[1].0, 3..18);
        assert_eq!(out[1].1.background_color, Some(p.mark_bg(LineKind::Added)));
        assert_eq!(out[1].1.color, Some(p.link));
        assert_eq!(out[2].0, 18..23);
    }

    #[test]
    fn a_url_overlapping_a_comment_anchor_is_split() {
        let mut c = cell("// https://a.org/x done", LineKind::Common);
        c.comments = vec![10..21];
        let out = runs(&c);
        let p = palette();
        let at = |i: usize| out.iter().find(|(r, _)| r.contains(&i)).map(|(_, s)| *s);
        assert_eq!(at(5).and_then(|s| s.background_color), None);
        assert_eq!(
            at(12).and_then(|s| s.background_color),
            Some(p.comment_mark)
        );
        assert_eq!(at(12).and_then(|s| s.color), Some(p.link));
        assert_eq!(
            at(19).and_then(|s| s.background_color),
            Some(p.comment_mark)
        );
        assert_eq!(at(19).and_then(|s| s.color), None);
    }

    #[test]
    fn a_tab_marker_before_a_url_stays_dimmed() {
        let text = "\u{2192}   https://a.org/x";
        let mut c = cell(text, LineKind::Common);
        c.tabs = vec![0..3];
        let out = runs(&c);
        assert_eq!(out[0].0, 0..3);
        assert_eq!(out[0].1.color, Some(palette().gutter_fg));
        assert!(out[0].1.underline.is_none());
        assert_eq!(out[1].0, 6..text.len());
        assert_eq!(out[1].1.color, Some(palette().link));
    }
}
