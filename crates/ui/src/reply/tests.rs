//! The reply dialog on GPUI's `TestAppContext` with a path-routed fake
//! server that sees the method and body of every request.

use std::sync::{Arc, Mutex};

use gerrit_api::{Auth, fake::RouteHttpClient};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
use model::{
    Account, AccountId, AttentionEntry, ChangeDetail, ChangeNumber, DraftInput, LabelValue,
    LabelVote, Location, PATCHSET_LEVEL_PATH, PatchSetNumber, PatchSetRef, Side,
};
use serde_json::{Value, json};
use store::{DraftFailure, Store};

use super::{ReplyDialog, ReplyEvent, ReplyRequest};
use crate::{
    outbox::DraftOutbox,
    session::Session,
    testing::detail,
    tests::{URL, env_with, open, setup},
};

const CHANGE: ChangeNumber = ChangeNumber(7);
const SEND: &str = if cfg!(target_os = "macos") {
    "cmd-enter"
} else {
    "ctrl-enter"
};
const REVIEW: &str = "POST /changes/7/revisions/sha3/review";
const DRAFTS: &str = "PUT /changes/7/revisions/sha3/drafts";

fn acct(id: u64, user: &str) -> Account {
    Account {
        id: AccountId(id),
        name: Some(user.to_owned()),
        email: None,
        username: Some(user.to_owned()),
        display_name: None,
    }
}

/// Change 7, three patch sets; alice owns it, bob (who replies) and carol
/// review; bob voted Code-Review +1, carol -1; bob and carol are in the
/// attention set.
fn change() -> ChangeDetail {
    let mut d = detail(7, 3, 3);
    d.summary.owner = acct(1, "alice");
    d.reviewers = vec![acct(2, "bob"), acct(3, "carol")];
    d.cc = vec![acct(4, "dave")];
    d.permitted_labels = [
        ("Code-Review".to_owned(), vec![-2, -1, 0, 1, 2]),
        ("Verified".to_owned(), vec![-1, 0, 1]),
    ]
    .into();
    d.label_values = [(
        "Code-Review".to_owned(),
        vec![
            LabelValue {
                value: -2,
                description: "This shall not be merged".into(),
            },
            LabelValue {
                value: 2,
                description: "Looks good to me, approved".into(),
            },
        ],
    )]
    .into();
    d.votes = [(
        "Code-Review".to_owned(),
        vec![
            LabelVote {
                account: acct(2, "bob"),
                value: 1,
                date: None,
            },
            LabelVote {
                account: acct(3, "carol"),
                value: -1,
                date: None,
            },
        ],
    )]
    .into();
    d.attention = vec![
        AttentionEntry {
            account: acct(2, "bob"),
            reason: Some("review requested".into()),
            last_update: None,
        },
        AttentionEntry {
            account: acct(3, "carol"),
            reason: None,
            last_update: None,
        },
    ];
    d
}

fn ps(n: u32) -> PatchSetRef {
    PatchSetRef {
        number: PatchSetNumber(n),
        sha: format!("sha{n}").into(),
    }
}

fn draft_input(path: &str, line: Option<u32>, message: &str, unresolved: bool) -> DraftInput {
    DraftInput {
        path: path.into(),
        side: Side::Revision,
        parent: None,
        location: line.map_or(Location::File, Location::Line),
        in_reply_to: None,
        message: message.into(),
        unresolved,
    }
}

/// How a seeded draft stands with the server.
#[derive(Clone, Copy)]
enum Seed {
    Synced,
    Pending,
    Failed,
}

/// Adds a draft to the store before the session opens it.
fn seed(store: &mut Store, patch_set: u32, input: DraftInput, how: Seed) {
    let server = setup().server;
    let now = "2024-06-15T10:00:00Z".parse().expect("ts");
    let d = store
        .create_draft(server, CHANGE, &ps(patch_set), &input, now)
        .expect("draft");
    match how {
        Seed::Pending => {}
        Seed::Synced => {
            store
                .record_draft_uploaded(
                    server,
                    d.local_id,
                    d.version,
                    &format!("srv{}", d.local_id.0),
                    now,
                )
                .expect("uploaded");
        }
        Seed::Failed => {
            store
                .record_draft_failure(
                    server,
                    d.local_id,
                    d.version,
                    &DraftFailure::Rejected {
                        error: "no".into(),
                        retryable: false,
                    },
                    now,
                )
                .expect("failed");
        }
    }
}

struct Fx {
    window: AnyWindowHandle,
    dialog: Entity<ReplyDialog>,
    http: RouteHttpClient,
    session: Session,
    events: Arc<Mutex<Vec<String>>>,
    outbox: Entity<DraftOutbox>,
    store: Arc<Mutex<Store>>,
    /// Kept alive like the page that opened the dialog keeps it.
    _comments: Entity<crate::comments::ChangeComments>,
}

/// Opens the dialog as bob. `prepare` fills the store; `routes` the fake
/// server (a bare `/comments` and `/changes/7?` answer too, so a refresh
/// after a send finds something).
fn dialog(
    cx: &mut TestAppContext,
    prepare: impl FnOnce(&mut Store),
    routes: impl FnOnce(&RouteHttpClient),
) -> Fx {
    dialog_for(cx, change(), prepare, routes)
}

fn dialog_for(
    cx: &mut TestAppContext,
    detail: ChangeDetail,
    prepare: impl FnOnce(&mut Store),
    routes: impl FnOnce(&RouteHttpClient),
) -> Fx {
    dialog_on(cx, prepare, |_, _| ReplyRequest::gerrit(detail), routes)
}

/// Opens the dialog as bob for the request `request` makes once the store
/// is prepared and the session is open.
fn dialog_on(
    cx: &mut TestAppContext,
    prepare: impl FnOnce(&mut Store),
    request: impl FnOnce(&Session, &Mutex<Store>) -> ReplyRequest,
    routes: impl FnOnce(&RouteHttpClient),
) -> Fx {
    let mut store = Store::open_in_memory().expect("store");
    assert_eq!(store.server_id(URL).expect("server"), setup().server);
    prepare(&mut store);
    let http = RouteHttpClient::new();
    // `/comments` and `/drafts` are not routed unless a test does: the
    // refresh fails and leaves the cache as seeded.
    routes(&http);
    let env = env_with(store, &http);
    let store = env.store.clone();
    let mut server = setup();
    server.username = Some("bob".into());
    let auth = Auth::Basic {
        username: "bob".into(),
        password: "p".into(),
    };
    let session = Session::new(&env, &server, Some(auth)).expect("session");
    let request = request(&session, &store);
    let number = request.detail.summary.number;
    let events = Arc::new(Mutex::new(Vec::new()));
    let log = events.clone();
    let outbox = cx.new(|_| DraftOutbox::new(session.clone()));
    // The page that opens the dialog has the comments loaded already.
    let comments = cx.update(|cx| outbox.update(cx, |o, cx| o.comments(number, cx)));
    cx.run_until_parked();
    let dialog_session = session.clone();
    let (window, dialog) = open(cx, |window, cx| {
        let dialog =
            cx.new(|cx| ReplyDialog::open(dialog_session, outbox.clone(), request, window, cx));
        cx.subscribe(&dialog, move |_, ev: &ReplyEvent, _| {
            let text = match ev {
                ReplyEvent::Close => "close".to_owned(),
                ReplyEvent::Unauthorized => "unauthorized".to_owned(),
                ReplyEvent::Saved(note) => format!("saved: {note}"),
                ReplyEvent::Sent(s) => format!(
                    "sent detail={} unapplied={:?} warning={:?}{}",
                    s.detail.is_some(),
                    s.unapplied,
                    s.warning,
                    if s.voted.is_empty() {
                        String::new()
                    } else {
                        format!(" voted={:?}", s.voted)
                    }
                ),
            };
            log.lock().expect("log").push(text);
        })
        .detach();
        dialog
    });
    cx.run_until_parked();
    Fx {
        window,
        dialog,
        http,
        session,
        events,
        outbox,
        store,
        _comments: comments,
    }
}

