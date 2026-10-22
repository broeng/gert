//! GPUI views. Built on gpui-kit; no I/O in render, all network and SQLite
//! work runs on the background executor.

mod action_menu;
mod actions;
mod card;
mod comments;
pub mod diff;
mod diff_load;
pub mod filters;
mod hint;
mod labels;
mod links;
mod list_model;
mod login;
mod nav;
mod outbox;
mod pages;
mod reply;
mod reviewed;
mod row;
mod session;
mod settings;
mod shell;
mod theme;
mod time_fmt;
mod tracker;
mod window;

#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

pub use login::{LoginError, LoginOutcome, LoginRequest};
pub use session::{
    Backend, BackendError, Env, LocalDiff, SavedQuery, ServerSetup, Startup, StartupPage,
};
pub use shell::{Shell, init};
pub use window::{application, ensure_maximized, window_options};
