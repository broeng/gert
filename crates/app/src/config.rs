//! The TOML configuration file at `<config_dir>/<APP_NAME>/config.toml`.
//!
//! ```toml
//! # One table per Gerrit server. `id` is a slug made from the name when the
//! # server is added; it names the server in the cache and must not change
//! # (the name may). `color` is optional.
//! [[servers]]
//! id = "example"
//! name = "example"
//! url = "https://gerrit.example.com"
//! username = "jdoe"          # absent: anonymous, read-only access
//!
//! [[servers.queries]]
//! name = "Your turn"
//! query = "attention:self"
//!
//! # Optional: bookmarks, buttons on the dashboard that open a search.
//! [[servers.bookmarks]]
//! name = "My open"
//! query = "is:open owner:self"
//!
//! # Optional: file filters. Rules that hide, dim or highlight files of a
//! # change by path glob. `projects` (exact names), `color`, `bold` and
//! # `enabled` are optional; `enabled` defaults to true.
//! [[filters]]
//! name = "Generated"
//! globs = ["*.pb.go", "gen/**"]        # no "/": the name at any depth
//! projects = ["platform/foo"]          # absent or empty: every project
//! servers = ["example"]                # server ids; absent or empty: every server
//! action = "hide"                      # hide | dim | highlight
//! color = "#e5a50a"                    # highlight only
//! bold = true                          # highlight only
//!
//! # Optional: what opens on start. Default: the dashboard.
//! [startup]
//! page = "dashboard"                   # or instead: query = "status:open project:foo"
//!
//! # Optional: review flow.
//! [review]
//! advance_after_vote = true            # go to the next open change after a vote
//!
//! # Optional: look. The light or dark variant follows the system setting.
//! [appearance]
//! theme = "cwc"                        # or "solarized"
//! mode = "system"                      # or "light", "dark": the UI
//! diff_mode = "system"                 # or "light", "dark": the diff view
//!
//! # Optional: local overrides of the diff view. Every key is optional. They
//! # win over the preferences stored on the server (Settings > Preferences >
//! # Diff Preferences in the web UI) and are never written back to it.
//! [diff]
//! font_family = "JetBrains Mono"       # default: the theme's monospace font
//! view = "side-by-side"                # or "unified"
//! context = 10                         # lines around a change, or "all"
//! ignore_whitespace = "IGNORE_NONE"    # IGNORE_TRAILING, IGNORE_LEADING_AND_TRAILING, IGNORE_ALL
//! tab_size = 4
//! line_length = 100                    # column of the ruler
//! show_tabs = true
//! intraline_difference = true
//! line_wrapping = false
//! show_whitespace_errors = true
//! hide_line_numbers = false
//! syntax_highlighting = true
//! ```
//!
//! The HTTP password is never stored here; see [`crate::secrets`].
//!
//! Older files had a single `[server]` table with top-level `[[queries]]` and
//! `[[bookmarks]]`. The first load rewrites them in place into the layout
//! above (comments kept, no backup file).
//!
//! The file is also edited by hand, so the writer ([`Config::update`]) never
//! regenerates it: it re-reads the file, and applies only the keys that
//! changed to the parsed document, which keeps comments and formatting.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use model::{DiffContext, DiffPreferences, DiffViewMode, Whitespace};
use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item};
use ui::filters::{FileFilter, FilterAction, format_hex_color, parse_hex_color, validate_glob};
use ui::{ColorMode, ColorModes, ServerKey, ThemeName, server_name_from_url};

use crate::APP_NAME;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("cannot parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("cannot write {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
    #[error("cannot serialize the configuration: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("cannot parse {path} for editing, leaving it untouched: {message}")]
    Edit { path: PathBuf, message: String },
    #[error("{path}: server id {id:?} is not a slug (lowercase letters, digits, - and _)")]
    InvalidServerId { path: PathBuf, id: String },
    #[error("{path}: two servers have the id {id:?}; ids must be unique")]
    DuplicateServerId { path: PathBuf, id: String },
    #[error("no home directory found")]
    NoHome,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
    #[serde(default)]
    pub filters: Vec<FilterConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup: Option<StartupConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub appearance: Option<AppearanceConfig>,
}

/// What opens on start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartupPage {
    Dashboard,
    Query(String),
}

/// The `[startup]` section: `page = "dashboard"` or `query = "..."`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StartupConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
}

impl StartupConfig {
    /// The section that selects `page`.
    pub fn for_page(page: &StartupPage) -> Self {
        match page {
            StartupPage::Dashboard => Self {
                page: Some("dashboard".into()),
                query: None,
            },
            StartupPage::Query(q) => Self {
                page: None,
                query: Some(q.clone()),
            },
        }
    }

    /// A non-blank `query` wins; `page` must be "dashboard" (or absent).
    /// Anything else falls back to the dashboard with a warning.
    pub fn target(&self) -> StartupPage {
        if let Some(q) = self
            .query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
        {
            return StartupPage::Query(q.to_owned());
        }
        match self.page.as_deref().map(|p| p.trim().to_ascii_lowercase()) {
            None => StartupPage::Dashboard,
            Some(p) if p == "dashboard" => StartupPage::Dashboard,
            Some(p) => {
                log::warn!("config: [startup] page = {p:?} is not \"dashboard\"; using it");
                StartupPage::Dashboard
            }
        }
    }
}

/// The `[review]` section.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advance_after_vote: Option<bool>,
}

/// The `[appearance]` section.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceConfig {
    /// A [`ThemeName`] key; one this client does not know falls back to the
    /// default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// A [`ColorMode`] key for the UI; one this client does not know falls
    /// back to following the system.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// The same for the diff view.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_mode: Option<String>,
}

/// `context = 10` or `context = "all"`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContextSetting {
    Lines(u32),
    Word(String),
}

/// The `[diff]` section: local overrides of the server's diff preferences.
///
/// Enumerated values are strings and converted tolerantly: a view mode this
/// client does not know is dropped (with a warning), a whitespace mode it
/// does not know is kept as [`Whitespace::Unknown`], which the preference
/// resolution skips, so a typo falls back to the server's value or the
/// default instead of breaking the file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiffConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextSetting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore_whitespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_length: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_tabs: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intraline_difference: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_wrapping: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_whitespace_errors: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hide_line_numbers: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub syntax_highlighting: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expand_all_comments: Option<bool>,
}

impl DiffConfig {
    /// The settings as local overrides for `model::resolve_diff_preferences`.
    pub fn preferences(&self) -> DiffPreferences {
        let view_mode = self.view.as_deref().and_then(|v| {
            let mode = match v.trim().to_ascii_lowercase().as_str() {
                "side-by-side" | "side_by_side" => Some(DiffViewMode::SideBySide),
                "unified" | "unified_diff" => Some(DiffViewMode::Unified),
                _ => None,
            };
            if mode.is_none() {
                log::warn!("config: [diff] view = {v:?} is not \"side-by-side\" or \"unified\"");
            }
            mode
        });
        let context = self.context.as_ref().and_then(|c| match c {
            ContextSetting::Lines(n) => Some(DiffContext::Lines(*n)),
            ContextSetting::Word(w) if w.trim().eq_ignore_ascii_case("all") => {
                Some(DiffContext::All)
            }
            ContextSetting::Word(w) => {
                log::warn!("config: [diff] context = {w:?} is not a number or \"all\"");
                None
            }
        });
        DiffPreferences {
            context,
            ignore_whitespace: self
                .ignore_whitespace
                .as_deref()
                .map(|w| Whitespace::from_wire(w.trim())),
            line_length: self.line_length,
            tab_size: self.tab_size,
            show_tabs: self.show_tabs,
            intraline_difference: self.intraline_difference,
            line_wrapping: self.line_wrapping,
            show_whitespace_errors: self.show_whitespace_errors,
            hide_line_numbers: self.hide_line_numbers,
            syntax_highlighting: self.syntax_highlighting,
            expand_all_comments: self.expand_all_comments,
            view_mode,
        }
    }
}

