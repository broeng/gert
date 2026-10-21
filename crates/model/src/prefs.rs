use serde::{Deserialize, Serialize};

/// Whitespace handling of a diff (`Whitespace`, the `whitespace=` query
/// parameter and `DiffPreferencesInfo.ignore_whitespace`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Whitespace {
    IgnoreNone,
    IgnoreTrailing,
    IgnoreLeadingAndTrailing,
    IgnoreAll,
    Unknown(String),
}

impl Whitespace {
    /// Maps Gerrit's wire value (`IGNORE_NONE`, ...).
    pub fn from_wire(s: &str) -> Self {
        match s {
            "IGNORE_NONE" => Self::IgnoreNone,
            "IGNORE_TRAILING" => Self::IgnoreTrailing,
            "IGNORE_LEADING_AND_TRAILING" => Self::IgnoreLeadingAndTrailing,
            "IGNORE_ALL" => Self::IgnoreAll,
            other => Self::Unknown(other.to_owned()),
        }
    }

    /// The wire value, also usable as a stable cache key component.
    pub fn as_wire(&self) -> &str {
        match self {
            Self::IgnoreNone => "IGNORE_NONE",
            Self::IgnoreTrailing => "IGNORE_TRAILING",
            Self::IgnoreLeadingAndTrailing => "IGNORE_LEADING_AND_TRAILING",
            Self::IgnoreAll => "IGNORE_ALL",
            Self::Unknown(s) => s,
        }
    }
}

/// How many unchanged lines surround a change (`context=`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DiffContext {
    Lines(u32),
    /// The whole file.
    All,
}

impl DiffContext {
    /// Converts the number Gerrit stores in `DiffPreferencesInfo.context`.
    /// A negative value is how the web UI stores "entire file".
    pub fn from_pref(n: i64) -> Self {
        u32::try_from(n).map_or(Self::All, Self::Lines)
    }

    /// The `context=` query value: a number or `ALL`.
    pub fn to_wire(self) -> String {
        match self {
            Self::Lines(n) => n.to_string(),
            Self::All => "ALL".to_owned(),
        }
    }
}

/// Side-by-side or unified layout. Gerrit keeps this in the general
/// preferences (`diff_view`), not in the diff preferences.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DiffViewMode {
    SideBySide,
    Unified,
}

impl DiffViewMode {
    /// Maps Gerrit's `DiffView` (`SIDE_BY_SIDE`, `UNIFIED_DIFF`); `None`
    /// for values this client does not know.
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "SIDE_BY_SIDE" => Some(Self::SideBySide),
            "UNIFIED_DIFF" => Some(Self::Unified),
            _ => None,
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Self::SideBySide => Self::Unified,
            Self::Unified => Self::SideBySide,
        }
    }
}

/// Diff display settings from one source, each possibly unset. Used for
/// what the server reports (`DiffPreferencesInfo`, plus the view mode from
/// the general preferences) and for the user's local overrides in
/// `config.toml`. [`resolve_diff_preferences`] combines them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiffPreferences {
    pub context: Option<DiffContext>,
    pub ignore_whitespace: Option<Whitespace>,
    pub line_length: Option<u32>,
    pub tab_size: Option<u32>,
    pub show_tabs: Option<bool>,
    pub intraline_difference: Option<bool>,
    pub line_wrapping: Option<bool>,
    pub show_whitespace_errors: Option<bool>,
    pub hide_line_numbers: Option<bool>,
    pub syntax_highlighting: Option<bool>,
    pub expand_all_comments: Option<bool>,
    pub view_mode: Option<DiffViewMode>,
}

/// The settings a diff view actually uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedDiffPreferences {
    pub context: DiffContext,
    pub ignore_whitespace: Whitespace,
    pub line_length: u32,
    pub tab_size: u32,
    pub show_tabs: bool,
    pub intraline_difference: bool,
    pub line_wrapping: bool,
    pub show_whitespace_errors: bool,
    pub hide_line_numbers: bool,
    pub syntax_highlighting: bool,
    pub expand_all_comments: bool,
    pub view_mode: DiffViewMode,
}

impl Default for ResolvedDiffPreferences {
    /// Gerrit's own defaults.
    fn default() -> Self {
        Self {
            context: DiffContext::Lines(10),
            ignore_whitespace: Whitespace::IgnoreNone,
            line_length: 100,
            tab_size: 8,
            show_tabs: true,
            intraline_difference: true,
            line_wrapping: false,
            show_whitespace_errors: true,
            hide_line_numbers: false,
            syntax_highlighting: true,
            expand_all_comments: false,
            view_mode: DiffViewMode::SideBySide,
        }
    }
}

