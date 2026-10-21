use crate::{ConversionError, HttpError};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("transport error: {0}")]
    Http(#[from] HttpError),
    #[error("server returned HTTP {code}: {body}")]
    Status {
        code: u16,
        /// Start of the response body, for diagnostics.
        body: String,
    },
    /// 401 or 403: bad credentials, or anonymous access not allowed.
    #[error("not authorized")]
    Unauthorized,
    #[error("not found")]
    NotFound,
    #[error("invalid response body: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("unexpected response contents: {0}")]
    Conversion(#[from] ConversionError),
    #[error("invalid server URL: {0}")]
    InvalidUrl(String),
}