/// A `[[servers]]` table: one Gerrit server with its saved queries and
/// bookmarks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    /// A slug, stable for the life of the server entry: the cache and the
    /// UI refer to the server by it. See [`slugify`] and [`is_slug`].
    pub id: String,
    pub name: String,
    pub url: String,
    /// `None` means anonymous access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// `"#rrggbb"`; absent: assigned automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queries: Vec<QueryConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bookmarks: Vec<QueryConfig>,
}

impl ServerConfig {
    /// Writes the scalar keys and the nested lists into `t`, touching only
    /// what differs, so comments and the spelling of the rest survive.
    fn fill(&self, t: &mut toml_edit::Table) {
        set_str(t, "id", &self.id);
        set_str(t, "name", &self.name);
        set_str(t, "url", &self.url);
        match &self.username {
            Some(u) => set_str(t, "username", u),
            None => {
                t.remove("username");
            }
        }
        match &self.color {
            Some(c) => set_str(t, "color", c),
            None => {
                t.remove("color");
            }
        }
        for (key, list) in [("queries", &self.queries), ("bookmarks", &self.bookmarks)] {
            if table_queries(t, key) != *list {
                apply_queries(key, list, t);
            }
        }
    }
}

/// True for a usable server id: lowercase ASCII letters, digits, `-` and
/// `_`, not empty.
pub fn is_slug(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// A slug for `name`: lowercase letters and digits, runs of anything else
/// become one `-`. `"server"` if nothing is left.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-');
    if out.is_empty() { "server" } else { out }.to_owned()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryConfig {
    pub name: String,
    pub query: String,
}

/// A `[[filters]]` table. The fields are plain strings and options, so a
/// typo in a value never fails the whole file; [`FilterConfig::to_filter`]
/// drops what it cannot use, with a warning.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FilterConfig {
    pub name: String,
    pub globs: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<String>,
    /// Ids of the servers the rule applies to; empty: all of them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<String>,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl FilterConfig {
    /// The rule as the UI uses it. A rule with an unknown action or no name
    /// can do nothing and is dropped; a bad color or glob is dropped from
    /// an otherwise good rule.
    pub fn to_filter(&self) -> Option<FileFilter> {
        let name = self.name.trim();
        if name.is_empty() {
            log::warn!("config: ignoring a [[filters]] entry without a name");
            return None;
        }
        let action = match self.action.trim().to_ascii_lowercase().as_str() {
            "hide" => FilterAction::Hide,
            "dim" => FilterAction::Dim,
            "highlight" => FilterAction::Highlight {
                color: self.color.as_deref().and_then(|c| {
                    let color = parse_hex_color(c);
                    if color.is_none() {
                        log::warn!("config: filter {name:?}: color {c:?} is not \"#rrggbb\"");
                    }
                    color
                }),
                bold: self.bold.unwrap_or(false),
            },
            other => {
                log::warn!(
                    "config: ignoring filter {name:?}: action {other:?} is not \
                     \"hide\", \"dim\" or \"highlight\""
                );
                return None;
            }
        };
        let globs = self
            .globs
            .iter()
            .filter(|g| match validate_glob(g) {
                Ok(()) => true,
                Err(e) => {
                    log::warn!("config: filter {name:?}: ignoring glob {g:?}: {e}");
                    false
                }
            })
            .cloned()
            .collect();
        Some(FileFilter {
            name: name.to_owned(),
            globs,
            projects: self.projects.clone(),
            servers: self.servers.iter().map(ServerKey::new).collect(),
            action,
            enabled: self.enabled.unwrap_or(true),
        })
    }

    /// The table for a rule; default values are left as `None` or empty so
    /// they are not written.
    pub fn from_filter(f: &FileFilter) -> Self {
        let (action, color, bold) = match &f.action {
            FilterAction::Hide => ("hide", None, None),
            FilterAction::Dim => ("dim", None, None),
            FilterAction::Highlight { color, bold } => (
                "highlight",
                color.map(format_hex_color),
                bold.then_some(true),
            ),
        };
        Self {
            name: f.name.clone(),
            globs: f.globs.clone(),
            projects: f.projects.clone(),
            servers: f.servers.iter().map(ToString::to_string).collect(),
            action: action.to_owned(),
            color,
            bold,
            enabled: (!f.enabled).then_some(false),
        }
    }

    /// The same rule with explicit defaults (`enabled = true`, `bold =
    /// false`) removed, for matching tables of the file by content.
    fn normalized(mut self) -> Self {
        if self.enabled == Some(true) {
            self.enabled = None;
        }
        if self.bold == Some(false) {
            self.bold = None;
        }
        self
    }

    /// Reads a table of the document; `None` if a key has the wrong type.
    fn from_table(t: &toml_edit::Table) -> Option<Self> {
        let strings = |key: &str| -> Option<Vec<String>> {
            match t.get(key) {
                None => Some(Vec::new()),
                Some(item) => item
                    .as_array()?
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned))
                    .collect(),
            }
        };
        let opt = |key: &str| t.get(key);
        Some(
            Self {
                name: opt("name")?.as_str()?.to_owned(),
                globs: strings("globs")?,
                projects: strings("projects")?,
                servers: strings("servers")?,
                action: opt("action")?.as_str()?.to_owned(),
                color: match opt("color") {
                    None => None,
                    Some(c) => Some(c.as_str()?.to_owned()),
                },
                bold: match opt("bold") {
                    None => None,
                    Some(b) => Some(b.as_bool()?),
                },
                enabled: match opt("enabled") {
                    None => None,
                    Some(b) => Some(b.as_bool()?),
                },
            }
            .normalized(),
        )
    }

    /// Writes this rule into `t`, keeping the comments and the key order of
    /// what is there. Keys that hold a default are removed.
    fn fill(&self, t: &mut toml_edit::Table) {
        set_value(t, "name", self.name.as_str().into());
        set_value(t, "globs", self.globs.iter().map(String::as_str).collect());
        let projects =
            (!self.projects.is_empty()).then(|| self.projects.iter().map(String::as_str).collect());
        set_or_remove(t, "projects", projects);
        let servers =
            (!self.servers.is_empty()).then(|| self.servers.iter().map(String::as_str).collect());
        set_or_remove(t, "servers", servers);
        set_value(t, "action", self.action.as_str().into());
        set_or_remove(t, "color", self.color.as_deref().map(Into::into));
        set_or_remove(t, "bold", self.bold.map(Into::into));
        set_or_remove(t, "enabled", self.enabled.map(Into::into));
    }
}

fn q(name: &str, query: &str) -> QueryConfig {
    QueryConfig {
        name: name.to_owned(),
        query: query.to_owned(),
    }
}

/// Saved queries written on first login. An anonymous user has no `self`,
/// so only queries that work without an account are seeded for them.
pub fn default_queries(anonymous: bool) -> Vec<QueryConfig> {
    if anonymous {
        return vec![
            q("Open changes", "status:open"),
            q("Recently merged", "status:merged -age:1w"),
        ];
    }
    vec![
        q("Your turn", "attention:self"),
        q("Work in progress", "is:open owner:self is:wip"),
        q("Outgoing reviews", "is:open owner:self -is:wip"),
        q(
            "Incoming reviews",
            "is:open -owner:self -is:wip reviewer:self",
        ),
        q("CCed", "is:open -owner:self cc:self"),
        q(
            "Recently closed",
            "is:closed (owner:self OR reviewer:self) -age:4w",
        ),
    ]
}

impl Config {
    /// The server with this id.
    pub fn server(&self, id: &str) -> Option<&ServerConfig> {
        self.servers.iter().find(|s| s.id == id)
    }

    /// An id for a new server called `name`: its slug, with `-2`, `-3`, ...
    /// added if another server has it. The id is stored and never derived
    /// again, so renaming a server does not change it.
    pub fn unique_server_id(&self, name: &str) -> String {
        let base = slugify(name);
        let mut id = base.clone();
        let mut n = 2;
        while self.server(&id).is_some() {
            id = format!("{base}-{n}");
            n += 1;
        }
        id
    }

