use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use gert::{
    APP_NAME,
    backend::{AppBackend, queries_to_ui, startup_to_ui},
    config::{Config, DiffConfig, Paths},
    http::GpuiHttpClient,
    secrets::Keyring,
};
use gpui_kit::component::Theme;
use gpui_kit::*;
use store::Store;
use ui::{Env, LocalDiff, ServerSetup, Shell, Startup};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if let Err(err) = run() {
        eprintln!("{APP_NAME}: {err:#}");
        std::process::exit(1);
    }
}

/// Everything that can fail before the window opens.
fn startup() -> anyhow::Result<(Startup, bool)> {
    let paths = Paths::discover()?;
    let config = Config::load(&paths.config_file)
        .context("reading the configuration")?
        .unwrap_or_default();
    let db = paths.cache_db()?;
    let mut store = Store::open(&db).with_context(|| format!("opening {}", db.display()))?;
    let server = match &config.server {
        Some(s) => Some(ServerSetup {
            server: store.server_id(&s.url)?,
            base_url: s.url.clone(),
            username: s.username.clone(),
        }),
        None => None,
    };
    let http = GpuiHttpClient::new().context("creating the HTTP client")?;
    let advance_after_vote = config.advance_after_vote();
    let startup = Startup {
        env: Env {
            http: Arc::new(http),
            store: Arc::new(Mutex::new(store)),
            backend: Arc::new(AppBackend {
                config_file: paths.config_file,
                keyring: Keyring,
            }),
        },
        server,
        queries: queries_to_ui(&config.queries),
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
    Ok((startup, advance_after_vote))
}

fn run() -> anyhow::Result<()> {
    let (startup, advance_after_vote) = startup()?;
    ui::application().run(move |cx| {
        gpui_kit::init(cx);
        ui::init(cx);
        let options = ui::window_options(APP_NAME, APP_NAME, size(px(1200.), px(760.)), cx);
        let opened = gpui_kit::open_window(options, cx, |window, cx| {
            Theme::sync_system_appearance(Some(window), cx);
            cx.new(|cx| {
                Shell::new(startup, window, cx)
                    .with_app_name(APP_NAME)
                    .with_advance_after_vote(advance_after_vote, cx)
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
