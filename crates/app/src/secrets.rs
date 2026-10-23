//! The HTTP password in the OS keyring (Secret Service on Linux, Keychain
//! on macOS). The password never goes to the config file, the cache DB or
//! the logs, and there is no plaintext fallback: when the keyring is
//! unavailable the caller must tell the user.

use crate::APP_NAME;

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("the OS keyring is not available: {0}")]
    Unavailable(String),
    #[error("keyring error: {0}")]
    Other(String),
}

impl From<keyring::Error> for SecretError {
    fn from(e: keyring::Error) -> Self {
        // The messages of keyring errors never contain the secret.
        match e {
            keyring::Error::NoDefaultStore
            | keyring::Error::NoStorageAccess(_)
            | keyring::Error::PlatformFailure(_) => SecretError::Unavailable(e.to_string()),
            other => SecretError::Other(other.to_string()),
        }
    }
}

/// The keyring account name for a server and user: `user@url`, with the
/// scheme and host lowercased and trailing slashes dropped so that
/// `HTTPS://Gerrit.Example.com/` and `https://gerrit.example.com` agree.
pub fn account_key(server_url: &str, username: &str) -> String {
    format!("{username}@{}", normalize_url(server_url))
}

/// Trims whitespace and trailing slashes, and lowercases everything up to
/// the end of the host (paths are case-sensitive).
pub fn normalize_url(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    let host_start = url.find("://").map_or(0, |i| i + 3);
    let host_end = url[host_start..]
        .find('/')
        .map_or(url.len(), |i| host_start + i);
    format!("{}{}", url[..host_end].to_lowercase(), &url[host_end..])
}

/// Where the passwords live. The OS keyring in the app; tests use
/// [`MemorySecrets`] so that no real keyring is touched.
pub trait SecretStore: Send + Sync + 'static {
    /// May block (D-Bus, Keychain); call from a background thread.
    fn get(&self, server_url: &str, username: &str) -> Result<Option<String>, SecretError>;
    /// May block; call from a background thread.
    fn set(&self, server_url: &str, username: &str, password: &str) -> Result<(), SecretError>;
    /// Removes the password; fine if there is none. May block.
    fn delete(&self, server_url: &str, username: &str) -> Result<(), SecretError>;
}

/// Reads and writes the password of one server login in the OS keyring.
#[derive(Clone, Copy, Debug, Default)]
pub struct Keyring;

impl Keyring {
    fn entry(server_url: &str, username: &str) -> Result<keyring::Entry, SecretError> {
        Ok(keyring::Entry::new(
            APP_NAME,
            &account_key(server_url, username),
        )?)
    }
}

impl SecretStore for Keyring {
    fn get(&self, server_url: &str, username: &str) -> Result<Option<String>, SecretError> {
        match Self::entry(server_url, username)?.get_password() {
            Ok(p) => Ok(Some(p)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn set(&self, server_url: &str, username: &str, password: &str) -> Result<(), SecretError> {
        Ok(Self::entry(server_url, username)?.set_password(password)?)
    }

    fn delete(&self, server_url: &str, username: &str) -> Result<(), SecretError> {
        match Self::entry(server_url, username)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Passwords kept in memory, keyed like the keyring accounts. For tests.
#[derive(Debug, Default)]
pub struct MemorySecrets(std::sync::Mutex<std::collections::HashMap<String, String>>);

impl MemorySecrets {
    fn map(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, String>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The accounts that hold a password, sorted.
    pub fn accounts(&self) -> Vec<String> {
        let mut accounts: Vec<String> = self.map().keys().cloned().collect();
        accounts.sort();
        accounts
    }
}

impl SecretStore for MemorySecrets {
    fn get(&self, server_url: &str, username: &str) -> Result<Option<String>, SecretError> {
        Ok(self.map().get(&account_key(server_url, username)).cloned())
    }

    fn set(&self, server_url: &str, username: &str, password: &str) -> Result<(), SecretError> {
        self.map()
            .insert(account_key(server_url, username), password.to_owned());
        Ok(())
    }

    fn delete(&self, server_url: &str, username: &str) -> Result<(), SecretError> {
        self.map().remove(&account_key(server_url, username));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_user_at_normalized_url() {
        assert_eq!(
            account_key("https://gerrit.example.com", "jdoe"),
            "jdoe@https://gerrit.example.com"
        );
    }

    #[test]
    fn normalization_ignores_case_of_host_and_trailing_slash() {
        assert_eq!(
            account_key(" HTTPS://Gerrit.Example.com/ ", "jdoe"),
            account_key("https://gerrit.example.com", "jdoe")
        );
    }

    #[test]
    fn path_prefix_case_is_kept() {
        assert_eq!(
            normalize_url("https://Host.example.com/R/Gerrit/"),
            "https://host.example.com/R/Gerrit"
        );
    }

    #[test]
    fn different_users_get_different_keys() {
        assert_ne!(
            account_key("https://g.example.com", "a"),
            account_key("https://g.example.com", "b")
        );
    }
}
