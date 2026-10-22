//! The reviewers, topic and hashtags steps of the action menu.

use std::time::Duration;

use super::*;
use crate::action_menu::tags::parse_hashtags;

const SUGGEST_ROUTE: &str = "GET /a/changes/7/suggest_reviewers";

/// Bob (2) is a reviewer already; Dee (4) and the group are new.
const SUGGESTIONS: &str = r#"[
  {"account":{"_account_id":2,"name":"Bob Reviewer"}},
  {"account":{"_account_id":4,"name":"Dee"}},
  {"group":{"id":"grp1","name":"Big Team"},"count":120,"confirm":true}
]"#;

/// The change after Dee became a reviewer.
fn with_dee() -> String {
    format!(
        ")]}}'\n{}",
        r#"{"_number":7,"project":"p","branch":"main","subject":"s","status":"NEW","owner":{"_account_id":1},"created":"2024-06-15 10:00:00.000000000","updated":"2024-06-15 10:00:00.000000000","reviewers":{"REVIEWER":[{"_account_id":2,"name":"Bob Reviewer"},{"_account_id":4,"name":"Dee"}]}}"#
    )
}

fn hashtagged() -> ChangeDetail {
    let mut d = change();
    d.summary.hashtags = vec!["old".into(), "keep".into()];
    d.summary.topic = Some("t1".into());
    d.change_actions
        .insert("hashtags".into(), act("Edit hashtags", true, None));
    d
}

fn advance(cx: &mut TestAppContext, ms: u64) {
    cx.executor().advance_clock(Duration::from_millis(ms));
    cx.run_until_parked();
}

fn suggest_requests(f: &Fx) -> Vec<String> {
    f.http
        .requested()
        .into_iter()
        .filter(|t| t.contains("suggest_reviewers"))
        .collect()
}

fn suggestions(cx: &mut TestAppContext, f: &Fx) -> Vec<String> {
    cx.update(|cx| f.dialog.read(cx).suggestions_for_test())
}

/// The reviewers step with the suggestion route in place.
fn reviewers_step(cx: &mut TestAppContext) -> Fx {
    let f = dialog(cx, change(), |http| {
        http.route_json(SUGGEST_ROUTE, SUGGESTIONS);
        http.route("GET /a/changes/7?", 200, with_dee());
        http.route_json(
            "POST /a/changes/7/reviewers",
            r#"{"input":"4","reviewers":[{"_account_id":4}]}"#,
        );
        http.route("DELETE /a/changes/7/reviewers/2", 204, "");
    });
    press(cx, &f, &["v"]);
    assert_eq!(step(cx, &f), "Reviewers");
    f
}

#[gpui_kit::test]
fn typing_asks_once_after_the_pause_and_a_blank_field_asks_nothing(cx: &mut TestAppContext) {
    let f = reviewers_step(cx);
    type_text(cx, &f, "d");
    advance(cx, 100);
    type_text(cx, &f, "e");
    advance(cx, 150);
    assert!(
        suggest_requests(&f).is_empty(),
        "the pause starts over with each key"
    );
    advance(cx, 100);
    let asked = suggest_requests(&f);
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(asked[0].contains("q=de"), "{asked:?}");

    // Emptying the field drops the suggestions and asks for nothing.
    press(cx, &f, &["backspace", "backspace"]);
    assert!(suggestions(cx, &f).is_empty());
    advance(cx, 500);
    assert_eq!(suggest_requests(&f).len(), 1);
}

#[gpui_kit::test]
fn a_late_answer_for_older_text_is_dropped(cx: &mut TestAppContext) {
    let f = reviewers_step(cx);
    type_text(cx, &f, "de");
    advance(cx, 250);
    assert_eq!(suggestions(cx, &f), ["4", "grp1"]);
    // New text drops the old list at once and the new answer replaces it.
    type_text(cx, &f, "e");
    assert!(suggestions(cx, &f).is_empty());
    f.http.route_json(
        SUGGEST_ROUTE,
        r#"[{"account":{"_account_id":9,"name":"Eve"}}]"#,
    );
    advance(cx, 250);
    assert_eq!(suggestions(cx, &f), ["9"]);
    // A stale answer (for text the field no longer holds) is ignored.
    cx.update(|cx| {
        f.dialog.update(cx, |d, cx| {
            d.suggested_for_test("de", vec![], cx);
        })
    });
    assert_eq!(suggestions(cx, &f), ["9"]);
}

