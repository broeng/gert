//! Login: validating credentials and persisting them. The page itself is
//! in `pages::login`.

use gerrit_api::{Auth, Error as ApiError, GerritClient};
use store::ServerId;

use crate::session::{Env, SavedQuery, ServerKey, server_name_from_url};

/// What the user typed.
pub struct LoginRequest {
    /// The configured server this logs in to again; `None` for a new one.
    pub server: Option<ServerKey>,
    /// The display name of a new server; empty or `None` names it after
    /// its host. Ignored when logging in to a configured server again.
    pub name: Option<String>,
    pub base_url: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// A successful login. Holds the password, so it deliberately has no
/// `Debug`.
pub struct LoginOutcome {
    pub key: ServerKey,
    /// What the user called a new server (its host by default). For a
    /// server that was configured already the shell keeps its own name.
    pub name: String,
    pub base_url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub server: ServerId,
    pub queries: Vec<SavedQuery>,
}

#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("{0}")]
    Input(String),
    #[error("wrong username or HTTP password")]
    Unauthorized,
    #[error("this server does not allow anonymous access")]
    AnonymousDenied,
    #[error("could not talk to the server: {0}")]
    Server(String),
    #[error("could not save the login: {0}")]
    Save(String),
}

/// Trims whitespace and trailing slashes and checks the scheme.
pub fn normalize_base_url(input: &str) -> Result<String, LoginError> {
    let url = input.trim().trim_end_matches('/');
    if url.is_empty() {
        return Err(LoginError::Input("enter the server URL".into()));
    }
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(LoginError::Input(
            "the server URL must start with https:// or http://".into(),
        ));
    }
    Ok(url.to_owned())
}

/// The page where an SSO user creates an HTTP password.
pub fn credentials_page_url(base_url: &str) -> String {
    format!(
        "{}/settings/#HTTPCredentials",
        base_url.trim_end_matches('/')
    )
}

