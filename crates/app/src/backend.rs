//! The keyring and config persistence the UI asks for.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use store::Store;

use ui::{
    Backend, BackendError, ColorModes, SavedQuery, ServerBackend, ServerConfig as UiServerConfig,
    ServerKey, StartupPage as UiStartupPage, ThemeName, filters::FileFilter, server_name_from_url,
};

use crate::{
    config::{Config, FilterConfig, QueryConfig, ServerConfig, StartupConfig, StartupPage},
    secrets::SecretStore,
};

pub struct AppBackend {
    pub config_file: PathBuf,
    pub keyring: Arc<dyn SecretStore>,
    /// The cache, for purging a removed server.
    pub store: Arc<Mutex<Store>>,
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

/// Debug builds only: the HTTP password from `GERT_DEV_PASSWORD`, for
/// running against the local dev Gerrit (`dev/README.md`) where no keyring
/// is at hand, such as a headless session. It only applies to a server on
/// this machine, so a password meant for the dev server cannot be sent to
/// another one. It is never logged, stored or written anywhere.
#[cfg(debug_assertions)]
fn dev_password(base_url: &str) -> Option<String> {
    dev_password_from(base_url, std::env::var("GERT_DEV_PASSWORD").ok())
}

#[cfg(debug_assertions)]
fn dev_password_from(base_url: &str, value: Option<String>) -> Option<String> {
    let local = ["http://127.0.0.1:", "http://localhost:"]
        .iter()
        .any(|prefix| base_url.starts_with(prefix));
    value.filter(|p| local && !p.is_empty())
}

/// The persistence of one server: its keyring entry and its part of the
/// config file, named by the server's id.
pub struct AppServerBackend {
    config_file: PathBuf,
    keyring: Arc<dyn SecretStore>,
    id: String,
}

impl AppServerBackend {
    /// Changes this server's entry of the config, which must exist.
    fn update_server(
        &self,
        change: impl FnOnce(&mut ServerConfig),
    ) -> Result<ServerConfig, BackendError> {
        let mut found = false;
        let config = Config::update(&self.config_file, |c| {
            if let Some(s) = c.servers.iter_mut().find(|s| s.id == self.id) {
                found = true;
                change(s);
            }
        })
        .map_err(|e| BackendError(e.to_string()))?;
        match config.server(&self.id) {
            Some(s) if found => Ok(s.clone()),
            _ => Err(BackendError(format!(
                "no server {:?} in the configuration",
                self.id
            ))),
        }
    }
}

fn queries_from_ui(queries: &[SavedQuery]) -> Vec<QueryConfig> {
    queries
        .iter()
        .map(|q| QueryConfig {
            name: q.name.clone(),
            query: q.query.clone(),
        })
        .collect()
}

impl ServerBackend for AppServerBackend {
    fn load_password(
        &self,
        base_url: &str,
        username: &str,
    ) -> Result<Option<String>, BackendError> {
        #[cfg(debug_assertions)]
        if let Some(password) = dev_password(base_url) {
            return Ok(Some(password));
        }
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
        let config = Config::update(&self.config_file, |c| {
            *c = std::mem::take(c).after_login(&self.id, base_url, username);
        })
        .map_err(|e| BackendError(e.to_string()))?;
        Ok(config
            .server(&self.id)
            .map_or_else(Vec::new, |s| queries_to_ui(&s.queries)))
    }

    fn save_queries(&self, queries: &[SavedQuery]) -> Result<Vec<SavedQuery>, BackendError> {
        let s = self.update_server(|s| s.queries = queries_from_ui(queries))?;
        Ok(queries_to_ui(&s.queries))
    }

