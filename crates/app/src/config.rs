//! The TOML configuration file at `<config_dir>/<APP_NAME>/config.toml`.
//!
//! ```toml
//! [server]
//! url = "https://gerrit.example.com"
//! username = "jdoe"          # absent: anonymous, read-only access
//!
//! [[queries]]
//! name = "Your turn"
//! query = "attention:self"
//!
//! # Optional: what opens on start. Default: the dashboard.
//! [startup]
//! page = "dashboard"                   # or instead: query = "status:open project:foo"
//!
//! # Optional: review flow.
//! [review]
//! advance_after_vote = true            # go to the next open change after a vote
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
    #[error("no home directory found")]
    NoHome,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub server: Option<ServerConfig>,
    #[serde(default)]
    pub queries: Vec<QueryConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup: Option<StartupConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewConfig>,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    pub url: String,
    /// `None` means anonymous access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryConfig {
    pub name: String,
    pub query: String,
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
    /// The configuration after a successful login: the server is replaced,
    /// existing saved queries and the `[diff]` section are kept, and an
    /// empty query list is seeded.
    pub fn after_login(self, url: &str, username: Option<&str>) -> Config {
        let queries = if self.queries.is_empty() {
            default_queries(username.is_none())
        } else {
            self.queries
        };
        Config {
            server: Some(ServerConfig {
                url: url.to_owned(),
                username: username.map(str::to_owned),
            }),
            queries,
            diff: self.diff,
            startup: self.startup,
            review: self.review,
        }
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

    /// Reads the file; `Ok(None)` if it does not exist.
    pub fn load(path: &Path) -> Result<Option<Config>, ConfigError> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        toml::from_str(&text)
            .map(Some)
            .map_err(|source| ConfigError::Parse {
                path: path.to_owned(),
                source,
            })
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
    /// created. Returns the configuration as written.
    pub fn update(path: &Path, change: impl FnOnce(&mut Config)) -> Result<Config, ConfigError> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let old: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })?;
        let mut doc: DocumentMut =
            text.parse()
                .map_err(|e: toml_edit::TomlError| ConfigError::Edit {
                    path: path.to_owned(),
                    message: e.to_string(),
                })?;
        let mut new = old.clone();
        change(&mut new);
        if new != old {
            apply_changes(&old, &new, &mut doc)?;
            write_atomic(path, &doc.to_string())?;
        }
        Ok(new)
    }
}

