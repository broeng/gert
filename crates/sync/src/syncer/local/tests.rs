use std::sync::{Arc, Mutex};

use futures::executor::block_on;
use gerrit_api::{Auth, GerritClient, fake::FakeHttpClient};
use model::{
    AccountId, BranchTargets, ChangeNumber, CommitInfo, LocalChangeState, LocalCommit, Signature,
    Upstream,
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
        on_head: true,
    }
}

fn scan(commits: Vec<LocalCommit>) -> LocalScan {
    let head_commits = commits.iter().map(|c| c.commit.sha.clone()).collect();
    LocalScan {
        fingerprint: 7,
        commits,
        truncated: false,
        no_remotes: false,
        detached_head: false,
        head_commits,
        head_truncated: false,
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
    assert_eq!(syncer.account().unwrap(), Some((AccountId(1), None)));
    http.push_response(200, ")]}'\n{\"_account_id\": 2, \"username\": \"u\"}");
    block_on(syncer.account_self()).unwrap();
    assert_eq!(syncer.account_email().unwrap(), None);
    assert_eq!(
        syncer.account().unwrap(),
        Some((AccountId(2), Some("u".into())))
    );
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

#[test]
fn local_versions_keep_their_names_when_one_is_pushed_and_across_rows() {
    let (http, syncer) = syncer();
    http.push_response(200, change_42());
    block_on(syncer.refresh_change(ChangeNumber(42))).unwrap();
    let ordinals = |syncer: &Syncer| -> Vec<(String, u32)> {
        syncer
            .local_patch_sets_of(ChangeNumber(42))
            .unwrap()
            .into_iter()
            .map(|v| (v.sha.0, v.ordinal))
            .collect()
    };
    // Two versions of 42 (I2), seen one after the other.
    let sc = scan(vec![commit("v1", "I2")]);
    syncer.reconcile_local("p", &sc, &emails(), true).unwrap();
    let sc = scan(vec![commit("v2", "I2")]);
    syncer.reconcile_local("p", &sc, &emails(), true).unwrap();
    assert_eq!(ordinals(&syncer), [("v1".into(), 1), ("v2".into(), 2)]);

    // v1 is pushed as patch set 2: v2 is still "local 2".
    let mut detail = syncer
        .cached_change(ChangeNumber(42))
        .unwrap()
        .unwrap()
        .detail;
    let mut ps2 = detail.revisions[0].clone();
    ps2.number = model::PatchSetNumber(2);
    ps2.sha = "v1".into();
    detail.revisions.push(ps2);
    syncer
        .store()
        .save_change_detail(syncer.server, &detail, Timestamp::UNIX_EPOCH)
        .unwrap();
    assert_eq!(ordinals(&syncer), [("v2".into(), 2)]);

    // A second row of 42 (as a change of target branch leaves it) with a
    // later version: the versions of both rows are numbered together.
    let op = store::LocalOp::Upsert(store::LocalChangeUpsert {
        seen: None,
        branch: None,
        change_id: "I2".into(),
        gerrit_number: Some(ChangeNumber(42)),
        subject: "s".into(),
        author_name: "Me".into(),
        author_email: "me@example.com".into(),
        updated: Timestamp::now(),
        state: Some(LocalChangeState::Local),
        patch_sets: vec![store::NewLocalPatchSet {
            sha: "v3".into(),
            parent_sha: None,
            seen_at: Timestamp::now(),
        }],
    });
    syncer
        .store()
        .reconcile_local(syncer.server, "p", &[op], Timestamp::now())
        .unwrap();
    let rows = syncer
        .store()
        .local_changes_of_gerrit_change(syncer.server, ChangeNumber(42))
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(ordinals(&syncer), [("v2".into(), 2), ("v3".into(), 3)]);
}

#[test]
fn chain_commits_are_told_apart_from_the_store() {
    let (http, syncer) = syncer();
    http.push_response(200, change_42());
    block_on(syncer.refresh_change(ChangeNumber(42))).unwrap();
    // `amended` is a local version of 42 (I2); I1 is a change of its own.
    let sc = scan(vec![commit("amended", "I2"), commit("c1", "I1")]);
    syncer.reconcile_local("p", &sc, &emails(), true).unwrap();
    let own = syncer
        .store()
        .local_changes_by_change_id(syncer.server, "I1")
        .unwrap()
        .remove(0);
    let (_, own_versions) = syncer.local_change(own.number).unwrap().unwrap();
    let local_42 = syncer.local_patch_sets_of(ChangeNumber(42)).unwrap();

    let mut other = commit("other", "I9").commit;
    other.change_id = None;
    let chain = [
        commit("c1", "I1").commit,
        commit("amended", "I2").commit,
        commit("pushed", "I2").commit,
        other,
    ];
    let kinds = syncer.classify_chain("p", &chain).unwrap();
    assert_eq!(
        kinds[0],
        ChainCommit::Local {
            change: own.number,
            patch_set: own_versions[0].number,
            ordinal: 1,
            newest: true,
        }
    );
    assert_eq!(
        kinds[1],
        ChainCommit::LocalVersion {
            change: ChangeNumber(42),
            patch_set: local_42[0].number,
            ordinal: 1,
        }
    );
    let ChainCommit::Gerrit {
        change,
        patch_set,
        current,
        summary,
    } = &kinds[2]
    else {
        panic!("{:?}", kinds[2]);
    };
    assert_eq!(
        (*change, *patch_set, *current),
        (ChangeNumber(42), PatchSetNumber(1), Some(PatchSetNumber(1)))
    );
    assert_eq!(summary.as_ref().map(|s| s.subject.as_str()), Some("s"));
    assert_eq!(kinds[3], ChainCommit::Unknown);

    // Another project's checkout knows none of them.
    let kinds = syncer.classify_chain("q", &chain).unwrap();
    assert!(
        kinds.iter().all(|k| *k == ChainCommit::Unknown),
        "{kinds:?}"
    );

    // Pushed: the local version is patch set 2 of 42 now.
    let mut detail = syncer
        .cached_change(ChangeNumber(42))
        .unwrap()
        .unwrap()
        .detail;
    let mut ps2 = detail.revisions[0].clone();
    ps2.number = PatchSetNumber(2);
    ps2.sha = "amended".into();
    detail.revisions.push(ps2);
    detail.current_patch_set = Some(PatchSetNumber(2));
    syncer
        .store()
        .save_change_detail(syncer.server, &detail, Timestamp::UNIX_EPOCH)
        .unwrap();
    let kinds = syncer.classify_chain("p", &chain[1..2]).unwrap();
    assert!(
        matches!(
            kinds[0],
            ChainCommit::Gerrit {
                change: ChangeNumber(42),
                patch_set: PatchSetNumber(2),
                ..
            }
        ),
        "{kinds:?}"
    );
    assert_eq!(http.requests().len(), 1);
}

/// A change as the server sends it: Change-Id `I2`, one patch set whose
/// commit is `sha`.
fn change_on(number: u64, branch: &str, sha: &str) -> String {
    let body = json!({
        "_number": number, "branch": branch, "change_id": "I2", "project": "p",
        "created": "2024-02-01 09:59:32.126000000", "owner": {"_account_id": 1},
        "status": "NEW", "subject": "s", "updated": "2024-02-01 10:00:00.000000000",
        "mergeable": true, "current_revision": sha,
        "revisions": {sha: {"_number": 1}}
    });
    format!(")]}}'\n{body}")
}

fn upsert(
    change_id: &str,
    gerrit: Option<ChangeNumber>,
    state: LocalChangeState,
    sha: &str,
) -> store::LocalOp {
    store::LocalOp::Upsert(store::LocalChangeUpsert {
        seen: None,
        branch: Some("main".into()),
        change_id: change_id.into(),
        gerrit_number: gerrit,
        subject: "s".into(),
        author_name: "Me".into(),
        author_email: "me@example.com".into(),
        updated: Timestamp::now(),
        state: Some(state),
        patch_sets: vec![store::NewLocalPatchSet {
            sha: sha.into(),
            parent_sha: None,
            seen_at: Timestamp::now(),
        }],
    })
}

#[test]
fn chain_commits_in_the_corners() {
    let (http, syncer) = syncer();
    // The same Change-Id on two branches: 42 on main, 43 on stable.
    http.push_response(200, change_on(42, "main", "pushed"));
    block_on(syncer.refresh_change(ChangeNumber(42))).unwrap();
    http.push_response(200, change_on(43, "stable", "on-stable"));
    block_on(syncer.refresh_change(ChangeNumber(43))).unwrap();
    // I1: a local change amended once; I5: versions of 55 (not cached),
    // one mapped to its patch set 3; I6: versions of 66, consolidated.
    let server = syncer.server;
    let apply = |op| {
        syncer
            .store()
            .reconcile_local(server, "p", &[op], Timestamp::now())
            .unwrap()
    };
    apply(upsert("I1", None, LocalChangeState::Local, "v1"));
    apply(upsert("I1", None, LocalChangeState::Local, "v2"));
    apply(upsert(
        "I5",
        Some(ChangeNumber(55)),
        LocalChangeState::Local,
        "m5",
    ));
    let m5 = syncer
        .store()
        .local_patch_sets_by_sha(server, &"m5".into())
        .unwrap()
        .remove(0);
    let report = apply(store::LocalOp::MapPatchSet {
        patch_set: m5.id,
        gerrit_patch_set: PatchSetNumber(3),
    });
    assert_eq!(report.patch_sets_mapped, 1);
    apply(upsert(
        "I6",
        Some(ChangeNumber(66)),
        LocalChangeState::Consolidated,
        "x6",
    ));
    let i6 = syncer
        .store()
        .local_changes_by_change_id(server, "I6")
        .unwrap()
        .remove(0);
    assert_eq!(i6.state, LocalChangeState::Consolidated);

    let chain: Vec<CommitInfo> = ["v1", "v2", "m5", "x6", "on-stable", "pushed"]
        .iter()
        .map(|sha| commit(sha, "I2").commit)
        .collect();
    let kinds = syncer.classify_chain("p", &chain).unwrap();
    // The older version of I1 is not its newest; the newer one is.
    assert!(
        matches!(
            kinds[0],
            ChainCommit::Local {
                ordinal: 1,
                newest: false,
                ..
            }
        ),
        "{:?}",
        kinds[0]
    );
    assert!(
        matches!(
            kinds[1],
            ChainCommit::Local {
                ordinal: 2,
                newest: true,
                ..
            }
        ),
        "{:?}",
        kinds[1]
    );
    // Mapped while 55 is not cached: its patch set, nothing else known.
    assert_eq!(
        kinds[2],
        ChainCommit::Gerrit {
            change: ChangeNumber(55),
            patch_set: PatchSetNumber(3),
            current: None,
            summary: None,
        }
    );
    // What was on a consolidated row's versions moved to its change.
    assert_eq!(kinds[3], ChainCommit::Unknown);
    // Matched by commit, not by Change-Id: the right branch's change.
    for (kind, number) in [(&kinds[4], 43), (&kinds[5], 42)] {
        assert!(
            matches!(kind, ChainCommit::Gerrit { change, patch_set: PatchSetNumber(1), .. }
                if *change == ChangeNumber(number)),
            "{kind:?}"
        );
    }
    assert_eq!(http.requests().len(), 2);
}

/// The local versions of change 42 (`I2`) with commits `shas`, seen at
/// minutes 1, 2, ... of `t(0)`.
fn versions_of_42(shas: &[&str]) -> store::LocalOp {
    store::LocalOp::Upsert(store::LocalChangeUpsert {
        seen: None,
        branch: Some("main".into()),
        change_id: "I2".into(),
        gerrit_number: Some(ChangeNumber(42)),
        subject: "s".into(),
        author_name: "Me".into(),
        author_email: "me@example.com".into(),
        updated: Timestamp::UNIX_EPOCH,
        state: Some(LocalChangeState::Local),
        patch_sets: shas
            .iter()
            .zip(1..)
            .map(|(sha, m)| store::NewLocalPatchSet {
                sha: (*sha).into(),
                parent_sha: None,
                seen_at: Timestamp::UNIX_EPOCH + jiff::SignedDuration::from_mins(m),
            })
            .collect(),
    })
}

fn newer(syncer: &Syncer, on_branches: Option<&[RevisionId]>) -> Option<String> {
    syncer
        .newer_local_version(ChangeNumber(42), on_branches)
        .unwrap()
        .map(|v| format!("{} local {}", v.sha, v.ordinal))
}

#[test]
fn the_newer_local_version_is_the_one_on_a_branch_nearest_its_tip() {
    let (http, syncer) = syncer();
    http.push_response(200, change_42());
    block_on(syncer.refresh_change(ChangeNumber(42))).unwrap();
    assert_eq!(newer(&syncer, None), None);
    let ops = [versions_of_42(&["amended", "amended2"])];
    syncer
        .store()
        .reconcile_local(syncer.server, "p", &ops, Timestamp::UNIX_EPOCH)
        .unwrap();

    // A fresh scan: the first of its commits (newest first) that is a
    // version, whichever was seen last.
    let shas: Vec<RevisionId> = vec!["other".into(), "amended".into(), "amended2".into()];
    assert_eq!(
        newer(&syncer, Some(&shas)).as_deref(),
        Some("amended local 1")
    );
    assert_eq!(
        newer(&syncer, Some(&shas[2..])).as_deref(),
        Some("amended2 local 2")
    );
    // Neither on a branch any more.
    assert_eq!(newer(&syncer, Some(&shas[..1])), None);
    // No scan: the newest seen, as no version reached Gerrit.
    assert_eq!(newer(&syncer, None).as_deref(), Some("amended2 local 2"));
    // Not for a local change's number.
    let own = ChangeNumber(ChangeNumber::LOCAL_BASE + 1);
    assert_eq!(syncer.newer_local_version(own, None).unwrap(), None);

    // Gone from the checkout: the row is deleted (it has no unsent data).
    syncer
        .reconcile_local("p", &scan(vec![]), &emails(), true)
        .unwrap();
    assert_eq!(newer(&syncer, None), None);
    assert_eq!(http.requests().len(), 1);
}

#[test]
fn a_patch_set_made_on_gerrit_after_the_version_does_not_hide_it() {
    let (http, syncer) = syncer();
    // 42's current patch set was made long after the local version was
    // seen (a rebase on Gerrit, or a co-author's upload), by a server
    // whose clock is ahead: it still counts.
    let body = json!({
        "_number": 42, "branch": "main", "change_id": "I2", "project": "p",
        "created": "2024-02-01 09:59:32.126000000", "owner": {"_account_id": 1},
        "status": "NEW", "subject": "s", "updated": "2099-02-01 10:00:00.000000000",
        "mergeable": true, "current_revision": "rebased",
        "revisions": {
            "pushed": {"_number": 1, "created": "2024-02-01 09:59:32.126000000"},
            "rebased": {"_number": 2, "created": "2099-02-01 10:00:00.000000000"}
        }
    });
    http.push_response(200, format!(")]}}'\n{body}"));
    block_on(syncer.refresh_change(ChangeNumber(42))).unwrap();
    let ops = [versions_of_42(&["amended"])];
    syncer
        .store()
        .reconcile_local(syncer.server, "p", &ops, Timestamp::UNIX_EPOCH)
        .unwrap();
    assert_eq!(newer(&syncer, None).as_deref(), Some("amended local 1"));
    assert_eq!(
        newer(&syncer, Some(&["amended".into()])).as_deref(),
        Some("amended local 1")
    );
}

#[test]
fn without_a_scan_only_versions_after_the_last_push_count() {
    let (http, syncer) = syncer();
    http.push_response(200, change_42());
    block_on(syncer.refresh_change(ChangeNumber(42))).unwrap();
    // `old` (minute 1), then `pushed` (minute 2), which reached Gerrit.
    let ops = [versions_of_42(&["old", "pushed"])];
    let mut store = syncer.store();
    store
        .reconcile_local(syncer.server, "p", &ops, Timestamp::UNIX_EPOCH)
        .unwrap();
    let row = store
        .local_changes_by_change_id(syncer.server, "I2")
        .unwrap()
        .remove(0);
    let pushed = store.local_patch_sets(syncer.server, row.id).unwrap()[1].id;
    let map = store::LocalOp::MapPatchSet {
        patch_set: pushed,
        gerrit_patch_set: model::PatchSetNumber(2),
    };
    store
        .reconcile_local(syncer.server, "p", &[map], Timestamp::UNIX_EPOCH)
        .unwrap();
    drop(store);
    assert_eq!(newer(&syncer, None), None, "older than the push");
    // A scan still sees `old` on a branch.
    assert_eq!(
        newer(&syncer, Some(&["old".into()])).as_deref(),
        Some("old local 1")
    );
    // Amended after the push (minute 3).
    let ops = [versions_of_42(&["old", "pushed", "new"])];
    syncer
        .store()
        .reconcile_local(syncer.server, "p", &ops, Timestamp::UNIX_EPOCH)
        .unwrap();
    assert_eq!(newer(&syncer, None).as_deref(), Some("new local 3"));
}

#[test]
fn a_submit_looks_down_the_cached_chain() {
    let (http, syncer) = syncer();
    http.push_response(200, change_42());
    block_on(syncer.refresh_change(ChangeNumber(42))).unwrap();
    let ops = [versions_of_42(&["amended"])];
    syncer
        .store()
        .reconcile_local(syncer.server, "p", &ops, Timestamp::UNIX_EPOCH)
        .unwrap();
    let related = |n: u64, status| model::RelatedChange {
        project: "p".into(),
        change_id: None,
        number: ChangeNumber(n),
        patch_set: model::PatchSetNumber(1),
        current_patch_set: None,
        status: Some(status),
        submittable: None,
        work_in_progress: false,
        commit: None,
    };
    // 50 is built on 42, which is on 41 (merged).
    let chain = [
        related(50, ChangeStatus::New),
        related(42, ChangeStatus::New),
        related(41, ChangeStatus::Merged),
    ];
    syncer
        .store()
        .save_related(
            syncer.server,
            ChangeNumber(50),
            &"sha50".into(),
            &chain,
            Timestamp::UNIX_EPOCH,
        )
        .unwrap();
    let found = syncer
        .newer_local_versions(ChangeNumber(50), &"sha50".into(), None)
        .unwrap();
    assert_eq!(
        found
            .iter()
            .map(|(n, v)| (n.0, v.sha.as_str()))
            .collect::<Vec<_>>(),
        [(42, "amended")]
    );
    // From 42 itself; nothing cached for another patch set.
    assert_eq!(
        syncer
            .newer_local_versions(ChangeNumber(42), &"pushed".into(), None)
            .unwrap()
            .len(),
        1
    );
    assert!(
        syncer
            .newer_local_versions(ChangeNumber(50), &"other".into(), None)
            .unwrap()
            .is_empty()
    );
}
