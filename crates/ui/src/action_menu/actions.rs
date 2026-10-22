//! Actions and key bindings of the change action menu and its dialogs. The
//! keys apply inside the dialog's key contexts only; the page key that
//! opens it (`m`) is in [`crate::actions`].

use gpui_kit::*;

use crate::actions::Entry;

actions!(
    action_menu,
    [
        /// Esc: close the menu or the dialog.
        CloseMenu,
        /// Ctrl-Enter / Cmd-Enter: run the action in a confirmation dialog.
        ConfirmAction,
        /// Tab / Shift-Tab: the next / previous control.
        MenuNext,
        MenuPrev,
        /// `j` / Down and `k` / Up in the menu.
        MenuDown,
        MenuUp,
        /// Enter in the menu: pick the selected entry.
        MenuRun,
        PickSubmit,
        PickRebase,
        PickAbandon,
        PickRestore,
        PickWip,
        PickPrivate,
    ]
);

/// Key context of the menu and of every dialog step.
pub const ACTION_DIALOG: &str = "ActionDialog";
/// Added to the context while the menu itself (not a confirmation) shows.
pub const ACTION_MENU: &str = "ActionMenu";

/// The text fields give Tab up instead of indenting: this binding is as
/// deep as the field's own (`Input`) and registered after it, so it wins.
const IN_FIELD: &str = "ActionDialog > Input";

pub fn entries() -> Vec<Entry> {
    fn e(
        keys: &'static str,
        key: &'static str,
        action: impl Action,
        context: &'static str,
        description: &'static str,
    ) -> Entry {
        Entry {
            binding: KeyBinding::new(key, action, Some(context)),
            keys,
            description,
        }
    }
    let m = ACTION_MENU;
    vec![
        e(
            "J / K / Down / Up",
            "j",
            MenuDown,
            m,
            "In the menu: next / previous entry",
        ),
        e("", "down", MenuDown, m, ""),
        e("", "k", MenuUp, m, ""),
        e("", "up", MenuUp, m, ""),
        e("Enter", "enter", MenuRun, m, "In the menu: run the entry"),
        e("S", "s", PickSubmit, m, "Submit"),
        e("B", "b", PickRebase, m, "Rebase"),
        e("A", "a", PickAbandon, m, "Abandon"),
        e("R", "r", PickRestore, m, "Restore"),
        e(
            "W",
            "w",
            PickWip,
            m,
            "Mark work in progress / ready for review",
        ),
        e("P", "p", PickPrivate, m, "Mark private / not private"),
        e(
            if cfg!(target_os = "macos") {
                "Cmd-Enter"
            } else {
                "Ctrl-Enter"
            },
            "secondary-enter",
            ConfirmAction,
            ACTION_DIALOG,
            "In a confirmation: do it",
        ),
        e(
            "Esc",
            "escape",
            CloseMenu,
            ACTION_DIALOG,
            "Close the menu, or cancel",
        ),
        e(
            "Tab / Shift-Tab",
            "tab",
            MenuNext,
            ACTION_DIALOG,
            "Next / previous control",
        ),
        e("", "shift-tab", MenuPrev, ACTION_DIALOG, ""),
        e("", "tab", MenuNext, IN_FIELD, ""),
        e("", "shift-tab", MenuPrev, IN_FIELD, ""),
    ]
}

/// Registers the dialog's bindings. Call once at startup, after the
/// component library's own (so that Tab in a field is ours).
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(entries().into_iter().map(|e| e.binding));
}
