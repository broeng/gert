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
    ChangeNumber, ChangedChunk, Diff, DiffChangeType, DiffChunk, DiffViewMode,
    ResolvedDiffPreferences, RevisionId,
};

use super::{
    DiffEvent, DiffSizing, DiffView, Expand,
    layout::{Layout, LayoutOptions, Row},
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

/// Changes at line 13 and 39, a hidden region (old 16.., new 15..) between.
fn sample() -> Diff {
    Diff {
        meta_a: None,
        meta_b: None,
        change_type: Some(DiffChangeType::Modified),
        intraline_status: None,
        diff_header: Vec::new(),
        chunks: vec![
            DiffChunk::Skip { lines: 10 },
            DiffChunk::Common {
                lines: lines(&["c11", "c12"]),
            },
            DiffChunk::Changed(ChangedChunk {
                a: lines(&["old13", "old14"]),
                b: lines(&["new13"]),
                ..Default::default()
            }),
            DiffChunk::Common {
                lines: lines(&["c15"]),
            },
            DiffChunk::Skip { lines: 24 },
            DiffChunk::Changed(ChangedChunk {
                a: Vec::new(),
                b: lines(&["added40"]),
                ..Default::default()
            }),
            DiffChunk::Common {
                lines: lines(&["c41"]),
            },
        ],
        binary: false,
    }
}

/// `sample()` after asking for the whole file: the hidden lines are there.
fn sample_expanded() -> Diff {
    let mut d = sample();
    let fill = |from: u32, to: u32| DiffChunk::Common {
        lines: (from..=to).map(|n| format!("c{n}")).collect(),
    };
    d.chunks[4] = fill(16, 39);
    d.chunks[0] = fill(1, 10);
    d
}

fn prefs(mode: DiffViewMode) -> ResolvedDiffPreferences {
    ResolvedDiffPreferences {
        view_mode: mode,
        ..Default::default()
    }
}

type Events = Arc<Mutex<Vec<String>>>;

fn open(
    cx: &mut TestAppContext,
    diff: Diff,
    mode: DiffViewMode,
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
                let view = cx.new(|cx| DiffView::new(diff, prefs(mode), cx));
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
                DiffEvent::ExpandRequested(Expand::All) => "expand all".to_owned(),
                DiffEvent::ExpandRequested(Expand::Skip(s)) => {
                    format!(
                        "expand {}+{} @{}/{}",
                        s.old_start, s.lines, s.old_start, s.new_start
                    )
                }
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
    // rows: skip, c11, c12, old13/new13, old14/-, c15, skip, -/added40, c41
    assert_eq!(cursor(cx, &view), 0);
    press(cx, window, "k");
    assert_eq!(cursor(cx, &view), 0);
    press(cx, window, "j");
    press(cx, window, "j");
    assert_eq!(cursor(cx, &view), 2);
    press(cx, window, "down");
    press(cx, window, "up");
    assert_eq!(cursor(cx, &view), 2);
    for _ in 0..20 {
        press(cx, window, "j");
    }
    assert_eq!(cursor(cx, &view), 8);
    press(cx, window, "home");
    assert_eq!(cursor(cx, &view), 0);
    press(cx, window, "end");
    assert_eq!(cursor(cx, &view), 8);
}

#[gpui_kit::test]
fn n_and_p_jump_between_changed_chunks(cx: &mut TestAppContext) {
    let (window, view, _) = open(cx, sample(), DiffViewMode::SideBySide);
    press(cx, window, "n");
    assert_eq!(cursor(cx, &view), 3);
    assert_eq!(anchor(cx, &view), (Some(13), Some(13)));
    // Inside the first chunk `n` still goes to the next one.
    press(cx, window, "j");
    press(cx, window, "n");
    assert_eq!(cursor(cx, &view), 7);
    press(cx, window, "n"); // no further chunk: stay
    assert_eq!(cursor(cx, &view), 7);
    press(cx, window, "p");
    assert_eq!(cursor(cx, &view), 3);
    press(cx, window, "p");
    assert_eq!(cursor(cx, &view), 3);
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
    assert_eq!(
        *events.lock().expect("log"),
        ["mode Unified", "mode SideBySide"]
    );

    // On an inserted line the new-side number is what stays.
    press(cx, window, "n");
    assert_eq!(anchor(cx, &view), (None, Some(39)));
    press(cx, window, "m");
    assert_eq!(anchor(cx, &view), (None, Some(39)));
}

#[gpui_kit::test]
fn enter_on_a_skip_row_requests_expansion(cx: &mut TestAppContext) {
    let (window, view, events) = open(cx, sample(), DiffViewMode::SideBySide);
    press(cx, window, "enter"); // row 0 is the leading skip
    // A row that is not a skip does nothing.
    press(cx, window, "j");
    press(cx, window, "enter");
    press(cx, window, "shift-x");
    assert_eq!(
        *events.lock().expect("log"),
        ["expand 1+10 @1/1", "expand all"]
    );
    cx.update(|cx| assert_eq!(view.read(cx).cursor(), 1));
}

#[gpui_kit::test]
fn a_new_diff_keeps_the_cursor_on_its_line(cx: &mut TestAppContext) {
    let (window, view, _) = open(cx, sample(), DiffViewMode::SideBySide);
    // Rest on the second skip row (hidden lines 16..39).
    for _ in 0..6 {
        press(cx, window, "j");
    }
    assert_eq!(anchor(cx, &view), (Some(16), Some(15)));
    cx.update(|cx| view.update(cx, |v, cx| v.set_diff(sample_expanded(), cx)));
    cx.run_until_parked();
    // The hidden region is now lines, and the cursor is on its first line.
    assert_eq!(anchor(cx, &view), (Some(16), Some(15)));
    cx.update(|cx| {
        let v = view.read(cx);
        assert_eq!(v.layout().len(), 10 + 2 + 2 + 1 + 24 + 1 + 1);
        assert!(matches!(v.layout().rows()[v.cursor()], Row::Side { .. }));
    });

    // From a line that exists in both diffs it stays on that line.
    press(cx, window, "n");
    assert_eq!(anchor(cx, &view), (None, Some(39)));
    cx.update(|cx| view.update(cx, |v, cx| v.set_diff(sample(), cx)));
    assert_eq!(anchor(cx, &view), (None, Some(39)));
}

#[gpui_kit::test]
fn paging_and_scrolling_follow_the_cursor(cx: &mut TestAppContext) {
    let diff = fixture("large_diff.response.json");
    let (window, view, _) = open(cx, diff, DiffViewMode::SideBySide);
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
                },
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
    let opts = LayoutOptions::from_prefs(&ResolvedDiffPreferences::default());
    let mut best = std::time::Duration::MAX;
    for mode in [DiffViewMode::SideBySide, DiffViewMode::Unified] {
        let opts = LayoutOptions { mode, ..opts };
        let t = Instant::now();
        let l = Layout::build(&d, &opts);
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
