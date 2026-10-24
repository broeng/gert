use std::collections::BTreeMap;

use model::{
    AttentionEntry, ChangeNumber, ChangeSummary, LabelVote, PatchSetNumber, Revision, RevisionId,
};

use super::*;

const SERVER: ServerId = ServerId(1);

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

fn account(id: u64) -> Account {
    Account {
        id: AccountId(id),
        name: Some(format!("User {id}")),
        email: None,
        username: None,
        display_name: None,
    }
}

/// An open change by account 1, with one patch set and its upload message.
fn detail() -> ChangeDetail {
    ChangeDetail {
        summary: ChangeSummary {
            number: ChangeNumber(7),
            change_id: None,
            project: "proj".into(),
            branch: "main".into(),
            subject: "Fix it".into(),
            status: ChangeStatus::New,
            owner: account(1),
            created: ts("2024-03-01T00:00:00Z"),
            updated: ts("2024-03-01T00:00:00Z"),
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
            submittable: Some(false),
            mergeable: Some(true),
        },
        current_revision: Some("aaa".into()),
        current_patch_set: Some(PatchSetNumber(1)),
        commit_message: None,
        reviewers: vec![],
        cc: vec![],
        revisions: vec![revision(1)],
        messages: vec![message(
            "m1",
            1,
            "Uploaded patch set 1.",
            "2024-03-01T00:00:00Z",
        )],
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

fn revision(n: u32) -> Revision {
    Revision {
        number: PatchSetNumber(n),
        sha: RevisionId(format!("sha{n}")),
        kind: None,
        created: Some(ts("2024-03-01T00:00:00Z")),
        uploader: Some(account(1)),
        ref_name: None,
        description: None,
        commit: None,
    }
}

fn message(id: &str, author: u64, text: &str, at: &str) -> ChangeMessage {
    ChangeMessage {
        id: id.into(),
        author: Some(account(author)),
        real_author: None,
        date: ts(at),
        message: text.into(),
        tag: None,
        revision: Some(PatchSetNumber(1)),
    }
}

fn vote(account_id: u64, value: i32, at: Option<&str>) -> LabelVote {
    LabelVote {
        account: account(account_id),
        value,
        date: at.map(ts),
    }
}

/// `new` with a later `updated`, as the server would report it.
fn later(mut new: ChangeDetail) -> ChangeDetail {
    new.summary.updated = ts("2024-03-02T00:00:00Z");
    new
}

fn diff(old: &ChangeDetail, new: &ChangeDetail) -> Vec<Event> {
    diff_change(SERVER, old, new)
}

fn kinds(events: &[Event]) -> Vec<EventKind> {
    events.iter().map(|e| e.kind.clone()).collect()
}

#[test]
fn identical_states_have_no_events() {
    let d = detail();
    assert!(diff(&d, &d).is_empty());
}

#[test]
fn event_carries_the_change_facts() {
    let old = detail();
    let mut new = later(detail());
    new.summary.status = ChangeStatus::Merged;
    let e = &diff(&old, &new)[0];
    assert_eq!(e.server, SERVER);
    assert_eq!(e.change, ChangeNumber(7));
    assert_eq!((e.project.as_str(), e.subject.as_str()), ("proj", "Fix it"));
    assert_eq!(e.owner.as_ref().map(|a| a.id), Some(AccountId(1)));
}

#[test]
fn a_new_vote_names_the_voter_and_uses_the_vote_time() {
    let old = detail();
    let mut new = later(detail());
    new.votes.insert(
        "Code-Review".into(),
        vec![vote(2, -1, Some("2024-03-01T12:00:00Z"))],
    );
    let events = diff(&old, &new);
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].kind,
        EventKind::Vote {
            label: "Code-Review".into(),
            value: -1,
            previous: None
        }
    );
    assert_eq!(events[0].actor.as_ref().map(|a| a.id), Some(AccountId(2)));
    assert_eq!(events[0].at, ts("2024-03-01T12:00:00Z"));
    assert_eq!(events[0].subject_account, None);
}

