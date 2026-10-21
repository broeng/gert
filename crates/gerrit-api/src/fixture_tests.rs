//! Tests against the schema-generated fixtures in `tests/fixtures/`
//! (regenerate with `cargo xtask gen-fixtures`).

use std::{fs, path::PathBuf, sync::Arc};

use futures::executor::block_on;
use model::{
    AccountId, ChangeNumber, ChangeStatus, DiffChangeType, DiffChunk, FileStatus, PatchSetNumber,
    RevisionId,
};

use crate::{Auth, ChangeQuery, DiffParams, GerritClient, convert, fake::FakeHttpClient, wire};

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
    client_with_auth(name, Auth::Anonymous)
}

fn client_with_auth(name: &str, auth: Auth) -> (GerritClient, FakeHttpClient) {
    let fake = FakeHttpClient::new();
    fake.push_response(200, read(name));
    let c = GerritClient::new(Arc::new(fake.clone()), "https://h", auth).unwrap();
    (c, fake)
}

fn basic() -> Auth {
    Auth::Basic {
        username: "jdoe".into(),
        password: "pw".into(),
    }
}

fn sha() -> RevisionId {
    RevisionId::from("0123456789abcdef0123456789abcdef01234567")
}

#[test]
fn every_change_info_fixture_converts() {
    let files = names("ChangeInfo.");
    assert!(files.len() >= 3, "{files:?}");
    for f in files {
        let w: wire::ChangeInfo =
            serde_json::from_str(&read(&f)).unwrap_or_else(|e| panic!("{f}: {e}"));
        let s = convert::change_summary(&w).unwrap_or_else(|e| panic!("{f}: {e}"));
        assert_eq!(s.number, ChangeNumber(12345), "{f}");
    }
}

#[test]
fn every_account_info_fixture_converts() {
    let files = names("AccountInfo.");
    assert!(files.len() >= 2, "{files:?}");
    for f in files {
        let a: wire::AccountInfo = serde_json::from_str(&read(&f)).unwrap();
        let a = convert::account(&a).unwrap_or_else(|e| panic!("{f}: {e}"));
        assert_eq!(a.id, AccountId(1_000_001), "{f}");
    }
}

#[test]
fn other_schema_fixtures_deserialize_into_wire_types() {
    macro_rules! check {
        ($ty:ty, $prefix:expr) => {{
            let files = names($prefix);
            assert_eq!(files.len(), 2, "{files:?}");
            for f in files {
                serde_json::from_str::<$ty>(&read(&f)).unwrap_or_else(|e| panic!("{f}: {e}"));
            }
        }};
    }
    check!(wire::LabelInfo, "LabelInfo.");
    check!(wire::ApprovalInfo, "ApprovalInfo.");
    check!(wire::RevisionInfo, "RevisionInfo.");
    check!(wire::CommitInfo, "CommitInfo.");
    check!(wire::GitPerson, "GitPerson.");
    check!(wire::AttentionSetInfo, "AttentionSetInfo.");
    check!(wire::FileMeta, "FileMeta.");
    check!(wire::ContentEntry, "ContentEntry.");
    check!(wire::DiffInfo, "DiffInfo.");
    check!(
        wire::RelatedChangeAndCommitInfo,
        "RelatedChangeAndCommitInfo."
    );
    check!(wire::RelatedChangesInfo, "RelatedChangesInfo.");
    check!(wire::ChangeMessageInfo, "ChangeMessageInfo.");
    check!(
        wire::SubmitRequirementExpressionInfo,
        "SubmitRequirementExpressionInfo."
    );
    check!(wire::DiffPreferencesInfo, "DiffPreferencesInfo.");
    check!(wire::GeneralPreferencesInfo, "GeneralPreferencesInfo.");
    check!(wire::RobotCommentInfo, "RobotCommentInfo.");
    check!(wire::Range, "Range.");
    check!(wire::ContextLineInfo, "ContextLineInfo.");
    check!(wire::ReviewResult, "ReviewResult.");
}

#[test]
fn schema_fixtures_with_unknown_enum_values_deserialize() {
    for f in names("CommonFileInfo.") {
        let info: wire::CommonFileInfo = serde_json::from_str(&read(&f)).unwrap();
        let files = convert::file_list([("x".to_owned(), info)].into());
        assert!(files[0].lines_inserted <= 1, "{f}");
    }
    let files = names("CommonFileInfo.");
    assert_eq!(files.len(), 3, "{files:?}");

    let sr: wire::SubmitRequirementResultInfo =
        serde_json::from_str(&read("SubmitRequirementResultInfo.unknown_enum.json")).unwrap();
    assert_eq!(sr.status.as_deref(), Some("PAUSED"));
    for f in names("SubmitRequirementResultInfo.") {
        serde_json::from_str::<wire::SubmitRequirementResultInfo>(&read(&f))
            .unwrap_or_else(|e| panic!("{f}: {e}"));
    }
}

