//! The reply dialog's local reviews as the shell handles them.

use gerrit_api::fake::FakeHttpClient;
use gpui_kit::{AppContext as _, TestAppContext};
use model::ChangeNumber;
use store::Store;

use super::{URL, env_with, open, setup};
use crate::{
    nav::Route,
    reply::ReplyRequest,
    session::{Session, Startup},
    shell::Shell,
};

#[gpui_kit::test]
fn a_saved_local_review_closes_the_dialog_and_the_page_says_so(cx: &mut TestAppContext) {
    let mut store = Store::open_in_memory().expect("store");
    let server = store.server_id(URL).expect("server");
    let mut d = crate::testing::detail(7, 1, 1);
    d.summary.project = "p".into();
    d.permitted_labels = [("Code-Review".to_owned(), vec![-1, 0, 1])].into();
    store
        .save_change_detail(server, &d, jiff::Timestamp::now())
        .expect("detail");
    let version = crate::testing::local_version(&mut store, Some(ChangeNumber(7)), "I7", "l7");
    let http = FakeHttpClient::new();
    let env = env_with(store, &http);
    let detail = {
        let session = Session::new(&env, &setup(), None).expect("session");
        crate::change_detail::cached(session.syncer(), ChangeNumber(7))
            .expect("store")
            .expect("cached")
            .detail
    };
    let startup = Startup {
        env,
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
    cx.update_window(window, |_, window, cx| {
        shell.update(cx, |s, cx| {
            s.navigate_for_test(Route::Change(ChangeNumber(7)), window, cx);
            s.open_reply_for_test(
                ReplyRequest {
                    detail,
                    target: Some(version.number),
                    edit: None,
                    strategy: model::LocalApproval::Exact,
                },
                window,
                cx,
            );
        })
    })
    .expect("open");
    cx.run_until_parked();
    let dialog = cx
        .update(|cx| shell.read(cx).reply_for_test())
        .expect("the dialog");
    cx.update_window(window, |_, window, cx| {
        dialog.update(cx, |d, cx| {
            d.set_text_for_test("Looks right", window, cx);
            d.send(window, cx);
        })
    })
    .expect("send");
    cx.run_until_parked();
    cx.update(|cx| {
        let s = shell.read(cx);
        assert!(s.reply_for_test().is_none(), "the dialog closed");
        let page = s.change_page_for_test().expect("change page");
        assert_eq!(
            page.read(cx).hint_for_test().map(|(t, _)| t),
            Some(crate::reply::local_note("local 1"))
        );
    });
    // The page reads Gerrit; nothing is written there.
    let writes: Vec<_> = http
        .requests()
        .into_iter()
        .filter(|r| r.method() != "GET")
        .collect();
    assert!(writes.is_empty(), "{writes:?}");
}
