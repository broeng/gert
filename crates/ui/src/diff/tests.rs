//! Tests of the diff view on GPUI's `TestAppContext`, and of layout
//! building against the fixtures.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

use gerrit_api::{Auth, GerritClient, fake::FakeHttpClient};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext, WindowOptions, px, size};
use model::{
    ChangeNumber, ChangedChunk, Diff, DiffChangeType, DiffChunk, DiffContext, DiffViewMode,
    ResolvedDiffPreferences, RevisionId,
};

use super::{
    DiffEvent, DiffSizing, DiffView, DraftRequest,
    layout::{Expansion, Layout, LayoutOptions, Row},
};

/// A fixture parsed through the real client, as the app would see it.
pub(crate) fn fixture(rel: &str) -> Diff {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(rel);
    let body = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let fake = FakeHttpClient::new();
    fake.push_response(200, body);
    let client = GerritClient::new(Arc::new(fake), "https://g.example.com", Auth::Anonymous)
        .expect("client");
    futures::executor::block_on(client.get_diff(
        ChangeNumber(1),
        &RevisionId::from("abc"),
        "f",
        &gerrit_api::DiffParams::default(),
    ))
    .expect("fixture parses")
}

fn lines(l: &[&str]) -> Vec<String> {
    l.iter().map(|s| (*s).to_owned()).collect()
}

/// A whole-file diff like the server sends it (it never folds): lines 1-12,
/// a 2:1 change at 13, 24 more lines, an insertion before the last 3 lines.
/// With a context of 3 the rows are (side by side): fold(9), c10-c12,
/// old13/new13, old14/-, c15-c17, fold(18), c36-c38, -/added, c39-c41.
fn sample() -> Diff {
    let common = |from: u32, to: u32| DiffChunk::Common {
        lines: (from..=to).map(|n| format!("c{n}")).collect(),
    };
    Diff {
        meta_a: None,
        meta_b: None,
        change_type: Some(DiffChangeType::Modified),
        intraline_status: None,
        diff_header: Vec::new(),
        chunks: vec![
            common(1, 12),
            DiffChunk::Changed(ChangedChunk {
                a: lines(&["old13", "old14"]),
                b: lines(&["new13"]),
                ..Default::default()
            }),
            common(15, 38),
            DiffChunk::Changed(ChangedChunk {
                a: Vec::new(),
                b: lines(&["added"]),
                ..Default::default()
            }),
            common(39, 41),
        ],
        binary: false,
    }
}

fn prefs(mode: DiffViewMode) -> ResolvedDiffPreferences {
    ResolvedDiffPreferences {
        view_mode: mode,
        context: DiffContext::Lines(3),
        ..Default::default()
    }
}

type Events = Arc<Mutex<Vec<String>>>;

fn open(
    cx: &mut TestAppContext,
    diff: Diff,
    mode: DiffViewMode,
) -> (AnyWindowHandle, Entity<DiffView>, Events) {
    open_with(cx, diff, prefs(mode))
}

fn open_with(
    cx: &mut TestAppContext,
    diff: Diff,
    prefs: ResolvedDiffPreferences,
) -> (AnyWindowHandle, Entity<DiffView>, Events) {
    open_made(cx, move |cx| DiffView::new(diff, prefs, cx))
}

fn open_made(
    cx: &mut TestAppContext,
    make: impl FnOnce(&mut gpui_kit::Context<DiffView>) -> DiffView,
) -> (AnyWindowHandle, Entity<DiffView>, Events) {
    let events: Events = Arc::default();
    let log = events.clone();
    let (window, view) = cx.update(|cx| {
        gpui_kit::init(cx);
        crate::init(cx);
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(gpui_kit::WindowBounds::Windowed(gpui_kit::Bounds {
                    origin: Default::default(),
                    size: size(px(1000.), px(400.)),
                })),
                ..Default::default()
            },
            cx,
            |window, cx| {
                let view = cx.new(make);
                let focus = view.read(cx).focus_for_test();
                window.focus(&focus, cx);
                view
            },
        )
        .expect("open window")
    });
    cx.update(|cx| {
        cx.subscribe(&view, move |_, ev: &DiffEvent, _| {
            let text = match ev {
                DiffEvent::RowsChanged => "rows".to_owned(),
                DiffEvent::ModeChanged(m) => format!("mode {m:?}"),
                DiffEvent::Draft(DraftRequest::Retry(id)) => format!("retry {id:?}"),
                DiffEvent::Draft(req) => format!("draft {req:?}"),
            };
            log.lock().expect("log").push(text);
        })
        .detach();
    });
    cx.run_until_parked();
    (window, view, events)
}

fn press(cx: &mut TestAppContext, window: AnyWindowHandle, key: &str) {
    cx.update_window(window, |_, window, cx| window.press(key, cx))
        .expect("press");
}

fn cursor(cx: &mut TestAppContext, view: &Entity<DiffView>) -> usize {
    cx.update(|cx| view.read(cx).cursor())
}

fn anchor(cx: &mut TestAppContext, view: &Entity<DiffView>) -> (Option<u32>, Option<u32>) {
    let a = cx.update(|cx| view.read(cx).cursor_anchor());
    (a.old, a.new)
}