#[test]
fn schema_generated_diff_info_converts() {
    for f in names("DiffInfo.") {
        let w: wire::DiffInfo = serde_json::from_str(&read(&f)).unwrap();
        let d = convert::diff(&w).unwrap_or_else(|e| panic!("{f}: {e}"));
        assert!(!d.chunks.is_empty(), "{f}");
    }
}

#[test]
fn schema_generated_messages_convert() {
    for f in names("ChangeMessageInfo.") {
        let w: wire::ChangeMessageInfo = serde_json::from_str(&read(&f)).unwrap();
        let m = convert::message(&w).unwrap_or_else(|e| panic!("{f}: {e}"));
        assert!(!m.id.is_empty() && !m.message.is_empty(), "{f}");
    }
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
    assert_eq!(d.current_patch_set, Some(PatchSetNumber(2)));
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
    // Every optional collection of the full schema converts.
    assert!(!d.revisions.is_empty() && !d.messages.is_empty());
    assert!(!d.submit_requirements.is_empty() && !d.votes.is_empty());
    assert!(d.current().is_some_and(|r| r.commit.is_some()));
}

#[test]
fn change_detail_minimal_fixture_has_the_current_revision() {
    let (c, _) = client_with("change_detail.response.json");
    let d = block_on(c.get_change(ChangeNumber(12345))).unwrap();
    assert!(d.messages.is_empty() && d.submit_requirements.is_empty());
    assert_eq!(d.revisions.len(), 1);
    let commit = d.current().and_then(|r| r.commit.as_ref()).unwrap();
    assert_eq!(commit.subject, "Fix the frobnicator");
    assert_eq!(commit.sha, Some(sha()));
}

#[test]
fn file_list_fixtures() {
    let (c, _) = client_with("files.response.json");
    let files = block_on(c.get_files(ChangeNumber(12345), &sha(), None)).unwrap();
    let paths: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/COMMIT_MSG",
            "assets/logo.png",
            "src/Gone.java",
            "src/Main.java",
            "src/New.java",
            "src/Weird.java",
            "src/renamed/Name.java"
        ]
    );
    let by = |p: &str| files.iter().find(|f| f.path == p).unwrap();
    assert_eq!(by("src/Main.java").status, FileStatus::Modified);
    assert_eq!(by("src/New.java").status, FileStatus::Added);
    assert_eq!(by("src/Gone.java").status, FileStatus::Deleted);
    assert_eq!(by("src/Weird.java").status, FileStatus::Unknown("Q".into()));
    assert!(by("assets/logo.png").binary);
    let renamed = by("src/renamed/Name.java");
    assert_eq!(renamed.status, FileStatus::Renamed);
    assert_eq!(renamed.old_path.as_deref(), Some("src/Old name.java"));

    let (c, _) = client_with("files_empty.response.json");
    assert!(
        block_on(c.get_files(ChangeNumber(1), &sha(), None))
            .unwrap()
            .is_empty()
    );

    let (c, _) = client_with_auth("files_reviewed.response.json", basic());
    let reviewed = block_on(c.get_reviewed_files(ChangeNumber(1), &sha())).unwrap();
    assert_eq!(reviewed, ["/COMMIT_MSG", "src/Main.java"]);
}

#[test]
fn diff_response_fixtures() {
    let (c, _) = client_with("diff.response.json");
    let d = block_on(c.get_diff(
        ChangeNumber(1),
        &sha(),
        "src/Main.java",
        &DiffParams::default(),
    ))
    .unwrap();
    assert_eq!(d.change_type, Some(DiffChangeType::Modified));
    assert_eq!(d.chunks.len(), 4);
    let DiffChunk::Changed(c) = &d.chunks[1] else {
        panic!("{:?}", d.chunks[1]);
    };
    assert_eq!(c.marks_a(), vec![vec![8..9]]);
    // "total" in both lines; the second mark starts after the line break.
    assert_eq!(c.marks_b(), vec![vec![8..13], vec![8..13]]);
    assert_eq!((d.a_len(), d.b_len()), (7, 8));
    assert_eq!(d.meta_a.as_ref().unwrap().lines, 7);

    let (c, _) = client_with("diff_unknown_enum.response.json");
    let d = block_on(c.get_diff(ChangeNumber(1), &sha(), "f", &DiffParams::default())).unwrap();
    assert_eq!(
        d.change_type,
        Some(DiffChangeType::Unknown("TELEPORTED".into()))
    );
}

