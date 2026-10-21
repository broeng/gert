use futures::future::BoxFuture;

/// Transport-level failure (DNS, TLS, connection reset, timeout, ...).
/// A response with an error status is *not* an `HttpError`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct HttpError(pub String);

/// Minimal async HTTP transport. Object-safe and runtime-agnostic: the
/// production implementation lives in `app`, tests use [`crate::fake`].
pub trait HttpClient: Send + Sync {
    fn send(
        &self,
        req: http::Request<Vec<u8>>,
    ) -> BoxFuture<'static, Result<http::Response<Vec<u8>>, HttpError>>;
}
