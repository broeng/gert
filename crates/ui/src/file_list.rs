//! The file list of one comparison and its local source, for the change
//! page and the diff page.
//!
//! [`FileList`] gets the list for a [`FilesKey`] in this order: the store
//! (or what the caller read from it, see [`Seed`]), then, on a miss, the
//! project's checkout (when it has the commits) and Gerrit at the same
//! time. The first answer is shown and Gerrit's replaces the checkout's;
//! nothing from the checkout is stored, and a list that comes late for
//! another key is dropped. Gerrit is never asked about a local change or
//! version ([`FilesKey::is_local`]): only the checkout can list it. A store
//! read that fails counts as a miss.
//!
//! It also resolves the comparison in the checkout as soon as it has a key
//! and a project ([`LocalState`]), for the diffs and the syntax worker, and
//! shows the reviewed marks: `r` flips one at once and sends it, and once
//! the mark settled (`Syncer::set_reviewed`) it shows what the server has.
//!
//! Pages read a [`Files`] snapshot, which they copy when an event says it
//! changed (the change page reads the list in many places without a `cx`).

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use gpui_kit::*;
use model::{FileEntry, FilesKey};
use store::CachedFileList;

use crate::{
    checkouts::Checkouts,
    local_source::{
        LocalLazy, LocalSource, LocalState, Lookup, Renames, Resolved, Unserved, local_file_list,
        lookup, renames_of,
    },
    reviewed,
    session::{Session, is_unauthorized},
};

/// The files notice of a local version the checkout cannot list, which
/// Gerrit does not have.
pub const LOCAL_FILES_UNAVAILABLE: &str =
    "the project's checkout does not have this version (the files of local versions come from it)";

/// What the caller already knows of the store's list for a key.
pub enum Seed {
    /// Nothing: the list reads the store first.
    Unread,
    /// The store had this list.
    Hit(CachedFileList),
    /// The store had none.
    Miss,
}

/// What a store read of the list says for [`FileList::load`]. A read that
/// fails counts as a miss (logged): the checkout and Gerrit can still list
/// the files.
pub fn seed_of(read: Result<Option<CachedFileList>, sync::Error>) -> Seed {
    match read {
        Ok(Some(list)) => Seed::Hit(list),
        Ok(None) => Seed::Miss,
        Err(e) => {
            log::warn!("reading cached files: {e}");
            Seed::Miss
        }
    }
}

/// When Gerrit is asked for the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ask {
    /// Not now (offline loads, a page that only reads the cache).
    Never,
    /// Only when the store has no list.
    OnMiss,
    /// Also after a store hit, for fresh reviewed marks.
    Always,
}

pub enum FileListEvent {
    /// A list arrived for the key (from the store, the checkout or Gerrit).
    Listed,
    /// No new list, but what the page shows about it may differ: a request
    /// ended without one (an error, or nothing left that could bring one).
    Changed,
    /// Only the reviewed marks changed.
    MarksChanged,
    /// The local source for the key is resolved (served or not).
    LocalResolved,
    /// A reviewed mark could not be sent and was put back; the note says
    /// why.
    ReviewedFailed(String),
    /// The server answered 401: back to the login flow.
    Unauthorized,
}

/// What a page shows of the list: cheap to clone (`Arc`s inside).
#[derive(Clone, Default)]
pub struct Files {
    pub key: Option<FilesKey>,
    pub list: Arc<Vec<FileEntry>>,
    pub reviewed: Arc<HashSet<String>>,
    /// The renames of `list`, for the local source.
    pub renames: Arc<Renames>,
    /// A list (possibly empty) has arrived for `key`.
    pub loaded: bool,
    /// The checkout the list was made in, while it is the local list (no
    /// reviewed marks, never stored) that Gerrit's has not replaced yet.
    pub checkout: Option<PathBuf>,
    /// Why there is no list, while there is none.
    pub error: Option<String>,
    /// The local source for `key`.
    pub source: LocalState,
    /// A list is there, or no request is left that could bring one.
    pub settled: bool,
}

impl Files {
    /// The resolved local source for the list, with its renames.
    pub fn local_source(&self) -> Option<LocalSource> {
        let source = self.source.source_for(self.key.as_ref()?)?;
        Some(source.with_renames(self.renames.clone()))
    }

    /// The local source for `load_diff` and the syntax worker, resolved or
    /// not yet.
    pub fn local_lazy(&self) -> LocalLazy {
        match &self.key {
            Some(key) => self.source.lazy(key, &self.renames),
            None => LocalLazy::none(),
        }
    }

