//! Inline draft editing: the editor inside a thread card, the keys that
//! open it, and the requests the view makes of its owner.
//!
//! The view does no I/O. Saving, deleting and retrying are
//! [`DraftRequest`]s in [`DiffEvent::Draft`]; the owner hands them to the
//! draft outbox, and the drafts come back as threads with `set_threads`.
//!
//! # What is edited
//!
//! One editor at a time, identified by an [`EditKey`]:
//!
//! * `Draft(id)`: the text of an existing draft, in place of its message.
//! * `Reply(thread)`: a new draft reply, below the thread's entries. It
//!   starts with the thread's state (a reply to an unresolved thread is
//!   unresolved unless "Resolved" is ticked).
//! * `New(side, location)`: a new thread. It has no thread yet, so the view
//!   shows a synthetic one without entries at the line; it is placed by the
//!   normal layout rules and disappears when the editor closes.
//!
//! # Closing
//!
//! Saving sends the request and closes the editor. Cancelling (Esc, the
//! button, or opening another editor) closes it too, and never asks: text
//! that is not yet in a draft (non-empty, and for an existing draft
//! different from the saved text) is kept in the view as an unsaved buffer
//! under the same key, and comes back when that editor is opened again, as
//! long as the view lives. An existing draft therefore always keeps its
//! saved text, and typed-but-unsaved text is not lost to a stray Esc.
//!
//! # Focus
//!
//! The text field lives in a list row that scrolls out of view; the list
//! keeps rendering a row whose focus handle it was told about
//! (`splice_focusable`), so the field keeps its focus and text. The list
//! forgets the handles whenever it is reset, so [`DiffView::reset_list`]
//! registers the editor's handle again.

use std::collections::HashMap;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::*;
use model::{
    DiffSide, DraftInput, Entry, LocalDraftId, Location, PatchSetNumber, Side, Thread, ThreadId,
};

use super::{DiffEvent, DiffView, Reveal};
use crate::diff::{
    actions::{CancelDraft, DRAFT_EDITOR, NewComment, QuoteReply, SaveDraft, ThreadDone},
    layout::{LineKind, Row},
    paint::Style,
};

/// What the view asks its owner to do with the user's drafts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DraftRequest {
    /// A new draft on a patch set of the change. The owner knows the
    /// patch set's commit.
    Create {
        patch_set: PatchSetNumber,
        input: DraftInput,
    },
    /// New text (and resolution) for an existing draft.
    Update {
        id: LocalDraftId,
        message: String,
        unresolved: bool,
    },
    Delete(LocalDraftId),
    /// Try a draft whose upload failed again.
    Retry(LocalDraftId),
}

/// What an editor edits. See the module documentation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum EditKey {
    Draft(LocalDraftId),
    Reply(ThreadId),
    New(DiffSide, Location),
}

pub(super) struct DraftEditor {
    pub key: EditKey,
    pub input: Entity<TextareaState>,
    /// The text field's focus handle (what the list keeps alive).
    pub focus: FocusHandle,
    /// The "Resolved" checkbox; the draft's `unresolved` is its opposite.
    pub resolved: bool,
    /// A line under the field: a refused save, or a note that text came
    /// back from the buffer.
    pub note: Option<SharedString>,
    _subs: Vec<Subscription>,
}

/// Unsaved text of an editor that was closed without saving.
pub(super) struct Unsaved {
    text: String,
    resolved: bool,
}

pub(super) type UnsavedBuffers = HashMap<EditKey, Unsaved>;

/// The id of the synthetic thread of a new comment.
fn new_thread_id() -> ThreadId {
    ThreadId::Draft(LocalDraftId(i64::MIN))
}

/// Whether the thread is the one `key` edits in.
pub(super) fn thread_has_key(thread: &Thread, key: &EditKey) -> bool {
    match key {
        EditKey::Draft(id) => thread
            .entries
            .iter()
            .any(|e| matches!(e, Entry::Draft(d) if d.local_id == *id)),
        EditKey::Reply(id) => &thread.id == id,
        EditKey::New(..) => thread.id == new_thread_id(),
    }
}

/// The side and line a comment on this row would go to: the new side if
/// the row has one, else the old side.
fn line_target(row: &Row) -> Option<(DiffSide, u32)> {
    match row {
        Row::Side { left, right } => right
            .as_ref()
            .map(|c| (DiffSide::Right, c.number))
            .or_else(|| left.as_ref().map(|c| (DiffSide::Left, c.number))),
        Row::Unified { old, new, cell } => match cell.kind {
            LineKind::Removed => old.map(|n| (DiffSide::Left, n)),
            _ => new
                .map(|n| (DiffSide::Right, n))
                .or_else(|| old.map(|n| (DiffSide::Left, n))),
        },
        _ => None,
    }
}

