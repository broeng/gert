//! The local git checkouts configured for one server (`[[servers.projects]]`)
//! and the `local_git::Repo`s opened from them.
//!
//! One [`Checkouts`] entity per server, kept on the server's registry entry
//! like the syntax cache, so two servers that both have a project called
//! `tools` never share a checkout. It is an entity (not plain data) so that
//! opening can run as a task of its own and the pages that show the result
//! (the settings page's project list, later the change and diff pages)
//! observe it and redraw when an open finishes.
//!
//! **Opening is lazy.** `Repo::open` starts a `git` process, and most
//! configured projects are never looked at in a given run, so nothing is
//! opened when the setup is loaded. The first [`Checkouts::get`] for a
//! project starts the open on the background executor and answers
//! [`CheckoutState::Opening`] at once (it never blocks); the entity notifies
//! when the result is in, and every later `get` answers it from memory. A
//! page that wants the status of every project (the settings tab) calls
//! [`Checkouts::open_all`]. [`Checkouts::state`] reads without starting
//! anything.
//!
//! After a settings save [`Checkouts::set_projects`] swaps the list: an entry
//! with the same name and the same path keeps its result (or its open in
//! flight); a changed path is opened again, at once if the old entry had been
//! opened, else lazily like any other.

use std::{path::PathBuf, sync::Arc};

use gpui_kit::{AppContext as _, Context, Task};
use local_git::Repo;

use crate::session::ProjectSettings;

/// How opening one checkout went.
#[derive(Clone, Debug)]
pub enum CheckoutState {
    /// `git` is checking the directory in the background.
    Opening,
    /// A git repository; ask it for diffs and commits.
    Open(Repo),
    /// Why it could not be used: no `git`, not a repository, a missing
    /// directory. The message is a complete sentence for the user.
    Failed(Arc<local_git::Error>),
}

/// One configured project and what is known about its checkout. Cheap to
/// clone (a path, an `Arc`-backed error and a `Repo`, which holds one path).
#[derive(Clone, Debug)]
pub struct Checkout {
    /// The Gerrit project name.
    pub project: String,
    /// The directory, with `~` expanded.
    pub path: PathBuf,
    pub state: CheckoutState,
}

impl Checkout {
    /// The repository, once it is open.
    pub fn repo(&self) -> Option<&Repo> {
        match &self.state {
            CheckoutState::Open(repo) => Some(repo),
            _ => None,
        }
    }

    /// The reason the open failed, if it did.
    pub fn error(&self) -> Option<&local_git::Error> {
        match &self.state {
            CheckoutState::Failed(e) => Some(e),
            _ => None,
        }
    }
}

struct Entry {
    project: ProjectSettings,
    /// `None`: not opened yet (lazy).
    state: Option<CheckoutState>,
    /// The open in flight. Dropping the entry drops the task.
    task: Option<Task<()>>,
}

impl Entry {
    fn checkout(&self) -> Checkout {
        Checkout {
            project: self.project.name.clone(),
            path: self.project.checkout.clone(),
            state: self.state.clone().unwrap_or(CheckoutState::Opening),
        }
    }
}

/// The checkouts of one server. See the module comment.
pub struct Checkouts {
    entries: Vec<Entry>,
}

impl Checkouts {
    pub fn new(projects: Vec<ProjectSettings>) -> Self {
        Self {
            entries: projects.into_iter().map(Entry::new).collect(),
        }
    }

    /// The configured projects, in config order.
    pub fn projects(&self) -> impl Iterator<Item = &ProjectSettings> {
        self.entries.iter().map(|e| &e.project)
    }

    /// The checkout of Gerrit project `name`, or `None` when none is
    /// configured for it. Starts opening it on the first call and reports
    /// [`CheckoutState::Opening`] until the result is in; the entity
    /// notifies then. Never blocks. With the same name configured twice, the
    /// first entry wins.
    pub fn get(&mut self, name: &str, cx: &mut Context<Self>) -> Option<Checkout> {
        let i = self.entries.iter().position(|e| e.project.name == name)?;
        self.open(i, cx);
        Some(self.entries[i].checkout())
    }

    /// Like [`Checkouts::get`] but only reads: nothing is started, and an
    /// entry that was never opened reports [`CheckoutState::Opening`].
    pub fn state(&self, name: &str) -> Option<Checkout> {
        self.entries
            .iter()
            .find(|e| e.project.name == name)
            .map(Entry::checkout)
    }

    /// Starts opening every configured project that has not been tried.
    pub fn open_all(&mut self, cx: &mut Context<Self>) {
        for i in 0..self.entries.len() {
            self.open(i, cx);
        }
    }

    /// Every configured project with its state, in config order. Starts
    /// nothing; see [`Checkouts::open_all`].
    pub fn all(&self) -> Vec<Checkout> {
        self.entries.iter().map(Entry::checkout).collect()
    }

    /// Replaces the configured projects (after a settings save). Entries
    /// whose name and path are unchanged keep what they have; one whose path
    /// changed is opened again, at once if the old one had been opened.
    pub fn set_projects(&mut self, projects: Vec<ProjectSettings>, cx: &mut Context<Self>) {
        let mut old: Vec<Option<Entry>> = std::mem::take(&mut self.entries)
            .into_iter()
            .map(Some)
            .collect();
        let mut restart = Vec::new();
        for (i, project) in projects.into_iter().enumerate() {
            let same_name = old
                .iter()
                .position(|e| e.as_ref().is_some_and(|e| e.project.name == project.name));
            match same_name.and_then(|j| old[j].take()) {
                Some(mut kept) if kept.project.checkout == project.checkout => {
                    kept.project = project;
                    self.entries.push(kept);
                }
                Some(prev) => {
                    if prev.state.is_some() {
                        restart.push(i);
                    }
                    self.entries.push(Entry::new(project));
                }
                None => self.entries.push(Entry::new(project)),
            }
        }
        for i in restart {
            self.open(i, cx);
        }
        cx.notify();
    }