    /// The configuration after a successful login to server `id`: the
    /// server is updated if it exists (name, color and bookmarks stay), else
    /// added at the end, named after its host. A server without saved
    /// queries gets the default ones. Everything else is kept.
    pub fn after_login(mut self, id: &str, url: &str, username: Option<&str>) -> Config {
        let anonymous = username.is_none();
        match self.servers.iter_mut().find(|s| s.id == id) {
            Some(s) => {
                s.url = url.to_owned();
                s.username = username.map(str::to_owned);
                if s.queries.is_empty() {
                    s.queries = default_queries(anonymous);
                }
            }
            None => self.servers.push(ServerConfig {
                id: id.to_owned(),
                name: server_name_from_url(url),
                url: url.to_owned(),
                username: username.map(str::to_owned),
                color: None,
                queries: default_queries(anonymous),
                bookmarks: Vec::new(),
            }),
        }
        self
    }

    /// The usable `[[filters]]` rules, in file order. A later rule with the
    /// name of an earlier one is dropped: the names are the identity of the
    /// toggle buttons.
    pub fn file_filters(&self) -> Vec<FileFilter> {
        let mut out: Vec<FileFilter> = Vec::new();
        for f in self.filters.iter().filter_map(FilterConfig::to_filter) {
            if out.iter().any(|o| o.name == f.name) {
                log::warn!("config: ignoring a second filter named {:?}", f.name);
            } else {
                out.push(f);
            }
        }
        out
    }

    /// The `[[filters]]` entries that [`Config::file_filters`] ignores: no
    /// name, an unknown action, or a name used before. A save from the UI
    /// keeps them in the file, so a hand-written rule is never lost just
    /// because the loader could not use it.
    pub fn ignored_filters(&self) -> Vec<FilterConfig> {
        let mut seen: Vec<String> = Vec::new();
        let mut out = Vec::new();
        for f in &self.filters {
            match f.to_filter() {
                Some(r) if !seen.contains(&r.name) => seen.push(r.name),
                _ => out.push(f.clone()),
            }
        }
        out
    }

    pub fn startup_page(&self) -> StartupPage {
        self.startup
            .as_ref()
            .map(StartupConfig::target)
            .unwrap_or(StartupPage::Dashboard)
    }

    /// Default on.
    pub fn advance_after_vote(&self) -> bool {
        self.review
            .as_ref()
            .and_then(|r| r.advance_after_vote)
            .unwrap_or(true)
    }

    /// Default: [`ThemeName::default`], also for a name this client does
    /// not know (with a warning).
    pub fn theme(&self) -> ThemeName {
        let Some(key) = self.appearance.as_ref().and_then(|a| a.theme.as_deref()) else {
            return ThemeName::default();
        };
        ThemeName::from_key(key).unwrap_or_else(|| {
            let default = ThemeName::default();
            log::warn!(
                "config: [appearance] theme = {key:?} is unknown; using {:?}",
                default.key()
            );
            default
        })
    }

    /// Default: following the system, also for a value this client does not
    /// know (with a warning).
    pub fn color_modes(&self) -> ColorModes {
        let appearance = self.appearance.as_ref();
        let read = |name: &str, value: Option<&String>| {
            let Some(key) = value else {
                return ColorMode::default();
            };
            ColorMode::from_key(key).unwrap_or_else(|| {
                log::warn!(
                    "config: [appearance] {name} = {key:?} is unknown; following the system"
                );
                ColorMode::default()
            })
        };
        ColorModes {
            ui: read("mode", appearance.and_then(|a| a.mode.as_ref())),
            diff: read("diff_mode", appearance.and_then(|a| a.diff_mode.as_ref())),
        }
    }

    /// Reads the file; `Ok(None)` if it does not exist. A file in the old
    /// layout (`[server]`) is rewritten in the current one first.
    pub fn load(path: &Path) -> Result<Option<Config>, ConfigError> {
        let Some(mut text) = read_text(path)? else {
            return Ok(None);
        };
        if let Some(migrated) = migrate_legacy(&text) {
            write_atomic(path, &migrated)?;
            text = migrated;
        }
        parse(path, &text).map(Some)
    }

    /// Writes this configuration as a whole, as [`Config::update`] would.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        Self::update(path, |c| *c = self.clone()).map(|_| ())
    }

    /// Re-reads the file, applies `change` to what is on disk now, and
    /// writes back only the keys that `change` altered, into the parsed
    /// document so comments and formatting survive. Reading at save time
    /// merges with edits made to the file since it was loaded. A file that
    /// does not parse is refused and left untouched; a missing one is
    /// created. A file in the old layout is migrated as part of the write.
    /// Returns the configuration as written.
    pub fn update(path: &Path, change: impl FnOnce(&mut Config)) -> Result<Config, ConfigError> {
        let mut text = read_text(path)?.unwrap_or_default();
        let migrated = migrate_legacy(&text);
        if let Some(m) = &migrated {
            text = m.clone();
        }
        let old = parse(path, &text)?;
        let mut doc: DocumentMut =
            text.parse()
                .map_err(|e: toml_edit::TomlError| ConfigError::Edit {
                    path: path.to_owned(),
                    message: e.to_string(),
                })?;
        let mut new = old.clone();
        change(&mut new);
        if new != old {
            validate(path, &new)?;
            apply_changes(&old, &new, &mut doc)?;
            write_atomic(path, &doc.to_string())?;
        } else if migrated.is_some() {
            write_atomic(path, &text)?;
        }
        Ok(new)
    }
}

fn read_text(path: &Path) -> Result<Option<String>, ConfigError> {
    match fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Read {
            path: path.to_owned(),
            source,
        }),
    }
}

fn parse(path: &Path, text: &str) -> Result<Config, ConfigError> {
    let config: Config = toml::from_str(text).map_err(|source| ConfigError::Parse {
        path: path.to_owned(),
        source,
    })?;
    validate(path, &config)?;
    Ok(config)
}

/// Every server id is a slug, and no two are the same.
fn validate(path: &Path, config: &Config) -> Result<(), ConfigError> {
    for (i, s) in config.servers.iter().enumerate() {
        if !is_slug(&s.id) {
            return Err(ConfigError::InvalidServerId {
                path: path.to_owned(),
                id: s.id.clone(),
            });
        }
        if config.servers[..i].iter().any(|o| o.id == s.id) {
            return Err(ConfigError::DuplicateServerId {
                path: path.to_owned(),
                id: s.id.clone(),
            });
        }
    }
    Ok(())
}

/// The text of a file in the old layout (a `[server]` table, top-level
/// `[[queries]]` and `[[bookmarks]]`) in the current one, or `None` if the
/// file is not in the old layout (or does not parse; the caller reports
/// that). The `[server]` table becomes the first `[[servers]]` table, named
/// after its host, and the lists move into it, all with their comments.
fn migrate_legacy(text: &str) -> Option<String> {
    let mut doc: DocumentMut = text.parse().ok()?;
    if doc.contains_key("servers") {
        return None;
    }
    let old = match doc.get("server")? {
        Item::Table(t) => t.clone(),
        Item::Value(toml_edit::Value::InlineTable(t)) => t.clone().into_table(),
        _ => return None,
    };
    let url = old.get("url")?.as_str()?.to_owned();
    doc.remove("server");
    let name = server_name_from_url(&url);
    let mut table = toml_edit::Table::new();
    *table.decor_mut() = old.decor().clone();
    // The new table takes the place of the old one in the file.
    table.set_position(old.position());
    table.insert("id", slugify(&name).as_str().into());
    table.insert("name", name.as_str().into());
    for (key, item) in old {
        table.insert(&key, item);
    }
    for key in ["queries", "bookmarks"] {
        if let Some(Item::ArrayOfTables(list)) = doc.remove(key) {
            table.insert(key, Item::ArrayOfTables(list));
        }
    }
    clear_nested_positions(&mut table);
    let mut servers = toml_edit::ArrayOfTables::new();
    servers.push(table);
    doc.insert("servers", Item::ArrayOfTables(servers));
    Some(doc.to_string())
}

/// The scalar and table keys of `new` that differ from `old` (everything
/// but the server and filter lists) go into `doc`.
fn apply_changes(old: &Config, new: &Config, doc: &mut DocumentMut) -> Result<(), ConfigError> {
    let tables = |c: &Config| -> Result<toml::Table, ConfigError> {
        let mut t = toml::Table::try_from(c)?;
        t.remove("servers");
        t.remove("filters");
        Ok(t)
    };
    apply_table(&tables(old)?, &tables(new)?, doc.as_table_mut());
    if old.servers != new.servers {
        apply_servers(&new.servers, doc.as_table_mut());
    }
    if old.filters != new.filters {
        apply_filters(&new.filters, doc.as_table_mut());
    }
    Ok(())
}

