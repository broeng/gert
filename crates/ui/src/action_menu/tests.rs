//! The change action menu on GPUI's `TestAppContext` with a path-routed
//! fake server that sees the method and body of every request.

use std::sync::{Arc, Mutex};

use gerrit_api::{Auth, fake::RouteHttpClient};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
use model::{ActionInfo, ChangeDetail};
use serde_json::{Value, json};
use store::Store;

use super::{ActionDialog, ActionEvent, Kind, menu_entries};
use crate::{
    session::Session,
    testing::detail,
    tests::{URL, env_with, open, setup},
};

/// What the server answers to submit, rebase, abandon and restore: the
/// change.
const CHANGE_OK: &str = concat!(
    ")]}'\n",
    r#"{"_number":7,"project":"p","branch":"main","subject":"s","status":"NEW","owner":{"_account_id":1},"created":"2024-06-15 10:00:00.000000000","updated":"2024-06-15 10:00:00.000000000"}"#
);

const CONFIRM: &str = if cfg!(target_os = "macos") {
    "cmd-enter"
} else {
    "ctrl-enter"
};

fn act(label: &str, enabled: bool, title: Option<&str>) -> ActionInfo {
    ActionInfo {
        method: Some("POST".into()),
        label: Some(label.into()),
        title: title.map(str::to_owned),
        enabled,
        enabled_options: vec![],
    }
}

/// Change 7 as bob sees it: he may submit, rebase (not now: it is at the
/// branch tip), abandon, mark WIP and mark private.
pub(crate) fn change() -> ChangeDetail {
    let mut d = detail(7, 3, 3);
    d.revision_actions.insert(
        "submit".into(),
        act("Submit", true, Some("Submit patch set 3")),
    );
    d.revision_actions.insert(
        "rebase".into(),
        act("Rebase", false, Some("Change is already up to date")),
    );
    d.revision_actions
        .insert("cherrypick".into(), act("Cherry Pick", true, None));
    d.change_actions
        .insert("abandon".into(), act("Abandon", true, None));
    d.change_actions
        .insert("wip".into(), act("WIP", true, None));
    d.change_actions
        .insert("private".into(), act("Mark private", true, None));
    d.change_actions
        .insert("topic".into(), act("Edit topic", true, None));
    d
}

/// Like [`change`], but rebasing is possible.
fn rebasable() -> ChangeDetail {
    let mut d = change();
    d.revision_actions
        .insert("rebase".into(), act("Rebase", true, None));
    d
}

struct Fx {
    window: AnyWindowHandle,
    dialog: Entity<ActionDialog>,
    http: RouteHttpClient,
    events: Arc<Mutex<Vec<String>>>,
}

