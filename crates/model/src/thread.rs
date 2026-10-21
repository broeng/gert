//! Comment threads: grouping, state, and where a thread shows in a diff.
//!
//! # Threads
//!
//! Gerrit has no thread object. A thread is the set of comments that share
//! a root, where a comment points at the one it answers with `in_reply_to`
//! and a root has none. Drafts join the thread of the published comment
//! they reply to; a draft without `in_reply_to` is a new thread of its own.
//! Replies are flattened (two replies to one comment are two entries of the
//! same thread), ordered by time, as PolyGerrit does.
//!
//! **State.** A thread is unresolved if its *last* entry is: Gerrit stores
//! the resolution in the newest comment. A draft counts, as it does in
//! PolyGerrit: a draft reply that resolves a thread shows the thread as
//! resolved before it is published, and one that reopens it shows it as
//! unresolved. (Drafts being deleted are ignored, see
//! [`Draft::is_visible`].)
//!
//! **Orphans.** A comment whose `in_reply_to` names a comment we do not have
//! (deleted on the server, or not fetched) starts a thread of its own at the
//! topmost comment of its chain, flagged [`Thread::orphaned`]. A reply cycle
//! cannot happen on a real server; it is cut at the first repeated comment
//! rather than looping.
//!
//! # Where a thread shows in a diff
//!
//! A diff compares a *target* patch set with a *base*, which is another
//! patch set or, for `None`, the parent commit. The right side shows the
//! target, the left side shows the base.
//!
//! * Side `REVISION` on patch set `n` is the file of patch set `n`. It shows
//!   on the right when `n` is the target, and on the left when `n` is the
//!   base patch set.
//! * Side `PARENT` on patch set `n` is the file in `n`'s parent commit. It
//!   shows on the left when the base is the parent (`None`) and `n` is the
//!   target: then the left side is exactly that file. For a merge commit
//!   only the first parent is ever diffed, so a comment on another parent
//!   (`parent` of 2 or more) shows nowhere.
//! * A thread on any other patch set does not show in this diff; it is
//!   counted as "elsewhere" in [`comment_counts`].
//! * Paths are those of the patch set the comment is on, so for a renamed
//!   file a left-side comment carries the old path. A thread matches a file
//!   on the right by `path`, and on the left by `path` or `old_path`.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::{
    Account, Comment, Draft, FileEntry, LocalDraftId, Location, PatchSetNumber, Side, Timestamp,
    file_order,
};

/// Identifies a thread: its root comment, or the draft that started it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ThreadId {
    /// The id of the published root comment (or, for an orphan, of the
    /// comment it answered but we lack).
    Comment(String),
    /// A thread that so far consists of this one new draft.
    Draft(LocalDraftId),
}

/// One comment or draft within a thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Entry {
    Published(Comment),
    Draft(Draft),
}

impl Entry {
    pub fn message(&self) -> &str {
        match self {
            Self::Published(c) => &c.message,
            Self::Draft(d) => &d.message,
        }
    }

    /// `None` for drafts (they are the signed-in user's own).
    pub fn author(&self) -> Option<&Account> {
        match self {
            Self::Published(c) => c.author.as_ref(),
            Self::Draft(_) => None,
        }
    }

    pub fn updated(&self) -> Timestamp {
        match self {
            Self::Published(c) => c.updated,
            Self::Draft(d) => d.updated,
        }
    }

    pub fn unresolved(&self) -> bool {
        match self {
            Self::Published(c) => c.unresolved,
            Self::Draft(d) => d.unresolved,
        }
    }

    pub fn is_draft(&self) -> bool {
        matches!(self, Self::Draft(_))
    }
}

/// A thread of comments anchored at one place of one patch set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub id: ThreadId,
    pub path: String,
    pub patch_set: PatchSetNumber,
    pub side: Side,
    pub parent: Option<u32>,
    pub location: Location,
    /// Oldest first; published comments before drafts at equal times.
    pub entries: Vec<Entry>,
    /// The root answers a comment that is not among those given.
    pub orphaned: bool,
}

impl Thread {
    /// Whether the thread asks for something to be done: its last entry
    /// (drafts included) is unresolved.
    pub fn is_unresolved(&self) -> bool {
        self.entries.last().is_some_and(Entry::unresolved)
    }

    pub fn has_draft(&self) -> bool {
        self.entries.iter().any(Entry::is_draft)
    }

    /// Number of published comments.
    pub fn comment_count(&self) -> usize {
        self.entries.iter().filter(|e| !e.is_draft()).count()
    }

