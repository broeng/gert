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
    DiffEvent, DiffSizing, DiffView,
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
                let view = cx.new(|cx| DiffView::new(diff, prefs, cx));
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