/// `> ` before every line, a blank line after.
fn quote(message: &str) -> String {
    let mut out = String::new();
    for line in message.trim_end().lines() {
        out.push_str(if line.is_empty() { ">" } else { "> " });
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out
}

impl DiffView {
    /// Whether the user may write comments (signed in). Without it the
    /// comment keys say so and the cards have no buttons.
    pub fn set_can_comment(&mut self, can: bool, cx: &mut Context<Self>) {
        if self.can_comment != can {
            self.can_comment = can;
            cx.notify();
        }
    }

    pub fn can_comment(&self) -> bool {
        self.can_comment
    }

    /// Whether a draft editor is open.
    pub fn is_editing(&self) -> bool {
        self.editor.is_some()
    }

    /// The thread the editor is in, if it is open and shown.
    pub(super) fn editor_slot(&self) -> Option<usize> {
        let key = &self.editor.as_ref()?.key;
        (0..self.slots.len()).find(|&s| self.thread_at(s).is_some_and(|t| thread_has_key(t, key)))
    }

    /// The layout row of the thread the editor is in.
    fn editor_row(&self) -> Option<usize> {
        let slot = self.editor_slot()?;
        self.layout.thread_rows().iter().copied().find(
            |&r| matches!(self.layout.rows().get(r), Some(Row::Thread(tr)) if tr.slot == slot),
        )
    }

    /// Tells the list which row holds the focused text field, so that it
    /// stays alive when scrolled out of view.
    pub(super) fn register_editor_focus(&self) {
        if let (Some(ed), Some(row)) = (&self.editor, self.editor_row()) {
            self.list
                .splice_focusable(row..row + 1, [Some(ed.focus.clone())]);
        }
    }

    /// The text in the editor.
    pub fn editor_text(&self, cx: &App) -> Option<String> {
        self.editor
            .as_ref()
            .map(|e| e.input.read(cx).value().to_string())
    }

    #[cfg(test)]
    pub(crate) fn editor_focused_for_test(&self, window: &Window) -> bool {
        self.editor
            .as_ref()
            .is_some_and(|e| e.focus.is_focused(window))
    }

    #[cfg(test)]
    pub(crate) fn editor_note_for_test(&self) -> Option<String> {
        self.editor
            .as_ref()
            .and_then(|e| e.note.as_ref().map(|n| n.to_string()))
    }

    // --- opening ---------------------------------------------------------

    /// `c`.
    pub(super) fn new_comment(
        &mut self,
        _: &NewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_comment {
            return self.say("Sign in to comment", cx);
        }
        if self.threads_only && !matches!(self.layout.rows().get(self.cursor), Some(Row::Thread(_)))
        {
            return self.new_patchset_comment(window, cx);
        }
        match self.layout.rows().get(self.cursor) {
            Some(Row::Thread(tr)) => {
                let Some(thread) = self.thread_at(tr.slot) else {
                    return;
                };
                let key = match thread.draft() {
                    Some(d) => EditKey::Draft(d.local_id),
                    None if thread.entries.is_empty() => {
                        // The synthetic thread of a new comment.
                        return self.focus_editor(window, cx);
                    }
                    None => EditKey::Reply(thread.id.clone()),
                };
                self.open_editor(key, None, window, cx);
            }
            Some(row) => match line_target(row) {
                Some((side, line)) => {
                    self.open_editor(EditKey::New(side, Location::Line(line)), None, window, cx)
                }
                None => self.say("Move to a code line to comment", cx),
            },
            None => {}
        }
    }

    /// A new patch-set level thread on the current patch set, in a
    /// [`DiffView::threads_only`] view.
    pub fn new_patchset_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_comment {
            return self.say("Sign in to comment", cx);
        }
        if self.threads_only {
            self.open_editor(
                EditKey::New(DiffSide::Right, Location::File),
                None,
                window,
                cx,
            );
        }
    }

    /// `q`: a reply that quotes the last comment.
    pub(super) fn quote_reply(
        &mut self,
        _: &QuoteReply,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_comment {
            return self.say("Sign in to comment", cx);
        }
        self.quote_thread_at(self.cursor, window, cx);
    }

    pub(super) fn quote_thread_at(
        &mut self,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = self.thread_of_row(row) else {
            return self.say("The cursor is not on a comment thread", cx);
        };
        let Some(last) = thread.entries.iter().rev().find_map(|e| match e {
            Entry::Published(c) => Some(c.message.clone()),
            Entry::Draft(_) => None,
        }) else {
            return self.say("Nothing to quote yet", cx);
        };
        let key = match thread.draft() {
            Some(d) => EditKey::Draft(d.local_id),
            None => EditKey::Reply(thread.id.clone()),
        };
        self.open_editor(key, Some(quote(&last)), window, cx);
    }

    /// Reply, or edit the thread's draft, from the card's button.
    pub(super) fn reply_to_thread_at(
        &mut self,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = self.thread_of_row(row) else {
            return;
        };
        let key = match thread.draft() {
            Some(d) => EditKey::Draft(d.local_id),
            None => EditKey::Reply(thread.id.clone()),
        };
        self.open_editor(key, None, window, cx);
    }

    /// `d`.
    pub(super) fn thread_done(&mut self, _: &ThreadDone, _: &mut Window, cx: &mut Context<Self>) {
        if !self.can_comment {
            return self.say("Sign in to comment", cx);
        }
        self.quick_reply(self.cursor, "Done", cx);
    }

    /// Replies to the thread on `row` with a fixed message ("Ack",
    /// "Done"), resolving it, without opening the editor.
    pub(super) fn quick_reply(&mut self, row: usize, message: &str, cx: &mut Context<Self>) {
        let Some(thread) = self.thread_of_row(row) else {
            return self.say("The cursor is not on a comment thread", cx);
        };
        if thread.reply_target().is_none() {
            return self.say("There is nothing to reply to yet", cx);
        }
        let request = reply_request(thread, message.to_owned(), false);
        cx.emit(DiffEvent::Draft(request));
    }

    fn thread_of_row(&self, row: usize) -> Option<&Thread> {
        match self.layout.rows().get(row) {
            Some(Row::Thread(tr)) => self.thread_at(tr.slot),
            _ => None,
        }
    }

    fn focus_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ed) = &self.editor {
            let input = ed.input.clone();
            input.update(cx, |i, cx| i.focus(window, cx));
        }
    }

    /// Opens the editor for `key` (closing another one, keeping its text)
    /// and puts the cursor on its thread. `quoted` is added to the text.
    pub(super) fn open_editor(
        &mut self,
        key: EditKey,
        quoted: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_comment {
            return;
        }
        if quoted.is_none() && self.editor.as_ref().is_some_and(|e| e.key == key) {
            return self.focus_editor(window, cx);
        }
        self.close_editor(true, window, cx);
        let (mut text, mut resolved) = self.initial_text(&key);
        let mut note = None;
        if let Some(unsaved) = self.unsaved.remove(&key) {
            text = unsaved.text;
            resolved = unsaved.resolved;
            note = Some("Unsaved text restored".into());
        }
        if let Some(q) = quoted {
            text = if text.trim().is_empty() {
                q
            } else {
                format!("{}\n\n{q}", text.trim_end())
            };
        }
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 12)
                .placeholder("Write a comment")
        });
        let focus = input.read(cx).focus_handle(cx);
        input.update(cx, |i, cx| {
            if !text.is_empty() {
                i.set_value(text.clone(), window, cx);
                // The end of the text, where one goes on writing.
                let line = text.matches('\n').count();
                let column = text
                    .rsplit('\n')
                    .next()
                    .unwrap_or("")
                    .encode_utf16()
                    .count();
                i.set_cursor_position(
                    gpui_kit::component::input::Position::new(line as u32, column as u32),
                    window,
                    cx,
                );
            }
        });
        let subs =
            vec![cx.subscribe_in(
                &input,
                window,
                |this, _, ev: &InputEvent, window, cx| match ev {
                    // Ctrl-Enter. (The field inserts a newline too; the text
                    // is trimmed.)
                    InputEvent::PressEnter {
                        secondary: true, ..
                    } => this.save_editor(window, cx),
                    InputEvent::Change => {
                        if let Some(ed) = this.editor.as_mut()
                            && ed.note.take().is_some()
                        {
                            cx.notify();
                        }
                    }
                    _ => {}
                },
            )];
        let is_new = matches!(key, EditKey::New(..));
        self.editor = Some(DraftEditor {
            key: key.clone(),
            input,
            focus,
            resolved,
            note,
            _subs: subs,
        });
        self.hint = None;
        if let EditKey::New(side, location) = key {
            self.new_thread = Some(self.synthetic_thread(side, location));
        }
        if is_new {
            self.reslot_now(cx);
        } else {
            self.register_editor_focus();
            if let Some(row) = self.editor_row() {
                self.list.remeasure_items(row..row + 1);
            }
        }
        if let Some(row) = self.editor_row() {
            self.cursor = row;
            self.scroll_to_cursor(Reveal::Nearest);
        }
        self.focus_editor(window, cx);
        cx.emit(DiffEvent::RowsChanged);
        cx.notify();
    }

    /// The text and "resolved" state an editor for `key` starts with.
    fn initial_text(&self, key: &EditKey) -> (String, bool) {
        match key {
            EditKey::Draft(id) => self
                .threads
                .iter()
                .flat_map(|t| &t.entries)
                .find_map(|e| match e {
                    Entry::Draft(d) if d.local_id == *id => {
                        Some((d.message.clone(), !d.unresolved))
                    }
                    _ => None,
                })
                .unwrap_or_default(),
            EditKey::Reply(id) => (
                String::new(),
                self.threads
                    .iter()
                    .find(|t| &t.id == id)
                    .is_some_and(|t| !t.is_unresolved()),
            ),
            EditKey::New(..) => (String::new(), false),
        }
    }

    fn synthetic_thread(&self, side: DiffSide, location: Location) -> Thread {
        let (patch_set, thread_side, parent, path) = self.anchor_for(side);
        Thread {
            id: new_thread_id(),
            path,
            patch_set,
            side: thread_side,
            parent,
            location,
            entries: Vec::new(),
            orphaned: false,
        }
    }

    /// Where a new comment on `side` is anchored: the patch set, the
    /// side and parent it is stored with, and the file's name there.
    ///
    /// The right side is the target patch set's file. The left side is the
    /// base patch set's file when the diff is against another patch set,
    /// and otherwise the file in the target's parent commit (`PARENT`).
    /// This is what `Thread::side_in` shows again on the same side.
    fn anchor_for(&self, side: DiffSide) -> (PatchSetNumber, Side, Option<u32>, String) {
        let Some(ctx) = &self.thread_context else {
            return (PatchSetNumber(0), Side::Revision, None, String::new());
        };
        match side {
            DiffSide::Right => (ctx.pair.target, Side::Revision, None, ctx.path.clone()),
            DiffSide::Left => {
                let name = self
                    .diff
                    .meta_a
                    .as_ref()
                    .map(|m| m.name.clone())
                    .or_else(|| ctx.old_path.clone())
                    .unwrap_or_else(|| ctx.path.clone());
                match ctx.pair.base {
                    Some(base) => (base, Side::Revision, None, name),
                    None => (ctx.pair.target, Side::Parent, None, name),
                }
            }
        }
    }

    // --- closing ---------------------------------------------------------

    /// Esc, or the Cancel button.
    pub(super) fn cancel_editor(
        &mut self,
        _: &CancelDraft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_editor(true, window, cx);
    }

    /// Ctrl-S, or the Save button.
    pub(super) fn save_action(
        &mut self,
        _: &SaveDraft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_editor(window, cx);
    }

    /// Closes the editor. With `keep`, text that is not in a draft yet is
    /// kept as an unsaved buffer (see the module documentation).
    pub(super) fn close_editor(&mut self, keep: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ed) = self.editor.as_ref() else {
            return;
        };
        let key = ed.key.clone();
        let text = ed.input.read(cx).value().to_string();
        let resolved = ed.resolved;
        let saved = self.initial_text(&key).0;
        let row = self.editor_row();
        self.editor = None;
        if keep && !text.trim().is_empty() && text.trim() != saved.trim() {
            self.unsaved.insert(key.clone(), Unsaved { text, resolved });
        } else {
            self.unsaved.remove(&key);
        }
        if matches!(key, EditKey::New(..)) {
            self.new_thread = None;
            self.reslot_now(cx);
        } else if let Some(row) = row {
            // Forget the focus handle and measure the row again.
            self.list.splice_focusable(row..row + 1, [None]);
        }
        window.focus(&self.focus, cx);
        cx.emit(DiffEvent::RowsChanged);
        cx.notify();
    }

    /// Sends the editor's text as a draft request and closes it. An empty
    /// message is refused here: the server answers 400 for it.
    pub(super) fn save_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ed) = self.editor.as_mut() else {
            return;
        };
        let text = ed.input.read(cx).value().trim().to_owned();
        if text.is_empty() {
            ed.note = Some("A comment cannot be empty. Write something, or press Esc.".into());
            cx.notify();
            return;
        }
        let (key, unresolved) = (ed.key.clone(), !ed.resolved);
        let Some(request) = self.request_for(&key, text, unresolved) else {
            return;
        };
        self.unsaved.remove(&key);
        self.close_editor(false, window, cx);
        cx.emit(DiffEvent::Draft(request));
    }

    fn request_for(
        &self,
        key: &EditKey,
        message: String,
        unresolved: bool,
    ) -> Option<DraftRequest> {
        match key {
            EditKey::Draft(id) => Some(DraftRequest::Update {
                id: *id,
                message,
                unresolved,
            }),
            EditKey::Reply(id) => {
                let thread = self.threads.iter().find(|t| &t.id == id)?;
                Some(reply_request(thread, message, unresolved))
            }
            EditKey::New(side, location) => {
                let (patch_set, thread_side, parent, path) = self.anchor_for(*side);
                Some(DraftRequest::Create {
                    patch_set,
                    input: DraftInput {
                        path,
                        side: thread_side,
                        parent,
                        location: *location,
                        in_reply_to: None,
                        message,
                        unresolved,
                    },
                })
            }
        }
    }

    // --- deleting --------------------------------------------------------

    /// Asks "delete this draft?" on its card.
    pub(super) fn ask_delete(&mut self, id: LocalDraftId, row: usize, cx: &mut Context<Self>) {
        self.confirm_delete = Some(id);
        self.list.remeasure_items(row..row + 1);
        cx.emit(DiffEvent::RowsChanged);
        cx.notify();
    }

    pub(super) fn answer_delete(
        &mut self,
        id: LocalDraftId,
        row: usize,
        delete: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.confirm_delete = None;
        if delete {
            if self
                .editor
                .as_ref()
                .is_some_and(|e| e.key == EditKey::Draft(id))
            {
                self.close_editor(false, window, cx);
            }
            self.unsaved.remove(&EditKey::Draft(id));
            cx.emit(DiffEvent::Draft(DraftRequest::Delete(id)));
        }
        self.list.remeasure_items(row..row + 1);
        cx.emit(DiffEvent::RowsChanged);
        cx.notify();
    }

    pub(super) fn edit_draft_at(
        &mut self,
        id: LocalDraftId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_editor(EditKey::Draft(id), None, window, cx);
    }

    // --- rendering -------------------------------------------------------

    /// The editor: the text field, "Resolved", and the buttons.
    pub(super) fn editor_element(
        &self,
        ix: usize,
        style: &Style,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let ed = self.editor.as_ref()?;
        let muted = style.palette.muted_fg;
        let save_hint = if cfg!(target_os = "macos") {
            "Cmd-Enter saves, Esc closes"
        } else {
            "Ctrl-Enter saves, Esc closes"
        };
        let note = ed.note.clone();
        Some(
            v_flex()
                .id(("draft-editor", ix))
                .key_context(DRAFT_EDITOR)
                .on_action(cx.listener(Self::cancel_editor))
                .on_action(cx.listener(Self::save_action))
                // A click inside is not a click on the row (which would
                // take the focus back to the code).
                .on_click(|_, _, cx| cx.stop_propagation())
                .gap_1()
                .child(Textarea::new(&ed.input).small())
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            Checkbox::new(("resolved", ix))
                                .label("Resolved")
                                .checked(ed.resolved)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    if let Some(ed) = this.editor.as_mut() {
                                        ed.resolved = *checked;
                                        cx.notify();
                                    }
                                })),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(if note.is_some() {
                                    style.palette.danger
                                } else {
                                    muted
                                })
                                .child(note.unwrap_or_else(|| save_hint.into())),
                        )
                        .child(
                            Button::new(("draft-cancel", ix))
                                .label("Cancel")
                                .xsmall()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.close_editor(true, window, cx);
                                })),
                        )
                        .child(
                            Button::new(("draft-save", ix))
                                .label("Save")
                                .xsmall()
                                .primary()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.save_editor(window, cx);
                                })),
                        ),
                )
                .into_any_element(),
        )
    }
}

/// A draft reply to `thread`, anchored where the thread is.
fn reply_request(thread: &Thread, message: String, unresolved: bool) -> DraftRequest {
    DraftRequest::Create {
        patch_set: thread.patch_set,
        input: DraftInput {
            path: thread.path.clone(),
            side: thread.side,
            parent: thread.parent,
            location: thread.location,
            in_reply_to: thread.reply_target().map(str::to_owned),
            message,
            unresolved,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn quoting_prefixes_every_line() {
        assert_eq!(quote("one\n\ntwo\n"), "> one\n>\n> two\n\n");
    }
}
