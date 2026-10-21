//! Inline draft editing: opening and closing the editor, the keys, and the
//! requests the view makes of its owner.

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
use model::{DiffViewMode, DraftSync, Entry, Location};

use super::{
    DiffView, Events, at13, cursor, give_threads, open, press, rows, thread, thread_context,
    thread_rows, threads::draft,
};

fn editable(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<DiffView>, Events) {
    let (window, view, events) = open(cx, super::sample(), DiffViewMode::SideBySide);
    cx.update(|cx| view.update(cx, |v, cx| v.set_can_comment(true, cx)));
    (window, view, events)
}

fn typed(cx: &mut TestAppContext, window: AnyWindowHandle, text: &str) {
    cx.update_window(window, |_, window, cx| window.input(text, cx))
        .expect("input");
}

/// Ctrl-Enter (Cmd-Enter on macOS): the text field's own save key.
fn save_key(cx: &mut TestAppContext, window: AnyWindowHandle) {
    let key = if cfg!(target_os = "macos") {
        "cmd-enter"
    } else {
        "ctrl-enter"
    };
    press(cx, window, key);
}

fn text(cx: &mut TestAppContext, view: &Entity<DiffView>) -> Option<String> {
    cx.update(|cx| view.read(cx).editor_text(cx))
}

fn drafts(events: &Events) -> Vec<String> {
    events
        .lock()
        .expect("log")
        .iter()
        .filter(|e| e.starts_with("draft") || e.starts_with("retry"))
        .cloned()
        .collect()
}

fn clear(events: &Events) {
    events.lock().expect("log").clear();
}

/// Moves to the first changed chunk: row 4, old13 / new13.
fn on_line_13(cx: &mut TestAppContext, window: AnyWindowHandle) {
    press(cx, window, "n");
}

#[gpui_kit::test]
fn c_opens_an_editor_and_ctrl_enter_saves_the_trimmed_text(cx: &mut TestAppContext) {
    let (window, view, events) = editable(cx);
    give_threads(cx, &view, vec![]);
    on_line_13(cx, window);
    let before = rows(cx, &view);
    press(cx, window, "c");
    assert_eq!(text(cx, &view).as_deref(), Some(""));
    // The synthetic thread of the new comment sits after the line.
    assert_eq!(rows(cx, &view), before + 1);
    assert_eq!(thread_rows(cx, &view), [5]);
    assert_eq!(cursor(cx, &view), 5);
    cx.update_window(window, |_, window, cx| {
        assert!(view.read(cx).editor_focused_for_test(window));
    })
    .expect("window");

    typed(cx, window, "  please fix ");
    assert_eq!(text(cx, &view).as_deref(), Some("  please fix "));
    clear(&events);
    save_key(cx, window);
    assert_eq!(
        drafts(&events),
        [
            "draft Create { patch_set: PatchSetNumber(2), input: DraftInput { \
             path: \"f\", side: Revision, parent: None, location: Line(13), \
             in_reply_to: None, message: \"please fix\", unresolved: true } }"
        ]
    );
    // The editor is gone, and so is the row of the unsaved comment.
    assert_eq!(text(cx, &view), None);
    assert_eq!(rows(cx, &view), before);
    // The cursor is back on the code line and the diff has the focus.
    assert_eq!(cursor(cx, &view), 4);
    press(cx, window, "j");
    assert_eq!(cursor(cx, &view), 5);
}

#[gpui_kit::test]
fn letters_typed_in_the_editor_are_text_not_diff_keys(cx: &mut TestAppContext) {
    let (window, view, _) = editable(cx);
    on_line_13(cx, window);
    press(cx, window, "c");
    let at = cursor(cx, &view);
    for key in ["j", "k", "n", "c", "q", "d", "e", "m", "x", "space"] {
        press(cx, window, key);
    }
    assert_eq!(
        text(cx, &view).as_deref(),
        Some("jkncqdemx "),
        "keys are text"
    );
    assert_eq!(cursor(cx, &view), at, "the cursor did not move");
    assert!(!cx.update(|cx| view.read(cx).all_expanded()));
}

