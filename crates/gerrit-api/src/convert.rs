//! Conversion from [`crate::wire`] types to [`model`] types.

use std::collections::BTreeMap;

use model::{
    Account, AccountId, ActionInfo, Actions, AddReviewerResult, AttentionEntry, ChangeDetail,
    ChangeMessage, ChangeNumber, ChangeStatus, ChangeSummary, ChangedChunk, Comment, CommentRange,
    Commit, CommitParent, ContextLine, Diff, DiffChangeType, DiffChunk, DiffContext, DiffFileMeta,
    DiffPreferences, DiffViewMode, DraftHandling, DraftInput, DraftReceipt, ExpressionResult,
    ExpressionStatus, FileEntry, FileStatus, GitPerson, IntralineEdit, IntralineStatus,
    LabelSummary, LabelValue, LabelVote, Location, PatchSetNumber, RelatedChange, ReviewInput,
    ReviewResult, Revision, RevisionId, RevisionKind, Side, SubmitRequirement,
    SubmitRequirementStatus, SuggestedGroup, SuggestedReviewer, Timestamp, TimestampParseError,
    Whitespace, file_order, parse_gerrit_timestamp,
};

use crate::wire::{
    self, AccountInfo, ApprovalInfo, ChangeInfo, ChangeMessageInfo, CommitInfo, LabelInfo,
    ReviewerResult, RevisionInfo, Status, SubmitRequirementExpressionInfo,
    SubmitRequirementResultInfo, SuggestedReviewerInfo,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConversionError {
    #[error("missing required field `{0}`")]
    MissingField(&'static str),
    /// A field is present but not shaped as documented.
    #[error("invalid field `{0}`")]
    Invalid(&'static str),
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

/// The reviewer autocomplete. An entry with neither an account nor a group
/// is skipped, and so is an account that cannot be converted (no id): one
/// odd suggestion must not fail the list.
pub(crate) fn suggested_reviewers(items: &[SuggestedReviewerInfo]) -> Vec<SuggestedReviewer> {
    items
        .iter()
        .filter_map(|s| {
            if let Some(a) = &s.account {
                return account(a).ok().map(SuggestedReviewer::Account);
            }
            let g = s.group.as_ref()?;
            Some(SuggestedReviewer::Group {
                group: SuggestedGroup {
                    id: g.id.clone()?,
                    name: g.name.clone(),
                },
                count: s.count,
                confirm: s.confirm.unwrap_or(false),
            })
        })
        .collect()
}

pub(crate) fn add_reviewer_result(
    r: &ReviewerResult,
) -> Result<AddReviewerResult, ConversionError> {
    let accounts = |v: &Option<Vec<AccountInfo>>| -> Result<Vec<Account>, ConversionError> {
        v.iter().flatten().map(account).collect()
    };
    Ok(AddReviewerResult {
        input: r.input.clone().unwrap_or_default(),
        error: r.error.clone().filter(|e| !e.trim().is_empty()),
        confirm: r.confirm.unwrap_or(false),
        reviewers: accounts(&r.reviewers)?,
        ccs: accounts(&r.ccs)?,
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

fn actions(m: Option<&BTreeMap<String, wire::ActionInfo>>) -> Actions {
    m.into_iter()
        .flatten()
        .map(|(id, a)| {
            (
                id.clone(),
                ActionInfo {
                    method: a.method.clone(),
                    label: a.label.clone(),
                    title: a.title.clone(),
                    enabled: a.enabled.unwrap_or(false),
                    enabled_options: a.enabled_options.clone().unwrap_or_default(),
                },
            )
        })
        .collect()
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

pub(crate) fn message(m: &ChangeMessageInfo) -> Result<ChangeMessage, ConversionError> {
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
    let mut attention = Vec::new();
    for (key, entry) in c.attention_set.iter().flatten() {
        // `DETAILED_ACCOUNTS` fills the account; without it the key is
        // still the id.
        let acct = match &entry.account {
            Some(a) if a.account_id.is_some() => account(a)?,
            _ => match key.parse::<u64>() {
                Ok(id) => Account {
                    id: AccountId(id),
                    name: None,
                    email: None,
                    username: None,
                    display_name: None,
                },
                Err(_) => continue,
            },
        };
        attention.push(AttentionEntry {
            account: acct,
            reason: entry.reason.clone().filter(|r| !r.is_empty()),
            last_update: opt_timestamp(entry.last_update.as_deref(), "attention_set.last_update")?,
        });
    }
    attention.sort_by_key(|a| a.account.id);
    let permitted_labels = c
        .permitted_labels
        .iter()
        .flatten()
        .map(|(name, values)| {
            let mut v: Vec<i32> = values.iter().filter_map(|s| label_value(s)).collect();
            v.sort_unstable();
            v.dedup();
            (name.clone(), v)
        })
        .collect();
    let label_values = c
        .labels
        .iter()
        .flatten()
        .filter_map(|(name, l)| {
            let values = l.values.as_ref()?;
            let mut v: Vec<LabelValue> = values
                .iter()
                .filter_map(|(k, d)| {
                    Some(LabelValue {
                        value: label_value(k)?,
                        description: d.clone(),
                    })
                })
                .collect();
            v.sort_by_key(|v| v.value);
            Some((name.clone(), v))
        })
        .collect();
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
        attention,
        permitted_labels,
        label_values,
        change_actions: actions(c.actions.as_ref()),
        revision_actions: actions(rev.and_then(|r| r.actions.as_ref())),
    })
}

/// Parses a label value as Gerrit writes it: `"-1"`, `" 0"`, `"+2"`.
fn label_value(s: &str) -> Option<i32> {
    s.trim().trim_start_matches('+').parse().ok()
}

/// The `files` map as a list in the server's display order.
pub(crate) fn file_list(files: BTreeMap<String, wire::CommonFileInfo>) -> Vec<FileEntry> {
    let mut list: Vec<_> = files
        .into_iter()
        .map(|(path, f)| FileEntry {
            path,
            status: FileStatus::from_wire(f.status.as_deref()),
            old_path: f.old_path,
            lines_inserted: f.lines_inserted.unwrap_or(0),
            lines_deleted: f.lines_deleted.unwrap_or(0),
            size_delta: f.size_delta.unwrap_or(0),
            size: f.size.unwrap_or(0),
            binary: f.binary.unwrap_or(false),
        })
        .collect();
    list.sort_by(|a, b| file_order(&a.path, &b.path));
    list
}

fn file_meta(m: &wire::FileMeta) -> Result<DiffFileMeta, ConversionError> {
    Ok(DiffFileMeta {
        name: required(m.name.clone(), "meta.name")?,
        content_type: m.content_type.clone(),
        lines: m.lines.unwrap_or(0),
    })
}

fn edits(
    pairs: &Option<Vec<Vec<u32>>>,
    field: &'static str,
) -> Result<Vec<IntralineEdit>, ConversionError> {
    pairs
        .iter()
        .flatten()
        .map(|p| match p.as_slice() {
            [skip, mark] => Ok(IntralineEdit {
                skip: *skip,
                mark: *mark,
            }),
            _ => Err(ConversionError::Invalid(field)),
        })
        .collect()
}

/// An entry is `ab` (common), else `skip`, else a changed region with
/// `a` and/or `b`. Gerrit never mixes them in one entry.
fn chunk(e: &wire::ContentEntry) -> Result<DiffChunk, ConversionError> {
    if let Some(lines) = &e.ab {
        return Ok(DiffChunk::Common {
            lines: lines.clone(),
        });
    }
    if let Some(lines) = e.skip {
        return Ok(DiffChunk::Skip { lines });
    }
    Ok(DiffChunk::Changed(ChangedChunk {
        a: e.a.clone().unwrap_or_default(),
        b: e.b.clone().unwrap_or_default(),
        edits_a: edits(&e.edit_a, "content.edit_a")?,
        edits_b: edits(&e.edit_b, "content.edit_b")?,
        due_to_rebase: e.due_to_rebase.unwrap_or(false),
        common: e.common.unwrap_or(false),
    }))
}

pub(crate) fn diff(d: &wire::DiffInfo) -> Result<Diff, ConversionError> {
    Ok(Diff {
        meta_a: d.meta_a.as_ref().map(file_meta).transpose()?,
        meta_b: d.meta_b.as_ref().map(file_meta).transpose()?,
        change_type: d.change_type.as_deref().map(DiffChangeType::from_wire),
        intraline_status: d
            .intraline_status
            .as_deref()
            .map(IntralineStatus::from_wire),
        diff_header: d.diff_header.clone().unwrap_or_default(),
        chunks: d
            .content
            .iter()
            .flatten()
            .map(chunk)
            .collect::<Result<_, _>>()?,
        binary: d.binary.unwrap_or(false),
    })
}

pub(crate) fn related(r: &wire::RelatedChangesInfo) -> Result<Vec<RelatedChange>, ConversionError> {
    r.changes
        .iter()
        .flatten()
        .map(|c| {
            Ok(RelatedChange {
                project: required(c.project.clone(), "related.project")?,
                change_id: c.change_id.clone(),
                number: ChangeNumber(required(c.change_number, "_change_number")?),
                patch_set: PatchSetNumber(required(c.revision_number, "_revision_number")?),
                current_patch_set: c.current_revision_number.map(PatchSetNumber),
                status: c.status.as_deref().map(ChangeStatus::from_wire),
                submittable: c.submittable,
                work_in_progress: c.work_in_progress.unwrap_or(false),
                commit: c.commit.as_ref().map(commit).transpose()?,
            })
        })
        .collect()
}

pub(crate) fn diff_preferences(
    p: &wire::DiffPreferencesInfo,
    general: Option<&wire::GeneralPreferencesInfo>,
) -> DiffPreferences {
    DiffPreferences {
        context: p.context.map(DiffContext::from_pref),
        ignore_whitespace: p.ignore_whitespace.as_deref().map(Whitespace::from_wire),
        line_length: p.line_length,
        tab_size: p.tab_size,
        show_tabs: p.show_tabs,
        intraline_difference: p.intraline_difference,
        line_wrapping: p.line_wrapping,
        show_whitespace_errors: p.show_whitespace_errors,
        hide_line_numbers: p.hide_line_numbers,
        syntax_highlighting: p.syntax_highlighting,
        expand_all_comments: p.expand_all_comments,
        view_mode: general
            .and_then(|g| g.diff_view.as_deref())
            .and_then(DiffViewMode::from_wire),
    }
}

fn range(r: &wire::Range) -> Result<CommentRange, ConversionError> {
    Ok(CommentRange {
        start_line: required(r.start_line, "range.start_line")?,
        start_character: required(r.start_character, "range.start_character")?,
        end_line: required(r.end_line, "range.end_line")?,
        end_character: required(r.end_character, "range.end_character")?,
    })
}

/// A comment or draft from a by-file list: `path` is the map key, used
/// when the comment does not carry one itself.
pub(crate) fn comment(
    c: &wire::CommentInfo,
    path: Option<&str>,
) -> Result<Comment, ConversionError> {
    let range = c.range.as_ref().map(range).transpose()?;
    Ok(Comment {
        id: required(c.id.clone(), "comment.id")?,
        path: required(
            c.path.clone().or_else(|| path.map(str::to_owned)),
            "comment.path",
        )?,
        patch_set: PatchSetNumber(required(c.patch_set, "comment.patch_set")?),
        side: Side::from_wire(c.side.as_deref()),
        parent: c.parent,
        location: Location::from_parts(c.line, range),
        in_reply_to: c.in_reply_to.clone().filter(|r| !r.is_empty()),
        message: c.message.clone().unwrap_or_default(),
        author: c.author.as_ref().map(account).transpose()?,
        updated: timestamp(c.updated.as_deref(), "comment.updated")?,
        unresolved: c.unresolved.unwrap_or(false),
        commit_id: c.commit_id.clone().map(RevisionId::from),
        tag: c.tag.clone(),
        change_message_id: c.change_message_id.clone(),
        robot_id: None,
        context: c
            .context_lines
            .iter()
            .flatten()
            .filter_map(|l| {
                Some(ContextLine {
                    line_number: l.line_number?,
                    text: l.context_line.clone().unwrap_or_default(),
                })
            })
            .collect(),
    })
}

pub(crate) fn robot_comment(
    c: &wire::RobotCommentInfo,
    path: Option<&str>,
) -> Result<Comment, ConversionError> {
    let mut out = comment(&c.comment, path)?;
    // A robot comment is recognised by its robot id; keep that even if the
    // server sent it empty.
    out.robot_id = Some(c.robot_id.clone().unwrap_or_default());
    Ok(out)
}

/// The by-file map of `/comments`, `/drafts` and `/robotcomments` as a flat
/// list: by path (special files first), then in server order within a path.
pub(crate) fn comments_by_path<T, F>(
    map: BTreeMap<String, Vec<T>>,
    convert: F,
) -> Result<Vec<Comment>, ConversionError>
where
    F: Fn(&T, Option<&str>) -> Result<Comment, ConversionError>,
{
    let mut paths: Vec<_> = map.keys().cloned().collect();
    paths.sort_by(|a, b| file_order(a, b));
    let mut out = Vec::new();
    for path in paths {
        for c in map.get(&path).into_iter().flatten() {
            out.push(convert(c, Some(&path))?);
        }
    }
    Ok(out)
}

/// The answer to creating or updating a draft: the server sends neither
/// the patch set nor an author, so only the id and time are taken.
pub(crate) fn draft_receipt(c: &wire::CommentInfo) -> Result<DraftReceipt, ConversionError> {
    Ok(DraftReceipt {
        id: required(c.id.clone(), "comment.id")?,
        updated: timestamp(c.updated.as_deref(), "comment.updated")?,
    })
}

fn wire_range(r: CommentRange) -> wire::Range {
    wire::Range {
        start_line: Some(r.start_line),
        start_character: Some(r.start_character),
        end_line: Some(r.end_line),
        end_character: Some(r.end_character),
    }
}

pub(crate) fn draft_input(d: &DraftInput) -> wire::DraftInput {
    // The server refuses a side on a patch-set level comment.
    let patch_set_level = d.path == model::PATCHSET_LEVEL_PATH;
    wire::DraftInput {
        path: Some(d.path.clone()),
        side: (!patch_set_level).then(|| d.side.as_wire().to_owned()),
        parent: if patch_set_level { None } else { d.parent },
        // The server wants `line` to equal the range's end line.
        line: d.location.line(),
        range: d.location.range().map(wire_range),
        in_reply_to: d.in_reply_to.clone(),
        message: Some(d.message.clone()),
        unresolved: Some(d.unresolved),
    }
}

pub(crate) fn review_input(r: &ReviewInput) -> wire::ReviewInput {
    let attention = |list: &[model::AttentionSetInput]| {
        (!list.is_empty()).then(|| {
            list.iter()
                .map(|a| wire::AttentionSetInput {
                    user: Some(a.user.to_string()),
                    reason: Some(a.reason.clone()),
                })
                .collect()
        })
    };
    wire::ReviewInput {
        message: r.message.clone().filter(|m| !m.trim().is_empty()),
        tag: r.tag.clone(),
        labels: (!r.labels.is_empty()).then(|| r.labels.clone()),
        drafts: Some(DraftHandling::as_wire(r.drafts).to_owned()),
        notify: r.notify.map(|n| n.as_wire().to_owned()),
        work_in_progress: r.work_in_progress,
        ready: r.ready,
        add_to_attention_set: attention(&r.add_to_attention_set),
        remove_from_attention_set: attention(&r.remove_from_attention_set),
    }
}

pub(crate) fn review_result(r: &wire::ReviewResult) -> ReviewResult {
    ReviewResult {
        labels: r.labels.clone().unwrap_or_default(),
        ready: r.ready,
        error: r.error.clone().filter(|e| !e.is_empty()),
    }
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
