//! Actions and key bindings of the project checkout dialog. The keys apply
//! inside the dialog's key context only.

use gpui_kit::*;

use crate::actions::Entry;

actions!(
    project_dialog,
    [
        /// Esc: discard the edits and close.
        CloseProject,
        /// Ctrl-Enter / Cmd-Enter (or Enter in a field): save.
        SaveProject,
        /// Tab / Shift-Tab: the next / previous control.
        ProjectNextField,
        ProjectPrevField,
        /// Up / Down on the vote strategy: the option above / below.
        ApprovalPrev,
        ApprovalNext,
    ]
);

/// Key context of the dialog.
pub const PROJECT_DIALOG: &str = "ProjectDialog";

/// Around the choice of what happens to votes on a local change.
pub const PROJECT_APPROVAL: &str = "ProjectApprovalGroup";

/// The text fields give Tab, Esc and Ctrl-Enter up instead of using them:
/// this binding is as deep as the field's own (`Input`) and registered after
/// it, so it wins.
const IN_FIELD: &str = "ProjectDialog > Input";

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
    vec![
        e(
            "Tab / Shift-Tab",
            "tab",
            ProjectNextField,
            PROJECT_DIALOG,
            "Next / previous control (Browse\u{2026} opens a folder picker)",
        ),
        e("", "shift-tab", ProjectPrevField, PROJECT_DIALOG, ""),
        e("", "tab", ProjectNextField, IN_FIELD, ""),
        e("", "shift-tab", ProjectPrevField, IN_FIELD, ""),
        e(
            "Up / Down",
            "up",
            ApprovalPrev,
            PROJECT_APPROVAL,
            "On the vote choice: the option above / below (Space ticks the local changes box)",
        ),
        e("", "down", ApprovalNext, PROJECT_APPROVAL, ""),
        e(
            if cfg!(target_os = "macos") {
                "Cmd-Enter"
            } else {
                "Ctrl-Enter"
            },
            "secondary-enter",
            SaveProject,
            PROJECT_DIALOG,
            "Save the checkout (Enter in a field does too)",
        ),
        e("", "secondary-enter", SaveProject, IN_FIELD, ""),
        e(
            "Esc",
            "escape",
            CloseProject,
            PROJECT_DIALOG,
            "Discard and close",
        ),
        e("", "escape", CloseProject, IN_FIELD, ""),
    ]
}

/// Registers the dialog's bindings. Call once at startup, after the
/// component library's own (so that Tab, Esc and Ctrl-Enter in a field are
/// ours).
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(entries().into_iter().map(|e| e.binding));
}