#[gpui_kit::test]
fn an_empty_comment_is_refused_with_a_hint(cx: &mut TestAppContext) {
    let (window, view, events) = editable(cx);
    on_line_13(cx, window);
    press(cx, window, "c");
    clear(&events);
    save_key(cx, window);
    assert!(drafts(&events).is_empty());
    assert!(text(cx, &view).is_some(), "the editor stays open");
    let note = cx.update(|cx| view.read(cx).editor_note_for_test());
    assert!(note.is_some_and(|n| n.contains("empty")));
    // Only blanks is empty too.
    typed(cx, window, "  \n ");
    save_key(cx, window);
    assert!(drafts(&events).is_empty());
    // Typing clears the note.
    typed(cx, window, "x");
    assert_eq!(cx.update(|cx| view.read(cx).editor_note_for_test()), None);
}

#[gpui_kit::test]
fn escape_discards_a_new_comment_and_keeps_its_text_as_an_unsaved_buffer(cx: &mut TestAppContext) {
    let (window, view, events) = editable(cx);
    on_line_13(cx, window);
    let before = rows(cx, &view);
    press(cx, window, "c");
    typed(cx, window, "half written");
    clear(&events);
    press(cx, window, "escape");
    assert_eq!(text(cx, &view), None);
    assert_eq!(rows(cx, &view), before, "the thread row is gone");
    assert!(drafts(&events).is_empty(), "nothing was saved");
    // Back on the same line, the text comes back.
    press(cx, window, "c");
    assert_eq!(text(cx, &view).as_deref(), Some("half written"));
    // An empty one leaves nothing behind.
    press(cx, window, "ctrl-a");
    press(cx, window, "backspace");
    press(cx, window, "escape");
    press(cx, window, "c");
    assert_eq!(text(cx, &view).as_deref(), Some(""));
}

#[gpui_kit::test]
fn c_on_a_thread_replies_and_the_reply_inherits_the_threads_state(cx: &mut TestAppContext) {
    let (window, view, events) = editable(cx);
    give_threads(
        cx,
        &view,
        vec![
            thread("a", Location::Line(13), "please fix", true),
            thread("r", Location::Line(14), "all good", false),
        ],
    );
    press(cx, window, "shift-n");
    assert_eq!(cursor(cx, &view), 5);
    press(cx, window, "c");
    typed(cx, window, "will do");
    clear(&events);
    save_key(cx, window);
    let sent = drafts(&events);
    assert_eq!(sent.len(), 1);
    assert!(sent[0].contains("in_reply_to: Some(\"a\")"), "{sent:?}");
    assert!(sent[0].contains("message: \"will do\""), "{sent:?}");
    assert!(sent[0].contains("unresolved: true"), "{sent:?}");
    assert!(sent[0].contains("Line(13)"), "{sent:?}");

    // A resolved thread: the reply is resolved; ticking "Resolved" off or on
    // is what the checkbox does.
    press(cx, window, "shift-n");
    press(cx, window, "c");
    let row = cursor(cx, &view);
    typed(cx, window, "thanks");
    cx.update_window(window, |_, window, cx| {
        window.click(("resolved", row), cx);
    })
    .expect("click");
    clear(&events);
    save_key(cx, window);
    let sent = drafts(&events);
    assert!(sent[0].contains("in_reply_to: Some(\"r\")"), "{sent:?}");
    assert!(sent[0].contains("unresolved: true"), "unticked: {sent:?}");
}