    fn open(&mut self, i: usize, cx: &mut Context<Self>) {
        let entry = &mut self.entries[i];
        if entry.state.is_some() {
            return;
        }
        entry.state = Some(CheckoutState::Opening);
        let name = entry.project.name.clone();
        let path = entry.project.checkout.clone();
        entry.task = Some(cx.spawn(async move |this, cx| {
            let open_path = path.clone();
            let result = cx
                .background_spawn(async move { Repo::open(open_path) })
                .await;
            this.update(cx, |this, cx| {
                // The list may have been replaced meanwhile; the result only
                // belongs to an entry that still has this name and path.
                if let Some(e) = this
                    .entries
                    .iter_mut()
                    .find(|e| e.project.name == name && e.project.checkout == path)
                {
                    e.state = Some(match result {
                        Ok(repo) => CheckoutState::Open(repo),
                        Err(err) => {
                            log::warn!("checkout of {name} at {}: {err}", path.display());
                            CheckoutState::Failed(Arc::new(err))
                        }
                    });
                    e.task = None;
                    cx.notify();
                }
            })
            .ok();
        }));
    }
}

impl Entry {
    fn new(project: ProjectSettings) -> Self {
        Self {
            project,
            state: None,
            task: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use gpui_kit::{Entity, TestAppContext};

    use super::*;

    /// A fresh directory under the system temp dir.
    fn tmp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("checkouts-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// `git init` with an environment that ignores the user's git config.
    fn git_init(dir: &std::path::Path) {
        let status = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .expect("run git");
        assert!(status.success());
    }

    fn project(name: &str, dir: &std::path::Path) -> ProjectSettings {
        ProjectSettings::new(name, dir.to_string_lossy())
    }

    fn state(c: &Entity<Checkouts>, name: &str, cx: &TestAppContext) -> Option<Checkout> {
        c.read_with(cx, |c, _| c.state(name))
    }

    #[gpui_kit::test]
    fn a_configured_checkout_opens_in_the_background(cx: &mut TestAppContext) {
        let dir = tmp("ok");
        git_init(&dir);
        let c = cx.new(|_| Checkouts::new(vec![project("demo/a", &dir)]));
        // The first lookup answers at once and starts the open.
        let first = c
            .update(cx, |c, cx| c.get("demo/a", cx))
            .expect("configured");
        assert!(matches!(first.state, CheckoutState::Opening));
        cx.run_until_parked();
        let done = state(&c, "demo/a", cx).expect("configured");
        assert_eq!(done.repo().map(|r| r.path().to_owned()), Some(dir.clone()));
        assert!(done.error().is_none());
        // Unknown projects have no checkout.
        assert!(c.update(cx, |c, cx| c.get("demo/none", cx)).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui_kit::test]
    fn a_bad_path_reports_the_error(cx: &mut TestAppContext) {
        let plain = tmp("plain");
        let missing = plain.join("does-not-exist");
        let c = cx
            .new(|_| Checkouts::new(vec![project("plain", &plain), project("missing", &missing)]));
        c.update(cx, |c, cx| c.open_all(cx));
        cx.run_until_parked();
        for name in ["plain", "missing"] {
            let ck = state(&c, name, cx).expect("configured");
            assert!(ck.repo().is_none(), "{name}");
            assert!(
                matches!(ck.error(), Some(local_git::Error::NotARepository(_))),
                "{name}: {ck:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&plain);
    }

    #[gpui_kit::test]
    fn state_does_not_start_an_open(cx: &mut TestAppContext) {
        let dir = tmp("lazy");
        git_init(&dir);
        let c = cx.new(|_| Checkouts::new(vec![project("demo/a", &dir)]));
        cx.run_until_parked();
        assert!(matches!(
            state(&c, "demo/a", cx).expect("configured").state,
            CheckoutState::Opening
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui_kit::test]
    fn set_projects_reopens_only_what_changed(cx: &mut TestAppContext) {
        let a = tmp("keep-a");
        let b = tmp("keep-b");
        let b2 = tmp("keep-b2");
        for d in [&a, &b, &b2] {
            git_init(d);
        }
        let c = cx.new(|_| Checkouts::new(vec![project("a", &a), project("b", &b)]));
        c.update(cx, |c, cx| c.open_all(cx));
        cx.run_until_parked();
        // `b` moves, `a` stays, `n` is new and stays lazy.
        c.update(cx, |c, cx| {
            c.set_projects(
                vec![project("a", &a), project("b", &b2), project("n", &a)],
                cx,
            )
        });
        // `a` kept its result without another open.
        assert!(state(&c, "a", cx).expect("a").repo().is_some());
        // `b` is being opened again at its new path.
        let b_now = state(&c, "b", cx).expect("b");
        assert_eq!(b_now.path, b2);
        assert!(matches!(b_now.state, CheckoutState::Opening));
        cx.run_until_parked();
        assert_eq!(
            state(&c, "b", cx)
                .expect("b")
                .repo()
                .map(|r| r.path().to_owned()),
            Some(b2.clone())
        );
        // `n` was never asked for: still not opened.
        c.read_with(cx, |c, _| {
            assert!(
                c.entries
                    .iter()
                    .find(|e| e.project.name == "n")
                    .is_some_and(|e| e.state.is_none())
            );
        });
        // A removed project is gone.
        c.update(cx, |c, cx| c.set_projects(vec![project("a", &a)], cx));
        assert!(state(&c, "b", cx).is_none());
        for d in [&a, &b, &b2] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}
