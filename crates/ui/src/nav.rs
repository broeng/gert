//! PolyGerrit-style page navigation: a history stack of routes with
//! back/forward and "up". Plain data, no GPUI.

use model::{ChangeNumber, PatchSetNumber, ThreadId};

/// A page the user can be on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Login,
    Dashboard,
    Search(String),
    Change(ChangeNumber),
    /// One file's diff, like PolyGerrit's `/c/proj/+/123/2..5/path`.
    Diff(DiffRoute),
}

/// Which diff the diff page shows. The patch sets are numbers (the SHAs are
/// looked up in the change); `target` is always concrete so that history
/// entries keep meaning the same thing after a new patch set arrives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffRoute {
    pub change: ChangeNumber,
    /// `None` compares with the parent commit.
    pub base: Option<PatchSetNumber>,
    pub target: PatchSetNumber,
    pub path: String,
    /// Put the cursor on this comment thread (opened) once the comments are
    /// there; the thread list of the change page sets it.
    pub thread: Option<ThreadId>,
}

/// What the change page restores when the user comes back to it: the
/// patch set pair and the file under the cursor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeView {
    pub base: Option<PatchSetNumber>,
    /// `None` is the current patch set.
    pub target: Option<PatchSetNumber>,
    pub file: Option<String>,
}

impl DiffRoute {
    /// The change page state that matches this diff.
    pub fn change_view(&self) -> ChangeView {
        ChangeView {
            base: self.base,
            target: Some(self.target),
            file: Some(self.path.clone()),
        }
    }
}

impl Route {
    /// Pages that show a list of changes.
    pub fn is_list(&self) -> bool {
        matches!(self, Route::Dashboard | Route::Search(_))
    }
}

#[derive(Clone, Debug)]
struct Entry {
    route: Route,
    /// The change that was selected when the user left this page.
    selected: Option<ChangeNumber>,
    /// The state of a change page that was left.
    change_view: Option<ChangeView>,
}

#[derive(Clone, Debug)]
pub struct NavState {
    entries: Vec<Entry>,
    index: usize,
}

impl NavState {
    pub fn new(initial: Route) -> Self {
        Self {
            entries: vec![Entry {
                route: initial,
                selected: None,
                change_view: None,
            }],
            index: 0,
        }
    }

    pub fn current(&self) -> &Route {
        &self.entries[self.index].route
    }

    pub fn can_go_back(&self) -> bool {
        self.index > 0
    }

    pub fn can_go_forward(&self) -> bool {
        self.index + 1 < self.entries.len()
    }

    /// Remembers the selected change of the current page, to restore when
    /// the user comes back.
    pub fn set_selected(&mut self, selected: Option<ChangeNumber>) {
        self.entries[self.index].selected = selected;
    }

    pub fn selected(&self) -> Option<ChangeNumber> {
        self.entries[self.index].selected
    }

    pub fn set_change_view(&mut self, view: Option<ChangeView>) {
        self.entries[self.index].change_view = view;
    }

    pub fn change_view(&self) -> Option<ChangeView> {
        self.entries[self.index].change_view.clone()
    }

    /// Navigates to a new page, dropping the forward history. Pushing the
    /// current route again does nothing.
    pub fn push(&mut self, route: Route) {
        if *self.current() == route {
            return;
        }
        self.entries.truncate(self.index + 1);
        self.entries.push(Entry {
            route,
            selected: None,
            change_view: None,
        });
        self.index += 1;
    }

    /// Forgets all history and starts over at `route` (login, logout).
    pub fn reset(&mut self, route: Route) {
        *self = Self::new(route);
    }

    pub fn back(&mut self) -> bool {
        if self.can_go_back() {
            self.index -= 1;
            true
        } else {
            false
        }
    }

    pub fn forward(&mut self) -> bool {
        if self.can_go_forward() {
            self.index += 1;
            true
        } else {
            false
        }
    }

    /// Where "up" leads from the current page: a diff goes to its change, a
    /// change to the list it was opened from (else the dashboard), a search
    /// to the dashboard.
    pub fn up_target(&self) -> Option<Route> {
        match self.current() {
            Route::Login | Route::Dashboard => None,
            Route::Search(_) => Some(Route::Dashboard),
            Route::Diff(d) => Some(Route::Change(d.change)),
            Route::Change(_) => Some(
                self.entries[..self.index]
                    .iter()
                    .rev()
                    .map(|e| &e.route)
                    .find(|r| r.is_list())
                    .cloned()
                    .unwrap_or(Route::Dashboard),
            ),
        }
    }

