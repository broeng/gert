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
        /// `ctrl-,` / `cmd-,`: open the settings page.
        OpenSettings,
        /// `g`: the Go to dialog (open a change from a link to it).
        OpenGoto,
        // Settings page.
        /// `Enter` / `e`: edit the selected saved query.
        EditQuery,
        /// `a`: add a saved query.
        AddQuery,
        /// `d`: delete the selected saved query (asks first).
        DeleteQuery,
        MoveQueryDown,
        MoveQueryUp,
        /// `space`: switch the startup page, or toggle advance-after-vote.
        ToggleSetting,
        /// `y` / `Enter` while asked to delete.
        ConfirmDelete,
        /// `n` / `Esc` while asked to delete.
        CancelDelete,
        /// `Esc` while editing a saved query.
        CancelEdit,
        /// `Tab` / `Shift-Tab` while editing: the other field.
        NextField,
        PrevField,
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
        /// `shift-j` / `shift-k`: open the next / previous change of the
        /// relation chain (down / up the list).
        NextRelated,
        PrevRelated,
        /// `shift-l`: show the relation chain as a side panel, or as the
        /// capped box again.
        ToggleRelatedPanel,
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
        /// `a`: open the reply dialog (message, votes, drafts, attention set).
        Reply,
        /// `m` on the change page: open the change action menu (submit,
        /// rebase, abandon, ...).
        ChangeMenu,
        /// `r`: mark the file under the cursor (change page) or shown (diff
        /// page) as reviewed, or take the mark off again.
        ToggleReviewed,
        /// `f m` on the change page: open the Manage filters dialog.
        ManageFilters,
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
        PrefsSyntax,
    ]
);

/// `shift-v` and `v 1` .. `v 9` on the change page: cast the quick vote
/// of the header button at `position` (0 is the first).
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = ui, no_json)]
pub struct QuickVote {
    pub position: usize,
}

/// `f 1` .. `f 9` on the change page: toggle the file filter button at
/// `position` (0 is the first rule that applies to the change).
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = ui, no_json)]
pub struct ToggleFilter {
    pub position: usize,
}

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
const SHELL_KEYS: &str =
    "Shell && !Input && !ReplyDialog && !ActionDialog && !FiltersDialog && !GotoDialog";
