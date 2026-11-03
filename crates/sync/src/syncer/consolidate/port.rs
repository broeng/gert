//! Porting: where each draft of a local version that never reached Gerrit
//! goes on a Gerrit patch set, or why it is held.
//!
//! The rules (a user decision, and the plan's "Porting"):
//!
//! - A comment ports only where its lines are unchanged: an unchanged line
//!   maps 1:1 (shifted by what was inserted or deleted before it), a range
//!   ports when every line it touches is unchanged and nothing was inserted
//!   inside. Renames are followed.
//! - A changed or deleted line, a range crossing a change, a deleted file
//!   or a binary file that changed is held.
//! - A comment on a file as a whole ports while the file is there. One on
//!   a file the local version deletes ports when the target deletes it too.
//! - `/PATCHSET_LEVEL` always ports; `/COMMIT_MSG` maps through the two
//!   commits' synthetic texts, as Gerrit builds them.
//! - A comment on the parent side ports only when that side is unchanged:
//!   the two parents have the same content for the file, and neither
//!   commit is a merge (Gerrit compares a merge with an auto-merge).
//! - A reply stays on its thread's patch set and is never carried over.
//! - Gerrit takes a comment only on a file that its patch set changes (or a
//!   magic file such as `/COMMIT_MSG`) and refuses the whole post for any
//!   other, so a comment whose file the target does not change is held.
//! - What the checkout cannot read (a commit removed by `git gc`, a file
//!   whose content a partial clone lacks) is held, with the reason.

use model::{
    COMMIT_MSG_PATH, Draft, FileEntry, FileStatus, Location, PATCHSET_LEVEL_PATH, PatchSetNumber,
    Side,
};
use store::DraftAnchor;

use super::*;

use crate::port::{NotLocalPath, PathMap, PortMaps};

/// The checkout's answer for the drafts of one side of a port: the maps, or
/// why the drafts on that side are held.
pub(super) type SideMaps = Result<PortMaps, String>;

/// Where draft `d` goes on patch set `ps`, whose files (against its parent,
/// as Gerrit lists them) are `files`, given the maps of the revision side
/// (`revision`) and of the parent side (`parent`); else why it is held.
pub(super) fn port_draft(
    d: &Draft,
    ps: PatchSetNumber,
    files: &[FileEntry],
    revision: &SideMaps,
    parent: &SideMaps,
) -> Result<DraftAnchor, String> {
    let at = |path: &str, location: Location| DraftAnchor {
        path: path.to_owned(),
        side: d.side,
        parent: d.parent,
        location,
    };
    if d.in_reply_to.is_some() {
        // Replies stay on their thread's patch set and are uploaded as
        // ordinary drafts; one here would answer a thread elsewhere.
        return Err(
            "A reply belongs to its thread's patch set and is not carried over.".to_owned(),
        );
    }
    if d.path == PATCHSET_LEVEL_PATH {
        return Ok(at(&d.path, d.location));
    }
    let ps = ps.0;
    match d.side {
        Side::Parent => {
            if let Some(p) = d.parent.filter(|p| *p > 1) {
                return Err(format!(
                    "Comments on parent {p} of a merge commit are not carried over."
                ));
            }
            if d.path == COMMIT_MSG_PATH {
                return Err(
                    "Comments on the base of the commit message are not carried over.".to_owned(),
                );
            }
            let maps = parent.as_ref().map_err(Clone::clone)?;
            let same = match maps.get(&d.path) {
                Some(PathMap::Lines(m)) => {
                    m.is_identity() && m.target_path() == Some(d.path.as_str())
                }
                Some(PathMap::NotLocal(NotLocalPath::NotAFile)) => {
                    return Err(format!(
                        "The base side of {} in the local version is another file (renamed or \
                         added there), so a comment on it is not carried over.",
                        d.path
                    ));
                }
                Some(PathMap::NotLocal(why)) => return Err(not_local(&d.path, why)),
                None => false,
            };
            if !same {
                return Err(format!(
                    "The base of {} differs in PS {ps}, and a comment on the base side is \
                     carried over only when it is the same.",
                    d.path
                ));
            }
            // The base side of the file in the target: the file must be in
            // its list, under the same name (not renamed or copied there).
            match files.iter().find(|f| f.path == d.path) {
                Some(f) if f.old_path.is_none() => Ok(at(&d.path, d.location)),
                Some(_) => Err(format!(
                    "{} is renamed in PS {ps}, so its base side is another file.",
                    d.path
                )),
                None => Err(unchanged(&d.path, ps)),
            }
        }
        Side::Revision => {
            let maps = revision.as_ref().map_err(Clone::clone)?;
            match maps.get(&d.path) {
                Some(PathMap::Lines(m)) => {
                    let Some(target) = m.target_path() else {
                        return Err(format!("{} is deleted in PS {ps}.", d.path));
                    };
                    let Some(location) = m.map_location(d.location) else {
                        return Err(if m.is_opaque() {
                            format!("{} is a binary file that changed in PS {ps}.", d.path)
                        } else {
                            match d.location {
                                Location::Line(l) => {
                                    format!("Line {l} of {} changed in PS {ps}.", d.path)
                                }
                                _ => {
                                    format!("The commented lines of {} changed in PS {ps}.", d.path)
                                }
                            }
                        });
                    };
                    if target == COMMIT_MSG_PATH {
                        return Ok(at(target, location));
                    }
                    match files.iter().find(|f| f.path == target) {
                        Some(f) if f.status != FileStatus::Deleted => Ok(at(target, location)),
                        Some(_) => Err(format!("{target} is deleted in PS {ps}.")),
                        None => Err(unchanged(target, ps)),
                    }
                }
                Some(PathMap::NotLocal(NotLocalPath::NotAFile)) => {
                    // Deleted by the local version: a comment on the file as
                    // a whole goes where the target deletes it too.
                    let deleted_there = files
                        .iter()
                        .any(|f| f.path == d.path && f.status == FileStatus::Deleted);
                    if d.location == Location::File && deleted_there {
                        Ok(at(&d.path, d.location))
                    } else if d.location == Location::File {
                        Err(format!(
                            "{} is not a file in the local version, and PS {ps} does not \
                             delete it.",
                            d.path
                        ))
                    } else {
                        Err(format!("{} is not a file in the local version.", d.path))
                    }
                }
                Some(PathMap::NotLocal(why)) => Err(not_local(&d.path, why)),
                None => Err(format!(
                    "The checkout did not say where {} went in PS {ps}.",
                    d.path
                )),
            }
        }
    }
}

