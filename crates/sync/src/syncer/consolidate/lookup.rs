//! Finding local changes on Gerrit: the batched lookup, and what happens
//! to the changes it found (known ones, merged or abandoned ones).

use super::*;

impl Syncer {
    /// Looks `rows` up on Gerrit, per project in batches. A refused query
    /// skips its batch (recorded); a lost connection ends the run.
    pub(super) async fn look_up(
        &self,
        rows: &[LocalChange],
        run: &mut Run<'_>,
    ) -> Result<Vec<(LocalChange, Lookup)>, Error> {
        let mut by_project: BTreeMap<&str, Vec<&LocalChange>> = BTreeMap::new();
        let mut out = Vec::new();
        for row in rows {
            if !is_change_id(&row.change_id) {
                out.push((row.clone(), Lookup::Malformed));
            } else if row.project.contains('"') {
                out.push((row.clone(), Lookup::Unknown));
            } else {
                by_project.entry(&row.project).or_default().push(row);
            }
        }
        for (project, rows) in by_project {
            for batch in rows.chunks(BATCH) {
                let terms: Vec<String> = batch.iter().map(|r| lookup_term(r)).collect();
                let q = format!("({}) project:\"{project}\"", terms.join(" OR "));
                let page = match self
                    .client
                    .query_changes(&ChangeQuery::new(q).limit(LOOKUP_LIMIT))
                    .await
                {
                    Ok(page) => page,
                    Err(e) => {
                        let why = skip_or_stop(e.into())?;
                        run.report
                            .errors
                            .extend(batch.iter().map(|r| (r.id, why.clone())));
                        continue;
                    }
                };
                for row in batch {
                    let found = classify_hits(row, &page.changes, page.more_changes);
                    out.push(((*row).clone(), found));
                }
            }
        }
        Ok(out)
    }

    /// What `row` has that consolidation could act on.
    pub(super) fn row_data(&self, row: &LocalChange) -> Result<RowData, Error> {
        let store = self.store();
        let versions = store.local_patch_sets(self.server, row.id)?;
        let numbers: BTreeSet<PatchSetNumber> = versions.iter().map(|v| v.number).collect();
        let mut on_versions = store.load_drafts(self.server, row.number)?;
        if let Some(g) = row.gerrit_number {
            on_versions.extend(store.load_drafts(self.server, g)?);
        }
        on_versions.retain(|d| numbers.contains(&d.patch_set) && d.reserved_by.is_none());
        Ok(RowData {
            reviews: store.pending_reviews_of_local_change(self.server, row.id)?,
            reserved: store.consolidation_drafts(self.server, row.id)?,
            versions,
            on_versions,
        })
    }

    /// Handles the local changes that have a Gerrit change: asks about
    /// those due in batches, then moves and sends where there is work.
    pub(super) async fn consolidate_known(
        &self,
        rows: Vec<LocalChange>,
        run: &mut Run<'_>,
    ) -> Result<(), Error> {
        let now = Timestamp::now();
        let slow_before = now
            .checked_sub(self.consolidation_timing.slow_recheck)
            .unwrap_or(now);
        let mut due: Vec<(LocalChange, ChangeNumber, RowData)> = Vec::new();
        for row in rows {
            let Some(number) = row.gerrit_number else {
                continue;
            };
            let data = self.row_data(&row)?;
            let urgent = data.sendable() && row.state != LocalChangeState::Closed;
            let checked = self.store().local_change_checked_at(self.server, row.id)?;
            if urgent || checked.is_none_or(|t| t < slow_before) {
                due.push((row, number, data));
            }
        }
        for batch in due.chunks(BATCH) {
            let numbers: BTreeSet<ChangeNumber> = batch.iter().map(|(_, n, _)| *n).collect();
            let q = numbers
                .iter()
                .map(|n| format!("change:{n}"))
                .collect::<Vec<_>>()
                .join(" OR ");
            let mut query = ChangeQuery::new(q).limit(u32::try_from(numbers.len()).unwrap_or(1));
            query.options = vec![ChangeOption::AllRevisions];
            let changes: HashMap<ChangeNumber, ChangeDetail> =
                match self.client.query_change_details(&query).await {
                    Ok((list, _)) => list.into_iter().map(|d| (d.summary.number, d)).collect(),
                    Err(e) => {
                        let why = skip_or_stop(e.into())?;
                        for (row, ..) in batch {
                            run.report.errors.push((row.id, why.clone()));
                        }
                        continue;
                    }
                };
            let asked: Vec<LocalChangeId> = batch.iter().map(|(r, ..)| r.id).collect();
            self.store()
                .mark_local_changes_checked(self.server, &asked, Timestamp::now())?;
            for (row, number, data) in batch {
                let Some(lite) = changes.get(number) else {
                    run.report.errors.push((
                        row.id,
                        format!("change {number} is not on Gerrit (any more)"),
                    ));
                    continue;
                };
                let outcome = self
                    .consolidate_known_one(row, *number, data, lite, run)
                    .await;
                self.settle_problem(row, *number, outcome, run).await?;
            }
        }
        Ok(())
    }

