//! Dev-only: the whole UI against a fake Gerrit, served from the fixtures in
//! `tests/fixtures/example` and the diffs next to them. Nothing
//! touches the network, the keyring or the config file; the cache is in
//! memory. Sign-in is pre-arranged as the user `alice`.
//!
//! ```text
//! cargo run -p ui --example review [--dark|--light] [--unified] [--font NAME] [--files N] [--csd]
//!                                   [--drafts ok|fail|offline]
//! ```
//!
//! Open the first change with `j`, `Enter`. Requests that no route matches
//! answer 404, which the pages show as an error.
//!
//! Change 12345 has comments (including a patch-set level thread) and two
//! drafts of alice's. The fake server keeps drafts in memory: you can write,
//! edit and delete them, and they are listed again after a refresh.

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

/// `src/sched/queue.rs`: a 120-line file with changes at lines 40 and 90,
/// so that the default context folds three runs. Like the real Gerrit 3.x it
/// answers with the whole file whatever `context` says. Everything else is
/// answered by `inner`.
struct WholeFileDiffs {
    inner: RouteHttpClient,
}

const QUEUE_DIFF: &str = "src%2Fsched%2Fqueue.rs/diff";
const QUEUE_LINES: usize = 120;
const QUEUE_CHANGES: [usize; 2] = [40, 90];

fn queue_diff() -> Vec<u8> {
    use serde_json::{Value, json};
    let line = |n: usize| format!("    let value_{n} = compute({n});");
    let run = |from: usize, to: usize| (from..=to).map(line).collect::<Vec<_>>();
    let mut content: Vec<Value> = Vec::new();
    let mut next = 1;
    for change in QUEUE_CHANGES {
        content.push(json!({"ab": run(next, change - 1)}));
        content.push(json!({
            "a": [line(change)],
            "b": [format!("    let value_{change} = compute_fast({change});")],
        }));
        next = change + 1;
    }
    content.push(json!({"ab": run(next, QUEUE_LINES)}));
    let meta =
        json!({"name": "src/sched/queue.rs", "content_type": "text/x-rust", "lines": QUEUE_LINES});
    let body =
        json!({"meta_a": meta, "meta_b": meta, "change_type": "MODIFIED", "content": content});
    format!(")]}}'\n{body}").into_bytes()
}

impl HttpClient for WholeFileDiffs {
    fn send(
        &self,
        req: http::Request<Vec<u8>>,
    ) -> BoxFuture<'static, Result<http::Response<Vec<u8>>, HttpError>> {
        if req.uri().to_string().contains(QUEUE_DIFF) {
            let resp = http::Response::builder()
                .status(200)
                .body(queue_diff())
                .unwrap_or_default();
            return async move { Ok(resp) }.boxed();
        }
        self.inner.send(req)
    }
}