#[test]
fn a_vote_without_a_date_falls_back_to_updated() {
    let mut new = later(detail());
    new.votes.insert("Verified".into(), vec![vote(2, 1, None)]);
    assert_eq!(diff(&detail(), &new)[0].at, ts("2024-03-02T00:00:00Z"));
}

#[test]
fn changed_and_removed_votes() {
    let mut old = detail();
    old.votes.insert(
        "Code-Review".into(),
        vec![vote(2, 1, None), vote(3, 2, None), vote(4, 1, None)],
    );
    old.votes.insert("Verified".into(), vec![vote(2, -1, None)]);
    let mut new = later(detail());
    new.votes.insert(
        "Code-Review".into(),
        vec![
            vote(2, -1, Some("2024-03-01T13:00:00Z")), // changed
            vote(3, 2, None),                          // same
            vote(4, 0, None),                          // reset to 0
        ],
    );
    // Verified has no entry for account 2 any more.
    new.votes.insert("Verified".into(), vec![]);
    let events = diff(&old, &new);
    let got: Vec<_> = events
        .iter()
        .map(|e| (e.actor.as_ref().unwrap().id.0, e.kind.clone()))
        .collect();
    let v = |label: &str, value, previous| EventKind::Vote {
        label: label.into(),
        value,
        previous: Some(previous),
    };
    assert_eq!(got.len(), 3, "{got:?}");
    assert!(got.contains(&(2, v("Code-Review", -1, 1))));
    assert!(got.contains(&(4, v("Code-Review", 0, 1))));
    assert!(got.contains(&(2, v("Verified", 0, -1))));
}

#[test]
fn a_zero_vote_that_was_never_there_is_nothing() {
    let mut new = detail();
    new.votes
        .insert("Code-Review".into(), vec![vote(2, 0, None)]);
    assert!(diff(&detail(), &new).is_empty());
}

#[test]
fn a_label_that_disappeared_removes_its_votes() {
    let mut old = detail();
    old.votes.insert("Old-Label".into(), vec![vote(2, 1, None)]);
    let events = diff(&old, &later(detail()));
    assert_eq!(
        kinds(&events),
        [EventKind::Vote {
            label: "Old-Label".into(),
            value: 0,
            previous: Some(1)
        }]
    );
}

#[test]
fn new_comments_are_events_and_old_ones_are_not() {
    let old = detail();
    let mut new = later(detail());
    new.messages.push(message(
        "m2",
        2,
        "Patch Set 1:\n\n(2 comments)\n\nLooks odd to me.",
        "2024-03-01T10:00:00Z",
    ));
    let events = diff(&old, &new);
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].kind,
        EventKind::Comment {
            message_id: "m2".into(),
            snippet: "(2 comments)\n\nLooks odd to me.".into()
        }
    );
    assert_eq!(events[0].actor.as_ref().map(|a| a.id), Some(AccountId(2)));
    assert_eq!(events[0].at, ts("2024-03-01T10:00:00Z"));
}

#[test]
fn a_message_without_a_header_is_kept_whole() {
    let mut new = later(detail());
    new.messages
        .push(message("m2", 2, "  plain words\n", "2024-03-01T10:00:00Z"));
    assert_eq!(
        kinds(&diff(&detail(), &new)),
        [EventKind::Comment {
            message_id: "m2".into(),
            snippet: "plain words".into()
        }]
    );
}

#[test]
fn a_header_only_message_is_just_the_vote() {
    let mut new = later(detail());
    for (i, text) in [
        "Patch Set 1: Code-Review+1",
        "Patch Set 12: Code-Review-1 Verified+1\n",
        "Patch Set 1:",
    ]
    .iter()
    .enumerate()
    {
        new.messages
            .push(message(&format!("v{i}"), 2, text, "2024-03-01T10:00:00Z"));
    }
    assert!(diff(&detail(), &new).is_empty());
}

#[test]
fn a_text_starting_like_a_header_but_is_not_one_is_kept() {
    let mut new = later(detail());
    new.messages.push(message(
        "m2",
        2,
        "Patch Sets are fine here",
        "2024-03-01T10:00:00Z",
    ));
    assert_eq!(diff(&detail(), &new).len(), 1);
}