/// Why a comment on `path` is held when the checkout cannot map it.
fn not_local(path: &str, why: &NotLocalPath) -> String {
    match why {
        NotLocalPath::NotAFile => format!("{path} is not a file in the local version."),
        NotLocalPath::MissingContent => {
            format!("The content of {path} is not in the checkout (a partial clone?).")
        }
        NotLocalPath::Other(text) => format!("{}.", capitalized(text.trim_end_matches('.'))),
    }
}

/// Why a comment on `path` is held when patch set `ps` does not change the
/// file: Gerrit would refuse the whole post.
fn unchanged(path: &str, ps: u32) -> String {
    format!("PS {ps} does not change {path}, and Gerrit takes comments only on changed files.")
}

fn capitalized(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

impl Syncer {
    /// Ports the versions of `row` that never reached Gerrit
    /// ([`RowData::to_port`]) onto one patch set of `detail`: the current
    /// one if its commit is in the checkout, else the newest one that is.
    /// Each draft goes where [`port_draft`] says or is held with the
    /// reason; the pending reviews go along (their votes are decided when
    /// they are posted, see [`casts`]). With no patch set in the checkout,
    /// or no checkout configured for the project, everything is held.
    /// Without a port source those versions stay as they are; with a
    /// checkout that cannot be used now, or `git` failing, they stay too,
    /// and the change is not tried again before the slow re-check.
    pub(super) async fn port_plan(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        detail: &ChangeDetail,
        data: &RowData,
        run: &mut Run<'_>,
    ) -> Result<PortPlan, Problem> {
        let mut plan = PortPlan::default();
        let on_gerrit = |sha: &RevisionId| {
            detail
                .revisions
                .iter()
                .any(|r| &r.sha == sha && !r.number.is_local())
        };
        let versions = data.to_port(on_gerrit, row.state == LocalChangeState::Vanished);
        if versions.is_empty() {
            return Ok(plan);
        }
        let Some(source) = run.options.port.clone() else {
            return Ok(plan);
        };
        match source.has_checkout(&row.project) {
            HasCheckout::Yes => {}
            HasCheckout::NotNow => {
                self.port_failed(row.id);
                return Ok(plan);
            }
            HasCheckout::NotConfigured => {
                let why = format!(
                    "No checkout is configured for project {}, so this could not be carried \
                     over from the local version.",
                    row.project
                );
                for v in versions {
                    plan.hold_version(row, detail, data, v, &why, run.options);
                }
                return Ok(plan);
            }
        }
        let (rev, commit) = match port_target(&*source, &row.project, detail) {
            Ok(Some(target)) => target,
            Ok(None) => {
                let why = format!(
                    "No patch set of change {number} is in the checkout, so this could not be \
                     carried over from the local version."
                );
                for v in versions {
                    plan.hold_version(row, detail, data, v, &why, run.options);
                }
                return Ok(plan);
            }
            Err(e) => {
                self.port_failed(row.id);
                if e != PortError::NoCheckout {
                    run.report
                        .errors
                        .push((row.id, format!("porting the local versions: {e}")));
                }
                return Ok(plan);
            }
        };
        // Gerrit takes comments only on the files its patch set changes;
        // only comments on files need the list.
        let on_files = versions.iter().any(|v| {
            data.on_versions
                .iter()
                .any(|d| d.patch_set == v.number && movable(d) && !d.path.starts_with('/'))
        });
        let files = if on_files {
            match self.client.get_files(number, &rev.sha, None).await {
                Ok(files) => files,
                Err(e) => {
                    let why = skip_or_stop(e.into())?;
                    self.port_failed(row.id);
                    run.report.errors.push((row.id, why));
                    return Ok(plan);
                }
            }
        } else {
            Vec::new()
        };
        let mut failed = false;
        for v in versions {
            match port_version(&*source, row, v, rev, &commit, &files, data) {
                Ok((m, keep)) => {
                    plan.report.push(Ported {
                        local_change: row.id,
                        change: number,
                        from: v.number,
                        to: Some(rev.number),
                        drafts: m.drafts.len(),
                        reviews: m.reviews.len(),
                        held: keep.clone(),
                        held_reviews: 0,
                    });
                    plan.moves.push(m);
                    plan.keep.extend(keep);
                }
                Err(e) => {
                    failed = true;
                    if e != PortError::NoCheckout {
                        run.report
                            .errors
                            .push((row.id, format!("porting PS {}: {e}", v.number.0)));
                    }
                }
            }
        }
        if failed {
            self.port_failed(row.id);
        } else {
            self.port_worked(row.id);
        }
        Ok(plan)
    }

    /// Porting `change` failed in a way that may pass (a checkout that
    /// could not be opened, `git` failing): it is tried again after the
    /// slow re-check, not on every run.
    fn port_failed(&self, change: LocalChangeId) {
        self.port_failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(change, Timestamp::now());
    }

    fn port_worked(&self, change: LocalChangeId) {
        self.port_failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&change);
    }

    /// Whether porting `change` failed less than the slow re-check ago.
    pub(super) fn port_backing_off(&self, change: LocalChangeId) -> bool {
        let now = Timestamp::now();
        let since = now
            .checked_sub(self.consolidation_timing.slow_recheck)
            .unwrap_or(now);
        self.port_failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&change)
            .is_some_and(|t| *t > since)
    }
}

