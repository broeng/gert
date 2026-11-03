//! The checkouts as consolidation's [`PortSource`]: what ports the comments
//! of a local version that never reached Gerrit onto a Gerrit patch set.
//!
//! A [`CheckoutPorts`] is made from a server's [`Checkouts`] when a run
//! starts and goes with the run to the background executor, where `sync`
//! calls it (blocking: every call starts `git`). A checkout that was not
//! opened yet is opened there on first use, and its result kept for the
//! run, so a run never waits for the UI thread and opens nothing it does
//! not need.
//!
//! [`Checkouts`]: crate::checkouts::Checkouts

use std::{collections::HashMap, path::PathBuf, sync::Mutex};

use local_git::{Local, NotLocal, Repo};
use model::RevisionId;
use sync::{HasCheckout, NotLocalPath, PathMap, PortCommit, PortError, PortMaps, PortSource};

use crate::checkouts::{Checkout, CheckoutState};

/// A server's checkouts for one consolidation run. See the module comment.
pub struct CheckoutPorts {
    /// Project name and where its checkout is, in config order (the first
    /// entry of a name wins, as in `Checkouts`).
    projects: Vec<(String, Source)>,
    /// Checkouts opened during the run, by project; `None`: opening failed.
    opened: Mutex<HashMap<String, Option<Repo>>>,
}

enum Source {
    Open(Repo),
    /// Not opened yet: opened on first use.
    Path(PathBuf),
    /// Opening failed (the settings page says why).
    Failed,
}

impl CheckoutPorts {
    pub fn new(checkouts: Vec<Checkout>) -> Self {
        let projects = checkouts
            .into_iter()
            .map(|c| {
                let source = match c.state {
                    CheckoutState::Open(repo) => Source::Open(repo),
                    CheckoutState::Opening => Source::Path(c.path),
                    CheckoutState::Failed(_) => Source::Failed,
                };
                (c.project, source)
            })
            .collect();
        Self {
            projects,
            opened: Mutex::default(),
        }
    }

    /// The checkout of `project`, opened now if need be.
    fn repo(&self, project: &str) -> Result<Repo, PortError> {
        let source = self
            .projects
            .iter()
            .find(|(name, _)| name == project)
            .map(|(_, s)| s);
        let path = match source {
            Some(Source::Open(repo)) => return Ok(repo.clone()),
            Some(Source::Path(path)) => path,
            Some(Source::Failed) | None => return Err(PortError::NoCheckout),
        };
        // A panic in another call cannot leave the map half written.
        let mut opened = self.opened.lock().unwrap_or_else(|e| e.into_inner());
        let repo = opened
            .entry(project.to_owned())
            .or_insert_with(|| match Repo::open(path) {
                Ok(repo) => Some(repo),
                Err(e) => {
                    log::warn!("checkout of {project} at {}: {e}", path.display());
                    None
                }
            });
        repo.clone().ok_or(PortError::NoCheckout)
    }
}

impl PortSource for CheckoutPorts {
    fn has_checkout(&self, project: &str) -> HasCheckout {
        match self.projects.iter().find(|(name, _)| name == project) {
            None => HasCheckout::NotConfigured,
            Some((_, Source::Failed)) => HasCheckout::NotNow,
            Some((_, Source::Open(_))) => HasCheckout::Yes,
            Some((_, Source::Path(_))) => {
                let opened = self.opened.lock().unwrap_or_else(|e| e.into_inner());
                match opened.get(project) {
                    Some(None) => HasCheckout::NotNow,
                    _ => HasCheckout::Yes,
                }
            }
        }
    }

    fn commit(&self, project: &str, sha: &RevisionId) -> Result<Option<PortCommit>, PortError> {
        match self.repo(project)?.commit(sha) {
            Ok(Local::Available(c)) => Ok(Some(PortCommit { parents: c.parents })),
            Ok(Local::NotLocal(_)) => Ok(None),
            Err(e) => Err(PortError::Git(e.to_string())),
        }
    }

    fn line_maps(
        &self,
        project: &str,
        from: &RevisionId,
        to: &RevisionId,
        paths: &[String],
    ) -> Result<PortMaps, PortError> {
        let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
        match self.repo(project)?.line_maps(from, to, &paths) {
            Ok(Local::Available(maps)) => Ok(maps
                .into_iter()
                .map(|(path, map)| (path, path_map(map)))
                .collect()),
            Ok(Local::NotLocal(NotLocal::MissingCommit(sha))) => {
                Err(PortError::MissingCommit(sha.into()))
            }
            // `line_maps` names no other reason for a whole answer.
            Ok(Local::NotLocal(why)) => Err(PortError::Git(why.to_string())),
            Err(e) => Err(PortError::Git(e.to_string())),
        }
    }
}

fn path_map(map: Local<model::LineMap>) -> PathMap {
    match map {
        Local::Available(map) => PathMap::Lines(map),
        Local::NotLocal(NotLocal::NotAFile(_)) => PathMap::NotLocal(NotLocalPath::NotAFile),
        Local::NotLocal(NotLocal::MissingContent(_)) => {
            PathMap::NotLocal(NotLocalPath::MissingContent)
        }
        Local::NotLocal(why) => PathMap::NotLocal(NotLocalPath::Other(why.to_string())),
    }
}

#[cfg(test)]
mod tests;
