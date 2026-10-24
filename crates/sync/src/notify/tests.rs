use std::collections::BTreeMap;

use jiff::civil::time;
use model::{
    Account, AccountId, ChangeDetail, ChangeNumber, ChangeStatus, ChangeSummary, Event, EventKind,
    PatchSetNumber, Timestamp,
};

use super::*;

const ME: AccountId = AccountId(1);

fn account(id: u64) -> Account {
    Account {
        id: AccountId(id),
        name: Some(format!("User {id}")),
        email: None,
        username: Some(format!("user{id}")),
        display_name: None,
    }
}

/// A change owned by account 2, and mine to nothing else.
fn detail() -> ChangeDetail {
    ChangeDetail {
        summary: ChangeSummary {
            number: ChangeNumber(7),
            change_id: None,
            project: "proj".into(),
            branch: "main".into(),
            subject: "Fix it".into(),
            status: ChangeStatus::New,
            owner: account(2),
            created: ts(),
            updated: ts(),
            insertions: 0,
            deletions: 0,
            labels: BTreeMap::new(),
            work_in_progress: false,
            is_private: false,
            unresolved_comment_count: 0,
            total_comment_count: 0,
            topic: None,
            hashtags: vec![],
            attention_set: vec![],
            submittable: None,
            mergeable: None,
        },
        current_revision: None,
        current_patch_set: None,
        commit_message: None,
        reviewers: vec![],
        cc: vec![],
        revisions: vec![],
        messages: vec![],
        submit_requirements: vec![],
        votes: BTreeMap::new(),
        attention: vec![],
        permitted_labels: BTreeMap::new(),
        removable_reviewers: Default::default(),
        label_values: BTreeMap::new(),
        change_actions: BTreeMap::new(),
        revision_actions: BTreeMap::new(),
    }
}

fn ts() -> Timestamp {
    "2024-03-01T10:00:00Z".parse().unwrap()
}

fn mine() -> ChangeDetail {
    let mut d = detail();
    d.summary.owner = account(1);
    d
}

fn event(kind: EventKind) -> Event {
    Event {
        server: model::ServerId(1),
        change: ChangeNumber(7),
        project: "proj".into(),
        subject: "Fix it".into(),
        owner: None,
        actor: Some(account(5)),
        at: ts(),
        kind,
        subject_account: None,
    }
}

fn vote(label: &str, value: i32) -> Event {
    event(EventKind::Vote {
        label: label.into(),
        value,
        previous: None,
    })
}

fn comment(text: &str) -> Event {
    event(EventKind::Comment {
        message_id: "m".into(),
        snippet: text.into(),
    })
}

struct Fixed {
    starred: bool,
    saved: &'static str,
}

impl QueryMembership for Fixed {
    fn in_saved_query(&self, name: &str, _: ChangeNumber) -> bool {
        name == self.saved
    }
    fn starred(&self, _: ChangeNumber) -> bool {
        self.starred
    }
}

fn ctx<'a>(detail: &'a ChangeDetail, queries: &'a dyn QueryMembership) -> MatchContext<'a> {
    MatchContext {
        me: ME,
        handles: &[],
        server: "main",
        detail,
        queries,
    }
}

fn only(rule: Rule) -> NotificationSettings {
    NotificationSettings {
        rules: vec![rule],
        ..NotificationSettings::default()
    }
}

fn fires(rule: Rule, event: &Event, detail: &ChangeDetail) -> bool {
    !matches(&only(rule), event, &ctx(detail, &NoQueries)).is_empty()
}

