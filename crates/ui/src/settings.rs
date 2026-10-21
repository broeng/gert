//! The diff settings every diff view of the session shares: the server's
//! preferences, the user's local overrides from `config.toml`, and changes
//! made in the running session (the `,` panel, the mode key). Stacked in
//! that order of precedence (session over local over server over defaults).

use gpui_kit::{Context, SharedString};
use model::{DiffPreferences, ResolvedDiffPreferences, resolve_diff_preferences};

use crate::session::LocalDiff;

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
}
