use std::time::Duration;

use jiff::civil::time;
use model::{Account, AccountId, ChangeNumber, Event, EventKind, PatchSetNumber, ServerId};

use super::*;
use crate::notify::text;

fn account(name: &str) -> Account {
    Account {
        id: AccountId(5),
        name: Some(name.into()),
        email: None,
        username: None,
        display_name: None,
    }
}

fn at(secs: i64) -> Timestamp {
    Timestamp::from_second(1_700_000_000 + secs).unwrap()
}

fn event(change: u64, kind: EventKind, secs: i64) -> Event {
    Event {
        server: ServerId(1),
        change: ChangeNumber(change),
        project: "proj".into(),
        subject: "Fix it".into(),
        owner: None,
        actor: Some(account("Alice")),
        at: at(secs),
        kind,
        subject_account: None,
    }
}

fn vote(label: &str, value: i32) -> EventKind {
    EventKind::Vote {
        label: label.into(),
        value,
        previous: None,
    }
}

fn comment(text: &str) -> EventKind {
    EventKind::Comment {
        message_id: "m".into(),
        snippet: text.into(),
    }
}

/// Shown settings: notify even while focused.
fn settings() -> NotificationSettings {
    NotificationSettings {
        only_when_unfocused: false,
        suppress_current_change: false,
        ..Default::default()
    }
}

const AWAY: Screen<'static> = Screen {
    focused: false,
    current: None,
};

/// Long after every event of these tests.
fn started() -> Timestamp {
    at(-10_000)
}

fn run(settings: &NotificationSettings, events: Vec<Event>) -> Plan {
    let matched = events.into_iter().map(|e| (e, 0)).collect();
    plan(
        settings,
        "srv",
        matched,
        &AWAY,
        time(12, 0, 0, 0),
        started(),
    )
}

#[test]
fn titles_follow_the_event() {
    let t = |kind| text::title(&event(12345, kind, 0), false);
    assert_eq!(t(vote("Code-Review", -1)), "Code-Review -1 from Alice");
    assert_eq!(t(vote("Code-Review", 2)), "Code-Review +2 from Alice");
    assert_eq!(
        t(vote("Code-Review", 0)),
        "Vote removed: Code-Review by Alice"
    );
    assert_eq!(t(comment("x")), "New comment from Alice");
    assert_eq!(t(EventKind::AttentionAdded), "Your turn on #12345");
    assert_eq!(
        t(EventKind::ReviewerAdded { cc: false }),
        "Added as reviewer on #12345"
    );
    assert_eq!(
        t(EventKind::ReviewerAdded { cc: true }),
        "Added as CC on #12345"
    );
    assert_eq!(
        t(EventKind::NewPatchSet {
            number: PatchSetNumber(3)
        }),
        "New patch set 3 on #12345"
    );
    assert_eq!(t(EventKind::Merged), "Merged #12345");
    assert_eq!(t(EventKind::Abandoned), "Abandoned #12345");
    assert_eq!(t(EventKind::ReadyToSubmit), "Ready to submit #12345");
    assert_eq!(t(EventKind::MergeConflict), "Merge conflict on #12345");
    assert_eq!(
        text::title(&event(1, comment("x"), 0), true),
        "Mentioned by Alice"
    );
    let mut nobody = event(1, comment("x"), 0);
    nobody.actor = None;
    assert_eq!(text::title(&nobody, false), "New comment from someone");
}

#[test]
fn bodies_name_the_change_and_quote_comments() {
    assert_eq!(
        text::body(&event(12345, EventKind::Merged, 0)),
        "proj: Fix it (#12345)"
    );
    assert_eq!(
        text::body(&event(12345, comment("  Looks\n\n  odd   to me \n"), 0)),
        "proj: Fix it (#12345)\nLooks odd to me"
    );
    // Nothing after the header line: no empty second line.
    assert_eq!(
        text::body(&event(1, comment(" \n "), 0)),
        "proj: Fix it (#1)"
    );
}

