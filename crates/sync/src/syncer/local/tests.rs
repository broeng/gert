use std::sync::{Arc, Mutex};

use futures::executor::block_on;
use gerrit_api::{Auth, GerritClient, fake::FakeHttpClient};
use model::{
    BranchTargets, ChangeNumber, CommitInfo, LocalChangeState, LocalCommit, Signature, Upstream,
};
use serde_json::json;
use store::Store;

use super::*;

const BASE: &str = "https://g.example";

fn syncer() -> (FakeHttpClient, Syncer) {
    let http = FakeHttpClient::new();
    let client = GerritClient::new(Arc::new(http.clone()), BASE, Auth::Anonymous).unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let server = store.server_id(BASE).unwrap();
    (
        http,
        Syncer::new(client, Arc::new(Mutex::new(store)), server),
    )
}

fn commit(sha: &str, change_id: &str) -> LocalCommit {
    let sig = Signature {
        name: "Me".into(),
        email: "me@example.com".into(),
        time: Timestamp::UNIX_EPOCH,
        tz: 0,
    };
    LocalCommit {
        commit: CommitInfo {
            sha: sha.into(),
            parents: vec!["base".into()],
            author: sig.clone(),
            committer: sig,
            message: format!("Subject {sha}\n"),
            change_id: Some(change_id.into()),
        },
        branches: vec!["topic".into()],
    }
}

fn scan(commits: Vec<LocalCommit>) -> LocalScan {
    LocalScan {
        fingerprint: 7,
        commits,
        truncated: false,
        no_remotes: false,
        targets: BranchTargets {
            upstreams: [(
                "topic".to_owned(),
                Upstream {
                    remote: "origin".into(),
                    branch: "main".into(),
                },
            )]
            .into(),
            defaults: Default::default(),
        },
    }
}

fn emails() -> Vec<String> {
    vec!["me@example.com".into()]
}

