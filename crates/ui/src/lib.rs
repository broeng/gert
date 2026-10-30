//! GPUI views. Built on gpui-kit; no I/O in render, all network and SQLite
//! work runs on the background executor.

mod action_menu;
mod actions;
mod attention;
mod card;
mod comments;
pub mod diff;
mod diff_load;
pub mod filters;
mod filters_dialog;
mod goto;
mod hint;
mod inbox;
mod labels;
mod links;
mod list_model;
mod login;
mod nav;
mod notifier;
mod notify_rule_dialog;
mod outbox;
mod pages;
mod reply;
mod reviewed;
mod row;
mod search_link;
mod servers;
mod session;
mod settings;
mod shell;
mod switcher;
mod syntax_cache;
mod theme;
mod time_fmt;
mod tracker;
mod watching;
mod window;

#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

pub use login::{LoginError, LoginOutcome, LoginRequest};
pub use notifier::{Clicked, Notifier};
pub use servers::{ACTIVE_SERVER_STATE, Connection};
pub use session::{
    Backend, BackendError, Env, LocalDiff, SavedQuery, ServerBackend, ServerConfig, ServerKey,
    ServerSetup, Startup, StartupPage, server_name_from_url,
};
pub use shell::{Shell, init};
pub use theme::{ColorMode, ColorModes, ThemeName, set as set_theme, set_color_modes};
pub use window::{application, ensure_maximized, window_options};
