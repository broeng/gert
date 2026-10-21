//! The keyring and config persistence the UI asks for.

use std::path::PathBuf;

use ui::{Backend, BackendError, SavedQuery};

use crate::{
    config::{Config, QueryConfig},
    secrets::Keyring,
};

pub struct AppBackend {
    pub config_file: PathBuf,
    pub keyring: Keyring,
}

pub fn queries_to_ui(queries: &[QueryConfig]) -> Vec<SavedQuery> {
    queries
        .iter()
        .map(|q| SavedQuery {
            name: q.name.clone(),
            query: q.query.clone(),
        })
        .collect()
}

impl Backend for AppBackend {
    fn load_password(
        &self,
        base_url: &str,
        username: &str,
    ) -> Result<Option<String>, BackendError> {
        self.keyring
            .get(base_url, username)
            .map_err(|e| BackendError(e.to_string()))
    }

    fn save_login(
        &self,
        base_url: &str,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<Vec<SavedQuery>, BackendError> {
        // The keyring first: if it is unavailable nothing is written, and
        // the user sees the error instead of a login that cannot persist.
        if let (Some(user), Some(pass)) = (username, password) {
            self.keyring
                .set(base_url, user, pass)
                .map_err(|e| BackendError(e.to_string()))?;
        }
        let existing = Config::load(&self.config_file).unwrap_or_else(|e| {
            log::warn!("ignoring unreadable config: {e}");
            None
        });
        let config = existing.unwrap_or_default().after_login(base_url, username);
        config
            .save(&self.config_file)
            .map_err(|e| BackendError(e.to_string()))?;
        Ok(queries_to_ui(&config.queries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymous_login_writes_config_without_touching_the_keyring() {
        let dir = std::env::temp_dir().join(format!("backend-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let b = AppBackend {
            config_file: dir.join("config.toml"),
            keyring: Keyring,
        };
        let queries = b.save_login("https://g.example.com", None, None).unwrap();
        assert_eq!(queries.len(), 2);
        let cfg = Config::load(&b.config_file).unwrap().unwrap();
        assert_eq!(cfg.server.unwrap().username, None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