#[test]
fn defaults_notify_on_my_changes_and_my_turn() {
    let s = NotificationSettings::default();
    let m = |e: &Event, d: &ChangeDetail| matches(&s, e, &ctx(d, &NoQueries));
    let mine = mine();
    let theirs = detail();
    // Negative votes on my changes, per label.
    assert_eq!(m(&vote("Code-Review", -2), &mine), [1]);
    assert_eq!(m(&vote("Code-Review", -1), &mine), [1]);
    assert_eq!(m(&vote("Verified", -1), &mine), [2]);
    assert!(m(&vote("Code-Review", 1), &mine).is_empty());
    assert!(m(&vote("Verified", 1), &mine).is_empty());
    assert!(m(&vote("Code-Review", -2), &theirs).is_empty());
    // Comments on my changes.
    assert_eq!(m(&comment("hm"), &mine), [3]);
    assert!(m(&comment("hm"), &theirs).is_empty());
    // My attention set.
    let mut turn = theirs.clone();
    turn.summary.attention_set = vec![ME];
    let mut added = event(EventKind::AttentionAdded);
    added.subject_account = Some(ME);
    assert_eq!(m(&added, &turn), [0]);
    added.subject_account = Some(AccountId(9));
    assert!(m(&added, &turn).is_empty());
}

#[test]
fn every_event_kind_matches_only_its_rule_event() {
    let mut added = event(EventKind::AttentionAdded);
    added.subject_account = Some(ME);
    let table: Vec<(RuleEvent, Event)> = vec![
        (RuleEvent::Vote, vote("Code-Review", 1)),
        (RuleEvent::Comment, comment("x")),
        (RuleEvent::AttentionAdded, added),
        (
            RuleEvent::ReviewerAdded,
            event(EventKind::ReviewerAdded { cc: false }),
        ),
        (
            RuleEvent::PatchSet,
            event(EventKind::NewPatchSet {
                number: PatchSetNumber(2),
            }),
        ),
        (RuleEvent::Merged, event(EventKind::Merged)),
        (RuleEvent::Abandoned, event(EventKind::Abandoned)),
        (RuleEvent::ReadyToSubmit, event(EventKind::ReadyToSubmit)),
        (RuleEvent::MergeConflict, event(EventKind::MergeConflict)),
    ];
    let d = mine();
    for (rule_event, _) in &table {
        for (event_of, e) in &table {
            let rule = Rule::new(*rule_event, Scope::Owner);
            assert_eq!(
                fires(rule, e, &d),
                rule_event == event_of,
                "rule {rule_event:?} on event {event_of:?}"
            );
        }
    }
}

#[test]
fn scopes_look_at_the_cached_change() {
    let e = event(EventKind::Merged);
    let rule = |scope| Rule::new(RuleEvent::Merged, scope);

    let m = mine();
    assert!(fires(rule(Scope::Owner), &e, &m));
    assert!(!fires(rule(Scope::Reviewer), &e, &m));

    let mut reviewing = detail();
    reviewing.reviewers = vec![account(1)];
    assert!(fires(rule(Scope::Reviewer), &e, &reviewing));
    assert!(!fires(rule(Scope::Cc), &e, &reviewing));
    assert!(!fires(rule(Scope::Owner), &e, &reviewing));

    let mut cced = detail();
    cced.cc = vec![account(1)];
    assert!(fires(rule(Scope::Cc), &e, &cced));
    assert!(!fires(rule(Scope::Reviewer), &e, &cced));

    let mut turn = detail();
    turn.summary.attention_set = vec![ME];
    assert!(fires(rule(Scope::Attention), &e, &turn));
    assert!(!fires(rule(Scope::Attention), &e, &detail()));

    // Any is the union, not everything.
    for d in [&m, &reviewing, &cced, &turn] {
        assert!(fires(rule(Scope::Any), &e, d));
    }
    assert!(!fires(rule(Scope::Any), &e, &detail()));
    let starred = Fixed {
        starred: true,
        saved: "",
    };
    let d = detail();
    assert!(matches(&only(rule(Scope::Starred)), &e, &ctx(&d, &NoQueries)).is_empty());
    assert_eq!(
        matches(&only(rule(Scope::Starred)), &e, &ctx(&d, &starred)),
        [0]
    );
    assert_eq!(
        matches(&only(rule(Scope::Any)), &e, &ctx(&d, &starred)),
        [0]
    );
}