#[gpui_kit::test]
fn j_and_k_move_the_cursor_and_stop_at_the_ends(cx: &mut TestAppContext) {
    let (window, view, _) = open(cx, sample(), DiffViewMode::SideBySide);
    // 17 rows, see `sample`.
    assert_eq!(cursor(cx, &view), 0);
    press(cx, window, "k");
    assert_eq!(cursor(cx, &view), 0);
    press(cx, window, "j");
    press(cx, window, "j");
    assert_eq!(cursor(cx, &view), 2);
    press(cx, window, "down");
    press(cx, window, "up");
    assert_eq!(cursor(cx, &view), 2);
    for _ in 0..30 {
        press(cx, window, "j");
    }
    assert_eq!(cursor(cx, &view), 16);
    press(cx, window, "home");
    assert_eq!(cursor(cx, &view), 0);
    press(cx, window, "end");
    assert_eq!(cursor(cx, &view), 16);
}

#[gpui_kit::test]
fn n_and_p_jump_between_changed_chunks(cx: &mut TestAppContext) {
    let (window, view, _) = open(cx, sample(), DiffViewMode::SideBySide);
    press(cx, window, "n");
    assert_eq!(cursor(cx, &view), 4);
    assert_eq!(anchor(cx, &view), (Some(13), Some(13)));
    // Inside the first chunk `n` still goes to the next one.
    press(cx, window, "j");
    press(cx, window, "n");
    assert_eq!(cursor(cx, &view), 13);
    press(cx, window, "n"); // no further chunk: stay
    assert_eq!(cursor(cx, &view), 13);
    press(cx, window, "p");
    assert_eq!(cursor(cx, &view), 4);
    press(cx, window, "p");
    assert_eq!(cursor(cx, &view), 4);
}

#[gpui_kit::test]
fn m_toggles_the_mode_and_keeps_the_line(cx: &mut TestAppContext) {
    let (window, view, events) = open(cx, sample(), DiffViewMode::SideBySide);
    press(cx, window, "n");
    press(cx, window, "j"); // old14 / blank
    assert_eq!(anchor(cx, &view), (Some(14), None));
    press(cx, window, "m");
    cx.update(|cx| assert_eq!(view.read(cx).mode(), DiffViewMode::Unified));
    // Still on the deleted line 14.
    assert_eq!(anchor(cx, &view), (Some(14), None));
    press(cx, window, "shift-m");
    cx.update(|cx| assert_eq!(view.read(cx).mode(), DiffViewMode::SideBySide));
    assert_eq!(anchor(cx, &view), (Some(14), None));
    let log = events.lock().expect("log").clone();
    assert_eq!(
        log.iter()
            .filter(|e| e.starts_with("mode"))
            .collect::<Vec<_>>(),
        ["mode Unified", "mode SideBySide"]
    );

    // On an inserted line the new-side number is what stays.
    press(cx, window, "n");
    assert_eq!(anchor(cx, &view), (None, Some(38)));
    press(cx, window, "m");
    assert_eq!(anchor(cx, &view), (None, Some(38)));
}

fn rows(cx: &mut TestAppContext, view: &Entity<DiffView>) -> usize {
    cx.update(|cx| view.read(cx).layout().len())
}

fn fold_lines(cx: &mut TestAppContext, view: &Entity<DiffView>) -> Vec<u32> {
    cx.update(|cx| {
        view.read(cx)
            .layout()
            .rows()
            .iter()
            .filter_map(|r| match r {
                Row::Fold(f) => Some(f.lines),
                _ => None,
            })
            .collect()
    })
}

#[gpui_kit::test]
fn the_server_diff_is_folded_by_the_context_preference(cx: &mut TestAppContext) {
    let (_, view, events) = open(cx, sample(), DiffViewMode::SideBySide);
    assert_eq!(rows(cx, &view), 17);
    assert_eq!(fold_lines(cx, &view), [9, 18]);
    // The owner is never asked for anything.
    assert!(events.lock().expect("log").is_empty());
}

#[gpui_kit::test]
fn enter_opens_the_fold_under_the_cursor_only(cx: &mut TestAppContext) {
    let (window, view, events) = open(cx, sample(), DiffViewMode::SideBySide);
    press(cx, window, "enter"); // row 0 is the leading fold
    assert_eq!(fold_lines(cx, &view), [18]);
    assert_eq!(rows(cx, &view), 17 - 1 + 9);
    // The cursor is on the first line that was hidden.
    assert_eq!(anchor(cx, &view), (Some(1), Some(1)));
    assert_eq!(*events.lock().expect("log"), ["rows"]);
    // On a line nothing happens.
    press(cx, window, "enter");
    assert_eq!(rows(cx, &view), 25);
    // `x` does the same as Enter.
    go_to_second_fold(cx, window, &view, 17);
    press(cx, window, "x");
    assert!(fold_lines(cx, &view).is_empty());
    assert_eq!(anchor(cx, &view), (Some(18), Some(17)));
}

fn go_to_second_fold(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    view: &Entity<DiffView>,
    presses: usize,
) {
    for _ in 0..presses {
        press(cx, window, "j");
    }
    assert_eq!(anchor(cx, view), (Some(18), Some(17)));
}

#[gpui_kit::test]
fn alt_up_and_down_reveal_ten_lines_and_keep_the_cursor_on_the_fold(cx: &mut TestAppContext) {
    let (window, view, _) = open(cx, sample(), DiffViewMode::SideBySide);
    go_to_second_fold(cx, window, &view, 9);
    press(cx, window, "alt-up");
    assert_eq!(fold_lines(cx, &view), [9, 8]);
    // Ten lines were added above the fold, the cursor stayed on the fold
    // so that the key can be pressed again.
    assert_eq!(cursor(cx, &view), 19);
    assert_eq!(anchor(cx, &view), (Some(28), Some(27)));
    // Fewer than ten lines left: they are all shown.
    press(cx, window, "alt-down");
    assert_eq!(fold_lines(cx, &view), [9]);
    assert_eq!(rows(cx, &view), 17 + 18 - 1);
}