fn states(syncer: &Syncer) -> Vec<(String, LocalChangeState, Option<ChangeNumber>)> {
    let mut rows: Vec<_> = syncer
        .store()
        .local_changes(syncer.server)
        .unwrap()
        .into_iter()
        .map(|c| (c.change_id, c.state, c.gerrit_number))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// Change 42 with Change-Id `I2` as the server sends it, patch set 1 being
/// commit `pushed`.
fn change_42() -> String {
    let body = json!({
        "_number": 42, "branch": "main", "change_id": "I2", "project": "p",
        "created": "2024-02-01 09:59:32.126000000", "owner": {"_account_id": 1},
        "status": "NEW", "subject": "s", "updated": "2024-02-01 10:00:00.000000000",
        "mergeable": true, "current_revision": "pushed",
        "revisions": {"pushed": {"_number": 1}}
    });
    format!(")]}}'\n{body}")
}

#[test]
fn reconciling_reads_the_cache_and_sends_nothing() {
    let (http, syncer) = syncer();
    let sc = scan(vec![commit("amended", "I2"), commit("c1", "I1")]);
    let report = syncer.reconcile_local("p", &sc, &emails(), true).unwrap();
    assert_eq!(report.inserted.len(), 2);
    assert!(report.is_settled());
    assert_eq!(
        states(&syncer),
        [
            ("I1".to_owned(), LocalChangeState::Checking, None),
            ("I2".to_owned(), LocalChangeState::Checking, None),
        ]
    );

    // Change 42 (I2) gets cached: I2's commit is a local patch set of it.
    http.push_response(200, change_42());
    block_on(syncer.refresh_change(ChangeNumber(42))).unwrap();
    let requests = http.requests().len();
    let report = syncer.reconcile_local("p", &sc, &emails(), true).unwrap();
    assert!(report.is_settled());
    assert_eq!(states(&syncer)[1].2, Some(ChangeNumber(42)));
    // A commit that is a cached patch set never becomes local.
    let report = syncer
        .reconcile_local("p", &scan(vec![commit("pushed", "I2")]), &emails(), true)
        .unwrap();
    assert!(report.is_settled());
    assert!(
        syncer
            .store()
            .local_patch_sets_by_sha(syncer.server, &"pushed".into())
            .unwrap()
            .is_empty()
    );
    // Another project's rows and cache are not involved.
    let report = syncer.reconcile_local("q", &sc, &emails(), true).unwrap();
    assert_eq!(report.inserted.len(), 2);
    assert_eq!(http.requests().len(), requests);
}

#[test]
fn turning_the_option_off_keeps_only_changes_with_unsent_data() {
    let (http, syncer) = syncer();
    let sc = scan(vec![commit("c2", "I2"), commit("c1", "I1")]);
    syncer.reconcile_local("p", &sc, &emails(), true).unwrap();
    let kept = {
        let mut store = syncer.store();
        let c = store
            .local_changes_by_change_id(syncer.server, "I2")
            .unwrap()
            .remove(0);
        let p = store
            .local_patch_sets(syncer.server, c.id)
            .unwrap()
            .remove(0);
        store
            .create_draft(
                syncer.server,
                c.number,
                &model::PatchSetRef {
                    number: p.number,
                    sha: p.sha,
                },
                &model::DraftInput {
                    path: "a.rs".into(),
                    side: model::Side::Revision,
                    parent: None,
                    location: model::Location::Line(1),
                    in_reply_to: None,
                    message: "keep me".into(),
                    unresolved: true,
                },
                Timestamp::UNIX_EPOCH,
            )
            .unwrap();
        c.id
    };
    // The scan does not matter with the option off.
    let report = syncer.reconcile_local("p", &sc, &emails(), false).unwrap();
    assert!(report.is_settled());
    assert_eq!(report.deleted.len(), 1);
    assert_eq!(
        states(&syncer),
        [("I2".to_owned(), LocalChangeState::Checking, None)]
    );
    assert!(syncer.store().has_unsent(syncer.server, kept).unwrap());
    assert!(http.requests().is_empty());
}

/// A draft on the first version of the local change with `change_id`.
fn draft_on(syncer: &Syncer, change_id: &str) -> model::Draft {
    let mut store = syncer.store();
    let c = store
        .local_changes_by_change_id(syncer.server, change_id)
        .unwrap()
        .remove(0);
    let p = store
        .local_patch_sets(syncer.server, c.id)
        .unwrap()
        .remove(0);
    store
        .create_draft(
            syncer.server,
            c.number,
            &model::PatchSetRef {
                number: p.number,
                sha: p.sha,
            },
            &model::DraftInput {
                path: "a.rs".into(),
                side: model::Side::Revision,
                parent: None,
                location: model::Location::Line(1),
                in_reply_to: None,
                message: "keep me".into(),
                unresolved: true,
            },
            Timestamp::UNIX_EPOCH,
        )
        .unwrap()
}

#[test]
fn the_generation_follows_the_cache_but_not_its_update_times() {
    let (http, syncer) = syncer();
    let ids = vec!["I2".to_owned()];
    let empty = syncer.local_generation("p", &ids).unwrap();
    assert_eq!(empty, syncer.local_generation("p", &ids).unwrap());
    http.push_response(200, change_42());
    block_on(syncer.refresh_change(ChangeNumber(42))).unwrap();
    let cached = syncer.local_generation("p", &ids).unwrap();
    assert_ne!(cached, empty);
    // Unrelated Change-Ids and other projects do not see it.
    assert_eq!(
        syncer.local_generation("p", &["I9".to_owned()]).unwrap(),
        empty
    );
    assert_eq!(syncer.local_generation("q", &ids).unwrap(), empty);
    // Fetching the same detail again changes nothing, and neither does a
    // new `updated` (a comment).
    http.push_response(200, change_42());
    block_on(syncer.refresh_change(ChangeNumber(42))).unwrap();
    assert_eq!(syncer.local_generation("p", &ids).unwrap(), cached);
    http.push_response(200, change_42().replace("10:00:00.000", "11:30:00.000"));
    block_on(syncer.refresh_change(ChangeNumber(42))).unwrap();
    assert_eq!(syncer.local_generation("p", &ids).unwrap(), cached);
    // The status does.
    http.push_response(200, change_42().replace("\"NEW\"", "\"MERGED\""));
    block_on(syncer.refresh_change(ChangeNumber(42))).unwrap();
    assert_ne!(syncer.local_generation("p", &ids).unwrap(), cached);
}

#[test]
fn the_generation_follows_the_rows_and_their_change_ids() {
    let (http, syncer) = syncer();
    let sc = scan(vec![commit("c2", "I2")]);
    let (report, generation) = syncer.reconcile_local_keyed("p", &sc, &emails()).unwrap();
    assert!(report.is_settled());
    let ids = crate::local::scan_change_ids(&sc);
    assert_eq!(ids, ["I2"]);
    // Read after the plan was applied: the same as asking again.
    assert_eq!(syncer.local_generation("p", &ids).unwrap(), generation);
    // A second reconcile of the same scan changes nothing and agrees.
    let (report, again) = syncer.reconcile_local_keyed("p", &sc, &emails()).unwrap();
    assert!(!report.changed());
    assert_eq!(again, generation);

    // Unsent data on a row moves it, and so does its going away.
    let draft = draft_on(&syncer, "I2");
    let with_draft = syncer.local_generation("p", &ids).unwrap();
    assert_ne!(with_draft, generation);
    syncer
        .store()
        .delete_draft(syncer.server, draft.local_id)
        .unwrap();
    assert_eq!(syncer.local_generation("p", &ids).unwrap(), generation);

    // A row whose commits left the scan still counts its Change-Id's
    // cached change.
    let without = syncer.local_generation("p", &[]).unwrap();
    http.push_response(200, change_42());
    block_on(syncer.refresh_change(ChangeNumber(42))).unwrap();
    assert_ne!(syncer.local_generation("p", &[]).unwrap(), without);
}

#[test]
fn the_account_email_is_kept_when_the_account_is_read() {
    let (http, syncer) = syncer();
    assert_eq!(syncer.account_email().unwrap(), None);
    http.push_response(
        200,
        ")]}'\n{\"_account_id\": 1, \"email\": \" me@example.com \"}",
    );
    block_on(syncer.account_self()).unwrap();
    assert_eq!(
        syncer.account_email().unwrap().as_deref(),
        Some("me@example.com")
    );
    http.push_response(200, ")]}'\n{\"_account_id\": 1}");
    block_on(syncer.account_self()).unwrap();
    assert_eq!(syncer.account_email().unwrap(), None);
}

#[test]
fn dashboard_rows_and_projects_with_rows() {
    let (_http, syncer) = syncer();
    let sc = scan(vec![commit("c1", "I1")]);
    syncer.reconcile_local("p", &sc, &emails(), true).unwrap();
    syncer.reconcile_local("q", &sc, &emails(), true).unwrap();
    assert_eq!(syncer.local_change_projects().unwrap(), ["p", "q"]);
    let rows = syncer.dashboard_local_changes().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|c| c.number.is_local()));
}