#[gpui_kit::test]
fn people_on_the_change_are_not_suggested(cx: &mut TestAppContext) {
    let f = reviewers_step(cx);
    type_text(cx, &f, "b");
    advance(cx, 250);
    // Bob (2) is a reviewer: only Dee and the group remain.
    assert_eq!(suggestions(cx, &f), ["4", "grp1"]);
}

#[gpui_kit::test]
fn enter_adds_as_reviewer_and_the_step_stays_open(cx: &mut TestAppContext) {
    let f = reviewers_step(cx);
    type_text(cx, &f, "dee");
    advance(cx, 250);
    press(cx, &f, &["enter"]);
    assert_eq!(
        body_of(&f, "/changes/7/reviewers"),
        json!({"reviewer": "4", "state": "REVIEWER"})
    );
    assert_eq!(step(cx, &f), "Reviewers");
    assert!(events(&f).is_empty());
    let names = cx.update(|cx| {
        f.dialog
            .read(cx)
            .detail_for_test()
            .reviewers
            .iter()
            .map(|a| a.id.0)
            .collect::<Vec<_>>()
    });
    assert_eq!(names, [2, 4]);
    // The field is empty again and ready for the next person.
    assert!(suggestions(cx, &f).is_empty());
    // Esc goes back to the menu, a second one closes the dialog, and the
    // page gets the detail.
    press(cx, &f, &["escape"]);
    assert_eq!(step(cx, &f), "Menu");
    assert!(events(&f).is_empty());
    press(cx, &f, &["escape"]);
    assert_eq!(events(&f), ["done Reviewers detail=true"]);
}

#[gpui_kit::test]
fn shift_enter_adds_as_cc(cx: &mut TestAppContext) {
    let f = reviewers_step(cx);
    type_text(cx, &f, "dee");
    advance(cx, 250);
    press(cx, &f, &["down", "up", "shift-enter"]);
    assert_eq!(
        body_of(&f, "/changes/7/reviewers"),
        json!({"reviewer": "4", "state": "CC"})
    );
}

#[gpui_kit::test]
fn a_large_group_asks_and_the_second_enter_confirms(cx: &mut TestAppContext) {
    let f = reviewers_step(cx);
    f.http.route_json(
        "POST /a/changes/7/reviewers",
        r#"{"input":"grp1","confirm":true,"error":"Group Big Team has 120 members."}"#,
    );
    type_text(cx, &f, "big");
    advance(cx, 250);
    press(cx, &f, &["down", "enter"]);
    let body = body_of(&f, "/changes/7/reviewers");
    assert_eq!(body, json!({"reviewer": "grp1", "state": "REVIEWER"}));
    let notice = cx
        .update(|cx| f.dialog.read(cx).notice_for_test())
        .expect("a question");
    assert!(
        notice.contains("120 members") && notice.contains("Enter again"),
        "{notice}"
    );
    assert!(events(&f).is_empty());

    f.http.route_json(
        "POST /a/changes/7/reviewers",
        r#"{"input":"grp1","reviewers":[{"_account_id":4}]}"#,
    );
    press(cx, &f, &["enter"]);
    let sent: Vec<Value> = f
        .http
        .sent()
        .into_iter()
        .filter(|r| r.method == "POST" && r.target.ends_with("/reviewers"))
        .map(|r| serde_json::from_slice(&r.body).expect("json"))
        .collect();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["confirmed"], json!(true), "{sent:?}");
    assert!(
        cx.update(|cx| f.dialog.read(cx).notice_for_test())
            .is_none()
    );
}