fn press(cx: &mut TestAppContext, f: &Fx, keys: &[&'static str]) {
    for k in keys {
        cx.update_window(f.window, |_, window, cx| window.press(k, cx))
            .expect("press");
        cx.run_until_parked();
    }
}

fn type_text(cx: &mut TestAppContext, f: &Fx, text: &str) {
    cx.update_window(f.window, |_, window, cx| window.input(text, cx))
        .expect("input");
}

fn events(f: &Fx) -> Vec<String> {
    f.events.lock().expect("log").clone()
}

fn focused(cx: &mut TestAppContext, f: &Fx, handle: gpui_kit::FocusHandle) -> bool {
    cx.update_window(f.window, |_, window, _| handle.is_focused(window))
        .expect("window")
}

fn focus(cx: &mut TestAppContext, f: &Fx, handle: &gpui_kit::FocusHandle) {
    cx.update_window(f.window, |_, window, cx| window.focus(handle, cx))
        .expect("focus");
}

/// The writes that went out, as `METHOD target`.
fn writes(f: &Fx) -> Vec<String> {
    f.http
        .sent()
        .into_iter()
        .filter(|r| r.method != "GET")
        .map(|r| format!("{} {}", r.method, r.target))
        .collect()
}

/// The JSON body of the one request whose method and target match.
fn body_of(f: &Fx, method: &str, target_end: &str) -> Value {
    let found: Vec<_> = f
        .http
        .sent()
        .into_iter()
        .filter(|r| r.method == method && r.target.contains(target_end))
        .collect();
    assert_eq!(found.len(), 1, "{method} {target_end}: {:?}", f.http.sent());
    serde_json::from_slice(&found[0].body).expect("a JSON body")
}

fn cached_drafts(f: &Fx) -> Vec<model::Draft> {
    f.session
        .syncer()
        .cached_comments(CHANGE)
        .expect("cache")
        .drafts
}

fn text(cx: &mut TestAppContext, f: &Fx) -> String {
    cx.update(|cx| f.dialog.read(cx).text_for_test(cx))
}

/// Focuses vote `choice` of row `row` (Code-Review is row 0: -2 -1 0 +1 +2).
fn pick_vote(cx: &mut TestAppContext, f: &Fx, row: usize, choice: usize) {
    let h = cx.update(|cx| f.dialog.read(cx).vote_focus_for_test(row, choice));
    focus(cx, f, &h);
    press(cx, f, &["space"]);
}

const RECEIPT: &str = r#")]}'
{"id":"srv9","patch_set":3,"updated":"2024-06-15 10:00:00.000000000"}"#;

fn ok_review(http: &RouteHttpClient) {
    http.route(REVIEW, 200, ")]}'\n{\"labels\":{\"Code-Review\":2}}");
    http.route(DRAFTS, 201, RECEIPT);
}

#[gpui_kit::test]
fn it_opens_with_the_focus_in_the_message_and_escape_closes_it(cx: &mut TestAppContext) {
    let f = dialog(cx, |_| {}, |_| {});
    let input = cx.update(|cx| f.dialog.read(cx).input_focus_for_test());
    assert!(focused(cx, &f, input));
    press(cx, &f, &["escape"]);
    assert_eq!(events(&f), ["close"]);
    // Nothing was typed: no draft, no request.
    assert!(writes(&f).is_empty());
    assert!(cached_drafts(&f).is_empty());
}

#[gpui_kit::test]
fn single_letter_keys_are_text_in_the_message(cx: &mut TestAppContext) {
    let f = dialog(cx, |_| {}, |_| {});
    type_text(cx, &f, "rar");
    assert_eq!(text(cx, &f), "rar");
    assert!(events(&f).is_empty());
}

#[gpui_kit::test]
fn escape_keeps_what_was_typed_as_a_patchset_draft(cx: &mut TestAppContext) {
    let f = dialog(
        cx,
        |_| {},
        |http| {
            http.route(DRAFTS, 201, RECEIPT);
        },
    );
    type_text(cx, &f, "Looks good to me");
    press(cx, &f, &["escape"]);
    assert_eq!(events(&f), ["close"]);
    let body = body_of(&f, "PUT", "/revisions/sha3/drafts");
    assert_eq!(body["path"], PATCHSET_LEVEL_PATH);
    assert_eq!(body["message"], "Looks good to me");
    // The box is ticked by default: not a to-do for the owner.
    assert_eq!(body["unresolved"], false);
    assert!(
        body.get("line").is_none() && body.get("side").is_none(),
        "{body}"
    );
    let drafts = cached_drafts(&f);
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].patch_set, PatchSetNumber(3));
}

#[gpui_kit::test]
fn the_message_starts_as_the_existing_patchset_draft_of_the_current_patch_set(
    cx: &mut TestAppContext,
) {
    let f = dialog(
        cx,
        |store| {
            // Another patch set's, a reply and an inline draft are not it.
            seed(
                store,
                2,
                draft_input(PATCHSET_LEVEL_PATH, None, "old", true),
                Seed::Synced,
            );
            let mut reply = draft_input(PATCHSET_LEVEL_PATH, None, "a reply", true);
            reply.in_reply_to = Some("c1".into());
            seed(store, 3, reply, Seed::Synced);
            seed(
                store,
                3,
                draft_input("a.rs", Some(4), "inline", true),
                Seed::Synced,
            );
            seed(
                store,
                3,
                draft_input(
                    PATCHSET_LEVEL_PATH,
                    None,
                    "Please fix
the tests",
                    true,
                ),
                Seed::Synced,
            );
        },
        |_| {},
    );
    assert_eq!(
        text(cx, &f),
        "Please fix
the tests"
    );
    // The draft was unresolved: the box is not ticked.
    cx.update(|cx| assert!(!f.dialog.read(cx).resolved_for_test()));
    // The list has the other three, not the message.
    let places: Vec<_> = cx.update(|cx| {
        f.dialog
            .read(cx)
            .drafts_for_test()
            .into_iter()
            .map(|d| (d.place, d.text, d.other_patch_set))
            .collect()
    });
    assert_eq!(places.len(), 3, "{places:?}");
    assert!(places.contains(&("Patchset".into(), "old".into(), true)));
    assert!(places.contains(&("a.rs:4".into(), "inline".into(), false)));
}

