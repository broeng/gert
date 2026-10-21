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
//! ```
//!
//! The HTTP password is never stored here; see [`crate::secrets`].

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
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
    /// existing saved queries are kept, and an empty list is seeded.
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