/// Published comments on change 12345, patch set 3: an unresolved thread on
/// a range with a reply, a resolved one in a quiet part of `queue.rs`, a
/// comment on the base side, a file comment, and one on the commit message.
fn comments() -> String {
    use serde_json::json;
    let bob = json!({"_account_id": 1000002, "name": "Bob Reviewer", "username": "bob"});
    let alice = json!({"_account_id": 1000001, "name": "Alice Example", "username": "alice"});
    let at = |t: &str| format!("2024-06-14 {t}.000000000");
    let body = json!({
        "src/sched/queue.rs": [
            {"id": "q1", "patch_set": 3, "line": 40, "author": bob,
             "range": {"start_line": 40, "start_character": 8, "end_line": 40, "end_character": 21},
             "message": "Why the rename?\n\nThe old name was fine and this makes the diff bigger.",
             "updated": at("09:00:00"), "unresolved": true},
            {"id": "q2", "patch_set": 3, "line": 40, "author": alice, "in_reply_to": "q1",
             "range": {"start_line": 40, "start_character": 8, "end_line": 40, "end_character": 21},
             "message": "It matches the metrics names added in the next change.",
             "updated": at("09:30:00"), "unresolved": true},
            {"id": "q3", "patch_set": 3, "line": 70, "author": bob,
             "message": "nit: could use a constant here", "updated": at("10:00:00"),
             "unresolved": true},
            {"id": "q4", "patch_set": 3, "line": 70, "author": alice, "in_reply_to": "q3",
             "message": "Done", "updated": at("11:00:00"), "unresolved": false},
            {"id": "q5", "patch_set": 3, "line": 90, "side": "PARENT", "author": bob,
             "message": "This used to be the slow path, is it still reachable?",
             "updated": at("12:00:00"), "unresolved": true},
            {"id": "q6", "patch_set": 3, "author": bob,
             "message": "Please split this file in a follow-up.", "updated": at("13:00:00"),
             "unresolved": false},
        ],
        "/COMMIT_MSG": [
            {"id": "m1", "patch_set": 3, "line": 1, "author": bob,
             "message": "Say what changed, not that it was reworked.",
             "updated": at("14:00:00"), "unresolved": true},
        ],
        // Patch-set level: no line, no side (the dev Gerrit answers so).
        "/PATCHSET_LEVEL": [
            {"id": "ps1", "patch_set": 3, "author": bob,
             "message": "Overall this looks good.\n\nPlease add a test for the empty queue.",
             "updated": at("15:00:00"), "unresolved": true},
            {"id": "ps2", "patch_set": 3, "author": alice, "in_reply_to": "ps1",
             "message": "Will do, in the next patch set.", "updated": at("15:30:00"),
             "unresolved": true},
            {"id": "ps3", "patch_set": 2, "author": bob,
             "message": "Thanks for splitting this up.", "updated": at("08:00:00"),
             "unresolved": false},
        ],
    });
    format!(")]}}'\n{body}")
}

/// How the fake server treats draft writes (`--drafts`): it accepts them,
/// refuses them (400), or is unreachable (503).
#[derive(Clone, Copy, PartialEq)]
enum Drafts {
    Ok,
    Fail,
    Offline,
}

/// The user's drafts on change 12345, kept in memory like the real server
/// keeps them: `PUT .../drafts` creates one with a new id (201), `PUT
/// .../drafts/{id}` replaces its text, `DELETE` removes it (204), and
/// `GET /changes/12345/drafts` lists what is there, by path. Everything else
/// goes to `inner`.
struct DraftServer {
    inner: WholeFileDiffs,
    mode: Drafts,
    state: Mutex<DraftState>,
}

struct DraftState {
    next: u32,
    /// Drafts in creation order: the id and the `CommentInfo`.
    drafts: Vec<(String, serde_json::Value)>,
}

impl DraftServer {
    fn new(inner: WholeFileDiffs, mode: Drafts) -> Self {
        // Drafts of alice's, shaped like the ones `dev/seed.sh` leaves on
        // change 3: a reply in a thread, and one on a file.
        let seed = |id: &str, body: serde_json::Value| {
            let mut c = body;
            c["id"] = id.into();
            c["patch_set"] = 3.into();
            c["updated"] = "2024-06-15 09:00:00.000000000".into();
            (id.to_owned(), c)
        };
        let drafts = vec![
            seed(
                "draft-seed-1",
                serde_json::json!({"path": "src/sched/queue.rs", "line": 90, "side": "PARENT",
                    "in_reply_to": "q5", "message": "Still reachable from the old scheduler.",
                    "unresolved": false}),
            ),
            seed(
                "draft-seed-2",
                serde_json::json!({"path": "src/sched/queue.rs",
                    "message": "File-level draft: rename this module?", "unresolved": true}),
            ),
        ];
        Self {
            inner,
            mode,
            state: Mutex::new(DraftState { next: 1, drafts }),
        }
    }

