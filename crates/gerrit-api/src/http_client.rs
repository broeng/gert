use futures::future::BoxFuture;

/// Transport-level failure (DNS, TLS, connection reset, timeout, ...).
/// A response with an error status is *not* an `HttpError`.
///
/// The field holds everything the transport said, which for reqwest is a
/// whole chain (`error sending request for url (...): client error
/// (Connect): tcp connect error: Connection refused (os error 111)`). It
/// is for logs; `Display`, which ends up in the UI, shows only the
/// top-level message, see [`HttpError::short`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", self.short())]
pub struct HttpError(pub String);

impl HttpError {
    /// The first sentence of the chain without the request URL, e.g.
    /// `error sending request`. The URL is the user's own server and the
    /// rest is for the log.
    pub fn short(&self) -> String {
        let first = self.0.lines().next().unwrap_or_default();
        let top = first.split(": ").next().unwrap_or(first);
        let top = top.split(" for url").next().unwrap_or(top).trim();
        if top.is_empty() {
            self.0.trim().to_owned()
        } else {
            top.to_owned()
        }
    }
}

/// Minimal async HTTP transport. Object-safe and runtime-agnostic: the
/// production implementation lives in `app`, tests use [`crate::fake`].
pub trait HttpClient: Send + Sync {
    fn send(
        &self,
        req: http::Request<Vec<u8>>,
    ) -> BoxFuture<'static, Result<http::Response<Vec<u8>>, HttpError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_short_form_is_the_top_level_message_without_url_or_causes() {
        let e = HttpError(
            "error sending request for url (https://gerrit.example/a/changes/?q=x): \
             client error (Connect): tcp connect error: Connection refused (os error 111)"
                .into(),
        );
        assert_eq!(e.short(), "error sending request");
        assert_eq!(e.to_string(), "error sending request");
        assert!(e.0.contains("os error 111"));
        assert_eq!(
            HttpError("connection refused".into()).short(),
            "connection refused"
        );
        assert_eq!(HttpError("a\nsecond line".into()).short(), "a");
        assert_eq!(HttpError(String::new()).short(), "");
    }
}
