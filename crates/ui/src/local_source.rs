//! Diffs computed from the project's local git checkout.
//!
//! A [`LocalSource`] is one comparison (a change's target patch set against
//! its base) resolved in a checkout that has the commits, plus the path to
//! old path map of the file list in use. `DiffKey` carries no old path (the
//! server finds renames itself), so the map is how a rename is diffed
//! against the right file. It is cheap to clone (`Arc`s inside) and handed
//! to [`crate::diff_load::load_diff`] and to the syntax worker.
//!
//! Everything here that touches git blocks: call from the background
//! executor. Locally computed diffs are never written to the store.

use std::{collections::HashMap, path::Path, sync::Arc};

use gpui_kit::component::{ActiveTheme as _, tooltip::Tooltip};
use gpui_kit::*;
use local_git::{Comparison, Local, Repo};
use model::{Diff, DiffKey, FileEntry, FilesKey, RevisionId};

use crate::checkouts::{CheckoutState, Checkouts};

struct Core {
    repo: Repo,
    comparison: Comparison,
    target: RevisionId,
    base: Option<RevisionId>,
}

/// See the module comment.
#[derive(Clone)]
pub struct LocalSource {
    core: Arc<Core>,
    /// Path in the target to the path on the base side (`None`: not
    /// renamed or copied). `None` while the file list is not known; then
    /// nothing is served, since a rename cannot be told from an added file.
    renames: Option<Arc<HashMap<String, Option<String>>>>,
}

impl std::fmt::Debug for LocalSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalSource")
            .field("repo", &self.core.repo.path())
            .field("target", &self.core.target)
            .field("base", &self.core.base)
            .field("files", &self.renames.as_ref().map(|r| r.len()))
            .finish()
    }
}

impl LocalSource {
    /// Resolves `files` in `repo`. `None` when the checkout cannot serve
    /// the comparison (a commit is missing, a merge commit, patch sets with
    /// different parents) or git failed; both are logged, neither is an
    /// error for the page. Blocking.
    pub fn resolve(repo: Repo, files: &FilesKey) -> Option<Self> {
        let base = files.base.as_ref().map(|b| &b.sha);
        match repo.resolve(&files.target.sha, base) {
            Ok(Local::Available(comparison)) => Some(Self {
                core: Arc::new(Core {
                    repo,
                    comparison,
                    target: files.target.sha.clone(),
                    base: base.cloned(),
                }),
                renames: None,
            }),
            Ok(Local::NotLocal(why)) => {
                log::debug!(
                    "change {} is not served from {}: {why}",
                    files.change,
                    repo.path().display()
                );
                None
            }
            Err(e) => {
                log::warn!(
                    "resolving change {} in {}: {e}",
                    files.change,
                    repo.path().display()
                );
                None
            }
        }
    }

    /// The same comparison with the renames of `files`, the list in use.
    pub fn with_files(&self, files: &[FileEntry]) -> Self {
        let renames = files
            .iter()
            .map(|f| (f.path.clone(), f.old_path.clone()))
            .collect();
        Self {
            core: self.core.clone(),
            renames: Some(Arc::new(renames)),
        }
    }

    /// The checkout's directory.
    pub fn path(&self) -> &Path {
        self.core.repo.path()
    }

    /// Whether `key` is for this comparison and a file of the list in use.
    /// A path the list does not have is left to Gerrit rather than guessed
    /// at: it could be the new name of a rename we cannot see. (Nor is the
    /// diff of a file without a list tried, for the same reason.) The
    /// synthetic `/COMMIT_MSG` and `/MERGE_LIST` are Gerrit's.
    pub fn serves(&self, key: &DiffKey) -> bool {
        key.target.sha == self.core.target
            && key.base.as_ref().map(|b| &b.sha) == self.core.base.as_ref()
            && !key.path.starts_with('/')
            && self
                .renames
                .as_ref()
                .is_some_and(|r| r.contains_key(&key.path))
    }

    /// The diff of `key`, or `None` when it has to come from Gerrit (not
    /// served, git cannot answer like Gerrit would, or failed; the last is
    /// logged as a warning). Blocking.
    pub fn diff(&self, key: &DiffKey) -> Option<Diff> {
        if !self.serves(key) {
            return None;
        }
        let old_path = self
            .renames
            .as_ref()
            .and_then(|r| r.get(&key.path))
            .and_then(|o| o.as_deref());
        match self
            .core
            .repo
            .diff(&self.core.comparison, &key.path, old_path, &key.whitespace)
        {
            Ok(Local::Available(diff)) => Some(diff),
            Ok(Local::NotLocal(why)) => {
                log::debug!("{} is not diffed locally: {why}", key.path);
                None
            }
            Err(e) => {
                log::warn!("diffing {} in {}: {e}", key.path, self.path().display());
                None
            }
        }
    }
}

/// What a page knows about the local source of its current file list.
#[derive(Clone, Debug, Default)]
pub enum LocalState {
    /// Nothing asked yet (no file list).
    #[default]
    Unknown,
    /// Resolving in the background; diffs wait for it.
    Resolving(FilesKey),
    /// Resolved for this comparison: `None` means Gerrit serves it all.
    Ready(FilesKey, Option<LocalSource>),
}

impl LocalState {
    /// The source for `files`, if resolved for it.
    pub fn source_for(&self, files: &FilesKey) -> Option<&LocalSource> {
        match self {
            Self::Ready(k, Some(s)) if k == files => Some(s),
            _ => None,
        }
    }

    pub fn is_resolving(&self) -> bool {
        matches!(self, Self::Resolving(_))
    }

    /// Whether this state is (or is becoming) the answer for `files`.
    pub fn is_for(&self, files: &FilesKey) -> bool {
        match self {
            Self::Unknown => false,
            Self::Resolving(k) | Self::Ready(k, _) => k == files,
        }
    }
}

/// Whether a checkout is configured for `project` at all, so a page can
/// skip the whole business (and the wait for a task) when not.
pub fn configured(
    checkouts: &Option<Entity<Checkouts>>,
    project: &str,
    cx: &gpui_kit::App,
) -> bool {
    checkouts
        .as_ref()
        .is_some_and(|c| c.read(cx).state(project).is_some())
}

/// Opens (waiting for an open in flight) the checkout of `project` and
/// resolves `files` in it, on the background executor. `None` when there is
/// no usable checkout or it cannot serve the comparison.
pub async fn resolve_source(
    checkouts: Entity<Checkouts>,
    project: String,
    files: FilesKey,
    cx: &mut AsyncApp,
) -> Option<LocalSource> {
    let ck = Checkouts::opened(&checkouts, &project, cx).await?;
    let CheckoutState::Open(repo) = ck.state else {
        return None;
    };
    cx.background_spawn(async move { LocalSource::resolve(repo, &files) })
        .await
}

/// The quiet "local" marker for a diff computed from the checkout at
/// `path`. `id` must be unique on the page.
pub fn local_marker(id: impl Into<ElementId>, path: &Path, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    let tip = format!("Computed from the local checkout at {}", path.display());
    div()
        .id(id)
        .flex_none()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.muted_foreground)
        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
        .child("local")
}

#[cfg(test)]
pub(crate) mod tests;
