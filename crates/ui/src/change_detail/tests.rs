//! The provider over an in-memory store (and a temporary repository for
//! the commits), and its pure parts.

use gerrit_api::fake::RouteHttpClient;
use local_git::testutil::{TEST_EMAIL, TestRepo};
use model::{
    AccountId, ChangeNumber, LocalChangeState, PatchSetNumber, Revision, RevisionId, Timestamp,
};
use store::{LocalChangeUpsert, LocalOp, NewLocalPatchSet, ServerId, Store};
use sync::Syncer;

use super::*;
use crate::{
    session::Session,
    testing::detail,
    tests::{URL, env_with, setup},
};

const SERVER: ServerId = ServerId(1);

fn at(minute: u32) -> Timestamp {
    format!("2024-06-15T10:{minute:02}:00Z")
        .parse()
        .expect("timestamp")
}

fn local(id: i64, sha: &str, seen: u32) -> Revision {
    Revision {
        number: PatchSetNumber::local(id).expect("local number"),
        sha: sha.into(),
        kind: None,
        created: Some(at(seen)),
        uploader: None,
        ref_name: None,
        description: None,
        commit: None,
    }
}

/// Change 7 with patch sets 1..=3 uploaded at minutes 10, 20 and 30.
fn gerrit() -> ChangeDetail {
    let mut d = detail(7, 3, 3);
    for (r, m) in d.revisions.iter_mut().zip([10, 20, 30]) {
        r.created = Some(at(m));
    }
    d
}

fn labels(d: &ChangeDetail) -> Vec<String> {
    d.revisions
        .iter()
        .map(|r| patch_set_label(&d.revisions, r.number))
        .collect()
}

#[test]
fn local_versions_go_after_the_patch_set_current_when_they_were_seen() {
    let d = merge(gerrit(), vec![local(1, "l1", 35), local(2, "l2", 40)]);
    assert_eq!(labels(&d), ["PS 1", "PS 2", "PS 3", "local 1", "local 2"]);
    // Seen while PS 2 was current: PS 3 came later.
    let d = merge(gerrit(), vec![local(1, "l1", 25), local(2, "l2", 40)]);
    assert_eq!(labels(&d), ["PS 1", "PS 2", "local 1", "PS 3", "local 2"]);
    // The current patch set is still Gerrit's.
    assert_eq!(d.current_patch_set, Some(PatchSetNumber(3)));
}

#[test]
fn merging_again_replaces_and_a_pushed_version_drops_out() {
    let d = merge(gerrit(), vec![local(1, "l1", 35), local(2, "l2", 40)]);
    let d = merge(d, vec![local(2, "l2", 40)]);
    assert_eq!(labels(&d), ["PS 1", "PS 2", "PS 3", "local 1"]);
    // A version whose commit is a Gerrit patch set now.
    let d = merge(gerrit(), vec![local(1, "sha3", 35)]);
    assert_eq!(labels(&d), ["PS 1", "PS 2", "PS 3"]);
    // Old caches without upload times: after them.
    let mut old = gerrit();
    old.revisions.iter_mut().for_each(|r| r.created = None);
    let d = merge(old, vec![local(1, "l1", 5)]);
    assert_eq!(labels(&d), ["PS 1", "PS 2", "PS 3", "local 1"]);
}

#[test]
fn a_fresh_detail_takes_the_local_versions_on_screen() {
    let shown = merge(gerrit(), vec![local(1, "l1", 35)]);
    let mut fresh = gerrit();
    fresh.revisions.push(Revision {
        number: PatchSetNumber(4),
        sha: "sha4".into(),
        created: Some(at(50)),
        ..fresh.revisions[0].clone()
    });
    let d = carry_local(fresh.clone(), Some(&shown));
    assert_eq!(labels(&d), ["PS 1", "PS 2", "PS 3", "local 1", "PS 4"]);
    // Not from another change.
    let mut other = shown.clone();
    other.summary.number = ChangeNumber(8);
    assert_eq!(carry_local(fresh.clone(), Some(&other)).revisions.len(), 4);
    assert_eq!(carry_local(fresh, None).revisions.len(), 4);
}

#[test]
fn the_label_helper_never_shows_a_synthetic_number() {
    let d = merge(gerrit(), vec![local(7, "l1", 35), local(9, "l2", 40)]);
    let second = PatchSetNumber::local(9).expect("local");
    assert_eq!(patch_set_label(&d.revisions, PatchSetNumber(2)), "PS 2");
    assert_eq!(patch_set_label(&d.revisions, second), "local 2");
    assert_eq!(
        patch_set_label(&[], PatchSetNumber::local(9).expect("local")),
        "local"
    );
    assert_eq!(position(&d.revisions, second), Some(4));
    assert_eq!(change_label(ChangeNumber(42)), "#42");
    let synthetic = ChangeNumber::local(3).expect("local");
    assert_eq!(change_label(synthetic), "local");
}

