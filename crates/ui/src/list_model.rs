//! Selection and paging state of change lists, independent of GPUI.

use model::ChangeNumber;

/// Rows per page, for dashboard sections and search results.
pub const PAGE_SIZE: usize = 25;

/// A cursor over several sections of rows (one section for search
/// results, one per saved query on the dashboard).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListModel {
    sections: Vec<Vec<ChangeNumber>>,
    selected: Option<(usize, usize)>,
}

impl ListModel {
    pub fn new(section_count: usize) -> Self {
        Self {
            sections: vec![Vec::new(); section_count],
            selected: None,
        }
    }

    pub fn selected(&self) -> Option<(usize, usize)> {
        self.selected
    }

    pub fn selected_number(&self) -> Option<ChangeNumber> {
        let (s, r) = self.selected?;
        self.sections.get(s)?.get(r).copied()
    }

    /// Replaces the rows of a section. The selection stays on the same
    /// change if it is still there; otherwise it is clamped into the
    /// section. With nothing selected yet, nothing is selected.
    pub fn set_section(&mut self, section: usize, rows: Vec<ChangeNumber>) {
        let Some(slot) = self.sections.get_mut(section) else {
            return;
        };
        let previous = self
            .selected
            .filter(|(s, _)| *s == section)
            .and_then(|(_, r)| slot.get(r).copied());
        *slot = rows;
        if let Some((s, r)) = self.selected
            && s == section
        {
            let new_row = previous
                .and_then(|n| slot.iter().position(|x| *x == n))
                .unwrap_or(r);
            self.selected = if slot.is_empty() {
                self.first_selectable_from(section)
            } else {
                Some((section, new_row.min(slot.len() - 1)))
            };
        }
    }

    fn first_selectable_from(&self, start: usize) -> Option<(usize, usize)> {
        (start..self.sections.len())
            .find(|s| !self.sections[*s].is_empty())
            .map(|s| (s, 0))
            .or_else(|| {
                (0..start)
                    .rev()
                    .find(|s| !self.sections[*s].is_empty())
                    .map(|s| (s, self.sections[s].len() - 1))
            })
    }

    /// Selects the first row of the first non-empty section, if nothing is
    /// selected.
    pub fn ensure_selection(&mut self) {
        if self.selected.is_none() {
            self.selected = self.first_selectable_from(0);
        }
    }

    pub fn select_number(&mut self, n: ChangeNumber) -> bool {
        for (s, rows) in self.sections.iter().enumerate() {
            if let Some(r) = rows.iter().position(|x| *x == n) {
                self.selected = Some((s, r));
                return true;
            }
        }
        false
    }

    /// `j`: next row, crossing into the next non-empty section. Stays on the
    /// last row at the end.
    pub fn select_next(&mut self) {
        let Some((s, r)) = self.selected else {
            return self.ensure_selection();
        };
        if r + 1 < self.sections[s].len() {
            self.selected = Some((s, r + 1));
        } else if let Some(n) = (s + 1..self.sections.len()).find(|n| !self.sections[*n].is_empty())
        {
            self.selected = Some((n, 0));
        }
    }

    /// `k`: previous row, crossing into the previous non-empty section.
    pub fn select_prev(&mut self) {
        let Some((s, r)) = self.selected else {
            return self.ensure_selection();
        };
        if r > 0 {
            self.selected = Some((s, r - 1));
        } else if let Some(p) = (0..s).rev().find(|p| !self.sections[*p].is_empty()) {
            self.selected = Some((p, self.sections[p].len() - 1));
        }
    }

    pub fn select_first_in(&mut self, section: usize) {
        if self.sections.get(section).is_some_and(|s| !s.is_empty()) {
            self.selected = Some((section, 0));
        }
    }
}

/// Windows over a growing list of cached rows: page `page` of `PAGE_SIZE`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pager {
    pub page: usize,
}

impl Pager {
    pub fn range(&self, len: usize) -> std::ops::Range<usize> {
        let start = (self.page * PAGE_SIZE).min(len);
        start..(start + PAGE_SIZE).min(len)
    }

