//! The diff settings every diff view of the session shares: the server's
//! preferences, the user's local overrides from `config.toml`, and changes
//! made in the running session (the `,` panel, the mode key). Stacked in
//! that order of precedence (session over local over server over defaults).

use std::collections::HashMap;

use gpui_kit::{Context, SharedString};
use model::{DiffPreferences, ResolvedDiffPreferences, resolve_diff_preferences};

use crate::{
    filters::{FileFilter, FileMatcher, applies_to, compile},
    session::LocalDiff,
};

pub struct DiffSettings {
    server: Option<DiffPreferences>,
    local: DiffPreferences,
    session: DiffPreferences,
    font_family: Option<SharedString>,
}

impl DiffSettings {
    pub fn new(local: LocalDiff) -> Self {
        Self {
            server: None,
            local: local.preferences,
            session: DiffPreferences::default(),
            font_family: local
                .font_family
                .filter(|f| !f.trim().is_empty())
                .map(Into::into),
        }
    }

    /// The effective settings.
    pub fn resolved(&self) -> ResolvedDiffPreferences {
        resolve_diff_preferences(self.server.as_ref(), &self.session.or(&self.local))
    }

    pub fn font_family(&self) -> Option<SharedString> {
        self.font_family.clone()
    }

    /// The server's preferences arrived (`None`: anonymous, or forgotten
    /// after logging out).
    pub fn set_server(&mut self, prefs: Option<DiffPreferences>, cx: &mut Context<Self>) {
        if self.server != prefs {
            self.server = prefs;
            cx.notify();
        }
    }

    /// Changes a setting for this session only. Nothing is written to the
    /// config file or the server.
    pub fn update_session(
        &mut self,
        change: impl FnOnce(&mut DiffPreferences),
        cx: &mut Context<Self>,
    ) {
        let before = self.session.clone();
        change(&mut self.session);
        if self.session != before {
            cx.notify();
        }
    }
}

/// Layout choices of the running session that every change page shares,
/// so that stepping through a relation chain keeps the panel open. Not
/// persisted. Also holds the review flow choice that 3b will read from the
/// config.
pub struct PageLayout {
    /// The relation chain is wanted as a side panel (it still falls back
    /// to the box in a narrow window).
    related_panel: bool,
    /// After a vote, go on to the next open change of the relation chain.
    advance_after_vote: bool,
}

impl Default for PageLayout {
    fn default() -> Self {
        Self {
            related_panel: false,
            advance_after_vote: true,
        }
    }
}

impl PageLayout {
    pub fn advance_after_vote(&self) -> bool {
        self.advance_after_vote
    }

    pub fn set_advance_after_vote(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.advance_after_vote != on {
            self.advance_after_vote = on;
            cx.notify();
        }
    }

    pub fn related_panel(&self) -> bool {
        self.related_panel
    }

    pub fn toggle_related_panel(&mut self, cx: &mut Context<Self>) {
        self.related_panel = !self.related_panel;
        cx.notify();
    }
}

/// The file filter rules and which of them are on in this session. Each
/// rule starts as its `enabled` default from the config; a toggle on a
/// button overrides that for every change until the app is closed, and is
/// not written back. The pages observe this entity so a toggle or an edit
/// of the rules restyles them at once.
pub struct FileFilters {
    rules: Vec<FileFilter>,
    /// Rule name -> on/off, for the rules toggled in this session.
    overrides: HashMap<String, bool>,
}

impl FileFilters {
    pub fn new(rules: Vec<FileFilter>) -> Self {
        Self {
            rules,
            overrides: HashMap::new(),
        }
    }

    /// All rules, in config order.
    pub fn rules(&self) -> &[FileFilter] {
        &self.rules
    }

    /// The rules that apply to `project`, in config order. These get a
    /// toggle button on that project's changes.
    pub fn applicable(&self, project: &str) -> Vec<&FileFilter> {
        self.rules
            .iter()
            .filter(|r| applies_to(r, project))
            .collect()
    }

    /// The session toggle if there is one, else the rule's default.
    pub fn is_active(&self, rule: &FileFilter) -> bool {
        self.overrides
            .get(&rule.name)
            .copied()
            .unwrap_or(rule.enabled)
    }

    /// Flips the rule for the rest of the session.
    pub fn toggle(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(rule) = self.rules.iter().find(|r| r.name == name) else {
            return;
        };
        let on = !self.is_active(rule);
        self.overrides.insert(name.to_owned(), on);
        cx.notify();
    }

    /// Replaces the rules (after an edit was saved). Session toggles are
    /// kept for the names that still exist.
    pub fn set_rules(&mut self, rules: Vec<FileFilter>, cx: &mut Context<Self>) {
        self.overrides
            .retain(|name, _| rules.iter().any(|r| &r.name == name));
        self.rules = rules;
        cx.notify();
    }