#[test]
fn a_query_scope_asks_for_membership_by_name() {
    let e = event(EventKind::Merged);
    let d = detail();
    let q = Fixed {
        starred: false,
        saved: "Team",
    };
    let rule = |name: &str| only(Rule::new(RuleEvent::Merged, Scope::Query(name.into())));
    assert_eq!(matches(&rule("Team"), &e, &ctx(&d, &q)), [0]);
    assert!(matches(&rule("Other"), &e, &ctx(&d, &q)).is_empty());
}

#[test]
fn own_actions_are_dropped_unless_allowed() {
    let mut e = vote("Code-Review", -1);
    e.actor = Some(account(1));
    let d = mine();
    let rule = Rule::new(RuleEvent::Vote, Scope::Owner);
    assert!(!fires(rule.clone(), &e, &d));
    let mut s = only(rule);
    s.ignore_own_actions = false;
    assert_eq!(matches(&s, &e, &ctx(&d, &NoQueries)), [0]);
    // No known actor is not mine.
    e.actor = None;
    s.ignore_own_actions = true;
    assert_eq!(matches(&s, &e, &ctx(&d, &NoQueries)), [0]);
}

#[test]
fn vote_rules_match_label_and_exact_values() {
    let d = mine();
    let rule = |labels: &[&str], values: &[i32]| {
        let mut r = Rule::new(RuleEvent::Vote, Scope::Owner);
        r.labels = labels.iter().map(|s| (*s).to_owned()).collect();
        r.values = values.to_vec();
        r
    };
    assert!(fires(
        rule(&["Code-Review"], &[-2, -1]),
        &vote("Code-Review", -2),
        &d
    ));
    assert!(!fires(
        rule(&["Code-Review"], &[-2, -1]),
        &vote("Code-Review", 1),
        &d
    ));
    // Exact values: "-2" does not mean "everything below".
    assert!(!fires(
        rule(&["Code-Review"], &[-2]),
        &vote("Code-Review", -1),
        &d
    ));
    // Labels are case-sensitive; `*` and several labels work.
    assert!(!fires(
        rule(&["code-review"], &[]),
        &vote("Code-Review", 1),
        &d
    ));
    assert!(fires(rule(&["*"], &[1]), &vote("Anything", 1), &d));
    assert!(fires(
        rule(&["Verified", "Code-Review"], &[]),
        &vote("Code-Review", 1),
        &d
    ));
    assert!(!fires(
        rule(&["Verified"], &[]),
        &vote("Code-Review", 1),
        &d
    ));
    // No label list: any label; no values: any value.
    assert!(fires(rule(&[], &[]), &vote("Whatever", 2), &d));
    // A retraction is a 0.
    assert!(!fires(
        rule(&["Code-Review"], &[-1]),
        &vote("Code-Review", 0),
        &d
    ));
    assert!(fires(
        rule(&["Code-Review"], &[-1, 0]),
        &vote("Code-Review", 0),
        &d
    ));
}

#[test]
fn servers_restrict_a_rule() {
    let d = mine();
    let e = event(EventKind::Merged);
    let mut rule = Rule::new(RuleEvent::Merged, Scope::Owner);
    rule.servers = vec!["other".into()];
    assert!(!fires(rule.clone(), &e, &d));
    rule.servers.push("main".into());
    assert!(fires(rule, &e, &d));
}

#[test]
fn disabled_rules_and_the_master_switch_match_nothing() {
    let d = mine();
    let e = event(EventKind::Merged);
    let mut rule = Rule::new(RuleEvent::Merged, Scope::Owner);
    let mut s = only(rule.clone());
    assert_eq!(matches(&s, &e, &ctx(&d, &NoQueries)), [0]);
    s.enabled = false;
    assert!(matches(&s, &e, &ctx(&d, &NoQueries)).is_empty());
    rule.enabled = false;
    assert!(!fires(rule, &e, &d));
}

