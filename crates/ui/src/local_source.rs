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

use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

use futures::{
    FutureExt as _,
    future::{Either, Shared},
};

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
    renames: Option<Arc<Renames>>,
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
    #[cfg(test)]
    pub fn with_files(&self, files: &[FileEntry]) -> Self {
        self.with_renames(renames_of(files))
    }

    /// The same comparison with these renames.
    pub fn with_renames(&self, renames: Arc<Renames>) -> Self {
        Self {
            core: self.core.clone(),
            renames: Some(renames),
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

/// The renames of a file list: path in the target to the path on the base
/// side (`None`: not renamed or copied).
pub type Renames = HashMap<String, Option<String>>;

/// The renames of `files`, shared by every diff of the list.
pub fn renames_of(files: &[FileEntry]) -> Arc<Renames> {
    Arc::new(
        files
            .iter()
            .map(|f| (f.path.clone(), f.old_path.clone()))
            .collect(),
    )
}

/// A local source that may still be resolving. Cloneable; all clones wait
/// for the same resolve. [`load_diff`](crate::diff_load::load_diff) awaits it
/// only after the store missed, so a stored diff never waits for git.
#[derive(Clone, Default)]
pub struct LocalLazy {
    base: Option<Base>,
    renames: Option<Arc<Renames>>,
}

#[derive(Clone)]
enum Base {
    Ready(LocalSource),
    Pending(SharedSource),
}

/// A resolve in flight: the comparison without renames.
pub type SharedSource = Shared<Task<Option<LocalSource>>>;

impl LocalLazy {
    /// Nothing local: Gerrit serves it all.
    pub fn none() -> Self {
        Self::default()
    }

    /// Already resolved (a source without renames, or `None`).
    pub fn ready(source: Option<LocalSource>, renames: Arc<Renames>) -> Self {
        Self {
            base: source.map(Base::Ready),
            renames: Some(renames),
        }
    }

    /// Resolving; the answer arrives with `renames` applied.
    pub fn pending(source: SharedSource, renames: Arc<Renames>) -> Self {
        Self {
            base: Some(Base::Pending(source)),
            renames: Some(renames),
        }
    }

    /// The source, waiting for the resolve if it is still running.
    pub async fn get(self) -> Option<LocalSource> {
        let source = match self.base? {
            Base::Ready(s) => s,
            Base::Pending(p) => p.await?,
        };
        Some(match self.renames {
            Some(r) => source.with_renames(r),
            None => source,
        })
    }
}

/// What a page knows about the local source of its current file list.
#[derive(Clone, Default)]
pub enum LocalState {
    /// Nothing asked yet (no file list).
    #[default]
    Unknown,
    /// Resolving in the background.
    Resolving(FilesKey, SharedSource),
    /// Resolved for this comparison: `None` means Gerrit serves it all.
    Ready(FilesKey, Option<LocalSource>),
}

impl LocalState {
    /// The resolved source (without renames) for `files`.
    pub fn source_for(&self, files: &FilesKey) -> Option<&LocalSource> {
        match self {
            Self::Ready(k, Some(s)) if k == files => Some(s),
            _ => None,
        }
    }

    /// Whether this state is (or is becoming) the answer for `files`.
    pub fn is_for(&self, files: &FilesKey) -> bool {
        match self {
            Self::Unknown => false,
            Self::Resolving(k, _) | Self::Ready(k, _) => k == files,
        }
    }

    /// For [`load_diff`](crate::diff_load::load_diff) and the syntax worker.
    pub fn lazy(&self, files: &FilesKey, renames: &Arc<Renames>) -> LocalLazy {
        match self {
            Self::Resolving(k, p) if k == files => LocalLazy::pending(p.clone(), renames.clone()),
            Self::Ready(k, s) if k == files => LocalLazy::ready(s.clone(), renames.clone()),
            _ => LocalLazy::none(),
        }
    }
}

/// How long opening and resolving may take before the checkout is given up
/// on for this load (a hung git: stale NFS, a locked repository).
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

/// The answer to "what about the local source of `files`".
pub enum Lookup {
    /// No checkout is configured for the project.
    Unconfigured,
    /// Known from an earlier look (positive answers only).
    Ready(LocalSource),
    /// Being worked out in the background.
    Pending(SharedSource),
}

/// Looks the comparison up in the checkouts' memo, else starts resolving it
/// on the background executor. Never blocks.
pub fn lookup(
    checkouts: &Option<Entity<Checkouts>>,
    project: &str,
    files: &FilesKey,
    cx: &mut App,
) -> Lookup {
    let Some(checkouts) = checkouts else {
        return Lookup::Unconfigured;
    };
    let (configured, memo) = {
        let c = checkouts.read(cx);
        (c.state(project).is_some(), c.memoized(project, files))
    };
    if !configured {
        return Lookup::Unconfigured;
    }
    if let Some(source) = memo {
        return Lookup::Ready(source);
    }
    let (checkouts, project, files) = (checkouts.clone(), project.to_owned(), files.clone());
    let task = cx.spawn(async move |cx| {
        let source = resolve_source(&checkouts, &project, &files, cx).await;
        if let Some(s) = &source {
            checkouts.update(cx, |c, _| c.memoize(&project, &files, s.clone()));
        }
        source
    });
    Lookup::Pending(task.shared())
}

/// Opens (waiting for an open in flight) the checkout of `project` and
/// resolves `files` in it. `None` when there is no usable checkout, it
/// cannot serve the comparison, or this takes longer than
/// [`RESOLVE_TIMEOUT`] (logged).
async fn resolve_source(
    checkouts: &Entity<Checkouts>,
    project: &str,
    files: &FilesKey,
    cx: &mut AsyncApp,
) -> Option<LocalSource> {
    let timer = cx.background_executor().timer(RESOLVE_TIMEOUT);
    let work = async {
        let ck = Checkouts::opened(checkouts, project, cx).await?;
        let CheckoutState::Open(repo) = ck.state else {
            return None;
        };
        let files = files.clone();
        cx.background_spawn(async move { LocalSource::resolve(repo, &files) })
            .await
    };
    match futures::future::select(std::pin::pin!(work), std::pin::pin!(timer)).await {
        Either::Left((source, _)) => source,
        Either::Right(_) => {
            log::warn!(
                "the local checkout of {project} did not answer in {RESOLVE_TIMEOUT:?}; \
                 asking Gerrit"
            );
            None
        }
    }
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
