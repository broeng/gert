//! GPUI views. Built on gpui-kit; no I/O in render, all network and SQLite
//! work runs on the background executor.

mod actions;
pub mod diff;
mod labels;
mod list_model;
mod login;
mod nav;
mod pages;
mod row;
mod session;
mod settings;
mod shell;
mod theme;
mod time_fmt;
mod tracker;

#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

pub use login::{LoginError, LoginOutcome, LoginRequest};
pub use session::{Backend, BackendError, Env, LocalDiff, SavedQuery, ServerSetup, Startup};
pub use shell::{Shell, init};
