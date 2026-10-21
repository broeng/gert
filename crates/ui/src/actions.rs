//! Actions and the key bindings that trigger them. Bindings mirror the
//! Gerrit web UI where it has an equivalent.

use gpui_kit::*;

use crate::diff::actions::{NextThread, PrevThread};

actions!(
    ui,
    [
        SelectNext,
        SelectPrev,
        OpenSelected,
        NextPage,
        PrevPage,
        Refresh,
        OpenInBrowser,
        GoBack,
        GoForward,
        GoUp,
        FocusSearch,
        BlurSearch,
        ToggleHelp,
        Dismiss,
        // Change page.
        /// Show or hide the inline diff of the file under the cursor.
        ToggleInline,
        /// Show the inline diff of every file, or hide them all.
        ToggleAllInline,
        NextTarget,
        PrevTarget,
        NextBase,
        PrevBase,
        ToggleCommit,
        NextRelated,
        PrevRelated,
        OpenRelated,
        /// Esc on the change page: give the focus back to the file list.
        FocusFiles,
        /// `x`: open the comment thread under the thread cursor (its diff,
        /// or for a patch-set level thread its card on the page).
        OpenThread,
        /// `shift-u` / `shift-d` / `shift-a`: list only unresolved threads,
        /// only threads with a draft of yours, or all threads.
        ThreadsUnresolved,
        ThreadsDrafts,
        ThreadsAll,
        /// `c` on the change page: a new patch-set level comment.
        NewPatchSetComment,
        ScrollPageDown,
        ScrollPageUp,
        // Diff page.
        /// Show or hide the diff preferences panel.
        TogglePrefs,
        ClosePrefs,
        PrefsMode,
        PrefsContext,
        PrefsWhitespace,
        PrefsTabSize,
        PrefsShowTabs,
        PrefsIntraline,
    ]
);

/// Key context of the root view.
pub const SHELL: &str = "Shell";
/// Key context of the dashboard and search result pages.
pub const CHANGE_LIST: &str = "ChangeList";
/// Key context of the change page.
pub const CHANGE_PAGE: &str = "ChangePage";
/// Key context of the diff page (around the diff view).
pub const DIFF_PAGE: &str = "DiffPage";
/// Key context of the diff preferences panel.
pub const DIFF_PREFS: &str = "DiffPrefs";
/// Key context around the search box.
pub const SEARCH_BAR: &str = "SearchBar";

// Single-letter bindings must not fire while text is being typed; the text
// input marks its key context "Input".
const SHELL_KEYS: &str = "Shell && !Input";
const LIST_KEYS: &str = "ChangeList && !Input";
const CHANGE_KEYS: &str = "ChangePage && !Input";
const DIFF_PAGE_KEYS: &str = "DiffPage && !Input";
const DIFF_PREFS_KEYS: &str = "DiffPrefs && !Input";

/// All shell-level and list entries (what the help overlay called
/// "everything" before the pages grew their own).
pub fn entries() -> Vec<Entry> {
    list_entries().into_iter().chain(global_entries()).collect()
}

/// A titled section of the help overlay.
pub struct HelpGroup {
    pub title: &'static str,
    pub entries: Vec<Entry>,
}

/// The help overlay, grouped by where the keys apply.
pub fn help_groups() -> Vec<HelpGroup> {
    let g = |title, entries| HelpGroup { title, entries };
    vec![
        g("Everywhere", global_entries()),
        g("Change lists", list_entries()),
        g("Change page", change_entries()),
        g("Diff page", diff_page_entries()),
        g("In a diff", crate::diff::actions::entries()),
        g("Writing a comment", crate::diff::actions::editor_entries()),
        g("Diff preferences panel ( , )", diff_prefs_entries()),
    ]
}

