//! Conversion from [`crate::wire`] types to [`model`] types.

use std::collections::BTreeMap;

use model::{
    Account, AccountId, ChangeDetail, ChangeMessage, ChangeNumber, ChangeStatus, ChangeSummary,
    Commit, CommitParent, ExpressionResult, ExpressionStatus, GitPerson, LabelSummary, LabelVote,
    PatchSetNumber, Revision, RevisionId, RevisionKind, SubmitRequirement, SubmitRequirementStatus,
    Timestamp, TimestampParseError, parse_gerrit_timestamp,
};

use crate::wire::{
    self, AccountInfo, ApprovalInfo, ChangeInfo, ChangeMessageInfo, CommitInfo, LabelInfo,
    RevisionInfo, Status, SubmitRequirementExpressionInfo, SubmitRequirementResultInfo,
};

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

fn opt_timestamp(
    v: Option<&str>,
    field: &'static str,
) -> Result<Option<Timestamp>, ConversionError> {
    v.map(|v| {
        parse_gerrit_timestamp(v).map_err(|source| ConversionError::Timestamp { field, source })
    })
    .transpose()
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

fn git_person(p: &wire::GitPerson) -> Result<GitPerson, ConversionError> {
    Ok(GitPerson {
        name: p.name.clone(),
        email: p.email.clone(),
        date: opt_timestamp(p.date.as_deref(), "date")?,
        tz: p.tz,
    })
}

pub(crate) fn commit(c: &CommitInfo) -> Result<Commit, ConversionError> {
    Ok(Commit {
        sha: c.commit.clone().map(RevisionId::from),
        // A parent without a SHA cannot be followed; drop it.
        parents: c
            .parents
            .iter()
            .flatten()
            .filter_map(|p| {
                Some(CommitParent {
                    sha: RevisionId::from(p.commit.clone()?),
                    subject: p.subject.clone(),
                })
            })
            .collect(),
        author: c.author.as_ref().map(git_person).transpose()?,
        committer: c.committer.as_ref().map(git_person).transpose()?,
        subject: c.subject.clone().unwrap_or_default(),
        message: c.message.clone(),
    })
}

fn revision(sha: &str, r: &RevisionInfo) -> Result<Revision, ConversionError> {
    Ok(Revision {
        number: PatchSetNumber(required(r.number, "revisions._number")?),
        sha: RevisionId::from(sha),
        kind: r.kind.as_deref().map(RevisionKind::from_wire),
        created: opt_timestamp(r.created.as_deref(), "revisions.created")?,
        uploader: r.uploader.as_ref().map(account).transpose()?,
        ref_name: r.ref_name.clone(),
        description: r.description.clone().filter(|d| !d.is_empty()),
        commit: r.commit.as_ref().map(commit).transpose()?,
    })
}

fn message(m: &ChangeMessageInfo) -> Result<ChangeMessage, ConversionError> {
    Ok(ChangeMessage {
        id: required(m.id.clone(), "messages.id")?,
        author: m.author.as_ref().map(account).transpose()?,
        real_author: m.real_author.as_ref().map(account).transpose()?,
        date: timestamp(m.date.as_deref(), "messages.date")?,
        message: m.message.clone().unwrap_or_default(),
        tag: m.tag.clone(),
        revision: m.revision_number.map(PatchSetNumber),
    })
}

fn expression(e: &SubmitRequirementExpressionInfo) -> ExpressionResult {
    ExpressionResult {
        expression: e.expression.clone().unwrap_or_default(),
        fulfilled: e.fulfilled.unwrap_or(false),
        status: e.status.as_deref().map(ExpressionStatus::from_wire),
        passing_atoms: e.passing_atoms.clone().unwrap_or_default(),
        failing_atoms: e.failing_atoms.clone().unwrap_or_default(),
        atom_explanations: e.atom_explanations.clone().unwrap_or_default(),
        error_message: e.error_message.clone(),
    }
}

fn submit_requirement(
    r: &SubmitRequirementResultInfo,
) -> Result<SubmitRequirement, ConversionError> {
    Ok(SubmitRequirement {
        name: required(r.name.clone(), "submit_requirements.name")?,
        description: r.description.clone().filter(|d| !d.is_empty()),
        status: SubmitRequirementStatus::from_wire(required(
            r.status.as_deref(),
            "submit_requirements.status",
        )?),
        is_legacy: r.is_legacy.unwrap_or(false),
        applicability: r.applicability_expression_result.as_ref().map(expression),
        submittability: r.submittability_expression_result.as_ref().map(expression),
        override_result: r.override_expression_result.as_ref().map(expression),
    })
}

/// A vote needs a voter; entries without an id (which `DETAILED_LABELS`
/// does not produce) are skipped rather than failing the page.
fn vote(a: &ApprovalInfo) -> Result<Option<LabelVote>, ConversionError> {
    let Some(id) = a.account_id else {
        return Ok(None);
    };
    Ok(Some(LabelVote {
        account: Account {
            id: AccountId(id),
            name: a.name.clone(),
            email: a.email.clone(),
            username: a.username.clone(),
            display_name: a.display_name.clone(),
        },
        value: a.value.unwrap_or(0),
        date: opt_timestamp(a.date.as_deref(), "labels.all.date")?,
    }))
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
    let mut revisions = c
        .revisions
        .iter()
        .flatten()
        .map(|(sha, r)| revision(sha, r))
        .collect::<Result<Vec<_>, _>>()?;
    revisions.sort_by_key(|r| r.number);
    let mut votes = BTreeMap::new();
    for (name, l) in c.labels.iter().flatten() {
        if let Some(all) = &l.all {
            let list = all
                .iter()
                .map(vote)
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .flatten()
                .collect();
            votes.insert(name.clone(), list);
        }
    }
    Ok(ChangeDetail {
        summary,
        current_revision: c.current_revision.clone().map(RevisionId::from),
        current_patch_set: current_patch_set.map(PatchSetNumber),
        commit_message: rev
            .and_then(|r| r.commit.as_ref())
            .and_then(|commit| commit.message.clone()),
        reviewers: accounts("REVIEWER")?,
        cc: accounts("CC")?,
        revisions,
        messages: c
            .messages
            .iter()
            .flatten()
            .map(message)
            .collect::<Result<_, _>>()?,
        submit_requirements: c
            .submit_requirements
            .iter()
            .flatten()
            .map(submit_requirement)
            .collect::<Result<_, _>>()?,
        votes,
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