    fn save_bookmarks(&self, bookmarks: &[SavedQuery]) -> Result<Vec<SavedQuery>, BackendError> {
        let s = self.update_server(|s| s.bookmarks = queries_from_ui(bookmarks))?;
        Ok(queries_to_ui(&s.bookmarks))
    }
}

/// Whether a server other than `slug` logs in as `username` at `url`, so
/// that the keyring account is shared.
fn uses_account(config: &Config, slug: &str, url: &str, username: &str) -> bool {
    config.servers.iter().any(|s| {
        s.id != slug
            && s.username.as_deref() == Some(username)
            && crate::secrets::normalize_url(&s.url) == crate::secrets::normalize_url(url)
    })
}

/// The UI's form of a configured server.
pub fn server_to_ui(s: &ServerConfig) -> UiServerConfig {
    UiServerConfig {
        key: ServerKey::new(&s.id),
        name: s.name.clone(),
        url: s.url.clone(),
        username: s.username.clone(),
        color: s.color.clone(),
    }
}

impl Backend for AppBackend {
    fn for_server(self: Arc<Self>, key: &ServerKey) -> Arc<dyn ServerBackend> {
        Arc::new(AppServerBackend {
            config_file: self.config_file.clone(),
            keyring: self.keyring.clone(),
            id: key.as_str().to_owned(),
        })
    }

    fn new_server_key(&self, base_url: &str) -> ServerKey {
        // An unreadable file has no ids to avoid; the save that follows
        // reports the problem.
        let config = Config::load(&self.config_file)
            .ok()
            .flatten()
            .unwrap_or_default();
        ServerKey::new(config.unique_server_id(&server_name_from_url(base_url)))
    }

    fn save_servers(
        &self,
        servers: &[UiServerConfig],
    ) -> Result<Vec<UiServerConfig>, BackendError> {
        let config = Config::update(&self.config_file, |c| {
            let old = std::mem::take(&mut c.servers);
            c.servers = servers
                .iter()
                .map(|s| {
                    let prev = old.iter().find(|o| o.id == s.key.as_str());
                    ServerConfig {
                        id: s.key.as_str().to_owned(),
                        name: s.name.clone(),
                        url: s.url.clone(),
                        username: s.username.clone(),
                        color: s.color.clone(),
                        queries: prev.map(|p| p.queries.clone()).unwrap_or_default(),
                        bookmarks: prev.map(|p| p.bookmarks.clone()).unwrap_or_default(),
                    }
                })
                .collect();
        })
        .map_err(|e| BackendError(e.to_string()))?;
        Ok(config.servers.iter().map(server_to_ui).collect())
    }

    fn remove_server(
        &self,
        removed: &UiServerConfig,
        remaining: &[UiServerConfig],
    ) -> Result<Vec<UiServerConfig>, BackendError> {
        let slug = removed.key.as_str();
        // What the file says wins over what the UI last saw.
        let config = Config::load(&self.config_file)
            .map_err(|e| BackendError(e.to_string()))?
            .unwrap_or_default();
        let (url, username) = config
            .server(slug)
            .map_or((&removed.url, &removed.username), |s| (&s.url, &s.username));
        // The cache and the secret first: if either fails the entry stays
        // and the user can try again. The secret stays when another entry
        // logs in with the same account.
        self.store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .delete_server_by_slug(slug)
            .map_err(|e| BackendError(e.to_string()))?;
        if let Some(user) = username
            && !uses_account(&config, slug, url, user)
        {
            self.keyring
                .delete(url, user)
                .map_err(|e| BackendError(e.to_string()))?;
        }
        self.save_servers(remaining)
    }

    fn forget_login(&self, base_url: &str, username: &str) -> Result<(), BackendError> {
        let config = Config::load(&self.config_file)
            .map_err(|e| BackendError(e.to_string()))?
            .unwrap_or_default();
        if config.servers.iter().any(|s| {
            s.username.as_deref() == Some(username)
                && crate::secrets::normalize_url(&s.url) == crate::secrets::normalize_url(base_url)
        }) {
            return Ok(());
        }
        self.keyring
            .delete(base_url, username)
            .map_err(|e| BackendError(e.to_string()))
    }

