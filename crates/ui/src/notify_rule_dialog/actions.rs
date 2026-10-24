//! Actions and key bindings of the notification rule editor. The keys
//! apply inside the dialog's key contexts only.

use gpui_kit::*;

use crate::actions::Entry;

actions!(
    notify_rule_dialog,
    [
        /// Esc: discard the edits and close.
        CloseRule,
        /// Ctrl-Enter / Cmd-Enter: save the rule.
        SaveRule,
        /// Tab / Shift-Tab: the next / previous control.
        RuleNextField,
        RulePrevField,
        /// Left / Right on the event or the scope: the previous / next one.
        ChoicePrev,
        ChoiceNext,
    ]
);

/// Key context of the dialog.
pub const NOTIFY_RULE_DIALOG: &str = "NotifyRuleDialog";
/// Around the event and the scope choices.
pub const CHOICE_GROUP: &str = "NotifyChoiceGroup";

/// The text fields give Tab, Esc and Ctrl-Enter up instead of using them:
/// this binding is as deep as the field's own (`Input`) and registered after
/// it, so it wins.
const IN_FIELD: &str = "NotifyRuleDialog > Input";

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
            RuleNextField,
            NOTIFY_RULE_DIALOG,
            "Next / previous control",
        ),
        e("", "shift-tab", RulePrevField, NOTIFY_RULE_DIALOG, ""),
        e("", "tab", RuleNextField, IN_FIELD, ""),
        e("", "shift-tab", RulePrevField, IN_FIELD, ""),
        e(
            "Left / Right",
            "left",
            ChoicePrev,
            CHOICE_GROUP,
            "On the event or the scope: the previous / next one",
        ),
        e("", "right", ChoiceNext, CHOICE_GROUP, ""),
        e(
            if cfg!(target_os = "macos") {
                "Cmd-Enter"
            } else {
                "Ctrl-Enter"
            },
            "secondary-enter",
            SaveRule,
            NOTIFY_RULE_DIALOG,
            "Save the rule",
        ),
        e("", "secondary-enter", SaveRule, IN_FIELD, ""),
        e(
            "Esc",
            "escape",
            CloseRule,
            NOTIFY_RULE_DIALOG,
            "Discard and close",
        ),
        e("", "escape", CloseRule, IN_FIELD, ""),
    ]
}

/// Registers the dialog's bindings. Call once at startup, after the
/// component library's own (so that Tab, Esc and Ctrl-Enter in a field are
/// ours).
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(entries().into_iter().map(|e| e.binding));
}
