//! Actions and key bindings of the Manage filters dialog. The keys apply
//! inside the dialog's key contexts only; the page keys that open it
//! (`f m`) are in [`crate::actions`].

use gpui_kit::*;

use crate::actions::Entry;

actions!(
    filters_dialog,
    [
        /// Esc: leave the form (discarding edits), cancel a delete, or close.
        CloseFilters,
        /// Ctrl-Enter / Cmd-Enter: save the form.
        SaveFilter,
        /// Tab / Shift-Tab: the next / previous form control.
        FilterNextField,
        FilterPrevField,
        /// `j` / `k` and Down / Up in the list.
        RuleDown,
        RuleUp,
        /// `a`: a new rule.
        RuleAdd,
        /// `Enter` / `e`: edit the selected rule.
        RuleEdit,
        /// `d`: delete the selected rule (asks first).
        RuleDelete,
        /// `y` / `n` at the question.
        RuleDeleteYes,
        RuleDeleteNo,
        /// `shift-j` / `shift-k`: reorder.
        RuleMoveDown,
        RuleMoveUp,
        /// `space`: the selected rule on or off by default.
        RuleToggleDefault,
        /// Left / Right on the action: Hide, Dim, Highlight.
        ActionPrev,
        ActionNext,
    ]
);

/// Key context of the dialog.
pub const FILTERS_DIALOG: &str = "FiltersDialog";
// The dialog adds "FiltersBrowsing" (the list has the keyboard),
// "FiltersEditing" (the form) or "FiltersConfirming" (a delete is asked
// about) to it. They are not "Browsing" and so on, which the settings page
// uses: its field bindings (`Editing > Input`) would reach into this dialog.
/// Around the Hide / Dim / Highlight choice.
pub const ACTION_GROUP: &str = "FilterActionGroup";

/// The text fields give Tab, Esc and Ctrl-Enter up instead of using them:
/// this binding is as deep as the field's own (`Input`) and registered after
/// it, so it wins.
const IN_FIELD: &str = "FiltersDialog > Input";

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
    let b = "FiltersDialog && FiltersBrowsing";
    let c = "FiltersDialog && FiltersConfirming";
    vec![
        e(
            "J / K / Down / Up",
            "j",
            RuleDown,
            b,
            "In the list: next / previous rule",
        ),
        e("", "down", RuleDown, b, ""),
        e("", "k", RuleUp, b, ""),
        e("", "up", RuleUp, b, ""),
        e(
            "Enter / E",
            "enter",
            RuleEdit,
            b,
            "Edit the rule (Tab: next field, Esc: discard)",
        ),
        e("", "e", RuleEdit, b, ""),
        e("A", "a", RuleAdd, b, "New rule (for the change's project)"),
        e(
            "D",
            "d",
            RuleDelete,
            b,
            "Delete the rule (Y / Enter: yes, N / Esc: no)",
        ),
        e(
            "Shift-J / Shift-K",
            "shift-j",
            RuleMoveDown,
            b,
            "Move the rule down / up (the order of the buttons)",
        ),
        e("", "shift-k", RuleMoveUp, b, ""),
        e(
            "Space",
            "space",
            RuleToggleDefault,
            b,
            "Switch the rule on or off by default",
        ),
        e("", "y", RuleDeleteYes, c, ""),
        e("", "enter", RuleDeleteYes, c, ""),
        e("", "n", RuleDeleteNo, c, ""),
        e(
            if cfg!(target_os = "macos") {
                "Cmd-Enter"
            } else {
                "Ctrl-Enter"
            },
            "secondary-enter",
            SaveFilter,
            FILTERS_DIALOG,
            "In the form: save the rule",
        ),
        e("", "secondary-enter", SaveFilter, IN_FIELD, ""),
        e(
            "Esc",
            "escape",
            CloseFilters,
            FILTERS_DIALOG,
            "Leave the form, or close the dialog",
        ),
        e("", "escape", CloseFilters, IN_FIELD, ""),
        e(
            "Tab / Shift-Tab",
            "tab",
            FilterNextField,
            FILTERS_DIALOG,
            "In the form: next / previous control",
        ),
        e("", "shift-tab", FilterPrevField, FILTERS_DIALOG, ""),
        e("", "tab", FilterNextField, IN_FIELD, ""),
        e("", "shift-tab", FilterPrevField, IN_FIELD, ""),
        e(
            "Left / Right",
            "left",
            ActionPrev,
            ACTION_GROUP,
            "On the action: Hide, Dim or Highlight",
        ),
        e("", "right", ActionNext, ACTION_GROUP, ""),
    ]
}

/// Registers the dialog's bindings. Call once at startup, after the
/// component library's own (so that Tab, Esc and Ctrl-Enter in a field are
/// ours).
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(entries().into_iter().map(|e| e.binding));
}