/// What porting decided for one change, for its rewrite.
#[derive(Default)]
pub(super) struct PortPlan {
    pub(super) moves: Vec<PatchSetMove>,
    pub(super) keep: Vec<(LocalDraftId, String)>,
    pub(super) review_states: Vec<ReviewStateChange>,
    pub(super) report: Vec<Ported>,
}

impl PortPlan {
    /// Holds everything of version `v` that would have been ported, with
    /// `why`: its drafts, and its pending reviews with their votes
    /// (approvals drop-only where the rules say so).
    fn hold_version(
        &mut self,
        row: &LocalChange,
        detail: &ChangeDetail,
        data: &RowData,
        v: &LocalPatchSet,
        why: &str,
        options: &ConsolidateOptions,
    ) {
        let held: Vec<(LocalDraftId, String)> = data
            .on_versions
            .iter()
            .filter(|d| d.patch_set == v.number && movable(d))
            .map(|d| (d.local_id, why.to_owned()))
            .collect();
        let mut reviews = 0;
        for r in data
            .reviews
            .iter()
            .filter(|r| r.patch_set == v.number && r.state == PendingReviewState::Pending)
        {
            let votes: Votes = r.labels.iter().map(|(l, v)| (l.clone(), *v)).collect();
            let drop_only = drop_only_labels(row, detail, Some(r), &votes, options);
            let mut change = ReviewStateChange::new(
                r.id,
                &[PendingReviewState::Pending],
                PendingReviewState::Held,
                Some(format!("{why}{}", vote_actions(&votes, &drop_only))),
            );
            change.drop_only = drop_only;
            self.review_states.push(change);
            reviews += 1;
        }
        self.report.push(Ported {
            local_change: row.id,
            change: detail.summary.number,
            from: v.number,
            to: None,
            drafts: 0,
            reviews: 0,
            held: held.clone(),
            held_reviews: reviews,
        });
        self.keep.extend(held);
    }
}

