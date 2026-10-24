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
        mergeable: None,
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
        attention: Default::default(),
        permitted_labels: Default::default(),
        removable_reviewers: Default::default(),
        label_values: Default::default(),
        change_actions: Default::default(),
        revision_actions: Default::default(),
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

/// A whole-file response like the server's: 80 lines with one changed
/// line in the middle and nothing folded.
pub fn whole_file() -> Vec<u8> {
    let lines =
        |from: u32, to: u32| -> Vec<String> { (from..=to).map(|n| format!("l{n}")).collect() };
    let body = serde_json::json!({
        "meta_a": {"name": "src/a.rs", "content_type": "text/x-rust", "lines": 80},
        "meta_b": {"name": "src/a.rs", "content_type": "text/x-rust", "lines": 80},
        "change_type": "MODIFIED",
        "content": [
            {"ab": lines(1, 40)},
            {"a": ["old"], "b": ["new"]},
            {"ab": lines(42, 80)},
        ],
    });
    format!(")]}}'\n{body}").into_bytes()
}

/// A [`Notifier`](crate::Notifier) that remembers what it was asked to show,
/// with the click callbacks.
#[derive(Default)]
pub struct RecordingNotifier {
    shown: std::sync::Mutex<Vec<(sync::notify::Notification, Option<crate::Clicked>)>>,
}

impl crate::Notifier for RecordingNotifier {
    fn show(&self, notification: &sync::notify::Notification, clicked: crate::Clicked) {
        self.shown
            .lock()
            .expect("lock")
            .push((notification.clone(), Some(clicked)));
    }
}

impl RecordingNotifier {
    pub fn shown(&self) -> Vec<sync::notify::Notification> {
        self.shown
            .lock()
            .expect("lock")
            .iter()
            .map(|(n, _)| n.clone())
            .collect()
    }

    pub fn titles(&self) -> Vec<String> {
        self.shown().into_iter().map(|n| n.title).collect()
    }

    /// Clicks the `i`th notification shown (once).
    pub fn click(&self, i: usize) {
        let clicked = self.shown.lock().expect("lock")[i].1.take();
        clicked.expect("not clicked yet")();
    }
}
