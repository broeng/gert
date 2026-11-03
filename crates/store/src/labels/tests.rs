use std::collections::BTreeMap;

use model::{
    Account, AccountId, ChangeDetail, ChangeNumber, ChangeStatus, ChangeSummary, LabelSummary,
    LabelValue, LabelVote, Timestamp,
};

use super::*;

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

fn store() -> (Store, ServerId) {
    let mut s = Store::open_in_memory().unwrap();
    let id = s.server_id("https://g.example/").unwrap();
    (s, id)
}

const T1: &str = "2024-02-01T00:00:00Z";

fn owner() -> Account {
    Account {
        id: AccountId(1),
        name: Some("Ann".into()),
        email: None,
        username: None,
        display_name: None,
    }
}

fn change(n: u64) -> ChangeSummary {
    ChangeSummary {
        number: ChangeNumber(n),
        change_id: None,
        project: "p".into(),
        branch: "main".into(),
        subject: format!("change {n}"),
        status: ChangeStatus::New,
        owner: owner(),
        created: ts(T1),
        updated: ts(T1),
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
    }
}

fn detail(n: u64) -> ChangeDetail {
    ChangeDetail {
        summary: change(n),
        current_revision: None,
        current_patch_set: None,
        commit_message: None,
        reviewers: vec![],
        cc: vec![],
        revisions: vec![],
        messages: vec![],
        submit_requirements: vec![],
        votes: Default::default(),
        attention: vec![],
        permitted_labels: Default::default(),
        removable_reviewers: Default::default(),
        label_values: Default::default(),
        change_actions: Default::default(),
        revision_actions: Default::default(),
    }
}

fn value(v: i32) -> LabelValue {
    LabelValue {
        value: v,
        description: format!("meaning of {v}"),
    }
}

#[test]
fn nothing_cached_is_nothing_seen() {
    let (s, id) = store();
    assert!(s.label_values(&[id]).unwrap().is_empty());
    assert!(s.label_values(&[]).unwrap().is_empty());
}

#[test]
fn values_come_from_permitted_labels_label_values_and_votes() {
    let (mut s, id) = store();
    let mut a = detail(1);
    a.permitted_labels = [("Code-Review".to_owned(), vec![-1, 0, 1])].into();
    let mut b = detail(2);
    b.label_values = [
        ("Code-Review".to_owned(), vec![value(-2), value(2)]),
        ("Verified".to_owned(), vec![value(-1), value(0), value(1)]),
    ]
    .into();
    let mut c = detail(3);
    c.votes = [(
        "Custom-Gate".to_owned(),
        vec![LabelVote {
            account: owner(),
            value: -3,
            date: None,
        }],
    )]
    .into();
    for d in [&a, &b, &c] {
        s.save_change_detail(id, d, ts(T1)).unwrap();
    }
    let seen = s.label_values(&[id]).unwrap();
    assert_eq!(seen["Code-Review"], BTreeSet::from([-2, -1, 0, 1, 2]));
    assert_eq!(seen["Verified"], BTreeSet::from([-1, 0, 1]));
    assert_eq!(seen["Custom-Gate"], BTreeSet::from([-3]));
    assert_eq!(seen.len(), 3);
}

#[test]
fn a_label_of_a_list_row_is_a_name_without_values() {
    let (mut s, id) = store();
    let mut row = change(7);
    row.labels = [("Presubmit".to_owned(), LabelSummary::default())].into();
    s.save_query_page(id, "q", &[row], false, ts(T1)).unwrap();
    let seen = s.label_values(&[id]).unwrap();
    assert_eq!(seen["Presubmit"], BTreeSet::new());
}

