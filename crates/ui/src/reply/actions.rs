//! Actions and key bindings of the reply dialog. The keys apply inside the
//! dialog's key contexts only; the page keys that open it (`a`) are in
//! [`crate::actions`].

use gpui_kit::*;

use crate::actions::Entry;

actions!(
    reply,
    [
        /// Esc: close the dialog, keeping the text as a draft.
        CloseReply,
        /// Ctrl-Enter / Cmd-Enter: send the review.
        SendReply,
        /// Tab / Shift-Tab: the next / previous control.
        ReplyNext,
        ReplyPrev,
        /// Left / Right on a vote: the previous / next value.
        ReplyVoteLeft,
        ReplyVoteRight,
    ]
);

/// Key context of the dialog.
pub const REPLY_DIALOG: &str = "ReplyDialog";
/// Key context around the buttons of one vote.
pub const REPLY_VOTE: &str = "ReplyVote";

/// The message field gives Tab up instead of indenting: this binding is as
/// deep as the field's own (`Input`) and registered after it, so it wins.
const IN_FIELD: &str = "ReplyDialog > Input";

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
            if cfg!(target_os = "macos") {
                "Cmd-Enter"
            } else {
                "Ctrl-Enter"
            },
            "secondary-enter",
            SendReply,
            REPLY_DIALOG,
            "Send the reply",
        ),
        e(
            "Esc",
            "escape",
            CloseReply,
            REPLY_DIALOG,
            "Close; the text stays as a draft",
        ),
        e(
            "Tab / Shift-Tab",
            "tab",
            ReplyNext,
            REPLY_DIALOG,
            "Next / previous control",
        ),
        e("", "shift-tab", ReplyPrev, REPLY_DIALOG, ""),
        e("", "tab", ReplyNext, IN_FIELD, ""),
        e("", "shift-tab", ReplyPrev, IN_FIELD, ""),
        e(
            "Left / Right",
            "left",
            ReplyVoteLeft,
            REPLY_VOTE,
            "On a vote: the previous / next value",
        ),
        e("", "right", ReplyVoteRight, REPLY_VOTE, ""),
    ]
}

/// Registers the dialog's bindings. Call once at startup, after the
/// component library's own (so that Tab in the field is ours).
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(entries().into_iter().map(|e| e.binding));
}