#[gpui_kit::test]
fn closing_with_changed_text_updates_the_draft_and_clearing_it_deletes_it(cx: &mut TestAppContext) {
    let f = dialog(
        cx,
        |store| {
            seed(
                store,
                3,
                draft_input(PATCHSET_LEVEL_PATH, None, "first", false),
                Seed::Synced,
            );
        },
        |http| {
            http.route("PUT /changes/7/revisions/sha3/drafts/srv1", 200, RECEIPT);
        },
    );
    type_text(cx, &f, " and more");
    press(cx, &f, &["escape"]);
    let body = body_of(&f, "PUT", "/drafts/srv1");
    assert_eq!(body["message"], "first and more");

    // A second dialog: clear the text, close: the draft goes.
    let f = dialog(
        cx,
        |store| {
            seed(
                store,
                3,
                draft_input(PATCHSET_LEVEL_PATH, None, "first", false),
                Seed::Synced,
            );
        },
        |http| {
            http.route("DELETE /changes/7/revisions/sha3/drafts/srv1", 204, "");
        },
    );
    cx.update_window(f.window, |_, window, cx| {
        f.dialog
            .update(cx, |d, cx| d.set_text_for_test("  ", window, cx))
    })
    .expect("window");
    press(cx, &f, &["escape"]);
    assert_eq!(
        writes(&f),
        ["DELETE /a/changes/7/revisions/sha3/drafts/srv1"]
    );
    assert!(cached_drafts(&f).is_empty());
}

#[gpui_kit::test]
fn tab_moves_between_the_controls_and_does_not_indent(cx: &mut TestAppContext) {
    let f = dialog(cx, |_| {}, |_| {});
    type_text(cx, &f, "x");
    let (input, cancel) = cx.update(|cx| {
        let d = f.dialog.read(cx);
        (d.input_focus_for_test(), d.cancel_focus_for_test())
    });
    press(cx, &f, &["tab"]);
    assert!(!focused(cx, &f, input.clone()));
    assert_eq!(text(cx, &f), "x", "tab is not an indent");
    // Back to the message with shift-tab, and round the other way to the
    // last control (Cancel: nothing to send yet).
    press(cx, &f, &["shift-tab"]);
    assert!(focused(cx, &f, input.clone()));
    press(cx, &f, &["shift-tab"]);
    let can_send = cx.update(|cx| f.dialog.read(cx).can_send_for_test(cx));
    assert!(can_send, "text makes it sendable");
    let send = cx.update(|cx| f.dialog.read(cx).send_focus_for_test());
    assert!(focused(cx, &f, send));
    press(cx, &f, &["tab"]);
    assert!(focused(cx, &f, input));
    let _ = cancel;
}

#[gpui_kit::test]
fn the_shell_keys_are_off_while_the_dialog_is_open(cx: &mut TestAppContext) {
    let f = dialog(cx, |_| {}, |_| {});
    let cancel = cx.update(|cx| f.dialog.read(cx).cancel_focus_for_test());
    focus(cx, &f, &cancel);
    press(cx, &f, &["u", "r", "a", "j"]);
    assert!(events(&f).is_empty());
}

// ---- votes ----

#[gpui_kit::test]
fn the_rows_are_the_permitted_labels_with_the_own_vote_selected(cx: &mut TestAppContext) {
    let f = dialog(cx, |_| {}, |_| {});
    let rows: Vec<_> = cx.update(|cx| {
        f.dialog
            .read(cx)
            .votes_for_test()
            .iter()
            .map(|r| (r.label.clone(), r.current, r.selected))
            .collect()
    });
    assert_eq!(
        rows,
        [
            ("Code-Review".to_owned(), 1, 1),
            ("Verified".to_owned(), 0, 0)
        ]
    );
}

#[gpui_kit::test]
fn only_changed_votes_are_in_the_body(cx: &mut TestAppContext) {
    let f = dialog(cx, |_| {}, ok_review);
    type_text(cx, &f, "hi");
    // Code-Review +2 (index 4); Verified: pick 0, which it already is.
    pick_vote(cx, &f, 0, 4);
    pick_vote(cx, &f, 1, 1);
    press(cx, &f, &[SEND]);
    let body = body_of(&f, "POST", "/revisions/sha3/review");
    assert_eq!(body["labels"], json!({"Code-Review": 2}));
    assert_eq!(body["drafts"], "PUBLISH_ALL_REVISIONS");
    assert!(body.get("message").is_none(), "{body}");
    assert_eq!(events(&f).len(), 1);
}

#[gpui_kit::test]
fn the_sent_event_names_the_votes_cast_and_none_for_a_plain_reply(cx: &mut TestAppContext) {
    let f = dialog(cx, |_| {}, ok_review);
    type_text(cx, &f, "hi");
    pick_vote(cx, &f, 0, 4);
    press(cx, &f, &[SEND]);
    assert_eq!(
        events(&f),
        ["sent detail=false unapplied=[] warning=None voted=[(\"Code-Review\", 2)]"]
    );

    let f = dialog(cx, |_| {}, ok_review);
    type_text(cx, &f, "hi");
    press(cx, &f, &[SEND]);
    assert_eq!(events(&f), ["sent detail=false unapplied=[] warning=None"]);
}

#[gpui_kit::test]
fn tab_enters_a_vote_row_on_the_current_vote(cx: &mut TestAppContext) {
    let f = dialog(cx, |_| {}, |_| {});
    // Message, Resolved, then Code-Review on bob's +1 (index 3), not -2.
    press(cx, &f, &["tab", "tab"]);
    let on_plus_one = cx.update(|cx| f.dialog.read(cx).vote_focus_for_test(0, 3));
    assert!(focused(cx, &f, on_plus_one));
    press(cx, &f, &["right"]);
    let selected =
        |cx: &mut TestAppContext| cx.update(|cx| f.dialog.read(cx).votes_for_test()[0].selected);
    assert_eq!(selected(cx), 2, "Right moves from the current vote");
    // The next row (Verified, no vote) starts on 0, the middle of -1 0 +1.
    press(cx, &f, &["tab"]);
    let on_zero = cx.update(|cx| f.dialog.read(cx).vote_focus_for_test(1, 1));
    assert!(focused(cx, &f, on_zero));
}

