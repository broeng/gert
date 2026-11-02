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

#[test]
fn the_cache_generation_moves_when_a_matching_change_is_cached() {
    let (http, syncer) = syncer();
    let ids = vec!["I2".to_owned()];
    let empty = syncer.local_cache_generation("p", &ids).unwrap();
    assert_eq!(empty, syncer.local_cache_generation("p", &ids).unwrap());
    http.push_response(200, change_42());
    block_on(syncer.refresh_change(ChangeNumber(42))).unwrap();
    let cached = syncer.local_cache_generation("p", &ids).unwrap();
    assert_ne!(cached, empty);
    // Unrelated Change-Ids and other projects do not see it.
    assert_eq!(
        syncer
            .local_cache_generation("p", &["I9".to_owned()])
            .unwrap(),
        empty
    );
    assert_eq!(syncer.local_cache_generation("q", &ids).unwrap(), empty);
    // Fetching the same detail again changes nothing.
    http.push_response(200, change_42());
    block_on(syncer.refresh_change(ChangeNumber(42))).unwrap();
    assert_eq!(syncer.local_cache_generation("p", &ids).unwrap(), cached);
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