/// The patch set to port onto: the current one of `detail` if its commit
/// is in the checkout, else the newest one that is; with the commit as the
/// checkout has it. `None` when no patch set is in the checkout.
fn port_target<'a>(
    source: &dyn PortSource,
    project: &str,
    detail: &'a ChangeDetail,
) -> Result<Option<(&'a Revision, PortCommit)>, PortError> {
    let mut revisions: Vec<&Revision> = detail
        .revisions
        .iter()
        .filter(|r| !r.number.is_local())
        .collect();
    revisions.sort_by_key(|r| std::cmp::Reverse(r.number));
    if let Some(at) = revisions
        .iter()
        .position(|r| Some(r.number) == detail.current_patch_set)
    {
        let current = revisions.remove(at);
        revisions.insert(0, current);
    }
    for rev in revisions {
        if let Some(commit) = source.commit(project, &rev.sha)? {
            return Ok(Some((rev, commit)));
        }
    }
    Ok(None)
}

/// The move of version `v`'s drafts and pending reviews onto `rev` (whose
/// commit is `commit` and whose files Gerrit lists as `files`), and the
/// drafts held with the reason. One `line_maps` call for the revision side
/// and one for the parent side, as needed. `Err` leaves the version as it
/// is (no checkout, `git` failing); a commit missing from the checkout
/// holds what it covers.
fn port_version(
    source: &dyn PortSource,
    row: &LocalChange,
    v: &LocalPatchSet,
    rev: &Revision,
    commit: &PortCommit,
    files: &[FileEntry],
    data: &RowData,
) -> Result<(PatchSetMove, Vec<(LocalDraftId, String)>), PortError> {
    let drafts: Vec<&Draft> = data
        .on_versions
        .iter()
        .filter(|d| d.patch_set == v.number && movable(d))
        .collect();
    // The paths the checkout is asked about: not those `port_draft` holds
    // or carries over without a map.
    let paths = |side: Side| -> Vec<String> {
        let set: BTreeSet<&str> = drafts
            .iter()
            .filter(|d| d.side == side && d.in_reply_to.is_none())
            .filter(|d| d.path != model::PATCHSET_LEVEL_PATH)
            .filter(|d| {
                side == Side::Revision
                    || (d.path != model::COMMIT_MSG_PATH && d.parent.is_none_or(|p| p <= 1))
            })
            .map(|d| d.path.as_str())
            .collect();
        set.into_iter().map(str::to_owned).collect()
    };
    let (revision_paths, parent_paths) = (paths(Side::Revision), paths(Side::Parent));
    let revision = if revision_paths.is_empty() {
        Ok(PortMaps::new())
    } else {
        side_maps(source.line_maps(&row.project, &v.sha, &rev.sha, &revision_paths))?
    };
    let parent = if parent_paths.is_empty() {
        Ok(PortMaps::new())
    } else {
        match source.commit(&row.project, &v.sha)? {
            None => Err(format!(
                "The local version cannot be compared with the patch set: commit {} is not \
                 in the checkout.",
                v.sha.short(12)
            )),
            Some(from) if from.is_merge() || commit.is_merge() => Err(
                "Comments on the base side of a merge commit are not carried over: Gerrit \
                 compares a merge with an auto-merge."
                    .to_owned(),
            ),
            Some(from) => match (from.parent(), commit.parent()) {
                // The very same base: every file is the same there.
                (Some(a), Some(b)) if a == b => Ok(parent_paths
                    .iter()
                    .map(|p| {
                        let same = model::LineMap::identity(p.clone(), u32::MAX);
                        (p.clone(), crate::PathMap::Lines(same))
                    })
                    .collect()),
                (Some(a), Some(b)) => {
                    side_maps(source.line_maps(&row.project, a, b, &parent_paths))?
                }
                _ => Err("A root commit has no base side to carry a comment over to.".to_owned()),
            },
        }
    };
    let mut moved = Vec::new();
    let mut keep = Vec::new();
    for d in drafts {
        match port_draft(d, rev.number, files, &revision, &parent) {
            Ok(anchor) => moved.push((d.local_id, Some(anchor))),
            Err(why) => keep.push((d.local_id, why)),
        }
    }
    let reviews = data
        .reviews
        .iter()
        .filter(|r| r.patch_set == v.number && r.state == PendingReviewState::Pending)
        .map(|r| r.id)
        .collect();
    Ok((
        PatchSetMove {
            from: v.number,
            to: rev.as_ref(),
            exact: false,
            drafts: moved,
            reviews,
        },
        keep,
    ))
}

/// One side's answer of the checkout: the maps, or (a commit missing from
/// the checkout) why the drafts on that side are held. Other errors leave
/// the version as it is.
fn side_maps(answer: Result<PortMaps, PortError>) -> Result<SideMaps, PortError> {
    match answer {
        Ok(maps) => Ok(Ok(maps)),
        Err(e @ PortError::MissingCommit(_)) => Ok(Err(format!(
            "The local version cannot be compared with the patch set: {e}."
        ))),
        Err(e) => Err(e),
    }
}
