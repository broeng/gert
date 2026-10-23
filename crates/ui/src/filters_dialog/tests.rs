//! The Manage filters dialog in the shell on GPUI's `TestAppContext`, with
//! a backend that records the lists it is asked to save.

use std::sync::{Arc, Mutex};

use gerrit_api::fake::RouteHttpClient;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext, hsla, rgb};
use model::{ChangeNumber, FileStatus, FilesKey, PatchSetNumber, PatchSetRef};
use store::Store;

use super::{FiltersContext, FiltersDialog};
use crate::{
    filters::{FileFilter, FilterAction},
    nav::Route,
    session::{Backend, BackendError, SavedQuery, ServerSetup, Startup, StartupPage},
    shell::Shell,
    testing::{detail, file, summary},
    tests::{URL, env_with, open, setup},
};

const SAVE: &str = if cfg!(target_os = "macos") {
    "cmd-enter"
} else {
    "ctrl-enter"
};
const PATHS: [&str; 4] = ["/COMMIT_MSG", "gen/a.pb.go", "src/b.rs", "src/c.rs"];

/// Saves nothing anywhere; remembers every list and can refuse.
#[derive(Default)]
struct Recorder {
    saves: Mutex<Vec<Vec<FileFilter>>>,
    refuse: Mutex<Option<String>>,
}

impl Backend for Recorder {
    fn for_server(
        self: std::sync::Arc<Self>,
        _: &crate::ServerKey,
    ) -> std::sync::Arc<dyn crate::ServerBackend> {
        self
    }

    fn save_filters(&self, f: &[FileFilter]) -> Result<Vec<FileFilter>, BackendError> {
        if let Some(why) = self.refuse.lock().expect("lock").clone() {
            return Err(BackendError(why));
        }
        self.saves.lock().expect("lock").push(f.to_vec());
        Ok(f.to_vec())
    }

    fn save_startup_page(&self, p: &StartupPage) -> Result<StartupPage, BackendError> {
        Ok(p.clone())
    }

    fn save_advance_after_vote(&self, on: bool) -> Result<bool, BackendError> {
        Ok(on)
    }

    fn save_theme(
        &self,
        t: crate::theme::ThemeName,
    ) -> Result<crate::theme::ThemeName, BackendError> {
        Ok(t)
    }
}

impl crate::ServerBackend for Recorder {
    fn load_password(&self, _: &str, _: &str) -> Result<Option<String>, BackendError> {
        Ok(None)
    }

    fn save_queries(&self, _: &[SavedQuery]) -> Result<Vec<SavedQuery>, BackendError> {
        Err(BackendError("not used".into()))
    }