#[test]
fn long_snippets_are_cut_with_an_ellipsis() {
    let short = text::shorten(&"word ".repeat(100));
    assert_eq!(short.chars().count(), 140);
    assert!(short.ends_with("word\u{2026}"));
    assert_eq!(text::shorten(&"x".repeat(140)), "x".repeat(140));
    assert_eq!(text::shorten(&"x".repeat(141)).chars().count(), 141);
}

#[test]
fn one_event_is_a_plain_notification() {
    let p = run(&settings(), vec![event(7, vote("Code-Review", -1), 0)]);
    assert_eq!(
        p.notifications,
        vec![Notification {
            title: "Code-Review -1 from Alice".into(),
            body: "proj: Fix it (#7)".into(),
            server: "srv".into(),
            change: Some(ChangeNumber(7)),
        }]
    );
    assert_eq!(p.delivered.len(), 1);
    assert!(p.delivered[0].shown_on_os);
}

#[test]
fn events_within_the_window_make_one_summary() {
    let events = vec![
        event(7, vote("Code-Review", -1), 0),
        event(7, comment("hm"), 5),
        event(8, EventKind::Merged, 10),
        event(9, EventKind::AttentionAdded, 15),
        event(9, EventKind::MergeConflict, 20),
    ];
    let p = run(&settings(), events);
    assert_eq!(p.notifications.len(), 1);
    let n = &p.notifications[0];
    assert_eq!(n.title, "5 updates on 3 changes");
    assert_eq!(
        n.body,
        "Code-Review -1 from Alice (#7)\nNew comment from Alice (#7)\nMerged #8\nand 2 more"
    );
    assert_eq!(n.change, None);
    assert_eq!(p.delivered.len(), 5);
}

#[test]
fn a_summary_of_one_change_opens_it() {
    let p = run(
        &settings(),
        vec![event(7, comment("a"), 0), event(7, comment("b"), 1)],
    );
    assert_eq!(p.notifications[0].title, "2 updates on 1 change");
    assert_eq!(p.notifications[0].change, Some(ChangeNumber(7)));
}

#[test]
fn events_further_apart_than_the_window_stay_apart() {
    let mut s = settings();
    s.group_window = Duration::from_secs(20);
    let p = run(
        &s,
        vec![
            event(1, EventKind::Merged, 0),
            event(2, EventKind::Merged, 15),
            event(3, EventKind::Merged, 100),
        ],
    );
    let titles: Vec<_> = p.notifications.iter().map(|n| n.title.as_str()).collect();
    assert_eq!(titles, ["2 updates on 2 changes", "Merged #3"]);
}

#[test]
fn mention_rules_read_as_mentions() {
    let mut s = settings();
    s.rules = vec![
        crate::notify::Rule::new(RuleEvent::Comment, crate::notify::Scope::Any),
        crate::notify::Rule::new(RuleEvent::Mention, crate::notify::Scope::Any),
    ];
    assert_eq!(pick_rule(&s, &[0, 1]), Some(1));
    assert_eq!(pick_rule(&s, &[0]), Some(0));
    assert_eq!(pick_rule(&s, &[]), None);
    let p = plan(
        &s,
        "srv",
        vec![(event(1, comment("@me hi"), 0), 1)],
        &AWAY,
        time(12, 0, 0, 0),
        started(),
    );
    assert_eq!(p.notifications[0].title, "Mentioned by Alice");
}

#[test]
fn quiet_hours_keep_events_but_show_nothing() {
    let mut s = settings();
    s.quiet_hours = Some(crate::notify::QuietHours::parse("22:00-07:00").unwrap());
    let p = plan(
        &s,
        "srv",
        vec![(event(1, EventKind::Merged, 0), 0)],
        &AWAY,
        time(23, 30, 0, 0),
        started(),
    );
    assert!(p.notifications.is_empty());
    assert_eq!(p.delivered.len(), 1);
    assert!(!p.delivered[0].shown_on_os);
    // Outside the window the same event is shown.
    let p = plan(
        &s,
        "srv",
        vec![(event(1, EventKind::Merged, 0), 0)],
        &AWAY,
        time(8, 0, 0, 0),
        started(),
    );
    assert_eq!(p.notifications.len(), 1);
}