#[gpui_kit::test]
fn left_and_right_pick_the_neighbouring_value(cx: &mut TestAppContext) {
    let f = dialog(cx, |_| {}, |_| {});
    let h = cx.update(|cx| f.dialog.read(cx).vote_focus_for_test(0, 3));
    focus(cx, &f, &h);
    press(cx, &f, &["right"]);
    let selected =
        |cx: &mut TestAppContext| cx.update(|cx| f.dialog.read(cx).votes_for_test()[0].selected);
    assert_eq!(selected(cx), 2);
    let five = cx.update(|cx| f.dialog.read(cx).vote_focus_for_test(0, 4));
    assert!(focused(cx, &f, five));
    press(cx, &f, &["right", "left", "left"]);
    assert_eq!(selected(cx), 0);
}

// ---- attention set ----

#[gpui_kit::test]
fn the_attention_defaults_follow_polygerrit(cx: &mut TestAppContext) {
    let f = dialog(cx, |_| {}, |_| {});
    let people: Vec<_> = cx.update(|cx| {
        f.dialog
            .read(cx)
            .people_for_test()
            .iter()
            .map(|p| (p.account.display(), p.in_set, p.want))
            .collect()
    });
    // bob replies: he leaves, the owner is called, carol stays.
    assert_eq!(
        people,
        [
            ("bob".to_owned(), true, false),
            ("alice".to_owned(), false, true),
            ("carol".to_owned(), true, true),
            ("dave".to_owned(), false, false),
        ]
    );
}

#[gpui_kit::test]
fn only_the_differences_go_into_the_attention_lists(cx: &mut TestAppContext) {
    let f = dialog(cx, |_| {}, ok_review);
    // Take carol off and call dave.
    for (ix, _) in [(2, "carol"), (3, "dave")] {
        let h = cx.update(|cx| f.dialog.read(cx).person_focus_for_test(ix));
        focus(cx, &f, &h);
        press(cx, &f, &["space"]);
    }
    type_text(cx, &f, "hi");
    // Message text moves focus? No: typing needs the field.
    let input = cx.update(|cx| f.dialog.read(cx).input_focus_for_test());
    focus(cx, &f, &input);
    type_text(cx, &f, "!");
    press(cx, &f, &[SEND]);
    let body = body_of(&f, "POST", "/revisions/sha3/review");
    assert_eq!(
        body["add_to_attention_set"],
        json!([
            {"user": "1", "reason": "bob replied"},
            {"user": "4", "reason": "bob replied"},
        ])
    );
    assert_eq!(
        body["remove_from_attention_set"],
        json!([
            {"user": "2", "reason": "bob replied"},
            {"user": "3", "reason": "bob replied"},
        ])
    );
    assert!(body.get("labels").is_none());
}

// ---- drafts ----

#[gpui_kit::test]
fn unuploaded_drafts_block_the_send_until_they_are_uploaded(cx: &mut TestAppContext) {
    let f = dialog(
        cx,
        |store| {
            seed(
                store,
                2,
                draft_input("a.rs", Some(4), "waits", true),
                Seed::Pending,
            );
            seed(
                store,
                3,
                draft_input("b.rs", Some(1), "ok", true),
                Seed::Synced,
            );
        },
        |http| {
            ok_review(http);
            http.route("PUT /changes/7/revisions/sha2/drafts", 201, RECEIPT);
        },
    );
    let items = cx.update(|cx| f.dialog.read(cx).drafts_for_test());
    assert_eq!(items.len(), 2);
    assert!(items.iter().any(|d| d.pending() && d.other_patch_set));
    type_text(cx, &f, "hi");
    assert!(!cx.update(|cx| f.dialog.read(cx).can_send_for_test(cx)));
    press(cx, &f, &[SEND]);
    let error = cx.update(|cx| f.dialog.read(cx).error_for_test().map(str::to_owned));
    assert!(error.is_some_and(|e| e.contains("not uploaded")));
    assert!(
        !writes(&f).iter().any(|w| w.starts_with("POST")),
        "{:?}",
        writes(&f)
    );

    // "Upload drafts" brings it up, and then the send is possible.
    let upload = cx.update(|cx| f.dialog.read(cx).upload_focus_for_test());
    focus(cx, &f, &upload);
    press(cx, &f, &["space"]);
    cx.run_until_parked();
    assert!(cx.update(|cx| f.dialog.read(cx).can_send_for_test(cx)));
}

#[gpui_kit::test]
fn a_draft_the_server_refuses_is_reported_and_nothing_is_posted(cx: &mut TestAppContext) {
    // The message itself cannot be uploaded: the send says so.
    let f = dialog(
        cx,
        |_| {},
        |http| {
            http.route(DRAFTS, 503, "down");
            http.route(REVIEW, 200, ")]}'\n{}");
        },
    );
    type_text(cx, &f, "hello");
    press(cx, &f, &[SEND]);
    let error = cx
        .update(|cx| f.dialog.read(cx).error_for_test().map(str::to_owned))
        .expect("an error");
    assert!(error.contains("message could not be uploaded"), "{error}");
    // The dialog lists no drafts here, so it must not point at a button.
    assert!(!error.contains("Upload drafts"), "{error}");
    assert!(!writes(&f).iter().any(|w| w.starts_with("POST")));
    assert!(events(&f).is_empty(), "the dialog stays open");
    // The text is a draft in the store.
    assert_eq!(cached_drafts(&f).len(), 1);
    assert_eq!(text(cx, &f), "hello");
    // (ctrl-enter's newline was taken back.)
}

// ---- results ----

#[gpui_kit::test]
fn a_sent_review_refreshes_the_change_and_the_comments(cx: &mut TestAppContext) {
    let f = dialog(
        cx,
        |store| {
            seed(
                store,
                3,
                draft_input("b.rs", Some(1), "inline", true),
                Seed::Synced,
            );
        },
        ok_review,
    );
    type_text(cx, &f, "Looks good");
    pick_vote(cx, &f, 0, 4);
    // The server has published the draft by the time it is asked again.
    f.http.route_json("/comments", "{}");
    f.http.route_json("/drafts", "{}");
    press(cx, &f, &[SEND]);
    assert_eq!(
        events(&f),
        ["sent detail=false unapplied=[] warning=None voted=[(\"Code-Review\", 2)]"],
        "{:?}",
        f.http.sent()
    );
    let seen = f.http.requested();
    assert!(seen.iter().any(|r| r.contains("/changes/7?")), "{seen:?}");
    assert!(seen.iter().any(|r| r.contains("/comments")), "{seen:?}");
    // The drafts became comments on the server: none left locally.
    assert!(cached_drafts(&f).is_empty());
}

#[gpui_kit::test]
fn a_vote_the_server_dropped_is_reported_with_the_sent_event(cx: &mut TestAppContext) {
    let f = dialog(
        cx,
        |_| {},
        |http| {
            http.route(REVIEW, 200, ")]}'\n{\"labels\":{}}");
            http.route(DRAFTS, 201, RECEIPT);
        },
    );
    pick_vote(cx, &f, 0, 4);
    press(cx, &f, &[SEND]);
    assert_eq!(
        events(&f),
        ["sent detail=false unapplied=[(\"Code-Review\", 2)] warning=None"]
    );
}

