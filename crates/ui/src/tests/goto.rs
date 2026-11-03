//! The go-to dialog (`g`) and opening a web UI link.

use gpui_kit::{AnyWindowHandle, ClipboardItem};
use model::PatchSetNumber;

use super::*;
use crate::goto::LinkError;
use crate::nav::DiffRoute;

fn start(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Shell>) {
    let env = env_with(seeded(&[("status:open", &[1])]), &FakeHttpClient::new());
    let startup = Startup {
        env,
        diff: Default::default(),
        start_page: Default::default(),
        filters: Vec::new(),
        servers: vec![crate::session::ServerSetup {
            queries: vec![spec("Open", "status:open")],
            bookmarks: Vec::new(),
            ..setup()
        }],
        active: None,
    };
    let (window, shell) = open(cx, |window, cx| {
        cx.new(|cx| Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    (window, shell)
}

fn press(cx: &mut TestAppContext, window: AnyWindowHandle, key: &'static str) {
    cx.update_window(window, |_, window, cx| window.press(key, cx))
        .expect("press");
    cx.run_until_parked();
}

#[gpui_kit::test]
fn g_fills_in_the_clipboard_and_enter_opens_the_change(cx: &mut TestAppContext) {
    let (window, shell) = start(cx);
    cx.update(|cx| {
        cx.write_to_clipboard(ClipboardItem::new_string(
            "\n  https://g.example.com/c/proj/+/8/2..3  \nmore".into(),
        ))
    });
    press(cx, window, "g");
    let dialog = cx.update(|cx| shell.read(cx).goto_for_test().expect("dialog"));
    cx.update(|cx| {
        assert_eq!(
            dialog.read(cx).text_for_test(cx),
            "https://g.example.com/c/proj/+/8/2..3"
        )
    });
    press(cx, window, "enter");
    cx.update(|cx| {
        let shell = shell.read(cx);
        assert!(shell.goto_for_test().is_none());
        assert_eq!(shell.route(), &Route::Change(ChangeNumber(8)));
        let page = shell.change_page_for_test().expect("change page");
        let view = page.read(cx).view_state();
        assert_eq!(view.base, Some(PatchSetNumber(2)));
        assert_eq!(view.target, Some(PatchSetNumber(3)));
    });
    // Back leads to where we were.
    press(cx, window, "alt-left");
    cx.update(|cx| assert_eq!(shell.read(cx).route(), &Route::Dashboard));
}

#[gpui_kit::test]
fn a_bad_link_says_why_and_esc_closes(cx: &mut TestAppContext) {
    let (window, shell) = start(cx);
    press(cx, window, "g");
    let dialog = cx.update(|cx| shell.read(cx).goto_for_test().expect("dialog"));
    cx.update_window(window, |_, window, cx| {
        dialog.update(cx, |d, cx| {
            d.submit_for_test("https://other.example.com/c/p/+/1", window, cx)
        })
    })
    .expect("submit");
    cx.update(|cx| {
        assert_eq!(
            dialog.read(cx).error_for_test(),
            Some(LinkError::UnknownServer("https://other.example.com".into()))
        )
    });
    press(cx, window, "escape");
    cx.update(|cx| {
        assert!(shell.read(cx).goto_for_test().is_none());
        assert_eq!(shell.read(cx).route(), &Route::Dashboard);
    });
}

#[gpui_kit::test]
fn open_link_goes_to_the_diff_or_asks(cx: &mut TestAppContext) {
    let (window, shell) = start(cx);
    cx.update_window(window, |_, window, cx| {
        shell.update(cx, |s, cx| {
            s.open_link("https://g.example.com/c/proj/+/8/3/src/a.rs", window, cx)
        })
    })
    .expect("open link");
    cx.run_until_parked();
    cx.update(|cx| {
        assert_eq!(
            shell.read(cx).route(),
            &Route::Diff(DiffRoute {
                change: ChangeNumber(8),
                base: None,
                target: PatchSetNumber(3),
                path: "src/a.rs".into(),
                thread: None,
            })
        )
    });

    cx.update_window(window, |_, window, cx| {
        shell.update(cx, |s, cx| s.open_link("not a link", window, cx))
    })
    .expect("open link");
    cx.run_until_parked();
    cx.update(|cx| {
        let dialog = shell.read(cx).goto_for_test().expect("dialog");
        assert_eq!(dialog.read(cx).text_for_test(cx), "not a link");
        assert_eq!(
            dialog.read(cx).error_for_test(),
            Some(LinkError::NotAChange)
        );
    });
}