    /// Goes up. If the target is already in the history behind us, we move
    /// back to it (keeping its selection) instead of stacking a copy.
    pub fn up(&mut self) -> bool {
        let Some(target) = self.up_target() else {
            return false;
        };
        match self.entries[..self.index]
            .iter()
            .rposition(|e| e.route == target)
        {
            Some(i) => self.index = i,
            None => self.push(target),
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(n: u64) -> Route {
        Route::Change(ChangeNumber(n))
    }

    fn s(q: &str) -> Route {
        Route::Search(q.into())
    }

    #[test]
    fn push_back_forward() {
        let mut nav = NavState::new(Route::Dashboard);
        assert!(!nav.can_go_back());
        nav.push(s("a"));
        nav.push(c(1));
        assert_eq!(nav.current(), &c(1));
        assert!(nav.back());
        assert_eq!(nav.current(), &s("a"));
        assert!(nav.can_go_forward());
        assert!(nav.forward());
        assert_eq!(nav.current(), &c(1));
        assert!(!nav.forward());
    }

    #[test]
    fn push_drops_forward_history() {
        let mut nav = NavState::new(Route::Dashboard);
        nav.push(s("a"));
        nav.back();
        nav.push(s("b"));
        assert!(!nav.can_go_forward());
        nav.back();
        assert_eq!(nav.current(), &Route::Dashboard);
    }

    #[test]
    fn pushing_the_current_route_is_a_noop() {
        let mut nav = NavState::new(Route::Dashboard);
        nav.push(Route::Dashboard);
        assert!(!nav.can_go_back());
    }

    #[test]
    fn back_at_start_fails() {
        let mut nav = NavState::new(Route::Dashboard);
        assert!(!nav.back());
        assert_eq!(nav.current(), &Route::Dashboard);
    }

    #[test]
    fn up_from_change_returns_to_the_list_it_came_from() {
        let mut nav = NavState::new(Route::Dashboard);
        nav.push(s("owner:self"));
        nav.push(c(7));
        nav.push(c(8));
        assert_eq!(nav.up_target(), Some(s("owner:self")));
        assert!(nav.up());
        assert_eq!(nav.current(), &s("owner:self"));
        // The change pages are still forward history.
        assert!(nav.can_go_forward());
    }

    #[test]
    fn up_restores_selection_of_the_earlier_entry() {
        let mut nav = NavState::new(Route::Dashboard);
        nav.set_selected(Some(ChangeNumber(5)));
        nav.push(c(5));
        assert!(nav.up());
        assert_eq!(nav.current(), &Route::Dashboard);
        assert_eq!(nav.selected(), Some(ChangeNumber(5)));
    }

    fn d(change: u64, path: &str) -> Route {
        Route::Diff(DiffRoute {
            change: ChangeNumber(change),
            base: None,
            target: PatchSetNumber(2),
            path: path.into(),
            thread: None,
        })
    }

    #[test]
    fn up_from_a_diff_returns_to_the_change_behind_it() {
        let mut nav = NavState::new(Route::Dashboard);
        nav.push(c(7));
        nav.set_change_view(Some(ChangeView {
            file: Some("a.rs".into()),
            ..Default::default()
        }));
        nav.push(d(7, "a.rs"));
        nav.push(d(7, "b.rs"));
        assert_eq!(nav.up_target(), Some(c(7)));
        assert!(nav.up());
        assert_eq!(nav.current(), &c(7));
        // The view saved on that entry is still there.
        assert_eq!(
            nav.change_view().and_then(|v| v.file).as_deref(),
            Some("a.rs")
        );
        assert!(nav.can_go_forward());
    }

    #[test]
    fn up_from_a_diff_without_the_change_behind_it_pushes_it() {
        let mut nav = NavState::new(d(7, "a.rs"));
        assert!(nav.up());
        assert_eq!(nav.current(), &c(7));
        assert!(nav.can_go_back());
    }

    #[test]
    fn change_up_skips_diff_entries() {
        let mut nav = NavState::new(Route::Dashboard);
        nav.push(s("q"));
        nav.push(c(7));
        nav.push(d(7, "a.rs"));
        nav.push(c(8));
        assert_eq!(nav.up_target(), Some(s("q")));
    }

    #[test]
    fn up_without_earlier_list_pushes_dashboard() {
        let mut nav = NavState::new(c(1));
        assert_eq!(nav.up_target(), Some(Route::Dashboard));
        assert!(nav.up());
        assert_eq!(nav.current(), &Route::Dashboard);
        assert!(nav.can_go_back());
    }

    #[test]
    fn up_from_dashboard_and_login_does_nothing() {
        let mut nav = NavState::new(Route::Dashboard);
        assert!(!nav.up());
        let mut nav = NavState::new(Route::Login);
        assert!(!nav.up());
    }

    #[test]
    fn reset_clears_history() {
        let mut nav = NavState::new(Route::Login);
        nav.push(Route::Dashboard);
        nav.reset(Route::Dashboard);
        assert!(!nav.can_go_back());
        assert_eq!(nav.current(), &Route::Dashboard);
    }
}