fn apply_table(old: &toml::Table, new: &toml::Table, target: &mut toml_edit::Table) {
    for key in old.keys().filter(|k| !new.contains_key(*k)) {
        target.remove(key);
    }
    for (key, value) in new {
        if old.get(key) == Some(value) {
            continue;
        }
        if let toml::Value::Table(sub) = value {
            if !matches!(target.get(key), Some(Item::Table(_))) {
                let mut t = toml_edit::Table::new();
                t.set_implicit(false);
                target.insert(key, Item::Table(t));
            }
            if let Some(Item::Table(t)) = target.get_mut(key) {
                let empty = toml::Table::new();
                let before = match old.get(key) {
                    Some(toml::Value::Table(o)) => o,
                    _ => &empty,
                };
                apply_table(before, sub, t);
            }
            continue;
        }
        let mut v = to_edit_value(value);
        // Keep an inline comment on the line of the key we replace.
        if let Some(Item::Value(prev)) = target.get(key) {
            *v.decor_mut() = prev.decor().clone();
        }
        target.insert(key, Item::Value(v));
    }
}

fn to_edit_value(v: &toml::Value) -> toml_edit::Value {
    match v {
        toml::Value::String(s) => s.as_str().into(),
        toml::Value::Integer(i) => (*i).into(),
        toml::Value::Float(f) => (*f).into(),
        toml::Value::Boolean(b) => (*b).into(),
        toml::Value::Datetime(d) => d.to_string().into(),
        toml::Value::Array(a) => a.iter().map(to_edit_value).collect(),
        toml::Value::Table(t) => {
            let mut inline = toml_edit::InlineTable::new();
            for (k, v) in t {
                inline.insert(k, to_edit_value(v));
            }
            inline.into()
        }
    }
}

fn query_of(t: &toml_edit::Table) -> Option<(String, String)> {
    Some((
        t.get("name")?.as_str()?.to_owned(),
        t.get("query")?.as_str()?.to_owned(),
    ))
}

fn set_value(t: &mut toml_edit::Table, key: &str, mut v: toml_edit::Value) {
    if let Some(Item::Value(prev)) = t.get(key) {
        *v.decor_mut() = prev.decor().clone();
    }
    t.insert(key, Item::Value(v));
}

/// Clears the parsed position of the tables inside `t`, see [`apply_servers`].
fn clear_nested_positions(t: &mut toml_edit::Table) {
    for (_, item) in t.iter_mut() {
        if let Item::ArrayOfTables(list) = item {
            for nested in list.iter_mut() {
                nested.set_position(None);
                clear_nested_positions(nested);
            }
        }
    }
}

/// Sets a string key, leaving the value alone (and so its spelling) when it
/// already holds `v`.
fn set_str(t: &mut toml_edit::Table, key: &str, v: &str) {
    if t.get(key).and_then(Item::as_str) != Some(v) {
        set_value(t, key, v.into());
    }
}

/// The queries of the array of tables `key` in `t`.
fn table_queries(t: &toml_edit::Table, key: &str) -> Vec<QueryConfig> {
    match t.get(key) {
        Some(Item::ArrayOfTables(a)) => a
            .iter()
            .filter_map(query_of)
            .map(|(name, query)| QueryConfig { name, query })
            .collect(),
        _ => Vec::new(),
    }
}

/// Rewrites the `[[servers]]` tables. A server is matched to its table by
/// `id` (the table is reused, so its comments stay, and `fill` changes only
/// the keys that differ); a server with a new id gets a fresh table, and a
/// table whose id is gone is dropped with its nested lists.
fn apply_servers(new: &[ServerConfig], root: &mut toml_edit::Table) {
    let mut existing: Vec<Option<toml_edit::Table>> = match root.get("servers") {
        Some(Item::ArrayOfTables(a)) => a.iter().cloned().map(Some).collect(),
        _ => Vec::new(),
    };
    let mut out = toml_edit::ArrayOfTables::new();
    for server in new {
        let slot = existing.iter_mut().find(|t| {
            t.as_ref().and_then(|t| t.get("id")).and_then(Item::as_str) == Some(&server.id)
        });
        let mut table = slot.and_then(Option::take).unwrap_or_default();
        server.fill(&mut table);
        // Tables print sorted by their parsed position; clear it so that
        // they follow the order of the array, each list after its server.
        table.set_position(None);
        clear_nested_positions(&mut table);
        out.push(table);
    }
    if out.is_empty() {
        root.remove("servers");
    } else {
        root.insert("servers", Item::ArrayOfTables(out));
    }
}

fn set_or_remove(t: &mut toml_edit::Table, key: &str, v: Option<toml_edit::Value>) {
    match v {
        Some(v) => set_value(t, key, v),
        None => {
            t.remove(key);
        }
    }
}

/// Rewrites the `[[queries]]` or `[[bookmarks]]` tables (`key`), see
/// [`apply_array`].
fn apply_queries(key: &str, new: &[QueryConfig], root: &mut toml_edit::Table) {
    apply_array(
        root,
        key,
        new,
        |q| Some((q.name.clone(), q.query.clone())),
        query_of,
        |q, t| {
            set_value(t, "name", q.name.as_str().into());
            set_value(t, "query", q.query.as_str().into());
        },
    );
}

/// Rewrites the `[[filters]]` tables, see [`apply_array`]. Tables are
/// matched on the rule they hold, not on how it is spelled: an explicit
/// `enabled = true` is the same as none.
fn apply_filters(new: &[FilterConfig], root: &mut toml_edit::Table) {
    apply_array(
        root,
        "filters",
        new,
        |f| Some(f.clone().normalized()),
        FilterConfig::from_table,
        |f, t| f.fill(t),
    );
}

/// Rewrites the array of tables `key`. A table of the file that equals a new
/// entry (by `table_key`) is reused as is, with its comments, wherever the
/// entry moved to. A new entry that matches nothing takes over the unused
/// table at its own position when there is one (an edit in place: `fill`
/// replaces the values, comments and other keys stay), else a fresh table is
/// made. The limit: comments belong to the table below them, so deleting an
/// entry deletes the comment lines directly above it, and a section comment
/// above the first table moves with that table. Tables are matched by
/// content, so hand edits of the list on disk are respected.
fn apply_array<T, K: PartialEq>(
    root: &mut toml_edit::Table,
    key: &str,
    new: &[T],
    entry_key: impl Fn(&T) -> Option<K>,
    table_key: impl Fn(&toml_edit::Table) -> Option<K>,
    fill: impl Fn(&T, &mut toml_edit::Table),
) {
    let mut existing: Vec<Option<toml_edit::Table>> = match root.get(key) {
        Some(Item::ArrayOfTables(a)) => a.iter().cloned().map(Some).collect(),
        _ => Vec::new(),
    };
    let existing_keys: Vec<Option<K>> = existing
        .iter()
        .map(|t| t.as_ref().and_then(&table_key))
        .collect();
    let mut chosen: Vec<Option<toml_edit::Table>> = new.iter().map(|_| None).collect();
    for (i, entry) in new.iter().enumerate() {
        let wanted = entry_key(entry);
        if wanted.is_none() {
            continue;
        }
        if let Some(j) =
            (0..existing.len()).find(|&j| existing[j].is_some() && existing_keys[j] == wanted)
        {
            chosen[i] = existing[j].take();
        }
    }
    let mut out = toml_edit::ArrayOfTables::new();
    for (i, entry) in new.iter().enumerate() {
        let mut table = match chosen[i].take() {
            Some(t) => t,
            None => {
                let mut t = existing
                    .get_mut(i)
                    .and_then(Option::take)
                    .unwrap_or_default();
                fill(entry, &mut t);
                t
            }
        };
        // Tables print sorted by their parsed position; clear it so the new
        // order (the order of the array) wins.
        table.set_position(None);
        out.push(table);
    }
    if out.is_empty() {
        root.remove(key);
    } else {
        root.insert(key, Item::ArrayOfTables(out));
    }
}

