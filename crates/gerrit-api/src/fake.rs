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
