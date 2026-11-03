//! The relation chain across pages.

use gpui_kit::{AnyWindowHandle, ScrollHandle, VisualTestContext};

use super::*;
use crate::pages::change::ChangePage;

/// Changes 100..100+n in one chain (100 the tip), each with a cached
/// detail; only `at`'s chain is cached, so the others' pages show the
/// chain they are handed until a fetch that never succeeds.
fn start(cx: &mut TestAppContext, n: u64, at: u64) -> (AnyWindowHandle, Entity<Shell>) {
    let mut store = seeded(&[]);
    let ts = "2024-06-15T10:05:00Z".parse().expect("ts");
    let chain: Vec<model::RelatedChange> = (0..n)
        .map(|i| model::RelatedChange {
            project: "p".into(),
            change_id: None,
            number: ChangeNumber(100 + i),
            patch_set: model::PatchSetNumber(1),
            current_patch_set: Some(model::PatchSetNumber(1)),
            status: Some(model::ChangeStatus::New),
            submittable: None,
            work_in_progress: false,
            commit: None,
        })
        .collect();
    for i in 0..n {
        store
            .save_change_detail(ServerId(1), &crate::testing::detail(100 + i, 1, 1), ts)
            .expect("detail");
    }
    store
        .save_related(ServerId(1), ChangeNumber(at), &"sha1".into(), &chain, ts)
        .expect("related");
    let startup = Startup {
        env: env_with(store, &FakeHttpClient::new()),
        diff: Default::default(),
        start_page: Default::default(),
        filters: Vec::new(),
        servers: vec![setup()],
        active: None,
    };
    let (window, shell) = open(cx, |window, cx| {
        cx.new(|cx| Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    go(cx, window, &shell, at);
    (window, shell)
}

fn go(cx: &mut TestAppContext, window: AnyWindowHandle, shell: &Entity<Shell>, n: u64) {
    cx.update_window(window, |_, window, cx| {
        shell.update(cx, |s, cx| {
            s.navigate_for_test(Route::Change(ChangeNumber(n)), window, cx)
        })
    })
    .expect("navigate");
    cx.run_until_parked();
}

fn press(cx: &mut TestAppContext, window: AnyWindowHandle, key: &'static str) {
    cx.update_window(window, |_, window, cx| window.press(key, cx))
        .expect("press");
}

fn page(cx: &mut TestAppContext, shell: &Entity<Shell>) -> Entity<ChangePage> {
    cx.update(|cx| shell.read(cx).change_page_for_test().expect("change page"))
}

fn scroll(cx: &mut TestAppContext, shell: &Entity<Shell>) -> ScrollHandle {
    let page = page(cx, shell);
    cx.update(|cx| page.read(cx).related_scroll_for_test())
}

/// How far row `ix`'s middle sits from the middle of the scroll box.
fn off_centre(s: &ScrollHandle, ix: usize) -> f32 {
    let row = s.bounds_for_item(ix).expect("row").center().y + s.offset().y;
    (row - s.bounds().center().y).as_f32().abs()
}

#[gpui_kit::test]
fn the_box_keeps_the_current_change_in_the_middle_while_stepping(cx: &mut TestAppContext) {
    let (window, shell) = start(cx, 40, 120);
    let s = scroll(cx, &shell);
    assert!(s.max_offset().y > px(0.), "the chain overflows its box");
    assert!(off_centre(&s, 20) < 1., "{}", off_centre(&s, 20));

    press(cx, window, "shift-j");
    cx.run_until_parked();
    let s = scroll(cx, &shell);
    cx.update(|cx| assert_eq!(shell.read(cx).route(), &Route::Change(ChangeNumber(121))));
    assert!(off_centre(&s, 21) < 1., "{}", off_centre(&s, 21));
}

#[gpui_kit::test]
fn the_panel_keeps_its_chain_and_scroll_position_while_stepping(cx: &mut TestAppContext) {
    let (window, shell) = start(cx, 80, 140);
    VisualTestContext::from_window(window, cx).simulate_resize(size(px(1400.), px(800.)));
    cx.run_until_parked();
    press(cx, window, "shift-l");
    cx.run_until_parked();
    let p = page(cx, &shell);
    assert!(cx.update(|cx| p.read(cx).panel_shown_for_test()));
    let s = scroll(cx, &shell);
    assert!(s.max_offset().y > px(0.), "the chain overflows the panel");
    // The user scrolls a little; the current row stays in view.
    let row = s.bounds_for_item(40).expect("row").size.height;
    s.set_offset(gpui_kit::point(px(0.), s.offset().y - row));
    cx.run_until_parked();
    let before = s.offset().y;

    // The new page has the chain at once, before reading its own.
    press(cx, window, "shift-j");
    let next = page(cx, &shell);
    assert_ne!(next, p);
    assert_eq!(cx.update(|cx| next.read(cx).chain_len_for_test()), 80);
    cx.run_until_parked();
    assert!(cx.update(|cx| next.read(cx).panel_shown_for_test()));
    assert_eq!(scroll(cx, &shell).offset().y, before);

    // A change outside the chain starts afresh.
    go(cx, window, &shell, 7);
    let fresh = page(cx, &shell);
    assert_eq!(cx.update(|cx| fresh.read(cx).chain_len_for_test()), 0);
}