#[gpui_kit::test]
fn the_edge_next_to_the_file_boundary_offers_nothing(cx: &mut TestAppContext) {
    let (window, view, _) = open(cx, sample(), DiffViewMode::SideBySide);
    // The first fold borders on the start of the file above.
    press(cx, window, "alt-up");
    assert_eq!(fold_lines(cx, &view), [9, 18]);
    cx.update(|cx| {
        assert_eq!(
            view.read(cx).hint(),
            Some("Nothing to show above this range")
        );
    });
    // Below it works (and the 9 hidden lines all fit in ten).
    press(cx, window, "alt-down");
    assert_eq!(fold_lines(cx, &view), [18]);
    // On a line: a hint, no change.
    press(cx, window, "alt-down");
    cx.update(|cx| assert_eq!(view.read(cx).hint(), Some("Not on a folded range")));
    assert_eq!(fold_lines(cx, &view), [18]);
}

#[gpui_kit::test]
fn shift_x_opens_every_fold_and_again_folds_them(cx: &mut TestAppContext) {
    let (window, view, events) = open(cx, sample(), DiffViewMode::SideBySide);
    press(cx, window, "n");
    press(cx, window, "j");
    let before = anchor(cx, &view);
    press(cx, window, "shift-x");
    assert!(fold_lines(cx, &view).is_empty());
    assert_eq!(rows(cx, &view), 41 + 1);
    cx.update(|cx| assert!(view.read(cx).all_expanded()));
    assert_eq!(anchor(cx, &view), before);
    assert_eq!(*events.lock().expect("log"), ["rows"]);
    press(cx, window, "shift-x");
    assert_eq!(fold_lines(cx, &view), [9, 18]);
    assert_eq!(anchor(cx, &view), before);
    cx.update(|cx| assert!(!view.read(cx).all_expanded()));
}

#[gpui_kit::test]
fn shift_x_without_folds_says_so(cx: &mut TestAppContext) {
    let all = ResolvedDiffPreferences {
        context: DiffContext::All,
        ..prefs(DiffViewMode::SideBySide)
    };
    let (window, view, events) = open_with(cx, sample(), all);
    cx.update(|cx| assert!(!view.read(cx).has_folds()));
    press(cx, window, "shift-x");
    assert!(events.lock().expect("log").is_empty());
    cx.update(|cx| {
        assert_eq!(
            view.read(cx).hint(),
            Some("The whole file is already shown")
        );
    });
    // The message goes away with the next move.
    press(cx, window, "j");
    cx.update(|cx| assert_eq!(view.read(cx).hint(), None));
}

#[gpui_kit::test]
fn opened_folds_survive_mode_and_diff_changes(cx: &mut TestAppContext) {
    let (window, view, _) = open(cx, sample(), DiffViewMode::SideBySide);
    press(cx, window, "enter");
    go_to_second_fold(cx, window, &view, 17);
    press(cx, window, "m");
    // The first fold stays open in unified mode (9 + 3 + ... rows).
    assert_eq!(fold_lines(cx, &view), [18]);
    let a = anchor(cx, &view);
    // The same diff again (e.g. the cache answered after a refetch).
    cx.update(|cx| view.update(cx, |v, cx| v.set_diff(sample(), cx)));
    assert_eq!(fold_lines(cx, &view), [18]);
    assert_eq!(anchor(cx, &view), a);
    // The owner can hand a remembered state to a new view.
    let kept: Expansion = cx.update(|cx| view.read(cx).expansion().clone());
    let (_, other, _) = open(cx, sample(), DiffViewMode::Unified);
    cx.update(|cx| other.update(cx, |v, cx| v.set_expansion(kept, cx)));
    assert_eq!(fold_lines(cx, &other), [18]);
}

#[gpui_kit::test]
fn changing_the_context_refolds_locally_and_keeps_opened_folds(cx: &mut TestAppContext) {
    let (window, view, events) = open(cx, sample(), DiffViewMode::SideBySide);
    press(cx, window, "enter");
    let set = |cx: &mut TestAppContext, c| {
        cx.update(|cx| {
            view.update(cx, |v, cx| {
                v.set_preferences(
                    ResolvedDiffPreferences {
                        context: c,
                        ..prefs(DiffViewMode::SideBySide)
                    },
                    cx,
                )
            })
        })
    };
    set(cx, DiffContext::All);
    assert!(fold_lines(cx, &view).is_empty());
    assert_eq!(rows(cx, &view), 42);
    set(cx, DiffContext::Lines(1));
    // 12 lines: the first run folds 11 but the user opened it before.
    assert_eq!(fold_lines(cx, &view), [22]);
    set(cx, DiffContext::Lines(3));
    assert_eq!(fold_lines(cx, &view), [18]);
    // Only layout changes: no event asks the owner for a diff.
    assert!(events.lock().expect("log").iter().all(|e| e == "rows"));
}