#[gpui_kit::test]
fn the_sent_note_names_unapplied_votes(cx: &mut TestAppContext) {
    use super::Sent;
    let ok = Sent {
        detail: None,
        unapplied: vec![],
        warning: None,
        voted: vec![],
    };
    assert_eq!(ok.note(), ("Reply sent".to_owned(), false));
    let dropped = Sent {
        unapplied: vec![("Code-Review".into(), 2), ("Verified".into(), -1)],
        ..ok
    };
    let (note, warning) = dropped.note();
    assert!(
        warning && note.contains("Code-Review +2, Verified -1"),
        "{note}"
    );
    let _ = cx;
}

#[gpui_kit::test]
fn a_401_goes_to_the_login_flow_and_keeps_the_text(cx: &mut TestAppContext) {
    let f = dialog(
        cx,
        |_| {},
        |http| {
            http.route(DRAFTS, 401, "");
            http.route(REVIEW, 401, "");
        },
    );
    type_text(cx, &f, "precious");
    press(cx, &f, &[SEND]);
    assert_eq!(events(&f), ["unauthorized"]);
    let drafts = cached_drafts(&f);
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].message, "precious");
}

#[gpui_kit::test]
fn a_refusal_is_shown_in_the_dialog_with_the_servers_words(cx: &mut TestAppContext) {
    for (status, needle) in [
        (403, "not allowed (403): cannot vote on Verified"),
        (409, "in conflict (409): change is closed"),
        (400, "rejected (400): bad label"),
    ] {
        let message = match status {
            403 => "cannot vote on Verified",
            409 => "change is closed",
            _ => "bad label",
        };
        let f = dialog(
            cx,
            |_| {},
            move |http| {
                http.route(DRAFTS, 201, RECEIPT);
                http.route(REVIEW, status, message);
            },
        );
        type_text(cx, &f, "hello");
        press(cx, &f, &[SEND]);
        let error = cx
            .update(|cx| f.dialog.read(cx).error_for_test().map(str::to_owned))
            .expect("an error");
        assert!(error.contains(needle), "{status}: {error}");
        assert!(events(&f).is_empty(), "stays open");
        // Esc closes it; the text is still a draft.
        press(cx, &f, &["escape"]);
        assert_eq!(events(&f), ["close"]);
        assert_eq!(cached_drafts(&f).len(), 1);
    }
}

#[gpui_kit::test]
fn offline_the_review_is_not_sent_and_the_text_is_kept(cx: &mut TestAppContext) {
    let f = dialog(
        cx,
        |_| {},
        |http| {
            http.route(DRAFTS, 503, "down");
            http.route(REVIEW, 503, "down");
        },
    );
    type_text(cx, &f, "hello");
    press(cx, &f, &[SEND]);
    let error = cx
        .update(|cx| f.dialog.read(cx).error_for_test().map(str::to_owned))
        .expect("an error");
    assert!(error.contains("Not sent"), "{error}");
    assert!(events(&f).is_empty());
    press(cx, &f, &["escape"]);
    assert_eq!(cached_drafts(&f)[0].message, "hello");
}

#[gpui_kit::test]
fn escape_does_not_cancel_a_send_that_is_under_way(cx: &mut TestAppContext) {
    let f = dialog(cx, |_| {}, ok_review);
    type_text(cx, &f, "hello");
    // No parking between the keys: the request has not been answered yet.
    cx.update_window(f.window, |_, window, cx| window.press(SEND, cx))
        .expect("press");
    assert!(cx.update(|cx| f.dialog.read(cx).sending_for_test()));
    cx.update_window(f.window, |_, window, cx| window.press("escape", cx))
        .expect("press");
    cx.run_until_parked();
    assert_eq!(events(&f), ["sent detail=false unapplied=[] warning=None"]);
}

#[gpui_kit::test]
fn there_is_nothing_to_send_without_content(cx: &mut TestAppContext) {
    // The owner is called already and bob is not in the set: no change.
    let mut d = change();
    d.attention[0].account = acct(1, "alice");
    d.attention.truncate(1);
    let f = dialog_for(cx, d, |_| {}, ok_review);
    assert!(!cx.update(|cx| f.dialog.read(cx).can_send_for_test(cx)));
    press(cx, &f, &[SEND]);
    assert!(writes(&f).is_empty());
    assert!(cx.update(|cx| f.dialog.read(cx).error_for_test().is_some()));
}

#[gpui_kit::test]
fn the_failed_draft_is_listed_with_its_error(cx: &mut TestAppContext) {
    let f = dialog(
        cx,
        |store| {
            seed(
                store,
                3,
                draft_input("b.rs", Some(1), "inline", true),
                Seed::Failed,
            );
        },
        |_| {},
    );
    let items = cx.update(|cx| f.dialog.read(cx).drafts_for_test());
    assert_eq!(items[0].problem().as_deref(), Some("failed: no"));
    assert!(!cx.update(|cx| f.dialog.read(cx).can_send_for_test(cx)));
}

// ---- opened from a page ----

mod from_the_shell {
    use std::sync::Arc;

    use gerrit_api::fake::RouteHttpClient;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
    use model::ChangeNumber;
    use store::Store;

    use crate::{
        filters::FileFilter,
        nav::Route,
        session::{Backend, BackendError, SavedQuery, ServerSetup, Startup, StartupPage},
        shell::Shell,
        testing::{detail, summary},
        tests::{URL, env_with, open, setup},
    };

    struct Keyring;

    impl Backend for Keyring {
        fn for_server(
            self: std::sync::Arc<Self>,
            _: &crate::ServerKey,
        ) -> std::sync::Arc<dyn crate::ServerBackend> {
            self
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

        fn save_theme(
            &self,
            t: crate::theme::ThemeName,
        ) -> Result<crate::theme::ThemeName, BackendError> {
            Ok(t)
        }
    }

    impl crate::ServerBackend for Keyring {
        fn load_password(&self, _: &str, _: &str) -> Result<Option<String>, BackendError> {
            Ok(Some("secret".into()))
        }

        fn save_queries(&self, _: &[SavedQuery]) -> Result<Vec<SavedQuery>, BackendError> {
            Err(BackendError("not used".into()))
        }

        fn save_login(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<Vec<SavedQuery>, BackendError> {
            Err(BackendError("not used".into()))
        }
    }

