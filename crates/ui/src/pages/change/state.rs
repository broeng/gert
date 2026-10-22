//! The pure parts of the change page: which patch sets are compared, which
//! blocks the page consists of, and moving the file cursor.

use model::{ChangeDetail, FilesKey, PatchSetNumber, PatchSetRef};

/// The patch set pair being compared, as the user picked it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PatchSel {
    /// `None` compares with the parent commit ("Base").
    pub base: Option<PatchSetNumber>,
    /// `None` follows the current patch set.
    pub target: Option<PatchSetNumber>,
}

impl PatchSel {
    /// The patch set shown: the pick if the change has it, else the current
    /// one.
    pub fn target_number(&self, d: &ChangeDetail) -> Option<PatchSetNumber> {
        self.target
            .filter(|n| d.revision(*n).is_some())
            .or(d.current_patch_set)
    }

    /// The base shown: the pick if it exists and is older than the target.
    pub fn base_number(&self, d: &ChangeDetail) -> Option<PatchSetNumber> {
        let target = self.target_number(d)?;
        self.base
            .filter(|b| *b < target && d.revision(*b).is_some())
    }

    pub fn target_ref(&self, d: &ChangeDetail) -> Option<PatchSetRef> {
        Some(d.revision(self.target_number(d)?)?.as_ref())
    }

    pub fn base_ref(&self, d: &ChangeDetail) -> Option<PatchSetRef> {
        Some(d.revision(self.base_number(d)?)?.as_ref())
    }

    /// The file list to show; `None` until the change lists its patch sets.
    pub fn files_key(&self, d: &ChangeDetail) -> Option<FilesKey> {
        Some(FilesKey {
            change: d.summary.number,
            base: self.base_ref(d),
            target: self.target_ref(d)?,
        })
    }

    /// Moves the target to the next / previous patch set, wrapping. A base
    /// that is no longer older than the target goes back to "Base".
    pub fn cycle_target(&mut self, d: &ChangeDetail, forward: bool) {
        let Some(current) = self.target_number(d) else {
            return;
        };
        let numbers: Vec<_> = d.revisions.iter().map(|r| r.number).collect();
        let Some(next) = cycle(&numbers, Some(current), forward) else {
            return;
        };
        self.target = if Some(next) == d.current_patch_set {
            None
        } else {
            Some(next)
        };
        self.base = self.base_number(d);
    }

    /// Moves the base through "Base" and the patch sets older than the
    /// target, wrapping.
    pub fn cycle_base(&mut self, d: &ChangeDetail, forward: bool) {
        let Some(target) = self.target_number(d) else {
            return;
        };
        let mut options: Vec<Option<PatchSetNumber>> = vec![None];
        options.extend(
            d.revisions
                .iter()
                .map(|r| r.number)
                .filter(|n| *n < target)
                .map(Some),
        );
        let current = self.base_number(d);
        let at = options.iter().position(|o| *o == current).unwrap_or(0);
        let len = options.len();
        let next = if forward {
            (at + 1) % len
        } else {
            (at + len - 1) % len
        };
        self.base = options[next];
    }
}

/// The element after / before `current` in `items`, wrapping. A `current`
/// that is not in the list yields the first / last.
fn cycle<T: Copy + PartialEq>(items: &[T], current: Option<T>, forward: bool) -> Option<T> {
    if items.is_empty() {
        return None;
    }
    let len = items.len();
    let at = current.and_then(|c| items.iter().position(|i| *i == c));
    let next = match (at, forward) {
        (Some(i), true) => (i + 1) % len,
        (Some(i), false) => (i + len - 1) % len,
        (None, true) => 0,
        (None, false) => len - 1,
    };
    Some(items[next])
}

/// The pieces of the page, top to bottom. The page is a virtualized list of
/// these, so a change with thousands of files costs only the visible ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Block {
    /// The header bar and the three columns under it: change info, commit
    /// message and relation chain.
    Header,
    /// Patch set selector and the "Files" heading.
    PatchSets,
    /// "Loading files", an error or "no files" in place of the rows.
    FilesNotice,
    File(usize),
    /// "Comments" heading with the filter.
    ThreadsHeader,
    /// "No unresolved comments" and the like, in place of the rows.
    ThreadsNotice,
    /// A row of the thread list: an index into the filtered threads.
    Thread(usize),
    /// The patch-set level threads, as cards, and the button for a new one.
    PatchSetComments,
    LogHeader,
    Message(usize),
}

/// What the page has to show, as counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Content {
    pub loaded: bool,
    pub files: usize,
    /// Threads the list shows (after the filter).
    pub threads: usize,
    pub messages: usize,
}

pub fn blocks(c: Content) -> Vec<Block> {
    let mut v = vec![Block::Header];
    if !c.loaded {
        return v;
    }
    v.push(Block::PatchSets);
    if c.files == 0 {
        v.push(Block::FilesNotice);
    }
    v.extend((0..c.files).map(Block::File));
    v.push(Block::ThreadsHeader);
    if c.threads == 0 {
        v.push(Block::ThreadsNotice);
    }
    v.extend((0..c.threads).map(Block::Thread));
    v.push(Block::PatchSetComments);
    if c.messages > 0 {
        v.push(Block::LogHeader);
        v.extend((0..c.messages).map(Block::Message));
    }
    v
}

/// The cursor after moving `delta` rows in a list of `len`, stopping at the
/// ends.
pub fn step(cursor: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    cursor.saturating_add_signed(delta).min(len - 1)
}