#[gpui_kit::test]
fn a_new_diff_keeps_the_cursor_on_its_line(cx: &mut TestAppContext) {
    let (window, view, _) = open(cx, sample(), DiffViewMode::SideBySide);
    press(cx, window, "n");
    press(cx, window, "n");
    assert_eq!(anchor(cx, &view), (None, Some(38)));
    // Another version of the diff (say, other whitespace handling) whose
    // line 38 is the same.
    let mut other = sample();
    if let DiffChunk::Common { lines } = &mut other.chunks[0] {
        lines[0] = "changed".into();
    }
    cx.update(|cx| view.update(cx, |v, cx| v.set_diff(other, cx)));
    assert_eq!(anchor(cx, &view), (None, Some(38)));
    cx.update(|cx| view.update(cx, |v, cx| v.set_diff(sample(), cx)));
    assert_eq!(anchor(cx, &view), (None, Some(38)));
}

#[gpui_kit::test]
fn server_skips_are_shown_but_cannot_hold_the_cursor(cx: &mut TestAppContext) {
    let mut d = sample();
    d.chunks.insert(0, DiffChunk::Skip { lines: 5 });
    let (window, view, _) = open(cx, d, DiffViewMode::SideBySide);
    cx.update(|cx| {
        let v = view.read(cx);
        assert!(matches!(v.layout().rows()[0], Row::Hidden(_)));
        assert_eq!(v.cursor(), 1);
    });
    press(cx, window, "k");
    assert_eq!(cursor(cx, &view), 1);
    press(cx, window, "enter"); // nothing to open
    assert_eq!(rows(cx, &view), 26);
}

#[gpui_kit::test]
fn paging_and_scrolling_follow_the_cursor(cx: &mut TestAppContext) {
    let diff = fixture("large_diff.response.json");
    let all = ResolvedDiffPreferences {
        context: DiffContext::All,
        ..prefs(DiffViewMode::SideBySide)
    };
    let (window, view, _) = open_with(cx, diff, all);
    let len = cx.update(|cx| view.read(cx).layout().len());
    assert!(len > 5_000, "{len}");
    press(cx, window, "space");
    let page = cursor(cx, &view);
    assert!((5..200).contains(&page), "page moved by {page}");
    press(cx, window, "ctrl-d");
    assert!(cursor(cx, &view) > page);
    press(cx, window, "pageup");
    press(cx, window, "ctrl-u");
    assert!(cursor(cx, &view) < page);
    press(cx, window, "end");
    assert!(cursor(cx, &view) >= len - 1);
    // The viewport followed the cursor to the end.
    let top = cx.update(|cx| view.read(cx).top_row_for_test());
    assert!(top + 200 > len && top > 0, "top {top} of {len}");
    press(cx, window, "home");
    assert_eq!(cursor(cx, &view), 0);
    let top = cx.update(|cx| view.read(cx).top_row_for_test());
    assert_eq!(top, 0);
    // Chunk jumps work over the whole file.
    for _ in 0..50 {
        press(cx, window, "n");
    }
    assert!(cursor(cx, &view) > 100);
    press(cx, window, "m");
    cx.update(|cx| assert_eq!(view.read(cx).mode(), DiffViewMode::Unified));
}

#[gpui_kit::test]
#[ignore = "timing guard, flaky under a parallel cargo test; run with --ignored"]
fn the_large_diff_scrolls_frame_by_frame(cx: &mut TestAppContext) {
    // Each key press moves the cursor and draws a frame of variable-height
    // rows, so this times what scrolling a 10k-line diff costs per frame.
    let all = ResolvedDiffPreferences {
        context: DiffContext::All,
        ..prefs(DiffViewMode::SideBySide)
    };
    let (window, view, _) = open_with(cx, fixture("large_diff.response.json"), all);
    let t = Instant::now();
    let frames = 200;
    for _ in 0..frames {
        press(cx, window, "space");
    }
    let per_frame = t.elapsed() / frames;
    eprintln!("large diff: {per_frame:?} per page-down frame (debug build)");
    assert!(cursor(cx, &view) > 1_000);
    // A debug build with the software test renderer: a guard against doing
    // work per row on every frame, not a benchmark.
    assert!(per_frame.as_millis() < 100, "{per_frame:?}");
}

#[gpui_kit::test]
fn inline_sizing_and_font_overrides_render(cx: &mut TestAppContext) {
    let (window, view, _) = open(cx, sample(), DiffViewMode::Unified);
    cx.update(|cx| {
        view.update(cx, |v, cx| {
            v.set_sizing(DiffSizing::Inline { max_rows: Some(5) }, cx);
            v.set_font_family(Some("monospace".into()), cx);
        });
    });
    press(cx, window, "j");
    assert_eq!(cursor(cx, &view), 1);
    cx.update(|cx| {
        view.update(cx, |v, cx| {
            v.set_sizing(DiffSizing::Inline { max_rows: None }, cx);
            v.set_sizing(DiffSizing::Fill, cx);
        });
    });
    cx.run_until_parked();
}

#[gpui_kit::test]
fn placeholder_diffs_have_no_cursor_row(cx: &mut TestAppContext) {
    let mut d = sample();
    d.binary = true;
    d.chunks.clear();
    let (window, view, _) = open(cx, d, DiffViewMode::SideBySide);
    press(cx, window, "j");
    press(cx, window, "n");
    press(cx, window, "m");
    cx.update(|cx| assert_eq!(view.read(cx).layout().len(), 1));
}