#[test]
fn suppression_decisions() {
    let on = |focused, current| Screen { focused, current };
    let c = ChangeNumber(7);
    let noon = time(12, 0, 0, 0);
    let mut s = NotificationSettings::default();

    // Defaults: nothing while focused, everything while away.
    assert!(suppressed(&s, &on(true, None), "srv", c, noon));
    assert!(!suppressed(&s, &on(false, None), "srv", c, noon));

    s.only_when_unfocused = false;
    assert!(!suppressed(&s, &on(true, None), "srv", c, noon));
    // The change on screen, on this server, with the focus.
    let here = Some(("srv", c));
    assert!(suppressed(&s, &on(true, here), "srv", c, noon));
    assert!(!suppressed(&s, &on(true, here), "other", c, noon));
    assert!(!suppressed(
        &s,
        &on(true, here),
        "srv",
        ChangeNumber(8),
        noon
    ));
    assert!(!suppressed(&s, &on(false, here), "srv", c, noon));
    s.suppress_current_change = false;
    assert!(!suppressed(&s, &on(true, here), "srv", c, noon));

    s.quiet_hours = Some(crate::notify::QuietHours::parse("12:00-13:00").unwrap());
    assert!(suppressed(&s, &on(false, None), "srv", c, noon));
}

#[test]
fn suppressed_events_do_not_enter_a_summary() {
    let mut s = settings();
    s.suppress_current_change = true;
    let screen = Screen {
        focused: true,
        current: Some(("srv", ChangeNumber(1))),
    };
    let p = plan(
        &s,
        "srv",
        vec![
            (event(1, comment("a"), 0), 0),
            (event(2, comment("b"), 1), 0),
        ],
        &screen,
        time(12, 0, 0, 0),
        started(),
    );
    assert_eq!(p.notifications.len(), 1);
    assert_eq!(p.notifications[0].title, "New comment from Alice");
    assert_eq!(p.notifications[0].change, Some(ChangeNumber(2)));
    let shown: Vec<_> = p.delivered.iter().map(|d| d.shown_on_os).collect();
    assert_eq!(shown, [false, true]);
}

#[test]
fn many_old_events_collapse_into_a_catch_up_summary() {
    let events: Vec<Event> = (0..6)
        .map(|i| event(10 + i, EventKind::Merged, i as i64 * 100))
        .collect();
    // The app started after all of them.
    let p = plan(
        &settings(),
        "srv",
        events.iter().cloned().map(|e| (e, 0)).collect(),
        &AWAY,
        time(12, 0, 0, 0),
        at(1_000),
    );
    assert_eq!(p.notifications.len(), 1);
    assert_eq!(p.notifications[0].title, "6 updates while you were away");
    assert_eq!(
        p.notifications[0].body,
        "Merged #10\nMerged #11\nMerged #12\nand 3 more"
    );
    assert!(p.delivered.iter().all(|d| d.shown_on_os));

    // Five are not enough to collapse; they are grouped by the window.
    let p = plan(
        &settings(),
        "srv",
        events.iter().take(5).cloned().map(|e| (e, 0)).collect(),
        &AWAY,
        time(12, 0, 0, 0),
        at(1_000),
    );
    assert_eq!(p.notifications.len(), 5);

    // Events from after the start are not catch-up.
    let p = plan(
        &settings(),
        "srv",
        events.iter().cloned().map(|e| (e, 0)).collect(),
        &AWAY,
        time(12, 0, 0, 0),
        at(-1),
    );
    assert_eq!(p.notifications.len(), 6);
}