    /// The checkout the local diffs come from, once resolved.
    pub fn local_path(&self) -> Option<PathBuf> {
        self.local_source().map(|s| s.path().to_owned())
    }

    /// Whether the comparison is with a local version whose two sides have
    /// different parents (a rebase), which nobody can show: the checkout
    /// leaves it to Gerrit, which marks the edits a rebase brings, and
    /// Gerrit does not have the local version.
    pub fn parents_differ(&self) -> bool {
        self.key.as_ref().is_some_and(|k| {
            k.is_local() && !self.loaded && self.source.unserved(k) == Some(Unserved::ParentsDiffer)
        })
    }

    /// The checkout the list on show was made in, while it is the local one.
    pub fn checkout(&self) -> Option<&Path> {
        self.checkout.as_deref()
    }

    /// Whether `r` has to wait: there is no list yet, or the one on show is
    /// the checkout's, which Gerrit's (computed before a mark) replaces.
    /// Also when every request failed: a mark against a list we do not
    /// have would be a guess.
    pub fn marks_wait(&self) -> bool {
        !self.loaded || self.checkout.is_some()
    }

    pub fn is_reviewed(&self, path: &str) -> bool {
        self.reviewed.contains(path)
    }
}

pub struct FileList {
    session: Session,
    checkouts: Option<Entity<Checkouts>>,
    files: Files,
    /// The requests for the key that may still bring a list.
    reading: bool,
    listing: bool,
    fetching: bool,
    store_task: Option<Task<()>>,
    local_task: Option<Task<()>>,
    /// Making the list in the checkout.
    listing_task: Option<Task<()>>,
    fetch_task: Option<Task<()>>,
}

impl EventEmitter<FileListEvent> for FileList {}

impl FileList {
    pub fn new(session: Session) -> Self {
        Self {
            session,
            checkouts: None,
            files: Files::default(),
            reading: false,
            listing: false,
            fetching: false,
            store_task: None,
            local_task: None,
            listing_task: None,
            fetch_task: None,
        }
    }

    /// Names the server's checkouts. Set right after the page is made,
    /// before anything has loaded.
    pub fn set_checkouts(&mut self, checkouts: Option<Entity<Checkouts>>) {
        self.checkouts = checkouts;
    }

    /// What the page shows.
    pub fn files(&self) -> Files {
        Files {
            settled: self.settled(),
            ..self.files.clone()
        }
    }

    /// Back to the moment before the list arrived, with Gerrit still
    /// asked: a synchronous fake cannot hold that window open.
    #[cfg(test)]
    pub(crate) fn unload_for_test(&mut self) {
        self.files.loaded = false;
        self.fetching = true;
    }

    fn settled(&self) -> bool {
        self.files.loaded || !(self.reading || self.listing || self.fetching)
    }

    /// Loads the list for `key` of `project`. Another key than the one on
    /// show starts over at once, which the result says (`true`), so the
    /// page can drop what depended on the old list; the same key loads
    /// again (a poll). `seed` is what the caller read from the store, and
    /// `ask` says when Gerrit is asked.
    pub fn load(
        &mut self,
        key: Option<FilesKey>,
        project: &str,
        seed: Seed,
        ask: Ask,
        cx: &mut Context<Self>,
    ) -> bool {
        let restart = key != self.files.key;
        if restart {
            self.files = Files {
                key: key.clone(),
                ..Files::default()
            };
            self.reading = false;
            self.listing = false;
            self.fetching = false;
            self.store_task = None;
            self.local_task = None;
            self.listing_task = None;
            self.fetch_task = None;
        }
        let Some(key) = key else {
            return restart;
        };
        self.resolve_local(&key, project, cx);
        match seed {
            Seed::Unread => self.read_store(key, ask, cx),
            Seed::Hit(list) => {
                self.apply_stored(&key, list, cx);
                self.stored(key, false, ask, cx);
            }
            Seed::Miss => self.stored(key, true, ask, cx),
        }
        restart
    }

    fn read_store(&mut self, key: FilesKey, ask: Ask, cx: &mut Context<Self>) {
        let syncer = self.session.syncer().clone();
        self.reading = true;
        self.store_task = Some(cx.spawn(async move |this, cx| {
            let cached = {
                let key = key.clone();
                cx.background_spawn(async move { syncer.cached_files(&key) })
                    .await
            };
            this.update(cx, |this, cx| {
                this.reading = false;
                let miss = match seed_of(cached) {
                    Seed::Hit(list) => {
                        this.apply_stored(&key, list, cx);
                        false
                    }
                    Seed::Unread | Seed::Miss => true,
                };
                this.stored(key, miss, ask, cx);
            })
            .ok();
        }));
    }