#[test]
fn labels_are_borrowed_or_default_to_code_review() {
    let (permitted, values) = labels_of(None);
    assert_eq!(permitted["Code-Review"], [-2, -1, 0, 1, 2]);
    assert_eq!(
        values["Code-Review"][4].description,
        "Looks good to me, approved"
    );
    assert_eq!(values["Code-Review"][2].description, "No score");

    let value = |v: i32| LabelValue {
        value: v,
        description: format!("v{v}"),
    };
    let borrowed = BorrowedLabels {
        permitted: [("Verified".to_owned(), vec![0, 1])].into(),
        values: [("Verified".to_owned(), vec![value(-1), value(0), value(1)])].into(),
        open: true,
    };
    let (permitted, values) = labels_of(Some(borrowed.clone()));
    assert_eq!(permitted["Verified"], [0, 1]);
    assert_eq!(values["Verified"].len(), 3);
    assert!(!permitted.contains_key("Code-Review"));
    // Nothing permitted (a closed change): the whole range.
    let closed = BorrowedLabels {
        permitted: Default::default(),
        ..borrowed
    };
    let (permitted, _) = labels_of(Some(closed));
    assert_eq!(permitted["Verified"], [-1, 0, 1]);
}

fn syncer(store: Store) -> Syncer {
    let env = env_with(store, &RouteHttpClient::new());
    Session::new(&env, &setup(), None)
        .expect("session")
        .syncer()
        .clone()
}

fn upsert(change_id: &str, branch: Option<&str>, shas: &[&str], email: &str) -> LocalOp {
    LocalOp::Upsert(LocalChangeUpsert {
        seen: None,
        branch: branch.map(str::to_owned),
        change_id: change_id.into(),
        gerrit_number: None,
        subject: "Local work".into(),
        author_name: "Test Author".into(),
        author_email: email.into(),
        updated: at(45),
        state: Some(LocalChangeState::Local),
        patch_sets: shas
            .iter()
            .zip(40..)
            .map(|(sha, m)| NewLocalPatchSet {
                sha: RevisionId((*sha).to_owned()),
                parent_sha: None,
                seen_at: at(m),
            })
            .collect(),
    })
}

fn local_number(store: &Store, change_id: &str) -> ChangeNumber {
    store
        .local_changes_by_change_id(SERVER, change_id)
        .expect("rows")
        .remove(0)
        .number
}

#[test]
fn a_local_change_is_synthesised_from_its_row() {
    let mut store = Store::open_in_memory().expect("store");
    store.server_id(URL).expect("server");
    store
        .reconcile_local(
            SERVER,
            "p",
            &[upsert("I1", Some("main"), &["a", "b"], TEST_EMAIL)],
            at(45),
        )
        .expect("reconcile");
    store
        .set_account_email(SERVER, Some(TEST_EMAIL))
        .expect("email");
    let number = local_number(&store, "I1");
    let syncer = syncer(store);
    let viewer = Viewer {
        username: Some("u".into()),
        account: Some(AccountId(5)),
    };
    let p = cached(&syncer, number, &viewer)
        .expect("read")
        .expect("found");
    assert_eq!(p.fetched_at, None);
    assert_eq!(p.local.as_ref().map(|r| r.change_id.as_str()), Some("I1"));
    let d = p.detail;
    assert_eq!(d.summary.number, number);
    assert_eq!(d.summary.subject, "Local work");
    assert_eq!(d.summary.project, "p");
    assert_eq!(d.summary.branch, "main");
    assert_eq!(d.summary.change_id.as_deref(), Some("I1"));
    assert_eq!(d.summary.status, ChangeStatus::New);
    // The author is the signed-in account.
    assert_eq!(d.summary.owner.id, AccountId(5));
    assert_eq!(d.summary.owner.username.as_deref(), Some("u"));
    assert_eq!(d.summary.owner.name.as_deref(), Some("Test Author"));
    assert_eq!(labels(&d), ["local 1", "local 2"]);
    assert_eq!(
        d.current_revision.as_ref().map(RevisionId::as_str),
        Some("b")
    );
    assert_eq!(d.current_patch_set, Some(d.revisions[1].number));
    assert!(d.reviewers.is_empty() && d.messages.is_empty() && d.votes.is_empty());
    assert!(d.submit_requirements.is_empty() && d.change_actions.is_empty());
    // Nothing cached to borrow from: Code-Review.
    assert_eq!(d.permitted_labels["Code-Review"], [-2, -1, 0, 1, 2]);
    // The message comes from git.
    assert_eq!(d.commit_message, None);
}

