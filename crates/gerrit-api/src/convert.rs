//! Conversion from SDK models to [`model`] types.
//!
//! SDK types appear only in `pub(crate)` signatures here.

use std::collections::BTreeMap;

use gerrit_client::models::{AccountInfo, LabelInfo};
use model::{
    Account, AccountId, ChangeDetail, ChangeNumber, ChangeStatus, ChangeSummary, LabelSummary,
    Timestamp, TimestampParseError, parse_gerrit_timestamp,
};

use crate::wire::WireChange;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConversionError {
    #[error("missing required field `{0}`")]
    MissingField(&'static str),
    #[error("field `{field}` has out-of-range value {value}")]
    OutOfRange { field: &'static str, value: i64 },
    #[error("field `{field}`: {source}")]
    Timestamp {
        field: &'static str,
        source: TimestampParseError,
    },
}

fn required<T>(v: Option<T>, field: &'static str) -> Result<T, ConversionError> {
    v.ok_or(ConversionError::MissingField(field))
}

fn id(v: i32, field: &'static str) -> Result<u64, ConversionError> {
    u64::try_from(v).map_err(|_| ConversionError::OutOfRange {
        field,
        value: v.into(),
    })
}

/// Counts are never negative; clamp rather than fail a whole page.
fn count(v: Option<i32>) -> u32 {
    v.and_then(|v| u32::try_from(v).ok()).unwrap_or(0)
}

fn timestamp(v: Option<&str>, field: &'static str) -> Result<Timestamp, ConversionError> {
    parse_gerrit_timestamp(required(v, field)?)
        .map_err(|source| ConversionError::Timestamp { field, source })
}

pub(crate) fn account(a: &AccountInfo) -> Result<Account, ConversionError> {
    Ok(Account {
        id: AccountId(id(required(a._account_id, "_account_id")?, "_account_id")?),
        name: a.name.clone(),
        email: a.email.clone(),
        username: a.username.clone(),
        display_name: a.display_name.clone(),
    })
}

/// Summarises a label. Prefers the summary slots (`approved`, ...) that
/// `LABELS` yields; when only detailed votes (`all` + `values`) are present,
/// derives the slots from them.
fn label(l: &LabelInfo) -> LabelSummary {
    let mut s = LabelSummary {
        approved: l.approved.is_some(),
        rejected: l.rejected.is_some(),
        recommended: l.recommended.is_some(),
        disliked: l.disliked.is_some(),
        value: l.value,
    };
    let has_slots = s.approved || s.rejected || s.recommended || s.disliked;
    if let (false, Some(all), Some(values)) = (has_slots, &l.all, &l.values) {
        let range = values
            .keys()
            .filter_map(|k| k.trim().trim_start_matches('+').parse::<i32>().ok());
        let (min, max) = range.fold((0, 0), |(lo, hi), v| (lo.min(v), hi.max(v)));
        for v in all.iter().filter_map(|a| a.value).filter(|v| *v != 0) {
            match v {
                v if v == max && max > 0 => s.approved = true,
                v if v == min && min < 0 => s.rejected = true,
                v if v > 0 => s.recommended = true,
                _ => s.disliked = true,
            }
        }
    }
    s
}

pub(crate) fn change_summary(w: &WireChange) -> Result<ChangeSummary, ConversionError> {
    let c = &w.info;
    let number = id(required(c._number, "_number")?, "_number")?;
    let mut attention_set = Vec::new();
    for (key, entry) in c.attention_set.iter().flatten() {
        let from_key = key.parse::<u64>().ok();
        let from_account = entry
            .account
            .as_ref()
            .and_then(|a| a._account_id)
            .and_then(|v| u64::try_from(v).ok());
        attention_set.extend(from_key.or(from_account).map(AccountId));
    }
    attention_set.sort();
    attention_set.dedup();

    Ok(ChangeSummary {
        number: ChangeNumber(number),
        change_id: c.change_id.clone(),
        project: required(c.project.clone(), "project")?,
        branch: required(c.branch.clone(), "branch")?,
        subject: required(c.subject.clone(), "subject")?,
        status: ChangeStatus::from_wire(required(w.status.as_deref(), "status")?),
        owner: account(required(c.owner.as_deref(), "owner")?)?,
        created: timestamp(c.created.as_deref(), "created")?,
        updated: timestamp(c.updated.as_deref(), "updated")?,
        insertions: count(c.insertions),
        deletions: count(c.deletions),
        labels: c
            .labels
            .iter()
            .flatten()
            .map(|(k, v)| (k.clone(), label(v)))
            .collect::<BTreeMap<_, _>>(),
        work_in_progress: c.work_in_progress.unwrap_or(false),
        is_private: c.is_private.unwrap_or(false),
        unresolved_comment_count: count(c.unresolved_comment_count),
        total_comment_count: count(c.total_comment_count),
        topic: c.topic.clone(),
        hashtags: c.hashtags.clone().unwrap_or_default(),
        attention_set,
        submittable: c.submittable,
    })
}

pub(crate) fn change_detail(w: &WireChange) -> Result<ChangeDetail, ConversionError> {
    let c = &w.info;
    let summary = change_summary(w)?;
    let rev = c
        .current_revision
        .as_ref()
        .and_then(|sha| c.revisions.as_ref()?.get(sha));
    let current_patch_set = c
        .current_revision_number
        .or_else(|| rev.and_then(|r| r._number))
        .map(|v| id(v, "current_revision_number"))
        .transpose()?
        .and_then(|v| u32::try_from(v).ok());
    let accounts = |key: &str| -> Result<Vec<Account>, ConversionError> {
        c.reviewers
            .iter()
            .flat_map(|m| m.get(key))
            .flatten()
            .map(account)
            .collect()
    };
    Ok(ChangeDetail {
        summary,
        current_revision: c.current_revision.clone(),
        current_patch_set,
        commit_message: rev
            .and_then(|r| r.commit.as_ref())
            .and_then(|commit| commit.message.clone()),
        reviewers: accounts("REVIEWER")?,
        cc: accounts("CC")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detailed(votes: &[i32]) -> LabelInfo {
        serde_json::from_value(serde_json::json!({
            "values": {"-2": "", "-1": "", " 0": "", "+1": "", "+2": ""},
            "all": votes.iter().map(|v| serde_json::json!({"_account_id": 1, "value": v}))
                .collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    #[test]
    fn label_derived_from_detailed_votes() {
        assert!(label(&detailed(&[2])).approved);
        assert!(label(&detailed(&[-2])).rejected);
        assert!(label(&detailed(&[1])).recommended);
        assert!(label(&detailed(&[-1])).disliked);
        assert_eq!(label(&detailed(&[0])), LabelSummary::default());
    }

    #[test]
    fn negative_ids_are_rejected() {
        let a = AccountInfo {
            _account_id: Some(-1),
            ..Default::default()
        };
        assert!(matches!(
            account(&a),
            Err(ConversionError::OutOfRange { .. })
        ));
        assert_eq!(
            account(&AccountInfo::default()),
            Err(ConversionError::MissingField("_account_id"))
        );
    }
}
