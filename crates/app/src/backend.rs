//! The keyring and config persistence the UI asks for.

use std::path::PathBuf;

use ui::{
    Backend, BackendError, ColorModes, SavedQuery, StartupPage as UiStartupPage, ThemeName,
    filters::FileFilter,
};

use crate::{
    config::{Config, FilterConfig, QueryConfig, StartupConfig, StartupPage, server_name_from_url},
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

fn first_queries(config: &Config) -> Vec<SavedQuery> {
    queries_to_ui(
        config
            .servers
            .first()
            .map_or(&[][..], |s| s.queries.as_slice()),
    )
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

impl Backend for AppBackend {
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
        // One server for now: a login replaces the first one.
        let config = Config::update(&self.config_file, |c| {
            let id = c
                .servers
                .first()
                .map(|s| s.id.clone())
                .unwrap_or_else(|| c.unique_server_id(&server_name_from_url(base_url)));
            *c = std::mem::take(c).after_login(&id, base_url, username);
        })
        .map_err(|e| BackendError(e.to_string()))?;
        Ok(first_queries(&config))
    }

    fn save_queries(&self, queries: &[SavedQuery]) -> Result<Vec<SavedQuery>, BackendError> {
        let config = Config::update(&self.config_file, |c| {
            if let Some(s) = c.servers.first_mut() {
                s.queries = queries
                    .iter()
                    .map(|q| QueryConfig {
                        name: q.name.clone(),
                        query: q.query.clone(),
                    })
                    .collect();
            }
        })
        .map_err(|e| BackendError(e.to_string()))?;
        Ok(first_queries(&config))
    }

    fn save_bookmarks(&self, bookmarks: &[SavedQuery]) -> Result<Vec<SavedQuery>, BackendError> {
        let config = Config::update(&self.config_file, |c| {
            if let Some(s) = c.servers.first_mut() {
                s.bookmarks = bookmarks
                    .iter()
                    .map(|q| QueryConfig {
                        name: q.name.clone(),
                        query: q.query.clone(),
                    })
                    .collect();
            }
        })
        .map_err(|e| BackendError(e.to_string()))?;
        Ok(queries_to_ui(
            config
                .servers
                .first()
                .map_or(&[][..], |s| s.bookmarks.as_slice()),
        ))
    }

    fn save_filters(&self, filters: &[FileFilter]) -> Result<Vec<FileFilter>, BackendError> {
        let config = Config::update(&self.config_file, |c| {
            // The rules the loader ignores stay in the file, after the ones
            // the UI manages; they are still ignored on the next load.
            let ignored = c.ignored_filters();
            // The UI does not know the `servers` restriction of a rule;
            // it stays with the rule of the same name.
            let restriction = |name: &str| {
                c.filters
                    .iter()
                    .find(|f| f.name.trim() == name)
                    .map(|f| f.servers.clone())
                    .unwrap_or_default()
            };
            let managed: Vec<FilterConfig> = filters
                .iter()
                .map(|f| FilterConfig {
                    servers: restriction(&f.name),
                    ..FilterConfig::from_filter(f)
                })
                .collect();
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
            keyring: Keyring,
        };
        let queries = b.save_login("https://g.example.com", None, None).unwrap();
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
            keyring: Keyring,
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
        assert_eq!(b.save_queries(&new).unwrap(), new);
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
            keyring: Keyring,
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
        assert_eq!(b.save_bookmarks(&new).unwrap(), new);
        let text = std::fs::read_to_string(&b.config_file).unwrap();
        assert!(text.contains("# mine") && text.contains("# prod"), "{text}");
        assert!(
            text.contains("[[servers.bookmarks]]\nname = \"Mine\""),
            "{text}"
        );
        assert!(text.contains("[[servers.queries]]\nname = \"A\""), "{text}");
        assert_eq!(b.save_bookmarks(&[]).unwrap(), []);
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
            keyring: Keyring,
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
            keyring: Keyring,
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
            keyring: Keyring,
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
}