fn dialog(
    cx: &mut TestAppContext,
    detail: ChangeDetail,
    routes: impl FnOnce(&RouteHttpClient),
) -> Fx {
    let mut store = Store::open_in_memory().expect("store");
    assert_eq!(store.server_id(URL).expect("server"), setup().server);
    let http = RouteHttpClient::new();
    routes(&http);
    let env = env_with(store, &http);
    let mut server = setup();
    server.username = Some("bob".into());
    let auth = Auth::Basic {
        username: "bob".into(),
        password: "p".into(),
    };
    let session = Session::new(&env, &server, Some(auth)).expect("session");
    let events = Arc::new(Mutex::new(Vec::new()));
    let log = events.clone();
    let (window, dialog) = open(cx, |window, cx| {
        let dialog = cx.new(|cx| ActionDialog::new(session, detail, window, cx));
        cx.subscribe(&dialog, move |_, ev: &ActionEvent, _| {
            let text = match ev {
                ActionEvent::Close => "close".to_owned(),
                ActionEvent::Unauthorized => "unauthorized".to_owned(),
                ActionEvent::Refreshed(d) => format!("refreshed {:?}", d.summary.status),
                ActionEvent::Done(d) => {
                    let opened = d
                        .opened
                        .map(|n| format!(" opened=#{n}"))
                        .unwrap_or_default();
                    format!("done {:?} detail={}{opened}", d.kind, d.detail.is_some())
                }
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
        events,
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
    cx.run_until_parked();
}

fn events(f: &Fx) -> Vec<String> {
    f.events.lock().expect("log").clone()
}

fn step(cx: &mut TestAppContext, f: &Fx) -> String {
    cx.update(|cx| f.dialog.read(cx).step_for_test())
}

fn error(cx: &mut TestAppContext, f: &Fx) -> Option<String> {
    cx.update(|cx| f.dialog.read(cx).error_for_test().map(str::to_owned))
}

fn writes(f: &Fx) -> Vec<String> {
    f.http
        .sent()
        .into_iter()
        .filter(|r| r.method != "GET")
        .map(|r| format!("{} {}", r.method, r.target))
        .collect()
}

fn body_of(f: &Fx, target_end: &str) -> Value {
    let found: Vec<_> = f
        .http
        .sent()
        .into_iter()
        .filter(|r| r.method != "GET" && r.target.ends_with(target_end))
        .collect();
    assert_eq!(found.len(), 1, "{target_end}: {:?}", f.http.sent());
    serde_json::from_slice(&found[0].body).expect("a JSON body")
}

#[test]
fn the_menu_lists_what_the_server_offers_in_menu_order() {
    let entries = menu_entries(&change());
    let shown: Vec<_> = entries
        .iter()
        .map(|e| (e.kind, e.label.as_str(), e.enabled, e.reason.as_deref()))
        .collect();
    assert_eq!(
        shown,
        [
            (Kind::Submit, "Submit", true, Some("Submit patch set 3")),
            (
                Kind::Rebase,
                "Rebase",
                false,
                Some("Change is already up to date")
            ),
            (Kind::Abandon, "Abandon", true, None),
            (Kind::Wip, "WIP", true, None),
            (Kind::Private, "Mark private", true, None),
            (Kind::CherryPick, "Cherry Pick", true, None),
            (Kind::Reviewers, "Reviewers and CC", true, None),
            (Kind::Topic, "Edit topic", true, None),
        ],
        "restore, ready, revert, hashtags and the actions the menu does not know are absent"
    );
}

#[test]
fn a_missing_label_falls_back_and_the_other_toggles_show() {
    let mut d = detail(7, 1, 1);
    d.change_actions.insert(
        "ready".into(),
        ActionInfo {
            enabled: true,
            ..Default::default()
        },
    );
    d.change_actions
        .insert("private.delete".into(), act(" ", true, None));
    d.change_actions
        .insert("restore".into(), act("Restore", true, None));
    let shown: Vec<_> = menu_entries(&d)
        .into_iter()
        .map(|e| (e.kind, e.label))
        .collect();
    assert_eq!(
        shown,
        [
            (Kind::Restore, "Restore".to_owned()),
            (Kind::Ready, "Ready for review".to_owned()),
            (Kind::Unprivate, "Unmark private".to_owned()),
            (Kind::Reviewers, "Reviewers and CC".to_owned()),
        ]
    );
}

#[gpui_kit::test]
fn j_k_and_the_arrows_move_the_selection_and_wrap(cx: &mut TestAppContext) {
    let f = dialog(cx, change(), |_| {});
    let sel = |cx: &mut TestAppContext| cx.update(|cx| f.dialog.read(cx).selected_for_test());
    assert_eq!(sel(cx), 0);
    press(cx, &f, &["j", "down", "j"]);
    assert_eq!(sel(cx), 3);
    press(cx, &f, &["k", "up"]);
    assert_eq!(sel(cx), 1);
    press(cx, &f, &["k", "k"]);
    assert_eq!(sel(cx), 7);
    // The menu has no text fields: nothing was typed anywhere.
    assert_eq!(step(cx, &f), "Menu");
}

#[gpui_kit::test]
fn a_disabled_entry_does_nothing_when_picked(cx: &mut TestAppContext) {
    let f = dialog(cx, change(), |_| {});
    press(cx, &f, &["b"]);
    assert_eq!(step(cx, &f), "Menu");
    assert_eq!(cx.update(|cx| f.dialog.read(cx).selected_for_test()), 1);
    press(cx, &f, &["enter"]);
    assert_eq!(step(cx, &f), "Menu");
    assert!(writes(&f).is_empty());
}

#[gpui_kit::test]
fn a_mnemonic_that_is_not_offered_does_nothing(cx: &mut TestAppContext) {
    let f = dialog(cx, change(), |_| {});
    // No restore and no ready on this change.
    press(cx, &f, &["r"]);
    assert_eq!(step(cx, &f), "Menu");
    assert!(events(&f).is_empty());
}

#[gpui_kit::test]
fn escape_closes_the_menu_and_leaves_a_confirmation_for_the_menu(cx: &mut TestAppContext) {
    let f = dialog(cx, change(), |_| {});
    press(cx, &f, &["escape"]);
    assert_eq!(events(&f), ["close"]);
    let f = dialog(cx, change(), |_| {});
    press(cx, &f, &["s"]);
    assert_eq!(step(cx, &f), "Confirm(Submit)");
    press(cx, &f, &["escape"]);
    assert_eq!(step(cx, &f), "Menu");
    assert!(events(&f).is_empty());
    // The menu has the keyboard again.
    press(cx, &f, &["escape"]);
    assert_eq!(events(&f), ["close"]);
    assert!(writes(&f).is_empty());
}

#[gpui_kit::test]
fn submit_asks_then_posts_and_reports_the_refreshed_change(cx: &mut TestAppContext) {
    let f = dialog(cx, change(), |http| {
        http.route("POST /a/changes/7/submit", 200, CHANGE_OK);
        http.route_json(
            "GET /a/changes/7?",
            r#"{"_number":7,"project":"p","branch":"main","subject":"s","status":"MERGED","owner":{"_account_id":1},"created":"2024-06-15 10:00:00.000000000","updated":"2024-06-15 10:00:00.000000000"}"#,
        );
    });
    press(cx, &f, &["s"]);
    assert_eq!(step(cx, &f), "Confirm(Submit)");
    assert!(writes(&f).is_empty(), "nothing goes out before the confirm");
    press(cx, &f, &["enter"]);
    assert_eq!(writes(&f), ["POST /a/changes/7/submit"]);
    assert_eq!(events(&f), ["done Submit detail=true"]);
}

#[gpui_kit::test]
fn a_failed_refresh_still_counts_as_done(cx: &mut TestAppContext) {
    let f = dialog(cx, change(), |http| {
        http.route("POST /a/changes/7/submit", 200, CHANGE_OK);
    });
    press(cx, &f, &["s", CONFIRM]);
    assert_eq!(events(&f), ["done Submit detail=false"]);
}

#[gpui_kit::test]
fn abandon_sends_the_typed_message(cx: &mut TestAppContext) {
    let f = dialog(cx, change(), |http| {
        http.route("POST /a/changes/7/abandon", 200, CHANGE_OK);
    });
    press(cx, &f, &["a"]);
    assert_eq!(step(cx, &f), "Confirm(Abandon)");
    // The message field has the focus: letters are text, not mnemonics.
    let focused = cx
        .update_window(f.window, |_, w, cx| {
            f.dialog.read(cx).message_focus_for_test(cx).is_focused(w)
        })
        .expect("window");
    assert!(focused);
    type_text(cx, &f, "superseded by 8 ");
    press(cx, &f, &[CONFIRM]);
    assert_eq!(
        body_of(&f, "/changes/7/abandon"),
        json!({"message": "superseded by 8"})
    );
    assert_eq!(events(&f), ["done Abandon detail=false"]);
}

#[gpui_kit::test]
fn abandon_without_a_message_sends_none(cx: &mut TestAppContext) {
    let f = dialog(cx, change(), |http| {
        http.route("POST /a/changes/7/abandon", 200, CHANGE_OK);
    });
    press(cx, &f, &["a", CONFIRM]);
    assert_eq!(body_of(&f, "/changes/7/abandon"), json!({}));
}

#[gpui_kit::test]
fn wip_takes_a_message_too(cx: &mut TestAppContext) {
    let f = dialog(cx, change(), |http| {
        http.route("POST /a/changes/7/wip", 200, "");
    });
    press(cx, &f, &["w"]);
    assert_eq!(step(cx, &f), "Confirm(Wip)");
    type_text(cx, &f, "needs tests");
    press(cx, &f, &[CONFIRM]);
    assert_eq!(
        body_of(&f, "/changes/7/wip"),
        json!({"message": "needs tests"})
    );
}

#[gpui_kit::test]
fn rebase_sends_the_base_and_enter_confirms_from_the_field(cx: &mut TestAppContext) {
    let f = dialog(cx, rebasable(), |http| {
        http.route("POST /a/changes/7/rebase", 200, CHANGE_OK);
    });
    press(cx, &f, &["b"]);
    assert_eq!(step(cx, &f), "Confirm(Rebase)");
    let focused = cx
        .update_window(f.window, |_, w, cx| {
            f.dialog.read(cx).base_focus_for_test(cx).is_focused(w)
        })
        .expect("window");
    assert!(focused);
    type_text(cx, &f, "1234");
    press(cx, &f, &["enter"]);
    assert_eq!(body_of(&f, "/changes/7/rebase"), json!({"base": "1234"}));
}

#[gpui_kit::test]
fn rebase_with_an_empty_base_goes_onto_the_branch_tip(cx: &mut TestAppContext) {
    let f = dialog(cx, rebasable(), |http| {
        http.route("POST /a/changes/7/rebase", 200, CHANGE_OK);
    });
    press(cx, &f, &["b", CONFIRM]);
    assert_eq!(body_of(&f, "/changes/7/rebase"), json!({}));
}

/// Rebasable, and the server also offers rebasing on behalf of the uploader.
fn rebasable_for_others() -> ChangeDetail {
    let mut d = rebasable();
    if let Some(a) = d.revision_actions.get_mut("rebase") {
        a.enabled_options = vec!["rebase".into(), "rebase_on_behalf_of_uploader".into()];
    }
    d
}

#[gpui_kit::test]
fn allow_conflicts_is_a_checkbox_that_tab_and_space_reach(cx: &mut TestAppContext) {
    let f = dialog(cx, rebasable(), |http| {
        http.route("POST /a/changes/7/rebase", 200, CHANGE_OK);
    });
    // Base field, then the checkbox; there is no "on behalf" row here, so
    // the next Tab is already past the options.
    press(cx, &f, &["b", "tab", "space", CONFIRM]);
    assert_eq!(
        body_of(&f, "/changes/7/rebase"),
        json!({"allow_conflicts": true})
    );
}

#[gpui_kit::test]
fn on_behalf_of_the_uploader_shows_only_where_the_server_offers_it(cx: &mut TestAppContext) {
    let f = dialog(cx, rebasable_for_others(), |http| {
        http.route("POST /a/changes/7/rebase", 200, CHANGE_OK);
    });
    // Conflicts stay off, only "on behalf" is switched on.
    press(cx, &f, &["b", "tab", "tab", "space", CONFIRM]);
    assert_eq!(
        body_of(&f, "/changes/7/rebase"),
        json!({"on_behalf_of_uploader": true})
    );

    // Not offered: a second Tab leaves the options, Space there does
    // nothing to them and the request carries no option.
    let f = dialog(cx, rebasable(), |http| {
        http.route("POST /a/changes/7/rebase", 200, CHANGE_OK);
    });
    press(cx, &f, &["b", "tab", "tab", "space"]);
    cx.update(|cx| {
        assert!(
            !f.dialog
                .read(cx)
                .rebase_options_for_test()
                .on_behalf_of_uploader
        )
    });
}

#[gpui_kit::test]
fn the_rebase_options_are_clicked_or_toggled_and_both_are_sent(cx: &mut TestAppContext) {
    let f = dialog(cx, rebasable_for_others(), |http| {
        http.route("POST /a/changes/7/rebase", 200, CHANGE_OK);
    });
    press(cx, &f, &["b"]);
    type_text(cx, &f, "9");
    press(cx, &f, &["tab", "space", "tab", "space", CONFIRM]);
    assert_eq!(
        body_of(&f, "/changes/7/rebase"),
        json!({"base": "9", "allow_conflicts": true, "on_behalf_of_uploader": true})
    );
}

#[gpui_kit::test]
fn private_runs_from_the_menu_without_a_dialog(cx: &mut TestAppContext) {
    let f = dialog(cx, change(), |http| {
        http.route("POST /a/changes/7/private", 201, "");
    });
    press(cx, &f, &["p"]);
    assert_eq!(writes(&f), ["POST /a/changes/7/private"]);
    assert_eq!(events(&f), ["done Private detail=false"]);
}

#[gpui_kit::test]
fn ready_runs_from_the_menu_and_unmarking_private_deletes(cx: &mut TestAppContext) {
    let both = || {
        let mut d = detail(7, 1, 1);
        d.change_actions
            .insert("ready".into(), act("Ready for review", true, None));
        d.change_actions
            .insert("private.delete".into(), act("Unmark private", true, None));
        d
    };
    let routes = |http: &RouteHttpClient| {
        http.route("POST /a/changes/7/ready", 200, "");
        http.route("DELETE /a/changes/7/private", 204, "");
    };
    let f = dialog(cx, both(), routes);
    press(cx, &f, &["w"]);
    assert_eq!(writes(&f), ["POST /a/changes/7/ready"]);
    assert_eq!(body_of(&f, "/changes/7/ready"), json!({}));
    assert_eq!(events(&f), ["done Ready detail=false"]);
    let f = dialog(cx, both(), routes);
    press(cx, &f, &["p"]);
    assert_eq!(writes(&f), ["DELETE /a/changes/7/private"]);
    assert_eq!(events(&f), ["done Unprivate detail=false"]);
}

/// Change 7, merged, which bob may revert and cherry-pick.
fn merged() -> ChangeDetail {
    let mut d = change();
    d.summary.subject = "Fix the thing".into();
    d.change_actions
        .insert("revert".into(), act("Revert", true, None));
    d
}

/// What the server answers to a revert or cherry-pick: the new change 12.
const NEW_CHANGE: &str = concat!(
    ")]}'\n",
    r#"{"_number":12,"project":"p","branch":"stable","subject":"Revert","status":"NEW","owner":{"_account_id":1},"created":"2024-06-15 10:00:00.000000000","updated":"2024-06-15 10:00:00.000000000"}"#
);

/// Change 7 with its current revision, as a re-fetch returns it.
const WITH_REVISION: &str = concat!(
    ")]}'\n",
    r#"{"_number":7,"project":"p","branch":"main","subject":"s","status":"MERGED","owner":{"_account_id":1},"created":"2024-06-15 10:00:00.000000000","updated":"2024-06-15 10:00:00.000000000","current_revision":"sha3","revisions":{"sha3":{"_number":3,"actions":{"cherrypick":{"method":"POST","label":"Cherry Pick","enabled":true}}}}}"#
);

#[gpui_kit::test]
fn revert_asks_for_a_prefilled_message_and_opens_the_new_change(cx: &mut TestAppContext) {
    let f = dialog(cx, merged(), |http| {
        http.route("POST /a/changes/7/revert", 200, NEW_CHANGE);
        http.route("GET /a/changes/12?", 200, NEW_CHANGE);
        http.route("GET /a/changes/7?", 200, CHANGE_OK);
    });
    press(cx, &f, &["e"]);
    assert_eq!(step(cx, &f), "Confirm(Revert)");
    let text = cx.update(|cx| f.dialog.read(cx).commit_message_for_test(cx));
    assert_eq!(
        text,
        "Revert \"Fix the thing\"\n\nThis reverts commit sha3.\n\nReason for revert: "
    );
    // The cursor is at the end: the reason is typed right there.
    type_text(cx, &f, "it broke the build");
    press(cx, &f, &[CONFIRM]);
    assert_eq!(writes(&f), ["POST /a/changes/7/revert"]);
    assert_eq!(
        body_of(&f, "/changes/7/revert"),
        json!({"message": "Revert \"Fix the thing\"\n\nThis reverts commit sha3.\n\nReason for revert: it broke the build"})
    );
    assert_eq!(events(&f), ["done Revert detail=true opened=#12"]);
}

#[gpui_kit::test]
fn revert_is_hidden_without_the_action_and_greyed_with_the_servers_title(cx: &mut TestAppContext) {
    let f = dialog(cx, change(), |_| {});
    press(cx, &f, &["e"]);
    assert_eq!(step(cx, &f), "Menu");
    let mut d = merged();
    d.change_actions.insert(
        "revert".into(),
        act("Revert", false, Some("Change is not merged")),
    );
    let entry = menu_entries(&d)
        .into_iter()
        .find(|e| e.kind == Kind::Revert)
        .expect("revert");
    assert!(!entry.enabled);
    assert_eq!(entry.reason.as_deref(), Some("Change is not merged"));
    let f = dialog(cx, d, |_| {});
    press(cx, &f, &["e"]);
    assert_eq!(step(cx, &f), "Menu");
    assert!(writes(&f).is_empty());
}

#[gpui_kit::test]
fn cherry_pick_needs_a_branch_and_sends_the_message(cx: &mut TestAppContext) {
    let f = dialog(cx, merged(), |http| {
        http.route(
            "POST /a/changes/7/revisions/sha3/cherrypick",
            200,
            NEW_CHANGE,
        );
        http.route("GET /a/changes/12?", 200, NEW_CHANGE);
        http.route("GET /a/changes/7?", 200, CHANGE_OK);
    });
    press(cx, &f, &["c"]);
    assert_eq!(step(cx, &f), "Confirm(CherryPick)");
    let text = cx.update(|cx| f.dialog.read(cx).commit_message_for_test(cx));
    assert_eq!(text, "Subject\n\nBody of the message");
    // No branch: nothing is sent.
    press(cx, &f, &[CONFIRM]);
    assert!(writes(&f).is_empty());
    assert_eq!(
        error(cx, &f).as_deref(),
        Some("Enter the branch to cherry-pick to.")
    );
    type_text(cx, &f, "stable-1.0");
    press(cx, &f, &["enter"]);
    assert_eq!(
        body_of(&f, "/changes/7/revisions/sha3/cherrypick"),
        json!({"destination": "stable-1.0", "message": "Subject\n\nBody of the message"})
    );
    assert_eq!(events(&f), ["done CherryPick detail=true opened=#12"]);
}

#[gpui_kit::test]
fn a_cherry_pick_conflict_or_bad_branch_keeps_the_step_with_the_servers_text(
    cx: &mut TestAppContext,
) {
    let f = dialog(cx, merged(), |http| {
        http.route(
            "POST /a/changes/7/revisions/sha3/cherrypick",
            409,
            "Cherry pick failed: merge conflict in a.rs\n",
        );
        http.route("GET /a/changes/7?", 200, WITH_REVISION);
    });
    press(cx, &f, &["c"]);
    type_text(cx, &f, "stable");
    press(cx, &f, &[CONFIRM]);
    assert_eq!(step(cx, &f), "Confirm(CherryPick)");
    assert_eq!(
        error(cx, &f).as_deref(),
        Some("Not cherry-picked: Cherry pick failed: merge conflict in a.rs")
    );
    assert!(events(&f).iter().all(|e| e.starts_with("refreshed")));
    f.http.route(
        "POST /a/changes/7/revisions/sha3/cherrypick",
        400,
        "Branch nope not found\n",
    );
    press(cx, &f, &[CONFIRM]);
    assert_eq!(
        error(cx, &f).as_deref(),
        Some("Not cherry-picked: Branch nope not found")
    );
    // Esc goes back to the menu; what was typed stays when coming back.
    press(cx, &f, &["escape", "c"]);
    assert_eq!(step(cx, &f), "Confirm(CherryPick)");
    assert_eq!(error(cx, &f), None);
}

#[gpui_kit::test]
fn a_409_keeps_the_dialog_with_the_servers_message(cx: &mut TestAppContext) {
    let f = dialog(cx, rebasable(), |http| {
        http.route(
            "POST /a/changes/7/rebase",
            409,
            "Change is already up to date.\n",
        );
    });
    press(cx, &f, &["b", CONFIRM]);
    assert_eq!(step(cx, &f), "Confirm(Rebase)");
    assert_eq!(
        error(cx, &f).as_deref(),
        Some("Not rebased: Change is already up to date.")
    );
    assert!(events(&f).is_empty());
    // It can be tried again, or Esc goes back to the menu and closes.
    press(cx, &f, &["escape"]);
    assert_eq!(step(cx, &f), "Menu");
    assert_eq!(error(cx, &f), None);
    press(cx, &f, &["escape"]);
    assert_eq!(events(&f), ["close"]);
}

#[gpui_kit::test]
fn a_direct_action_that_fails_shows_its_error_in_the_menu(cx: &mut TestAppContext) {
    let f = dialog(cx, change(), |http| {
        http.route("POST /a/changes/7/private", 403, "not allowed here\n");
    });
    press(cx, &f, &["p"]);
    assert_eq!(step(cx, &f), "Menu");
    assert_eq!(
        error(cx, &f).as_deref(),
        Some("Not marked private: not allowed here")
    );
    // Moving on clears it.
    press(cx, &f, &["j"]);
    assert_eq!(error(cx, &f), None);
}

#[gpui_kit::test]
fn a_403_with_no_message_says_so(cx: &mut TestAppContext) {
    let f = dialog(cx, change(), |http| {
        http.route("POST /a/changes/7/submit", 403, "");
    });
    press(cx, &f, &["s", CONFIRM]);
    let e = error(cx, &f).expect("error");
    assert!(e.starts_with("Not submitted: ") && e.contains("403"), "{e}");
    assert!(events(&f).is_empty());
}

#[gpui_kit::test]
fn a_401_goes_to_the_login_flow(cx: &mut TestAppContext) {
    let f = dialog(cx, change(), |http| {
        http.route("POST /a/changes/7/submit", 401, "");
    });
    press(cx, &f, &["s", CONFIRM]);
    assert_eq!(events(&f), ["unauthorized"]);
}

#[gpui_kit::test]
fn a_server_that_cannot_be_reached_says_nothing_was_done(cx: &mut TestAppContext) {
    let f = dialog(cx, change(), |http| {
        http.route("POST /a/changes/7/submit", 503, "down");
    });
    press(cx, &f, &["s", CONFIRM]);
    let e = error(cx, &f).expect("error");
    assert!(
        e.starts_with("Not submitted: the server cannot be reached") && e.contains("Nothing"),
        "{e}"
    );
    assert_eq!(step(cx, &f), "Confirm(Submit)");
}

#[test]
fn a_transport_failure_is_described_with_its_cause_but_without_the_url() {
    let chain = "error sending request for url (https://g.example/a/changes/7/submit): \
                 client error (Connect): tcp connect error: Connection refused (os error 111)";
    let e = sync::Error::Api(gerrit_api::Error::Http(gerrit_api::HttpError(chain.into())));
    let text = super::describe(Kind::Submit, &e);
    assert!(
        text.starts_with("Not submitted: the server cannot be reached"),
        "{text}"
    );
    assert!(
        !text.contains("g.example") && !text.contains("tcp connect error"),
        "{text}"
    );
    assert!(
        text.contains("error sending request: Connection refused (os error 111)"),
        "{text}"
    );
}

const MERGED_JSON: &str = r#"{"_number":7,"project":"p","branch":"main","subject":"s","status":"MERGED","owner":{"_account_id":1},"created":"2024-06-15 10:00:00.000000000","updated":"2024-06-15 10:00:00.000000000"}"#;

#[gpui_kit::test]
fn a_refused_action_fetches_the_change_and_shows_its_state(cx: &mut TestAppContext) {
    let f = dialog(cx, change(), |http| {
        http.route("POST /a/changes/7/submit", 409, "change is merged\n");
        http.route_json("GET /a/changes/7?", MERGED_JSON);
    });
    press(cx, &f, &["s", CONFIRM]);
    // The step stays open with the error, and the page is told the state.
    assert_eq!(step(cx, &f), "Confirm(Submit)");
    assert_eq!(
        error(cx, &f).as_deref(),
        Some("Not submitted: change is merged")
    );
    assert_eq!(events(&f), ["refreshed Merged"]);
    assert_eq!(
        cx.update(|cx| f.dialog.read(cx).detail_for_test().summary.status.clone()),
        model::ChangeStatus::Merged
    );
    // The menu no longer offers what the server does not.
    press(cx, &f, &["escape"]);
    press(cx, &f, &["s"]);
    assert_eq!(step(cx, &f), "Menu");
}

#[gpui_kit::test]
fn a_403_on_a_reviewer_edit_fetches_the_change_too(cx: &mut TestAppContext) {
    let f = dialog(cx, change(), |http| {
        http.route("DELETE /a/changes/7/reviewers/2", 403, "no\n");
        http.route_json("GET /a/changes/7?", MERGED_JSON);
    });
    press(cx, &f, &["v", "tab", "delete"]);
    assert_eq!(step(cx, &f), "Reviewers");
    assert_eq!(events(&f), ["refreshed Merged"]);
    // Bob is not on the change the server has.
    assert!(cx.update(|cx| f.dialog.read(cx).detail_for_test().reviewers.is_empty()));
}

#[gpui_kit::test]
fn an_unreachable_server_or_a_401_fetches_nothing(cx: &mut TestAppContext) {
    let f = dialog(cx, change(), |http| {
        http.route("POST /a/changes/7/submit", 503, "down");
        http.route_json("GET /a/changes/7?", MERGED_JSON);
    });
    press(cx, &f, &["s", CONFIRM]);
    assert!(events(&f).is_empty());
    assert!(f.http.requested().iter().all(|r| !r.starts_with("GET")));
    let f = dialog(cx, change(), |http| {
        http.route("POST /a/changes/7/submit", 401, "");
        http.route_json("GET /a/changes/7?", MERGED_JSON);
    });
    press(cx, &f, &["s", CONFIRM]);
    assert_eq!(events(&f), ["unauthorized"]);
    assert!(f.http.requested().iter().all(|r| !r.starts_with("GET")));
}

#[gpui_kit::test]
fn escape_does_not_cancel_a_running_action(cx: &mut TestAppContext) {
    let f = dialog(cx, change(), |http| {
        http.route("POST /a/changes/7/submit", 200, CHANGE_OK);
    });
    press(cx, &f, &["s"]);
    // Confirm and Esc without letting the request finish in between.
    cx.update_window(f.window, |_, window, cx| {
        window.press(CONFIRM, cx);
        window.press("escape", cx);
    })
    .expect("press");
    assert_eq!(step(cx, &f), "Running(Submit)");
    cx.run_until_parked();
    assert_eq!(events(&f), ["done Submit detail=false"]);
}

#[gpui_kit::test]
fn tab_cycles_through_the_controls_of_a_confirmation(cx: &mut TestAppContext) {
    let f = dialog(cx, change(), |_| {});
    press(cx, &f, &["a"]);
    let is = |cx: &mut TestAppContext, which: u8| {
        cx.update_window(f.window, |_, w, cx| {
            let d = f.dialog.read(cx);
            match which {
                0 => d.message_focus_for_test(cx).is_focused(w),
                _ => d.confirm_focus_for_test().is_focused(w),
            }
        })
        .expect("window")
    };
    assert!(is(cx, 0));
    press(cx, &f, &["tab", "tab"]);
    assert!(is(cx, 1));
    press(cx, &f, &["tab"]);
    assert!(is(cx, 0));
    press(cx, &f, &["shift-tab", "shift-tab"]);
    // message -> confirm -> cancel
    assert!(!is(cx, 0) && !is(cx, 1));
}

mod in_the_shell {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
    use model::ChangeNumber;
    use store::Store;

    use super::change;
    use crate::{
        filters::FileFilter,
        nav::Route,
        session::{Backend, BackendError, SavedQuery, ServerSetup, Startup, StartupPage},
        shell::Shell,
        testing::summary,
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
        detail: model::ChangeDetail,
        http: &gerrit_api::fake::RouteHttpClient,
    ) -> (AnyWindowHandle, Entity<Shell>) {
        let mut store = Store::open_in_memory().expect("store");
        let server = store.server_id(URL).expect("server");
        let now = jiff::Timestamp::now();
        store
            .save_query_page(server, "status:open", &[summary(7)], false, now)
            .expect("query");
        store
            .save_change_detail(server, &detail, now)
            .expect("detail");
        let mut env = env_with(store, http);
        env.backend = std::sync::Arc::new(Keyring);
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
        press(cx, window, "enter");
        cx.update(|cx| assert_eq!(shell.read(cx).route(), &Route::Change(ChangeNumber(7))));
        (window, shell)
    }

    fn press(cx: &mut TestAppContext, window: AnyWindowHandle, key: &'static str) {
        cx.update_window(window, |_, w, cx| w.press(key, cx))
            .expect("press");
        cx.run_until_parked();
    }

    fn hint(cx: &mut TestAppContext, shell: &Entity<Shell>) -> Option<(String, bool)> {
        let page = cx
            .update(|cx| shell.read(cx).change_page_for_test())
            .expect("page");
        cx.update(|cx| page.read(cx).hint_for_test())
    }

    #[gpui_kit::test]
    fn m_opens_the_menu_over_the_page_and_escape_gives_the_page_back(cx: &mut TestAppContext) {
        let (window, shell) = on_the_change(cx, Some("bob"), change(), &Default::default());
        press(cx, window, "m");
        cx.update(|cx| assert!(shell.read(cx).menu_for_test().is_some()));
        // The keys of the shell and the page do nothing behind it.
        press(cx, window, "?");
        press(cx, window, "u");
        cx.update(|cx| {
            assert!(!shell.read(cx).help_open_for_test());
            assert_eq!(shell.read(cx).route(), &Route::Change(ChangeNumber(7)));
            assert!(shell.read(cx).reply_for_test().is_none());
        });
        press(cx, window, "escape");
        cx.update(|cx| assert!(shell.read(cx).menu_for_test().is_none()));
        // The page has the focus again: its keys work (`a` is the reply).
        press(cx, window, "a");
        cx.update(|cx| assert!(shell.read(cx).reply_for_test().is_some()));
    }

    #[gpui_kit::test]
    fn a_revert_opens_the_new_change_and_says_so(cx: &mut TestAppContext) {
        let http = gerrit_api::fake::RouteHttpClient::new();
        http.route("POST /a/changes/7/revert", 200, super::NEW_CHANGE);
        http.route("GET /a/changes/12?", 200, super::NEW_CHANGE);
        http.route(
            "GET /a/changes/7?",
            200,
            concat!(
                ")]}'\n",
                r#"{"_number":7,"project":"p","branch":"main","subject":"s","status":"MERGED","owner":{"_account_id":1},"created":"2024-06-15 10:00:00.000000000","updated":"2024-06-15 10:00:00.000000000","current_revision":"sha3","revisions":{"sha3":{"_number":3}},"actions":{"revert":{"method":"POST","label":"Revert","enabled":true}}}"#
            ),
        );
        let (window, shell) = on_the_change(cx, Some("bob"), super::merged(), &http);
        press(cx, window, "m");
        press(cx, window, "e");
        press(cx, window, super::CONFIRM);
        cx.update(|cx| {
            assert!(shell.read(cx).menu_for_test().is_none());
            assert_eq!(shell.read(cx).route(), &Route::Change(ChangeNumber(12)));
        });
        assert_eq!(
            hint(cx, &shell).map(|h| h.0).as_deref(),
            Some("Revert created as change #12")
        );
    }

    #[gpui_kit::test]
    fn a_mnemonic_is_not_the_pages_key(cx: &mut TestAppContext) {
        // `a` replies on the page and abandons in the menu.
        let (window, shell) = on_the_change(cx, Some("bob"), change(), &Default::default());
        press(cx, window, "m");
        press(cx, window, "a");
        cx.update(|cx| {
            assert!(shell.read(cx).reply_for_test().is_none());
            let menu = shell.read(cx).menu_for_test().expect("menu");
            assert_eq!(menu.read(cx).step_for_test(), "Confirm(Abandon)");
        });
    }

    const BEFORE: &str = r#"{"_number":7,"project":"p","branch":"main","subject":"Subject","status":"NEW","owner":{"_account_id":1},"created":"2024-06-15 10:00:00.000000000","updated":"2024-06-15 10:00:00.000000000","current_revision":"sha3","revisions":{"sha3":{"_number":3,"actions":{"rebase":{"method":"POST","label":"Rebase","enabled":true}}}}}"#;
    const AFTER: &str = r#"{"_number":7,"project":"p","branch":"main","subject":"Rebased subject","status":"NEW","owner":{"_account_id":1},"created":"2024-06-15 10:00:00.000000000","updated":"2024-06-15 10:00:00.000000000","current_revision":"sha4","revisions":{"sha4":{"_number":4}}}"#;

    #[gpui_kit::test]
    fn a_done_action_closes_the_menu_and_the_page_takes_the_change(cx: &mut TestAppContext) {
        let http = gerrit_api::fake::RouteHttpClient::new();
        http.route_json("GET /a/changes/7?", BEFORE);
        http.route("POST /a/changes/7/rebase", 200, super::CHANGE_OK);
        let (window, shell) = on_the_change(cx, Some("bob"), change(), &http);
        press(cx, window, "m");
        press(cx, window, "b");
        // From now on the server has the rebased change.
        http.route_json("GET /a/changes/7?", AFTER);
        press(cx, window, super::CONFIRM);
        cx.update(|cx| assert!(shell.read(cx).menu_for_test().is_none()));
        assert_eq!(hint(cx, &shell), Some(("Rebased".to_owned(), false)));
        let page = cx
            .update(|cx| shell.read(cx).change_page_for_test())
            .expect("page");
        cx.update(|cx| {
            let d = page.read(cx).detail().expect("detail");
            assert_eq!(d.summary.subject, "Rebased subject");
            assert_eq!(d.current_patch_set.map(|p| p.0), Some(4));
        });
    }

    #[gpui_kit::test]
    fn a_refused_action_keeps_the_menu_and_the_page_follows_the_server(cx: &mut TestAppContext) {
        let http = gerrit_api::fake::RouteHttpClient::new();
        http.route_json("GET /a/changes/7?", BEFORE);
        http.route("POST /a/changes/7/rebase", 409, "change is not open\n");
        let (window, shell) = on_the_change(cx, Some("bob"), change(), &http);
        press(cx, window, "m");
        press(cx, window, "b");
        http.route_json("GET /a/changes/7?", AFTER);
        press(cx, window, super::CONFIRM);
        cx.update(|cx| assert!(shell.read(cx).menu_for_test().is_some()));
        let page = cx
            .update(|cx| shell.read(cx).change_page_for_test())
            .expect("page");
        cx.update(|cx| {
            let d = page.read(cx).detail().expect("detail");
            assert_eq!(d.summary.subject, "Rebased subject");
        });
    }

    /// The diff page of `src/a.rs` of change 7, patch set 3, opened from the
    /// change page (its file list is cached).
    fn on_the_diff(
        cx: &mut TestAppContext,
        http: &gerrit_api::fake::RouteHttpClient,
    ) -> (AnyWindowHandle, Entity<Shell>) {
        let mut store = Store::open_in_memory().expect("store");
        let server = store.server_id(URL).expect("server");
        let now = jiff::Timestamp::now();
        let key = model::FilesKey {
            change: ChangeNumber(7),
            base: None,
            target: model::PatchSetRef {
                number: model::PatchSetNumber(3),
                sha: "sha3".into(),
            },
        };
        let files = [crate::testing::file(
            "src/a.rs",
            model::FileStatus::Modified,
        )];
        store
            .save_file_list(server, &key, &files, None, now)
            .expect("files");
        store
            .save_query_page(server, "status:open", &[summary(7)], false, now)
            .expect("query");
        store
            .save_change_detail(server, &change(), now)
            .expect("detail");
        http.route("/diff", 200, crate::testing::whole_file());
        let mut env = env_with(store, http);
        env.backend = std::sync::Arc::new(Keyring);
        let startup = Startup {
            env,
            diff: Default::default(),
            start_page: Default::default(),
            filters: Vec::new(),
            servers: vec![ServerSetup {
                username: Some("bob".into()),
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
        press(cx, window, "enter");
        press(cx, window, "j");
        press(cx, window, "enter");
        cx.update(|cx| assert!(matches!(shell.read(cx).route(), Route::Diff(_))));
        cx.update_window(window, |_, w, cx| w.render_frame(cx))
            .expect("frame");
        cx.run_until_parked();
        (window, shell)
    }

    fn diff_route(cx: &mut TestAppContext, shell: &Entity<Shell>) -> crate::nav::DiffRoute {
        match cx.update(|cx| shell.read(cx).route().clone()) {
            Route::Diff(r) => r,
            other => panic!("not a diff: {other:?}"),
        }
    }

    #[gpui_kit::test]
    fn on_the_diff_page_shift_a_opens_the_menu_and_a_rebase_moves_to_the_new_patch_set(
        cx: &mut TestAppContext,
    ) {
        let http = gerrit_api::fake::RouteHttpClient::new();
        http.route_json("GET /a/changes/7?", BEFORE);
        http.route("POST /a/changes/7/rebase", 200, super::CHANGE_OK);
        let (window, shell) = on_the_diff(cx, &http);
        let before = diff_route(cx, &shell);
        assert_eq!(before.target, model::PatchSetNumber(3));
        press(cx, window, "shift-a");
        cx.update(|cx| assert!(shell.read(cx).menu_for_test().is_some()));
        press(cx, window, "b");
        http.route_json("GET /a/changes/7?", AFTER);
        press(cx, window, super::CONFIRM);
        cx.update(|cx| assert!(shell.read(cx).menu_for_test().is_none()));
        // The same file, now in the new current patch set.
        let after = diff_route(cx, &shell);
        assert_eq!(after.path, before.path);
        assert_eq!(after.target, model::PatchSetNumber(4));
        let page = cx
            .update(|cx| shell.read(cx).diff_page_for_test())
            .expect("diff page");
        cx.update(|cx| {
            let page = page.read(cx);
            assert_eq!(
                page.hint_for_test(),
                Some(("Rebased; showing the new patchset".to_owned(), false))
            );
            let d = page.detail_for_test().expect("detail");
            assert_eq!(d.summary.subject, "Rebased subject");
        });
        // Back in the history, the change page shows the new detail too.
        press(cx, window, "u");
        let page = cx
            .update(|cx| shell.read(cx).change_page_for_test())
            .expect("change page");
        cx.update(|cx| {
            let d = page.read(cx).detail().expect("detail");
            assert_eq!(d.current_patch_set.map(|p| p.0), Some(4));
        });
    }

    #[gpui_kit::test]
    fn a_refused_action_updates_the_diff_page_but_keeps_the_menu(cx: &mut TestAppContext) {
        let http = gerrit_api::fake::RouteHttpClient::new();
        http.route_json("GET /a/changes/7?", BEFORE);
        http.route("POST /a/changes/7/rebase", 409, "change is not open\n");
        let (window, shell) = on_the_diff(cx, &http);
        press(cx, window, "shift-a");
        press(cx, window, "b");
        http.route_json("GET /a/changes/7?", AFTER);
        press(cx, window, super::CONFIRM);
        cx.update(|cx| assert!(shell.read(cx).menu_for_test().is_some()));
        let page = cx
            .update(|cx| shell.read(cx).diff_page_for_test())
            .expect("diff page");
        cx.update(|cx| {
            let d = page.read(cx).detail_for_test().expect("detail");
            assert_eq!(d.summary.subject, "Rebased subject");
        });
        assert_eq!(diff_route(cx, &shell).target, model::PatchSetNumber(3));
    }

    #[gpui_kit::test]
    fn anonymous_sessions_get_a_hint_and_no_menu(cx: &mut TestAppContext) {
        let (window, shell) = on_the_change(cx, None, change(), &Default::default());
        press(cx, window, "m");
        cx.update(|cx| assert!(shell.read(cx).menu_for_test().is_none()));
        assert_eq!(
            hint(cx, &shell),
            Some(("Sign in to change this change".to_owned(), false))
        );
    }

    #[gpui_kit::test]
    fn a_change_without_actions_still_offers_reviewers(cx: &mut TestAppContext) {
        let (window, shell) = on_the_change(
            cx,
            Some("bob"),
            crate::testing::detail(7, 3, 3),
            &Default::default(),
        );
        press(cx, window, "m");
        cx.update(|cx| assert!(shell.read(cx).menu_for_test().is_some()));
    }
}

mod edit_tests;