#[test]
fn fixtures_lay_out_in_both_modes() {
    for name in [
        "diff.response.json",
        "handwritten/diff_new_file.response.json",
        "handwritten/diff_deleted_file.response.json",
        "handwritten/diff_rename.response.json",
        "handwritten/diff_binary.response.json",
        "handwritten/diff_commit_msg.response.json",
        "handwritten/diff_whitespace_ignored.response.json",
        "handwritten/diff_whitespace_only.response.json",
    ] {
        let d = fixture(name);
        for mode in [DiffViewMode::SideBySide, DiffViewMode::Unified] {
            let l = Layout::build(
                &d,
                &LayoutOptions {
                    mode,
                    tab_size: 8,
                    show_tabs: true,
                    intraline: true,
                    context: DiffContext::Lines(3),
                    threads_only: false,
                },
                &Expansion::default(),
            );
            assert!(!l.is_empty(), "{name}");
            // Line numbers never go backwards.
            let mut last = 0;
            for r in l.rows() {
                if let Some(n) = r.new_number() {
                    assert!(n >= last, "{name} {mode:?}");
                    last = n;
                }
            }
        }
    }
}

#[test]
fn large_diff_layout_is_fast_and_numbered() {
    let d = fixture("large_diff.response.json");
    let opts = LayoutOptions {
        context: DiffContext::All,
        ..LayoutOptions::from_prefs(&ResolvedDiffPreferences::default())
    };
    let mut best = std::time::Duration::MAX;
    for mode in [DiffViewMode::SideBySide, DiffViewMode::Unified] {
        let opts = LayoutOptions { mode, ..opts };
        let t = Instant::now();
        let l = Layout::build(&d, &opts, &Expansion::default());
        let took = t.elapsed();
        best = best.min(took);
        eprintln!("large diff, {mode:?}: {} rows in {took:?}", l.len());
        assert!(l.len() > 5_000);
        // The last line numbers match the server's file sizes.
        let (old, new) = l.max_numbers();
        assert_eq!((old, new), (d.a_len(), d.b_len()));
        assert!(!l.chunks().is_empty());
    }
    // Generous: ~1 ms/1k lines in debug builds on a laptop. Guards against
    // an accidental quadratic step, not against noise.
    assert!(best.as_millis() < 1500, "{best:?}");
}

// --- comment threads ---------------------------------------------------

pub(crate) mod threads {
    use model::{
        Comment, Draft, DraftSync, Entry, LocalDraftId, Location, PatchSetNumber, Side, Thread,
        ThreadId,
    };

    use crate::testing::account;

    pub(crate) fn comment(
        id: &str,
        location: Location,
        message: &str,
        unresolved: bool,
    ) -> Comment {
        Comment {
            id: id.to_owned(),
            path: "f".into(),
            patch_set: PatchSetNumber(2),
            side: Side::Revision,
            parent: None,
            location,
            in_reply_to: None,
            message: message.to_owned(),
            author: Some(account(7, "Reviewer Rita")),
            updated: "2024-06-15T10:00:00Z".parse().expect("timestamp"),
            unresolved,
            commit_id: None,
            tag: None,
            change_message_id: None,
            robot_id: None,
            context: Vec::new(),
        }
    }

    /// A thread of one comment on patch set 2 of file `f`.
    pub(crate) fn thread(id: &str, location: Location, message: &str, unresolved: bool) -> Thread {
        let c = comment(id, location, message, unresolved);
        Thread {
            id: ThreadId::Comment(id.to_owned()),
            path: c.path.clone(),
            patch_set: c.patch_set,
            side: c.side,
            parent: None,
            location,
            entries: vec![Entry::Published(c)],
            orphaned: false,
        }
    }

    pub(crate) fn draft(n: i64, location: Location, message: &str, sync: DraftSync) -> Draft {
        Draft {
            local_id: LocalDraftId(n),
            server_id: None,
            change: model::ChangeNumber(1),
            patch_set: PatchSetNumber(2),
            revision: "sha2".into(),
            path: "f".into(),
            side: Side::Revision,
            parent: None,
            location,
            in_reply_to: None,
            message: message.to_owned(),
            unresolved: true,
            updated: "2024-06-15T10:05:00Z".parse().expect("timestamp"),
            sync,
            attempts: 0,
            last_error: None,
            version: 1,
        }
    }
}

use model::{DiffPair, DraftSync, Entry, Location, PatchSetNumber, Side, Thread, ThreadId};

use super::ThreadContext;
use threads::{draft, thread};

pub(crate) fn thread_context(base: Option<u32>, target: u32) -> ThreadContext {
    ThreadContext {
        pair: DiffPair {
            base: base.map(PatchSetNumber),
            target: PatchSetNumber(target),
        },
        path: "f".into(),
        old_path: None,
    }
}

pub(crate) fn give_threads(cx: &mut TestAppContext, view: &Entity<DiffView>, threads: Vec<Thread>) {
    cx.update(|cx| {
        view.update(cx, |v, cx| {
            v.set_threads(thread_context(None, 2), Arc::new(threads), cx)
        })
    });
    cx.run_until_parked();
}

fn thread_rows(cx: &mut TestAppContext, view: &Entity<DiffView>) -> Vec<usize> {
    cx.update(|cx| view.read(cx).layout().thread_rows().to_vec())
}

fn at13(id: &str, message: &str) -> Thread {
    thread(id, Location::Line(13), message, true)
}