/// Effective diff settings: local overrides win over the server's, which
/// win over the built-in defaults. `server` is `None` when anonymous or
/// not yet fetched. A whitespace mode this client does not know is ignored
/// at every layer, so the view never has to guess.
pub fn resolve_diff_preferences(
    server: Option<&DiffPreferences>,
    local: &DiffPreferences,
) -> ResolvedDiffPreferences {
    let empty = DiffPreferences::default();
    let server = server.unwrap_or(&empty);
    let d = ResolvedDiffPreferences::default();
    macro_rules! pick {
        ($f:ident) => {
            local
                .$f
                .clone()
                .or_else(|| server.$f.clone())
                .unwrap_or(d.$f)
        };
    }
    let known = |w: &Option<Whitespace>| w.clone().filter(|w| !matches!(w, Whitespace::Unknown(_)));
    ResolvedDiffPreferences {
        context: pick!(context),
        ignore_whitespace: known(&local.ignore_whitespace)
            .or_else(|| known(&server.ignore_whitespace))
            .unwrap_or(d.ignore_whitespace),
        line_length: pick!(line_length),
        tab_size: pick!(tab_size),
        show_tabs: pick!(show_tabs),
        intraline_difference: pick!(intraline_difference),
        line_wrapping: pick!(line_wrapping),
        show_whitespace_errors: pick!(show_whitespace_errors),
        hide_line_numbers: pick!(hide_line_numbers),
        syntax_highlighting: pick!(syntax_highlighting),
        expand_all_comments: pick!(expand_all_comments),
        view_mode: pick!(view_mode),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_round_trips_and_keeps_unknown() {
        for w in [
            Whitespace::IgnoreNone,
            Whitespace::IgnoreTrailing,
            Whitespace::IgnoreLeadingAndTrailing,
            Whitespace::IgnoreAll,
        ] {
            assert_eq!(Whitespace::from_wire(w.as_wire()), w);
        }
        let u = Whitespace::from_wire("IGNORE_SOME");
        assert_eq!(u, Whitespace::Unknown("IGNORE_SOME".into()));
        assert_eq!(u.as_wire(), "IGNORE_SOME");
    }

    #[test]
    fn context_wire_and_pref_values() {
        assert_eq!(DiffContext::Lines(10).to_wire(), "10");
        assert_eq!(DiffContext::All.to_wire(), "ALL");
        assert_eq!(DiffContext::from_pref(3), DiffContext::Lines(3));
        assert_eq!(DiffContext::from_pref(0), DiffContext::Lines(0));
        assert_eq!(DiffContext::from_pref(-1), DiffContext::All);
    }

    #[test]
    fn view_mode_from_wire() {
        assert_eq!(
            DiffViewMode::from_wire("UNIFIED_DIFF"),
            Some(DiffViewMode::Unified)
        );
        assert_eq!(DiffViewMode::from_wire("SPLIT"), None);
        assert_eq!(DiffViewMode::Unified.toggled(), DiffViewMode::SideBySide);
    }

    #[test]
    fn nothing_set_gives_gerrit_defaults() {
        let r = resolve_diff_preferences(None, &DiffPreferences::default());
        assert_eq!(r, ResolvedDiffPreferences::default());
        assert_eq!(r.context, DiffContext::Lines(10));
        assert_eq!(r.view_mode, DiffViewMode::SideBySide);
    }

    #[test]
    fn local_beats_server_beats_default_per_field() {
        let server = DiffPreferences {
            context: Some(DiffContext::Lines(3)),
            tab_size: Some(4),
            line_length: Some(120),
            show_tabs: Some(false),
            view_mode: Some(DiffViewMode::Unified),
            ..Default::default()
        };
        let local = DiffPreferences {
            tab_size: Some(2),
            show_tabs: Some(true),
            ..Default::default()
        };
        let r = resolve_diff_preferences(Some(&server), &local);
        assert_eq!(r.tab_size, 2); // local
        assert!(r.show_tabs); // local false-vs-true: local wins
        assert_eq!(r.line_length, 120); // server
        assert_eq!(r.context, DiffContext::Lines(3)); // server
        assert_eq!(r.view_mode, DiffViewMode::Unified); // server
        assert!(r.intraline_difference); // default
    }

    #[test]
    fn local_false_overrides_server_true() {
        let server = DiffPreferences {
            syntax_highlighting: Some(true),
            ..Default::default()
        };
        let local = DiffPreferences {
            syntax_highlighting: Some(false),
            ..Default::default()
        };
        assert!(!resolve_diff_preferences(Some(&server), &local).syntax_highlighting);
    }

    #[test]
    fn unknown_whitespace_falls_through_to_the_next_layer() {
        let server = DiffPreferences {
            ignore_whitespace: Some(Whitespace::IgnoreAll),
            ..Default::default()
        };
        let local = DiffPreferences {
            ignore_whitespace: Some(Whitespace::Unknown("IGNORE_X".into())),
            ..Default::default()
        };
        assert_eq!(
            resolve_diff_preferences(Some(&server), &local).ignore_whitespace,
            Whitespace::IgnoreAll
        );
        let both = DiffPreferences {
            ignore_whitespace: Some(Whitespace::Unknown("Y".into())),
            ..Default::default()
        };
        assert_eq!(
            resolve_diff_preferences(Some(&both), &local).ignore_whitespace,
            Whitespace::IgnoreNone
        );
    }
}
