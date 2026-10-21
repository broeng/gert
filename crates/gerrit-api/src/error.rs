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
    /// 401: missing or wrong credentials. The user has to log in (again).
    /// Anonymous sessions get this for everything that needs an account,
    /// including writes, without a request being made.
    #[error("not authorized")]
    Unauthorized,
    /// 403: the credentials are fine, but the account may not do this.
    /// Logging in again does not help.
    #[error("not allowed: {message}")]
    Forbidden {
        /// The server's explanation (a plain-text body), possibly empty.
        message: String,
    },
    #[error("not found")]
    NotFound,
    /// 400: the server rejected the request, e.g. a label value that is not
    /// permitted or a comment on a file that does not exist.
    #[error("rejected by the server: {message}")]
    BadRequest {
        /// The server's explanation (a plain-text body).
        message: String,
    },
    /// 409: the request conflicts with the state of the change, e.g. a
    /// draft on a patch set that is gone, or a change that is closed.
    #[error("conflict: {message}")]
    Conflict {
        /// The server's explanation (a plain-text body).
        message: String,
    },
    #[error("invalid response body: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("unexpected response contents: {0}")]
    Conversion(#[from] ConversionError),
    #[error("invalid server URL: {0}")]
    InvalidUrl(String),
}

impl Error {
    /// Whether repeating the same request later may succeed: the network
    /// failed, or the server was overloaded or unreachable behind a proxy
    /// (408, 429, 502, 503, 504). Everything else is an answer from the
    /// server about this very request and would be answered the same way
    /// again, so an offline outbox must not retry it.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Http(_) => true,
            Self::Status { code, .. } => matches!(code, 408 | 429 | 502 | 503 | 504),
            _ => false,
        }
    }

    /// Whether the credentials were refused (401); the one case that should
    /// send the user to the login page.
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Unauthorized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_transport_and_transient_failures_are_retryable() {
        let status = |code| Error::Status {
            code,
            body: String::new(),
        };
        assert!(Error::Http(HttpError("reset".into())).is_retryable());
        for code in [408, 429, 502, 503, 504] {
            assert!(status(code).is_retryable(), "{code}");
        }
        assert!(!status(500).is_retryable());
        assert!(!status(422).is_retryable());
        for e in [
            Error::Unauthorized,
            Error::NotFound,
            Error::Forbidden {
                message: "x".into(),
            },
            Error::BadRequest {
                message: "x".into(),
            },
            Error::Conflict {
                message: "x".into(),
            },
            Error::InvalidUrl("x".into()),
        ] {
            assert!(!e.is_retryable(), "{e}");
        }
    }
}