/// Entries of the change page (file cursor, inline diffs, patch sets).
pub fn change_entries() -> Vec<Entry> {
    use crate::diff::actions::{NextFile, PrevFile};
    let k = CHANGE_KEYS;
    vec![
        e(
            "j / k",
            KeyBinding::new("j", SelectNext, Some(k)),
            "Next / previous file",
        ),
        e("", KeyBinding::new("k", SelectPrev, Some(k)), ""),
        e("", KeyBinding::new("down", SelectNext, Some(k)), ""),
        e("", KeyBinding::new("up", SelectPrev, Some(k)), ""),
        e(
            "[ / ]",
            KeyBinding::new("]", NextFile, Some(k)),
            "Next / previous file",
        ),
        e("", KeyBinding::new("[", PrevFile, Some(k)), ""),
        e(
            "o / Enter",
            KeyBinding::new("o", OpenSelected, Some(k)),
            "Open the file's diff",
        ),
        e("", KeyBinding::new("enter", OpenSelected, Some(k)), ""),
        e(
            "i",
            KeyBinding::new("i", ToggleInline, Some(k)),
            "Show / hide the inline diff of the file",
        ),
        e(
            "Shift-I",
            KeyBinding::new("shift-i", ToggleAllInline, Some(k)),
            "Show / hide all inline diffs",
        ),
        e(
            "t / Shift-T",
            KeyBinding::new("t", NextTarget, Some(k)),
            "Next / previous patch set to show",
        ),
        e("", KeyBinding::new("shift-t", PrevTarget, Some(k)), ""),
        e(
            "b / Shift-B",
            KeyBinding::new("b", NextBase, Some(k)),
            "Next / previous patch set to compare with",
        ),
        e("", KeyBinding::new("shift-b", PrevBase, Some(k)), ""),
        e(
            "z",
            KeyBinding::new("z", ToggleCommit, Some(k)),
            "Fold / unfold the commit message",
        ),
        // The comment thread list. `shift-n` / `shift-p` are the diff's
        // next / previous thread keys (PolyGerrit's `N` / `P`); `x` opens a
        // thread like it does in the diff. `Enter` stays with the files
        // (as `shift-enter` does with the related changes).
        e(
            "Shift-N / Shift-P",
            KeyBinding::new("shift-n", NextThread, Some(k)),
            "Next / previous comment thread in the list",
        ),
        e("", KeyBinding::new("shift-p", PrevThread, Some(k)), ""),
        e(
            "X",
            KeyBinding::new("x", OpenThread, Some(k)),
            "Open the selected thread: its diff, or the card below for a patch-set comment",
        ),
        e(
            "Shift-U / Shift-D / Shift-A",
            KeyBinding::new("shift-u", ThreadsUnresolved, Some(k)),
            "List unresolved threads / threads with your drafts / all threads",
        ),
        e("", KeyBinding::new("shift-d", ThreadsDrafts, Some(k)), ""),
        e("", KeyBinding::new("shift-a", ThreadsAll, Some(k)), ""),
        e(
            "C",
            KeyBinding::new("c", NewPatchSetComment, Some(k)),
            "New patch-set level comment",
        ),
        e(
            "Shift-J / Shift-K",
            KeyBinding::new("shift-j", NextRelated, Some(k)),
            "Next / previous related change",
        ),
        e("", KeyBinding::new("shift-k", PrevRelated, Some(k)), ""),
        e(
            "Shift-Enter",
            KeyBinding::new("shift-enter", OpenRelated, Some(k)),
            "Open the selected related change",
        ),
        e(
            "Space / Shift-Space",
            KeyBinding::new("space", ScrollPageDown, Some(k)),
            "Scroll a page down / up",
        ),
        e(
            "",
            KeyBinding::new("shift-space", ScrollPageUp, Some(k)),
            "",
        ),
        e("", KeyBinding::new("pagedown", ScrollPageDown, Some(k)), ""),
        e("", KeyBinding::new("pageup", ScrollPageUp, Some(k)), ""),
        e(
            "Esc",
            KeyBinding::new("escape", FocusFiles, Some(k)),
            "Back to the file list (from an inline diff)",
        ),
    ]
}

/// Entries of the diff page itself; the keys inside the diff are in
/// [`crate::diff::actions::entries`].
pub fn diff_page_entries() -> Vec<Entry> {
    use crate::diff::actions::{NextFile, PrevFile};
    let k = DIFF_PAGE_KEYS;
    vec![
        e(
            "] / [",
            KeyBinding::new("]", NextFile, Some(k)),
            "Next / previous file",
        ),
        e("", KeyBinding::new("[", PrevFile, Some(k)), ""),
        e(
            ",",
            KeyBinding::new(",", TogglePrefs, Some(k)),
            "Diff preferences",
        ),
    ]
}

/// Entries of the diff preferences panel.
pub fn diff_prefs_entries() -> Vec<Entry> {
    let k = DIFF_PREFS_KEYS;
    vec![
        e(
            "m",
            KeyBinding::new("m", PrefsMode, Some(k)),
            "Side-by-side / unified",
        ),
        e(
            "c",
            KeyBinding::new("c", PrefsContext, Some(k)),
            "Context lines",
        ),
        e(
            "w",
            KeyBinding::new("w", PrefsWhitespace, Some(k)),
            "Whitespace handling",
        ),
        e(
            "t",
            KeyBinding::new("t", PrefsTabSize, Some(k)),
            "Tab width",
        ),
        e(
            "s",
            KeyBinding::new("s", PrefsShowTabs, Some(k)),
            "Show tab characters",
        ),
        e(
            "i",
            KeyBinding::new("i", PrefsIntraline, Some(k)),
            "Highlight changes within a line",
        ),
        e(
            "Esc or ,",
            KeyBinding::new("escape", ClosePrefs, Some(k)),
            "Close the panel",
        ),
        e("", KeyBinding::new(",", ClosePrefs, Some(k)), ""),
    ]
}

/// A binding and how it appears in the help overlay.
pub struct Entry {
    pub binding: KeyBinding,
    /// What the help overlay shows; empty for aliases that are not listed.
    pub keys: &'static str,
    pub description: &'static str,
}

fn e(keys: &'static str, binding: KeyBinding, description: &'static str) -> Entry {
    Entry {
        binding,
        keys,
        description,
    }
}