/// Compact byte size: `512 B`, `4.0 KiB`, `1.2 MiB`.
pub fn size_text(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if b < KIB * KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{:.1} MiB", b / KIB / KIB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::summary;
    use model::{Revision, RevisionId};

    fn detail(n: u32, current: u32) -> ChangeDetail {
        ChangeDetail {
            summary: summary(7),
            current_revision: Some(RevisionId::from(format!("sha{current}"))),
            current_patch_set: Some(PatchSetNumber(current)),
            commit_message: None,
            reviewers: vec![],
            cc: vec![],
            revisions: (1..=n)
                .map(|i| Revision {
                    number: PatchSetNumber(i),
                    sha: format!("sha{i}").into(),
                    kind: None,
                    created: None,
                    uploader: None,
                    ref_name: None,
                    description: None,
                    commit: None,
                })
                .collect(),
            messages: vec![],
            submit_requirements: vec![],
            votes: Default::default(),
            attention: Default::default(),
            permitted_labels: Default::default(),
            label_values: Default::default(),
            change_actions: Default::default(),
            revision_actions: Default::default(),
        }
    }

    fn ps(n: u32) -> Option<PatchSetNumber> {
        Some(PatchSetNumber(n))
    }

    #[test]
    fn default_is_base_against_current() {
        let d = detail(3, 3);
        let sel = PatchSel::default();
        let key = sel.files_key(&d).expect("key");
        assert_eq!(key.base, None);
        assert_eq!(key.target.number, PatchSetNumber(3));
        assert_eq!(key.target.sha.as_str(), "sha3");
    }

    #[test]
    fn stale_picks_fall_back() {
        let d = detail(3, 3);
        let sel = PatchSel {
            base: ps(3),
            target: ps(9),
        };
        assert_eq!(sel.target_number(&d), ps(3)); // 9 does not exist
        assert_eq!(sel.base_number(&d), None); // 3 is not older than 3
    }

    #[test]
    fn cycling_the_target_wraps_and_follows_current() {
        let d = detail(3, 3);
        let mut sel = PatchSel::default();
        sel.cycle_target(&d, false);
        assert_eq!(sel.target, ps(2));
        sel.cycle_target(&d, false);
        assert_eq!(sel.target, ps(1));
        sel.cycle_target(&d, false); // wraps to the last = current = None
        assert_eq!(sel.target, None);
        assert_eq!(sel.target_number(&d), ps(3));
        sel.cycle_target(&d, true); // wraps to the first
        assert_eq!(sel.target, ps(1));
    }

    #[test]
    fn lowering_the_target_resets_a_base_that_is_not_older() {
        let d = detail(4, 4);
        let mut sel = PatchSel {
            base: ps(3),
            target: None,
        };
        sel.cycle_target(&d, false); // target 3: base 3 is no longer older
        assert_eq!(sel.target, ps(3));
        assert_eq!(sel.base, None);
        let mut sel = PatchSel {
            base: ps(2),
            target: None,
        };
        sel.cycle_target(&d, false);
        assert_eq!(sel.base, ps(2)); // still older than 3
    }

    #[test]
    fn cycling_the_base_goes_through_base_and_older_patch_sets() {
        let d = detail(3, 3);
        let mut sel = PatchSel::default();
        sel.cycle_base(&d, true);
        assert_eq!(sel.base, ps(1));
        sel.cycle_base(&d, true);
        assert_eq!(sel.base, ps(2));
        sel.cycle_base(&d, true);
        assert_eq!(sel.base, None); // back to Base
        sel.cycle_base(&d, false);
        assert_eq!(sel.base, ps(2));
        // The first patch set has nothing older.
        let mut first = PatchSel {
            base: None,
            target: ps(1),
        };
        first.cycle_base(&d, true);
        assert_eq!(first.base, None);
    }

    #[test]
    fn no_revisions_no_key() {
        let mut d = detail(0, 1);
        d.current_patch_set = None;
        assert!(PatchSel::default().files_key(&d).is_none());
    }

    #[test]
    fn blocks_follow_the_content() {
        let none = blocks(Content::default());
        assert_eq!(none, [Block::Header]);
        let b = blocks(Content {
            loaded: true,
            files: 2,
            threads: 1,
            messages: 2,
        });
        assert_eq!(
            b,
            [
                Block::Header,
                Block::PatchSets,
                Block::File(0),
                Block::File(1),
                Block::ThreadsHeader,
                Block::Thread(0),
                Block::PatchSetComments,
                Block::LogHeader,
                Block::Message(0),
                Block::Message(1),
            ]
        );
        // No files: a notice.
        let b = blocks(Content {
            loaded: true,
            ..Default::default()
        });
        assert_eq!(
            b,
            [
                Block::Header,
                Block::PatchSets,
                Block::FilesNotice,
                Block::ThreadsHeader,
                Block::ThreadsNotice,
                Block::PatchSetComments,
            ]
        );
    }

    #[test]
    fn stepping_stops_at_the_ends() {
        assert_eq!(step(0, 3, -1), 0);
        assert_eq!(step(1, 3, 1), 2);
        assert_eq!(step(2, 3, 1), 2);
        assert_eq!(step(0, 0, 1), 0);
    }

    #[test]
    fn sizes() {
        assert_eq!(size_text(512), "512 B");
        assert_eq!(size_text(4096), "4.0 KiB");
        assert_eq!(size_text(3 * 1024 * 1024 / 2), "1.5 MiB");
    }
}
