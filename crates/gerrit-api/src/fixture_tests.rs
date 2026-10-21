//! Tests against the schema-generated fixtures in `tests/fixtures/`
//! (regenerate with `cargo xtask gen-fixtures`).

use std::{fs, path::PathBuf, sync::Arc};

use futures::executor::block_on;
use gerrit_client::models as sdk;
use model::{AccountId, ChangeNumber, ChangeStatus};
use serde_json::Value;

use crate::{Auth, ChangeQuery, GerritClient, convert, fake::FakeHttpClient, wire};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

fn read(name: &str) -> String {
    fs::read_to_string(dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// Names of fixtures starting with `prefix`, sorted.
fn names(prefix: &str) -> Vec<String> {
    let mut v: Vec<_> = fs::read_dir(dir())
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with(prefix) && n.ends_with(".json"))
        .collect();
    v.sort();
    v
}

fn client_with(name: &str) -> (GerritClient, FakeHttpClient) {
    let fake = FakeHttpClient::new();
    fake.push_response(200, read(name));
    let c = GerritClient::new(Arc::new(fake.clone()), "https://h", Auth::Anonymous).unwrap();
    (c, fake)
}

#[test]
fn every_change_info_fixture_converts() {
    let files = names("ChangeInfo.");
    assert!(files.len() >= 3, "{files:?}");
    for f in files {
        let v: Value = serde_json::from_str(&read(&f)).unwrap();
        let w = wire::parse_change(v).unwrap_or_else(|e| panic!("{f}: {e}"));
        let s = convert::change_summary(&w).unwrap_or_else(|e| panic!("{f}: {e}"));
        assert_eq!(s.number, ChangeNumber(12345), "{f}");
    }
}

#[test]
fn every_account_info_fixture_converts() {
    let files = names("AccountInfo.");
    assert!(files.len() >= 2, "{files:?}");
    for f in files {
        let a: sdk::AccountInfo = serde_json::from_str(&read(&f)).unwrap();
        let a = convert::account(&a).unwrap_or_else(|e| panic!("{f}: {e}"));
        assert_eq!(a.id, AccountId(1_000_001), "{f}");
    }
}

#[test]
fn other_schema_fixtures_deserialize_into_sdk_models() {
    macro_rules! check {
        ($ty:ty, $prefix:expr) => {{
            let files = names($prefix);
            assert_eq!(files.len(), 2, "{files:?}");
            for f in files {
                serde_json::from_str::<$ty>(&read(&f)).unwrap_or_else(|e| panic!("{f}: {e}"));
            }
        }};
    }
    check!(sdk::LabelInfo, "LabelInfo.");
    check!(sdk::ApprovalInfo, "ApprovalInfo.");
    check!(sdk::RevisionInfo, "RevisionInfo.");
    check!(sdk::CommitInfo, "CommitInfo.");
    check!(sdk::GitPerson, "GitPerson.");
    check!(sdk::AttentionSetInfo, "AttentionSetInfo.");
}

#[test]
fn response_fixtures_carry_the_xssi_prefix() {
    let files: Vec<_> = names("")
        .into_iter()
        .filter(|n| n.ends_with(".response.json"))
        .collect();
    assert!(files.len() >= 6, "{files:?}");
    for f in files {
        assert!(read(&f).starts_with(")]}'\n"), "{f}");
    }
}

#[test]
fn server_version_and_account_self() {
    let (c, _) = client_with("server_version.response.json");
    assert_eq!(block_on(c.server_version()).unwrap(), "3.15.0-SNAPSHOT");
    let (c, _) = client_with("account_self.response.json");
    let me = block_on(c.account_self()).unwrap();
    assert_eq!(me.id, AccountId(1_000_001));
    assert_eq!(me.display(), "Jane Doe");
}

#[test]
fn query_response_fixture() {
    let (c, _) = client_with("query_changes.response.json");
    let page = block_on(c.query_changes(&ChangeQuery::new("status:open"))).unwrap();
    assert!(page.more_changes);
    let statuses: Vec<_> = page.changes.iter().map(|c| c.status.clone()).collect();
    assert_eq!(
        statuses,
        [
            ChangeStatus::New,
            ChangeStatus::Merged,
            ChangeStatus::Abandoned
        ]
    );
    let first = &page.changes[0];
    assert_eq!(first.number, ChangeNumber(1001));
    assert_eq!((first.insertions, first.deletions), (120, 7));
    assert!(first.work_in_progress && !first.is_private);
    assert_eq!(first.unresolved_comment_count, 2);
    assert_eq!(first.topic.as_deref(), Some("frob"));
    assert_eq!(first.hashtags, ["wip", "frob"]);
    assert_eq!(first.attention_set, [AccountId(1_000_002)]);
    assert!(first.labels["Code-Review"].recommended);
    assert!(first.labels["Verified"].rejected);
    assert_eq!(first.labels["Verified"].value, Some(-1));
    // Fractional seconds are optional on the wire.
    assert_eq!(page.changes[1].updated.to_string(), "2013-02-02T10:00:00Z");
    assert!(page.changes[1].labels["Code-Review"].approved);
    let last = &page.changes[2];
    assert!(last.is_private && last.labels.is_empty());
}

#[test]
fn empty_query_response_fixture() {
    let (c, _) = client_with("query_changes_empty.response.json");
    let page = block_on(c.query_changes(&ChangeQuery::new("nothing"))).unwrap();
    assert!(page.changes.is_empty() && !page.more_changes);
}

#[test]
fn unknown_enum_query_response_fixture() {
    let (c, _) = client_with("query_changes_unknown_enum.response.json");
    let page = block_on(c.query_changes(&ChangeQuery::new("x"))).unwrap();
    assert_eq!(
        page.changes[0].status,
        ChangeStatus::Unknown("ARCHIVED".into())
    );
}

#[test]
fn change_detail_fixtures() {
    let (c, _) = client_with("change_detail.response.json");
    let d = block_on(c.get_change(ChangeNumber(12345))).unwrap();
    assert_eq!(d.current_patch_set, Some(2));
    assert!(
        d.commit_message
            .as_deref()
            .is_some_and(|m| m.starts_with("Fix the frobnicator\n\n"))
    );
    assert_eq!(d.reviewers.len(), 1);
    assert_eq!(d.cc[0].email.as_deref(), Some("cc@example.com"));

    let (c, _) = client_with("change_detail_full.response.json");
    let d = block_on(c.get_change(ChangeNumber(12345))).unwrap();
    assert!(d.commit_message.is_some());
    assert!(!d.reviewers.is_empty() && !d.cc.is_empty());
}