    fn apply_stored(&mut self, key: &FilesKey, list: CachedFileList, cx: &mut Context<Self>) {
        let reviewed = list.reviewed.unwrap_or_default().into_iter().collect();
        self.apply(key, list.files, reviewed, None, cx);
    }

    /// What the store said is known: on a miss the checkout makes a list
    /// until Gerrit's arrives, and Gerrit is asked as `ask` says.
    fn stored(&mut self, key: FilesKey, miss: bool, ask: Ask, cx: &mut Context<Self>) {
        if miss {
            self.list_locally(&key, cx);
        }
        let ask = match ask {
            Ask::Never => false,
            Ask::OnMiss => miss,
            Ask::Always => true,
        };
        // Gerrit cannot list a local version: the checkout does.
        if ask && self.session.can_sync() && !key.is_local() {
            self.fetch(key, cx);
        }
        if miss && self.settled() {
            // Nothing left that could bring a list.
            cx.emit(FileListEvent::Changed);
        }
    }

    /// Makes the list in the checkout when none is on show. Never stored;
    /// Gerrit's list replaces it.
    fn list_locally(&mut self, key: &FilesKey, cx: &mut Context<Self>) {
        if self.files.loaded {
            return;
        }
        if matches!(&self.files.source, LocalState::Ready(_, Err(_))) {
            // No checkout, or it cannot serve this comparison.
            self.no_local_list(key);
            return;
        }
        let lazy = self.files.source.lazy(key, &Arc::default());
        let key = key.clone();
        self.listing = true;
        self.listing_task = Some(cx.spawn(async move |this, cx| {
            let made = local_file_list(lazy, cx).await;
            this.update(cx, |this, cx| {
                this.listing = false;
                match made {
                    // Gerrit's list (or a stored one) may have come first.
                    Some((path, files)) if !this.files.loaded => {
                        this.apply(&key, files, HashSet::new(), Some(path), cx);
                    }
                    Some(_) => {}
                    None => {
                        this.no_local_list(&key);
                        cx.emit(FileListEvent::Changed);
                    }
                }
            })
            .ok();
        }));
    }

    /// The checkout cannot list `key`. Gerrit's list comes instead, except
    /// for a local version, which Gerrit does not have: then the error says
    /// so (never one of Gerrit's).
    fn no_local_list(&mut self, key: &FilesKey) {
        if self.files.key.as_ref() == Some(key)
            && key.is_local()
            && !self.files.loaded
            && !self.files.parents_differ()
        {
            self.files.error = Some(LOCAL_FILES_UNAVAILABLE.to_owned());
        }
    }

    fn fetch(&mut self, key: FilesKey, cx: &mut Context<Self>) {
        let syncer = self.session.syncer().clone();
        self.fetching = true;
        self.fetch_task = Some(cx.spawn(async move |this, cx| {
            let fetched = {
                let key = key.clone();
                cx.background_spawn(async move { syncer.refresh_files(&key).await })
                    .await
            };
            this.update(cx, |this, cx| {
                this.fetching = false;
                match fetched {
                    Ok(list) => this.apply_stored(&key, list, cx),
                    // The login flow takes over; no point in more requests.
                    Err(e) if is_unauthorized(&e) => cx.emit(FileListEvent::Unauthorized),
                    Err(e) => {
                        log::warn!("fetching files of change {}: {e}", key.change);
                        // With a list on show (a poll) nothing changes.
                        if this.files.key.as_ref() == Some(&key) && !this.files.loaded {
                            this.files.error = Some(e.to_string());
                            cx.emit(FileListEvent::Changed);
                        }
                    }
                }
            })
            .ok();
        }));
    }

    /// Shows `files`. `checkout` is where they were made, `None` for the
    /// stored list or Gerrit's, which also stops the checkout's from
    /// landing late.
    fn apply(
        &mut self,
        key: &FilesKey,
        files: Vec<FileEntry>,
        reviewed: HashSet<String>,
        checkout: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if self.files.key.as_ref() != Some(key) {
            return; // another pair was picked meanwhile
        }
        if checkout.is_none() {
            self.listing_task = None;
            self.listing = false;
        }
        self.files.renames = renames_of(&files);
        self.files.list = Arc::new(files);
        self.files.reviewed = Arc::new(reviewed);
        self.files.loaded = true;
        self.files.checkout = checkout;
        self.files.error = None;
        cx.emit(FileListEvent::Listed);
    }

