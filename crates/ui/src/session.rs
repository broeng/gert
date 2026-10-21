//! What the pages share: the cache, the server and the network client.

use std::sync::{Arc, Mutex};

use gerrit_api::{Auth, GerritClient, HttpClient};
use store::{ServerId, Store};
use sync::Syncer;

/// A named Gerrit query shown as a dashboard section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedQuery {
    pub name: String,
    pub query: String,
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{0}")]
pub struct BackendError(pub String);

/// Persistence the `app` crate provides: the keyring and the config file.
/// Both block, so the UI calls them from the background executor.
pub trait Backend: Send + Sync + 'static {
    /// The saved HTTP password; `Ok(None)` if there is none.
    fn load_password(&self, base_url: &str, username: &str)
    -> Result<Option<String>, BackendError>;

    /// Stores the password in the keyring (when there is one) and writes the
    /// config. Returns the saved queries now in effect (seeded on first
    /// login).
    fn save_login(
        &self,
        base_url: &str,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<Vec<SavedQuery>, BackendError>;
}

/// The pieces every page needs, handed over by `app`.
#[derive(Clone)]
pub struct Env {
    pub http: Arc<dyn HttpClient>,
    pub store: Arc<Mutex<Store>>,
    pub backend: Arc<dyn Backend>,
}

/// A configured server, possibly seen before.
#[derive(Clone, Debug)]
pub struct ServerSetup {
    pub base_url: String,
    pub username: Option<String>,
    pub server: ServerId,
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
    /// `None` when there is no config yet: start on the login page.
    pub server: Option<ServerSetup>,
    pub queries: Vec<SavedQuery>,
    pub diff: LocalDiff,
}

/// A logged-in (or anonymous) connection to one server.
#[derive(Clone)]
pub struct Session {
    pub base_url: String,
    pub username: Option<String>,
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
            base_url: setup.base_url.trim_end_matches('/').to_owned(),
            username: setup.username.clone(),
            syncer: Syncer::new(client, env.store.clone(), setup.server),
            can_sync,
        })
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