    pub fn draft_count(&self) -> usize {
        self.entries.len() - self.comment_count()
    }

    /// The `in_reply_to` for a new reply: the last published comment.
    /// `None` if the thread has only drafts so far.
    pub fn reply_target(&self) -> Option<&str> {
        self.entries.iter().rev().find_map(|e| match e {
            Entry::Published(c) => Some(c.id.as_str()),
            Entry::Draft(_) => None,
        })
    }

    /// The draft reply in this thread, if the user already started one.
    pub fn draft(&self) -> Option<&Draft> {
        self.entries.iter().find_map(|e| match e {
            Entry::Draft(d) => Some(d),
            Entry::Published(_) => None,
        })
    }

    /// Time of the newest entry.
    pub fn last_updated(&self) -> Option<Timestamp> {
        self.entries.iter().map(Entry::updated).max()
    }

    /// The side of the diff `pair` this thread shows on for the file
    /// `path` (with `old_path` for renames), or `None` if it does not show.
    /// See the module documentation for the rules.
    pub fn side_in(&self, pair: DiffPair, path: &str, old_path: Option<&str>) -> Option<DiffSide> {
        let on_path = |p: &str| self.path == p;
        match self.side {
            Side::Revision if self.patch_set == pair.target && on_path(path) => {
                Some(DiffSide::Right)
            }
            Side::Revision
                if pair.base == Some(self.patch_set)
                    && (on_path(path) || old_path.is_some_and(on_path)) =>
            {
                Some(DiffSide::Left)
            }
            Side::Parent
                if pair.base.is_none()
                    && self.patch_set == pair.target
                    && self.parent.unwrap_or(1) == 1
                    && (on_path(path) || old_path.is_some_and(on_path)) =>
            {
                Some(DiffSide::Left)
            }
            _ => None,
        }
    }
}

/// The two sides of a diff.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DiffSide {
    Left,
    Right,
}

/// Which two patch sets a diff compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DiffPair {
    /// `None`: the parent commit of `target`.
    pub base: Option<PatchSetNumber>,
    pub target: PatchSetNumber,
}

/// Groups comments and drafts into threads, sorted for a change-level list
/// (see [`sort_threads`]). Drafts that are about to be deleted are left
/// out. Input order does not matter.
pub fn build_threads(comments: &[Comment], drafts: &[Draft]) -> Vec<Thread> {
    let by_id: HashMap<&str, &Comment> = comments.iter().map(|c| (c.id.as_str(), c)).collect();

    // Climbs from a comment to the top of its reply chain. Returns the
    // topmost comment we have, and whether its own parent is missing.
    let root_of = |start: &Comment| -> (String, bool) {
        let mut cur = start;
        let mut seen = HashSet::new();
        seen.insert(cur.id.as_str());
        while let Some(parent_id) = cur.in_reply_to.as_deref() {
            match by_id.get(parent_id) {
                Some(parent) if seen.insert(parent.id.as_str()) => cur = parent,
                Some(_) => return (cur.id.clone(), false), // cycle
                None => return (cur.id.clone(), true),
            }
        }
        (cur.id.clone(), false)
    };

    struct Group<'a> {
        orphaned: bool,
        entries: Vec<Entry>,
        /// The comment the anchor is taken from: the root, or for a thread
        /// of drafts only, the earliest draft.
        root: Option<&'a Comment>,
    }
    // How many comments precede an entry in its reply chain.
    let depth = |e: &Entry| -> usize {
        let (mut next, mut n) = match e {
            Entry::Published(c) => (c.in_reply_to.as_deref(), 0),
            Entry::Draft(d) => (d.in_reply_to.as_deref(), 0),
        };
        while let Some(parent) = next.and_then(|p| by_id.get(p)) {
            n += 1;
            if n > comments.len() {
                break; // a cycle
            }
            next = parent.in_reply_to.as_deref();
        }
        n
    };
    let mut groups: HashMap<ThreadId, Group<'_>> = HashMap::new();

    for c in comments {
        let (root, orphaned) = root_of(c);
        let g = groups
            .entry(ThreadId::Comment(root.clone()))
            .or_insert_with(|| Group {
                orphaned,
                entries: Vec::new(),
                root: by_id.get(root.as_str()).copied(),
            });
        g.entries.push(Entry::Published(c.clone()));
    }

    let mut draft_only: Vec<(ThreadId, bool, &Draft)> = Vec::new();
    for d in drafts.iter().filter(|d| d.is_visible()) {
        let key = match d.in_reply_to.as_deref() {
            None => (ThreadId::Draft(d.local_id), false),
            Some(parent) => match by_id.get(parent) {
                Some(p) => (ThreadId::Comment(root_of(p).0), false),
                // Replying to a comment we do not have: a thread of drafts
                // under the missing comment's id.
                None => (ThreadId::Comment(parent.to_owned()), true),
            },
        };
        if let Some(g) = groups.get_mut(&key.0) {
            g.entries.push(Entry::Draft(d.clone()));
        } else {
            draft_only.push((key.0, key.1, d));
        }
    }
    // Drafts that open a thread (or answer a missing comment) form their own
    // groups, the earliest draft being the anchor.
    draft_only.sort_by_key(|(_, _, d)| (d.updated, d.local_id));
    let mut anchors: HashMap<ThreadId, &Draft> = HashMap::new();
    for (id, orphaned, d) in draft_only {
        anchors.entry(id.clone()).or_insert(d);
        groups
            .entry(id)
            .or_insert_with(|| Group {
                orphaned,
                entries: Vec::new(),
                root: None,
            })
            .entries
            .push(Entry::Draft(d.clone()));
    }

    let mut threads: Vec<Thread> = groups
        .into_iter()
        .filter_map(|(id, mut g)| {
            // Time first; the server's timestamps are only exact to the
            // second for comments made in one go, so a reply must also sort
            // after the comment it answers when they are equal.
            g.entries
                .sort_by_key(|e| (e.updated(), e.is_draft(), depth(e), entry_key(e)));
            let (path, patch_set, side, parent, location) = match (g.root, anchors.get(&id)) {
                (Some(c), _) => (c.path.clone(), c.patch_set, c.side, c.parent, c.location),
                (None, Some(d)) => (d.path.clone(), d.patch_set, d.side, d.parent, d.location),
                // A group is only created together with an entry, and an
                // entry is either a comment (root known) or a draft (anchor
                // known).
                (None, None) => return None,
            };
            Some(Thread {
                id,
                path,
                patch_set,
                side,
                parent,
                location,
                entries: g.entries,
                orphaned: g.orphaned,
            })
        })
        .collect();
    sort_threads(&mut threads);
    threads
}

