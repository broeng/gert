//! Dev-only: the whole UI against a fake Gerrit, served from the fixtures in
//! `tests/fixtures/example` and the diffs next to them. Nothing
//! touches the network, the keyring or the config file; the cache is in
//! memory. Sign-in is pre-arranged as the user `alice`.
//!
//! ```text
//! cargo run -p ui --example review [--dark|--light] [--unified] [--font NAME] [--files N] [--changes N] [--chain N] [--csd]
//!                                   [--drafts ok|fail|offline]
//! ```
//!
//! Open the first change with `j`, `Enter`. Requests that no route matches
//! answer 404, which the pages show as an error.
//!
//! Change 12345 has comments (including a patch-set level thread) and two
//! drafts of alice's. The fake server keeps drafts in memory: you can write,
//! edit and delete them, and they are listed again after a refresh.
//!
//! It also takes a reply (`a`): `POST .../review` applies the votes to the
//! change (alice may vote on Code-Review and Verified), publishes the drafts
//! as comments, adds a change message and moves the attention set; the
//! reviewed marks (`r`) are kept too. With `--drafts fail` or `offline` the
//! review is refused (403) or not reachable (503), as the draft writes are.
//!
//! The change actions menu (`m`) works too. The change carries `actions`
//! for alice: submit (the server answers 409 until Code-Review is +2),
//! rebase (it succeeds once and is greyed out afterwards), abandon and
//! restore, WIP and ready, private and not private. Each POST changes the
//! change and adds a message to its log; `--drafts fail` refuses them (403)
//! and `offline` cannot reach the server (503).
//!
//! Reviewers (`v` in the menu), topic (`t`) and hashtags (`h`) work too: the
//! autocomplete answers with the example people (matching name, username or
//! email) and one large group, "Scheduler Team" (type `team`), that asks for
//! confirmation; adding, moving and removing people, setting or clearing the
//! topic and editing hashtags change the change as the page sees it.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use futures::future::{BoxFuture, FutureExt as _};
use gerrit_api::{HttpClient, HttpError, fake::RouteHttpClient};
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::*;
use model::{DiffPreferences, DiffViewMode};
use store::Store;
use ui::filters::FileFilter;
use ui::{
    Backend, BackendError, Env, LocalDiff, SavedQuery, ServerSetup, Shell, Startup, StartupPage,
};

const URL: &str = "https://gerrit.example.com";

struct Demo;

impl Backend for Demo {
    fn load_password(&self, _: &str, _: &str) -> Result<Option<String>, BackendError> {
        Ok(Some("demo".into()))
    }

    fn save_queries(&self, _: &[SavedQuery]) -> Result<Vec<SavedQuery>, BackendError> {
        Err(BackendError("the demo has no settings".into()))
    }

    fn save_filters(&self, f: &[FileFilter]) -> Result<Vec<FileFilter>, BackendError> {
        Ok(f.to_vec())
    }

    fn save_startup_page(&self, p: &StartupPage) -> Result<StartupPage, BackendError> {
        Ok(p.clone())
    }

    fn save_advance_after_vote(&self, on: bool) -> Result<bool, BackendError> {
        Ok(on)
    }