#[test]
fn large_diff_fixture_is_consistent() {
    let (c, _) = client_with("large_diff.response.json");
    let d = block_on(c.get_diff(
        ChangeNumber(1),
        &sha(),
        "src/generated.rs",
        &DiffParams::default(),
    ))
    .unwrap();
    assert_eq!(d.a_len(), d.meta_a.as_ref().unwrap().lines);
    assert_eq!(d.b_len(), d.meta_b.as_ref().unwrap().lines);
    assert!(d.b_len() >= 10_000, "{}", d.b_len());

    let (mut common, mut changed, mut skips, mut rebase) = (0, 0, 0, 0);
    for chunk in &d.chunks {
        match chunk {
            DiffChunk::Common { .. } => common += 1,
            DiffChunk::Skip { .. } => skips += 1,
            DiffChunk::Changed(ch) => {
                changed += 1;
                rebase += usize::from(ch.due_to_rebase);
                // Every mark lies inside its line, on a char boundary.
                for (lines, marks) in [(&ch.a, ch.marks_a()), (&ch.b, ch.marks_b())] {
                    for (line, ranges) in lines.iter().zip(marks) {
                        for r in ranges {
                            assert!(line.get(r.clone()).is_some_and(|m| !m.is_empty()), "{line}");
                        }
                    }
                }
            }
        }
    }
    assert!(common > 100 && changed > 100 && skips > 50 && rebase > 10);
    // Pure insertions, pure deletions, and modified regions with edits.
    let kinds = |f: fn(&model::ChangedChunk) -> bool| {
        d.chunks
            .iter()
            .filter(|c| matches!(c, DiffChunk::Changed(ch) if f(ch)))
            .count()
    };
    assert!(kinds(|c| c.a.is_empty()) > 50);
    assert!(kinds(|c| c.b.is_empty()) > 50);
    assert!(kinds(|c| !c.edits_a.is_empty() && !c.edits_b.is_empty()) > 50);
}