#[gpui_kit::test]
fn ack_and_done_save_a_resolving_reply_without_an_editor(cx: &mut TestAppContext) {
    let (window, view, events) = editable(cx);
    give_threads(cx, &view, vec![at13("a", "please fix")]);
    press(cx, window, "shift-n");
    clear(&events);
    press(cx, window, "d");
    let sent = drafts(&events);
    assert_eq!(sent.len(), 1);
    assert!(sent[0].contains("message: \"Done\""), "{sent:?}");
    assert!(sent[0].contains("unresolved: false"), "{sent:?}");
    assert!(sent[0].contains("in_reply_to: Some(\"a\")"), "{sent:?}");
    assert_eq!(text(cx, &view), None);

    let row = cursor(cx, &view);
    cx.update_window(window, |_, window, cx| {
        window.click(("thread-ack", row), cx);
    })
    .expect("click");
    let sent = drafts(&events);
    assert!(sent[1].contains("message: \"Ack\""), "{sent:?}");
    assert!(sent[1].contains("unresolved: false"), "{sent:?}");
    // `d` away from a thread says so.
    press(cx, window, "home");
    press(cx, window, "d");
    assert_eq!(drafts(&events).len(), 2);
    assert!(cx.update(|cx| view.read(cx).hint().is_some()));
}

#[gpui_kit::test]
fn q_opens_a_reply_with_the_last_message_quoted(cx: &mut TestAppContext) {
    let (window, view, events) = editable(cx);
    give_threads(cx, &view, vec![at13("a", "first line\n\nthird line")]);
    press(cx, window, "shift-n");
    press(cx, window, "q");
    assert_eq!(
        text(cx, &view).as_deref(),
        Some("> first line\n>\n> third line\n\n")
    );
    typed(cx, window, "why?");
    clear(&events);
    save_key(cx, window);
    let sent = drafts(&events);
    assert!(
        sent[0].contains("message: \"> first line\\n>\\n> third line\\n\\nwhy?\""),
        "{sent:?}"
    );
}

#[gpui_kit::test]
fn editing_a_draft_updates_it_and_cancelling_keeps_the_saved_text(cx: &mut TestAppContext) {
    let (window, view, events) = editable(cx);
    let mut t = at13("a", "please fix");
    t.entries.push(Entry::Draft(draft(
        5,
        Location::Line(13),
        "my saved reply",
        DraftSync::Synced,
    )));
    give_threads(cx, &view, vec![t]);
    press(cx, window, "shift-n");
    // `c` on a thread with a draft edits the draft.
    press(cx, window, "c");
    assert_eq!(text(cx, &view).as_deref(), Some("my saved reply"));
    typed(cx, window, " and more");
    press(cx, window, "escape");
    // Closing did not change the draft ...
    let shown = cx.update(|cx| {
        view.read(cx)
            .shown_threads()
            .next()
            .and_then(|t| t.draft().map(|d| d.message.clone()))
    });
    assert_eq!(shown.as_deref(), Some("my saved reply"));
    assert!(drafts(&events).is_empty());
    // ... and the unsaved edit is offered again.
    press(cx, window, "c");
    assert_eq!(text(cx, &view).as_deref(), Some("my saved reply and more"));
    clear(&events);
    save_key(cx, window);
    assert_eq!(
        drafts(&events),
        [
            "draft Update { id: LocalDraftId(5), message: \"my saved reply and more\", unresolved: true }"
        ]
    );
}

#[gpui_kit::test]
fn delete_asks_in_the_card_first(cx: &mut TestAppContext) {
    let (window, view, events) = editable(cx);
    let mut t = at13("a", "please fix");
    t.entries.push(Entry::Draft(draft(
        5,
        Location::Line(13),
        "mine",
        DraftSync::Synced,
    )));
    give_threads(cx, &view, vec![t]);
    let row = thread_rows(cx, &view)[0];
    let click = |cx: &mut TestAppContext, id: &'static str| {
        cx.update_window(window, |_, window, cx| {
            window.click((id, row * 1000 + 1), cx);
        })
        .expect("click");
    };
    click(cx, "draft-delete");
    assert!(drafts(&events).is_empty(), "asked first");
    click(cx, "draft-delete-no");
    assert!(drafts(&events).is_empty());
    click(cx, "draft-delete");
    click(cx, "draft-delete-yes");
    assert_eq!(drafts(&events), ["draft Delete(LocalDraftId(5))"]);
    // The Edit button opens the editor.
    click(cx, "draft-edit");
    assert_eq!(text(cx, &view).as_deref(), Some("mine"));
}

