//! Dev-only: the whole UI against a fake Gerrit, served from the fixtures in
//! `tests/fixtures/example` and the diffs next to them. Nothing
//! touches the network, the keyring or the config file; the cache is in
//! memory. Sign-in is pre-arranged as the user `alice`.
//!
//! ```text
//! cargo run -p ui --example review [--dark|--light] [--unified] [--font NAME] [--files N] [--csd]
//! ```
//!
//! Open the first change with `j`, `Enter`. Requests that no route matches
//! answer 404, which the pages show as an error.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use futures::future::{BoxFuture, FutureExt as _};
use gerrit_api::{HttpClient, HttpError, fake::RouteHttpClient};
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::*;
use model::{DiffPreferences, DiffViewMode};
use store::Store;
use ui::{Backend, BackendError, Env, LocalDiff, SavedQuery, ServerSetup, Shell, Startup};

const URL: &str = "https://gerrit.example.com";

struct Demo;

impl Backend for Demo {
    fn load_password(&self, _: &str, _: &str) -> Result<Option<String>, BackendError> {
        Ok(Some("demo".into()))
    }

    fn save_login(
        &self,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<Vec<SavedQuery>, BackendError> {
        Err(BackendError("the demo has no login".into()))
    }
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

fn read(rel: &str) -> Vec<u8> {
    let path = fixtures().join(rel);
    std::fs::read(&path).unwrap_or_else(|e| {
        eprintln!("review: {}: {e}", path.display());
        std::process::exit(2);
    })
}

/// Routes: the longest matching fragment wins, so the per-file and
/// per-comparison routes sit on top of the general ones.
/// `n` generated files for the stress test (`--files N`): the example's
/// file list plus `n` modified files, every diff the same small fixture.
fn many_files(n: usize) -> Vec<u8> {
    let mut out = String::from(
        ")]}'\n{\n\"/COMMIT_MSG\": {\"lines_inserted\": 3, \"size\": 100, \"size_delta\": 10}",
    );
    for i in 0..n {
        out.push_str(&format!(
            ",\n\"src/generated/file_{i:05}.rs\": {{\"lines_inserted\": 5, \"lines_deleted\": 2, \"size\": 900, \"size_delta\": 30}}"
        ));
    }
    out.push_str("\n}\n");
    out.into_bytes()
}

/// `src/sched/queue.rs`: a 120-line file with changes at lines 40 and 90, so
/// that the default context leaves hidden runs and `context=N` / `ALL`
/// visibly change what is shown. Everything else is answered by `inner`.
struct ContextAware {
    inner: RouteHttpClient,
}

const QUEUE_DIFF: &str = "src%2Fsched%2Fqueue.rs/diff";
const QUEUE_LINES: usize = 120;
const QUEUE_CHANGES: [usize; 2] = [40, 90];

fn queue_diff(context: Option<usize>) -> Vec<u8> {
    use serde_json::{Value, json};
    let line = |n: usize| format!("    let value_{n} = compute({n});");
    let run = |from: usize, to: usize| (from..=to).map(line).collect::<Vec<_>>();
    let mut content: Vec<Value> = Vec::new();
    let mut next = 1;
    for (i, change) in QUEUE_CHANGES
        .iter()
        .copied()
        .chain([QUEUE_LINES + 1])
        .enumerate()
    {
        // Unchanged lines `next..change`, with `context` lines kept next
        // to the changes.
        let len = change - next;
        let first = i == 0;
        let last = change > QUEUE_LINES;
        let (head, tail) = match context {
            None => (len, 0),
            Some(c) if first => (0, c.min(len)),
            Some(c) if last => (c.min(len), 0),
            Some(c) if len > 2 * c => (c, c),
            Some(_) => (len, 0),
        };
        if head > 0 {
            content.push(json!({"ab": run(next, next + head - 1)}));
        }
        let hidden = len - head - tail;
        if hidden > 0 {
            content.push(json!({"skip": hidden}));
        }
        if tail > 0 {
            content.push(json!({"ab": run(change - tail, change - 1)}));
        }
        if !last {
            content.push(json!({
                "a": [line(change)],
                "b": [format!("    let value_{change} = compute_fast({change});")],
            }));
        }
        next = change + 1;
    }
    let meta =
        json!({"name": "src/sched/queue.rs", "content_type": "text/x-rust", "lines": QUEUE_LINES});
    let body =
        json!({"meta_a": meta, "meta_b": meta, "change_type": "MODIFIED", "content": content});
    format!(")]}}'\n{body}").into_bytes()
}

impl HttpClient for ContextAware {
    fn send(
        &self,
        req: http::Request<Vec<u8>>,
    ) -> BoxFuture<'static, Result<http::Response<Vec<u8>>, HttpError>> {
        let uri = req.uri().to_string();
        if let Some(at) = uri.find(QUEUE_DIFF) {
            let query = uri[at..].split_once('?').map_or("", |(_, q)| q);
            let context = query
                .split('&')
                .find_map(|p| p.strip_prefix("context="))
                .and_then(|c| c.parse::<usize>().ok());
            let resp = http::Response::builder()
                .status(200)
                .body(queue_diff(context))
                .unwrap_or_default();
            return async move { Ok(resp) }.boxed();
        }
        self.inner.send(req)
    }
}

fn server(files: Option<usize>) -> RouteHttpClient {
    let http = RouteHttpClient::new();
    let ex = |name: &str| read(&format!("example/{name}"));
    let hw = |name: &str| read(&format!("handwritten/{name}"));
    http.route("/accounts/self/detail", 200, ex("account.json"));
    http.route(
        "/accounts/self/preferences.diff",
        200,
        ex("diff_preferences.json"),
    );
    http.route(
        "/accounts/self/preferences",
        200,
        ex("general_preferences.json"),
    );
    http.route("/changes/?q=", 200, ex("query.json"));
    http.route("/changes/12345?", 200, ex("change.json"));
    http.route("/related", 200, ex("related.json"));
    let sha3 = "3".repeat(40);
    http.route(
        &format!("revisions/{sha3}/files"),
        200,
        files.map_or_else(|| ex("files_base_3.json"), many_files),
    );
    http.route(
        &format!("revisions/{sha3}/files?base=1"),
        200,
        ex("files_1_3.json"),
    );
    http.route(
        &format!("revisions/{sha3}/files?base=2"),
        200,
        ex("files_2_3.json"),
    );
    http.route(
        &format!("revisions/{sha3}/files?reviewed"),
        200,
        ex("files_reviewed.json"),
    );
    // Diffs: a few realistic ones for the files of the example change, the
    // 10k-line fixture for the biggest. The file routes must be longer than
    // the file-list route they share a prefix with.
    let diff = |file: &str| format!("revisions/{sha3}/files/{file}/diff");
    http.route(
        &format!("revisions/{sha3}/files/"),
        200,
        read("diff.response.json"),
    );
    http.route(
        &diff("%2FCOMMIT_MSG"),
        200,
        hw("diff_commit_msg.response.json"),
    );
    http.route(
        &diff("src%2Fsched%2Fheap.rs"),
        200,
        hw("diff_new_file.response.json"),
    );
    http.route(
        &diff("src%2Fsched%2Flist.rs"),
        200,
        hw("diff_deleted_file.response.json"),
    );
    http.route(
        &diff("src%2Fsched%2Ftasks.rs"),
        200,
        hw("diff_rename.response.json"),
    );
    http.route(
        &diff("docs%2Fqueue.png"),
        200,
        hw("diff_binary.response.json"),
    );
    http.route(
        &diff("tests%2Fordering.rs"),
        200,
        read("large_diff.response.json"),
    );
    http
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut mode = None;
    let mut local = DiffPreferences::default();
    let mut font = None;
    let mut files = None;
    let mut csd = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dark" => mode = Some(ThemeMode::Dark),
            "--light" => mode = Some(ThemeMode::Light),
            "--unified" => local.view_mode = Some(DiffViewMode::Unified),
            "--font" => font = args.next(),
            // Dev: ask for client-side decorations to see the title bar
            // buttons and the window frame on a compositor that draws its own.
            "--csd" => csd = true,
            "--files" => files = args.next().and_then(|n| n.parse().ok()),
            other => {
                eprintln!("review: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }

    let http = server(files);
    let mut store = Store::open_in_memory().unwrap_or_else(|e| {
        eprintln!("review: {e}");
        std::process::exit(2);
    });
    let server_id = store.server_id(URL).unwrap_or_else(|e| {
        eprintln!("review: {e}");
        std::process::exit(2);
    });
    let startup = Startup {
        env: Env {
            http: Arc::new(ContextAware { inner: http }),
            store: Arc::new(Mutex::new(store)),
            backend: Arc::new(Demo),
        },
        server: Some(ServerSetup {
            base_url: URL.into(),
            username: Some("alice".into()),
            server: server_id,
        }),
        queries: vec![SavedQuery {
            name: "Open changes".into(),
            query: "status:open".into(),
        }],
        diff: LocalDiff {
            preferences: local,
            font_family: font,
        },
    };

    ui::application().run(move |cx| {
        gpui_kit::init(cx);
        ui::init(cx);
        if let Some(m) = mode {
            Theme::change(m, None, cx);
        }
        let mut options =
            ui::window_options("review demo", "review-demo", size(px(1200.), px(800.)), cx);
        if csd {
            options.window_decorations = Some(WindowDecorations::Client);
        }
        let opened = gpui_kit::open_window(options, cx, |window, cx| {
            if mode.is_none() {
                Theme::sync_system_appearance(Some(window), cx);
            }
            cx.new(|cx| Shell::new(startup, window, cx).with_app_name("review demo"))
        });
        match opened {
            Ok(_) => {
                if let Some(m) = mode {
                    // The shell follows the system appearance as soon as the
                    // platform reports it; pin the requested one after that.
                    cx.spawn(async move |cx| {
                        for _ in 0..8 {
                            cx.background_executor()
                                .timer(std::time::Duration::from_millis(400))
                                .await;
                            cx.update(|cx| Theme::change(m, None, cx));
                        }
                    })
                    .detach();
                }
                cx.on_window_closed(|cx, _| {
                    if cx.windows().is_empty() {
                        cx.quit();
                    }
                })
                .detach();
                cx.activate(true);
            }
            Err(err) => {
                eprintln!("failed to open the window: {err:#}");
                cx.quit();
            }
        }
    });
}