#[test]
fn only_the_asked_servers_are_read() {
    let (mut s, a) = store();
    let b = s.server_id("https://other.example/").unwrap();
    let mut d = detail(1);
    d.permitted_labels = [("Code-Review".to_owned(), vec![-1, 1])].into();
    s.save_change_detail(a, &d, ts(T1)).unwrap();
    let mut d = detail(1);
    d.permitted_labels = [("Verified".to_owned(), vec![0])].into();
    s.save_change_detail(b, &d, ts(T1)).unwrap();
    let only_b = s.label_values(&[b]).unwrap();
    assert_eq!(only_b.keys().collect::<Vec<_>>(), ["Verified"]);
    let both = s.label_values(&[a, b]).unwrap();
    assert_eq!(both.len(), 2);
}

/// Change `n` of project `project`, branch `branch`, updated at `updated`,
/// with Code-Review values up to `max`.
fn labelled(n: u64, project: &str, branch: &str, updated: &str, max: i32) -> ChangeDetail {
    let mut d = detail(n);
    d.summary.project = project.into();
    d.summary.branch = branch.into();
    d.summary.updated = ts(updated);
    d.permitted_labels = [("Code-Review".to_owned(), (-max..=max).collect())].into();
    d.label_values = [("Code-Review".to_owned(), (-max..=max).map(value).collect())].into();
    d
}

#[test]
fn labels_are_borrowed_from_the_newest_change_of_the_branch_else_the_project() {
    let (mut s, id) = store();
    assert_eq!(s.labels_to_borrow(id, "p", Some("main")).unwrap(), None);
    let old = labelled(1, "p", "main", "2024-01-01T00:00:00Z", 1);
    let new = labelled(2, "p", "main", "2024-03-01T00:00:00Z", 2);
    let other_branch = labelled(3, "p", "stable", "2024-05-01T00:00:00Z", 3);
    let other_project = labelled(4, "q", "main", "2024-06-01T00:00:00Z", 4);
    // Defines no labels at all (an anonymous fetch): never lends.
    let mut bare = detail(5);
    bare.summary.project = "p".into();
    bare.summary.updated = ts("2024-07-01T00:00:00Z");
    for d in [&old, &new, &other_branch, &other_project, &bare] {
        s.save_change_detail(id, d, ts(T1)).unwrap();
    }
    let max = |b: Option<BorrowedLabels>| {
        let b = b.expect("borrowed");
        assert_eq!(
            b.values["Code-Review"].len(),
            b.permitted["Code-Review"].len()
        );
        *b.permitted["Code-Review"].last().unwrap()
    };
    assert_eq!(max(s.labels_to_borrow(id, "p", Some("main")).unwrap()), 2);
    assert_eq!(max(s.labels_to_borrow(id, "p", Some("stable")).unwrap()), 3);
    // An unknown or uncached branch: the newest of the project.
    assert_eq!(max(s.labels_to_borrow(id, "p", None).unwrap()), 3);
    assert_eq!(s.labels_to_borrow(id, "p", Some("dev")).unwrap(), None);
    assert_eq!(max(s.labels_to_borrow(id, "q", None).unwrap()), 4);
    assert_eq!(s.labels_to_borrow(id, "r", None).unwrap(), None);
}

#[test]
fn an_open_change_lends_before_a_newer_closed_one() {
    let (mut s, id) = store();
    let open = labelled(1, "p", "main", "2024-01-01T00:00:00Z", 2);
    let mut merged = labelled(2, "p", "main", "2024-03-01T00:00:00Z", 1);
    merged.summary.status = ChangeStatus::Merged;
    for d in [&open, &merged] {
        s.save_change_detail(id, d, ts(T1)).unwrap();
    }
    let b = s.labels_to_borrow(id, "p", Some("main")).unwrap().unwrap();
    assert_eq!(b.permitted["Code-Review"], [-2, -1, 0, 1, 2]);
    assert!(b.open);
    // Only closed ones: the newest of them still lends, and says so.
    let (mut s, id) = store();
    s.save_change_detail(id, &merged, ts(T1)).unwrap();
    let b = s.labels_to_borrow(id, "p", Some("main")).unwrap().unwrap();
    assert_eq!(b.permitted["Code-Review"], [-1, 0, 1]);
    assert!(!b.open);
}