    fn save_login(
        &self,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<Vec<SavedQuery>, BackendError> {
        Err(BackendError("not used".into()))
    }
}

fn rule(name: &str, glob: &str, action: FilterAction) -> FileFilter {
    FileFilter {
        name: name.into(),
        globs: vec![glob.into()],
        projects: Vec::new(),
        action,
        enabled: true,
    }
}

struct Fx {
    window: AnyWindowHandle,
    shell: Entity<Shell>,
    backend: Arc<Recorder>,
}

fn ps(n: u32) -> PatchSetRef {
    PatchSetRef {
        number: PatchSetNumber(n),
        sha: format!("sha{n}").into(),
    }
}

/// Change 7 of `platform/foo` with four cached files, opened from the
/// dashboard in a shell that starts with `rules`.
fn on_the_change(cx: &mut TestAppContext, rules: Vec<FileFilter>) -> Fx {
    let mut store = Store::open_in_memory().expect("store");
    let server = store.server_id(URL).expect("server");
    let now = jiff::Timestamp::now();
    store
        .save_query_page(server, "status:open", &[summary(7)], false, now)
        .expect("query");
    store
        .save_change_detail(server, &detail(7, 3, 3), now)
        .expect("detail");
    let files: Vec<_> = PATHS
        .iter()
        .map(|p| file(p, FileStatus::Modified))
        .collect();
    let key = FilesKey {
        change: ChangeNumber(7),
        base: None,
        target: ps(3),
    };
    store
        .save_file_list(server, &key, &files, None, now)
        .expect("files");
    let backend = Arc::new(Recorder::default());
    let mut env = env_with(store, &RouteHttpClient::new());
    env.backend = backend.clone();
    let startup = Startup {
        env,
        diff: Default::default(),
        start_page: Default::default(),
        filters: rules,
        servers: vec![ServerSetup {
            queries: vec![SavedQuery {
                name: "Open".into(),
                query: "status:open".into(),
            }],
            bookmarks: Vec::new(),
            ..setup()
        }],
        active: None,
    };
    let (window, shell) = open(cx, |window, cx| {
        cx.new(|cx| Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    let fx = Fx {
        window,
        shell,
        backend,
    };
    press(cx, &fx, &["enter"]);
    cx.update(|cx| assert_eq!(fx.shell.read(cx).route(), &Route::Change(ChangeNumber(7))));
    fx
}

fn press(cx: &mut TestAppContext, f: &Fx, keys: &[&'static str]) {
    for k in keys {
        cx.update_window(f.window, |_, w, cx| w.press(k, cx))
            .expect("press");
        cx.run_until_parked();
    }
}

fn type_text(cx: &mut TestAppContext, f: &Fx, text: &str) {
    cx.update_window(f.window, |_, w, cx| w.input(text, cx))
        .expect("input");
    cx.run_until_parked();
}

fn dialog(cx: &mut TestAppContext, f: &Fx) -> Option<Entity<FiltersDialog>> {
    cx.update(|cx| f.shell.read(cx).filters_dialog_for_test())
}

fn open_dialog(cx: &mut TestAppContext, f: &Fx) -> Entity<FiltersDialog> {
    press(cx, f, &["f", "m"]);
    dialog(cx, f).expect("the dialog is open")
}

fn mode(cx: &mut TestAppContext, d: &Entity<FiltersDialog>) -> &'static str {
    cx.update(|cx| d.read(cx).mode_for_test())
}

fn names(cx: &mut TestAppContext, d: &Entity<FiltersDialog>) -> Vec<String> {
    cx.update(|cx| {
        d.read(cx)
            .rules_for_test()
            .iter()
            .map(|r| r.name.clone())
            .collect()
    })
}

fn saves(f: &Fx) -> Vec<Vec<FileFilter>> {
    f.backend.saves.lock().expect("lock").clone()
}

fn visible(cx: &mut TestAppContext, f: &Fx) -> Vec<String> {
    cx.update(|cx| {
        let page = f.shell.read(cx).change_page_for_test().expect("page");
        page.read(cx)
            .visible_paths()
            .into_iter()
            .map(str::to_owned)
            .collect()
    })
}

fn two_rules() -> Vec<FileFilter> {
    vec![
        rule("Gen", "*.pb.go", FilterAction::Dim),
        rule("Locks", "*.lock", FilterAction::Hide),
    ]
}

#[gpui_kit::test]
fn f_m_opens_the_dialog_over_the_page_and_escape_gives_the_page_back(cx: &mut TestAppContext) {
    let f = on_the_change(cx, two_rules());
    let d = open_dialog(cx, &f);
    assert_eq!(mode(cx, &d), "browsing");
    // The keys of the shell and the page do nothing behind it.
    press(cx, &f, &["?", "u", "m", "a"]);
    cx.update(|cx| {
        assert!(!f.shell.read(cx).help_open_for_test());
        assert_eq!(f.shell.read(cx).route(), &Route::Change(ChangeNumber(7)));
        assert!(f.shell.read(cx).menu_for_test().is_none());
        assert!(f.shell.read(cx).reply_for_test().is_none());
    });
    // `a` was the dialog's: it is adding now.
    assert_eq!(mode(cx, &d), "adding");
    press(cx, &f, &["escape", "escape"]);
    assert!(dialog(cx, &f).is_none());
    // The page has the focus again.
    press(cx, &f, &["f", "m"]);
    assert!(dialog(cx, &f).is_some());
}

#[gpui_kit::test]
fn a_rule_added_from_a_change_is_scoped_to_its_project_and_hides_its_files(
    cx: &mut TestAppContext,
) {
    let f = on_the_change(cx, Vec::new());
    assert_eq!(visible(cx, &f), PATHS);
    let d = open_dialog(cx, &f);
    press(cx, &f, &["a"]);
    assert_eq!(mode(cx, &d), "adding");
    cx.update(|cx| {
        let form = d.read(cx).form_for_test(cx);
        assert_eq!(form.projects, "platform/foo");
        assert!(form.enabled);
    });
    // An untouched form says nothing yet.
    cx.update(|cx| assert_eq!(d.read(cx).problem_for_test(), None));

    type_text(cx, &f, "Generated");
    press(cx, &f, &["tab"]);
    type_text(cx, &f, "*.pb.go");
    cx.update(|cx| {
        assert_eq!(
            d.read(cx).preview_for_test().as_deref(),
            Some("Matches 1 of 3 files in this change")
        );
        assert_eq!(d.read(cx).problem_for_test(), None);
    });
    press(cx, &f, &[SAVE]);

    assert_eq!(mode(cx, &d), "browsing");
    let saved = saves(&f);
    assert_eq!(saved.len(), 1);
    assert_eq!(
        saved[0],
        [FileFilter {
            name: "Generated".into(),
            globs: vec!["*.pb.go".into()],
            projects: vec!["platform/foo".into()],
            action: FilterAction::Hide,
            enabled: true,
        }]
    );
    assert_eq!(names(cx, &d), ["Generated"]);
    assert_eq!(cx.update(|cx| d.read(cx).cursor_for_test()), 0);
    // The page behind the dialog restyled at once.
    assert_eq!(visible(cx, &f), ["/COMMIT_MSG", "src/b.rs", "src/c.rs"]);
}

#[gpui_kit::test]
fn an_invalid_form_shows_why_and_is_not_saved(cx: &mut TestAppContext) {
    let f = on_the_change(cx, two_rules());
    let d = open_dialog(cx, &f);
    press(cx, &f, &["a"]);
    type_text(cx, &f, "Bad");
    press(cx, &f, &["tab"]);
    type_text(cx, &f, "[");
    cx.update(|cx| {
        let why = d.read(cx).problem_for_test().expect("a problem");
        assert!(why.starts_with("Invalid glob \"[\""), "{why}");
    });
    press(cx, &f, &[SAVE]);
    assert_eq!(mode(cx, &d), "adding");
    assert!(saves(&f).is_empty());

    // A name that is taken is refused too.
    press(
        cx,
        &f,
        &["shift-tab", "backspace", "backspace", "backspace"],
    );
    type_text(cx, &f, "Gen");
    cx.update(|cx| {
        let why = d.read(cx).problem_for_test().expect("a problem");
        assert!(why.contains("already a filter named"), "{why}");
    });
    assert!(saves(&f).is_empty());
}

#[gpui_kit::test]
fn deleting_a_rule_needs_a_yes(cx: &mut TestAppContext) {
    let f = on_the_change(cx, two_rules());
    let d = open_dialog(cx, &f);
    press(cx, &f, &["d"]);
    assert_eq!(mode(cx, &d), "confirming");
    assert!(saves(&f).is_empty());
    press(cx, &f, &["n"]);
    assert_eq!(mode(cx, &d), "browsing");
    assert!(saves(&f).is_empty());
    // Escape is a no as well, and keeps the dialog open.
    press(cx, &f, &["d", "escape"]);
    assert_eq!(mode(cx, &d), "browsing");
    assert!(dialog(cx, &f).is_some());

    press(cx, &f, &["d", "y"]);
    assert_eq!(saves(&f), [[two_rules()[1].clone()]]);
    assert_eq!(names(cx, &d), ["Locks"]);
    assert_eq!(mode(cx, &d), "browsing");
}

#[gpui_kit::test]
fn shift_j_and_k_reorder_and_save(cx: &mut TestAppContext) {
    let f = on_the_change(cx, two_rules());
    let d = open_dialog(cx, &f);
    press(cx, &f, &["shift-k"]); // already first: nothing
    assert!(saves(&f).is_empty());
    press(cx, &f, &["shift-j"]);
    assert_eq!(names(cx, &d), ["Locks", "Gen"]);
    assert_eq!(cx.update(|cx| d.read(cx).cursor_for_test()), 1);
    let saved = saves(&f);
    assert_eq!(saved.len(), 1);
    assert_eq!(
        saved[0].iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        ["Locks", "Gen"]
    );
    press(cx, &f, &["shift-k"]);
    assert_eq!(names(cx, &d), ["Gen", "Locks"]);
    assert_eq!(cx.update(|cx| d.read(cx).cursor_for_test()), 0);
}

#[gpui_kit::test]
fn j_and_k_move_and_space_switches_the_default(cx: &mut TestAppContext) {
    let f = on_the_change(cx, two_rules());
    let d = open_dialog(cx, &f);
    press(cx, &f, &["j", "j"]);
    assert_eq!(cx.update(|cx| d.read(cx).cursor_for_test()), 1);
    press(cx, &f, &["k"]);
    assert_eq!(cx.update(|cx| d.read(cx).cursor_for_test()), 0);
    press(cx, &f, &["space"]);
    let saved = saves(&f);
    assert_eq!(saved.len(), 1);
    assert!(!saved[0][0].enabled && saved[0][1].enabled);
    // The page shows the new default (the session had no override).
    press(cx, &f, &["space"]);
    assert!(saves(&f)[1][0].enabled);
}

#[gpui_kit::test]
fn escape_in_the_form_drops_the_edits_and_a_second_one_closes(cx: &mut TestAppContext) {
    let f = on_the_change(cx, two_rules());
    let d = open_dialog(cx, &f);
    press(cx, &f, &["e"]);
    assert_eq!(mode(cx, &d), "editing");
    type_text(cx, &f, "xyz");
    cx.update(|cx| assert_eq!(d.read(cx).form_for_test(cx).name, "Genxyz"));
    press(cx, &f, &["escape"]);
    assert_eq!(mode(cx, &d), "browsing");
    assert!(dialog(cx, &f).is_some());
    cx.update(|cx| assert_eq!(d.read(cx).form_for_test(cx).name, "Gen"));
    assert!(saves(&f).is_empty());
    press(cx, &f, &["escape"]);
    assert!(dialog(cx, &f).is_none());
}

#[gpui_kit::test]
fn a_refused_write_is_shown_and_keeps_the_dialog_as_it_was(cx: &mut TestAppContext) {
    let f = on_the_change(cx, two_rules());
    *f.backend.refuse.lock().expect("lock") = Some("disk full".into());
    let d = open_dialog(cx, &f);
    press(cx, &f, &["shift-j"]);
    cx.update(|cx| {
        assert_eq!(
            d.read(cx).error_for_test(),
            Some("Could not save the filters: disk full")
        );
    });
    assert!(dialog(cx, &f).is_some());
    assert_eq!(names(cx, &d), ["Gen", "Locks"]);
    assert_eq!(cx.update(|cx| d.read(cx).cursor_for_test()), 0);

    // The same in the form: it stays open with what was typed.
    press(cx, &f, &["e"]);
    type_text(cx, &f, "2");
    press(cx, &f, &[SAVE]);
    assert_eq!(mode(cx, &d), "editing");
    cx.update(|cx| {
        assert!(d.read(cx).error_for_test().is_some());
        assert_eq!(d.read(cx).form_for_test(cx).name, "Gen2");
    });
    // Once the disk has room, saving works.
    *f.backend.refuse.lock().expect("lock") = None;
    press(cx, &f, &[SAVE]);
    assert_eq!(mode(cx, &d), "browsing");
    assert_eq!(names(cx, &d), ["Gen2", "Locks"]);
    cx.update(|cx| assert_eq!(d.read(cx).error_for_test(), None));
}

#[gpui_kit::test]
fn a_highlight_takes_its_color_and_bold_from_the_form(cx: &mut TestAppContext) {
    let f = on_the_change(cx, vec![rule("Gen", "*.pb.go", FilterAction::Dim)]);
    let d = open_dialog(cx, &f);
    press(cx, &f, &["e"]);
    // name, globs, projects, only this project, the action
    press(cx, &f, &["tab", "tab", "tab", "tab", "right", "right"]);
    cx.update(|cx| assert_eq!(d.read(cx).form_for_test(cx).kind, super::Kind::Highlight));
    let picker = cx.update(|cx| d.read(cx).picker_for_test());
    cx.update_window(f.window, |_, w, cx| {
        picker.update(cx, |p, cx| p.select_color(hsla(0., 1., 0.5, 1.), w, cx));
    })
    .expect("window");
    cx.run_until_parked();
    // picker, then bold: Enter presses the focused tick box
    press(cx, &f, &["tab", "tab", "enter"]);
    press(cx, &f, &[SAVE]);
    let saved = saves(&f);
    assert_eq!(saved.len(), 1);
    assert_eq!(
        saved[0][0].action,
        FilterAction::Highlight {
            color: Some(rgb(0xff0000)),
            bold: true
        }
    );
}

#[gpui_kit::test]
fn only_this_project_fills_in_the_changes_project(cx: &mut TestAppContext) {
    let mut scoped = rule("Gen", "*.pb.go", FilterAction::Dim);
    scoped.projects = vec!["other/a".into(), "other/b".into()];
    let f = on_the_change(cx, vec![scoped]);
    let d = open_dialog(cx, &f);
    press(cx, &f, &["e", "tab", "tab", "tab", "enter"]);
    cx.update(|cx| assert_eq!(d.read(cx).form_for_test(cx).projects, "platform/foo"));
    press(cx, &f, &[SAVE]);
    assert_eq!(saves(&f)[0][0].projects, ["platform/foo"]);
}

#[gpui_kit::test]
fn the_dialog_opens_without_a_change_on_a_named_rule(cx: &mut TestAppContext) {
    let f = on_the_change(cx, two_rules());
    cx.update_window(f.window, |_, w, cx| {
        f.shell.update(cx, |s, cx| {
            s.open_filters_dialog(
                FiltersContext {
                    select: Some("Locks".into()),
                    ..Default::default()
                },
                w,
                cx,
            )
        })
    })
    .expect("window");
    cx.run_until_parked();
    let d = dialog(cx, &f).expect("open");
    assert_eq!(cx.update(|cx| d.read(cx).cursor_for_test()), 1);
    // No change: no preview, and a new rule has no project.
    press(cx, &f, &["a"]);
    cx.update(|cx| {
        assert_eq!(d.read(cx).preview_for_test(), None);
        assert_eq!(d.read(cx).form_for_test(cx).projects, "");
    });
    // Only one modal at a time.
    press(cx, &f, &["escape", "escape"]);
    assert!(dialog(cx, &f).is_none());
}

fn color_field_opens_on(cx: &mut TestAppContext, key: &'static str) {
    {
        let f = on_the_change(cx, vec![rule("Gen", "*.pb.go", FilterAction::Dim)]);
        let d = open_dialog(cx, &f);
        press(
            cx,
            &f,
            &["e", "tab", "tab", "tab", "tab", "right", "right", "tab"],
        );
        let picker = cx.update(|cx| d.read(cx).picker_for_test());
        cx.update(|cx| assert!(!picker.read(cx).is_open()));
        press(cx, &f, &[key]);
        cx.update(|cx| assert!(picker.read(cx).is_open(), "{key}"));
        press(cx, &f, &["escape"]);
        cx.update(|cx| assert!(!picker.read(cx).is_open()));
        // Esc closed the popover only, not the form.
        assert_eq!(mode(cx, &d), "editing");
    }
}

#[gpui_kit::test]
fn enter_on_the_color_field_opens_the_picker(cx: &mut TestAppContext) {
    color_field_opens_on(cx, "enter");
}

#[gpui_kit::test]
fn space_on_the_color_field_opens_the_picker(cx: &mut TestAppContext) {
    color_field_opens_on(cx, "space");
}