/// The scalar and table keys of `new` that differ from `old` (everything
/// but the query list) go into `doc`.
fn apply_changes(old: &Config, new: &Config, doc: &mut DocumentMut) -> Result<(), ConfigError> {
    let tables = |c: &Config| -> Result<toml::Table, ConfigError> {
        let mut t = toml::Table::try_from(c)?;
        t.remove("queries");
        Ok(t)
    };
    apply_table(&tables(old)?, &tables(new)?, doc.as_table_mut());
    if old.queries != new.queries {
        apply_queries(&new.queries, doc.as_table_mut());
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

fn set_str(t: &mut toml_edit::Table, key: &str, text: &str) {
    let mut v: toml_edit::Value = text.into();
    if let Some(Item::Value(prev)) = t.get(key) {
        *v.decor_mut() = prev.decor().clone();
    }
    t.insert(key, Item::Value(v));
}

/// Rewrites the `[[queries]]` tables. A table of the file that equals a new
/// entry (name and query) is reused as is, with its comments, wherever the
/// entry moved to. A new entry that matches nothing takes over the unused
/// table at its own position when there is one (an edit in place: only
/// `name` and `query` are replaced, comments and other keys stay), else a
/// fresh table is made. The limit: comments belong to the table below them,
/// so deleting an entry deletes the comment lines directly above it, and a
/// section comment above the first `[[queries]]` moves with that table.
/// Tables are matched by content, so hand edits of the list on disk are
/// respected.
fn apply_queries(new: &[QueryConfig], root: &mut toml_edit::Table) {
    let mut existing: Vec<Option<toml_edit::Table>> = match root.get("queries") {
        Some(Item::ArrayOfTables(a)) => a.iter().cloned().map(Some).collect(),
        _ => Vec::new(),
    };
    let existing_keys: Vec<Option<(String, String)>> = existing
        .iter()
        .map(|t| t.as_ref().and_then(query_of))
        .collect();
    let mut chosen: Vec<Option<toml_edit::Table>> = vec![None; new.len()];
    for (i, q) in new.iter().enumerate() {
        let key = Some((q.name.clone(), q.query.clone()));
        if let Some(j) =
            (0..existing.len()).find(|&j| existing[j].is_some() && existing_keys[j] == key)
        {
            chosen[i] = existing[j].take();
        }
    }
    let mut out = toml_edit::ArrayOfTables::new();
    for (i, q) in new.iter().enumerate() {
        let table = match chosen[i].take() {
            Some(t) => t,
            None => {
                let mut t = existing
                    .get_mut(i)
                    .and_then(Option::take)
                    .unwrap_or_default();
                set_str(&mut t, "name", &q.name);
                set_str(&mut t, "query", &q.query);
                t
            }
        };
        // Tables print sorted by their parsed position; clear it so the new
        // order (the order of the array) wins.
        let mut table = table;
        table.set_position(None);
        out.push(table);
    }
    if out.is_empty() {
        root.remove("queries");
    } else {
        root.insert("queries", Item::ArrayOfTables(out));
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
        let cfg = Config::default().after_login("https://g.example.com", Some("jdoe"));
        cfg.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), Some(cfg));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_documented_shape() {
        let cfg: Config = toml::from_str(
            r#"
            [server]
            url = "https://gerrit.example.com"
            username = "jdoe"
            [[queries]]
            name = "Your turn"
            query = "attention:self"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.server.unwrap().username.as_deref(), Some("jdoe"));
        assert_eq!(cfg.queries, vec![q("Your turn", "attention:self")]);
    }

    #[test]
    fn anonymous_has_no_username_key_and_no_password_anywhere() {
        let cfg = Config::default().after_login("https://g.example.com", None);
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert!(!text.contains("username"));
        assert!(!text.contains("password"));
        assert_eq!(cfg.queries, default_queries(true));
    }

    #[test]
    fn login_seeds_defaults_once() {
        let cfg = Config::default().after_login("https://g.example.com", Some("jdoe"));
        assert_eq!(cfg.queries.len(), 6);
        assert_eq!(cfg.queries[0], q("Your turn", "attention:self"));
        // A user's edited list survives a re-login.
        let mut edited = cfg.clone();
        edited.queries = vec![q("Mine", "owner:self")];
        let again = edited.after_login("https://other.example.com", Some("x"));
        assert_eq!(again.queries, vec![q("Mine", "owner:self")]);
        assert_eq!(again.server.unwrap().url, "https://other.example.com");
    }

    #[test]
    fn diff_section_is_optional() {
        let cfg: Config = toml::from_str("[server]\nurl = \"https://g\"\n").unwrap();
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
        .after_login("https://g.example.com", Some("jdoe"));
        cfg.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[diff]"), "{text}");
        assert!(!text.contains("tab_size"), "{text}");
        assert_eq!(Config::load(&path).unwrap(), Some(cfg));
        let _ = fs::remove_dir_all(&dir);
    }

    const COMMENTED: &str = r#"# My config, edited by hand.

# The server.
[server]
url = "https://g.example.com" # prod
username = "jdoe"

# --- queries ---
# first
[[queries]]
name = "A"
query = "status:open" # inline a

# second
[[queries]]
name = "B"
query = "owner:self"

# third
[[queries]]
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
        Config::update(&path, |c| c.queries[1].query = "owner:self is:open".into()).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        lines_kept(&text, &["owner:self"]);
        assert!(text.contains("# second"), "{text}");
        assert!(text.contains("query = \"owner:self is:open\""), "{text}");
        let cfg = Config::load(&path).unwrap().unwrap();
        assert_eq!(cfg.queries[1], q("B", "owner:self is:open"));
        assert_eq!(cfg.queries.len(), 3);
    }

    #[test]
    fn adding_a_query_keeps_every_comment() {
        let path = commented("add1");
        Config::update(&path, |c| c.queries.push(q("D", "is:merged"))).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        lines_kept(&text, &[]);
        assert_eq!(Config::load(&path).unwrap().unwrap().queries.len(), 4);
    }

    #[test]
    fn deleting_a_query_drops_only_its_own_comment() {
        let path = commented("del1");
        Config::update(&path, |c| {
            c.queries.remove(1);
        })
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        lines_kept(&text, &["# second", "\"B\"", "owner:self"]);
        let cfg = Config::load(&path).unwrap().unwrap();
        assert_eq!(cfg.queries, vec![q("A", "status:open"), q("C", "is:wip")]);
    }

    #[test]
    fn reordering_queries_moves_their_comments_along() {
        let path = commented("reorder1");
        Config::update(&path, |c| c.queries.swap(0, 2)).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        lines_kept(&text, &[]);
        let third = text.find("# third").unwrap();
        let first = text.find("# first").unwrap();
        assert!(third < first, "{text}");
        let cfg = Config::load(&path).unwrap().unwrap();
        assert_eq!(cfg.queries[0].name, "C");
        assert_eq!(cfg.queries[2].name, "A");
    }

    #[test]
    fn changing_a_scalar_keeps_its_inline_comment() {
        let path = commented("scalar1");
        Config::update(&path, |c| {
            c.diff.get_or_insert_with(Default::default).tab_size = Some(8);
            c.server.as_mut().unwrap().url = "https://h.example.com".into();
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
            *c = std::mem::take(c).after_login("https://new.example.com", Some("x"));
        })
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        lines_kept(&text, &["url =", "username"]);
        let cfg = Config::load(&path).unwrap().unwrap();
        assert_eq!(cfg.server.unwrap().url, "https://new.example.com");
        assert_eq!(cfg.queries.len(), 3);
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
        for bad in ["[server\nurl = ", "server = 3\n"] {
            fs::write(&path, bad).unwrap();
            let err = Config::update(&path, |c| c.queries.push(q("x", "y"))).unwrap_err();
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
        Config::update(&path, |c| c.queries.push(q("D", "is:merged"))).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("tab_size = 2 # four"), "{text}");
        assert_eq!(Config::load(&path).unwrap().unwrap().queries.len(), 4);
    }

    #[test]
    fn a_missing_file_is_created() {
        let dir = tmp("create1");
        let path = dir.join("sub").join("config.toml");
        Config::update(&path, |c| c.queries.push(q("a", "b"))).unwrap();
        assert_eq!(
            Config::load(&path).unwrap().unwrap().queries,
            vec![q("a", "b")]
        );
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
        fs::write(&path, "server = 3").unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("config.toml"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }
}