#[test]
fn gerrit_log_entries_are_skipped_but_bots_are_kept() {
    let mut new = later(detail());
    let mut gerrit = message("m2", 2, "Uploaded patch set 2.", "2024-03-01T10:00:00Z");
    gerrit.tag = Some("autogenerated:gerrit:newPatchSet".into());
    let mut bot = message("m3", 3, "Build failed", "2024-03-01T10:01:00Z");
    bot.tag = Some("autogenerated:jenkins".into());
    new.messages.extend([gerrit, bot]);
    let events = diff(&detail(), &new);
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0].kind, EventKind::Comment { message_id, .. } if message_id == "m3"));
}

#[test]
fn long_comments_are_cut() {
    let mut new = later(detail());
    let text = "ä".repeat(COMMENT_SNIPPET_CHARS + 50);
    new.messages
        .push(message("m2", 2, &text, "2024-03-01T10:00:00Z"));
    let EventKind::Comment { snippet, .. } = &diff(&detail(), &new)[0].kind else {
        panic!("not a comment");
    };
    assert_eq!(snippet.chars().count(), COMMENT_SNIPPET_CHARS);
}

#[test]
fn an_old_state_without_messages_does_not_replay_the_log() {
    let mut old = detail();
    old.messages.clear();
    let mut new = later(detail());
    new.messages
        .push(message("m2", 2, "hello", "2024-03-01T10:00:00Z"));
    assert!(diff(&old, &new).is_empty());
}

fn attention(id: u64, at: Option<&str>) -> AttentionEntry {
    AttentionEntry {
        account: account(id),
        reason: None,
        last_update: at.map(ts),
    }
}

#[test]
fn only_accounts_new_to_the_attention_set_count() {
    let mut old = detail();
    old.attention = vec![attention(1, None)];
    let mut new = later(detail());
    new.attention = vec![
        attention(1, None),
        attention(2, Some("2024-03-01T09:00:00Z")),
    ];
    let events = diff(&old, &new);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, EventKind::AttentionAdded);
    assert_eq!(events[0].subject_account, Some(AccountId(2)));
    assert_eq!(events[0].actor, None);
    assert_eq!(events[0].at, ts("2024-03-01T09:00:00Z"));
    // Leaving the set is not an event.
    assert!(diff(&new, &old).is_empty());
}

#[test]
fn added_reviewers_and_cc() {
    let mut old = detail();
    old.reviewers = vec![account(2)];
    old.cc = vec![account(3)];
    let mut new = later(detail());
    new.reviewers = vec![account(2), account(4), account(3)]; // 3: CC promoted
    new.cc = vec![account(5), account(2)]; // 2: demoted, not new
    let got: Vec<_> = diff(&old, &new)
        .into_iter()
        .map(|e| (e.subject_account.unwrap().0, e.kind))
        .collect();
    assert_eq!(got.len(), 3, "{got:?}");
    assert!(got.contains(&(4, EventKind::ReviewerAdded { cc: false })));
    assert!(got.contains(&(3, EventKind::ReviewerAdded { cc: false })));
    assert!(got.contains(&(5, EventKind::ReviewerAdded { cc: true })));
}

#[test]
fn new_patch_sets_are_those_above_the_old_maximum() {
    let old = detail();
    let mut new = later(detail());
    let mut r2 = revision(2);
    r2.created = Some(ts("2024-03-01T20:00:00Z"));
    r2.uploader = Some(account(1));
    new.revisions.push(r2);
    new.revisions.push(revision(3));
    new.current_patch_set = Some(PatchSetNumber(3));
    let events = diff(&old, &new);
    let ps = |n| EventKind::NewPatchSet {
        number: PatchSetNumber(n),
    };
    // Oldest first: patch set 3 kept the default creation time.
    assert_eq!(kinds(&events), [ps(3), ps(2)]);
    assert_eq!(events[1].actor.as_ref().map(|a| a.id), Some(AccountId(1)));
    assert_eq!(events[1].at, ts("2024-03-01T20:00:00Z"));
}