/// Temp file in the same directory, flushed to disk, then renamed over the
/// target, so a crash leaves either the old or the new file.
fn write_atomic(path: &Path, text: &str) -> Result<(), ConfigError> {
    use io::Write as _;
    let werr = |source| ConfigError::Write {
        path: path.to_owned(),
        source,
    };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(werr)?;
    }
    let tmp = path.with_extension("toml.tmp");
    let mut f = fs::File::create(&tmp).map_err(werr)?;
    f.write_all(text.as_bytes()).map_err(werr)?;
    f.sync_all().map_err(werr)?;
    drop(f);
    fs::rename(&tmp, path).map_err(werr)
}

/// Where this application keeps its files.
#[derive(Clone, Debug)]
pub struct Paths {
    pub config_file: PathBuf,
    pub cache_dir: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Paths, ConfigError> {
        let dirs = ProjectDirs::from("", "", APP_NAME).ok_or(ConfigError::NoHome)?;
        Ok(Paths {
            config_file: dirs.config_dir().join("config.toml"),
            cache_dir: dirs.cache_dir().to_owned(),
        })
    }

    /// The SQLite cache, with its directory created.
    pub fn cache_db(&self) -> Result<PathBuf, ConfigError> {
        fs::create_dir_all(&self.cache_dir).map_err(|source| ConfigError::Write {
            path: self.cache_dir.clone(),
            source,
        })?;
        Ok(self.cache_dir.join("cache.sqlite"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("{APP_NAME}-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn missing_file_is_none() {
        let dir = tmp("missing");
        assert_eq!(Config::load(&dir.join("config.toml")).unwrap(), None);
    }

    #[test]
    fn save_creates_dirs_and_round_trips() {
        let dir = tmp("roundtrip");
        let path = dir.join("nested").join("config.toml");
        let cfg = Config::default().after_login("g", "https://g.example.com", Some("jdoe"));
        cfg.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), Some(cfg));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_documented_shape() {
        let cfg: Config = toml::from_str(
            r##"
            [[servers]]
            id = "work"
            name = "Work"
            url = "https://gerrit.example.com"
            username = "jdoe"
            color = "#112233"
            [[servers.queries]]
            name = "Your turn"
            query = "attention:self"
            [[servers.bookmarks]]
            name = "Mine"
            query = "owner:self"
            "##,
        )
        .unwrap();
        let s = &cfg.servers[0];
        assert_eq!((s.id.as_str(), s.name.as_str()), ("work", "Work"));
        assert_eq!(s.username.as_deref(), Some("jdoe"));
        assert_eq!(s.color.as_deref(), Some("#112233"));
        assert_eq!(s.queries, vec![q("Your turn", "attention:self")]);
        assert_eq!(s.bookmarks, vec![q("Mine", "owner:self")]);
    }

    #[test]
    fn anonymous_has_no_username_key_and_no_password_anywhere() {
        let cfg = Config::default().after_login("g", "https://g.example.com", None);
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert!(!text.contains("username"));
        assert!(!text.contains("password"));
        assert_eq!(cfg.servers[0].queries, default_queries(true));
    }

    #[test]
    fn login_seeds_defaults_once() {
        let cfg = Config::default().after_login("g", "https://g.example.com", Some("jdoe"));
        assert_eq!(cfg.servers[0].queries.len(), 6);
        assert_eq!(cfg.servers[0].queries[0], q("Your turn", "attention:self"));
        // A user's edited list survives a re-login, which updates the
        // server in place and keeps its name.
        let mut edited = cfg.clone();
        edited.servers[0].queries = vec![q("Mine", "owner:self")];
        let again = edited.after_login("g", "https://other.example.com", Some("x"));
        assert_eq!(again.servers.len(), 1);
        assert_eq!(again.servers[0].queries, vec![q("Mine", "owner:self")]);
        assert_eq!(again.servers[0].url, "https://other.example.com");
        assert_eq!(again.servers[0].name, "g.example.com");
    }

    #[test]
    fn diff_section_is_optional() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.diff, None);
        let none = DiffConfig::default().preferences();
        assert_eq!(none, DiffPreferences::default());
    }

    #[test]
    fn diff_section_maps_to_preferences() {
        let cfg: Config = toml::from_str(
            r#"
            [diff]
            font_family = "Iosevka"
            view = "unified"
            context = "all"
            ignore_whitespace = "IGNORE_TRAILING"
            tab_size = 4
            show_tabs = false
            "#,
        )
        .unwrap();
        let diff = cfg.diff.unwrap();
        assert_eq!(diff.font_family.as_deref(), Some("Iosevka"));
        let p = diff.preferences();
        assert_eq!(p.view_mode, Some(DiffViewMode::Unified));
        assert_eq!(p.context, Some(DiffContext::All));
        assert_eq!(p.ignore_whitespace, Some(Whitespace::IgnoreTrailing));
        assert_eq!(p.tab_size, Some(4));
        assert_eq!(p.show_tabs, Some(false));
        assert_eq!(p.line_length, None);
    }

    #[test]
    fn partial_diff_section_leaves_the_rest_unset() {
        let cfg: Config = toml::from_str("[diff]\ncontext = 3\n").unwrap();
        let p = cfg.diff.unwrap().preferences();
        assert_eq!(p.context, Some(DiffContext::Lines(3)));
        assert_eq!(
            DiffPreferences { context: None, ..p },
            DiffPreferences::default()
        );
    }

    #[test]
    fn unknown_values_are_tolerated_and_resolve_to_defaults() {
        let cfg: Config = toml::from_str(
            r#"
            [diff]
            ignore_whitespace = "IGNORE_SOME"
            view = "triple"
            context = "lots"
            "#,
        )
        .unwrap();
        let p = cfg.diff.unwrap().preferences();
        assert_eq!(
            p.ignore_whitespace,
            Some(Whitespace::Unknown("IGNORE_SOME".into()))
        );
        assert_eq!(p.view_mode, None);
        assert_eq!(p.context, None);
        let r = model::resolve_diff_preferences(None, &p);
        assert_eq!(r, model::ResolvedDiffPreferences::default());
    }

    #[test]
    fn login_and_save_keep_the_diff_section() {
        let dir = tmp("keepdiff");
        let path = dir.join("config.toml");
        let cfg = Config {
            diff: Some(DiffConfig {
                font_family: Some("Iosevka".into()),
                context: Some(ContextSetting::Lines(7)),
                ..Default::default()
            }),
            ..Default::default()
        }
        .after_login("g", "https://g.example.com", Some("jdoe"));
        cfg.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[diff]"), "{text}");
        assert!(!text.contains("tab_size"), "{text}");
        assert_eq!(Config::load(&path).unwrap(), Some(cfg));
        let _ = fs::remove_dir_all(&dir);
    }

    const COMMENTED: &str = r#"# My config, edited by hand.

# The server.
[[servers]]
id = "g"
name = "G"
url = "https://g.example.com" # prod
username = "jdoe"

# --- queries ---
# first
[[servers.queries]]
name = "A"
query = "status:open" # inline a

# second
[[servers.queries]]
name = "B"
query = "owner:self"

# third
[[servers.queries]]
name = "C"
query = "is:wip"

# Diff look.
[diff]
tab_size = 4 # four
# trailing diff comment

[review]
# advance after a vote
advance_after_vote = false
"#;