/// Entries of the change lists (dashboard and search), plus the refresh
/// and browser keys the change page shares.
pub fn list_entries() -> Vec<Entry> {
    vec![
        e(
            "j / k",
            KeyBinding::new("j", SelectNext, Some(LIST_KEYS)),
            "Next / previous change",
        ),
        e("", KeyBinding::new("k", SelectPrev, Some(LIST_KEYS)), ""),
        e("", KeyBinding::new("down", SelectNext, Some(LIST_KEYS)), ""),
        e("", KeyBinding::new("up", SelectPrev, Some(LIST_KEYS)), ""),
        e(
            "o / Enter",
            KeyBinding::new("o", OpenSelected, Some(LIST_KEYS)),
            "Open the selected change",
        ),
        e(
            "",
            KeyBinding::new("enter", OpenSelected, Some(LIST_KEYS)),
            "",
        ),
        e(
            "n / p",
            KeyBinding::new("n", NextPage, Some(LIST_KEYS)),
            "Next / previous page of search results",
        ),
        e("", KeyBinding::new("p", PrevPage, Some(LIST_KEYS)), ""),
        e(
            "Shift-R",
            KeyBinding::new("shift-r", Refresh, Some(LIST_KEYS)),
            "Refresh now",
        ),
        e(
            "",
            KeyBinding::new("shift-r", Refresh, Some(CHANGE_KEYS)),
            "",
        ),
        e(
            "Shift-O",
            KeyBinding::new("shift-o", OpenInBrowser, Some(LIST_KEYS)),
            "Open the change in the web UI",
        ),
        e(
            "",
            KeyBinding::new("shift-o", OpenInBrowser, Some(CHANGE_KEYS)),
            "",
        ),
    ]
}

/// Entries that work on every page.
pub fn global_entries() -> Vec<Entry> {
    vec![
        e(
            "u",
            KeyBinding::new("u", GoUp, Some(SHELL_KEYS)),
            "Up: to the list, or from a diff to its change",
        ),
        e(
            "Alt-Left / Alt-Right",
            KeyBinding::new("alt-left", GoBack, Some(SHELL_KEYS)),
            "Back / forward",
        ),
        e(
            "",
            KeyBinding::new("alt-right", GoForward, Some(SHELL_KEYS)),
            "",
        ),
        e(
            "/",
            KeyBinding::new("/", FocusSearch, Some(SHELL_KEYS)),
            "Search",
        ),
        e(
            "?",
            KeyBinding::new("?", ToggleHelp, Some(SHELL_KEYS)),
            "Show or hide this help",
        ),
        e(
            "",
            KeyBinding::new("shift-/", ToggleHelp, Some(SHELL_KEYS)),
            "",
        ),
        e(
            "Esc",
            KeyBinding::new("escape", Dismiss, Some(SHELL_KEYS)),
            "Close help",
        ),
        e(
            "",
            KeyBinding::new("escape", BlurSearch, Some(SEARCH_BAR)),
            "",
        ),
    ]
}

/// Registers all bindings. Call once at startup.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(
        entries()
            .into_iter()
            .chain(change_entries())
            .chain(diff_page_entries())
            .chain(diff_prefs_entries())
            .map(|e| e.binding),
    );
}

#[cfg(test)]
mod tests {
    use super::{
        Entry, change_entries, diff_page_entries, diff_prefs_entries, entries, help_groups,
    };

    /// Every binding the app registers (see `bind_keys` here and in
    /// `diff::actions`).
    fn bound() -> Vec<Entry> {
        entries()
            .into_iter()
            .chain(change_entries())
            .chain(diff_page_entries())
            .chain(diff_prefs_entries())
            .chain(crate::diff::actions::entries())
            .chain(crate::diff::actions::editor_entries())
            .collect()
    }

    /// The help overlay lists every action that has a binding. (An entry
    /// without a key label is the alias of the labelled one before it; the
    /// next test checks that.)
    #[test]
    fn every_bound_action_is_in_the_help_overlay() {
        let listed: std::collections::HashSet<&str> = help_groups()
            .iter()
            .flat_map(|g| &g.entries)
            .map(|e| e.binding.action().name())
            .collect();
        let missing: Vec<&str> = bound()
            .iter()
            .map(|e| e.binding.action().name())
            .filter(|name| !listed.contains(name))
            .collect();
        assert!(missing.is_empty(), "no help entry for {missing:?}");
        // And the overlay lists nothing that is not bound.
        let bound_names: std::collections::HashSet<&str> =
            bound().iter().map(|e| e.binding.action().name()).collect();
        for g in help_groups() {
            for e in g.entries {
                assert!(bound_names.contains(e.binding.action().name()));
            }
        }
    }

    /// An entry has either a key label and a description, or neither (an
    /// alias of the entry before it, so the first of a group has them).
    #[test]
    fn aliases_follow_a_labelled_entry() {
        for g in help_groups() {
            let first = g.entries.first().expect("a group has entries");
            assert!(
                !first.keys.is_empty() && !first.description.is_empty(),
                "{}",
                g.title
            );
            for e in g.entries {
                assert!(
                    !e.keys.is_empty() || e.description.is_empty(),
                    "{} has a description but no keys",
                    e.binding.action().name()
                );
            }
        }
    }
}