#[gpui_kit::test]
fn j_and_k_step_over_thread_rows(cx: &mut TestAppContext) {
    let (window, view, _) = open(cx, sample(), DiffViewMode::SideBySide);
    give_threads(cx, &view, vec![at13("t", "please fix")]);
    // Row 4 is old13/new13, row 5 its thread.
    assert_eq!(thread_rows(cx, &view), [5]);
    assert_eq!(rows(cx, &view), 18);
    press(cx, window, "n");
    assert_eq!(cursor(cx, &view), 4);
    press(cx, window, "j");
    assert_eq!(cursor(cx, &view), 6);
    press(cx, window, "k");
    assert_eq!(cursor(cx, &view), 4);
    // The last line row is the end, not a thread row (here there is none
    // after it, so also check `end` after a thread at the very end).
    press(cx, window, "end");
    assert_eq!(cursor(cx, &view), 17);
    give_threads(
        cx,
        &view,
        vec![thread("e", Location::Line(41), "late", true)],
    );
    press(cx, window, "end");
    cx.update(|cx| {
        let v = view.read(cx);
        assert!(!matches!(v.layout().rows()[v.cursor()], Row::Thread(_)));
    });
    press(cx, window, "j");
    cx.update(|cx| {
        let v = view.read(cx);
        assert!(!matches!(v.layout().rows()[v.cursor()], Row::Thread(_)));
    });
}

#[gpui_kit::test]
fn the_initial_cursor_skips_threads_at_the_top(cx: &mut TestAppContext) {
    let (_, view, _) = open(cx, sample(), DiffViewMode::SideBySide);
    let anchor_before = anchor(cx, &view);
    give_threads(
        cx,
        &view,
        vec![thread("f", Location::File, "about the file", true)],
    );
    assert_eq!(thread_rows(cx, &view), [0]);
    // The cursor kept its line (the leading fold), now one row down.
    assert_eq!(cursor(cx, &view), 1);
    assert_eq!(anchor(cx, &view), anchor_before);
    let fresh = Layout::build_with_threads(
        &sample(),
        &LayoutOptions::from_prefs(&prefs(DiffViewMode::SideBySide)),
        &Expansion::default(),
        &[super::layout::ThreadSlot {
            side: model::DiffSide::Right,
            location: Location::File,
        }],
    );
    assert_eq!(fresh.first_selectable(), Some(1));
}

#[gpui_kit::test]
fn new_threads_keep_the_cursor_line_and_the_opened_folds(cx: &mut TestAppContext) {
    let (window, view, events) = open(cx, sample(), DiffViewMode::SideBySide);
    press(cx, window, "enter"); // open the first fold
    go_to_second_fold(cx, window, &view, 17);
    press(cx, window, "j"); // c36
    let line = anchor(cx, &view);
    let before = cursor(cx, &view);
    events.lock().expect("log").clear();
    give_threads(cx, &view, vec![at13("t", "x")]);
    assert_eq!(fold_lines(cx, &view), [18]);
    assert_eq!(anchor(cx, &view), line);
    // The thread row came in above the cursor.
    assert_eq!(cursor(cx, &view), before + 1);
    assert_eq!(*events.lock().expect("log"), ["rows"]);
    // And it goes away again the same way.
    give_threads(cx, &view, Vec::new());
    assert_eq!(cursor(cx, &view), before);
    assert_eq!(anchor(cx, &view), line);
    assert_eq!(fold_lines(cx, &view), [18]);
}

#[gpui_kit::test]
fn unchanged_places_only_remeasure_instead_of_rebuilding(cx: &mut TestAppContext) {
    let (_, view, events) = open(cx, sample(), DiffViewMode::SideBySide);
    give_threads(cx, &view, vec![at13("t", "one")]);
    let ptr = |cx: &mut TestAppContext| {
        cx.update(|cx| std::ptr::from_ref(view.read(cx).layout()) as usize)
    };
    let first = ptr(cx);
    events.lock().expect("log").clear();
    // The text changed, the place did not.
    give_threads(cx, &view, vec![at13("t", "two\nlines")]);
    assert_eq!(ptr(cx), first);
    assert_eq!(*events.lock().expect("log"), ["rows"]);
    // Nothing changed at all: nothing to tell.
    give_threads(cx, &view, vec![at13("t", "two\nlines")]);
    assert_eq!(*events.lock().expect("log"), ["rows"]);
    // Another place: rebuilt.
    give_threads(
        cx,
        &view,
        vec![thread("t", Location::Line(14), "two", true)],
    );
    assert_ne!(ptr(cx), first);
}

#[gpui_kit::test]
fn only_threads_of_this_file_and_pair_are_shown(cx: &mut TestAppContext) {
    let (_, view, _) = open(cx, sample(), DiffViewMode::SideBySide);
    let mut other_ps = at13("a", "other patch set");
    other_ps.patch_set = PatchSetNumber(1);
    let mut other_file = at13("b", "other file");
    other_file.path = "g".into();
    let mut parent = thread("c", Location::Line(14), "on the base", true);
    parent.side = Side::Parent;
    let threads = Arc::new(vec![other_ps, other_file, parent, at13("d", "mine")]);
    let shown = |cx: &mut TestAppContext, ctx: ThreadContext| {
        cx.update(|cx| {
            view.update(cx, |v, cx| v.set_threads(ctx, threads.clone(), cx));
            view.read(cx)
                .shown_threads()
                .map(|t| match &t.id {
                    ThreadId::Comment(id) => id.clone(),
                    ThreadId::Draft(d) => format!("{d:?}"),
                })
                .collect::<Vec<_>>()
        })
    };
    // Against the parent: the base-side thread and the one on patch set 2.
    assert_eq!(shown(cx, thread_context(None, 2)), ["c", "d"]);
    // Against patch set 1: its right-side thread shows on the left.
    assert_eq!(shown(cx, thread_context(Some(1), 2)), ["a", "d"]);
    // Another patch set: only its own threads.
    assert_eq!(shown(cx, thread_context(None, 1)), ["a"]);
}

