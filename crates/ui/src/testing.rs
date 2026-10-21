//! Fixtures for unit tests.

use std::collections::BTreeMap;

use model::{Account, AccountId, ChangeNumber, ChangeStatus, ChangeSummary};

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
