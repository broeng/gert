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
        // Declared for the diff page and for the comment work (M3); the
        // widget does not handle them and no key is bound yet.
        PrevFile,
        NextFile,
        NextThread,
        PrevThread,
    ]
);

/// Key context of the diff view.
pub const DIFF_VIEW: &str = "DiffView";

// Single-letter bindings must not fire while text is being typed (inline
// comment editors, M3), which mark their key context "Input".
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
            "Show all lines of the folded range under the cursor",
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
        e(
            "Shift-X",
            "shift-x",
            ShowFullFile,
            "Show the whole file; again: fold unchanged lines",
        ),
    ]
}

/// Registers the diff bindings. Call once at startup.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(entries().into_iter().map(|e| e.binding));
}