#[gpui_kit::test]
fn a_left_comment_goes_to_the_parent_or_to_the_base_patch_set(cx: &mut TestAppContext) {
    // Row 5 (old14, nothing on the right) only has a left side.
    let go = |cx: &mut TestAppContext, base: Option<u32>| {
        let (window, view, events) = editable(cx);
        cx.update(|cx| {
            view.update(cx, |v, cx| {
                v.set_threads(thread_context(base, 2), std::sync::Arc::new(vec![]), cx)
            })
        });
        on_line_13(cx, window);
        press(cx, window, "j");
        assert_eq!(super::anchor(cx, &view), (Some(14), None));
        press(cx, window, "c");
        typed(cx, window, "gone");
        clear(&events);
        save_key(cx, window);
        drafts(&events)
    };
    let against_parent = go(cx, None);
    assert!(
        against_parent[0].contains("patch_set: PatchSetNumber(2)")
            && against_parent[0].contains("side: Parent")
            && against_parent[0].contains("Line(14)"),
        "{against_parent:?}"
    );
    let against_patch_set = go(cx, Some(1));
    assert!(
        against_patch_set[0].contains("patch_set: PatchSetNumber(1)")
            && against_patch_set[0].contains("side: Revision"),
        "{against_patch_set:?}"
    );
}

#[gpui_kit::test]
fn the_editor_survives_a_rebuild_with_its_text_and_focus(cx: &mut TestAppContext) {
    let (window, view, _) = editable(cx);
    on_line_13(cx, window);
    press(cx, window, "c");
    typed(cx, window, "work in progress");
    // New threads elsewhere change the rows (a rebuild).
    give_threads(
        cx,
        &view,
        vec![thread("late", Location::Line(41), "later", true)],
    );
    assert_eq!(text(cx, &view).as_deref(), Some("work in progress"));
    cx.update_window(window, |_, window, cx| {
        assert!(view.read(cx).editor_focused_for_test(window));
    })
    .expect("window");
    typed(cx, window, "!");
    assert_eq!(text(cx, &view).as_deref(), Some("work in progress!"));
}

#[gpui_kit::test]
fn without_sign_in_the_comment_keys_only_say_so(cx: &mut TestAppContext) {
    let (window, view, _) = open(cx, super::sample(), DiffViewMode::SideBySide);
    on_line_13(cx, window);
    press(cx, window, "c");
    assert_eq!(text(cx, &view), None);
    assert!(cx.update(|cx| view.read(cx).hint().is_some_and(|h| h.contains("Sign in"))));
}

/// 400 unchanged lines, then one changed line: nothing is folded with
/// the whole-file context.
fn long() -> model::Diff {
    let mut d = super::sample();
    d.chunks = vec![
        model::DiffChunk::Common {
            lines: (1..=400).map(|n| format!("c{n}")).collect(),
        },
        model::DiffChunk::Changed(model::ChangedChunk {
            a: vec!["old".into()],
            b: vec!["new".into()],
            ..Default::default()
        }),
    ];
    d
}

#[gpui_kit::test]
fn the_editor_keeps_its_focus_when_scrolled_out_of_view(cx: &mut TestAppContext) {
    let prefs = model::ResolvedDiffPreferences {
        context: model::DiffContext::All,
        ..Default::default()
    };
    let (window, view, _) = super::open_with(cx, long(), prefs);
    cx.update(|cx| view.update(cx, |v, cx| v.set_can_comment(true, cx)));
    press(cx, window, "c");
    typed(cx, window, "still here");
    // Far away from the editor's row.
    cx.update(|cx| view.update(cx, |v, cx| v.set_cursor(380, cx)));
    for _ in 0..3 {
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .expect("frame");
        cx.run_until_parked();
    }
    let top = cx.update(|cx| view.read(cx).top_row_for_test());
    assert!(top > 100, "scrolled away: {top}");
    cx.update_window(window, |_, window, cx| {
        assert!(view.read(cx).editor_focused_for_test(window));
    })
    .expect("window");
    assert_eq!(text(cx, &view).as_deref(), Some("still here"));
    // Typing still goes to the field.
    typed(cx, window, "!");
    assert_eq!(text(cx, &view).as_deref(), Some("still here!"));
}