#[test]
fn a_local_change_borrows_the_labels_of_a_cached_change_and_knows_other_authors() {
    let mut store = Store::open_in_memory().expect("store");
    store.server_id(URL).expect("server");
    let mut lender = detail(9, 1, 1);
    lender.summary.project = "p".into();
    lender.permitted_labels = [("Verified".to_owned(), vec![-1, 0, 1])].into();
    lender.label_values = [(
        "Verified".to_owned(),
        vec![LabelValue {
            value: 1,
            description: "Works".into(),
        }],
    )]
    .into();
    store
        .save_change_detail(SERVER, &lender, at(1))
        .expect("detail");
    store
        .reconcile_local(
            SERVER,
            "p",
            &[upsert("I1", None, &["a"], "someone@else")],
            at(45),
        )
        .expect("reconcile");
    let number = local_number(&store, "I1");
    let syncer = syncer(store);
    let viewer = Viewer {
        username: Some("u".into()),
        account: Some(AccountId(5)),
    };
    let d = cached(&syncer, number, &viewer)
        .expect("read")
        .expect("found")
        .detail;
    assert_eq!(d.permitted_labels["Verified"], [-1, 0, 1]);
    assert!(!d.permitted_labels.contains_key("Code-Review"));
    assert_eq!(d.summary.branch, "", "unknown target branch");
    assert_eq!(d.summary.owner.id, AccountId(0));
    assert_eq!(d.summary.owner.username, None);
    assert_eq!(d.summary.owner.email.as_deref(), Some("someone@else"));
}

#[test]
fn a_cached_gerrit_change_gets_its_local_versions_and_the_store_keeps_none() {
    let mut store = Store::open_in_memory().expect("store");
    store.server_id(URL).expect("server");
    let mut d = gerrit();
    d.summary.project = "p".into();
    d.summary.change_id = Some("I7".into());
    store
        .save_change_detail(SERVER, &d, at(31))
        .expect("detail");
    let op = LocalOp::Upsert(LocalChangeUpsert {
        gerrit_number: Some(ChangeNumber(7)),
        ..match upsert("I7", Some("main"), &["amended"], TEST_EMAIL) {
            LocalOp::Upsert(u) => u,
            _ => unreachable!(),
        }
    });
    store
        .reconcile_local(SERVER, "p", &[op], at(45))
        .expect("reconcile");
    let syncer = syncer(store);
    let p = cached(&syncer, ChangeNumber(7), &Viewer::default())
        .expect("read")
        .expect("found");
    assert_eq!(p.fetched_at, Some(at(31)));
    assert!(p.local.is_none());
    assert_eq!(labels(&p.detail), ["PS 1", "PS 2", "PS 3", "local 1"]);
    let stored = syncer
        .cached_change(ChangeNumber(7))
        .expect("read")
        .expect("cached")
        .detail;
    assert!(stored.revisions.iter().all(|r| !r.number.is_local()));
    assert!(
        cached(&syncer, ChangeNumber(8), &Viewer::default())
            .expect("read")
            .is_none()
    );
}

#[test]
fn commits_come_from_the_checkout() {
    let repo = TestRepo::new();
    repo.commit("Base");
    repo.write("a.rs", b"one\n");
    let first = repo.commit("Local work\n\nThe body.\n\nChange-Id: I1");
    repo.write("a.rs", b"two\n");
    let second = repo.commit("Local work, amended\n\nChange-Id: I1");
    let mut store = Store::open_in_memory().expect("store");
    store.server_id(URL).expect("server");
    store
        .reconcile_local(
            SERVER,
            "p",
            &[upsert(
                "I1",
                Some("main"),
                &[&first, &second, "gone"],
                TEST_EMAIL,
            )],
            at(45),
        )
        .expect("reconcile");
    let number = local_number(&store, "I1");
    let syncer = syncer(store);
    let mut d = cached(&syncer, number, &Viewer::default())
        .expect("read")
        .expect("found")
        .detail;
    // The newest version ("gone") is not in the checkout: no message.
    let mut git = GitCommits::default();
    let repo = Repo::open(repo.path()).expect("repo");
    assert_eq!(git.wanted(&d).len(), 3);
    git.extend(GitCommits::read(&repo, &git.wanted(&d)));
    assert!(
        git.wanted(&d).is_empty(),
        "a missing commit is not asked again"
    );
    apply_commits(&mut d, &git);
    assert!(d.revisions[0].commit.is_some() && d.revisions[2].commit.is_none());
    assert_eq!(d.commit_message, None);
    let author = d.revisions[0]
        .commit
        .as_ref()
        .and_then(|c| c.author.clone());
    assert_eq!(author.and_then(|a| a.email).as_deref(), Some(TEST_EMAIL));

    // Without the missing one, the newest version's message and dates.
    let mut d = cached(&syncer, number, &Viewer::default())
        .expect("read")
        .expect("found")
        .detail;
    d.revisions.pop();
    d.current_revision = Some(RevisionId(second.clone()));
    d.current_patch_set = Some(d.revisions[1].number);
    apply_commits(&mut d, &git);
    assert_eq!(
        d.commit_message.as_deref().map(str::trim_end),
        Some("Local work, amended\n\nChange-Id: I1")
    );
    let committed = d.revisions[1]
        .commit
        .as_ref()
        .and_then(|c| c.committer.as_ref())
        .and_then(|p| p.date);
    assert_eq!(Some(d.summary.updated), committed);
}
