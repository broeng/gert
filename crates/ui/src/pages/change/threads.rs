//! The pure parts of the change page's thread list: which threads a filter
//! lists, how a row reads, and which diff opens a thread.

use model::{
    COMMIT_MSG_PATH, DiffPair, DraftSync, Entry, FileEntry, Location, MERGE_LIST_PATH,
    PATCHSET_LEVEL_PATH, PatchSetNumber, Thread,
};

/// Which threads the list shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadFilter {
    Unresolved,
    /// Threads that have a draft of the user's own.
    Drafts,
    All,
}

impl ThreadFilter {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unresolved => "Unresolved",
            Self::Drafts => "Drafts",
            Self::All => "All",
        }
    }

    pub fn matches(self, t: &Thread) -> bool {
        match self {
            Self::Unresolved => t.is_unresolved(),
            Self::Drafts => t.has_draft(),
            Self::All => true,
        }
    }

    /// What the user sees without having picked: the unresolved threads,
    /// or all of them if none is unresolved.
    pub fn default_for(threads: &[Thread]) -> Self {
        if threads.iter().any(Thread::is_unresolved) {
            Self::Unresolved
        } else {
            Self::All
        }
    }

    /// Text for an empty list.
    pub fn nothing(self) -> &'static str {
        match self {
            Self::Unresolved => "No unresolved comments.",
            Self::Drafts => "No drafts.",
            Self::All => "No comments yet.",
        }
    }
}

/// The indexes of the threads the filter lists, in the given order.
pub fn listed(threads: &[Thread], filter: ThreadFilter) -> Vec<usize> {
    threads
        .iter()
        .enumerate()
        .filter(|(_, t)| filter.matches(t))
        .map(|(i, _)| i)
        .collect()
}

/// Where the thread is, for a row: `path:line`, `path (file)`, or
/// `Patchset`.
pub fn location_text(t: &Thread) -> String {
    let name = match t.path.as_str() {
        PATCHSET_LEVEL_PATH => return "Patchset".to_owned(),
        COMMIT_MSG_PATH => "Commit message",
        MERGE_LIST_PATH => "Merge list",
        p => p,
    };
    match t.location {
        Location::File => format!("{name} (file)"),
        Location::Line(_) | Location::Range(_) => {
            format!("{name}:{}", t.location.line().unwrap_or_default())
        }
    }
}

/// The first non-blank line of the thread's last message.
pub fn last_line(t: &Thread) -> String {
    t.entries
        .last()
        .map(Entry::message)
        .and_then(|m| m.lines().find(|l| !l.trim().is_empty()))
        .unwrap_or("")
        .trim()
        .to_owned()
}

/// Whether a draft of the thread could not be uploaded.
pub fn has_failed_draft(t: &Thread) -> bool {
    t.entries
        .iter()
        .any(|e| matches!(e, Entry::Draft(d) if matches!(d.sync, DraftSync::Failed { .. })))
}

/// The patch sets and file name of the diff that shows `t`: the pair the
/// page compares now if the thread is visible in it, else the thread's own
/// patch set against the parent (where `Side::Parent` comments are on the
/// left and `Side::Revision` ones on the right). `file` is the thread's
/// file in the page's list, which has the name under which the diff is
/// requested (a thread on a renamed file's old side carries the old name).
pub fn diff_for(
    t: &Thread,
    current: DiffPair,
    file: Option<&FileEntry>,
) -> (Option<PatchSetNumber>, PatchSetNumber, String) {
    let path = file.map_or_else(|| t.path.clone(), |f| f.path.clone());
    if let Some(f) = file
        && t.side_in(current, &f.path, f.old_path.as_deref()).is_some()
    {
        return (current.base, current.target, path);
    }
    (None, t.patch_set, path)
}

#[cfg(test)]
mod tests {
    use model::{DraftSync, FileStatus, Location, PatchSetNumber, Side, Thread};

    use super::*;
    use crate::{
        diff::tests::threads::{draft, thread},
        testing::file,
    };

    fn on(path: &str, location: Location, unresolved: bool) -> Thread {
        let mut t = thread("t", location, "msg", unresolved);
        t.path = path.into();
        t
    }