    fn commented(name: &str) -> PathBuf {
        let dir = tmp(name);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, COMMENTED).unwrap();
        path
    }

    fn lines_kept(text: &str, gone: &[&str]) {
        for line in COMMENTED.lines().filter(|l| !l.is_empty()) {
            if !gone.iter().any(|g| line.contains(g)) {
                assert!(text.contains(line), "lost {line:?} in\n{text}");
            }
        }
    }

    #[test]
    fn editing_one_query_keeps_every_other_comment() {
        let path = commented("edit1");
        Config::update(&path, |c| {
            c.servers[0].queries[1].query = "owner:self is:open".into()
        })
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        lines_kept(&text, &["owner:self"]);
        assert!(text.contains("# second"), "{text}");
        assert!(text.contains("query = \"owner:self is:open\""), "{text}");
        let cfg = Config::load(&path).unwrap().unwrap();
        assert_eq!(cfg.servers[0].queries[1], q("B", "owner:self is:open"));
        assert_eq!(cfg.servers[0].queries.len(), 3);
    }

    #[test]
    fn adding_a_query_keeps_every_comment() {
        let path = commented("add1");
        Config::update(&path, |c| c.servers[0].queries.push(q("D", "is:merged"))).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        lines_kept(&text, &[]);
        assert_eq!(
            Config::load(&path).unwrap().unwrap().servers[0]
                .queries
                .len(),
            4
        );
    }

    #[test]
    fn deleting_a_query_drops_only_its_own_comment() {
        let path = commented("del1");
        Config::update(&path, |c| {
            c.servers[0].queries.remove(1);
        })
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        lines_kept(&text, &["# second", "\"B\"", "owner:self"]);
        let cfg = Config::load(&path).unwrap().unwrap();
        assert_eq!(
            cfg.servers[0].queries,
            vec![q("A", "status:open"), q("C", "is:wip")]
        );
    }

    #[test]
    fn reordering_queries_moves_their_comments_along() {
        let path = commented("reorder1");
        Config::update(&path, |c| c.servers[0].queries.swap(0, 2)).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        lines_kept(&text, &[]);
        let third = text.find("# third").unwrap();
        let first = text.find("# first").unwrap();
        assert!(third < first, "{text}");
        let cfg = Config::load(&path).unwrap().unwrap();
        assert_eq!(cfg.servers[0].queries[0].name, "C");
        assert_eq!(cfg.servers[0].queries[2].name, "A");
    }

    #[test]
    fn changing_a_scalar_keeps_its_inline_comment() {
        let path = commented("scalar1");
        Config::update(&path, |c| {
            c.diff.get_or_insert_with(Default::default).tab_size = Some(8);
            c.servers[0].url = "https://h.example.com".into();
        })
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("tab_size = 8 # four"), "{text}");
        assert!(
            text.contains("url = \"https://h.example.com\" # prod"),
            "{text}"
        );
        lines_kept(&text, &["tab_size", "url ="]);
    }

    #[test]
    fn login_on_a_commented_file_keeps_the_comments() {
        let path = commented("login1");
        Config::update(&path, |c| {
            *c = std::mem::take(c).after_login("g", "https://new.example.com", Some("x"));
        })
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        lines_kept(&text, &["url =", "username"]);
        let cfg = Config::load(&path).unwrap().unwrap();
        assert_eq!(cfg.servers[0].url, "https://new.example.com");
        assert_eq!(cfg.servers[0].queries.len(), 3);
    }

    #[test]
    fn an_unchanged_update_does_not_touch_the_file() {
        let path = commented("noop1");
        Config::update(&path, |_| {}).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), COMMENTED);
    }

    #[test]
    fn an_unparseable_file_is_refused_and_untouched() {
        let dir = tmp("refuse1");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        for bad in ["[server\nurl = ", "servers = 3\n"] {
            fs::write(&path, bad).unwrap();
            let err =
                Config::update(&path, |c| c.filters.push(FilterConfig::default())).unwrap_err();
            assert!(err.to_string().contains("config.toml"), "{err}");
            assert_eq!(fs::read_to_string(&path).unwrap(), bad);
        }
    }

    #[test]
    fn edits_made_on_disk_since_loading_survive_a_save() {
        let path = commented("merge1");
        // The user edits another key by hand after we loaded.
        let edited = COMMENTED.replace("tab_size = 4", "tab_size = 2");
        fs::write(&path, edited).unwrap();
        // A save from the copy loaded earlier would revert tab_size; update
        // works on what is on disk now.
        Config::update(&path, |c| c.servers[0].queries.push(q("D", "is:merged"))).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("tab_size = 2 # four"), "{text}");
        assert_eq!(
            Config::load(&path).unwrap().unwrap().servers[0]
                .queries
                .len(),
            4
        );
    }

    #[test]
    fn a_missing_file_is_created() {
        let dir = tmp("create1");
        let path = dir.join("sub").join("config.toml");
        Config::update(&path, |c| {
            *c = std::mem::take(c).after_login("g", "https://g", None)
        })
        .unwrap();
        assert_eq!(Config::load(&path).unwrap().unwrap().servers.len(), 1);
    }

    #[test]
    fn startup_and_review_default_and_round_trip() {
        let cfg = Config::default();
        assert_eq!(cfg.startup_page(), StartupPage::Dashboard);
        assert!(cfg.advance_after_vote());

        let parsed: Config = toml::from_str(
            "[startup]\nquery = \"status:open project:foo\"\n[review]\nadvance_after_vote = false\n",
        )
        .unwrap();
        assert_eq!(
            parsed.startup_page(),
            StartupPage::Query("status:open project:foo".into())
        );
        assert!(!parsed.advance_after_vote());

        assert_eq!(cfg.theme(), ThemeName::Cwc);
        let themed: Config = toml::from_str("[appearance]\ntheme = \"Solarized\"\n").unwrap();
        assert_eq!(themed.theme(), ThemeName::Solarized);
        let unknown: Config = toml::from_str("[appearance]\ntheme = \"zenburn\"\n").unwrap();
        assert_eq!(unknown.theme(), ThemeName::Cwc);

        assert_eq!(cfg.color_modes(), ColorModes::default());
        let moded: Config =
            toml::from_str("[appearance]\nmode = \"Dark\"\ndiff_mode = \"light\"\n").unwrap();
        assert_eq!(
            moded.color_modes(),
            ColorModes {
                ui: ColorMode::Dark,
                diff: ColorMode::Light
            }
        );
        let odd_mode: Config = toml::from_str("[appearance]\nmode = \"dim\"\n").unwrap();
        assert_eq!(odd_mode.color_modes(), ColorModes::default());

        let odd: Config = toml::from_str("[startup]\npage = \"inbox\"\n").unwrap();
        assert_eq!(odd.startup_page(), StartupPage::Dashboard);
        let blank: Config = toml::from_str("[startup]\nquery = \"  \"\n").unwrap();
        assert_eq!(blank.startup_page(), StartupPage::Dashboard);

        let dir = tmp("startup1");
        let path = dir.join("config.toml");
        Config::update(&path, |c| {
            c.startup = Some(StartupConfig::for_page(&StartupPage::Query("a b".into())));
            c.review = Some(ReviewConfig {
                advance_after_vote: Some(false),
            });
        })
        .unwrap();
        let back = Config::load(&path).unwrap().unwrap();
        assert_eq!(back.startup_page(), StartupPage::Query("a b".into()));
        assert!(!back.advance_after_vote());
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("[startup]") && text.contains("[review]"),
            "{text}"
        );
    }

    #[test]
    fn parse_error_names_the_file() {
        let dir = tmp("bad");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, "servers = 3").unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("config.toml"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    const FILTERED: &str = r##"# Top of the file.
[[servers]]
id = "g"
name = "G"
url = "https://gerrit.example.com"

# queries
[[servers.queries]]
name = "A"
query = "status:open" # inline a

# generated code
[[filters]]
name = "Generated"
globs = ["*.pb.go", "gen/**"] # inline globs
projects = ["platform/foo"]
action = "hide"

# lock files
[[filters]]
name = "Locks"
globs = ["*.lock"]
action = "dim"
enabled = true

# mine
[[filters]]
name = "Mine"
globs = ["src/mine/**"]
action = "highlight"
color = "#e5a50a"
bold = true
enabled = false

[review]
# advance after a vote
advance_after_vote = false
"##;

    fn filtered(name: &str) -> PathBuf {
        let dir = tmp(name);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, FILTERED).unwrap();
        path
    }

    fn filter_lines_kept(text: &str, gone: &[&str]) {
        for line in FILTERED.lines().filter(|l| !l.is_empty()) {
            if !gone.iter().any(|g| line.contains(g)) {
                assert!(text.contains(line), "lost {line:?} in\n{text}");
            }
        }
    }

    fn parse(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn a_full_and_a_minimal_filter_parse() {
        let cfg = parse(FILTERED);
        let rules = cfg.file_filters();
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].name, "Generated");
        assert_eq!(rules[0].globs, ["*.pb.go", "gen/**"]);
        assert_eq!(rules[0].projects, ["platform/foo"]);
        assert_eq!(rules[0].action, FilterAction::Hide);
        assert!(rules[0].enabled);
        assert_eq!(rules[1].action, FilterAction::Dim);
        assert!(rules[1].projects.is_empty());
        assert_eq!(
            rules[2].action,
            FilterAction::Highlight {
                color: parse_hex_color("#e5a50a"),
                bold: true
            }
        );
        assert!(!rules[2].enabled);

        let cfg = parse("[[filters]]\nname = \"x\"\nglobs = [\"a\"]\naction = \"dim\"\n");
        let rules = cfg.file_filters();
        assert_eq!(rules.len(), 1);
        assert!(rules[0].enabled && rules[0].projects.is_empty());
    }

    #[test]
    fn a_file_without_filters_has_none() {
        assert!(
            parse("[server]\nurl = \"https://x\"\n")
                .file_filters()
                .is_empty()
        );
        assert!(parse("").filters.is_empty());
    }

    #[test]
    fn bad_values_drop_only_what_is_bad() {
        let cfg = parse(
            r##"
[[filters]]
name = "weird"
globs = ["a"]
action = "explode"

[[filters]]
name = "color"
globs = ["[", "*.rs"]
action = "highlight"
color = "red"
bold = true

[[filters]]
globs = ["x"]
action = "hide"

[[filters]]
name = "color"
globs = ["y"]
action = "hide"
"##,
        );
        let rules = cfg.file_filters();
        assert_eq!(rules.len(), 1, "{rules:?}");
        assert_eq!(rules[0].name, "color");
        assert_eq!(rules[0].globs, ["*.rs"]);
        assert_eq!(
            rules[0].action,
            FilterAction::Highlight {
                color: None,
                bold: true
            }
        );
    }

    #[test]
    fn filters_convert_back_without_default_keys() {
        let rules = parse(FILTERED).file_filters();
        let back: Vec<FilterConfig> = rules.iter().map(FilterConfig::from_filter).collect();
        assert_eq!(back[1].enabled, None);
        assert_eq!(back[0].bold, None);
        assert_eq!(back[0].color, None);
        assert_eq!(back[2].enabled, Some(false));
        assert_eq!(back[2].color.as_deref(), Some("#e5a50a"));
        let path = tmp("defaults").join("config.toml");
        Config::update(&path, |c| c.filters = back).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("enabled = true"), "{text}");
        assert!(!text.contains("bold = false"), "{text}");
        assert!(!text.contains("projects = []"), "{text}");
        let rules2 = Config::load(&path).unwrap().unwrap().file_filters();
        assert_eq!(rules, rules2);
    }

    #[test]
    fn editing_a_filter_keeps_every_comment_and_the_queries() {
        let path = filtered("fedit");
        Config::update(&path, |c| c.filters[1].globs.push("go.sum".into())).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        filter_lines_kept(&text, &["*.lock"]);
        assert!(text.contains("globs = [\"*.lock\", \"go.sum\"]"), "{text}");
        assert!(text.contains("# lock files"), "{text}");
        let cfg = Config::load(&path).unwrap().unwrap();
        assert_eq!(cfg.servers[0].queries, vec![q("A", "status:open")]);
        assert_eq!(cfg.file_filters()[1].globs, ["*.lock", "go.sum"]);
    }

    #[test]
    fn adding_a_filter_keeps_every_comment() {
        let path = filtered("fadd");
        let new = FileFilter {
            name: "Docs".into(),
            globs: vec!["docs/**".into()],
            projects: Vec::new(),
            servers: Vec::new(),
            action: FilterAction::Hide,
            enabled: true,
        };
        Config::update(&path, |c| c.filters.push(FilterConfig::from_filter(&new))).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        filter_lines_kept(&text, &[]);
        let rules = Config::load(&path).unwrap().unwrap().file_filters();
        assert_eq!(rules.len(), 4);
        assert_eq!(rules[3], new);
    }

    #[test]
    fn deleting_a_filter_drops_only_its_own_comment() {
        let path = filtered("fdel");
        Config::update(&path, |c| {
            c.filters.remove(1);
        })
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        filter_lines_kept(
            &text,
            &[
                "# lock files",
                "Locks",
                "*.lock",
                "\"dim\"",
                "enabled = true",
            ],
        );
        let cfg = Config::load(&path).unwrap().unwrap();
        let names: Vec<_> = cfg.file_filters().into_iter().map(|f| f.name).collect();
        assert_eq!(names, ["Generated", "Mine"]);
        assert_eq!(cfg.servers[0].queries.len(), 1);
    }

    #[test]
    fn reordering_filters_moves_their_comments_along() {
        let path = filtered("freorder");
        Config::update(&path, |c| c.filters.swap(0, 2)).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        filter_lines_kept(&text, &[]);
        assert!(text.find("# mine").unwrap() < text.find("# generated code").unwrap());
        // The explicit `enabled = true` of "Locks" survives the move.
        assert!(text.contains("enabled = true"), "{text}");
        let cfg = Config::load(&path).unwrap().unwrap();
        let names: Vec<_> = cfg.file_filters().into_iter().map(|f| f.name).collect();
        assert_eq!(names, ["Mine", "Locks", "Generated"]);
        assert_eq!(cfg.servers[0].queries.len(), 1);
    }

    #[test]
    fn deleting_every_filter_removes_the_array() {
        let path = filtered("fall");
        Config::update(&path, |c| c.filters.clear()).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("filters"), "{text}");
        assert!(text.contains("[[servers.queries]]"));
    }

    const LEGACY: &str = r##"# My config.