#[gpui_kit::test]
fn threads_open_by_default_unless_resolved_and_the_choice_is_kept(cx: &mut TestAppContext) {
    let (_, view, _) = open(cx, sample(), DiffViewMode::Unified);
    let open_of = |cx: &mut TestAppContext, id: &str| {
        cx.update(|cx| view.read(cx).is_thread_open(&ThreadId::Comment(id.into())))
    };
    let mut with_draft = thread("d", Location::Line(14), "answered", false);
    with_draft.entries.push(Entry::Draft(draft(
        1,
        Location::Line(14),
        "my reply",
        DraftSync::PendingCreate,
    )));
    let all = vec![
        at13("open", "unresolved"),
        thread("done", Location::Line(15), "resolved", false),
        with_draft,
    ];
    give_threads(cx, &view, all.clone());
    assert!(open_of(cx, "open"));
    assert!(!open_of(cx, "done"));
    // A resolved thread with a draft of the user's stays open.
    assert!(open_of(cx, "d"));
    // A user's choice survives new data for the same thread.
    cx.update(|cx| {
        view.update(cx, |v, cx| {
            v.set_all_threads_open(true, cx);
        })
    });
    assert!(open_of(cx, "done"));
    give_threads(cx, &view, all);
    assert!(open_of(cx, "done"));
}

#[gpui_kit::test]
fn draft_states_render_in_both_modes(cx: &mut TestAppContext) {
    let (_, view, _) = open(cx, sample(), DiffViewMode::SideBySide);
    let mut t = at13("t", "reviewed\n\nwith a blank line");
    let mut pending = draft(1, Location::Line(13), "pending", DraftSync::PendingUpdate);
    pending.last_error = Some("offline".into());
    let failed = draft(
        2,
        Location::Line(13),
        "failed",
        DraftSync::Failed {
            op: model::DraftOp::Create,
            error: "HTTP 500".into(),
            retryable: true,
        },
    );
    let refused = draft(
        3,
        Location::Line(13),
        "refused",
        DraftSync::Failed {
            op: model::DraftOp::Create,
            error: "forbidden".into(),
            retryable: false,
        },
    );
    t.entries
        .extend([pending, failed, refused].map(Entry::Draft));
    give_threads(cx, &view, vec![t]);
    cx.update(|cx| view.update(cx, |v, cx| v.set_mode(DiffViewMode::Unified, cx)));
    cx.run_until_parked();
    let row = thread_rows(cx, &view)[0];
    let h = cx.update(|cx| view.read(cx).row_height_for_test(row));
    assert!(h.is_some_and(|h| h > px(60.)), "{h:?}");
}

// --- thread navigation and collapsing ------------------------------------

/// Threads at the new line 13 (open: unresolved), the old line 14 (closed:
/// resolved) and the new line 41, the last. With the sample's rows the
/// thread rows are 5, 7 and 19.
fn three_threads() -> Vec<Thread> {
    let mut left = thread("b", Location::Line(14), "on the base", false);
    left.side = Side::Parent;
    vec![
        at13("a", "first"),
        left,
        thread("c", Location::Line(41), "last", true),
    ]
}

fn hint(cx: &mut TestAppContext, view: &Entity<DiffView>) -> Option<String> {
    cx.update(|cx| view.read(cx).hint().map(str::to_owned))
}

fn is_open(cx: &mut TestAppContext, view: &Entity<DiffView>, id: &str) -> bool {
    cx.update(|cx| view.read(cx).is_thread_open(&ThreadId::Comment(id.into())))
}

#[gpui_kit::test]
fn shift_n_and_shift_p_visit_the_thread_rows(cx: &mut TestAppContext) {
    let (window, view, _) = open(cx, sample(), DiffViewMode::SideBySide);
    give_threads(cx, &view, three_threads());
    assert_eq!(thread_rows(cx, &view), [5, 7, 19]);
    assert_eq!(cursor(cx, &view), 0);
    press(cx, window, "shift-n");
    assert_eq!(cursor(cx, &view), 5);
    press(cx, window, "shift-n");
    assert_eq!(cursor(cx, &view), 7);
    press(cx, window, "shift-n");
    assert_eq!(cursor(cx, &view), 19);
    // The end: stay, and say so.
    press(cx, window, "shift-n");
    assert_eq!(cursor(cx, &view), 19);
    assert_eq!(hint(cx, &view).as_deref(), Some("No next comment thread"));
    press(cx, window, "shift-p");
    assert_eq!(cursor(cx, &view), 7);
    assert_eq!(hint(cx, &view), None);
    press(cx, window, "shift-p");
    press(cx, window, "shift-p");
    assert_eq!(cursor(cx, &view), 5);
    assert_eq!(
        hint(cx, &view).as_deref(),
        Some("No previous comment thread")
    );
    // From a thread row j and k go to the lines around it.
    press(cx, window, "j");
    assert_eq!(cursor(cx, &view), 6);
    press(cx, window, "shift-p");
    assert_eq!(cursor(cx, &view), 5);
    press(cx, window, "k");
    assert_eq!(cursor(cx, &view), 4);
    // From a code row the next thread is the one below it.
    press(cx, window, "j");
    assert_eq!(cursor(cx, &view), 6);
    press(cx, window, "shift-n");
    assert_eq!(cursor(cx, &view), 7);
    // Both modes, and the cursor on a thread survives a mode change.
    press(cx, window, "m");
    cx.update(|cx| {
        let v = view.read(cx);
        assert!(matches!(v.layout().rows()[v.cursor()], Row::Thread(_)));
    });
}