    /// The matcher for the changes of `project`, with the rules that are on.
    pub fn matcher(&self, project: &str) -> FileMatcher {
        compile(&self.rules, project, |r| self.is_active(r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{DiffContext, DiffViewMode, Whitespace};

    fn settings(local: DiffPreferences) -> DiffSettings {
        DiffSettings::new(LocalDiff {
            preferences: local,
            font_family: Some("Iosevka".into()),
        })
    }

    #[test]
    fn layers_stack_session_local_server_default() {
        let mut s = settings(DiffPreferences {
            tab_size: Some(2),
            context: Some(DiffContext::Lines(5)),
            ..Default::default()
        });
        assert_eq!(s.resolved().tab_size, 2);
        assert_eq!(s.resolved().line_length, 100); // default

        s.server = Some(DiffPreferences {
            tab_size: Some(4),
            line_length: Some(120),
            view_mode: Some(DiffViewMode::Unified),
            ignore_whitespace: Some(Whitespace::IgnoreAll),
            ..Default::default()
        });
        let r = s.resolved();
        assert_eq!(r.tab_size, 2, "local beats server");
        assert_eq!(r.line_length, 120, "server beats default");
        assert_eq!(r.view_mode, DiffViewMode::Unified);
        assert_eq!(r.context, DiffContext::Lines(5));

        s.session.view_mode = Some(DiffViewMode::SideBySide);
        s.session.tab_size = Some(8);
        let r = s.resolved();
        assert_eq!(
            r.view_mode,
            DiffViewMode::SideBySide,
            "session beats server"
        );
        assert_eq!(r.tab_size, 8, "session beats local");
        assert_eq!(r.ignore_whitespace, Whitespace::IgnoreAll);
    }

    #[test]
    fn blank_font_family_is_ignored() {
        let s = DiffSettings::new(LocalDiff {
            preferences: Default::default(),
            font_family: Some("  ".into()),
        });
        assert_eq!(s.font_family(), None);
        assert_eq!(
            settings(Default::default()).font_family(),
            Some("Iosevka".into())
        );
    }

    mod file_filters {
        use gpui_kit::{AppContext as _, TestAppContext};

        use super::super::*;
        use crate::filters::FilterAction;

        fn rule(name: &str, glob: &str, enabled: bool) -> FileFilter {
            FileFilter {
                name: name.into(),
                globs: vec![glob.into()],
                projects: Vec::new(),
                action: FilterAction::Hide,
                enabled,
            }
        }

        #[gpui_kit::test]
        fn toggles_override_the_default_and_survive_edits(cx: &mut TestAppContext) {
            let filters =
                cx.new(|_| FileFilters::new(vec![rule("a", "*.a", true), rule("b", "*.b", false)]));
            let notified = std::rc::Rc::new(std::cell::Cell::new(0));
            let n = notified.clone();
            let _sub = cx.update(|cx| cx.observe(&filters, move |_, _| n.set(n.get() + 1)));

            filters.read_with(cx, |f, _| {
                assert!(f.is_active(&f.rules()[0]));
                assert!(!f.is_active(&f.rules()[1]));
                assert!(f.matcher("p").style("x.a").hidden);
                assert!(!f.matcher("p").style("x.b").hidden);
            });

            filters.update(cx, |f, cx| {
                f.toggle("a", cx);
                f.toggle("b", cx);
                f.toggle("nope", cx);
            });
            filters.read_with(cx, |f, _| {
                assert!(!f.is_active(&f.rules()[0]));
                assert!(f.is_active(&f.rules()[1]));
                assert!(!f.matcher("p").style("x.a").hidden);
                assert!(f.matcher("p").style("x.b").hidden);
            });
            assert!(notified.get() >= 1);

            // "a" is gone and "c" is new: only the surviving toggle stays.
            filters.update(cx, |f, cx| {
                f.set_rules(vec![rule("b", "*.b", false), rule("c", "*.c", true)], cx)
            });
            filters.read_with(cx, |f, _| {
                assert!(f.is_active(&f.rules()[0]), "the toggle of b is kept");
                assert!(f.is_active(&f.rules()[1]));
                assert!(!f.overrides.contains_key("a"));
            });
            // A rule re-created under a removed name starts from its default.
            filters.update(cx, |f, cx| f.set_rules(vec![rule("a", "*.a", true)], cx));
            filters.read_with(cx, |f, _| assert!(f.is_active(&f.rules()[0])));
        }

        #[gpui_kit::test]
        fn applicable_follows_the_project_in_config_order(cx: &mut TestAppContext) {
            let mut scoped = rule("scoped", "*.s", true);
            scoped.projects = vec!["p1".into()];
            let filters = cx.new(|_| {
                FileFilters::new(vec![
                    rule("first", "*.a", true),
                    scoped,
                    rule("last", "*.z", true),
                ])
            });
            filters.read_with(cx, |f, _| {
                let names = |p: &str| {
                    f.applicable(p)
                        .into_iter()
                        .map(|r| r.name.clone())
                        .collect::<Vec<_>>()
                };
                assert_eq!(names("p1"), ["first", "scoped", "last"]);
                assert_eq!(names("p2"), ["first", "last"]);
                assert!(f.matcher("p2").style("x.s").rules.is_empty());
            });
        }
    }
}
