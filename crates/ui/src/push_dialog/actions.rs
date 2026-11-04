//! Actions and key bindings of the push dialog. The keys apply inside the
//! dialog's key context only.

use gpui_kit::*;

use crate::actions::Entry;

actions!(
    push_dialog,
    [
        /// Esc: close without pushing.
        ClosePush,
        /// Enter: push with the chosen rule.
        Push,
        /// Up / Down (k / j): the rule above / below.
        PushChoicePrev,
        PushChoiceNext,
    ]
);

/// Key context of the dialog.
pub const PUSH_DIALOG: &str = "PushDialog";

pub fn entries() -> Vec<Entry> {
    fn e(
        keys: &'static str,
        key: &'static str,
        action: impl Action,
        description: &'static str,
    ) -> Entry {
        Entry {
            binding: KeyBinding::new(key, action, Some(PUSH_DIALOG)),
            keys,
            description,
        }
    }
    vec![
        e(
            "Up / Down",
            "up",
            PushChoicePrev,
            "The remote and push rule above / below (also k / j)",
        ),
        e("", "down", PushChoiceNext, ""),
        e("", "k", PushChoicePrev, ""),
        e("", "j", PushChoiceNext, ""),
        e("Enter", "enter", Push, "Push with the chosen rule"),
        e("Esc", "escape", ClosePush, "Close without pushing"),
    ]
}

/// Registers the dialog's bindings. Call once at startup.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(entries().into_iter().map(|e| e.binding));
}
