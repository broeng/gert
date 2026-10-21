//! Adapts Zed's reqwest-based client (`gpui-pre-reqwest-client`) to
//! [`gerrit_api::HttpClient`].
//!
//! The reqwest client runs its requests on a tokio runtime of its own, so
//! the futures returned here can be polled by any executor, including
//! GPUI's background executor.

use std::sync::Arc;

use futures::{AsyncReadExt as _, FutureExt as _, future::BoxFuture};
use gerrit_api::{HttpClient, HttpError};
use reqwest_client::ReqwestClient;

// The trait and body types come from the reqwest client's own dependency,
// which GPUI re-exports.
use gpui_kit::http_client::{AsyncBody, HttpClient as _};

use crate::APP_NAME;

pub struct GpuiHttpClient {
    inner: Arc<ReqwestClient>,
}

impl GpuiHttpClient {
    /// A client that identifies itself as `<APP_NAME>/<version>`.
    pub fn new() -> anyhow::Result<Self> {
        let agent = format!("{APP_NAME}/{}", env!("CARGO_PKG_VERSION"));
        Ok(Self {
            inner: Arc::new(ReqwestClient::user_agent(&agent)?),
        })
    }
}

impl HttpClient for GpuiHttpClient {
    fn send(
        &self,
        req: http::Request<Vec<u8>>,
    ) -> BoxFuture<'static, Result<http::Response<Vec<u8>>, HttpError>> {
        let fut = self.inner.send(req.map(AsyncBody::from));
        async move {
            let resp = fut.await.map_err(|e| HttpError(format!("{e:#}")))?;
            let (parts, mut body) = resp.into_parts();
            let mut bytes = Vec::new();
            body.read_to_end(&mut bytes)
                .await
                .map_err(|e| HttpError(format!("reading response body: {e}")))?;
            Ok(http::Response::from_parts(parts, bytes))
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_with_user_agent() {
        assert!(GpuiHttpClient::new().is_ok());
    }
}