# The server.
[server]
url = "https://gerrit.example.com:8443/r" # prod
username = "jdoe"

# first query
[[queries]]
name = "A"
query = "status:open" # inline a

# bookmark
[[bookmarks]]
name = "Mine"
query = "owner:self"

# generated
[[filters]]
name = "Generated"
globs = ["*.pb.go"]
action = "hide"

# look
[appearance]
theme = "solarized" # nice
"##;

    fn legacy(name: &str) -> PathBuf {
        let dir = tmp(name);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, LEGACY).unwrap();
        path
    }

    fn check_migrated(path: &Path) {
        let text = fs::read_to_string(path).unwrap();
        for kept in [
            "# My config.",
            "# The server.",
            "url = \"https://gerrit.example.com:8443/r\" # prod",
            "# first query",
            "query = \"status:open\" # inline a",
            "# bookmark",
            "# generated",
            "# look",
            "theme = \"solarized\" # nice",
        ] {
            assert!(text.contains(kept), "lost {kept:?} in\n{text}");
        }
        assert!(
            !text.contains("[server]") && !text.contains("\n[[queries]]"),
            "{text}"
        );
        assert!(!text.contains("\n[[bookmarks]]"), "{text}");
        let cfg = Config::load(path).unwrap().unwrap();
        assert_eq!(cfg.servers.len(), 1);
        let s = &cfg.servers[0];
        assert_eq!(s.id, "gerrit-example-com-8443");
        assert_eq!(s.name, "gerrit.example.com:8443");
        assert_eq!(s.url, "https://gerrit.example.com:8443/r");
        assert_eq!(s.username.as_deref(), Some("jdoe"));
        assert_eq!(s.queries, vec![q("A", "status:open")]);
        assert_eq!(s.bookmarks, vec![q("Mine", "owner:self")]);
        assert_eq!(cfg.filters.len(), 1);
        assert_eq!(cfg.theme(), ThemeName::Solarized);
    }

    #[test]
    fn load_migrates_the_old_layout_in_place_keeping_comments() {
        let path = legacy("migrate-load");
        let cfg = Config::load(&path).unwrap().unwrap();
        assert_eq!(cfg.servers[0].name, "gerrit.example.com:8443");
        check_migrated(&path);
        // Rewritten in place: no backup file next to it.
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("config.toml")]);
        // A second load leaves the migrated file as it is.
        let once = fs::read_to_string(&path).unwrap();
        Config::load(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), once);
    }

    #[test]
    fn update_migrates_the_old_layout_even_without_a_change() {
        let path = legacy("migrate-update");
        Config::update(&path, |_| {}).unwrap();
        check_migrated(&path);
    }

    #[test]
    fn update_migrates_then_applies_the_change() {
        let path = legacy("migrate-change");
        Config::update(&path, |c| {
            c.servers[0].queries.push(q("B", "is:wip"));
            c.servers[0].name = "Prod".into();
        })
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# first query") && text.contains("# The server."),
            "{text}"
        );
        let cfg = Config::load(&path).unwrap().unwrap();
        assert_eq!(cfg.servers[0].name, "Prod");
        // Renaming does not change the id.
        assert_eq!(cfg.servers[0].id, "gerrit-example-com-8443");
        assert_eq!(cfg.servers[0].queries.len(), 2);
    }

    #[test]
    fn an_old_file_without_a_server_is_left_alone() {
        let dir = tmp("migrate-none");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let text = "[[queries]]\nname = \"A\"\nquery = \"x\"\n\n[startup]\npage = \"dashboard\"\n";
        fs::write(&path, text).unwrap();
        let cfg = Config::load(&path).unwrap().unwrap();
        assert!(cfg.servers.is_empty());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }

    const TWO: &str = r##"# two servers