    /// The change page of change 7, opened from a dashboard row.
    fn on_the_change(
        cx: &mut TestAppContext,
        username: Option<&str>,
    ) -> (AnyWindowHandle, Entity<Shell>) {
        let mut store = Store::open_in_memory().expect("store");
        let server = store.server_id(URL).expect("server");
        let now = jiff::Timestamp::now();
        store
            .save_query_page(server, "status:open", &[summary(7)], false, now)
            .expect("query");
        store
            .save_change_detail(server, &detail(7, 3, 3), now)
            .expect("detail");
        let http = RouteHttpClient::new();
        let mut env = env_with(store, &http);
        env.backend = Arc::new(Keyring);
        let startup = Startup {
            env,
            diff: Default::default(),
            start_page: Default::default(),
            filters: Vec::new(),
            servers: vec![ServerSetup {
                username: username.map(str::to_owned),
                queries: vec![SavedQuery {
                    name: "Open".into(),
                    query: "status:open".into(),
                }],
                bookmarks: Vec::new(),
                ..setup()
            }],
            active: None,
        };
        let (window, shell) = open(cx, |window, cx| {
            cx.new(|cx| Shell::new(startup, window, cx))
        });
        cx.run_until_parked();
        cx.update_window(window, |_, w, cx| w.press("enter", cx))
            .expect("press");
        cx.run_until_parked();
        cx.update(|cx| assert_eq!(shell.read(cx).route(), &Route::Change(ChangeNumber(7))));
        (window, shell)
    }