    fn save_filters(&self, filters: &[FileFilter]) -> Result<Vec<FileFilter>, BackendError> {
        let config = Config::update(&self.config_file, |c| {
            // The rules the loader ignores stay in the file, after the ones
            // the UI manages; they are still ignored on the next load.
            let ignored = c.ignored_filters();
            let managed: Vec<FilterConfig> =
                filters.iter().map(FilterConfig::from_filter).collect();
            c.filters = managed.into_iter().chain(ignored).collect();
        })
        .map_err(|e| BackendError(e.to_string()))?;
        Ok(config.file_filters())
    }

    fn save_startup_page(&self, page: &UiStartupPage) -> Result<UiStartupPage, BackendError> {
        let target = match page {
            UiStartupPage::Dashboard => StartupPage::Dashboard,
            UiStartupPage::Search(q) => StartupPage::Query(q.clone()),
        };
        let config = Config::update(&self.config_file, |c| {
            c.startup = Some(StartupConfig::for_page(&target));
        })
        .map_err(|e| BackendError(e.to_string()))?;
        Ok(startup_to_ui(&config.startup_page()))
    }

    fn save_advance_after_vote(&self, on: bool) -> Result<bool, BackendError> {
        let config = Config::update(&self.config_file, |c| {
            c.review
                .get_or_insert_with(Default::default)
                .advance_after_vote = Some(on);
        })
        .map_err(|e| BackendError(e.to_string()))?;
        Ok(config.advance_after_vote())
    }

    fn save_theme(&self, theme: ThemeName) -> Result<ThemeName, BackendError> {
        let config = Config::update(&self.config_file, |c| {
            c.appearance.get_or_insert_with(Default::default).theme = Some(theme.key().to_owned());
        })
        .map_err(|e| BackendError(e.to_string()))?;
        Ok(config.theme())
    }