#[test]
fn from_filters_actors_by_any_name_case_insensitively() {
    let d = mine();
    let mut e = vote("Verified", -1);
    e.actor = Some(Account {
        id: AccountId(8),
        name: Some("Jenkins CI".into()),
        email: Some("ci@example.com".into()),
        username: Some("jenkins".into()),
        display_name: None,
    });
    let rule = |from: &[&str]| {
        let mut r = Rule::new(RuleEvent::Vote, Scope::Owner);
        r.from = from.iter().map(|s| (*s).to_owned()).collect();
        r
    };
    assert!(fires(rule(&["Jenkins"]), &e, &d));
    assert!(fires(rule(&["jenkins ci"]), &e, &d));
    assert!(fires(rule(&["CI@example.com"]), &e, &d));
    assert!(!fires(rule(&["zuul"]), &e, &d));
    assert!(fires(rule(&[]), &e, &d));
    // With a `from` list an unknown actor never matches.
    e.actor = None;
    assert!(!fires(rule(&["jenkins"]), &e, &d));
}

#[test]
fn nobody_is_a_bot_yet() {
    // The model has no account tags; exclude_bots must not hide people.
    let d = mine();
    let mut e = vote("Verified", -1);
    e.actor = Some(Account {
        username: Some("ci-bot".into()),
        ..account(8)
    });
    let rule = Rule::new(RuleEvent::Vote, Scope::Owner);
    assert!(rule.exclude_bots);
    assert!(fires(rule, &e, &d));
}

#[test]
fn mentions_look_for_the_handle_after_an_at_sign() {
    let d = detail();
    let handles = vec!["jdoe".to_owned(), "jdoe@example.com".to_owned()];
    let c = MatchContext {
        handles: &handles,
        ..ctx(&d, &NoQueries)
    };
    let s = only(Rule::new(RuleEvent::Mention, Scope::Any));
    let mut d2 = d.clone();
    d2.reviewers = vec![account(1)];
    let c = MatchContext { detail: &d2, ..c };
    let hit = |text: &str| !matches(&s, &comment(text), &c).is_empty();
    assert!(hit("please look, @jdoe"));
    assert!(hit("@JDoe: ping"));
    assert!(hit("cc @jdoe@example.com."));
    assert!(!hit("@jdoes is someone else"));
    assert!(!hit("jdoe without at"));
    // A mention is not a plain comment event for a comment rule.
    let s = only(Rule::new(RuleEvent::Comment, Scope::Any));
    assert_eq!(matches(&s, &comment("@jdoe"), &c), [0]);
    // No handles, no mentions.
    assert!(
        matches(
            &only(Rule::new(RuleEvent::Mention, Scope::Any)),
            &comment("@jdoe"),
            &ctx(&d2, &NoQueries)
        )
        .is_empty()
    );
}

