//! Read-only, anonymous sanity checks against a public Gerrit server.
//! Ignored by default (needs network and `curl`):
//! `cargo test -p gerrit-api --test live -- --ignored --nocapture`

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::{process::Command, sync::Arc};

use futures::{FutureExt, executor::block_on, future::BoxFuture};
use gerrit_api::{Auth, ChangeQuery, GerritClient, HttpClient, HttpError};
use model::ChangeNumber;

/// GET-only HttpClient that shells out to curl.
struct Curl;

impl HttpClient for Curl {
    fn send(
        &self,
        req: http::Request<Vec<u8>>,
    ) -> BoxFuture<'static, Result<http::Response<Vec<u8>>, HttpError>> {
        let out = Command::new("curl")
            .args([
                "-s",
                "-w",
                "\n%{http_code}",
                "-H",
                "Accept: application/json",
            ])
            .arg(req.uri().to_string())
            .output();
        async move {
            let out = out.map_err(|e| HttpError(e.to_string()))?;
            let mut body = out.stdout;
            let nl = body.iter().rposition(|&b| b == b'\n').unwrap_or(0);
            let code: u16 = String::from_utf8_lossy(&body[nl + 1..])
                .trim()
                .parse()
                .map_err(|_| HttpError("bad curl output".into()))?;
            body.truncate(nl);
            http::Response::builder()
                .status(code)
                .body(body)
                .map_err(|e| HttpError(e.to_string()))
        }
        .boxed()
    }
}

fn client() -> GerritClient {
    GerritClient::new(
        Arc::new(Curl),
        "https://gerrit-review.googlesource.com",
        Auth::Anonymous,
    )
    .expect("valid url")
}

#[test]
#[ignore = "network"]
fn live_version_query_and_detail() {
    let c = client();
    println!("version {}", block_on(c.server_version()).expect("version"));
    let page =
        block_on(c.query_changes(&ChangeQuery::new("status:merged").limit(5))).expect("query");
    assert_eq!(page.changes.len(), 5);
    assert!(page.more_changes);
    for ch in &page.changes {
        println!(
            "{} {:?} {} {:?}",
            ch.number, ch.status, ch.subject, ch.labels
        );
    }
    let open =
        block_on(c.query_changes(&ChangeQuery::new("status:open").limit(10))).expect("open query");
    for ch in &open.changes {
        println!(
            "{} {} +{} -{} {:?}",
            ch.number,
            ch.owner.display(),
            ch.insertions,
            ch.deletions,
            ch.attention_set
        );
    }
    let d = block_on(c.get_change(ChangeNumber(page.changes[0].number.0))).expect("detail");
    println!("{:?}\n{:?}", d.current_revision, d.commit_message);
    assert!(d.commit_message.is_some());
}