/// Asks the server a cheap question with these credentials: who they are
/// (or, anonymously, its version). Nothing is stored. Shared by the login
/// and the settings page's "Test connection".
pub async fn check_credentials(
    env: &Env,
    base_url: &str,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<(), LoginError> {
    let base_url = normalize_base_url(base_url)?;
    let anonymous = username.is_none();
    let auth = match (username, password) {
        (Some(u), Some(p)) => Auth::Basic {
            username: u.to_owned(),
            password: p.to_owned(),
        },
        (None, _) => Auth::Anonymous,
        (Some(_), None) => return Err(LoginError::Input("enter the HTTP password".into())),
    };
    let client = GerritClient::new(env.http.clone(), &base_url, auth)
        .map_err(|e| LoginError::Input(e.to_string()))?;
    let checked = if anonymous {
        client.server_version().await.map(|_| ())
    } else {
        client.account_self().await.map(|_| ())
    };
    match checked {
        Ok(()) => Ok(()),
        Err(ApiError::Unauthorized) if anonymous => Err(LoginError::AnonymousDenied),
        Err(ApiError::Unauthorized) => Err(LoginError::Unauthorized),
        Err(e) => Err(LoginError::Server(e.to_string())),
    }
}

/// Checks the credentials against the server, then stores them. Blocks on
/// SQLite and the keyring; run it on the background executor.
pub async fn login(env: &Env, req: LoginRequest) -> Result<LoginOutcome, LoginError> {
    let base_url = normalize_base_url(&req.base_url)?;
    check_credentials(
        env,
        &base_url,
        req.username.as_deref(),
        req.password.as_deref(),
    )
    .await?;
    let key = req
        .server
        .clone()
        .unwrap_or_else(|| env.backend.new_server_key(&base_url));
    let queries = env
        .server_backend(&key)
        .save_login(&base_url, req.username.as_deref(), req.password.as_deref())
        .map_err(|e| LoginError::Save(e.0))?;
    let server = env
        .store
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .server_for_slug(key.as_str(), &base_url)
        .map_err(|e| LoginError::Save(e.to_string()))?;
    let name = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map_or_else(|| server_name_from_url(&base_url), str::to_owned);
    Ok(LoginOutcome {
        key,
        name,
        base_url,
        username: req.username,
        password: req.password,
        server,
        queries,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Arc, Mutex};

    use futures::executor::block_on;
    use gerrit_api::fake::FakeHttpClient;
    use store::Store;

    use super::*;
    use crate::session::{Backend, BackendError, ServerBackend};

    type Saved = (String, Option<String>, Option<String>);

    #[derive(Default)]
    pub struct FakeBackend {
        pub saved: Mutex<Vec<Saved>>,
        pub fail: bool,
        pub saved_queries: Mutex<Vec<Vec<SavedQuery>>>,
        pub saved_bookmarks: Mutex<Vec<Vec<SavedQuery>>>,
        pub saved_startup: Mutex<Vec<crate::session::StartupPage>>,
        pub saved_advance: Mutex<Vec<bool>>,
        pub saved_theme: Mutex<Vec<crate::theme::ThemeName>>,
        /// The password `load_password` finds, for every account.
        pub password: Option<String>,
        pub saved_servers: Mutex<Vec<Vec<crate::session::ServerConfig>>>,
        /// The keys of the servers removed, in order.
        pub removed: Mutex<Vec<String>>,
        /// The (url, user) logins forgotten.
        pub forgotten: Mutex<Vec<(String, String)>>,
    }

    impl Backend for FakeBackend {
        fn for_server(self: Arc<Self>, _: &ServerKey) -> Arc<dyn ServerBackend> {
            self
        }

        fn save_filters(
            &self,
            f: &[crate::filters::FileFilter],
        ) -> Result<Vec<crate::filters::FileFilter>, BackendError> {
            Ok(f.to_vec())
        }

        fn save_servers(
            &self,
            servers: &[crate::session::ServerConfig],
        ) -> Result<Vec<crate::session::ServerConfig>, BackendError> {
            if self.fail {
                return Err(BackendError("disk full".into()));
            }
            self.saved_servers.lock().unwrap().push(servers.to_vec());
            Ok(servers.to_vec())
        }

        fn remove_server(
            &self,
            removed: &crate::session::ServerConfig,
            remaining: &[crate::session::ServerConfig],
        ) -> Result<Vec<crate::session::ServerConfig>, BackendError> {
            if self.fail {
                return Err(BackendError("disk full".into()));
            }
            self.removed.lock().unwrap().push(removed.key.to_string());
            self.save_servers(remaining)
        }

        fn forget_login(&self, url: &str, user: &str) -> Result<(), BackendError> {
            self.forgotten
                .lock()
                .unwrap()
                .push((url.to_owned(), user.to_owned()));
            Ok(())
        }

        fn save_startup_page(
            &self,
            p: &crate::session::StartupPage,
        ) -> Result<crate::session::StartupPage, BackendError> {
            if self.fail {
                return Err(BackendError("disk full".into()));
            }
            self.saved_startup.lock().unwrap().push(p.clone());
            Ok(p.clone())
        }

        fn save_advance_after_vote(&self, on: bool) -> Result<bool, BackendError> {
            if self.fail {
                return Err(BackendError("disk full".into()));
            }
            self.saved_advance.lock().unwrap().push(on);
            Ok(on)
        }

        fn save_theme(
            &self,
            t: crate::theme::ThemeName,
        ) -> Result<crate::theme::ThemeName, BackendError> {
            if self.fail {
                return Err(BackendError("disk full".into()));
            }
            self.saved_theme.lock().unwrap().push(t);
            Ok(t)
        }
    }

    impl ServerBackend for FakeBackend {
        fn load_password(&self, _: &str, _: &str) -> Result<Option<String>, BackendError> {
            Ok(self.password.clone())
        }
        fn save_queries(&self, queries: &[SavedQuery]) -> Result<Vec<SavedQuery>, BackendError> {
            if self.fail {
                return Err(BackendError("disk full".into()));
            }
            self.saved_queries.lock().unwrap().push(queries.to_vec());
            Ok(queries.to_vec())
        }
        fn save_bookmarks(&self, list: &[SavedQuery]) -> Result<Vec<SavedQuery>, BackendError> {
            if self.fail {
                return Err(BackendError("disk full".into()));
            }
            self.saved_bookmarks.lock().unwrap().push(list.to_vec());
            Ok(list.to_vec())
        }

        fn save_login(
            &self,
            url: &str,
            user: Option<&str>,
            pass: Option<&str>,
        ) -> Result<Vec<SavedQuery>, BackendError> {
            if self.fail {
                return Err(BackendError("keyring locked".into()));
            }
            self.saved.lock().unwrap().push((
                url.into(),
                user.map(Into::into),
                pass.map(Into::into),
            ));
            Ok(vec![SavedQuery {
                name: "All".into(),
                query: "status:open".into(),
            }])
        }
    }

    fn env(http: &FakeHttpClient, backend: Arc<FakeBackend>) -> Env {
        Env {
            http: Arc::new(http.clone()),
            store: Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
            backend,
        }
    }

    fn req(user: Option<&str>, pass: Option<&str>) -> LoginRequest {
        LoginRequest {
            server: None,
            name: None,
            base_url: " https://g.example.com/ ".into(),
            username: user.map(Into::into),
            password: pass.map(Into::into),
        }
    }

    const ACCOUNT: &str = ")]}'\n{\"_account_id\": 1, \"name\": \"Alice\"}";

    #[test]
    fn successful_login_saves_and_returns_queries() {
        let http = FakeHttpClient::new();
        http.push_response(200, ACCOUNT);
        let backend = Arc::new(FakeBackend::default());
        let out = block_on(login(
            &env(&http, backend.clone()),
            req(Some("alice"), Some("pw")),
        ))
        .unwrap();
        assert_eq!(out.base_url, "https://g.example.com");
        assert_eq!(out.queries.len(), 1);
        assert_eq!(
            backend.saved.lock().unwrap()[0],
            (
                "https://g.example.com".into(),
                Some("alice".into()),
                Some("pw".into())
            )
        );
        let reqs = http.requests();
        assert_eq!(
            reqs[0].uri().to_string(),
            "https://g.example.com/a/accounts/self/detail"
        );
    }

    #[test]
    fn login_goes_to_the_servers_own_backend() {
        let http = FakeHttpClient::new();
        http.push_response(200, ACCOUNT);
        http.push_response(200, ACCOUNT);
        let backend = Arc::new(FakeBackend::default());
        let env = env(&http, backend);
        // A new server gets a key from the backend ...
        let out = block_on(login(&env, req(Some("a"), Some("b")))).unwrap();
        assert_eq!(out.key, ServerKey::new("default"));
        // ... and logging in again to a configured one keeps its own.
        let again = LoginRequest {
            server: Some(ServerKey::new("work")),
            ..req(Some("a"), Some("b"))
        };
        let out = block_on(login(&env, again)).unwrap();
        assert_eq!(out.key, ServerKey::new("work"));
    }

    #[test]
    fn anonymous_login_checks_the_version() {
        let http = FakeHttpClient::new();
        http.push_response(200, ")]}'\n\"3.9.1\"");
        let backend = Arc::new(FakeBackend::default());
        let out = block_on(login(&env(&http, backend.clone()), req(None, None))).unwrap();
        assert_eq!(out.username, None);
        assert_eq!(
            http.requests()[0].uri().to_string(),
            "https://g.example.com/config/server/version"
        );
        assert_eq!(backend.saved.lock().unwrap()[0].2, None);
    }

    #[test]
    fn unauthorized_is_reported_and_nothing_is_saved() {
        let http = FakeHttpClient::new();
        http.push_response(401, "nope");
        let backend = Arc::new(FakeBackend::default());
        let err = block_on(login(
            &env(&http, backend.clone()),
            req(Some("a"), Some("bad")),
        ))
        .err()
        .unwrap();
        assert_eq!(err.to_string(), "wrong username or HTTP password");
        assert!(backend.saved.lock().unwrap().is_empty());
    }

    #[test]
    fn transport_errors_are_reported() {
        let http = FakeHttpClient::new();
        http.push_error("connection refused");
        let err = block_on(login(
            &env(&http, Arc::default()),
            req(Some("a"), Some("b")),
        ))
        .err()
        .unwrap();
        assert!(err.to_string().contains("connection refused"), "{err}");
    }

    #[test]
    fn keyring_failure_is_reported() {
        let http = FakeHttpClient::new();
        http.push_response(200, ACCOUNT);
        let backend = Arc::new(FakeBackend {
            fail: true,
            ..Default::default()
        });
        let err = block_on(login(&env(&http, backend), req(Some("a"), Some("b"))))
            .err()
            .unwrap();
        assert_eq!(err.to_string(), "could not save the login: keyring locked");
    }

    #[test]
    fn input_validation() {
        assert!(normalize_base_url("").is_err());
        assert!(normalize_base_url("gerrit.example.com").is_err());
        assert_eq!(normalize_base_url(" http://h/r/ ").unwrap(), "http://h/r");
        assert_eq!(
            credentials_page_url("https://g.example.com/"),
            "https://g.example.com/settings/#HTTPCredentials"
        );
    }
}
