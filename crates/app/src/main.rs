use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use gert::{
    APP_NAME,
    backend::{AppBackend, queries_to_ui, startup_to_ui},
    config::{Config, DiffConfig, Paths},
    http::GpuiHttpClient,
    secrets::Keyring,
};
use gpui_kit::*;
use store::Store;
use ui::{
    ACTIVE_SERVER_STATE, ColorModes, Env, LocalDiff, ServerKey, ServerSetup, Shell, Startup,
    ThemeName,
};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if let Err(err) = run() {
        eprintln!("{APP_NAME}: {err:#}");
        std::process::exit(1);
    }
}

/// Everything that can fail before the window opens.
fn startup() -> anyhow::Result<(Startup, bool, ThemeName, ColorModes)> {
    let paths = Paths::discover()?;
    let config = Config::load(&paths.config_file)
        .context("reading the configuration")?
        .unwrap_or_default();
    let db = paths.cache_db()?;
    let mut store = Store::open(&db).with_context(|| format!("opening {}", db.display()))?;
    let mut servers = Vec::new();
    for s in &config.servers {
        servers.push(ServerSetup {
            key: ServerKey::new(&s.id),
            name: s.name.clone(),
            base_url: s.url.clone(),
            username: s.username.clone(),
            color: s.color.clone(),
            server: store.server_for_slug(&s.id, &s.url)?,
            queries: queries_to_ui(&s.queries),
            bookmarks: queries_to_ui(&s.bookmarks),
        });
    }
    // The server shown last time; the UI falls back to the first one if it
    // is no longer configured.
    let active = store
        .app_state(ACTIVE_SERVER_STATE)?
        .map(|slug| ServerKey::new(&slug))
        .filter(|key| servers.iter().any(|s| s.key == *key))
        .or_else(|| servers.first().map(|s| s.key.clone()));
    let http = GpuiHttpClient::new().context("creating the HTTP client")?;
    let advance_after_vote = config.advance_after_vote();
    let theme = config.theme();
    let color_modes = config.color_modes();
    let store = Arc::new(Mutex::new(store));
    let startup = Startup {
        env: Env {
            http: Arc::new(http),
            store: store.clone(),
            backend: Arc::new(AppBackend {
                config_file: paths.config_file,
                keyring: Arc::new(Keyring),
                store,
            }),
        },
        servers,
        active,
        filters: config.file_filters(),
        start_page: startup_to_ui(&config.startup_page()),
        diff: LocalDiff {
            preferences: config
                .diff
                .as_ref()
                .map(DiffConfig::preferences)
                .unwrap_or_default(),
            font_family: config.diff.and_then(|d| d.font_family),
        },
    };
    Ok((startup, advance_after_vote, theme, color_modes))
}

fn run() -> anyhow::Result<()> {
    // `gert <link>` opens the change a web UI link points at, like a
    // browser would.
    let link = std::env::args().nth(1);
    let (startup, advance_after_vote, theme, color_modes) = startup()?;
    ui::application().run(move |cx| {
        gpui_kit::init(cx);
        ui::init(cx);
        ui::set_theme(theme, cx);
        let options = ui::window_options(APP_NAME, APP_NAME, size(px(1200.), px(760.)), cx);
        let opened = gpui_kit::open_window(options, cx, |window, cx| {
            ui::set_color_modes(color_modes, window, cx);
            cx.new(|cx| {
                let mut shell = Shell::new(startup, window, cx)
                    .with_app_name(APP_NAME)
                    .with_advance_after_vote(advance_after_vote, cx);
                if let Some(link) = &link {
                    shell.open_link(link, window, cx);
                }
                shell
            })
        });
        match opened {
            Ok((window, _)) => {
                ui::ensure_maximized(window, cx);
                cx.on_window_closed(|cx, _| {
                    if cx.windows().is_empty() {
                        cx.quit();
                    }
                })
                .detach();
                cx.activate(true);
            }
            Err(err) => {
                log::error!("failed to open the window: {err:#}");
                cx.quit();
            }
        }
    });
    Ok(())
}