    /// Whether `n` has somewhere to go without fetching.
    pub fn has_cached_next(&self, len: usize) -> bool {
        (self.page + 1) * PAGE_SIZE < len
    }

    /// Whether a next page exists locally or on the server.
    pub fn has_next(&self, len: usize, more_on_server: bool) -> bool {
        self.has_cached_next(len) || more_on_server
    }

    /// Keeps the page inside the cached rows after they shrank.
    pub fn clamp(&mut self, len: usize) {
        let last = len.saturating_sub(1) / PAGE_SIZE;
        self.page = self.page.min(last);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(v: &[u64]) -> Vec<ChangeNumber> {
        v.iter().map(|x| ChangeNumber(*x)).collect()
    }

    fn model(sections: &[&[u64]]) -> ListModel {
        let mut m = ListModel::new(sections.len());
        for (i, s) in sections.iter().enumerate() {
            m.set_section(i, n(s));
        }
        m
    }

    #[test]
    fn no_selection_until_asked() {
        let mut m = model(&[&[1, 2]]);
        assert_eq!(m.selected(), None);
        m.ensure_selection();
        assert_eq!(m.selected(), Some((0, 0)));
    }

    #[test]
    fn j_and_k_cross_sections_and_skip_empty_ones() {
        let mut m = model(&[&[1, 2], &[], &[3]]);
        m.ensure_selection();
        m.select_next();
        assert_eq!(m.selected_number(), Some(ChangeNumber(2)));
        m.select_next();
        assert_eq!(m.selected(), Some((2, 0)));
        m.select_next();
        assert_eq!(m.selected(), Some((2, 0)), "stays on the last row");
        m.select_prev();
        assert_eq!(m.selected(), Some((0, 1)));
        m.select_prev();
        m.select_prev();
        assert_eq!(m.selected(), Some((0, 0)), "stays on the first row");
    }

    #[test]
    fn first_j_selects_the_first_row() {
        let mut m = model(&[&[], &[4, 5]]);
        m.select_next();
        assert_eq!(m.selected(), Some((1, 0)));
    }

    #[test]
    fn selection_follows_the_change_when_rows_move() {
        let mut m = model(&[&[1, 2, 3]]);
        m.select_number(ChangeNumber(2));
        m.set_section(0, n(&[9, 1, 2, 3]));
        assert_eq!(m.selected_number(), Some(ChangeNumber(2)));
    }

    #[test]
    fn selection_is_clamped_when_its_change_disappears() {
        let mut m = model(&[&[1, 2, 3]]);
        m.select_number(ChangeNumber(3));
        m.set_section(0, n(&[1, 2]));
        assert_eq!(m.selected(), Some((0, 1)));
    }

    #[test]
    fn emptied_section_moves_selection_elsewhere() {
        let mut m = model(&[&[1], &[2]]);
        m.select_number(ChangeNumber(1));
        m.set_section(0, vec![]);
        assert_eq!(m.selected_number(), Some(ChangeNumber(2)));
        m.set_section(1, vec![]);
        assert_eq!(m.selected(), None);
    }

    #[test]
    fn other_sections_do_not_disturb_selection() {
        let mut m = model(&[&[1, 2], &[3]]);
        m.select_number(ChangeNumber(2));
        m.set_section(1, n(&[7, 8, 9]));
        assert_eq!(m.selected(), Some((0, 1)));
    }

    #[test]
    fn pager_ranges() {
        let p = Pager { page: 1 };
        assert_eq!(p.range(60), 25..50);
        assert_eq!(p.range(30), 25..30);
        assert_eq!(p.range(10), 10..10);
        assert!(p.has_cached_next(60));
        assert!(!p.has_cached_next(50));
        assert!(p.has_next(50, true));
        assert!(!p.has_next(50, false));
    }

    #[test]
    fn pager_clamps_after_shrink() {
        let mut p = Pager { page: 3 };
        p.clamp(30);
        assert_eq!(p.page, 1);
        p.clamp(0);
        assert_eq!(p.page, 0);
        p.clamp(25);
        assert_eq!(p.page, 0);
    }
}