#[test]
fn related_response_fixtures() {
    let (c, _) = client_with("related.response.json");
    let r = block_on(c.get_related(ChangeNumber(12345), &sha())).unwrap();
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].number, ChangeNumber(12346));
    assert!(r[0].is_outdated());
    assert_eq!(r[1].status, Some(ChangeStatus::Merged));
    assert!(r[1].commit.is_some());

    let (c, _) = client_with("related_empty.response.json");
    assert!(
        block_on(c.get_related(ChangeNumber(12345), &sha()))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn preferences_response_fixtures() {
    let fake = FakeHttpClient::new();
    fake.push_response(200, read("diff_preferences.response.json"));
    fake.push_response(200, read("general_preferences.response.json"));
    let c = GerritClient::new(Arc::new(fake), "https://h", basic()).unwrap();
    let p = block_on(c.get_diff_preferences()).unwrap();
    assert_eq!(p.context, Some(model::DiffContext::Lines(10)));
    assert_eq!(p.tab_size, Some(8));
    assert_eq!(p.ignore_whitespace, Some(model::Whitespace::IgnoreTrailing));
    assert_eq!(p.view_mode, Some(model::DiffViewMode::Unified));

    let fake = FakeHttpClient::new();
    fake.push_response(200, read("diff_preferences_unknown_enum.response.json"));
    fake.push_response(404, "");
    let c = GerritClient::new(Arc::new(fake), "https://h", basic()).unwrap();
    let p = block_on(c.get_diff_preferences()).unwrap();
    assert_eq!(p.context, Some(model::DiffContext::All));
    assert_eq!(
        p.ignore_whitespace,
        Some(model::Whitespace::Unknown("IGNORE_SOMETHING".into()))
    );
    assert_eq!(p.view_mode, None);
}

/// The fixtures in `tests/fixtures/handwritten/` are written by hand to
/// look like real server output (a rename, a binary file, ...). They are
/// not produced by `cargo xtask gen-fixtures`, which leaves them alone.
mod handwritten {
    use super::*;

    fn diff(name: &str) -> model::Diff {
        let text = read(&format!("handwritten/{name}.response.json"));
        assert!(text.starts_with(")]}'\n"), "{name}");
        let fake = FakeHttpClient::new();
        fake.push_response(200, text);
        let c = GerritClient::new(Arc::new(fake), "https://h", Auth::Anonymous).unwrap();
        block_on(c.get_diff(ChangeNumber(1), &sha(), "f", &DiffParams::default()))
            .unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    #[test]
    fn every_handwritten_fixture_converts_with_consistent_line_counts() {
        let dir = super::dir().join("handwritten");
        let mut seen = 0;
        for entry in fs::read_dir(dir).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            let name = name.strip_suffix(".response.json").unwrap().to_owned();
            let d = diff(&name);
            if !d.binary {
                let lines = |m: &Option<model::DiffFileMeta>| m.as_ref().map_or(0, |m| m.lines);
                assert_eq!(d.a_len(), lines(&d.meta_a), "{name}");
                assert_eq!(d.b_len(), lines(&d.meta_b), "{name}");
            }
            seen += 1;
        }
        assert_eq!(seen, 7);
    }

    #[test]
    fn rename() {
        let d = diff("diff_rename");
        assert_eq!(d.change_type, Some(DiffChangeType::Renamed));
        assert_ne!(d.meta_a.unwrap().name, d.meta_b.unwrap().name);
        assert!(d.diff_header.iter().any(|h| h.starts_with("rename from")));
    }

    #[test]
    fn binary() {
        let d = diff("diff_binary");
        assert!(d.binary && d.chunks.is_empty());
        assert!(
            d.meta_b
                .unwrap()
                .content_type
                .unwrap()
                .starts_with("image/")
        );
    }

    #[test]
    fn added_and_deleted() {
        let d = diff("diff_new_file");
        assert_eq!(d.change_type, Some(DiffChangeType::Added));
        assert!(d.meta_a.is_none() && d.meta_b.is_some());
        assert_eq!((d.a_len(), d.b_len()), (0, 3));
        let d = diff("diff_deleted_file");
        assert_eq!(d.change_type, Some(DiffChangeType::Deleted));
        assert!(d.meta_a.is_some() && d.meta_b.is_none());
        assert_eq!((d.a_len(), d.b_len()), (2, 0));
    }

    #[test]
    fn commit_message() {
        let d = diff("diff_commit_msg");
        assert_eq!(d.meta_a.unwrap().name, "/COMMIT_MSG");
        assert!(d.diff_header.is_empty());
        let DiffChunk::Changed(c) = &d.chunks[1] else {
            panic!();
        };
        assert!(c.edits_a.is_empty());
        assert_eq!(c.marks_b(), vec![vec![19..28]]);
    }

    #[test]
    fn whitespace_only() {
        let d = diff("diff_whitespace_only");
        let DiffChunk::Changed(c) = &d.chunks[1] else {
            panic!();
        };
        assert_eq!(c.marks_a(), vec![vec![0..1]]);
        assert_eq!(c.marks_b(), vec![vec![0..4]]);
        // With the whitespace ignored the lines are common.
        let d = diff("diff_whitespace_ignored");
        assert!(matches!(d.chunks.as_slice(), [DiffChunk::Common { lines }] if lines.len() == 3));
    }
}

#[test]
fn every_comment_info_fixture_converts() {
    let files = names("CommentInfo.");
    assert_eq!(files.len(), 3, "{files:?}");
    for f in files {
        let w: wire::CommentInfo =
            serde_json::from_str(&read(&f)).unwrap_or_else(|e| panic!("{f}: {e}"));
        let c = convert::comment(&w, Some("fallback/path")).unwrap_or_else(|e| panic!("{f}: {e}"));
        assert!(!c.id.is_empty(), "{f}");
    }
    let w: wire::CommentInfo =
        serde_json::from_str(&read("CommentInfo.unknown_enum.json")).unwrap();
    let c = convert::comment(&w, Some("p")).unwrap();
    assert_eq!(c.side, model::Side::Revision);
}

#[test]
fn robot_comment_fixtures_convert() {
    for f in names("RobotCommentInfo.") {
        let w: wire::RobotCommentInfo = serde_json::from_str(&read(&f)).unwrap();
        let c = convert::robot_comment(&w, Some("p")).unwrap_or_else(|e| panic!("{f}: {e}"));
        assert!(c.is_robot(), "{f}");
    }
}

#[test]
fn comments_response_builds_the_expected_threads() {
    use model::{Location, ThreadId, build_threads};
    let (c, fake) = client_with("comments.response.json");
    let comments = block_on(c.get_comments(ChangeNumber(12345))).unwrap();
    assert_eq!(comments.len(), 9);
    assert_eq!(fake.requests()[0].uri(), "https://h/changes/12345/comments");

    let threads = build_threads(&comments, &[]);
    let summary: Vec<_> = threads
        .iter()
        .map(|t| {
            (
                t.path.as_str(),
                t.patch_set.0,
                t.entries.len(),
                t.is_unresolved(),
                t.orphaned,
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("/COMMIT_MSG", 1, 1, true, false),
            ("/PATCHSET_LEVEL", 2, 1, false, false),
            // ps 1 line thread: ends with a resolving reply.
            ("src/Main.java", 1, 3, false, false),
            // ps 2: the PARENT comment (line 3), the range thread, the
            // orphan, then the file comment (file level sorts first).
            ("src/Main.java", 2, 1, false, false),
            ("src/Main.java", 2, 1, false, false),
            ("src/Main.java", 2, 1, true, false),
            ("src/Main.java", 2, 1, true, true),
        ]
    );
    let main2: Vec<_> = threads
        .iter()
        .filter(|t| t.path == "src/Main.java" && t.patch_set.0 == 2)
        .map(|t| (t.id.clone(), t.location))
        .collect();
    assert_eq!(main2[0], (ThreadId::Comment("fl01".into()), Location::File));
    assert_eq!(main2[1].0, ThreadId::Comment("pa01".into()));
    assert!(matches!(main2[2].1, Location::Range(r) if r.start_line == 20 && r.end_line == 21));
    assert_eq!(main2[3].0, ThreadId::Comment("or01".into()));
}

#[test]
fn drafts_response_joins_the_published_threads() {
    use model::{DiffPair, Draft, DraftSync, LocalDraftId, build_threads, comment_counts};
    let (c, _) = client_with("comments.response.json");
    let comments = block_on(c.get_comments(ChangeNumber(12345))).unwrap();
    let (c, fake) = client_with_auth("drafts.response.json", basic());
    let server_drafts = block_on(c.get_drafts(ChangeNumber(12345))).unwrap();
    assert_eq!(fake.requests()[0].uri(), "https://h/a/changes/12345/drafts");
    assert_eq!(server_drafts.len(), 2);

    let drafts: Vec<Draft> = server_drafts
        .iter()
        .zip(1..)
        .map(|(d, i)| Draft {
            local_id: LocalDraftId(i),
            server_id: Some(d.id.clone()),
            change: ChangeNumber(12345),
            patch_set: d.patch_set,
            revision: sha(),
            path: d.path.clone(),
            side: d.side,
            parent: d.parent,
            location: d.location,
            in_reply_to: d.in_reply_to.clone(),
            message: d.message.clone(),
            unresolved: d.unresolved,
            updated: d.updated,
            sync: DraftSync::Synced,
            attempts: 0,
            last_error: None,
        })
        .collect();
    let threads = build_threads(&comments, &drafts);
    // One new thread (dr02), and dr01 joined the ps 1 line thread.
    assert_eq!(threads.len(), 8);
    let line = threads
        .iter()
        .find(|t| t.id == model::ThreadId::Comment("ln01".into()))
        .unwrap();
    assert_eq!(line.entries.len(), 4);
    assert_eq!(line.reply_target(), Some("ln03"));
    assert!(line.has_draft() && !line.is_unresolved());

    // File list numbers for the diff of patch set 2 against its parent.
    let files = [model::FileEntry {
        path: "src/Main.java".into(),
        status: FileStatus::Modified,
        old_path: None,
        lines_inserted: 0,
        lines_deleted: 0,
        size_delta: 0,
        size: 0,
        binary: false,
    }];
    let n = comment_counts(
        &threads,
        DiffPair {
            base: None,
            target: PatchSetNumber(2),
        },
        &files,
    );
    let n = n["src/Main.java"];
    // ps 2: range, file, PARENT and orphan comments + dr02; ps 1 is elsewhere.
    assert_eq!((n.comments, n.drafts, n.unresolved), (4, 1, 3));
    assert_eq!(n.elsewhere, 4);
}

#[test]
fn robot_comments_response() {
    let (c, _) = client_with("robotcomments.response.json");
    let cs = block_on(c.get_robot_comments(ChangeNumber(1))).unwrap();
    assert_eq!(cs.len(), 1);
    assert_eq!(cs[0].robot_id.as_deref(), Some("lint"));
    assert_eq!(cs[0].path, "src/Main.java");
}

#[test]
fn review_result_fixtures() {
    let (c, _) = client_with_auth("review_result.response.json", basic());
    let r =
        block_on(c.post_review(ChangeNumber(1), &sha(), &model::ReviewInput::default())).unwrap();
    assert_eq!(r.labels["Code-Review"], 1);
    assert_eq!(r.ready, Some(true));
    assert!(r.error.is_none());
    let (c, _) = client_with_auth("review_result_error.response.json", basic());
    let r =
        block_on(c.post_review(ChangeNumber(1), &sha(), &model::ReviewInput::default())).unwrap();
    assert_eq!(r.error.as_deref(), Some("Not Found: unknown-reviewer"));
}
