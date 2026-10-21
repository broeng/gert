//! Fixtures for unit tests.

use std::collections::BTreeMap;

use model::{
    Account, AccountId, ChangeDetail, ChangeNumber, ChangeStatus, ChangeSummary, FileEntry,
    FileStatus, PatchSetNumber, Revision,
};

pub fn account(id: u64, name: &str) -> Account {
    Account {
        id: AccountId(id),
        name: Some(name.to_owned()),
        email: None,
        username: None,
        display_name: None,
    }
}

pub fn summary(n: u64) -> ChangeSummary {
    let t = "2024-06-15T10:00:00Z".parse().expect("valid timestamp");
    ChangeSummary {
        number: ChangeNumber(n),
        change_id: None,
        project: "platform/foo".into(),
        branch: "main".into(),
        subject: format!("Change number {n}"),
        status: ChangeStatus::New,
        owner: account(1, "Alice Example"),
        created: t,
        updated: t,
        insertions: 10,
        deletions: 2,
        labels: BTreeMap::new(),
        work_in_progress: false,
        is_private: false,
        unresolved_comment_count: 0,
        total_comment_count: 0,
        topic: None,
        hashtags: Vec::new(),
        attention_set: Vec::new(),
        submittable: None,
    }
}

/// A change with `n` patch sets whose current one is `current`. The SHA of
/// patch set `i` is `sha{i}`.
pub fn detail(number: u64, n: u32, current: u32) -> ChangeDetail {
    ChangeDetail {
        summary: summary(number),
        current_revision: Some(format!("sha{current}").into()),
        current_patch_set: Some(PatchSetNumber(current)),
        commit_message: Some("Subject\n\nBody of the message".into()),
        reviewers: vec![account(2, "Bob Reviewer")],
        cc: vec![],
        revisions: (1..=n)
            .map(|i| Revision {
                number: PatchSetNumber(i),
                sha: format!("sha{i}").into(),
                kind: None,
                created: None,
                uploader: None,
                ref_name: None,
                description: None,
                commit: None,
            })
            .collect(),
        messages: vec![],
        submit_requirements: vec![],
        votes: Default::default(),
    }
}

pub fn file(path: &str, status: FileStatus) -> FileEntry {
    FileEntry {
        path: path.into(),
        status,
        old_path: None,
        lines_inserted: 3,
        lines_deleted: 1,
        size_delta: 10,
        size: 100,
        binary: false,
    }
}