const LIST_KEYS: &str = "ChangeList && !Input";
const CHANGE_KEYS: &str = "ChangePage && !Input";
const DIFF_PAGE_KEYS: &str = "DiffPage && !Input";
const DIFF_PREFS_KEYS: &str = "DiffPrefs && !Input";
const SETTINGS_BROWSE: &str = "SettingsPage && Browsing";
const SETTINGS_CONFIRM: &str = "SettingsPage && Confirming";
const SETTINGS_EDIT: &str = "SettingsPage && Editing";
/// As deep as the input's own context, to win over its Tab / Escape (like
/// the reply dialog's fields).
const SETTINGS_FIELD: &str = "Editing > Input";
/// Opening the settings works from anywhere but the modal dialogs.
const SETTINGS_OPEN: &str =
    "Shell && !ReplyDialog && !ActionDialog && !FiltersDialog && !GotoDialog";

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
        g("Settings page", settings_entries()),
        g("In a diff", crate::diff::actions::entries()),
        g("Writing a comment", crate::diff::actions::editor_entries()),
        g("Reply dialog ( A )", crate::reply::actions::entries()),
        g(
            "Change actions ( M )",
            crate::action_menu::actions::entries(),
        ),
        g(
            "Manage filters ( F M )",
            crate::filters_dialog::actions::entries(),
        ),
        g("Diff preferences panel ( , )", diff_prefs_entries()),
        g("Go to a change ( G )", crate::goto::entries()),
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
            "Next / previous file that is not dimmed",
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
        // (`shift-j` / `shift-k` open the related changes).
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
            "A",
            KeyBinding::new("a", Reply, Some(k)),
            "Reply: message, votes, drafts, attention set",
        ),
        e(
            "M",
            KeyBinding::new("m", ChangeMenu, Some(k)),
            "Change actions: submit, rebase, abandon, restore, WIP, private",
        ),
        e(
            "R",
            KeyBinding::new("r", ToggleReviewed, Some(k)),
            "Mark the file under the cursor reviewed / not reviewed",
        ),
        e(
            "Shift-J / Shift-K",
            KeyBinding::new("shift-j", NextRelated, Some(k)),
            "Open the next / previous change of the relation chain (down / up the list: towards the parent / the child)",
        ),
        e("", KeyBinding::new("shift-k", PrevRelated, Some(k)), ""),
        // `shift-r` is Refresh, hence `l`: the chain as a list.
        e(
            "Shift-L",
            KeyBinding::new("shift-l", ToggleRelatedPanel, Some(k)),
            "Show the relation chain as a side panel / back in the box",
        ),
        e(
            "Shift-V, V 1 to V 9",
            KeyBinding::new("shift-v", QuickVote { position: 0 }, Some(k)),
            "Quick vote: the first button of the header (normally Code-Review +2) / the button with that number",
        ),
        e(
            "",
            KeyBinding::new("v 1", QuickVote { position: 0 }, Some(k)),
            "",
        ),
        e(
            "",
            KeyBinding::new("v 2", QuickVote { position: 1 }, Some(k)),
            "",
        ),
        e(
            "",
            KeyBinding::new("v 3", QuickVote { position: 2 }, Some(k)),
            "",
        ),
        e(
            "",
            KeyBinding::new("v 4", QuickVote { position: 3 }, Some(k)),
            "",
        ),
        e(
            "",
            KeyBinding::new("v 5", QuickVote { position: 4 }, Some(k)),
            "",
        ),
        e(
            "",
            KeyBinding::new("v 6", QuickVote { position: 5 }, Some(k)),
            "",
        ),
        e(
            "",
            KeyBinding::new("v 7", QuickVote { position: 6 }, Some(k)),
            "",
        ),
        e(
            "",
            KeyBinding::new("v 8", QuickVote { position: 7 }, Some(k)),
            "",
        ),
        e(
            "",
            KeyBinding::new("v 9", QuickVote { position: 8 }, Some(k)),
            "",
        ),
        e(
            "F 1 to F 9",
            KeyBinding::new("f 1", ToggleFilter { position: 0 }, Some(k)),
            "Toggle the file filter with that number (hide, dim or highlight files)",
        ),
        e(
            "",
            KeyBinding::new("f 2", ToggleFilter { position: 1 }, Some(k)),
            "",
        ),
        e(
            "",
            KeyBinding::new("f 3", ToggleFilter { position: 2 }, Some(k)),
            "",
        ),
        e(
            "",
            KeyBinding::new("f 4", ToggleFilter { position: 3 }, Some(k)),
            "",
        ),
        e(
            "",
            KeyBinding::new("f 5", ToggleFilter { position: 4 }, Some(k)),
            "",
        ),
        e(
            "",
            KeyBinding::new("f 6", ToggleFilter { position: 5 }, Some(k)),
            "",
        ),
        e(
            "",
            KeyBinding::new("f 7", ToggleFilter { position: 6 }, Some(k)),
            "",
        ),
        e(
            "",
            KeyBinding::new("f 8", ToggleFilter { position: 7 }, Some(k)),
            "",
        ),
        e(
            "",
            KeyBinding::new("f 9", ToggleFilter { position: 8 }, Some(k)),
            "",
        ),
        e(
            "F M",
            KeyBinding::new("f m", ManageFilters, Some(k)),
            "Manage file filters",
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
            "Next / previous file that is not hidden or dimmed; ] on the last file goes back to the change",
        ),
        e("", KeyBinding::new("[", PrevFile, Some(k)), ""),
        e(
            "A",
            KeyBinding::new("a", Reply, Some(k)),
            "Reply: message, votes, attention set",
        ),
        // `m` toggles side-by-side / unified in the diff, and `shift-m` is
        // its older alias, so the menu gets `shift-a` ("actions"), which
        // nothing in the diff uses (`a` is the reply dialog).
        e(
            "Shift-A",
            KeyBinding::new("shift-a", ChangeMenu, Some(k)),
            "Change actions: submit, rebase, abandon, restore, WIP, private, reviewers",
        ),
        e(
            "R",
            KeyBinding::new("r", ToggleReviewed, Some(k)),
            "Mark the file reviewed / not reviewed",
        ),
        e(
            ",",
            KeyBinding::new(",", TogglePrefs, Some(k)),
            "Diff preferences",
        ),
        // Also in the preferences panel; here it needs no panel first.
        e(
            "Y",
            KeyBinding::new("y", PrefsSyntax, Some(k)),
            "Syntax highlighting on / off",
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
            "y",
            KeyBinding::new("y", PrefsSyntax, Some(k)),
            "Syntax highlighting",
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

/// Entries of the settings page (saved queries, startup page, review flow and
/// file filters).
pub fn settings_entries() -> Vec<Entry> {
    let b = SETTINGS_BROWSE;
    vec![
        e(
            "j / k",
            KeyBinding::new("j", SelectNext, Some(b)),
            "Next / previous row",
        ),
        e("", KeyBinding::new("k", SelectPrev, Some(b)), ""),
        e("", KeyBinding::new("down", SelectNext, Some(b)), ""),
        e("", KeyBinding::new("up", SelectPrev, Some(b)), ""),
        e(
            "Enter / e",
            KeyBinding::new("enter", EditQuery, Some(b)),
            "Edit the query (Tab: other field, Enter: save, Esc: cancel); on a file filter: open the dialog on it",
        ),
        e("", KeyBinding::new("e", EditQuery, Some(b)), ""),
        e(
            "Space",
            KeyBinding::new("space", ToggleSetting, Some(b)),
            "Startup page: dashboard or search; advance after a vote: on or off",
        ),
        e(
            "a",
            KeyBinding::new("a", AddQuery, Some(b)),
            "Add a query; in File filters: a new rule in the dialog",
        ),
        e(
            "d",
            KeyBinding::new("d", DeleteQuery, Some(b)),
            "Delete the query (y / Enter: yes, n / Esc: no); on a file filter: open the dialog on it",
        ),
        e(
            "Shift-J / Shift-K",
            KeyBinding::new("shift-j", MoveQueryDown, Some(b)),
            "Move the query down / up",
        ),
        e("", KeyBinding::new("shift-k", MoveQueryUp, Some(b)), ""),
        e(
            "",
            KeyBinding::new("y", ConfirmDelete, Some(SETTINGS_CONFIRM)),
            "",
        ),
        e(
            "",
            KeyBinding::new("enter", ConfirmDelete, Some(SETTINGS_CONFIRM)),
            "",
        ),
        e(
            "",
            KeyBinding::new("n", CancelDelete, Some(SETTINGS_CONFIRM)),
            "",
        ),
        e(
            "",
            KeyBinding::new("escape", CancelDelete, Some(SETTINGS_CONFIRM)),
            "",
        ),
        e(
            "",
            KeyBinding::new("escape", CancelEdit, Some(SETTINGS_EDIT)),
            "",
        ),
        e(
            "",
            KeyBinding::new("escape", CancelEdit, Some(SETTINGS_FIELD)),
            "",
        ),
        e(
            "",
            KeyBinding::new("tab", NextField, Some(SETTINGS_FIELD)),
            "",
        ),
        e(
            "",
            KeyBinding::new("shift-tab", PrevField, Some(SETTINGS_FIELD)),
            "",
        ),
    ]
}

/// Entries that work on every page.
pub fn global_entries() -> Vec<Entry> {
    vec![
        e(
            if cfg!(target_os = "macos") {
                "Cmd-,"
            } else {
                "Ctrl-,"
            },
            KeyBinding::new("secondary-,", OpenSettings, Some(SETTINGS_OPEN)),
            "Open the settings",
        ),
        e(
            "u",
            KeyBinding::new("u", GoUp, Some(SHELL_KEYS)),
            "Up: from a diff to its change, from a change to its list",
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
            "G",
            KeyBinding::new("g", OpenGoto, Some(SHELL_KEYS)),
            "Go to a change: paste a link to it from the web UI (the clipboard is filled in)",
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
            .chain(settings_entries())
            .map(|e| e.binding),
    );
}

#[cfg(test)]
mod tests {
    use super::{
        Entry, change_entries, diff_page_entries, diff_prefs_entries, entries, help_groups,
        settings_entries,
    };

    /// Every binding the app registers (see `bind_keys` here and in
    /// `diff::actions`).
    fn bound() -> Vec<Entry> {
        entries()
            .into_iter()
            .chain(change_entries())
            .chain(diff_page_entries())
            .chain(diff_prefs_entries())
            .chain(settings_entries())
            .chain(crate::diff::actions::entries())
            .chain(crate::diff::actions::editor_entries())
            .chain(crate::reply::actions::entries())
            .chain(crate::action_menu::actions::entries())
            .chain(crate::filters_dialog::actions::entries())
            .chain(crate::goto::entries())
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