#[gpui_kit::test]
fn thread_keys_without_threads_say_so(cx: &mut TestAppContext) {
    let (window, view, _) = open(cx, sample(), DiffViewMode::SideBySide);
    press(cx, window, "shift-n");
    assert_eq!(hint(cx, &view).as_deref(), Some("No next comment thread"));
    press(cx, window, "e");
    assert_eq!(
        hint(cx, &view).as_deref(),
        Some("No comment threads in this file")
    );
    assert_eq!(cursor(cx, &view), 0);
}

#[gpui_kit::test]
fn e_and_shift_e_open_and_close_every_thread(cx: &mut TestAppContext) {
    let (window, view, events) = open(cx, sample(), DiffViewMode::SideBySide);
    give_threads(cx, &view, three_threads());
    assert!(is_open(cx, &view, "a"));
    assert!(!is_open(cx, &view, "b"));
    assert!(is_open(cx, &view, "c"));
    events.lock().expect("log").clear();
    press(cx, window, "shift-e");
    for id in ["a", "b", "c"] {
        assert!(!is_open(cx, &view, id), "{id}");
    }
    assert_eq!(*events.lock().expect("log"), ["rows"]);
    press(cx, window, "e");
    for id in ["a", "b", "c"] {
        assert!(is_open(cx, &view, id), "{id}");
    }
    // The choice survives new data for the same threads.
    press(cx, window, "shift-e");
    give_threads(cx, &view, three_threads());
    assert!(!is_open(cx, &view, "a"));
}

#[gpui_kit::test]
fn enter_and_x_toggle_the_thread_under_the_cursor(cx: &mut TestAppContext) {
    let (window, view, events) = open(cx, sample(), DiffViewMode::SideBySide);
    give_threads(cx, &view, three_threads());
    press(cx, window, "shift-n");
    assert_eq!(cursor(cx, &view), 5);
    events.lock().expect("log").clear();
    press(cx, window, "enter");
    assert!(!is_open(cx, &view, "a"));
    press(cx, window, "x");
    assert!(is_open(cx, &view, "a"));
    assert_eq!(*events.lock().expect("log"), ["rows", "rows"]);
    // The cursor stays on the thread, and the other threads are untouched.
    assert_eq!(cursor(cx, &view), 5);
    assert!(!is_open(cx, &view, "b"));
    // On a fold Enter still opens the fold; on a plain line it does nothing.
    press(cx, window, "home");
    press(cx, window, "enter");
    assert_eq!(fold_lines(cx, &view), [18]);
    press(cx, window, "j");
    press(cx, window, "enter");
    assert_eq!(fold_lines(cx, &view), [18]);
}

#[gpui_kit::test]
fn toggling_a_thread_changes_the_height_of_its_row(cx: &mut TestAppContext) {
    let (window, view, _) = open(cx, sample(), DiffViewMode::SideBySide);
    give_threads(
        cx,
        &view,
        vec![at13("a", "a long comment\n\nthat has\nseveral lines")],
    );
    let row = thread_rows(cx, &view)[0];
    let height = |cx: &mut TestAppContext| {
        cx.update(|cx| view.read(cx).row_height_for_test(row))
            .expect("measured")
    };
    let open = height(cx);
    press(cx, window, "shift-n");
    press(cx, window, "enter");
    let closed = height(cx);
    assert!(closed < open, "{closed:?} vs {open:?}");
    press(cx, window, "enter");
    assert_eq!(height(cx), open);
}

#[gpui_kit::test]
fn clicking_a_thread_header_toggles_it_and_retry_asks_the_owner(cx: &mut TestAppContext) {
    let (window, view, events) = open(cx, sample(), DiffViewMode::SideBySide);
    let mut t = at13("a", "text");
    t.entries.push(Entry::Draft(draft(
        2,
        Location::Line(13),
        "mine",
        DraftSync::Failed {
            op: model::DraftOp::Create,
            error: "HTTP 500".into(),
            retryable: true,
        },
    )));
    give_threads(cx, &view, vec![t]);
    let row = thread_rows(cx, &view)[0];
    cx.update_window(window, |_, window, cx| {
        window.click(("retry-draft", row * 1000 + 1), cx);
    })
    .expect("click retry");
    assert_eq!(
        events
            .lock()
            .expect("log")
            .iter()
            .filter(|e| e.starts_with("retry"))
            .count(),
        1
    );
    // The thread stayed open; the header click closes it and moves the
    // cursor there.
    assert!(is_open(cx, &view, "a"));
    cx.update_window(window, |_, window, cx| {
        window.click(("thread-header", row), cx);
    })
    .expect("click header");
    assert!(!is_open(cx, &view, "a"));
    assert_eq!(cursor(cx, &view), row);
}

mod editing;
