//! Async Gerrit REST client. GPUI-free and runtime-agnostic.
//!
//! Responses are deserialized with the `gerrit_client` SDK models and
//! converted to [`model`] types at the crate boundary; SDK types never appear
//! in this crate's public API.

mod auth;
mod client;
mod convert;
mod error;
#[cfg(any(test, feature = "test-support"))]
pub mod fake;
mod http_client;
mod query;
mod wire;

pub use auth::Auth;
pub use client::{GerritClient, QueryPage};
pub use convert::ConversionError;
pub use error::Error;
pub use http_client::{HttpClient, HttpError};
pub use query::{ChangeOption, ChangeQuery};