    /// Starts resolving `key` in the project's checkout unless that is done
    /// or under way. Nothing waits for it: stored diffs are read first, and
    /// a diff that has to be computed awaits it in `load_diff`.
    fn resolve_local(&mut self, key: &FilesKey, project: &str, cx: &mut Context<Self>) {
        if self.files.source.is_for(key) {
            return;
        }
        self.local_task = None;
        self.files.source = match lookup(&self.checkouts, project, key, cx) {
            Lookup::Unconfigured => LocalState::Ready(key.clone(), Err(Unserved::NoCheckout)),
            Lookup::Ready(s) => LocalState::Ready(key.clone(), Ok(s)),
            Lookup::Pending(p) => {
                let (task_key, shared) = (key.clone(), p.clone());
                self.local_task = Some(cx.spawn(async move |this, cx| {
                    let source = shared.await;
                    this.update(cx, |this, cx| this.local_resolved(&task_key, source, cx))
                        .ok();
                }));
                LocalState::Resolving(key.clone(), p)
            }
        };
    }

    fn local_resolved(&mut self, key: &FilesKey, source: Resolved, cx: &mut Context<Self>) {
        if !matches!(&self.files.source, LocalState::Resolving(k, _) if k == key) {
            return;
        }
        self.files.source = LocalState::Ready(key.clone(), source);
        self.local_task = None;
        if self.files.parents_differ() {
            // Not the checkout's fault, whatever the list's request said.
            self.files.error = None;
        }
        cx.emit(FileListEvent::LocalResolved);
    }

    /// `r` on `path`: flips its reviewed mark at once and sends it, or says
    /// why not now ([`Files::marks_wait`]). The caller has checked the
    /// local rules and the session ([`crate::local_rules::refusal`]). The
    /// request is detached, so it ends also when the page goes (the diff
    /// page is made anew for each file). `Syncer::set_reviewed` keeps the
    /// cache, and so every list read meanwhile, in step with the marks on
    /// their way; once none for this one is open, the mark shows what it
    /// settled to.
    pub fn toggle_reviewed(
        &mut self,
        path: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), &'static str> {
        if self.files.marks_wait() {
            return Err(reviewed::WAITS);
        }
        let Some(key) = self.files.key.clone() else {
            return Err(reviewed::WAITS);
        };
        let reviewed = !self.files.is_reviewed(path);
        set_mark(Arc::make_mut(&mut self.files.reviewed), path, reviewed);
        cx.emit(FileListEvent::MarksChanged);
        let syncer = self.session.syncer().clone();
        let (change, sha, path) = (key.change, key.target.sha.clone(), path.to_owned());
        let sent = path.clone();
        cx.spawn(async move |this, cx| {
            let answer = cx
                .background_spawn(reviewed::send(syncer, change, sha, sent, reviewed))
                .await;
            this.update(cx, |this, cx| this.answered(&key, &path, answer, cx))
                .ok();
        })
        .detach();
        Ok(())
    }

    /// The answer to a mark request: a 401 goes to the login flow, and once
    /// the mark settled it shows that, with a note if it went back.
    fn answered(
        &mut self,
        key: &FilesKey,
        path: &str,
        answer: reviewed::Answer,
        cx: &mut Context<Self>,
    ) {
        let login = answer.unauthorized;
        if login {
            cx.emit(FileListEvent::Unauthorized);
        }
        let Some(settled) = answer.settled else {
            return;
        };
        if self.files.key.as_ref() != Some(key) || self.files.is_reviewed(path) == settled.reviewed
        {
            return;
        }
        set_mark(
            Arc::make_mut(&mut self.files.reviewed),
            path,
            settled.reviewed,
        );
        cx.emit(FileListEvent::MarksChanged);
        if let Some((asked, e)) = settled.refused
            && !login
        {
            let note = reviewed::failure_note(path, asked, &e);
            cx.emit(FileListEvent::ReviewedFailed(note));
        }
    }
}

fn set_mark(marks: &mut HashSet<String>, path: &str, reviewed: bool) {
    if reviewed {
        marks.insert(path.to_owned());
    } else {
        marks.remove(path);
    }
}

#[cfg(test)]
mod tests;