[[servers]]
id = "work"
name = "Work" # the big one
url = "https://gerrit.work.example"
username = "jdoe"

# work queries
[[servers.queries]]
name = "Mine"
query = "owner:self"

[[servers]]
id = "oss"
name = "OSS"
url = "https://gerrit.oss.example"
color = "#ff8800"

[[servers.queries]]
name = "Open"
query = "status:open"

[[servers.bookmarks]]
name = "Recent"
query = "-age:1d"
"##;

    fn two(name: &str) -> PathBuf {
        let dir = tmp(name);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, TWO).unwrap();
        path
    }

    #[test]
    fn two_servers_keep_their_own_lists() {
        let path = two("two-parse");
        let cfg = Config::load(&path).unwrap().unwrap();
        assert_eq!(cfg.servers.len(), 2);
        assert_eq!(
            cfg.server("work").unwrap().queries,
            vec![q("Mine", "owner:self")]
        );
        assert_eq!(cfg.server("work").unwrap().bookmarks, vec![]);
        let oss = cfg.server("oss").unwrap();
        assert_eq!(oss.username, None);
        assert_eq!(oss.color.as_deref(), Some("#ff8800"));
        assert_eq!(oss.queries, vec![q("Open", "status:open")]);
        assert_eq!(oss.bookmarks, vec![q("Recent", "-age:1d")]);
        // Nothing to migrate or change: the file is untouched.
        assert_eq!(fs::read_to_string(&path).unwrap(), TWO);
    }

    #[test]
    fn editing_one_server_leaves_the_other_and_the_comments_alone() {
        let path = two("two-edit");
        Config::update(&path, |c| {
            c.servers[1].queries.push(q("Merged", "status:merged"));
            c.servers[1].name = "Open source".into();
            c.servers[1].username = Some("alice".into());
        })
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        for kept in [
            "# two servers",
            "# the big one",
            "# work queries",
            "id = \"oss\"",
        ] {
            assert!(text.contains(kept), "lost {kept:?} in\n{text}");
        }
        assert!(text.contains("name = \"Open source\""), "{text}");
        let cfg = Config::load(&path).unwrap().unwrap();
        assert_eq!(
            cfg.servers[0],
            Config::load(&path).unwrap().unwrap().servers[0]
        );
        assert_eq!(cfg.servers[0].queries, vec![q("Mine", "owner:self")]);
        assert_eq!(cfg.servers[1].queries.len(), 2);
        assert_eq!(cfg.servers[1].username.as_deref(), Some("alice"));
        assert_eq!(cfg.servers[1].bookmarks.len(), 1);
    }

    #[test]
    fn adding_removing_and_reordering_servers_keeps_the_other_comments() {
        let path = two("two-shape");
        Config::update(&path, |c| {
            *c = std::mem::take(c).after_login("third", "https://third.example", None);
        })
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# two servers") && text.contains("# work queries"),
            "{text}"
        );
        let cfg = Config::load(&path).unwrap().unwrap();
        let ids: Vec<_> = cfg.servers.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["work", "oss", "third"]);
        assert_eq!(cfg.servers[2].name, "third.example");
        assert_eq!(cfg.servers[2].queries, default_queries(true));

        Config::update(&path, |c| c.servers.swap(0, 1)).unwrap();
        let cfg = Config::load(&path).unwrap().unwrap();
        let ids: Vec<_> = cfg.servers.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["oss", "work", "third"]);
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# work queries") && text.contains("# the big one"),
            "{text}"
        );

        Config::update(&path, |c| {
            c.servers.remove(0);
        })
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("oss") && !text.contains("-age:1d"), "{text}");
        assert!(text.contains("# work queries"), "{text}");
        let cfg = Config::load(&path).unwrap().unwrap();
        let ids: Vec<_> = cfg.servers.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["work", "third"]);
    }

    #[test]
    fn duplicate_and_invalid_server_ids_are_clear_errors() {
        let dir = tmp("slug-errors");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let server =
            |id: &str| format!("[[servers]]\nid = \"{id}\"\nname = \"N\"\nurl = \"https://g\"\n\n");

        fs::write(&path, server("a") + &server("a")).unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(
            matches!(err, ConfigError::DuplicateServerId { .. }),
            "{err}"
        );
        assert!(
            err.to_string().contains("\"a\"") && err.to_string().contains("config.toml"),
            "{err}"
        );

        for bad in ["", "Has Space", "UPPER", "a/b"] {
            fs::write(&path, server(bad)).unwrap();
            let err = Config::load(&path).unwrap_err();
            assert!(
                matches!(err, ConfigError::InvalidServerId { .. }),
                "{bad:?}: {err}"
            );
        }

        // An update refuses a file like that, and a change that makes one.
        let before = server("a") + &server("a");
        fs::write(&path, &before).unwrap();
        assert!(Config::update(&path, |_| {}).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        fs::write(&path, server("a")).unwrap();
        let err = Config::update(&path, |c| c.servers[0].id = "Bad".into()).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidServerId { .. }), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), server("a"));
    }

    #[test]
    fn slugs_are_derived_from_names_and_made_unique() {
        assert_eq!(slugify("Gerrit Code Review"), "gerrit-code-review");
        assert_eq!(
            slugify("gerrit.example.com:8080"),
            "gerrit-example-com-8080"
        );
        assert_eq!(slugify("  --Ünï--  "), "n");
        assert_eq!(slugify("!!!"), "server");
        assert!(is_slug("a-b_1") && !is_slug("") && !is_slug("A") && !is_slug("a b"));

        assert_eq!(
            server_name_from_url("https://gerrit.example.com/r/"),
            "gerrit.example.com"
        );
        assert_eq!(
            server_name_from_url("http://127.0.0.1:8080"),
            "127.0.0.1:8080"
        );
        assert_eq!(
            server_name_from_url("https://me@host.example/x"),
            "host.example"
        );

        let mut cfg = Config::default();
        for expected in ["dev", "dev-2", "dev-3"] {
            let id = cfg.unique_server_id("Dev");
            assert_eq!(id, expected);
            cfg = cfg.after_login(&id, "http://127.0.0.1:8080", None);
        }
        // The id is stored: renaming leaves it alone.
        cfg.servers[0].name = "Something else".into();
        assert_eq!(cfg.servers[0].id, "dev");
        assert_eq!(cfg.unique_server_id("Something else"), "something-else");
    }

    #[test]
    fn a_filters_server_restriction_is_parsed_and_preserved() {
        let dir = tmp("filter-servers");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let text =
            "[[filters]]\nname = \"G\"\nglobs = [\"a\"]\nservers = [\"work\"]\naction = \"hide\"\n";
        fs::write(&path, text).unwrap();
        let cfg = Config::load(&path).unwrap().unwrap();
        assert_eq!(cfg.filters[0].servers, vec!["work".to_owned()]);
        assert_eq!(cfg.file_filters().len(), 1);
        Config::update(&path, |c| c.filters[0].action = "dim".into()).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("servers = [\"work\"]") && after.contains("\"dim\""),
            "{after}"
        );
    }
}
