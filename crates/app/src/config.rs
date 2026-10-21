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

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use model::{DiffContext, DiffPreferences, DiffViewMode, Whitespace};
use serde::{Deserialize, Serialize};

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
        }
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

    /// Writes the file, creating parent directories. The write goes to a
    /// temporary file first so a crash cannot leave a truncated config.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let text = toml::to_string_pretty(self)?;
        let werr = |source| ConfigError::Write {
            path: path.to_owned(),
            source,
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(werr)?;
        }
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text).map_err(werr)?;
        fs::rename(&tmp, path).map_err(werr)
    }
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
