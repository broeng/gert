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

    /// Opt-in: the production client sends methods and JSON bodies as the
    /// write calls need (POST with a body, PUT with a body, DELETE). Only
    /// against a local dev Gerrit, see `dev/README.md`:
    /// `GERT_TEST_GERRIT=http://127.0.0.1:8080 cargo test -p gert -- --ignored`
    #[test]
    #[ignore = "needs GERT_TEST_GERRIT"]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn writes_reach_a_local_gerrit() {
        use gerrit_api::{Auth, GerritClient};
        use model::{DraftInput, Location, Side};

        let url = std::env::var("GERT_TEST_GERRIT").expect("GERT_TEST_GERRIT");
        let authority = url
            .strip_prefix("http://")
            .and_then(|r| r.split('/').next())
            .unwrap_or("");
        let (host, port) = authority.split_once(':').unwrap_or((authority, ""));
        assert!(
            matches!(host, "127.0.0.1" | "localhost")
                && !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit()),
            "refusing to write to {url:?}: only http://127.0.0.1:PORT or http://localhost:PORT"
        );
        let http = std::sync::Arc::new(GpuiHttpClient::new().unwrap());
        let alice = || Auth::Basic {
            username: "alice".into(),
            password: "alice-dev-password".into(),
        };
        // POST with a JSON body: a throwaway change.
        let body = br#"{"project":"demo/scheduler","branch":"main","status":"NEW","subject":"Dev test http adapter"}"#;
        let token = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode("alice:alice-dev-password")
        };
        let req = http::Request::builder()
            .method("POST")
            .uri(format!("{url}/a/changes/"))
            .header("authorization", format!("Basic {token}"))
            .header("content-type", "application/json")
            .body(body.to_vec())
            .unwrap();
        let resp = futures::executor::block_on(http.send(req)).unwrap();
        assert_eq!(
            resp.status(),
            201,
            "{}",
            String::from_utf8_lossy(resp.body())
        );
        let text = String::from_utf8_lossy(resp.body()).into_owned();
        let info: serde_json::Value =
            serde_json::from_str(text.trim_start_matches(")]}'")).unwrap();
        let n = model::ChangeNumber(info["_number"].as_u64().unwrap());

        // PUT with a body, then DELETE, through the typed client.
        let client = GerritClient::new(http, &url, alice()).unwrap();
        let rev = model::RevisionId::from("current");
        let draft = DraftInput {
            path: "/PATCHSET_LEVEL".into(),
            side: Side::Revision,
            parent: None,
            location: Location::File,
            in_reply_to: None,
            message: "from the app's http client".into(),
            unresolved: false,
        };
        let receipt = futures::executor::block_on(client.create_draft(n, &rev, &draft)).unwrap();
        let drafts = futures::executor::block_on(client.get_drafts(n)).unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].message, draft.message);
        futures::executor::block_on(client.delete_draft(n, &rev, &receipt.id)).unwrap();
        assert!(
            futures::executor::block_on(client.get_drafts(n))
                .unwrap()
                .is_empty()
        );
    }
}
