//! The notification rule editor in the shell on GPUI's `TestAppContext`,
//! opened from the Notifications tab of the settings page, with a backend
//! that records what it is asked to save.

use std::sync::Arc;

use gerrit_api::fake::FakeHttpClient;
use gpui_kit::component::input::InputState;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
use store::{ServerId, Store};
use sync::notify::{NotificationSettings, Rule, RuleEvent, Scope};

use super::NotifyRuleDialog;
use crate::{
    login::tests::FakeBackend,
    session::{SavedQuery, ServerSetup, Startup},
    shell::Shell,
    testing::detail,
    tests::{URL, env_with, open, setup},
};

const SAVE: &str = if cfg!(target_os = "macos") {
    "cmd-enter"
} else {
    "ctrl-enter"
};

struct Fx {
    window: AnyWindowHandle,
    shell: Entity<Shell>,
    backend: Arc<FakeBackend>,
}

/// The first notification row of the tab on the settings page, with the
/// four default rules after the six rows of options and timing.
const RULES: usize = 16;

/// A shell with the notification `settings`, on the Notifications tab of
/// the settings page. `cache` fills the store of the first server (the
/// second, if any, is server 2).
fn fixture(
    cx: &mut TestAppContext,
    settings: NotificationSettings,
    second: bool,
    fail: bool,
    cache: impl FnOnce(&mut Store, ServerId),
) -> Fx {
    let mut store = Store::open_in_memory().expect("store");
    let first = store.server_id(URL).expect("server");
    let other = store.server_id("https://other.example").expect("server");
    cache(&mut store, first);
    let backend = Arc::new(FakeBackend {
        fail,
        ..Default::default()
    });
    let mut env = env_with(store, &FakeHttpClient::new());
    env.backend = backend.clone();
    let query = |name: &str| SavedQuery {
        name: name.into(),
        query: "status:open".into(),
    };
    let startup = Startup {
        env,
        diff: Default::default(),
        start_page: Default::default(),
        filters: Vec::new(),
        servers: std::iter::once(ServerSetup {
            queries: vec![query("Open"), query("Team")],
            ..setup()
        })
        .chain(second.then(|| ServerSetup {
            key: crate::ServerKey::new("other"),
            name: "Other".into(),
            base_url: "https://other.example".into(),
            server: other,
            queries: vec![query("Team"), query("Mine")],
            ..setup()
        }))
        .collect(),
        active: None,
    };
    let (window, shell) = open(cx, |window, cx| {
        cx.new(|cx| Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    cx.update_window(window, |_, w, cx| {
        shell.update(cx, |s, cx| s.set_notification_settings(settings, w, cx))
    })
    .expect("window");
    let fx = Fx {
        window,
        shell,
        backend,
    };
    press(
        cx,
        &fx,
        &[
            if cfg!(target_os = "macos") {
                "cmd-,"
            } else {
                "ctrl-,"
            },
            "6",
        ],
    );
    fx
}

fn press(cx: &mut TestAppContext, f: &Fx, keys: &[&'static str]) {
    for k in keys {
        cx.update_window(f.window, |_, w, cx| w.press(k, cx))
            .expect("press");
        cx.run_until_parked();
    }
}

/// Moves the settings cursor to `row` of the tab.
fn at(cx: &mut TestAppContext, f: &Fx, row: usize) {
    cx.update(|cx| {
        let page = f.shell.read(cx).settings_page_for_test().expect("page");
        page.update(cx, |p, cx| p.select_row_for_test(row, cx));
    });
}

fn dialog(cx: &mut TestAppContext, f: &Fx) -> Option<Entity<NotifyRuleDialog>> {
    cx.update(|cx| f.shell.read(cx).rule_dialog_for_test())
}

/// Opens the editor on rule `index` of the page's list.
fn open_on(cx: &mut TestAppContext, f: &Fx, index: usize) -> Entity<NotifyRuleDialog> {
    at(cx, f, RULES + index);
    press(cx, f, &["enter"]);
    dialog(cx, f).expect("the editor is open")
}

fn rule(cx: &mut TestAppContext, d: &Entity<NotifyRuleDialog>) -> Rule {
    cx.update(|cx| d.read(cx).rule_for_test(cx))
}

fn writes(f: &Fx) -> Vec<NotificationSettings> {
    f.backend.saved_notifications.lock().expect("lock").clone()
}

fn type_into(cx: &mut TestAppContext, f: &Fx, input: &Entity<InputState>, text: &'static str) {
    cx.update_window(f.window, |_, w, cx| {
        input.update(cx, |s, cx| s.set_value(text, w, cx))
    })
    .expect("set value");
}

fn vote_rule(labels: &[&str], values: &[i32]) -> Rule {
    let mut r = Rule::new(RuleEvent::Vote, Scope::Owner);
    r.name = Some("V".into());
    r.labels = labels.iter().map(|l| (*l).to_owned()).collect();
    r.values = values.to_vec();
    r
}

fn settings_with(rules: Vec<Rule>) -> NotificationSettings {
    NotificationSettings {
        rules,
        ..Default::default()
    }
}

fn nothing(_: &mut Store, _: ServerId) {}

#[gpui_kit::test]
fn enter_edits_the_rule_under_the_cursor_and_a_adds_a_new_one(cx: &mut TestAppContext) {
    let f = fixture(cx, NotificationSettings::default(), false, false, nothing);
    let d = open_on(cx, &f, 1);
    assert_eq!(cx.update(|cx| d.read(cx).index_for_test()), 1);
    let r = rule(cx, &d);
    assert_eq!(
        r.name.as_deref(),
        Some("Negative Code-Review on my changes")
    );
    assert_eq!(
        (r.event, r.labels, r.values),
        (
            RuleEvent::Vote,
            vec!["Code-Review".to_owned()],
            vec![-2, -1]
        )
    );
    // Escape closes it without a write, and the page has the keyboard back.
    press(cx, &f, &["escape"]);
    assert!(dialog(cx, &f).is_none());
    assert!(writes(&f).is_empty());
    press(cx, &f, &["j"]);
    press(cx, &f, &["a"]);
    let d = dialog(cx, &f).expect("open");
    assert_eq!(cx.update(|cx| d.read(cx).index_for_test()), 4);
    let r = rule(cx, &d);
    assert_eq!(
        (r.event, r.scope, r.name),
        (RuleEvent::Comment, Scope::Owner, None)
    );
}

#[gpui_kit::test]
fn left_and_right_cycle_the_events_and_the_scopes(cx: &mut TestAppContext) {
    let f = fixture(cx, NotificationSettings::default(), false, false, nothing);
    press(cx, &f, &["a"]);
    let d = dialog(cx, &f).expect("open");
    // Name, then the events.
    press(cx, &f, &["tab"]);
    let seen = |cx: &mut TestAppContext| rule(cx, &d).event;
    assert_eq!(seen(cx), RuleEvent::Comment);
    press(cx, &f, &["right"]);
    assert_eq!(seen(cx), RuleEvent::AttentionAdded);
    for _ in 0..7 {
        press(cx, &f, &["right"]);
    }
    assert_eq!(seen(cx), RuleEvent::Mention);
    // It wraps both ways.
    press(cx, &f, &["right"]);
    assert_eq!(seen(cx), RuleEvent::Vote);
    press(cx, &f, &["left"]);
    assert_eq!(seen(cx), RuleEvent::Mention);

    // The scopes: six simple ones and the saved queries of the servers.
    press(cx, &f, &["tab"]);
    assert_eq!(cx.update(|cx| d.read(cx).scopes_for_test().len()), 8);
    press(cx, &f, &["right"]);
    assert_eq!(rule(cx, &d).scope, Scope::Reviewer);
    for _ in 0..5 {
        press(cx, &f, &["right"]);
    }
    assert_eq!(rule(cx, &d).scope, Scope::Query("Open".into()));
    press(cx, &f, &["right", "right"]);
    assert_eq!(rule(cx, &d).scope, Scope::Owner);
    press(cx, &f, &["left"]);
    assert_eq!(rule(cx, &d).scope, Scope::Query("Team".into()));
}

#[gpui_kit::test]
fn the_fields_follow_the_event(cx: &mut TestAppContext) {
    let f = fixture(cx, NotificationSettings::default(), false, false, nothing);
    press(cx, &f, &["a"]);
    let d = dialog(cx, &f).expect("open");
    let fields = |cx: &mut TestAppContext| cx.update(|cx| d.read(cx).fields_for_test());
    // One server: no server ticks. Comment: no labels or values.
    assert_eq!(fields(cx), (false, false));
    cx.update(|cx| d.update(cx, |d, cx| d.set_event_for_test(RuleEvent::Vote, cx)));
    assert_eq!(fields(cx), (true, false));
    cx.update(|cx| d.update(cx, |d, cx| d.set_event_for_test(RuleEvent::Merged, cx)));
    assert_eq!(fields(cx), (false, false));
}

#[gpui_kit::test]
fn without_a_cache_the_usual_labels_and_ranges_are_offered(cx: &mut TestAppContext) {
    let settings = settings_with(vec![vote_rule(&[], &[])]);
    let f = fixture(cx, settings, false, false, nothing);
    let d = open_on(cx, &f, 0);
    cx.update(|cx| {
        let d = d.read(cx);
        assert_eq!(d.labels_for_test(), ["Code-Review", "Verified"]);
        // Any label: the values of both.
        assert_eq!(d.values_for_test(), [-2, -1, 0, 1, 2]);
    });
    cx.update(|cx| {
        d.update(cx, |d, cx| {
            d.toggle_any_label_for_test(cx);
            d.toggle_label_for_test("Verified", cx);
        })
    });
    // Only Verified: its range.
    assert_eq!(cx.update(|cx| d.read(cx).values_for_test()), [-1, 0, 1]);
    cx.update(|cx| d.update(cx, |d, cx| d.toggle_label_for_test("Code-Review", cx)));
    assert_eq!(
        cx.update(|cx| d.read(cx).values_for_test()),
        [-2, -1, 0, 1, 2]
    );
    cx.update(|cx| {
        d.update(cx, |d, cx| {
            d.toggle_any_value_for_test(cx);
            d.toggle_value_for_test(-2, cx);
            d.toggle_value_for_test(-1, cx);
        })
    });
    let r = rule(cx, &d);
    assert_eq!(
        (r.labels, r.values),
        (
            vec!["Code-Review".to_owned(), "Verified".to_owned()],
            vec![-2, -1]
        )
    );
}

#[gpui_kit::test]
fn the_cache_decides_the_labels_and_values(cx: &mut TestAppContext) {
    let settings = settings_with(vec![vote_rule(&[], &[])]);
    let f = fixture(cx, settings, false, false, |store, server| {
        let mut d = detail(1, 1, 1);
        d.permitted_labels = [
            ("Code-Review".to_owned(), vec![-1, 0, 1]),
            ("Gate".to_owned(), vec![-3, 0, 3]),
        ]
        .into();
        store
            .save_change_detail(server, &d, jiff::Timestamp::now())
            .expect("detail");
    });
    // The read is in the background: it has landed when the editor is up.
    let d = open_on(cx, &f, 0);
    cx.update(|cx| {
        let d = d.read(cx);
        assert_eq!(d.labels_for_test(), ["Code-Review", "Gate"]);
        assert_eq!(d.values_for_test(), [-3, -1, 0, 1, 3]);
    });
}

#[gpui_kit::test]
fn labels_and_values_of_the_rule_are_kept_even_when_unseen(cx: &mut TestAppContext) {
    let settings = settings_with(vec![vote_rule(&["Legacy"], &[7, -1])]);
    let f = fixture(cx, settings, false, false, nothing);
    let d = open_on(cx, &f, 0);
    cx.update(|cx| {
        let d = d.read(cx);
        assert_eq!(d.labels_for_test(), ["Code-Review", "Legacy", "Verified"]);
        // Legacy has the fallback range, and the 7 stays on offer.
        assert_eq!(d.values_for_test(), [-1, 0, 1, 7]);
    });
    // Saving unchanged writes the same labels and values back.
    press(cx, &f, &[SAVE]);
    let saved = &writes(&f)[0].rules[0];
    assert_eq!(saved.labels, ["Legacy"]);
    assert_eq!(saved.values, [-1, 7]);
}

#[gpui_kit::test]
fn server_ticks_are_offered_with_several_servers_and_none_means_all(cx: &mut TestAppContext) {
    let f = fixture(cx, NotificationSettings::default(), true, false, nothing);
    press(cx, &f, &["a"]);
    let d = dialog(cx, &f).expect("open");
    assert!(cx.update(|cx| d.read(cx).fields_for_test()).1);
    assert!(rule(cx, &d).servers.is_empty());
    cx.update(|cx| {
        d.update(cx, |d, cx| {
            d.toggle_server_for_test("other", cx);
            d.toggle_server_for_test("test", cx);
        })
    });
    // In config order, whichever way they were ticked.
    assert_eq!(rule(cx, &d).servers, ["test", "other"]);
    cx.update(|cx| d.update(cx, |d, cx| d.toggle_server_for_test("test", cx)));
    assert_eq!(rule(cx, &d).servers, ["other"]);
    // The queries of both servers are in the scopes, each once.
    let scopes = cx.update(|cx| d.read(cx).scopes_for_test());
    let queries: Vec<_> = scopes
        .iter()
        .filter_map(|s| match s {
            Scope::Query(n) => Some(n.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(queries, ["Open", "Team", "Mine"]);
}

#[gpui_kit::test]
fn a_vote_rule_needs_a_label_and_a_value_or_any(cx: &mut TestAppContext) {
    let f = fixture(cx, NotificationSettings::default(), false, false, nothing);
    press(cx, &f, &["a"]);
    let d = dialog(cx, &f).expect("open");
    cx.update(|cx| {
        d.update(cx, |d, cx| {
            d.set_event_for_test(RuleEvent::Vote, cx);
            d.toggle_any_label_for_test(cx);
        })
    });
    // Nothing is said before a save is tried.
    assert_eq!(cx.update(|cx| d.read(cx).problem_for_test()), None);
    press(cx, &f, &[SAVE]);
    let why = cx
        .update(|cx| d.read(cx).problem_for_test())
        .expect("problem");
    assert!(why.contains("label"), "{why}");
    assert!(writes(&f).is_empty() && dialog(cx, &f).is_some());
    cx.update(|cx| {
        d.update(cx, |d, cx| {
            d.toggle_label_for_test("Verified", cx);
            d.toggle_any_value_for_test(cx);
        })
    });
    press(cx, &f, &[SAVE]);
    let why = cx
        .update(|cx| d.read(cx).problem_for_test())
        .expect("problem");
    assert!(why.contains("value"), "{why}");
    assert!(writes(&f).is_empty());
    cx.update(|cx| d.update(cx, |d, cx| d.toggle_value_for_test(-1, cx)));
    press(cx, &f, &[SAVE]);
    assert!(dialog(cx, &f).is_none());
    let saved = writes(&f);
    assert_eq!(saved.len(), 1);
    let new = saved[0].rules.last().expect("rule");
    assert_eq!(
        (new.labels.clone(), new.values.clone()),
        (vec!["Verified".to_owned()], vec![-1])
    );
}

#[gpui_kit::test]
fn saving_writes_through_the_backend_and_the_shell_and_page_follow(cx: &mut TestAppContext) {
    let f = fixture(cx, NotificationSettings::default(), false, false, nothing);
    press(cx, &f, &["a"]);
    let d = dialog(cx, &f).expect("open");
    let (name, from) = cx.update(|cx| {
        let d = d.read(cx);
        (d.name_input_for_test(), d.actors_input_for_test())
    });
    type_into(cx, &f, &name, " Team comments ");
    type_into(cx, &f, &from, "alice, bob@x.org");
    cx.update(|cx| d.update(cx, |d, cx| d.set_event_for_test(RuleEvent::Merged, cx)));
    press(cx, &f, &[SAVE]);
    assert!(dialog(cx, &f).is_none());
    // The defaults were written out with the new rule after them.
    let saved = writes(&f);
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].rules.len(), 5);
    let new = &saved[0].rules[4];
    assert_eq!(new.name.as_deref(), Some("Team comments"));
    assert_eq!(new.event, RuleEvent::Merged);
    assert_eq!(new.from, ["alice", "bob@x.org"]);
    cx.update(|cx| {
        let shell = f.shell.read(cx);
        assert_eq!(shell.notify_settings_for_test().rules.len(), 5);
        let page = shell.settings_page_for_test().expect("page");
        assert!(!page.read(cx).rules_are_defaults_for_test());
        // The cursor is on the new rule.
        assert_eq!(page.read(cx).cursor_for_test(), RULES + 4);
    });
    // Editing it again changes it in place.
    press(cx, &f, &["enter"]);
    let d = dialog(cx, &f).expect("open");
    assert_eq!(cx.update(|cx| d.read(cx).index_for_test()), 4);
    press(cx, &f, &["tab", "tab", "tab"]);
    cx.update(|cx| d.update(cx, |d, cx| d.set_event_for_test(RuleEvent::Abandoned, cx)));
    press(cx, &f, &[SAVE]);
    let saved = writes(&f);
    assert_eq!(saved[1].rules.len(), 5);
    assert_eq!(saved[1].rules[4].event, RuleEvent::Abandoned);
}

#[gpui_kit::test]
fn a_failed_save_keeps_the_form_and_changes_nothing(cx: &mut TestAppContext) {
    let f = fixture(cx, NotificationSettings::default(), false, true, nothing);
    press(cx, &f, &["a"]);
    let d = dialog(cx, &f).expect("open");
    press(cx, &f, &[SAVE]);
    assert!(dialog(cx, &f).is_some());
    let e = cx
        .update(|cx| d.read(cx).error_for_test().map(str::to_owned))
        .expect("error");
    assert!(e.contains("disk full"), "{e}");
    cx.update(|cx| {
        let shell = f.shell.read(cx);
        assert_eq!(shell.notify_settings_for_test().rules.len(), 4);
        let page = shell.settings_page_for_test().expect("page");
        assert!(page.read(cx).rules_are_defaults_for_test());
    });
    // Escape still leaves.
    press(cx, &f, &["escape"]);
    assert!(dialog(cx, &f).is_none());
}

#[gpui_kit::test]
fn space_ticks_the_focused_box_and_tab_reaches_the_buttons(cx: &mut TestAppContext) {
    let settings = settings_with(vec![vote_rule(&[], &[])]);
    let f = fixture(cx, settings, false, false, nothing);
    let d = open_on(cx, &f, 0);
    // Name, event, scope, Any label: Space unticks it.
    press(cx, &f, &["tab", "tab", "tab", "space"]);
    assert_eq!(
        cx.update(|cx| d.read(cx).problem_for_test()),
        None,
        "nothing said before a save"
    );
    press(cx, &f, &[SAVE]);
    assert!(cx.update(|cx| d.read(cx).problem_for_test()).is_some());
    // Shift-Tab goes back, and the cycle wraps around the other way.
    press(cx, &f, &["shift-tab"]);
    press(cx, &f, &["escape"]);
    assert!(dialog(cx, &f).is_none());
}