#[test]
fn local_versions_of_a_gerrit_change_and_of_a_local_change() {
    let (http, syncer) = syncer();
    http.push_response(200, change_42());
    block_on(syncer.refresh_change(ChangeNumber(42))).unwrap();
    // `amended` is a local version of 42 (I2); I1 is a change of its own.
    let sc = scan(vec![commit("amended", "I2"), commit("c1", "I1")]);
    syncer.reconcile_local("p", &sc, &emails(), true).unwrap();

    let versions = syncer.local_patch_sets_of(ChangeNumber(42)).unwrap();
    assert_eq!(
        versions.iter().map(|v| v.sha.as_str()).collect::<Vec<_>>(),
        ["amended"]
    );
    assert!(versions[0].number.is_local());
    assert!(
        syncer
            .local_patch_sets_of(ChangeNumber(7))
            .unwrap()
            .is_empty()
    );

    let own = syncer
        .store()
        .local_changes_by_change_id(syncer.server, "I1")
        .unwrap()
        .remove(0);
    let (row, versions) = syncer.local_change(own.number).unwrap().expect("the row");
    assert_eq!(row.change_id, "I1");
    assert_eq!(versions.len(), 1);
    assert!(syncer.local_patch_sets_of(own.number).unwrap().is_empty());
    assert!(syncer.local_change(ChangeNumber(42)).unwrap().is_none());

    // Pushed: the version is a Gerrit patch set now and drops out.
    let mut detail = syncer
        .cached_change(ChangeNumber(42))
        .unwrap()
        .unwrap()
        .detail;
    let mut ps2 = detail.revisions[0].clone();
    ps2.number = model::PatchSetNumber(2);
    ps2.sha = "amended".into();
    detail.revisions.push(ps2);
    syncer
        .store()
        .save_change_detail(syncer.server, &detail, Timestamp::UNIX_EPOCH)
        .unwrap();
    assert!(
        syncer
            .local_patch_sets_of(ChangeNumber(42))
            .unwrap()
            .is_empty()
    );
    assert_eq!(http.requests().len(), 1);
}

#[test]
fn labels_are_borrowed_from_the_branch_else_the_project() {
    let (_http, syncer) = syncer();
    assert_eq!(syncer.labels_to_borrow("p", Some("main")).unwrap(), None);
    let mut d: model::ChangeDetail = serde_json::from_value(json!({
            "summary": {
                "number": 9, "change_id": null, "project": "p", "branch": "stable",
                "subject": "s", "status": "New",
                "owner": {"id": 1, "name": null, "email": null, "username": null,
                          "display_name": null},
                "created": "2024-01-01T00:00:00Z", "updated": "2024-01-01T00:00:00Z",
                "insertions": 0, "deletions": 0, "labels": {}, "work_in_progress": false,
                "is_private": false, "unresolved_comment_count": 0,
                "total_comment_count": 0, "topic": null, "hashtags": [],
                "attention_set": [], "submittable": null
            },
            "current_revision": null, "current_patch_set": null, "commit_message": null,
            "reviewers": [], "cc": []
    }))
    .unwrap();
    d.permitted_labels = [("Verified".to_owned(), vec![-1, 0, 1])].into();
    d.label_values = [(
        "Verified".to_owned(),
        vec![model::LabelValue {
            value: 1,
            description: "Works".into(),
        }],
    )]
    .into();
    syncer
        .store()
        .save_change_detail(syncer.server, &d, Timestamp::UNIX_EPOCH)
        .unwrap();
    // No change of `main` cached: the project's.
    let b = syncer.labels_to_borrow("p", Some("main")).unwrap().unwrap();
    assert_eq!(b.permitted["Verified"], [-1, 0, 1]);
    assert!(
        syncer
            .labels_to_borrow("p", Some("stable"))
            .unwrap()
            .is_some()
    );
    assert!(syncer.labels_to_borrow("p", None).unwrap().is_some());
    assert_eq!(syncer.labels_to_borrow("q", Some("main")).unwrap(), None);
}
