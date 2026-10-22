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
    /// The top-level message plus the root cause, without the request URL,
    /// e.g. `error sending request: Connection refused (os error 111)` or
    /// `error sending request: invalid peer certificate: UnknownIssuer`.
    /// The URL is the user's own server and the layers in between
    /// (`client error (Connect)`, `tcp connect error`) say nothing a user
    /// can act on; the whole chain is for the log.
    pub fn short(&self) -> String {
        let first = self.0.lines().next().unwrap_or_default();
        let mut parts = first.split(": ");
        let top = parts.next().unwrap_or(first);
        let top = top.split(" for url").next().unwrap_or(top).trim();
        let causes: Vec<&str> = parts.collect();
        // Everything after the connector's own wrapper is the cause: one
        // io/TLS/DNS error, which may itself contain ": " (a rustls error
        // reads `invalid peer certificate: UnknownIssuer`).
        let from = causes
            .iter()
            .rposition(|c| c.starts_with("client error"))
            .map_or(0, |i| i + 1);
        let cause = causes[from..]
            .iter()
            .copied()
            .filter(|c| *c != "tcp connect error")
            .collect::<Vec<_>>()
            .join(": ");
        match (top.is_empty(), cause.is_empty()) {
            (true, _) => self.0.trim().to_owned(),
            (false, true) => top.to_owned(),
            (false, false) => format!("{top}: {cause}"),
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
    fn the_short_form_is_the_top_level_message_plus_the_root_cause() {
        let refused = HttpError(
            "error sending request for url (https://gerrit.example/a/changes/?q=x): \
             client error (Connect): tcp connect error: Connection refused (os error 111)"
                .into(),
        );
        assert_eq!(
            refused.short(),
            "error sending request: Connection refused (os error 111)"
        );
        assert_eq!(refused.to_string(), refused.short());
        assert!(refused.0.contains("https://gerrit.example"));
        assert!(!refused.short().contains("gerrit.example"));

        let dns = HttpError(
            "error sending request for url (https://x.example/): client error (Connect): \
             dns error: failed to lookup address information: Name or service not known"
                .into(),
        );
        assert_eq!(
            dns.short(),
            "error sending request: dns error: failed to lookup address information: \
             Name or service not known"
        );

        let tls = HttpError(
            "error sending request for url (https://x.example/): client error (Connect): \
             invalid peer certificate: UnknownIssuer"
                .into(),
        );
        assert_eq!(
            tls.short(),
            "error sending request: invalid peer certificate: UnknownIssuer"
        );
    }

    #[test]
    fn the_short_form_copes_with_plain_messages() {
        assert_eq!(
            HttpError("error sending request for url (https://x/)".into()).short(),
            "error sending request"
        );
        assert_eq!(
            HttpError("connection refused".into()).short(),
            "connection refused"
        );
        assert_eq!(HttpError("a\nsecond line".into()).short(), "a");
        assert_eq!(HttpError(String::new()).short(), "");
    }
}