#[test]
fn patch_sets_are_compared_by_the_number_when_the_old_state_had_no_revisions() {
    let mut old = detail();
    old.revisions.clear();
    let mut new = later(detail());
    new.revisions = vec![revision(1), revision(2)];
    let events = diff(&old, &new);
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].kind,
        EventKind::NewPatchSet {
            number: PatchSetNumber(2)
        }
    );
    old.current_patch_set = None;
    assert!(diff(&old, &new).is_empty());
}

#[test]
fn merging_and_abandoning_name_the_actor_from_the_log() {
    for (status, tag, kind) in [
        (
            ChangeStatus::Merged,
            "autogenerated:gerrit:merged",
            EventKind::Merged,
        ),
        (
            ChangeStatus::Abandoned,
            "autogenerated:gerrit:abandon",
            EventKind::Abandoned,
        ),
    ] {
        let mut new = later(detail());
        new.summary.status = status;
        new.summary.submittable = Some(false);
        let mut m = message("m2", 3, "closed", "2024-03-01T10:00:00Z");
        m.tag = Some(tag.into());
        new.messages.push(m);
        let events = diff(&detail(), &new);
        // The log entry itself is not a comment.
        assert_eq!(kinds(&events), [kind]);
        assert_eq!(events[0].actor.as_ref().map(|a| a.id), Some(AccountId(3)));
    }
}

#[test]
fn closing_without_a_log_entry_has_no_actor_and_closed_changes_stay_closed() {
    let mut new = later(detail());
    new.summary.status = ChangeStatus::Merged;
    let events = diff(&detail(), &new);
    assert_eq!(kinds(&events), [EventKind::Merged]);
    assert_eq!(events[0].actor, None);
    // Merged to merged, and a reopened change, are no status events.
    assert!(diff(&new, &new).is_empty());
    let mut restored = later(detail());
    restored.summary.status = ChangeStatus::New;
    assert!(diff(&new, &restored).is_empty());
}

#[test]
fn submittable_becoming_true_is_ready_to_submit() {
    let mut new = later(detail());
    new.summary.submittable = Some(true);
    assert_eq!(kinds(&diff(&detail(), &new)), [EventKind::ReadyToSubmit]);
    // Going back, staying, or an unknown old value are not.
    assert!(diff(&new, &detail()).is_empty());
    assert!(diff(&new, &new).is_empty());
    let mut unknown = detail();
    unknown.summary.submittable = None;
    assert!(diff(&unknown, &new).is_empty());
}

#[test]
fn mergeable_becoming_false_is_a_conflict() {
    let mut new = later(detail());
    new.summary.mergeable = Some(false);
    assert_eq!(kinds(&diff(&detail(), &new)), [EventKind::MergeConflict]);
    assert!(diff(&new, &detail()).is_empty());
    assert!(diff(&new, &new).is_empty());
    let mut unknown = detail();
    unknown.summary.mergeable = None;
    assert!(diff(&unknown, &new).is_empty());
}

#[test]
fn merged_changes_report_neither_ready_nor_conflict() {
    let mut new = later(detail());
    new.summary.status = ChangeStatus::Merged;
    new.summary.submittable = Some(true);
    new.summary.mergeable = Some(false);
    assert_eq!(kinds(&diff(&detail(), &new)), [EventKind::Merged]);
}

#[test]
fn several_events_come_out_oldest_first_with_distinct_keys() {
    let mut new = later(detail());
    new.votes.insert(
        "Code-Review".into(),
        vec![vote(2, 2, Some("2024-03-01T15:00:00Z"))],
    );
    new.messages.push(message(
        "m2",
        2,
        "Patch Set 1: Code-Review+2\n\nnice",
        "2024-03-01T15:00:00Z",
    ));
    new.summary.submittable = Some(true);
    new.attention = vec![attention(1, Some("2024-03-01T15:00:01Z"))];
    new.reviewers = vec![account(2)];
    let events = diff(&detail(), &new);
    assert_eq!(events.len(), 5);
    assert!(events.windows(2).all(|w| w[0].at <= w[1].at));
    let keys: HashSet<_> = events.iter().map(Event::key).collect();
    assert_eq!(keys.len(), 5);
    // Detecting the same transition again gives the same keys.
    let again: HashSet<_> = diff(&detail(), &new).iter().map(Event::key).collect();
    assert_eq!(keys, again);
}