fn entry_key(e: &Entry) -> String {
    match e {
        Entry::Published(c) => c.id.clone(),
        Entry::Draft(d) => format!("{:020}", d.local_id.0),
    }
}

/// Sorts threads like a change-level list: by file (special files first,
/// then by path), patch set, position in the file, then age.
pub fn sort_threads(threads: &mut [Thread]) {
    threads.sort_by(|a, b| {
        file_order(&a.path, &b.path)
            .then(a.patch_set.cmp(&b.patch_set))
            .then(a.location.start_line().cmp(&b.location.start_line()))
            .then(a.last_updated().cmp(&b.last_updated()))
            .then_with(|| format!("{:?}", a.id).cmp(&format!("{:?}", b.id)))
    });
}

/// Comment numbers of one file for the file list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileCommentCounts {
    /// Published comments visible in the diff.
    pub comments: u32,
    /// Unresolved threads visible in the diff.
    pub unresolved: u32,
    /// Drafts visible in the diff.
    pub drafts: u32,
    /// Comments and drafts on this file that the diff does not show
    /// (other patch sets).
    pub elsewhere: u32,
}

/// Per-file comment numbers for the diff `pair`, keyed by the path in
/// `files`. Files without any comment are left out; threads whose path is
/// no file of the list (the commit message when it is not listed,
/// patch-set level comments) are ignored.
pub fn comment_counts(
    threads: &[Thread],
    pair: DiffPair,
    files: &[FileEntry],
) -> BTreeMap<String, FileCommentCounts> {
    let mut counts: BTreeMap<String, FileCommentCounts> = BTreeMap::new();
    for t in threads {
        // The file this thread belongs to: by new path, else by old path.
        let Some(file) = files.iter().find(|f| f.path == t.path).or_else(|| {
            files
                .iter()
                .find(|f| f.old_path.as_deref() == Some(&t.path))
        }) else {
            continue;
        };
        let n = counts.entry(file.path.clone()).or_default();
        let comments = u32::try_from(t.comment_count()).unwrap_or(u32::MAX);
        let drafts = u32::try_from(t.draft_count()).unwrap_or(u32::MAX);
        if t.side_in(pair, &file.path, file.old_path.as_deref())
            .is_some()
        {
            n.comments += comments;
            n.drafts += drafts;
            n.unresolved += u32::from(t.is_unresolved());
        } else {
            n.elsewhere += comments + drafts;
        }
    }
    counts
}

#[cfg(test)]
mod tests;