    fn save_theme(&self, t: ui::ThemeName) -> Result<ui::ThemeName, BackendError> {
        Ok(t)
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

/// The query result with `n` changes: the fixture's three, then copies with
/// new numbers (`--changes N`), to fill and overflow the window.
fn many_changes(n: usize) -> Vec<u8> {
    let body = read("example/query.json");
    let text = String::from_utf8_lossy(&body);
    let Ok(serde_json::Value::Array(base)) =
        serde_json::from_str(text.trim_start_matches(")]}'")).map_err(|_| ())
    else {
        return body;
    };
    let mut out = base.clone();
    for i in base.len()..n {
        let mut c = base[i % base.len()].clone();
        c["_number"] = serde_json::json!(20000 + i);
        c["subject"] = serde_json::json!(format!("Generated change {i}"));
        out.push(c);
    }
    format!(")]}}'\n{}", serde_json::Value::Array(out)).into_bytes()
}

/// The related list with `n` generated entries on top of the fixture's, so
/// the box in the change header overflows (`--chain N`).
fn long_chain(n: usize) -> Vec<u8> {
    let body = read("example/related.json");
    let text = String::from_utf8_lossy(&body);
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(text.trim_start_matches(")]}'"))
    else {
        return body;
    };
    if let Some(list) = v["changes"].as_array_mut() {
        for i in 0..n {
            list.push(serde_json::json!({
                "project": "platform/scheduler",
                "change_id": format!("I{:040}", 30000 + i),
                "commit": {"commit": format!("{:040}", 30000 + i), "subject": format!("Generated ancestor {i}")},
                "_change_number": 30000 + i,
                "_revision_number": 1,
                "_current_revision_number": 1,
                "status": "MERGED"
            }));
        }
    }
    format!(")]}}'\n{v}").into_bytes()
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
    /// `change.json`, which replies change.
    change: serde_json::Value,
    mode: Drafts,
    state: Mutex<DraftState>,
}

struct DraftState {
    next: u32,
    /// Drafts in creation order: the id and the `CommentInfo`.
    drafts: Vec<(String, serde_json::Value)>,
    /// Comments that alice published with a reply, with their path.
    published: Vec<(String, serde_json::Value)>,
    /// Alice's votes after a reply.
    votes: BTreeMap<String, i64>,
    /// The attention set: account id, name, reason. Alice and carol to
    /// begin with, so that a reply has something to change.
    attention: Vec<(u64, String, String)>,
    /// Change messages added by replies.
    messages: Vec<serde_json::Value>,
    /// Files of patch set 3 that alice marked reviewed.
    reviewed: BTreeSet<String>,
    /// The change as alice's actions left it.
    status: &'static str,
    wip: bool,
    private: bool,
    rebased: bool,
    reviewers: Vec<u64>,
    ccs: Vec<u64>,
    topic: Option<String>,
    hashtags: Vec<String>,
}

const ALICE: u64 = 1_000_001;

/// The people of the example change: id, name and username.
const PEOPLE: [(u64, &str, &str); 7] = [
    (1_000_001, "Alice Example", "alice"),
    (1_000_002, "Bob Reviewer", "bob"),
    (1_000_003, "Carol Maintainer", "carol"),
    (1_000_004, "Dave CC", "dave"),
    (1_000_005, "Erin Engineer", "erin"),
    (1_000_006, "Frank Fixer", "frank"),
    (1_000_007, "Grace Gardner", "grace"),
];

/// The large group of the autocomplete, and who is in it (for the example).
const GROUP: (&str, &str) = ("grp-scheduler", "Scheduler Team");
const GROUP_MEMBERS: [u64; 3] = [1_000_005, 1_000_006, 1_000_007];

fn person(id: u64) -> serde_json::Value {
    let (_, name, user) = PEOPLE
        .iter()
        .copied()
        .find(|(i, ..)| *i == id)
        .unwrap_or((id, "Someone", "someone"));
    serde_json::json!({"_account_id": id, "name": name, "username": user,
        "email": format!("{user}@example.com")})
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
        let attention = vec![
            (ALICE, "Alice Example".to_owned(), "Bob replied".to_owned()),
            (
                1_000_003,
                "Carol Maintainer".to_owned(),
                "Review requested".to_owned(),
            ),
        ];
        let change = String::from_utf8(read("example/change.json"))
            .ok()
            .and_then(|t| serde_json::from_str(t.trim_start_matches(")]}'\n")).ok())
            .unwrap_or_else(|| {
                eprintln!("review: example/change.json is not JSON");
                std::process::exit(2);
            });
        Self {
            inner,
            change,
            mode,
            state: Mutex::new(DraftState {
                next: 1,
                drafts,
                published: Vec::new(),
                votes: BTreeMap::new(),
                attention,
                messages: Vec::new(),
                reviewed: ["src/sched/heap.rs", "src/lib.rs"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                status: "NEW",
                wip: false,
                private: false,
                rebased: false,
                reviewers: vec![1_000_002, 1_000_003],
                ccs: vec![1_000_004],
                topic: Some("sched-heap".to_owned()),
                hashtags: vec!["performance".to_owned(), "cleanup".to_owned()],
            }),
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

/// The ids of the example change, patch set 3's revision.
const REVIEW_PATH: &str = "/revisions/3333333333333333333333333333333333333333/review";

impl DraftServer {
    /// Everything about the change itself that depends on what alice did:
    /// its detail, the comments, the review, the reviewed marks. `None` for
    /// requests that are not about those.
    fn other(
        &self,
        req: &http::Request<Vec<u8>>,
        path: &str,
    ) -> Option<BoxFuture<'static, Result<http::Response<Vec<u8>>, HttpError>>> {
        use serde_json::{Value, json};
        let method = req.method().clone();
        let query = req.uri().query().unwrap_or_default().to_owned();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());

        if method == http::Method::GET && path.ends_with("/changes/12345") {
            let mut c = self.change.clone();
            Self::apply_to_change(&mut c, &state);
            return Some(Self::json(200, &c));
        }
        if method == http::Method::GET && path.ends_with("/changes/12345/comments") {
            let mut by_path: serde_json::Map<String, Value> =
                serde_json::from_str(comments().trim_start_matches(")]}'\n")).ok()?;
            for (p, c) in &state.published {
                if let Value::Array(list) = by_path.entry(p.clone()).or_insert_with(|| json!([])) {
                    list.push(c.clone());
                }
            }
            return Some(Self::json(200, &Value::Object(by_path)));
        }
        if path.contains("/revisions/3333333333333333333333333333333333333333/files") {
            if method == http::Method::GET && query == "reviewed" {
                let list: Vec<_> = state.reviewed.iter().cloned().collect();
                return Some(Self::json(200, &json!(list)));
            }
            if let Some((_, rest)) = path.split_once("/files/")
                && let Some(file) = rest.strip_suffix("/reviewed")
            {
                let file = file.replace("%2F", "/");
                return Some(match (&self.mode, &method) {
                    (Drafts::Offline, _) => Self::reply(503, b"unavailable".to_vec()),
                    (Drafts::Fail, _) => Self::reply(403, b"not allowed".to_vec()),
                    (_, &http::Method::PUT) => {
                        state.reviewed.insert(file);
                        Self::reply(201, Vec::new())
                    }
                    _ => {
                        state.reviewed.remove(&file);
                        Self::reply(204, Vec::new())
                    }
                });
            }
        }
        if method == http::Method::POST && path.ends_with(REVIEW_PATH) {
            return Some(match self.mode {
                Drafts::Offline => Self::reply(503, b"unavailable".to_vec()),
                Drafts::Fail => Self::reply(403, b"Vote on Code-Review is not permitted".to_vec()),
                Drafts::Ok => {
                    let body: Value = serde_json::from_slice(req.body()).unwrap_or(Value::Null);
                    let result = Self::post_review(&mut state, &body);
                    Self::json(200, &result)
                }
            });
        }
        // The change actions: `POST /changes/12345/{submit,rebase,...}`.
        if let Some(action) = path.strip_prefix("/a/changes/12345/")
            && matches!(
                action,
                "submit" | "rebase" | "abandon" | "restore" | "wip" | "ready" | "private"
            )
            && matches!(method, http::Method::POST | http::Method::DELETE)
        {
            return Some(match self.mode {
                Drafts::Offline => Self::reply(503, b"unavailable".to_vec()),
                Drafts::Fail => Self::reply(403, b"not allowed".to_vec()),
                Drafts::Ok => {
                    let body: Value = serde_json::from_slice(req.body()).unwrap_or(Value::Null);
                    let delete = method == http::Method::DELETE;
                    match Self::post_action(&mut state, action, delete, &body) {
                        Ok(()) => {
                            let mut c = self.change.clone();
                            Self::apply_to_change(&mut c, &state);
                            // The three that answer with a body send the
                            // change; the rest answer with nothing.
                            if matches!(action, "submit" | "rebase" | "abandon" | "restore") {
                                Self::json(200, &c)
                            } else {
                                Self::reply(200, Vec::new())
                            }
                        }
                        Err(message) => Self::reply(409, message.into_bytes()),
                    }
                }
            });
        }
        self.edit(req, path, &mut state)
    }

    /// Reviewers, topic and hashtags: `suggest_reviewers`, `POST/DELETE
    /// .../reviewers`, `PUT/DELETE .../topic`, `POST .../hashtags`.
    fn edit(
        &self,
        req: &http::Request<Vec<u8>>,
        path: &str,
        state: &mut DraftState,
    ) -> Option<BoxFuture<'static, Result<http::Response<Vec<u8>>, HttpError>>> {
        use serde_json::{Value, json};
        let rest = path.strip_prefix("/a/changes/12345/")?;
        let method = req.method().clone();
        let body: Value = serde_json::from_slice(req.body()).unwrap_or(Value::Null);
        let is_write = method != http::Method::GET;
        if is_write && matches!(self.mode, Drafts::Offline) {
            return Some(Self::reply(503, b"unavailable".to_vec()));
        }
        if is_write && matches!(self.mode, Drafts::Fail) {
            return Some(Self::reply(403, b"not allowed".to_vec()));
        }
        if method == http::Method::GET && rest == "suggest_reviewers" {
            let q = req
                .uri()
                .query()
                .unwrap_or_default()
                .split('&')
                .find_map(|kv| kv.strip_prefix("q="))
                .unwrap_or_default()
                .replace("%20", " ")
                .to_lowercase();
            let mut out: Vec<Value> = PEOPLE
                .iter()
                .filter(|(id, name, user)| {
                    *id != ALICE
                        && (name.to_lowercase().contains(&q)
                            || user.contains(&q)
                            || format!("{user}@example.com").contains(&q))
                })
                .map(|(id, ..)| json!({"account": person(*id), "count": 1}))
                .collect();
            if !q.is_empty() && GROUP.1.to_lowercase().contains(&q) {
                out.push(json!({"group": {"id": GROUP.0, "name": GROUP.1},
                    "count": 150, "confirm": true}));
            }
            return Some(Self::json(200, &Value::Array(out)));
        }
        if method == http::Method::POST && rest == "reviewers" {
            let input = body["reviewer"].as_str().unwrap_or_default().to_owned();
            let cc = body["state"] == "CC";
            let ids: Vec<u64> = if input == GROUP.0 {
                if body["confirmed"] != true {
                    return Some(Self::json(
                        200,
                        &json!({"input": input, "confirm": true, "error":
                            format!("{} has 150 members. Are you sure you want to add them all?",
                                GROUP.1)}),
                    ));
                }
                GROUP_MEMBERS.to_vec()
            } else {
                let found = PEOPLE.iter().find(|(id, _, user)| {
                    id.to_string() == input
                        || *user == input
                        || format!("{user}@example.com") == input
                });
                match found {
                    Some((id, ..)) => vec![*id],
                    None => {
                        return Some(Self::json(
                            200,
                            &json!({"input": input, "error":
                                format!("\"{input}\" is not a valid user or group")}),
                        ));
                    }
                }
            };
            for id in &ids {
                state.reviewers.retain(|r| r != id);
                state.ccs.retain(|r| r != id);
                if cc {
                    state.ccs.push(*id);
                } else {
                    state.reviewers.push(*id);
                }
            }
            let added: Vec<Value> = ids.iter().map(|i| person(*i)).collect();
            let key = if cc { "ccs" } else { "reviewers" };
            return Some(Self::json(200, &json!({"input": input, key: added})));
        }
        if method == http::Method::DELETE
            && let Some(who) = rest.strip_prefix("reviewers/")
        {
            let before = state.reviewers.len() + state.ccs.len();
            state.reviewers.retain(|r| r.to_string() != who);
            state.ccs.retain(|r| r.to_string() != who);
            return Some(if state.reviewers.len() + state.ccs.len() == before {
                Self::reply(404, b"Reviewer not found".to_vec())
            } else {
                Self::reply(204, Vec::new())
            });
        }
        if rest == "topic" && method == http::Method::PUT {
            let topic = body["topic"].as_str().unwrap_or_default().trim().to_owned();
            state.topic = (!topic.is_empty()).then(|| topic.clone());
            return Some(Self::json(200, &json!(topic)));
        }
        if rest == "topic" && method == http::Method::DELETE {
            state.topic = None;
            return Some(Self::reply(204, Vec::new()));
        }
        if rest == "hashtags" && method == http::Method::POST {
            let list = |k: &str| -> Vec<String> {
                body[k]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            };
            state.hashtags.retain(|h| !list("remove").contains(h));
            for h in list("add") {
                if !state.hashtags.contains(&h) {
                    state.hashtags.push(h);
                }
            }
            return Some(Self::json(200, &json!(state.hashtags)));
        }
        None
    }

    /// What alice's action does to the change, or why the server refuses.
    fn post_action(
        state: &mut DraftState,
        action: &str,
        delete: bool,
        body: &serde_json::Value,
    ) -> Result<(), String> {
        use serde_json::json;
        if state.status == "MERGED" {
            return Err("change is merged".to_owned());
        }
        let note = body["message"].as_str().map(str::to_owned);
        let text = match action {
            "submit" => {
                if state.votes.get("Code-Review") != Some(&2) {
                    return Err("submit requirement 'Code-Review' is unsatisfied".to_owned());
                }
                state.status = "MERGED";
                "Change has been successfully merged".to_owned()
            }
            "rebase" => {
                if state.rebased {
                    return Err("Change is already up to date.".to_owned());
                }
                state.rebased = true;
                "Patch Set 3: Patch Set 3 was rebased".to_owned()
            }
            "abandon" => {
                state.status = "ABANDONED";
                "Abandoned".to_owned()
            }
            "restore" => {
                if state.status != "ABANDONED" {
                    return Err("change is not abandoned".to_owned());
                }
                state.status = "NEW";
                "Restored".to_owned()
            }
            "wip" => {
                state.wip = true;
                "Set Work In Progress".to_owned()
            }
            "ready" => {
                state.wip = false;
                "Set Ready For Review".to_owned()
            }
            _ => {
                state.private = !delete;
                state.messages.push(json!({
                    "id": format!("act-{}", state.messages.len()),
                    "author": person(ALICE), "date": "2024-06-15 12:30:00.000000000",
                    "message": if delete { "Set public" } else { "Set private" },
                    "_revision_number": 3,
                }));
                return Ok(());
            }
        };
        let text = match note {
            Some(n) => format!("{text}\n\n{n}"),
            None => text,
        };
        state.messages.push(json!({
            "id": format!("act-{}", state.messages.len()),
            "author": person(ALICE), "date": "2024-06-15 12:30:00.000000000",
            "message": text, "_revision_number": 3,
        }));
        Ok(())
    }

    /// The labels alice may vote on and the values they have.
    fn permitted() -> BTreeMap<&'static str, Vec<i64>> {
        BTreeMap::from([
            ("Code-Review", vec![-2, -1, 0, 1, 2]),
            ("Verified", vec![-1, 0, 1]),
        ])
    }

    /// The change as the server has it after alice's replies so far.
    fn apply_to_change(c: &mut serde_json::Value, state: &DraftState) {
        use serde_json::json;
        let permitted = Self::permitted();
        // Gerrit writes the values as signed strings with a blank for 0.
        let text = |v: &i64| match *v {
            0 => " 0".to_owned(),
            v => format!("{v:+}"),
        };
        let strings: BTreeMap<_, Vec<String>> = permitted
            .iter()
            .map(|(label, values)| (*label, values.iter().map(text).collect()))
            .collect();
        c["permitted_labels"] = json!(strings);
        if let Some(labels) = c["labels"].as_object_mut() {
            for (label, vote) in &state.votes {
                let Some(info) = labels.get_mut(label) else {
                    continue;
                };
                let mut all = info["all"].as_array().cloned().unwrap_or_default();
                all.retain(|a| a["_account_id"] != ALICE);
                let mut mine = person(ALICE);
                mine["value"] = (*vote).into();
                mine["date"] = "2024-06-15 12:00:00.000000000".into();
                all.push(mine);
                info["all"] = all.into();
            }
        }
        let mut set = serde_json::Map::new();
        for (id, _, reason) in &state.attention {
            set.insert(
                id.to_string(),
                json!({"account": person(*id), "last_update": "2024-06-15 11:00:00.000000000",
                    "reason": reason}),
            );
        }
        c["attention_set"] = set.into();
        c["status"] = state.status.into();
        c["work_in_progress"] = state.wip.into();
        c["is_private"] = state.private.into();
        c["reviewers"] = json!({
            "REVIEWER": state.reviewers.iter().map(|i| person(*i)).collect::<Vec<_>>(),
            "CC": state.ccs.iter().map(|i| person(*i)).collect::<Vec<_>>(),
        });
        c["topic"] = state.topic.clone().into();
        c["hashtags"] = json!(state.hashtags);
        // What alice may do now (Gerrit leaves out `enabled` when false).
        let act = |label: &str| json!({"method": "POST", "label": label, "enabled": true});
        let mut change_actions = serde_json::Map::new();
        let mut revision_actions = serde_json::Map::new();
        match state.status {
            "NEW" => {
                change_actions.insert("abandon".into(), act("Abandon"));
                if state.wip {
                    change_actions.insert("ready".into(), act("Ready"));
                } else {
                    change_actions.insert("wip".into(), act("WIP"));
                    revision_actions.insert("submit".into(), act("Submit"));
                }
                revision_actions.insert(
                    "rebase".into(),
                    if state.rebased {
                        json!({"method": "POST", "label": "Rebase",
                            "title": "Change is already up to date"})
                    } else {
                        act("Rebase")
                    },
                );
            }
            "ABANDONED" => {
                change_actions.insert("restore".into(), act("Restore"));
            }
            _ => {}
        }
        if state.status != "MERGED" {
            if state.private {
                change_actions.insert("private.delete".into(), act("Unmark private"));
            } else {
                change_actions.insert("private".into(), act("Mark private"));
            }
        }
        if state.status != "MERGED" {
            change_actions.insert("topic".into(), act("Edit topic"));
            change_actions.insert("hashtags".into(), act("Edit hashtags"));
        }
        c["actions"] = change_actions.into();
        if let Some(rev) =
            c["revisions"]["3333333333333333333333333333333333333333"].as_object_mut()
        {
            rev.insert("actions".into(), revision_actions.into());
        }
        if let Some(messages) = c["messages"].as_array_mut() {
            messages.extend(state.messages.iter().cloned());
        }
    }

    /// Applies a `ReviewInput` the way Gerrit does: unknown labels and
    /// values are dropped with a 200, the drafts become comments, and the
    /// message lists the votes.
    fn post_review(state: &mut DraftState, body: &serde_json::Value) -> serde_json::Value {
        use serde_json::json;
        let permitted = Self::permitted();
        let mut applied = serde_json::Map::new();
        if let Some(labels) = body["labels"].as_object() {
            for (label, value) in labels {
                let Some(v) = value.as_i64() else { continue };
                if permitted
                    .get(label.as_str())
                    .is_some_and(|ok| ok.contains(&v))
                {
                    state.votes.insert(label.clone(), v);
                    applied.insert(label.clone(), v.into());
                }
            }
        }
        let tick = state.messages.len() + 1;
        let now = format!("2024-06-15 12:{:02}:00.000000000", tick % 60);
        let mut texts = Vec::new();
        let mut count = 0;
        if body["drafts"] != "KEEP" {
            for (id, mut c) in std::mem::take(&mut state.drafts) {
                let path = c["path"].as_str().unwrap_or_default().to_owned();
                if let Some(o) = c.as_object_mut() {
                    o.remove("path");
                }
                c["author"] = person(ALICE);
                c["updated"] = now.clone().into();
                if path == "/PATCHSET_LEVEL" {
                    texts.push(c["message"].as_str().unwrap_or_default().to_owned());
                } else {
                    count += 1;
                }
                c["id"] = format!("published-{id}").into();
                state.published.push((path, c));
            }
        }
        let ids = |key: &str| -> Vec<u64> {
            body[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| a["user"].as_str()?.parse().ok())
                .collect()
        };
        for id in ids("remove_from_attention_set") {
            state.attention.retain(|(i, ..)| *i != id);
        }
        for id in ids("add_to_attention_set") {
            if !state.attention.iter().any(|(i, ..)| *i == id) {
                let name = person(id)["name"].as_str().unwrap_or("Someone").to_owned();
                state
                    .attention
                    .push((id, name, "Alice Example replied".to_owned()));
            }
        }
        let mut summary = String::from("Patch Set 3:");
        for (label, v) in &applied {
            summary.push_str(&format!(" {label}{v:+}"));
        }
        for t in texts {
            summary.push_str(&format!("\n\n{t}"));
        }
        if count > 0 {
            summary.push_str(&format!(
                "\n\n({count} comment{})",
                if count == 1 { "" } else { "s" }
            ));
        }
        state.messages.push(json!({
            "id": format!("reply-{tick}"), "author": person(ALICE), "date": now,
            "message": summary, "_revision_number": 3,
        }));
        json!({"labels": applied})
    }
}

impl HttpClient for DraftServer {
    fn send(
        &self,
        req: http::Request<Vec<u8>>,
    ) -> BoxFuture<'static, Result<http::Response<Vec<u8>>, HttpError>> {
        use serde_json::{Value, json};
        let path = req.uri().path().to_owned();
        if let Some(answer) = self.other(&req, &path) {
            return answer;
        }
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

fn server(files: Option<usize>, changes: Option<usize>, chain: Option<usize>) -> RouteHttpClient {
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
    http.route(
        "/changes/?q=",
        200,
        changes.map_or_else(|| ex("query.json"), many_changes),
    );
    http.route("/changes/12345?", 200, ex("change.json"));
    // Stepping through the relation chain (Shift-J) lands on these: they
    // show the same detail under another number.
    for n in [12340, 12347, 12348] {
        http.route(&format!("/changes/{n}?"), 200, ex("change.json"));
    }
    http.route(
        "/related",
        200,
        chain.map_or_else(|| ex("related.json"), long_chain),
    );
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

/// Three file filters for the example change: one of each effect.
fn demo_filters() -> Vec<FileFilter> {
    use ui::filters::FilterAction;
    let rule = |name: &str, glob: &str, action| FileFilter {
        name: name.into(),
        globs: vec![glob.into()],
        projects: Vec::new(),
        action,
        enabled: true,
    };
    vec![
        rule("Heap", "heap.rs", FilterAction::Hide),
        rule("Manifest", "Cargo.toml", FilterAction::Dim),
        rule(
            "Tests",
            "tests/**",
            FilterAction::Highlight {
                color: Some(rgb(0xe5a50a)),
                bold: true,
            },
        ),
    ]
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut mode = None;
    let mut theme = ui::ThemeName::default();
    let mut local = DiffPreferences::default();
    let mut font = None;
    let mut files = None;
    let mut changes = None;
    let mut chain = None;
    let mut csd = false;
    let mut drafts = Drafts::Ok;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dark" => mode = Some(ThemeMode::Dark),
            "--theme" => {
                theme = args
                    .next()
                    .and_then(|t| ui::ThemeName::from_key(&t))
                    .unwrap_or_default()
            }
            "--light" => mode = Some(ThemeMode::Light),
            "--unified" => local.view_mode = Some(DiffViewMode::Unified),
            "--font" => font = args.next(),
            // Dev: ask for client-side decorations to see the title bar
            // buttons and the window frame on a compositor that draws its own.
            "--csd" => csd = true,
            "--files" => files = args.next().and_then(|n| n.parse().ok()),
            "--changes" => changes = args.next().and_then(|n| n.parse().ok()),
            "--chain" => chain = args.next().and_then(|n| n.parse().ok()),
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
            inner: server(files, changes, chain),
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
        start_page: Default::default(),
        filters: demo_filters(),
    };

    ui::application().run(move |cx| {
        gpui_kit::init(cx);
        ui::init(cx);
        ui::set_theme(theme, cx);
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
            Ok((window, _)) => {
                ui::ensure_maximized(window, cx);
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