    /// One known change, given what the batch query said about it.
    async fn consolidate_known_one(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        data: &RowData,
        lite: &ChangeDetail,
        run: &mut Run<'_>,
    ) -> Result<(), Problem> {
        if lite.summary.status != ChangeStatus::New {
            // Every time: whatever was written since is held too.
            return self.hold_closed(row, number, lite, run);
        }
        if row.state == LocalChangeState::Closed {
            // Restored: the Gerrit state is open again, but what was held
            // stays held (a user decision).
            self.store()
                .set_local_change_state(self.server, row.id, LocalChangeState::Local)?;
            run.report.reopened.push((row.id, number));
        } else if row.state == LocalChangeState::Checking {
            // Asked about: Gerrit has the change.
            self.store()
                .set_local_change_state(self.server, row.id, LocalChangeState::Local)?;
        }
        let shas: Vec<&model::RevisionId> = lite.revisions.iter().map(|r| &r.sha).collect();
        // A version pushed since: the detail maps it, once.
        let newly_pushed = data
            .versions
            .iter()
            .any(|v| v.gerrit_patch_set.is_none() && shas.contains(&&v.sha));
        let port_work = run
            .options
            .port
            .as_ref()
            .is_some_and(|p| p.has_checkout(&row.project) != HasCheckout::NotNow)
            && !self.port_backing_off(row.id)
            && !data
                .to_port(
                    |sha| shas.contains(&sha),
                    row.state == LocalChangeState::Vanished,
                )
                .is_empty();
        if newly_pushed || data.exact_work(&shas) || data.moved_sendable() || port_work {
            let detail = self.refresh_change(number).await?;
            if detail.summary.status != ChangeStatus::New {
                return self.hold_closed(row, number, &detail, run);
            }
            self.consolidate_open(row, number, detail, run).await
        } else {
            self.finish_change(row, number, run)
        }
    }

    /// A change found by the lookup, with its fresh detail.
    pub(super) async fn consolidate_found(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        detail: ChangeDetail,
        run: &mut Run<'_>,
    ) -> Result<(), Problem> {
        run.report.found.push(found(row, number));
        if detail.summary.status != ChangeStatus::New {
            return self.hold_closed(row, number, &detail, run);
        }
        self.consolidate_open(row, number, detail, run).await
    }

    /// Holds everything of a change found merged or abandoned, its
    /// approvals that may not move as drop-only (judged from `detail`).
    pub(super) fn hold_closed(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        detail: &ChangeDetail,
        run: &mut Run<'_>,
    ) -> Result<(), Problem> {
        let reason = match &detail.summary.status {
            ChangeStatus::Merged => "change merged".to_owned(),
            ChangeStatus::Abandoned => "change abandoned".to_owned(),
            other => format!("change {}", other.to_string().to_lowercase()),
        };
        let held = self
            .store()
            .hold_closed_local_change(self.server, row.id, number, &reason);
        match held {
            Ok(Some((reviews, drafts))) => {
                if row.state != LocalChangeState::Closed || reviews + drafts > 0 {
                    run.report.closed.push((row.id, number));
                }
                self.mark_drop_only(row, number, detail, run.options)?;
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(e @ (store::Error::LocalTarget | store::Error::OtherGerritChange(_))) => {
                Err(Problem::Skip(e.to_string()))
            }
            Err(e) => Err(e.into()),
        }
    }
}

/// A Change-Id Gerrit can be asked about without matching anything else:
/// `I` and 40 hex digits (a shorter one would be a prefix search, and
/// anything else makes Gerrit refuse the whole query: "Invalid change
/// format").
fn is_change_id(id: &str) -> bool {
    id.len() == 41
        && id.starts_with('I')
        && id[1..]
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Percent-encodes a part of a `project~branch~Change-Id` triplet: Gerrit
/// URL-decodes each part, so a `%` or `+` must be encoded to survive (and
/// a `~` or `"` cannot break the term).
fn triplet_part(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The lookup term of a row: `change:"project~branch~Id"` with a known
/// branch, else `change:Id`.
fn lookup_term(row: &LocalChange) -> String {
    match &row.branch {
        Some(branch) => format!(
            "change:\"{}~{}~{}\"",
            triplet_part(&row.project),
            triplet_part(branch),
            row.change_id
        ),
        None => format!("change:{}", row.change_id),
    }
}

/// What the lookup's answer says about `row`. A row with a known branch
/// takes only hits on that branch.
fn classify_hits(row: &LocalChange, hits: &[ChangeSummary], more: bool) -> Lookup {
    let matching: Vec<&ChangeSummary> = hits
        .iter()
        .filter(|c| {
            c.project == row.project
                && c.change_id.as_deref() == Some(row.change_id.as_str())
                && row.branch.as_ref().is_none_or(|b| *b == c.branch)
        })
        .collect();
    match (matching.as_slice(), more) {
        // The answer may be cut off: "none" or "one" might be wrong.
        (_, true) if matching.len() < 2 => Lookup::Unknown,
        ([], false) => Lookup::NotFound,
        ([one], false) => Lookup::Found(one.number),
        _ => Lookup::Ambiguous,
    }
}

fn found(row: &LocalChange, gerrit: ChangeNumber) -> FoundChange {
    FoundChange {
        local_change: row.id,
        local_number: row.number,
        gerrit,
    }
}