#[test]
fn quiet_hours_parse_and_wrap_midnight() {
    let q = QuietHours::parse("22:00-07:00").unwrap();
    assert_eq!(q.format(), "22:00-07:00");
    assert!(q.contains(time(23, 0, 0, 0)));
    assert!(q.contains(time(22, 0, 0, 0)));
    assert!(q.contains(time(0, 0, 0, 0)));
    assert!(q.contains(time(6, 59, 0, 0)));
    assert!(!q.contains(time(7, 0, 0, 0)));
    assert!(!q.contains(time(12, 0, 0, 0)));

    let day = QuietHours::parse(" 12:00 - 13:30 ").unwrap();
    assert!(day.contains(time(12, 0, 0, 0)));
    assert!(!day.contains(time(13, 30, 0, 0)));
    assert!(!day.contains(time(23, 0, 0, 0)));

    let empty = QuietHours::parse("08:00-08:00").unwrap();
    assert!(!empty.contains(time(8, 0, 0, 0)));

    for bad in [
        "",
        "22:00",
        "22-07",
        "25:00-07:00",
        "22:00-07:61",
        "a-b",
        "22:00-",
    ] {
        assert!(QuietHours::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn settings_know_whether_it_is_quiet() {
    let mut s = NotificationSettings::default();
    assert!(!s.in_quiet_hours(time(3, 0, 0, 0)));
    s.quiet_hours = Some(QuietHours::parse("22:00-07:00").unwrap());
    assert!(s.in_quiet_hours(time(3, 0, 0, 0)));
    assert!(!s.in_quiet_hours(time(9, 0, 0, 0)));
}

fn saved() -> Vec<(String, String)> {
    vec![("Team".into(), "project:foo status:open".into())]
}

#[test]
fn the_default_rules_watch_my_changes_and_my_turn() {
    let s = NotificationSettings::default();
    assert_eq!(
        watch_query(&s, "main", &[]),
        "is:open (attention:self OR owner:self)"
    );
}

#[test]
fn the_watch_query_unions_scopes_without_repeats() {
    let mut s = only(Rule::new(RuleEvent::Comment, Scope::Owner));
    s.rules = vec![
        Rule::new(RuleEvent::Comment, Scope::Owner),
        Rule::new(RuleEvent::Vote, Scope::Owner),
        Rule::new(RuleEvent::PatchSet, Scope::Reviewer),
        Rule::new(RuleEvent::ReviewerAdded, Scope::Cc),
        Rule::new(RuleEvent::Comment, Scope::Starred),
        Rule::new(RuleEvent::Comment, Scope::Query("Team".into())),
    ];
    assert_eq!(
        watch_query(&s, "main", &saved()),
        "is:open (owner:self OR reviewer:self OR cc:self OR is:starred OR (project:foo status:open))"
    );
    s.rules = vec![Rule::new(RuleEvent::Comment, Scope::Any)];
    assert_eq!(
        watch_query(&s, "main", &[]),
        "is:open (owner:self OR reviewer:self OR cc:self OR attention:self OR is:starred)"
    );
    s.rules = vec![Rule::new(RuleEvent::Comment, Scope::Owner)];
    assert_eq!(watch_query(&s, "main", &[]), "is:open owner:self");
}

#[test]
fn closing_rules_drop_the_open_restriction() {
    let mut s = only(Rule::new(RuleEvent::Comment, Scope::Owner));
    s.rules = vec![
        Rule::new(RuleEvent::Comment, Scope::Owner),
        Rule::new(RuleEvent::Merged, Scope::Reviewer),
    ];
    assert_eq!(
        watch_query(&s, "main", &[]),
        "-age:7d (owner:self OR reviewer:self)"
    );
    // A disabled closing rule does not count.
    s.rules[1].enabled = false;
    assert_eq!(watch_query(&s, "main", &[]), "is:open owner:self");
}

#[test]
fn the_watch_query_is_per_server_and_empty_without_rules() {
    let mut s = only(Rule::new(RuleEvent::Comment, Scope::Owner));
    let mut other = Rule::new(RuleEvent::Comment, Scope::Reviewer);
    other.servers = vec!["other".into()];
    s.rules = vec![Rule::new(RuleEvent::Comment, Scope::Owner), other];
    assert_eq!(watch_query(&s, "main", &[]), "is:open owner:self");
    assert_eq!(
        watch_query(&s, "other", &[]),
        "is:open (owner:self OR reviewer:self)"
    );
    // An unknown saved query watches nothing.
    s.rules = vec![Rule::new(RuleEvent::Comment, Scope::Query("Gone".into()))];
    assert_eq!(watch_query(&s, "main", &[]), "");
    s.rules.clear();
    assert_eq!(watch_query(&s, "main", &[]), "");
    s = NotificationSettings {
        enabled: false,
        ..NotificationSettings::default()
    };
    assert_eq!(watch_query(&s, "main", &[]), "");
}
