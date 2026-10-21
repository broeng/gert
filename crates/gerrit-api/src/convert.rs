//! Conversion from [`crate::wire`] types to [`model`] types.

use std::collections::BTreeMap;

use model::{
    Account, AccountId, ChangeDetail, ChangeNumber, ChangeStatus, ChangeSummary, LabelSummary,
    Timestamp, TimestampParseError, parse_gerrit_timestamp,
};

use crate::wire::{AccountInfo, ChangeInfo, LabelInfo, Status};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConversionError {
    #[error("missing required field `{0}`")]
    MissingField(&'static str),
    #[error("field `{field}`: {source}")]
    Timestamp {
        field: &'static str,
        source: TimestampParseError,
    },
}

fn required<T>(v: Option<T>, field: &'static str) -> Result<T, ConversionError> {
    v.ok_or(ConversionError::MissingField(field))
}

fn timestamp(v: Option<&str>, field: &'static str) -> Result<Timestamp, ConversionError> {
    parse_gerrit_timestamp(required(v, field)?)
        .map_err(|source| ConversionError::Timestamp { field, source })
}

pub(crate) fn account(a: &AccountInfo) -> Result<Account, ConversionError> {
    Ok(Account {
        id: AccountId(required(a.account_id, "_account_id")?),
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

pub(crate) fn change_summary(c: &ChangeInfo) -> Result<ChangeSummary, ConversionError> {
    let number = required(c.number, "_number")?;
    let mut attention_set = Vec::new();
    for (key, entry) in c.attention_set.iter().flatten() {
        let from_key = key.parse::<u64>().ok();
        let from_account = entry.account.as_ref().and_then(|a| a.account_id);
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
        status: match required(c.status.as_ref(), "status")? {
            Status::New => ChangeStatus::New,
            Status::Merged => ChangeStatus::Merged,
            Status::Abandoned => ChangeStatus::Abandoned,
            Status::Unknown(s) => ChangeStatus::Unknown(s.clone()),
        },
        owner: account(required(c.owner.as_ref(), "owner")?)?,
        created: timestamp(c.created.as_deref(), "created")?,
        updated: timestamp(c.updated.as_deref(), "updated")?,
        insertions: c.insertions.unwrap_or(0),
        deletions: c.deletions.unwrap_or(0),
        labels: c
            .labels
            .iter()
            .flatten()
            .map(|(k, v)| (k.clone(), label(v)))
            .collect::<BTreeMap<_, _>>(),
        work_in_progress: c.work_in_progress.unwrap_or(false),
        is_private: c.is_private.unwrap_or(false),
        unresolved_comment_count: c.unresolved_comment_count.unwrap_or(0),
        total_comment_count: c.total_comment_count.unwrap_or(0),
        topic: c.topic.clone(),
        hashtags: c.hashtags.clone().unwrap_or_default(),
        attention_set,
        submittable: c.submittable,
    })
}

pub(crate) fn change_detail(c: &ChangeInfo) -> Result<ChangeDetail, ConversionError> {
    let summary = change_summary(c)?;
    let rev = c
        .current_revision
        .as_ref()
        .and_then(|sha| c.revisions.as_ref()?.get(sha));
    let current_patch_set = c
        .current_revision_number
        .or_else(|| rev.and_then(|r| r.number));
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
    fn account_without_id_is_rejected() {
        assert_eq!(
            account(&AccountInfo::default()),
            Err(ConversionError::MissingField("_account_id"))
        );
    }
}
