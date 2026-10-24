//! Notification events from two states of one change.
//!
//! Pure functions, no I/O: the caller (see [`crate::Watcher`]) has the old
//! cached detail and the freshly fetched one. A change that was never
//! cached has no old state and must not be diffed (that is the baseline,
//! it notifies nothing).
//!
//! Own actions are not filtered here: the actor is recorded and the rule
//! matcher decides.

use std::collections::{BTreeSet, HashSet};

use model::{
    Account, AccountId, COMMENT_SNIPPET_CHARS, ChangeDetail, ChangeMessage, ChangeStatus, Event,
    EventKind, ServerId, Timestamp,
};

/// The events that turned `old` into `new`, oldest first.
pub fn diff_change(server: ServerId, old: &ChangeDetail, new: &ChangeDetail) -> Vec<Event> {
    let mut out = Vec::new();
    let mut emit = |actor: Option<&Account>,
                    at: Timestamp,
                    kind: EventKind,
                    subject_account: Option<AccountId>| {
        out.push(Event {
            server,
            change: new.summary.number,
            project: new.summary.project.clone(),
            subject: new.summary.subject.clone(),
            owner: Some(new.summary.owner.clone()),
            actor: actor.cloned(),
            at,
            kind,
            subject_account,
        });
    };
    let updated = new.summary.updated;

    votes(old, new, &mut emit);
    comments(old, new, &mut emit);

    let old_attention: HashSet<_> = old.attention.iter().map(|a| a.account.id).collect();
    for entry in &new.attention {
        if !old_attention.contains(&entry.account.id) {
            let at = entry.last_update.unwrap_or(updated);
            emit(None, at, EventKind::AttentionAdded, Some(entry.account.id));
        }
    }

    let old_reviewers: HashSet<_> = old.reviewers.iter().map(|a| a.id).collect();
    let old_cc: HashSet<_> = old.cc.iter().map(|a| a.id).collect();
    for a in &new.reviewers {
        if !old_reviewers.contains(&a.id) {
            emit(
                None,
                updated,
                EventKind::ReviewerAdded { cc: false },
                Some(a.id),
            );
        }
    }
    for a in &new.cc {
        // Moving from CC to reviewer is not new to the change; the other
        // way round is not "added" either.
        if !old_cc.contains(&a.id) && !old_reviewers.contains(&a.id) {
            emit(
                None,
                updated,
                EventKind::ReviewerAdded { cc: true },
                Some(a.id),
            );
        }
    }

    // A cache from before `revisions` was stored has only the number.
    let old_max = old
        .revisions
        .iter()
        .map(|r| r.number)
        .chain(old.current_patch_set)
        .max();
    if let Some(old_max) = old_max {
        for r in new.revisions.iter().filter(|r| r.number > old_max) {
            let at = r.created.unwrap_or(updated);
            let kind = EventKind::NewPatchSet { number: r.number };
            emit(r.uploader.as_ref(), at, kind, None);
        }
    }

    if old.summary.status == ChangeStatus::New {
        let (kind, tag) = match new.summary.status {
            ChangeStatus::Merged => (Some(EventKind::Merged), "merged"),
            ChangeStatus::Abandoned => (Some(EventKind::Abandoned), "abandon"),
            _ => (None, ""),
        };
        if let Some(kind) = kind {
            // Gerrit's own log entry names who did it, when there is one.
            let by = new_messages(old, new)
                .find(|m| m.tag.as_deref().is_some_and(|t| t.contains(tag)))
                .and_then(|m| m.author.as_ref());
            emit(by, updated, kind, None);
        }
        // A closed change is neither submittable nor in conflict any more.
        if new.summary.status == ChangeStatus::New {
            if old.summary.submittable == Some(false) && new.summary.submittable == Some(true) {
                emit(None, updated, EventKind::ReadyToSubmit, None);
            }
            if old.summary.mergeable == Some(true) && new.summary.mergeable == Some(false) {
                emit(None, updated, EventKind::MergeConflict, None);
            }
        }
    }

    out.sort_by_key(|e| e.at);
    out
}

/// Votes that were added, changed or removed. A missing vote and a vote of
/// 0 are the same thing to Gerrit; `previous` is `Some` only if the account
/// had an entry.
fn votes(
    old: &ChangeDetail,
    new: &ChangeDetail,
    emit: &mut impl FnMut(Option<&Account>, Timestamp, EventKind, Option<AccountId>),
) {
    let labels: BTreeSet<_> = old.votes.keys().chain(new.votes.keys()).collect();
    for label in labels {
        let old_votes = old.votes.get(label).map(Vec::as_slice).unwrap_or_default();
        let new_votes = new.votes.get(label).map(Vec::as_slice).unwrap_or_default();
        for v in new_votes {
            let prev = old_votes.iter().find(|o| o.account.id == v.account.id);
            if prev.map_or(0, |p| p.value) != v.value {
                let kind = EventKind::Vote {
                    label: label.clone(),
                    value: v.value,
                    previous: prev.map(|p| p.value),
                };
                emit(
                    Some(&v.account),
                    v.date.unwrap_or(new.summary.updated),
                    kind,
                    None,
                );
            }
        }
        for p in old_votes {
            let still = new_votes.iter().any(|v| v.account.id == p.account.id);
            if !still && p.value != 0 {
                let kind = EventKind::Vote {
                    label: label.clone(),
                    value: 0,
                    previous: Some(p.value),
                };
                emit(Some(&p.account), new.summary.updated, kind, None);
            }
        }
    }
}

/// Messages of `new` whose id `old` does not have. With no old messages at
/// all (a cache from before they were stored; every real change has at
/// least its upload message) nothing counts as new.
fn new_messages<'a>(
    old: &'a ChangeDetail,
    new: &'a ChangeDetail,
) -> impl Iterator<Item = &'a ChangeMessage> {
    let seen: HashSet<&str> = old.messages.iter().map(|m| m.id.as_str()).collect();
    let ignore = old.messages.is_empty();
    new.messages
        .iter()
        .filter(move |m| !ignore && !seen.contains(m.id.as_str()))
}

/// New change messages that say something. Gerrit's own entries (upload,
/// merge, abandon, ...) are skipped, as other events cover them; bots that
/// tag their messages `autogenerated:<bot>` are kept.
fn comments(
    old: &ChangeDetail,
    new: &ChangeDetail,
    emit: &mut impl FnMut(Option<&Account>, Timestamp, EventKind, Option<AccountId>),
) {
    for m in new_messages(old, new) {
        if m.tag
            .as_deref()
            .is_some_and(|t| t.starts_with("autogenerated:gerrit:"))
        {
            continue;
        }
        let text = body(&m.message);
        if text.is_empty() {
            continue;
        }
        let kind = EventKind::Comment {
            message_id: m.id.clone(),
            snippet: text.chars().take(COMMENT_SNIPPET_CHARS).collect(),
        };
        emit(m.author.as_ref(), m.date, kind, None);
    }
}

/// A change message without its `Patch Set N: Code-Review+1` header line,
/// trimmed. A vote with no text leaves nothing, the vote event covers it.
fn body(message: &str) -> &str {
    let message = message.trim();
    match message.strip_prefix("Patch Set ") {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => {
            rest.split_once('\n').map_or("", |(_, tail)| tail).trim()
        }
        _ => message,
    }
}

#[cfg(test)]
mod tests;