    fn press(cx: &mut TestAppContext, window: AnyWindowHandle, key: &'static str) {
        cx.update_window(window, |_, w, cx| w.press(key, cx))
            .expect("press");
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn a_opens_the_dialog_over_the_page_and_escape_gives_the_page_back(cx: &mut TestAppContext) {
        let (window, shell) = on_the_change(cx, Some("bob"));
        press(cx, window, "a");
        cx.update(|cx| assert!(shell.read(cx).reply_for_test().is_some()));
        // The keys of the shell and the page do nothing behind it.
        press(cx, window, "?");
        press(cx, window, "u");
        cx.update(|cx| {
            assert!(!shell.read(cx).help_open_for_test());
            assert_eq!(shell.read(cx).route(), &Route::Change(ChangeNumber(7)));
        });
        // Letters are text in the message (the two keys above were too).
        cx.update_window(window, |_, w, cx| w.input("ar", cx))
            .expect("input");
        cx.update(|cx| {
            let dialog = shell.read(cx).reply_for_test().expect("dialog");
            assert_eq!(dialog.read(cx).text_for_test(cx), "?uar");
        });
        press(cx, window, "escape");
        cx.update(|cx| assert!(shell.read(cx).reply_for_test().is_none()));
        // The page has the focus again: its keys work.
        let page = cx
            .update(|cx| shell.read(cx).change_page_for_test())
            .expect("page");
        let focused = cx
            .update_window(window, |_, w, cx| {
                page.read(cx).focus_handle_for_test().is_focused(w)
            })
            .expect("window");
        assert!(focused);
    }

    #[gpui_kit::test]
    fn anonymous_sessions_get_a_hint_and_no_dialog(cx: &mut TestAppContext) {
        let (window, shell) = on_the_change(cx, None);
        press(cx, window, "a");
        cx.update(|cx| assert!(shell.read(cx).reply_for_test().is_none()));
        let page = cx
            .update(|cx| shell.read(cx).change_page_for_test())
            .expect("page");
        let hint = cx.update(|cx| page.read(cx).hint_for_test());
        assert!(matches!(hint, Some((t, false)) if t == "Sign in to reply"));
    }
}

#[test]
fn the_upload_hint_names_the_button_only_when_the_list_has_the_drafts() {
    use model::{LocalDraftId, PatchSetNumber};
    let pending = |id| sync::PendingDraft {
        local_id: LocalDraftId(id),
        path: "a.rs".into(),
        patch_set: PatchSetNumber(3),
        reason: "down".into(),
    };
    // Draft 1 is in the dialog's list, 2 is the message.
    let both = sync::Error::DraftsNotUploaded(vec![pending(1), pending(2)]);
    assert!(super::describe(&both, &[LocalDraftId(1)], &[]).contains("Upload drafts"));
    let only_message = sync::Error::DraftsNotUploaded(vec![pending(2)]);
    let text = super::describe(&only_message, &[LocalDraftId(1)], &[]);
    assert!(!text.contains("Upload drafts"), "{text}");
    assert!(text.contains("message could not be uploaded"), "{text}");
}

/// A local version of change 7 (commit `localsha`) in the store, with a
/// draft on it and a pending review with a message; returns its number.
fn seed_local_version(store: &mut Store) -> PatchSetNumber {
    let server = setup().server;
    let now: jiff::Timestamp = "2024-06-15T10:00:00Z".parse().expect("ts");
    let version = crate::testing::local_version(store, Some(CHANGE), "I7", "localsha");
    let on = PatchSetRef {
        number: version.number,
        sha: version.sha.clone(),
    };
    store
        .create_draft(
            server,
            CHANGE,
            &on,
            &draft_input("a.rs", Some(2), "on my local version", true),
            now,
        )
        .expect("draft");
    store
        .create_pending_review(
            server,
            &store::NewPendingReview {
                patch_set: version.number,
                labels: [("Code-Review".to_owned(), 2)].into(),
                strategy: model::LocalApproval::Exact,
                message: "the local review".into(),
                unresolved: false,
                created: now,
                replaces_draft: None,
            },
        )
        .expect("review");
    version.number
}

#[gpui_kit::test]
fn local_drafts_neither_block_nor_join_a_reply_to_gerrit(cx: &mut TestAppContext) {
    let f = dialog(
        cx,
        |store| {
            seed_local_version(store);
        },
        ok_review,
    );
    // Neither the local draft nor the review's message is listed, counted
    // or taken for the dialog's text.
    assert_eq!(text(cx, &f), "");
    cx.update(|cx| {
        let d = f.dialog.read(cx);
        assert!(d.drafts_for_test().is_empty());
        // The local draft is not uploaded, yet it holds nothing up.
        assert!(d.can_send_for_test(cx));
    });
    type_text(cx, &f, "Looks good");
    f.http.route_json("/comments", "{}");
    f.http.route_json("/drafts", "{}");
    press(cx, &f, &[SEND]);
    assert_eq!(events(&f), ["sent detail=false unapplied=[] warning=None"]);
    assert_eq!(
        writes(&f),
        [
            "PUT /a/changes/7/revisions/sha3/drafts",
            "POST /a/changes/7/revisions/sha3/review"
        ]
    );
    // The local draft is still there, untouched; the message went up.
    let left: Vec<_> = cached_drafts(&f)
        .into_iter()
        .map(|d| (d.message.clone(), d.is_local()))
        .collect();
    assert_eq!(left, [("on my local version".to_owned(), true)]);
}

// ---- local reviews ----

/// A local change with one version (`local 1`, commit `l1`) in the store.
fn seed_local_change(store: &mut Store) {
    crate::testing::local_version(store, None, "I1", "l1");
}

/// The local review of the version `sha` (of change `change_id`), the way
/// the change page asks for it, with the project's mode `strategy`.
fn local_request(
    session: &Session,
    store: &Mutex<Store>,
    change_id: &str,
    sha: &str,
    strategy: model::LocalApproval,
) -> ReplyRequest {
    let syncer = session.syncer();
    let row = store
        .lock()
        .expect("store")
        .local_changes_by_change_id(setup().server, change_id)
        .expect("rows")
        .remove(0);
    let number = row.gerrit_number.unwrap_or(row.number);
    let detail = crate::change_detail::cached(syncer, number)
        .expect("store")
        .expect("a detail")
        .detail;
    let target = detail
        .revisions
        .iter()
        .find(|r| r.sha.as_str() == sha)
        .map(|r| r.number);
    ReplyRequest {
        detail,
        target,
        edit: None,
        strategy,
    }
}

/// A pending review on `local 1` of the local change, as Save makes it.
fn seed_pending_review(store: &mut Store, message: &str) {
    let server = setup().server;
    let row = store
        .local_changes_by_change_id(server, "I1")
        .expect("rows")
        .remove(0);
    let version = store
        .local_patch_sets(server, row.id)
        .expect("versions")
        .remove(0);
    store
        .create_pending_review(
            server,
            &store::NewPendingReview {
                patch_set: version.number,
                labels: [("Code-Review".to_owned(), 1)].into(),
                strategy: model::LocalApproval::Exact,
                message: message.into(),
                unresolved: true,
                created: jiff::Timestamp::now(),
                replaces_draft: None,
            },
        )
        .expect("review");
}

#[gpui_kit::test]
fn new_review_instead_is_on_the_keyboard_while_a_review_is_edited(cx: &mut TestAppContext) {
    let f = local_dialog(cx, |store| seed_pending_review(store, "Saved before"));
    cx.update(|cx| {
        let d = f.dialog.read(cx);
        assert!(d.editing_for_test().is_some());
        assert_eq!(d.text_for_test(cx), "Saved before");
        // The message asked for something to be done.
        assert!(!d.resolved_for_test());
    });
    // Tab from the message: Resolved, the vote, then the chip.
    press(cx, &f, &["tab", "tab", "tab"]);
    let fresh = cx.update(|cx| f.dialog.read(cx).fresh_focus_for_test());
    assert!(focused(cx, &f, fresh));
    press(cx, &f, &["space"]);
    cx.update(|cx| {
        let d = f.dialog.read(cx);
        assert_eq!(d.editing_for_test(), None);
        assert_eq!(d.text_for_test(cx), "");
    });
}

fn local_dialog(cx: &mut TestAppContext, prepare: impl FnOnce(&mut Store)) -> Fx {
    dialog_on(
        cx,
        |store| {
            seed_local_change(store);
            prepare(store);
        },
        |s, st| local_request(s, st, "I1", "l1", model::LocalApproval::ExactOrManual),
        |_| {},
    )
}

fn local_number(f: &Fx) -> ChangeNumber {
    f.store
        .lock()
        .expect("store")
        .local_changes_by_change_id(setup().server, "I1")
        .expect("rows")[0]
        .number
}

fn reviews_of(f: &Fx, change: ChangeNumber) -> Vec<model::PendingReview> {
    f.session.syncer().pending_reviews(change).expect("reviews")
}

/// Runs `f` on a dialog that is not the window's root (one reopened with
/// [`reopen`]): keys do not reach it, so the tests call it directly.
fn on_dialog(
    cx: &mut TestAppContext,
    f: &Fx,
    dialog: &Entity<ReplyDialog>,
    act: impl FnOnce(&mut ReplyDialog, &mut gpui_kit::Window, &mut gpui_kit::Context<ReplyDialog>),
) {
    cx.update_window(f.window, |_, window, cx| {
        dialog.update(cx, |d, cx| act(d, window, cx))
    })
    .expect("window");
    cx.run_until_parked();
}

/// Reopens the dialog on the same store, like `a` again on the page.
fn reopen(cx: &mut TestAppContext, f: &Fx, request: ReplyRequest) -> Entity<ReplyDialog> {
    let (session, outbox) = (f.session.clone(), f.outbox.clone());
    let dialog = cx
        .update_window(f.window, |_, window, cx| {
            cx.new(|cx| ReplyDialog::open(session, outbox, request, window, cx))
        })
        .expect("window");
    cx.run_until_parked();
    dialog
}

const NOTE: &str = "Saved locally; sent automatically when local 1 is on Gerrit";

#[gpui_kit::test]
fn a_local_review_is_saved_as_a_pending_review_with_the_borrowed_labels(cx: &mut TestAppContext) {
    let f = local_dialog(cx, |_| {});
    let number = local_number(&f);
    cx.update(|cx| {
        let d = f.dialog.read(cx);
        assert!(d.is_local_for_test());
        assert_eq!(
            d.texts_for_test(),
            (
                "Local review".to_owned(),
                Some(NOTE.to_owned()),
                Some("An approval is only sent on this exact commit"),
            )
        );
        // Nothing borrowed from a cached change: Code-Review -2..+2. And no
        // attention set: a local review moves no one.
        let rows = d.votes_for_test();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Code-Review");
        assert!(d.people_for_test().is_empty());
        assert!(!d.can_send_for_test(cx), "nothing to save yet");
    });
    let mut visual = gpui_kit::VisualTestContext::from_window(f.window, cx);
    visual.run_until_parked();
    assert!(
        visual
            .debug_bounds("reply-note: Saved locally; sent automatically when local 1 is on Gerrit")
            .is_some()
    );
    assert!(
        visual
            .debug_bounds("reply-hint: An approval is only sent on this exact commit")
            .is_some()
    );
    type_text(cx, &f, "Looks good");
    pick_vote(cx, &f, 0, 4);
    press(cx, &f, &[SEND]);
    assert_eq!(events(&f), [format!("saved: {NOTE}")]);
    let reviews = reviews_of(&f, number);
    assert_eq!(reviews.len(), 1);
    let r = &reviews[0];
    assert_eq!(r.labels, [("Code-Review".to_owned(), 2)].into());
    assert_eq!(r.message.as_deref(), Some("Looks good"));
    assert_eq!(r.strategy, model::LocalApproval::ExactOrManual);
    assert_eq!(r.given_on_sha.as_str(), "l1");
    assert!(!r.unresolved);
    // The text became the review's message: no draft is left over.
    assert!(
        f.session
            .syncer()
            .cached_comments(number)
            .expect("cache")
            .drafts
            .is_empty()
    );
    assert!(f.http.requested().is_empty(), "{:?}", f.http.requested());

    // `a` again edits it; "New review instead" makes a second one with a
    // message of its own (B8).
    let request = local_request(
        &f.session,
        &f.store,
        "I1",
        "l1",
        model::LocalApproval::Follow,
    );
    let again = reopen(cx, &f, request);
    cx.update(|cx| {
        let d = again.read(cx);
        assert_eq!(d.editing_for_test(), Some(r.id));
        assert_eq!(d.texts_for_test().0, "Edit local review");
        assert_eq!(d.text_for_test(cx), "Looks good");
        assert_eq!(d.votes_for_test()[0].selected, 2);
    });
    on_dialog(cx, &f, &again, |d, window, cx| d.start_fresh(window, cx));
    cx.update(|cx| {
        let d = again.read(cx);
        assert_eq!(d.editing_for_test(), None);
        assert_eq!(d.texts_for_test().0, "Local review");
        assert_eq!(d.text_for_test(cx), "");
        assert_eq!(d.votes_for_test()[0].selected, 0);
    });
    on_dialog(cx, &f, &again, |d, window, cx| {
        d.set_text_for_test("One more thing", window, cx);
        d.send(window, cx)
    });
    let reviews = reviews_of(&f, number);
    assert_eq!(reviews.len(), 2);
    assert_eq!(reviews[0].message.as_deref(), Some("Looks good"));
    assert_eq!(reviews[1].message.as_deref(), Some("One more thing"));
    // No vote changed: the second review carries none, and takes the
    // project's mode of the moment.
    assert!(reviews[1].labels.is_empty());
    assert_eq!(reviews[1].strategy, model::LocalApproval::Follow);
    assert!(f.http.requested().is_empty());
}

#[gpui_kit::test]
fn a_pending_review_is_edited_in_place_and_escape_drops_the_changes(cx: &mut TestAppContext) {
    let f = local_dialog(cx, |_| {});
    let number = local_number(&f);
    type_text(cx, &f, "First thoughts");
    pick_vote(cx, &f, 0, 3);
    press(cx, &f, &[SEND]);
    let id = reviews_of(&f, number)[0].id;

    // Escape while editing leaves the review as it was, and writes no draft.
    let request = local_request(
        &f.session,
        &f.store,
        "I1",
        "l1",
        model::LocalApproval::Exact,
    );
    let edit = reopen(cx, &f, request.clone());
    on_dialog(cx, &f, &edit, |d, window, cx| {
        d.set_text_for_test("First thoughts and more", window, cx);
        d.close(cx)
    });
    assert_eq!(
        reviews_of(&f, number)[0].message.as_deref(),
        Some("First thoughts")
    );
    assert!(cx.update(|cx| edit.read(cx).editing_for_test()) == Some(id));
    let drafts = f.session.syncer().cached_comments(number).expect("cache");
    assert!(drafts.drafts.is_empty());

    // Saving replaces votes and message; it stays one review.
    let edit = reopen(cx, &f, request);
    on_dialog(cx, &f, &edit, |d, window, cx| {
        d.set_text_for_test("Second thoughts", window, cx);
        d.select_vote(0, 4, cx);
        d.send(window, cx)
    });
    let reviews = reviews_of(&f, number);
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].id, id);
    assert_eq!(reviews[0].message.as_deref(), Some("Second thoughts"));
    assert_eq!(reviews[0].labels, [("Code-Review".to_owned(), 2)].into());
    // The new vote takes the project's mode now.
    assert_eq!(reviews[0].strategy, model::LocalApproval::Exact);
    assert!(f.http.requested().is_empty());
}

#[gpui_kit::test]
fn the_text_of_a_closed_local_review_waits_as_a_local_draft(cx: &mut TestAppContext) {
    let f = local_dialog(cx, |_| {});
    let number = local_number(&f);
    type_text(cx, &f, "half a thought");
    press(cx, &f, &["escape"]);
    assert_eq!(events(&f), ["close"]);
    cx.run_until_parked();
    let drafts = f
        .session
        .syncer()
        .cached_comments(number)
        .expect("cache")
        .drafts;
    assert_eq!(drafts.len(), 1);
    assert!(drafts[0].is_local() && drafts[0].message == "half a thought");
    assert!(reviews_of(&f, number).is_empty());
    // Opened again it is the text, and saving makes it the message.
    let request = local_request(
        &f.session,
        &f.store,
        "I1",
        "l1",
        model::LocalApproval::Exact,
    );
    let again = reopen(cx, &f, request);
    assert_eq!(
        cx.update(|cx| again.read(cx).text_for_test(cx)),
        "half a thought"
    );
    on_dialog(cx, &f, &again, |d, window, cx| d.send(window, cx));
    let reviews = reviews_of(&f, number);
    assert_eq!(reviews[0].message.as_deref(), Some("half a thought"));
    let left = f.session.syncer().cached_comments(number).expect("cache");
    assert!(left.drafts.is_empty(), "{:?}", left.drafts);
    assert!(f.http.requested().is_empty());
}

#[gpui_kit::test]
fn a_local_version_of_a_gerrit_change_gets_a_local_review_with_the_strict_hint(
    cx: &mut TestAppContext,
) {
    let f = dialog_on(
        cx,
        |store| {
            let server = setup().server;
            store
                .save_change_detail(server, &change(), jiff::Timestamp::now())
                .expect("detail");
            crate::testing::local_version(store, Some(CHANGE), "I7", "l7");
            seed(
                store,
                3,
                draft_input("b.rs", Some(1), "pending ordinary", true),
                Seed::Pending,
            );
        },
        |s, st| local_request(s, st, "I7", "l7", model::LocalApproval::Follow),
        |_| {},
    );
    cx.update(|cx| {
        let d = f.dialog.read(cx);
        // The project follows amended commits, but this is a Gerrit change:
        // the strict rule applies.
        assert_eq!(
            d.texts_for_test().2,
            Some("An approval is only sent on this exact commit")
        );
        // Bob's +1 on Gerrit is where the row starts.
        assert_eq!(d.votes_for_test()[0].current, 1);
        assert!(d.people_for_test().is_empty());
        // The ordinary draft waiting for its upload is no local comment and
        // holds nothing up.
        assert!(d.drafts_for_test().is_empty());
    });
    pick_vote(cx, &f, 0, 4);
    press(cx, &f, &[SEND]);
    assert_eq!(events(&f).len(), 1, "{:?}", events(&f));
    let reviews = reviews_of(&f, CHANGE);
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].labels, [("Code-Review".to_owned(), 2)].into());
    assert_eq!(reviews[0].strategy, model::LocalApproval::Follow);
    assert!(writes(&f).is_empty(), "{:?}", writes(&f));
}

#[gpui_kit::test]
fn a_version_already_on_gerrit_takes_no_local_review(cx: &mut TestAppContext) {
    for (state, words) in [
        (model::LocalChangeState::Closed, "closed on Gerrit"),
        (model::LocalChangeState::Consolidated, "on Gerrit by now"),
    ] {
        let f = local_dialog(cx, |_| {});
        // The change was found on Gerrit while the dialog was open.
        {
            let mut store = f.store.lock().expect("store");
            let row = store
                .local_changes_by_change_id(setup().server, "I1")
                .expect("rows")
                .remove(0);
            store
                .set_local_change_state(setup().server, row.id, state)
                .expect("state");
        }
        type_text(cx, &f, "late");
        press(cx, &f, &[SEND]);
        let error = cx
            .update(|cx| f.dialog.read(cx).error_for_test().map(str::to_owned))
            .expect("an error");
        assert!(error.contains(words), "{error}");
        assert!(events(&f).is_empty());
        assert!(f.http.requested().is_empty());
    }
}