    fn save_color_modes(&self, modes: ColorModes) -> Result<ColorModes, BackendError> {
        let config = Config::update(&self.config_file, |c| {
            let appearance = c.appearance.get_or_insert_with(Default::default);
            appearance.mode = Some(modes.ui.key().to_owned());
            appearance.diff_mode = Some(modes.diff.key().to_owned());
        })
        .map_err(|e| BackendError(e.to_string()))?;
        Ok(config.color_modes())
    }
}

/// The UI's form of the configured startup page.
pub fn startup_to_ui(page: &StartupPage) -> UiStartupPage {
    match page {
        StartupPage::Dashboard => UiStartupPage::Dashboard,
        StartupPage::Query(q) => UiStartupPage::Search(q.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::MemorySecrets;

    fn mem_store() -> Arc<Mutex<Store>> {
        Arc::new(Mutex::new(Store::open_in_memory().unwrap()))
    }

    impl AppBackend {
        /// The backend of server `id`, without going through `Arc`.
        fn server(&self, id: &str) -> AppServerBackend {
            AppServerBackend {
                config_file: self.config_file.clone(),
                keyring: self.keyring.clone(),
                id: id.to_owned(),
            }
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn the_dev_password_only_applies_to_a_local_server() {
        let pw = || Some("pw".to_owned());
        assert_eq!(
            dev_password_from("http://127.0.0.1:8099", pw()).as_deref(),
            Some("pw")
        );
        assert_eq!(
            dev_password_from("http://localhost:8080", pw()).as_deref(),
            Some("pw")
        );
        assert_eq!(dev_password_from("https://gerrit.example.com", pw()), None);
        assert_eq!(
            dev_password_from("http://127.0.0.1.example.com", pw()),
            None
        );
        assert_eq!(
            dev_password_from("http://127.0.0.1:8099", Some(String::new())),
            None
        );
        assert_eq!(dev_password_from("http://127.0.0.1:8099", None), None);
    }

    #[test]
    fn anonymous_login_writes_config_without_touching_the_keyring() {
        let dir = std::env::temp_dir().join(format!("backend-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let b = AppBackend {
            config_file: dir.join("config.toml"),
            keyring: Arc::new(MemorySecrets::default()),
            store: mem_store(),
        };
        let queries = b
            .server("g")
            .save_login("https://g.example.com", None, None)
            .unwrap();
        assert_eq!(queries.len(), 2);
        let cfg = Config::load(&b.config_file).unwrap().unwrap();
        assert_eq!(cfg.servers[0].username, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_queries_keeps_the_comments_of_the_file() {
        let dir = std::env::temp_dir().join(format!("backend-queries-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let b = AppBackend {
            config_file: dir.join("config.toml"),
            keyring: Arc::new(MemorySecrets::default()),
            store: mem_store(),
        };
        std::fs::write(
            &b.config_file,
            "# mine\n[server]\nurl = \"https://g\" # prod\n\n[[queries]]\nname = \"A\"\nquery = \"x\"\n",
        )
        .unwrap();
        let new = [SavedQuery {
            name: "B".into(),
            query: "y".into(),
        }];
        assert_eq!(b.server("g").save_queries(&new).unwrap(), new);
        let text = std::fs::read_to_string(&b.config_file).unwrap();
        assert!(text.contains("# mine") && text.contains("# prod"), "{text}");
        assert!(text.contains("name = \"B\""), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_bookmarks_keeps_the_comments_and_the_queries() {
        let dir = std::env::temp_dir().join(format!("backend-bookmarks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let b = AppBackend {
            config_file: dir.join("config.toml"),
            keyring: Arc::new(MemorySecrets::default()),
            store: mem_store(),
        };
        std::fs::write(
            &b.config_file,
            "# mine\n[server]\nurl = \"https://g\" # prod\n\n[[queries]]\nname = \"A\"\nquery = \"x\"\n",
        )
        .unwrap();
        let new = [SavedQuery {
            name: "Mine".into(),
            query: "owner:self".into(),
        }];
        assert_eq!(b.server("g").save_bookmarks(&new).unwrap(), new);
        let text = std::fs::read_to_string(&b.config_file).unwrap();
        assert!(text.contains("# mine") && text.contains("# prod"), "{text}");
        assert!(
            text.contains("[[servers.bookmarks]]\nname = \"Mine\""),
            "{text}"
        );
        assert!(text.contains("[[servers.queries]]\nname = \"A\""), "{text}");
        assert_eq!(b.server("g").save_bookmarks(&[]).unwrap(), []);
        let text = std::fs::read_to_string(&b.config_file).unwrap();
        assert!(!text.contains("bookmarks"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_filters_keeps_the_comments_and_the_queries() {
        use ui::filters::FilterAction;
        let dir = std::env::temp_dir().join(format!("backend-filters-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let b = AppBackend {
            config_file: dir.join("config.toml"),
            keyring: Arc::new(MemorySecrets::default()),
            store: mem_store(),
        };
        std::fs::write(
            &b.config_file,
            "# mine\n[server]\nurl = \"https://g\" # prod\n\n[[queries]]\nname = \"A\"\nquery = \"x\"\n",
        )
        .unwrap();
        let new = [FileFilter {
            name: "Gen".into(),
            globs: vec!["*.pb.go".into()],
            projects: Vec::new(),
            servers: Vec::new(),
            action: FilterAction::Dim,
            enabled: true,
        }];
        assert_eq!(b.save_filters(&new).unwrap(), new);
        let text = std::fs::read_to_string(&b.config_file).unwrap();
        assert!(text.contains("# mine") && text.contains("# prod"), "{text}");
        assert!(text.contains("name = \"A\""), "{text}");
        assert!(text.contains("action = \"dim\""), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_filters_keeps_the_tables_the_loader_ignores() {
        use ui::filters::FilterAction;
        let dir = std::env::temp_dir().join(format!("backend-ignored-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let b = AppBackend {
            config_file: dir.join("config.toml"),
            keyring: Arc::new(MemorySecrets::default()),
            store: mem_store(),
        };
        std::fs::write(
            &b.config_file,
            "[[filters]]\nname = \"Gen\"\nglobs = [\"a\"]\naction = \"dim\"\n\n\
             # my experiment, not working yet\n[[filters]]\nname = \"Odd\"\n\
             globs = [\"b\"]\naction = \"explode\"\n",
        )
        .unwrap();
        let new = [FileFilter {
            name: "Other".into(),
            globs: vec!["*.lock".into()],
            projects: Vec::new(),
            servers: Vec::new(),
            action: FilterAction::Hide,
            enabled: true,
        }];
        assert_eq!(b.save_filters(&new).unwrap(), new);
        let text = std::fs::read_to_string(&b.config_file).unwrap();
        assert!(text.contains("# my experiment"), "{text}");
        assert!(text.contains("action = \"explode\""), "{text}");
        assert!(text.contains("name = \"Other\""), "{text}");
        assert!(!text.contains("name = \"Gen\""), "{text}");
        // Saving the same list again changes nothing.
        let before = text.clone();
        b.save_filters(&new).unwrap();
        assert_eq!(std::fs::read_to_string(&b.config_file).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_startup_page_and_advance_setting_round_trip_with_comments() {
        let dir = std::env::temp_dir().join(format!("backend-startup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let b = AppBackend {
            config_file: dir.join("config.toml"),
            keyring: Arc::new(MemorySecrets::default()),
            store: mem_store(),
        };
        std::fs::write(
            &b.config_file,
            "[server]\nurl = \"https://g\"\n\n# opens on start\n[startup]\npage = \"dashboard\"\n",
        )
        .unwrap();
        let search = UiStartupPage::Search("is:open owner:self".into());
        assert_eq!(b.save_startup_page(&search).unwrap(), search);
        let text = std::fs::read_to_string(&b.config_file).unwrap();
        assert!(text.contains("# opens on start"), "{text}");
        assert!(text.contains("query = \"is:open owner:self\""), "{text}");
        let cfg = Config::load(&b.config_file).unwrap().unwrap();
        assert_eq!(
            cfg.startup_page(),
            StartupPage::Query("is:open owner:self".into())
        );

        assert_eq!(
            b.save_startup_page(&UiStartupPage::Dashboard).unwrap(),
            UiStartupPage::Dashboard
        );
        let text = std::fs::read_to_string(&b.config_file).unwrap();
        assert!(!text.contains("query ="), "{text}");
        assert!(text.contains("# opens on start"), "{text}");

        assert!(!b.save_advance_after_vote(false).unwrap());
        let cfg = Config::load(&b.config_file).unwrap().unwrap();
        assert!(!cfg.advance_after_vote());

        assert_eq!(
            b.save_theme(ThemeName::Solarized).unwrap(),
            ThemeName::Solarized
        );
        let text = std::fs::read_to_string(&b.config_file).unwrap();
        assert!(
            text.contains("[appearance]\ntheme = \"solarized\""),
            "{text}"
        );
        assert!(text.contains("# opens on start"), "{text}");
        let cfg = Config::load(&b.config_file).unwrap().unwrap();
        assert_eq!(cfg.theme(), ThemeName::Solarized);

        let modes = ColorModes {
            ui: ui::ColorMode::Dark,
            diff: ui::ColorMode::Light,
        };
        assert_eq!(b.save_color_modes(modes).unwrap(), modes);
        let text = std::fs::read_to_string(&b.config_file).unwrap();
        assert!(text.contains("mode = \"dark\""), "{text}");
        assert!(text.contains("diff_mode = \"light\""), "{text}");
        assert!(text.contains("theme = \"solarized\""), "{text}");
        let cfg = Config::load(&b.config_file).unwrap().unwrap();
        assert_eq!(cfg.color_modes(), modes);
        let _ = std::fs::remove_dir_all(&dir);
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

[[servers.queries]]
name = "Open"
query = "status:open"

[[filters]]
name = "Gen"
globs = ["a"]
servers = ["oss"]
action = "dim"
"##;

    fn two_servers(name: &str) -> AppBackend {
        let dir = std::env::temp_dir().join(format!("backend-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let b = AppBackend {
            config_file: dir.join("config.toml"),
            keyring: Arc::new(MemorySecrets::default()),
            store: mem_store(),
        };
        std::fs::write(&b.config_file, TWO).unwrap();
        b
    }

    fn saved(name: &str, query: &str) -> SavedQuery {
        SavedQuery {
            name: name.into(),
            query: query.into(),
        }
    }

    #[test]
    fn each_server_backend_writes_only_its_own_lists() {
        let b = two_servers("own-lists");
        let new = [saved("Merged", "status:merged")];
        assert_eq!(b.server("oss").save_queries(&new).unwrap(), new);
        assert_eq!(b.server("oss").save_bookmarks(&new).unwrap(), new);
        let text = std::fs::read_to_string(&b.config_file).unwrap();
        for kept in [
            "# two servers",
            "# the big one",
            "# work queries",
            "query = \"owner:self\"",
        ] {
            assert!(text.contains(kept), "lost {kept:?} in\n{text}");
        }
        let cfg = Config::load(&b.config_file).unwrap().unwrap();
        assert_eq!(cfg.server("work").unwrap().queries.len(), 1);
        assert_eq!(cfg.server("work").unwrap().queries[0].name, "Mine");
        assert!(cfg.server("work").unwrap().bookmarks.is_empty());
        assert_eq!(cfg.server("oss").unwrap().queries[0].name, "Merged");
        assert_eq!(cfg.server("oss").unwrap().bookmarks[0].name, "Merged");
        // Another server's writes do not go through this one.
        assert_eq!(b.server("work").save_bookmarks(&[]).unwrap(), []);
        let cfg = Config::load(&b.config_file).unwrap().unwrap();
        assert_eq!(cfg.server("oss").unwrap().bookmarks.len(), 1);
        let _ = std::fs::remove_dir_all(b.config_file.parent().unwrap());
    }

    #[test]
    fn a_server_that_is_not_configured_is_an_error_and_writes_nothing() {
        let b = two_servers("unknown");
        let err = b.server("gone").save_queries(&[]).unwrap_err();
        assert!(err.0.contains("gone"), "{err}");
        assert_eq!(std::fs::read_to_string(&b.config_file).unwrap(), TWO);
        let _ = std::fs::remove_dir_all(b.config_file.parent().unwrap());
    }

    #[test]
    fn login_updates_its_server_and_adds_a_new_one() {
        let b = two_servers("login");
        let q = b
            .server("oss")
            .save_login("https://gerrit.oss.example", Some("alice"), None)
            .unwrap();
        assert_eq!(q, [saved("Open", "status:open")]);
        let key = b.new_server_key("https://other.example");
        assert_eq!(key.as_str(), "other-example");
        b.server(key.as_str())
            .save_login("https://other.example", None, None)
            .unwrap();
        // A second server on the same host gets another id.
        assert_eq!(
            b.new_server_key("https://other.example").as_str(),
            "other-example-2"
        );
        let cfg = Config::load(&b.config_file).unwrap().unwrap();
        let ids: Vec<_> = cfg.servers.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["work", "oss", "other-example"]);
        assert_eq!(
            cfg.server("oss").unwrap().username.as_deref(),
            Some("alice")
        );
        assert_eq!(cfg.server("other-example").unwrap().queries.len(), 2);
        let text = std::fs::read_to_string(&b.config_file).unwrap();
        assert!(
            text.contains("# the big one") && text.contains("# work queries"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(b.config_file.parent().unwrap());
    }

    #[test]
    fn save_servers_edits_reorders_and_removes_but_keeps_the_lists() {
        let b = two_servers("save-servers");
        let ui_servers: Vec<_> = Config::load(&b.config_file)
            .unwrap()
            .unwrap()
            .servers
            .iter()
            .map(server_to_ui)
            .collect();
        let mut edited = vec![ui_servers[1].clone(), ui_servers[0].clone()];
        edited[0].name = "Open source".into();
        edited[0].color = Some("#ff8800".into());
        let back = b.save_servers(&edited).unwrap();
        assert_eq!(back, edited);
        let cfg = Config::load(&b.config_file).unwrap().unwrap();
        assert_eq!(cfg.servers[0].id, "oss");
        assert_eq!(cfg.servers[0].name, "Open source");
        assert_eq!(cfg.servers[0].queries[0].name, "Open");
        assert_eq!(cfg.servers[1].queries[0].name, "Mine");
        let text = std::fs::read_to_string(&b.config_file).unwrap();
        assert!(
            text.contains("# work queries") && text.contains("# the big one"),
            "{text}"
        );

        b.save_servers(&edited[..1]).unwrap();
        let cfg = Config::load(&b.config_file).unwrap().unwrap();
        assert_eq!(cfg.servers.len(), 1);
        assert!(
            !std::fs::read_to_string(&b.config_file)
                .unwrap()
                .contains("work")
        );

        let dup = [edited[0].clone(), edited[0].clone()];
        let err = b.save_servers(&dup).unwrap_err();
        assert!(err.0.contains("oss"), "{err}");
        let _ = std::fs::remove_dir_all(b.config_file.parent().unwrap());
    }

    #[test]
    fn the_servers_restriction_of_a_rule_is_loaded_and_saved() {
        use ui::filters::FilterAction;
        let b = two_servers("filter-servers");
        // The rule of the file reaches the UI with its servers.
        let loaded = Config::load(&b.config_file)
            .unwrap()
            .unwrap()
            .file_filters();
        assert_eq!(loaded[0].servers, [ServerKey::new("oss")]);
        // What the UI saves is what is written, restriction included: a
        // rule the UI made without servers has none afterwards.
        let new = [
            FileFilter {
                servers: vec![ServerKey::new("work"), ServerKey::new("oss")],
                ..loaded[0].clone()
            },
            FileFilter {
                name: "Lock".into(),
                globs: vec!["*.lock".into()],
                projects: Vec::new(),
                servers: Vec::new(),
                action: FilterAction::Hide,
                enabled: true,
            },
        ];
        assert_eq!(b.save_filters(&new).unwrap(), new);
        let cfg = Config::load(&b.config_file).unwrap().unwrap();
        assert_eq!(cfg.filters[0].servers, ["work", "oss"]);
        assert!(cfg.filters[1].servers.is_empty());
        let text = std::fs::read_to_string(&b.config_file).unwrap();
        assert_eq!(text.matches("servers = ").count(), 1, "{text}");
        let _ = std::fs::remove_dir_all(b.config_file.parent().unwrap());
    }

    /// Two servers, with a password and a cache for each.
    fn stocked(name: &str) -> (AppBackend, Arc<MemorySecrets>, Arc<Mutex<Store>>) {
        let mut b = two_servers(name);
        let secrets = Arc::new(MemorySecrets::default());
        b.keyring = secrets.clone();
        let store = b.store.clone();
        {
            let mut s = store.lock().unwrap();
            for (slug, url) in [
                ("work", "https://gerrit.work.example"),
                ("oss", "https://gerrit.oss.example"),
            ] {
                let id = s.server_for_slug(slug, url).unwrap();
                s.set_app_state(&format!("seen-{slug}"), "1").unwrap();
                s.save_query_page(id, "q", &[], false, "2024-06-15T10:00:00Z".parse().unwrap())
                    .unwrap();
            }
        }
        secrets
            .set("https://gerrit.work.example", "jdoe", "pw")
            .unwrap();
        (b, secrets, store)
    }

    fn ui_servers(b: &AppBackend) -> Vec<UiServerConfig> {
        Config::load(&b.config_file)
            .unwrap()
            .unwrap()
            .servers
            .iter()
            .map(server_to_ui)
            .collect()
    }

    fn cached(store: &Mutex<Store>, slug: &str, url: &str) -> bool {
        let mut s = store.lock().unwrap();
        let id = s.server_for_slug(slug, url).unwrap();
        s.load_query(id, "q").unwrap().is_some()
    }

    #[test]
    fn removing_a_server_purges_config_cache_and_secret() {
        let (b, secrets, store) = stocked("remove");
        let all = ui_servers(&b);
        let left = b.remove_server(&all[0], &all[1..]).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].key.as_str(), "oss");
        let cfg = Config::load(&b.config_file).unwrap().unwrap();
        assert!(cfg.server("work").is_none());
        assert_eq!(cfg.server("oss").unwrap().queries[0].name, "Open");
        assert!(secrets.accounts().is_empty(), "{:?}", secrets.accounts());
        assert!(!cached(&store, "work", "https://gerrit.work.example"));
        // The other server's cache is still there.
        assert!(cached(&store, "oss", "https://gerrit.oss.example"));
        let text = std::fs::read_to_string(&b.config_file).unwrap();
        assert!(text.contains("[[filters]]"), "{text}");
        let _ = std::fs::remove_dir_all(b.config_file.parent().unwrap());
    }

    #[test]
    fn removing_the_last_server_leaves_an_empty_config() {
        let (b, secrets, store) = stocked("remove-last");
        let all = ui_servers(&b);
        b.remove_server(&all[0], &all[1..]).unwrap();
        let all = ui_servers(&b);
        assert!(b.remove_server(&all[0], &[]).unwrap().is_empty());
        let cfg = Config::load(&b.config_file).unwrap().unwrap();
        assert!(cfg.servers.is_empty());
        assert!(secrets.accounts().is_empty());
        assert!(!cached(&store, "oss", "https://gerrit.oss.example"));
        let _ = std::fs::remove_dir_all(b.config_file.parent().unwrap());
    }

    #[test]
    fn removing_a_server_keeps_a_secret_another_server_shares() {
        let (b, secrets, _) = stocked("remove-shared");
        Config::update(&b.config_file, |c| {
            let mut twin = c.servers[0].clone();
            twin.id = "work-2".into();
            c.servers.push(twin);
        })
        .unwrap();
        let all = ui_servers(&b);
        let rest: Vec<_> = all[1..].to_vec();
        b.remove_server(&all[0], &rest).unwrap();
        assert_eq!(secrets.accounts(), ["jdoe@https://gerrit.work.example"]);
        let all = ui_servers(&b);
        let twin = all.iter().find(|s| s.key.as_str() == "work-2").unwrap();
        let rest: Vec<_> = all.iter().filter(|s| s.key != twin.key).cloned().collect();
        b.remove_server(twin, &rest).unwrap();
        assert!(secrets.accounts().is_empty());
        let _ = std::fs::remove_dir_all(b.config_file.parent().unwrap());
    }

    #[test]
    fn changing_the_url_moves_the_secret_and_keeps_the_cache() {
        let (b, secrets, store) = stocked("move-secret");
        let work = b.server("work");
        // The flow of the settings page: the login with the new URL first
        // (secret, config, cache row), then the old secret goes.
        work.save_login("https://gerrit.moved.example", Some("jdoe"), Some("new"))
            .unwrap();
        let id = store
            .lock()
            .unwrap()
            .server_for_slug("work", "https://gerrit.moved.example")
            .unwrap();
        b.forget_login("https://gerrit.work.example", "jdoe")
            .unwrap();
        assert_eq!(secrets.accounts(), ["jdoe@https://gerrit.moved.example"]);
        let cfg = Config::load(&b.config_file).unwrap().unwrap();
        assert_eq!(
            cfg.server("work").unwrap().url,
            "https://gerrit.moved.example"
        );
        // Same row, so the cache survived the move.
        assert!(store.lock().unwrap().load_query(id, "q").unwrap().is_some());
        // Forgetting a login that is still in use does nothing.
        b.forget_login("https://gerrit.moved.example/", "jdoe")
            .unwrap();
        assert_eq!(secrets.accounts().len(), 1);
        let _ = std::fs::remove_dir_all(b.config_file.parent().unwrap());
    }
}
