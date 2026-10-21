//! Actions and key bindings of the diff view. They follow PolyGerrit's diff
//! shortcuts where it has one (see the notes per binding), and apply only
//! inside the [`DIFF_VIEW`] key context.

use gpui_kit::*;

use crate::actions::Entry;

actions!(
    diff_view,
    [
        LineDown,
        LineUp,
        NextChunk,
        PrevChunk,
        PageDown,
        PageUp,
        HalfPageDown,
        HalfPageUp,
        CursorTop,
        CursorBottom,
        ToggleMode,
        /// Enter / x on a fold row: show all of its lines.
        ExpandFold,
        /// Show 10 more lines at the top edge of the fold under the cursor.
        ExpandAbove,
        /// Show 10 more lines at the bottom edge of the fold under the cursor.
        ExpandBelow,
        /// Open every fold of the file; again, fold them again.
        ShowFullFile,
        /// `shift-n` / `shift-p`: the next / previous comment thread.
        NextThread,
        PrevThread,
        /// `e` / `shift-e`: open / close every comment thread of the file.
        ExpandThreads,
        CollapseThreads,
        /// `c`: a new comment on the cursor line; on a thread, a reply (or
        /// the thread's draft, edited).
        NewComment,
        /// `q`: reply to the thread under the cursor, quoting its last message.
        QuoteReply,
        /// `d`: reply "Done" to the thread under the cursor and resolve it.
        ThreadDone,
        // In the draft editor (key context `DraftEditor`).
        /// Ctrl-Enter / Ctrl-S: save the draft.
        SaveDraft,
        /// Esc: close the editor.
        CancelDraft,
        // Declared for the diff page (handled there, not by the widget).
        PrevFile,
        NextFile,
    ]
);

/// Key context of the diff view.
pub const DIFF_VIEW: &str = "DiffView";

/// Key context of the inline draft editor, inside [`DIFF_VIEW`].
pub const DRAFT_EDITOR: &str = "DraftEditor";

// Single-letter bindings must not fire while text is being typed (the inline
// draft editor), whose text field marks its key context "Input".
const KEYS: &str = "DiffView && !Input";

fn e(
    keys: &'static str,
    key: &'static str,
    action: impl Action,
    description: &'static str,
) -> Entry {
    Entry {
        binding: KeyBinding::new(key, action, Some(KEYS)),
        keys,
        description,
    }
}

pub fn entries() -> Vec<Entry> {
    vec![
        e("j / k", "j", LineDown, "Next / previous line"),
        e("", "k", LineUp, ""),
        e("", "down", LineDown, ""),
        e("", "up", LineUp, ""),
        e("n / p", "n", NextChunk, "Next / previous changed chunk"),
        e("", "p", PrevChunk, ""),
        e("Space / Shift-Space", "space", PageDown, "Page down / up"),
        e("", "shift-space", PageUp, ""),
        e("", "pagedown", PageDown, ""),
        e("", "pageup", PageUp, ""),
        e(
            "Ctrl-D / Ctrl-U",
            "ctrl-d",
            HalfPageDown,
            "Half page down / up",
        ),
        e("", "ctrl-u", HalfPageUp, ""),
        e("Home / End", "home", CursorTop, "First / last line"),
        e("", "end", CursorBottom, ""),
        e("", "shift-g", CursorBottom, ""),
        // PolyGerrit toggles side-by-side/unified with `m` in current
        // versions (older ones used shift-m); bind both.
        e("m", "m", ToggleMode, "Toggle side-by-side / unified"),
        e("", "shift-m", ToggleMode, ""),
        e(
            "Enter / X",
            "enter",
            ExpandFold,
            "Show all lines of the folded range, or open / close the thread, under the cursor",
        ),
        e("", "x", ExpandFold, ""),
        // alt-left / alt-right are history; alt-up / alt-down reveal
        // lines above / below a fold, like PolyGerrit's "+10 above/below".
        e(
            "Alt-Up / Alt-Down",
            "alt-up",
            ExpandAbove,
            "Show 10 more lines above / below a folded range",
        ),
        e("", "alt-down", ExpandBelow, ""),
        // PolyGerrit: `N` / `P` next / previous thread, `e` expand all
        // threads. Its `E` collapses them all; so does shift-e here.
        e(
            "Shift-N / Shift-P",
            "shift-n",
            NextThread,
            "Next / previous comment thread",
        ),
        e("", "shift-p", PrevThread, ""),
        e(
            "E / Shift-E",
            "e",
            ExpandThreads,
            "Open / close all comment threads",
        ),
        e("", "shift-e", CollapseThreads, ""),
        e(
            "Shift-X",
            "shift-x",
            ShowFullFile,
            "Show the whole file; again: fold unchanged lines",
        ),
        // PolyGerrit: `c` comments on the cursor line. On a thread it
        // replies (`a` and `r`, the reply dialog and "reviewed", belong to
        // the pages).
        e(
            "C",
            "c",
            NewComment,
            "Comment on the cursor line; on a thread: reply, or edit your draft",
        ),
        e("Q", "q", QuoteReply, "Reply to the thread, quoting it"),
        e(
            "D",
            "d",
            ThreadDone,
            "Reply \"Done\" to the thread, resolving it",
        ),
    ]
}

/// Bindings of the draft editor. They work while the text field has the
/// focus (so not under `!Input`), and the field itself handles Ctrl-Enter
/// (it emits an event the view saves on).
pub fn editor_entries() -> Vec<Entry> {
    fn ed(
        keys: &'static str,
        key: &'static str,
        action: impl Action,
        description: &'static str,
    ) -> Entry {
        Entry {
            binding: KeyBinding::new(key, action, Some(DRAFT_EDITOR)),
            keys,
            description,
        }
    }
    vec![
        ed(
            if cfg!(target_os = "macos") {
                "Cmd-Enter / Cmd-S"
            } else {
                "Ctrl-Enter / Ctrl-S"
            },
            "secondary-s",
            SaveDraft,
            "Save the draft being edited",
        ),
        ed("Esc", "escape", CancelDraft, "Close the draft editor"),
    ]
}

/// Registers the diff bindings. Call once at startup.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(entries().into_iter().map(|e| e.binding));
    cx.bind_keys(editor_entries().into_iter().map(|e| e.binding));
}