    #[test]
    fn filters_and_the_default() {
        let open = on("a", Location::Line(1), true);
        let done = on("b", Location::Line(2), false);
        let mut drafted = on("c", Location::Line(3), false);
        drafted.entries.push(Entry::Draft(draft(
            1,
            Location::Line(3),
            "d",
            DraftSync::Synced,
        )));
        // A draft that does not resolve makes the thread unresolved.
        drafted.entries.push(Entry::Draft(draft(
            2,
            Location::Line(3),
            "x",
            DraftSync::PendingCreate,
        )));
        let all = [open, done, drafted];
        assert_eq!(listed(&all, ThreadFilter::All), [0, 1, 2]);
        assert_eq!(listed(&all, ThreadFilter::Drafts), [2]);
        assert_eq!(listed(&all, ThreadFilter::Unresolved), [0, 2]);
        assert_eq!(ThreadFilter::default_for(&all), ThreadFilter::Unresolved);
        assert_eq!(ThreadFilter::default_for(&all[1..2]), ThreadFilter::All);
        assert_eq!(ThreadFilter::default_for(&[]), ThreadFilter::All);
    }

    #[test]
    fn rows_read_like_a_location() {
        assert_eq!(
            location_text(&on("src/a.rs", Location::Line(12), true)),
            "src/a.rs:12"
        );
        assert_eq!(
            location_text(&on("src/a.rs", Location::File, true)),
            "src/a.rs (file)"
        );
        assert_eq!(
            location_text(&on("/COMMIT_MSG", Location::Line(2), true)),
            "Commit message:2"
        );
        assert_eq!(
            location_text(&on("/PATCHSET_LEVEL", Location::File, true)),
            "Patchset"
        );
        let mut t = on("a", Location::Line(1), true);
        if let Some(Entry::Published(c)) = t.entries.first_mut() {
            c.message = "\n\n  first real line  \nsecond".into();
        }
        assert_eq!(last_line(&t), "first real line");
    }

    #[test]
    fn a_failed_draft_is_noticed() {
        let mut t = on("a", Location::Line(1), true);
        assert!(!has_failed_draft(&t));
        t.entries.push(Entry::Draft(draft(
            1,
            Location::Line(1),
            "d",
            DraftSync::Failed {
                op: model::DraftOp::Create,
                error: "no".into(),
                retryable: false,
            },
        )));
        assert!(has_failed_draft(&t));
    }

    #[test]
    fn a_thread_opens_where_it_shows() {
        let pair = |base: Option<u32>, target| DiffPair {
            base: base.map(PatchSetNumber),
            target: PatchSetNumber(target),
        };
        let f = file("src/a.rs", FileStatus::Modified);
        let mut t = on("src/a.rs", Location::Line(3), true);
        t.patch_set = PatchSetNumber(2);
        // Shown in the current pair: keep it.
        assert_eq!(
            diff_for(&t, pair(Some(1), 2), Some(&f)),
            (
                Some(PatchSetNumber(1)),
                PatchSetNumber(2),
                "src/a.rs".into()
            )
        );
        // Not shown (another target): its own patch set against the parent.
        assert_eq!(
            diff_for(&t, pair(None, 3), Some(&f)),
            (None, PatchSetNumber(2), "src/a.rs".into())
        );
        // A left-side comment (on the parent) is shown against the parent.
        t.side = Side::Parent;
        assert_eq!(
            diff_for(&t, pair(None, 2), Some(&f)),
            (None, PatchSetNumber(2), "src/a.rs".into())
        );
        // The file is no longer in the list: the thread's own path.
        assert_eq!(
            diff_for(&t, pair(None, 3), None),
            (None, PatchSetNumber(2), "src/a.rs".into())
        );
        // A comment on the old name of a renamed file opens the new name.
        let mut renamed = file("src/new.rs", FileStatus::Renamed);
        renamed.old_path = Some("src/a.rs".into());
        assert_eq!(
            diff_for(&t, pair(None, 3), Some(&renamed)).2,
            "src/new.rs".to_owned()
        );
    }
}
