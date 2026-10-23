//! What the pages share: the cache, the server and the network client.

use std::sync::{Arc, Mutex};

use gerrit_api::{Auth, GerritClient, HttpClient};
use store::{ServerId, Store};
use sync::Syncer;

use crate::{
    filters::FileFilter,
    theme::{ColorModes, ThemeName},
};

/// A named Gerrit query: a dashboard section, or a bookmark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedQuery {
    pub name: String,
    pub query: String,
}

/// What opens on start.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum StartupPage {
    #[default]
    Dashboard,
    /// A search with this (non-blank) query.
    Search(String),
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{0}")]
pub struct BackendError(pub String);

/// The stable id of a configured server: the slug in the config file, also
/// what names the server to its [`ServerBackend`]. It does not change when
/// the server is renamed or its URL is edited.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ServerKey(String);

impl ServerKey {
    pub fn new(slug: impl Into<String>) -> Self {
        Self(slug.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ServerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A display name for a server URL: its host (with the port, if any).
pub fn server_name_from_url(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    if host.is_empty() {
        "server".to_owned()
    } else {
        host.to_owned()
    }
}

/// What the config file says about one server, without its saved lists:
/// the unit of [`Backend::save_servers`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerConfig {
    pub key: ServerKey,
    pub name: String,
    pub url: String,
    /// `None`: anonymous.
    pub username: Option<String>,
    /// `"#rrggbb"`; `None`: assigned automatically.
    pub color: Option<String>,
}

/// The persistence of one server, bound to it: the keyring entry and the
/// server's queries and bookmarks in the config file. Everything blocks, so
/// the UI calls it from the background executor.
pub trait ServerBackend: Send + Sync + 'static {
    /// The saved HTTP password; `Ok(None)` if there is none.
    fn load_password(&self, base_url: &str, username: &str)
    -> Result<Option<String>, BackendError>;

    /// Stores the password in the keyring (when there is one) and writes the
    /// server to the config (adding it if it is new). Returns the saved
    /// queries now in effect (seeded for a new server).
    fn save_login(
        &self,
        base_url: &str,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<Vec<SavedQuery>, BackendError>;

    /// Writes the saved queries of this server to the config file, keeping
    /// the rest of it as it is. Returns the list now in effect.
    fn save_queries(&self, queries: &[SavedQuery]) -> Result<Vec<SavedQuery>, BackendError>;

    /// Writes the bookmarks of this server to the config file, keeping the
    /// rest of it as it is. Returns the list now in effect. Defaults to
    /// accepting them without saving, for backends with no config file.
    fn save_bookmarks(&self, bookmarks: &[SavedQuery]) -> Result<Vec<SavedQuery>, BackendError> {
        Ok(bookmarks.to_vec())
    }
}

/// The global persistence the `app` crate provides: the settings that are
/// not about one server, and the way to a server's own [`ServerBackend`].
/// It blocks, so the UI calls it from the background executor.
pub trait Backend: Send + Sync + 'static {
    /// The backend of server `key`. Takes the `Arc` so that a backend that
    /// keeps its state in itself (a test fake) can hand itself out.
    fn for_server(self: Arc<Self>, key: &ServerKey) -> Arc<dyn ServerBackend>;

    /// A key for a server to be added at `base_url`, one no configured
    /// server has. Defaults to a fixed key, for backends with no config.
    fn new_server_key(&self, _base_url: &str) -> ServerKey {
        ServerKey::new("default")
    }

    /// Writes the list of servers (`[[servers]]`): add, edit, reorder and
    /// remove. A server that stays keeps its queries and bookmarks. Returns
    /// the list now in effect. Defaults to accepting it without saving, for
    /// backends with no config file.
    fn save_servers(&self, servers: &[ServerConfig]) -> Result<Vec<ServerConfig>, BackendError> {
        Ok(servers.to_vec())
    }

    /// Removes server `removed` for good: its cached rows, its keyring
    /// secret and its `[[servers]]` entry. `remaining` is the list of the
    /// servers that stay; it is written as by `save_servers`. Nothing is
    /// asked and nothing is kept: a server added again starts from scratch.
    /// Defaults to just writing the list, for backends with no cache or
    /// keyring.
    fn remove_server(
        &self,
        _removed: &ServerConfig,
        remaining: &[ServerConfig],
    ) -> Result<Vec<ServerConfig>, BackendError> {
        self.save_servers(remaining)
    }

    /// Drops the keyring secret of the login `username` at `base_url`,
    /// which a server has left behind by changing its URL or user. A secret
    /// another configured server still uses is kept. Defaults to doing
    /// nothing.
    fn forget_login(&self, _base_url: &str, _username: &str) -> Result<(), BackendError> {
        Ok(())
    }

    /// Writes the file filters (`[[filters]]`) to the config file, keeping
    /// the rest of it as it is. Returns the list now in effect.
    fn save_filters(&self, filters: &[FileFilter]) -> Result<Vec<FileFilter>, BackendError>;

    /// Writes the startup page (`[startup]`). Returns what is now in effect.
    fn save_startup_page(&self, page: &StartupPage) -> Result<StartupPage, BackendError>;

    /// Writes `[review] advance_after_vote`. Returns the value now in effect.
    fn save_advance_after_vote(&self, on: bool) -> Result<bool, BackendError>;

    /// Writes `[appearance] theme`. Returns the theme now in effect.
    fn save_theme(&self, theme: ThemeName) -> Result<ThemeName, BackendError>;

    /// Writes `[appearance] mode` and `diff_mode`. Returns the modes now in
    /// effect. Defaults to accepting them without saving, for backends with
    /// no config file.
    fn save_color_modes(&self, modes: ColorModes) -> Result<ColorModes, BackendError> {
        Ok(modes)
    }
}

/// The pieces every page needs, handed over by `app`.
#[derive(Clone)]
pub struct Env {
    pub http: Arc<dyn HttpClient>,
    pub store: Arc<Mutex<Store>>,
    /// The global backend; a server's own is `Session::backend`.
    pub backend: Arc<dyn Backend>,
}

impl Env {
    /// The backend of server `key`.
    pub fn server_backend(&self, key: &ServerKey) -> Arc<dyn ServerBackend> {
        self.backend.clone().for_server(key)
    }
}

/// A configured server, possibly seen before.
#[derive(Clone, Debug)]
pub struct ServerSetup {
    pub key: ServerKey,
    pub name: String,
    pub base_url: String,
    pub username: Option<String>,
    /// `"#rrggbb"` if the user chose one.
    pub color: Option<String>,
    /// The cache's id of the server.
    pub server: ServerId,
    /// The saved queries (`[[servers.queries]]`): the dashboard sections.
    pub queries: Vec<SavedQuery>,
    /// The bookmarks (`[[servers.bookmarks]]`): searches the dashboard has
    /// buttons for.
    pub bookmarks: Vec<SavedQuery>,
}

/// The user's local diff settings from the config file.
#[derive(Clone, Debug, Default)]
pub struct LocalDiff {
    /// Overrides of the server's diff preferences (unset fields fall
    /// through to the server, then to the built-in defaults).
    pub preferences: model::DiffPreferences,
    /// Replaces the theme's monospace font in diffs.
    pub font_family: Option<String>,
}

/// Everything `app` gives the UI at startup.
pub struct Startup {
    pub env: Env,
    /// The configured servers, in config order. Empty when there is no
    /// config yet: start on the login page. The UI works with one for now.
    pub servers: Vec<ServerSetup>,
    /// The server to start on; the first one if `None` or unknown.
    pub active: Option<ServerKey>,
    /// The file filter rules (`[[filters]]`).
    pub filters: Vec<FileFilter>,
    pub diff: LocalDiff,
    /// The page to open once a server is set up.
    pub start_page: StartupPage,
}

/// A logged-in (or anonymous) connection to one server.
#[derive(Clone)]
pub struct Session {
    pub key: ServerKey,
    pub base_url: String,
    pub username: Option<String>,
    backend: Arc<dyn ServerBackend>,
    syncer: Syncer,
    can_sync: bool,
}

impl Session {
    /// `auth` is `None` while the password is still being fetched from the
    /// keyring: the session can read the cache but not the network.
    pub fn new(
        env: &Env,
        setup: &ServerSetup,
        auth: Option<Auth>,
    ) -> Result<Self, gerrit_api::Error> {
        let can_sync = auth.is_some();
        let client = GerritClient::new(
            env.http.clone(),
            &setup.base_url,
            auth.unwrap_or(Auth::Anonymous),
        )?;
        Ok(Self {
            key: setup.key.clone(),
            backend: env.server_backend(&setup.key),
            base_url: setup.base_url.trim_end_matches('/').to_owned(),
            username: setup.username.clone(),
            syncer: Syncer::new(client, env.store.clone(), setup.server),
            can_sync,
        })
    }

