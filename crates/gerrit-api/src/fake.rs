//! A scripted [`HttpClient`] for tests. Available in this crate's own tests
//! and, via the `test-support` feature, in downstream crates.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard},
};

use futures::future::{BoxFuture, FutureExt};

use crate::{HttpClient, HttpError};

type Reply = Result<http::Response<Vec<u8>>, HttpError>;

#[derive(Default)]
struct State {
    replies: VecDeque<Reply>,
    requests: Vec<http::Request<Vec<u8>>>,
}

/// Replies with queued responses in order and records every request.
/// Cloning shares the same script and request log.
#[derive(Clone, Default)]
pub struct FakeHttpClient {
    state: Arc<Mutex<State>>,
}

impl FakeHttpClient {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A poisoned lock only means another test thread panicked.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues a response with the given status and body.
    pub fn push_response(&self, status: u16, body: impl Into<Vec<u8>>) -> &Self {
        let resp = http::Response::builder()
            .status(status)
            .body(body.into())
            .unwrap_or_default();
        self.lock().replies.push_back(Ok(resp));
        self
    }

    /// Queues a transport failure.
    pub fn push_error(&self, message: impl Into<String>) -> &Self {
        self.lock()
            .replies
            .push_back(Err(HttpError(message.into())));
        self
    }

    /// All requests seen so far.
    pub fn requests(&self) -> Vec<http::Request<Vec<u8>>> {
        self.lock().requests.clone()
    }
}

impl HttpClient for FakeHttpClient {
    fn send(&self, req: http::Request<Vec<u8>>) -> BoxFuture<'static, Reply> {
        let mut state = self.lock();
        state.requests.push(req);
        let reply = state
            .replies
            .pop_front()
            .unwrap_or_else(|| Err(HttpError("FakeHttpClient: no scripted reply".into())));
        async move { reply }.boxed()
    }
}

/// Needle, status and body of one route.
type Route = (String, u16, Vec<u8>);

/// A path-routed [`HttpClient`] for tests that issue several requests at
/// once (so a queue of replies would be order-sensitive) and for dev tools.
///
/// A request is answered by the route with the longest `needle` that occurs
/// in its path and query (`/changes/5?o=` ...); with no match the answer is
/// `404`. A needle that starts with an HTTP method and a space
/// (`"POST /changes/5/"`) only answers requests with that method. Cloning
/// shares the routes and the request log.
#[derive(Clone, Default)]
pub struct RouteHttpClient {
    routes: Arc<Mutex<Vec<Route>>>,
    requests: Arc<Mutex<Vec<String>>>,
    sent: Arc<Mutex<Vec<SentRequest>>>,
}

/// A request as the [`RouteHttpClient`] saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentRequest {
    pub method: String,
    /// Path and query.
    pub target: String,
    pub body: Vec<u8>,
}

impl SentRequest {
    /// The body as text (lossy).
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Splits `"POST /x"` into the method and the rest.
fn split_method(needle: &str) -> (Option<&str>, &str) {
    match needle.split_once(' ') {
        Some((m, rest)) if !m.is_empty() && m.bytes().all(|b| b.is_ascii_uppercase()) => {
            (Some(m), rest)
        }
        _ => (None, needle),
    }
}

impl RouteHttpClient {
    pub fn new() -> Self {
        Self::default()
    }

    /// Answers requests matching `needle` with `status` and `body`
    /// (replacing an earlier route with the same needle).
    pub fn route(&self, needle: &str, status: u16, body: impl Into<Vec<u8>>) -> &Self {
        let mut routes = self.routes.lock().unwrap_or_else(|e| e.into_inner());
        routes.retain(|(n, ..)| n != needle);
        routes.push((needle.to_owned(), status, body.into()));
        self
    }

    /// A `200` route whose body is `json` behind Gerrit's XSSI guard.
    pub fn route_json(&self, needle: &str, json: &str) -> &Self {
        self.route(needle, 200, format!(")]}}'\n{json}"))
    }

    /// Method, path, query and body of every request seen so far.
    pub fn sent(&self) -> Vec<SentRequest> {
        self.sent.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Path and query of every request seen so far.
    pub fn requested(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl HttpClient for RouteHttpClient {
    fn send(&self, req: http::Request<Vec<u8>>) -> BoxFuture<'static, Reply> {
        let target = req
            .uri()
            .path_and_query()
            .map_or_else(String::new, |p| p.as_str().to_owned());
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(target.clone());
        let method = req.method().as_str().to_owned();
        self.sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(SentRequest {
                method: method.clone(),
                target: target.clone(),
                body: req.body().clone(),
            });
        let (status, body) = self
            .routes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(needle, ..)| {
                let (m, path) = split_method(needle);
                m.is_none_or(|m| m == method) && target.contains(path)
            })
            .max_by_key(|(needle, ..)| {
                let (m, path) = split_method(needle);
                path.len() + usize::from(m.is_some())
            })
            .map_or((404, b"not found".to_vec()), |(_, s, b)| (*s, b.clone()));
        let resp = http::Response::builder()
            .status(status)
            .body(body)
            .unwrap_or_default();
        async move { Ok(resp) }.boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(client: &RouteHttpClient, path: &str) -> (u16, String) {
        let req = http::Request::builder()
            .uri(format!("https://g.example.com{path}"))
            .body(Vec::new())
            .expect("request");
        let resp = futures::executor::block_on(client.send(req)).expect("reply");
        (
            resp.status().as_u16(),
            String::from_utf8_lossy(resp.body()).into_owned(),
        )
    }

    #[test]
    fn longest_needle_wins_and_misses_are_404() {
        let c = RouteHttpClient::new();
        c.route("/changes/1", 200, "change")
            .route("/changes/1/revisions/a/files", 200, "files");
        assert_eq!(get(&c, "/changes/1?o=x"), (200, "change".into()));
        assert_eq!(
            get(&c, "/changes/1/revisions/a/files?base=2"),
            (200, "files".into())
        );
        assert_eq!(get(&c, "/changes/2").0, 404);
        assert_eq!(c.requested().len(), 3);
    }

    #[test]
    fn a_method_in_the_needle_restricts_the_route() {
        let c = RouteHttpClient::new();
        c.route("PUT /x", 204, "put").route("DELETE /x", 200, "del");
        let send = |m: &str| {
            let req = http::Request::builder()
                .method(m)
                .uri("https://g.example.com/x")
                .body(b"body".to_vec())
                .expect("request");
            let resp = futures::executor::block_on(c.send(req)).expect("reply");
            resp.status().as_u16()
        };
        assert_eq!((send("PUT"), send("DELETE"), send("GET")), (204, 200, 404));
        assert_eq!(c.sent()[0].method, "PUT");
        assert_eq!(c.sent()[0].body_text(), "body");
    }
}