    fn reply(
        status: u16,
        body: Vec<u8>,
    ) -> BoxFuture<'static, Result<http::Response<Vec<u8>>, HttpError>> {
        let resp = http::Response::builder()
            .status(status)
            .body(body)
            .unwrap_or_default();
        async move { Ok(resp) }.boxed()
    }

    fn json(
        status: u16,
        value: &serde_json::Value,
    ) -> BoxFuture<'static, Result<http::Response<Vec<u8>>, HttpError>> {
        Self::reply(status, format!(")]}}'\n{value}").into_bytes())
    }

    /// Without the patch set: the server's answer to a write leaves it out.
    fn receipt(c: &serde_json::Value) -> serde_json::Value {
        let mut c = c.clone();
        if let Some(o) = c.as_object_mut() {
            o.remove("patch_set");
        }
        c
    }
}

impl HttpClient for DraftServer {
    fn send(
        &self,
        req: http::Request<Vec<u8>>,
    ) -> BoxFuture<'static, Result<http::Response<Vec<u8>>, HttpError>> {
        use serde_json::{Value, json};
        let path = req.uri().path().to_owned();
        let Some((head, tail)) = path
            .split_once("/changes/12345/")
            .and_then(|(_, rest)| rest.split_once("drafts"))
        else {
            return self.inner.send(req);
        };
        let method = req.method().clone();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // The list: `GET /changes/12345/drafts`.
        if method == http::Method::GET {
            let mut by_path = serde_json::Map::new();
            for (_, c) in &state.drafts {
                let key = c["path"].as_str().unwrap_or_default().to_owned();
                if let Value::Array(list) = by_path.entry(key).or_insert_with(|| json!([])) {
                    list.push(c.clone());
                }
            }
            return Self::json(200, &Value::Object(by_path));
        }
        match self.mode {
            Drafts::Ok => {}
            Drafts::Fail => return Self::reply(400, b"This comment is not welcome here".to_vec()),
            Drafts::Offline => return Self::reply(503, b"unavailable".to_vec()),
        }
        let id = tail.trim_matches('/').to_owned();
        let at = state.drafts.iter().position(|(d, _)| *d == id);
        if method == http::Method::DELETE {
            return match at {
                Some(i) => {
                    state.drafts.remove(i);
                    Self::reply(204, Vec::new())
                }
                None => Self::reply(404, b"Not found".to_vec()),
            };
        }
        let body: Value = serde_json::from_slice(req.body()).unwrap_or(Value::Null);
        // The patch set is the digit the example's revision SHAs repeat.
        let patch_set = head
            .rsplit('/')
            .find(|s| s.len() == 40)
            .and_then(|sha| sha.chars().next()?.to_digit(10))
            .unwrap_or(3);
        let tick = state.next;
        let updated = format!(
            "2024-06-15 10:{:02}:{:02}.000000000",
            tick / 60 % 60,
            tick % 60
        );
        if id.is_empty() {
            let id = format!("draft-{tick}");
            state.next += 1;
            let mut c = body;
            c["id"] = id.clone().into();
            c["patch_set"] = patch_set.into();
            c["updated"] = updated.into();
            state.drafts.push((id, c.clone()));
            return Self::json(201, &Self::receipt(&c));
        }
        match at {
            Some(i) => {
                state.next += 1;
                let c = &mut state.drafts[i].1;
                for key in ["message", "unresolved"] {
                    if let Some(v) = body.get(key) {
                        c[key] = v.clone();
                    }
                }
                c["updated"] = updated.into();
                let c = c.clone();
                Self::json(200, &Self::receipt(&c))
            }
            None => Self::reply(404, b"Not found".to_vec()),
        }
    }
}

fn server(files: Option<usize>) -> RouteHttpClient {
    let http = RouteHttpClient::new();
    http.route("/changes/12345/comments", 200, comments());
    let sha3 = "3".repeat(40);
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
    let mut drafts = Drafts::Ok;
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
            "--drafts" => {
                drafts = match args.next().as_deref() {
                    Some("fail") => Drafts::Fail,
                    Some("offline") => Drafts::Offline,
                    _ => Drafts::Ok,
                }
            }
            other => {
                eprintln!("review: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }

    let http = DraftServer::new(
        WholeFileDiffs {
            inner: server(files),
        },
        drafts,
    );
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
            http: Arc::new(http),
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