#[gpui_kit::test]
fn an_error_result_keeps_the_step_open_with_the_message(cx: &mut TestAppContext) {
    let f = reviewers_step(cx);
    f.http.route_json(
        "POST /a/changes/7/reviewers",
        r#"{"input":"zed@x","error":"Account 'zed@x' not found\nTry the full name."}"#,
    );
    // Nothing is suggested for it: what was typed is sent.
    f.http.route_json(SUGGEST_ROUTE, "[]");
    type_text(cx, &f, "zed@x");
    advance(cx, 250);
    press(cx, &f, &["enter"]);
    assert_eq!(
        body_of(&f, "/changes/7/reviewers"),
        json!({"reviewer": "zed@x", "state": "REVIEWER"})
    );
    assert_eq!(step(cx, &f), "Reviewers");
    let e = error(cx, &f).expect("an error");
    assert!(
        e.contains("not found") && e.contains('\n') && e.contains("full name"),
        "{e}"
    );
    assert!(events(&f).is_empty());
}

#[gpui_kit::test]
fn delete_on_a_listed_reviewer_removes_it(cx: &mut TestAppContext) {
    let f = reviewers_step(cx);
    // Tab from the field goes to the list.
    press(cx, &f, &["tab", "delete"]);
    assert_eq!(writes(&f), ["DELETE /a/changes/7/reviewers/2"]);
    assert_eq!(step(cx, &f), "Reviewers");
    press(cx, &f, &["escape", "escape"]);
    assert_eq!(events(&f), ["done Reviewers detail=true"]);
}

#[gpui_kit::test]
fn enter_on_a_listed_reviewer_moves_it_to_cc(cx: &mut TestAppContext) {
    let f = reviewers_step(cx);
    press(cx, &f, &["tab", "enter"]);
    assert_eq!(
        body_of(&f, "/changes/7/reviewers"),
        json!({"reviewer": "2", "state": "CC"})
    );
}

#[gpui_kit::test]
fn closing_without_a_change_is_a_plain_close(cx: &mut TestAppContext) {
    let f = reviewers_step(cx);
    press(cx, &f, &["escape"]);
    assert_eq!(step(cx, &f), "Menu");
    press(cx, &f, &["escape"]);
    assert_eq!(events(&f), ["close"]);
}

#[gpui_kit::test]
fn a_403_on_a_reviewer_edit_shows_the_server_message(cx: &mut TestAppContext) {
    let f = reviewers_step(cx);
    f.http.route(
        "DELETE /a/changes/7/reviewers/2",
        403,
        "not allowed to remove\n",
    );
    press(cx, &f, &["tab", "delete"]);
    assert_eq!(
        error(cx, &f).as_deref(),
        Some("Reviewers not changed: not allowed to remove")
    );
    assert_eq!(step(cx, &f), "Reviewers");
}

