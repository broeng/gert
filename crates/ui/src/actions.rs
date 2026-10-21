//! Actions and the key bindings that trigger them. Bindings mirror the
//! Gerrit web UI where it has an equivalent.

use gpui_kit::*;

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
    ]
);

/// Key context of the root view.
pub const SHELL: &str = "Shell";
/// Key context of the dashboard and search result pages.
pub const CHANGE_LIST: &str = "ChangeList";
/// Key context of the change page.
#[allow(dead_code)]
pub const CHANGE_PAGE: &str = "ChangePage";
/// Key context around the search box.
pub const SEARCH_BAR: &str = "SearchBar";

// Single-letter bindings must not fire while text is being typed; the text
// input marks its key context "Input".
const SHELL_KEYS: &str = "Shell && !Input";
const LIST_KEYS: &str = "ChangeList && !Input";
const CHANGE_KEYS: &str = "ChangePage && !Input";

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

pub fn entries() -> Vec<Entry> {
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
        e(
            "u",
            KeyBinding::new("u", GoUp, Some(SHELL_KEYS)),
            "Up to the list (or dashboard)",
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
    cx.bind_keys(entries().into_iter().map(|e| e.binding));
}
