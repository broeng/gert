//! Wiring for the desktop client: configuration, secrets and the HTTP
//! transport. The `ui` crate does the rest.

pub mod backend;
pub mod config;
pub mod http;
pub mod secrets;

/// Single source of the (working) application name. Used for config and
/// cache directories, the window title and the keyring service name.
pub const APP_NAME: &str = "gert";