    /// The persistence of this server: password, queries and bookmarks.
    pub fn backend(&self) -> &Arc<dyn ServerBackend> {
        &self.backend
    }

    pub fn syncer(&self) -> &Syncer {
        &self.syncer
    }

    /// Whether the user is signed in, so that comments can be written
    /// (also while the password is still loading: drafts are kept locally
    /// and uploaded later).
    pub fn can_comment(&self) -> bool {
        self.username.is_some()
    }

    /// Whether drafts can be sent: signed in, and the password is known.
    pub fn can_upload(&self) -> bool {
        self.can_sync && self.can_comment()
    }

    /// False while credentials are still loading.
    pub fn can_sync(&self) -> bool {
        self.can_sync
    }
}

/// True if `e` means the credentials were refused (401). A 403 (logged in,
/// but not allowed) is deliberately not included: it must not log the user out.
pub fn is_unauthorized(e: &sync::Error) -> bool {
    matches!(e, sync::Error::Api(e) if e.is_unauthorized())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_401_means_login_again() {
        let api = |e| sync::Error::Api(e);
        assert!(is_unauthorized(&api(gerrit_api::Error::Unauthorized)));
        assert!(!is_unauthorized(&api(gerrit_api::Error::Forbidden {
            message: "no".into()
        })));
        assert!(!is_unauthorized(&api(gerrit_api::Error::NotFound)));
    }
}