#[gpui_kit::test]
fn the_topic_is_prefilled_and_saved_or_cleared(cx: &mut TestAppContext) {
    let f = dialog(cx, hashtagged(), |http| {
        http.route_json("PUT /a/changes/7/topic", r#""new""#);
        http.route("DELETE /a/changes/7/topic", 204, "");
    });
    press(cx, &f, &["t"]);
    assert_eq!(step(cx, &f), "Topic");
    // Prefilled: typing appends.
    type_text(cx, &f, "x");
    press(cx, &f, &["enter"]);
    assert_eq!(body_of(&f, "/changes/7/topic"), json!({"topic": "t1x"}));
    assert_eq!(events(&f), ["done Topic detail=false"]);

    let f = dialog(cx, hashtagged(), |http| {
        http.route("DELETE /a/changes/7/topic", 204, "");
    });
    press(cx, &f, &["t", "backspace", "backspace", "enter"]);
    assert_eq!(writes(&f), ["DELETE /a/changes/7/topic"]);
    assert_eq!(events(&f), ["done Topic detail=false"]);
}

#[gpui_kit::test]
fn escape_in_the_topic_and_hashtag_steps_goes_back_to_the_menu(cx: &mut TestAppContext) {
    let f = dialog(cx, hashtagged(), |_| {});
    press(cx, &f, &["t", "escape"]);
    assert_eq!(step(cx, &f), "Menu");
    press(cx, &f, &["h", "escape"]);
    assert_eq!(step(cx, &f), "Menu");
    assert!(events(&f).is_empty());
    press(cx, &f, &["escape"]);
    assert_eq!(events(&f), ["close"]);
}

#[gpui_kit::test]
fn an_unchanged_topic_sends_nothing(cx: &mut TestAppContext) {
    let f = dialog(cx, hashtagged(), |_| {});
    press(cx, &f, &["t", "enter"]);
    assert!(writes(&f).is_empty());
    assert_eq!(events(&f), ["close"]);
}

#[gpui_kit::test]
fn a_topic_refused_with_409_stays_with_the_message(cx: &mut TestAppContext) {
    let f = dialog(cx, hashtagged(), |http| {
        http.route("PUT /a/changes/7/topic", 409, "change is closed\n");
    });
    press(cx, &f, &["t"]);
    type_text(cx, &f, "z");
    press(cx, &f, &["enter"]);
    assert_eq!(
        error(cx, &f).as_deref(),
        Some("Topic not changed: change is closed")
    );
    assert_eq!(step(cx, &f), "Topic");
    assert!(events(&f).is_empty());
}

#[gpui_kit::test]
fn hashtags_apply_the_add_and_remove_diff_in_one_call(cx: &mut TestAppContext) {
    let f = dialog(cx, hashtagged(), |http| {
        http.route_json("POST /a/changes/7/hashtags", r#"["keep","a","b"]"#);
    });
    press(cx, &f, &["h"]);
    assert_eq!(step(cx, &f), "Hashtags");
    type_text(cx, &f, "#a, b keep");
    // Tab to the first chip ("old") and mark it for removal.
    press(cx, &f, &["tab", "delete", CONFIRM]);
    assert_eq!(
        body_of(&f, "/changes/7/hashtags"),
        json!({"add": ["a", "b"], "remove": ["old"]})
    );
    assert_eq!(events(&f), ["done Hashtags detail=false"]);
}

#[gpui_kit::test]
fn hashtags_without_a_change_close_without_a_request(cx: &mut TestAppContext) {
    let f = dialog(cx, hashtagged(), |_| {});
    press(cx, &f, &["h", "enter"]);
    assert!(writes(&f).is_empty());
    assert_eq!(events(&f), ["close"]);
}

#[test]
fn hashtags_are_split_on_commas_and_spaces() {
    assert_eq!(parse_hashtags("a,b c"), ["a", "b", "c"].map(String::from));
}

#[test]
fn topic_and_hashtags_are_offered_only_when_the_server_has_them() {
    let kinds = |d: &ChangeDetail| {
        menu_entries(d)
            .into_iter()
            .map(|e| e.kind)
            .collect::<Vec<_>>()
    };
    let none = detail(7, 1, 1);
    assert_eq!(kinds(&none), [Kind::Reviewers]);
    assert_eq!(
        kinds(&hashtagged()),
        [
            Kind::Submit,
            Kind::Rebase,
            Kind::Abandon,
            Kind::Wip,
            Kind::Private,
            Kind::Reviewers,
            Kind::Topic,
            Kind::Hashtags
        ]
    );
}

#[gpui_kit::test]
fn a_401_in_a_step_goes_to_the_login_flow(cx: &mut TestAppContext) {
    let f = dialog(cx, hashtagged(), |http| {
        http.route("PUT /a/changes/7/topic", 401, "");
    });
    press(cx, &f, &["t"]);
    type_text(cx, &f, "z");
    press(cx, &f, &["enter"]);
    assert_eq!(events(&f), ["unauthorized"]);
}
