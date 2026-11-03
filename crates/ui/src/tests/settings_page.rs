//! The settings page.

use std::sync::Arc;

use gerrit_api::fake::FakeHttpClient;
use gpui_kit::component::input::InputState;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
use model::ChangeNumber;

use super::{env_with, open, seeded, setup, spec};
use crate::{
    filters::{FileFilter, FilterAction},
    login::tests::FakeBackend,
    nav::Route,
    pages::settings::SettingsTab,
    session::{Env, Startup, StartupPage},
    shell::Shell,
};

struct Fixture {
    window: AnyWindowHandle,
    shell: Entity<Shell>,
    backend: Arc<FakeBackend>,
}

fn fixture(cx: &mut TestAppContext, fail: bool) -> Fixture {
    fixture_at(cx, fail, StartupPage::Dashboard)
}

fn fixture_at(cx: &mut TestAppContext, fail: bool, start_page: StartupPage) -> Fixture {
    fixture_with(cx, fail, start_page, Vec::new())
}

fn fixture_with(
    cx: &mut TestAppContext,
    fail: bool,
    start_page: StartupPage,
    filters: Vec<FileFilter>,
) -> Fixture {
    fixture_full(cx, fail, start_page, filters, Vec::new())
}

fn fixture_full(
    cx: &mut TestAppContext,
    fail: bool,
    start_page: StartupPage,
    filters: Vec<FileFilter>,
    bookmarks: Vec<crate::session::SavedQuery>,
) -> Fixture {
    fixture_all(cx, fail, start_page, filters, bookmarks, Vec::new(), None)
}

/// `username`: `None` is an anonymous server.
fn fixture_all(
    cx: &mut TestAppContext,
    fail: bool,
    start_page: StartupPage,
    filters: Vec<FileFilter>,
    bookmarks: Vec<crate::session::SavedQuery>,
    projects: Vec<crate::session::ProjectSettings>,
    username: Option<&str>,
) -> Fixture {
    let backend = Arc::new(FakeBackend {
        fail,
        password: username.map(|_| "pw".to_owned()),
        ..Default::default()
    });
    let env = Env {
        backend: backend.clone(),
        ..env_with(
            seeded(&[("status:open", &[1]), ("owner:self", &[2])]),
            &FakeHttpClient::new(),
        )
    };
    let startup = Startup {
        env,
        diff: Default::default(),
        start_page,
        filters,
        servers: vec![crate::session::ServerSetup {
            queries: vec![spec("Open", "status:open"), spec("Mine", "owner:self")],
            bookmarks,
            projects,
            username: username.map(Into::into),
            ..setup()
        }],
        active: None,
    };
    let (window, shell) = open(cx, |window, cx| {
        cx.new(|cx| Shell::new(startup, window, cx))
    });
    cx.run_until_parked();
    Fixture {
        window,
        shell,
        backend,
    }
}

fn press(cx: &mut TestAppContext, f: &Fixture, key: &'static str) {
    cx.update_window(f.window, |_, window, cx| window.press(key, cx))
        .expect("press");
    cx.run_until_parked();
}

fn open_settings(cx: &mut TestAppContext, f: &Fixture) {
    press(
        cx,
        f,
        if cfg!(target_os = "macos") {
            "cmd-,"
        } else {
            "ctrl-,"
        },
    );
}

/// Shows `tab` of the open settings page, the cursor on its first row.
fn show_tab(cx: &mut TestAppContext, f: &Fixture, tab: SettingsTab) {
    let page = cx.update(|cx| f.shell.read(cx).settings_page_for_test().expect("page"));
    page.update(cx, |p, cx| p.show_tab_for_test(tab, cx));
}

fn names(cx: &mut TestAppContext, f: &Fixture) -> Vec<String> {
    cx.update(|cx| {
        f.shell
            .read(cx)
            .queries_for_test()
            .iter()
            .map(|q| q.name.clone())
            .collect()
    })
}

fn type_into(cx: &mut TestAppContext, f: &Fixture, input: &Entity<InputState>, text: &'static str) {
    cx.update_window(f.window, |_, window, cx| {
        input.update(cx, |s, cx| s.set_value(text, window, cx))
    })
    .expect("set value");
}

fn inputs(cx: &mut TestAppContext, f: &Fixture) -> (Entity<InputState>, Entity<InputState>) {
    cx.update(|cx| {
        let page = f.shell.read(cx).settings_page_for_test().expect("page");
        page.read(cx).inputs_for_test()
    })
}

#[gpui_kit::test]
fn the_key_opens_the_page_and_u_goes_back_to_the_dashboard(cx: &mut TestAppContext) {
    let f = fixture(cx, false);
    open_settings(cx, &f);
    cx.update(|cx| assert_eq!(f.shell.read(cx).route(), &Route::Settings));
    press(cx, &f, "u");
    cx.update(|cx| assert_eq!(f.shell.read(cx).route(), &Route::Dashboard));
}

#[gpui_kit::test]
fn saving_projects_updates_the_setup_and_the_server_checkouts(cx: &mut TestAppContext) {
    use crate::session::{ProjectSettings, ServerKey};
    let f = fixture(cx, false);
    let key = ServerKey::new("test");
    let checkouts = f
        .shell
        .update(cx, |s, cx| s.checkouts(&key, cx))
        .expect("server");
    assert_eq!(checkouts.read_with(cx, |c, _| c.all().len()), 0);

    let list = vec![ProjectSettings::new("demo/a", "/nowhere/a")];
    f.shell
        .update(cx, |s, cx| s.save_projects(list.clone(), cx));
    cx.run_until_parked();
    assert_eq!(*f.backend.saved_projects.lock().unwrap(), vec![list]);
    let now = checkouts.read_with(cx, |c, _| c.all());
    assert_eq!(now.len(), 1);
    assert_eq!(now[0].project, "demo/a");
    // The shell hands out the same entity again.
    let again = f
        .shell
        .update(cx, |s, cx| s.checkouts(&key, cx))
        .expect("server");
    assert_eq!(again.entity_id(), checkouts.entity_id());
}

#[gpui_kit::test]
fn a_failed_projects_save_shows_a_hint_and_keeps_the_list(cx: &mut TestAppContext) {
    use crate::session::{ProjectSettings, ServerKey};
    let f = fixture(cx, true);
    open_settings(cx, &f);
    show_tab(cx, &f, SettingsTab::Projects);
    let key = ServerKey::new("test");
    let checkouts = f
        .shell
        .update(cx, |s, cx| s.checkouts(&key, cx))
        .expect("server");
    let list = vec![ProjectSettings::new("demo/a", "/nowhere/a")];
    f.shell.update(cx, |s, cx| s.save_projects(list, cx));
    cx.run_until_parked();
    let page = cx.update(|cx| f.shell.read(cx).settings_page_for_test().expect("page"));
    let hint = page.read_with(cx, |p, _| p.hint_for_test());
    let (text, error) = hint.expect("a hint");
    assert!(error);
    assert!(text.contains("Could not save the projects"), "{text}");
    assert!(text.contains("disk full"), "{text}");
    assert!(checkouts.read_with(cx, |c, _| c.all().is_empty()));
}

// ---- the Projects tab and the project dialog ----

fn tmp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("projects-tab-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// `git init` with an environment that ignores the user's git config.
fn git_init(dir: &std::path::Path) {
    let status = std::process::Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .arg(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("run git");
    assert!(status.success());
}

fn project(name: &str, dir: &std::path::Path) -> crate::session::ProjectSettings {
    crate::session::ProjectSettings::new(name, dir.to_string_lossy())
}

/// The Projects tab of an anonymous server.
fn projects_fixture(
    cx: &mut TestAppContext,
    projects: Vec<crate::session::ProjectSettings>,
) -> Fixture {
    projects_fixture_as(cx, projects, None)
}

fn projects_fixture_as(
    cx: &mut TestAppContext,
    projects: Vec<crate::session::ProjectSettings>,
    username: Option<&str>,
) -> Fixture {
    let f = fixture_all(
        cx,
        false,
        StartupPage::Dashboard,
        Vec::new(),
        Vec::new(),
        projects,
        username,
    );
    open_settings(cx, &f);
    show_tab(cx, &f, SettingsTab::Projects);
    cx.run_until_parked();
    f
}

/// The status words of every project the tab lists.
fn statuses(cx: &mut TestAppContext, f: &Fixture) -> Vec<(String, String)> {
    cx.update(|cx| {
        let key = crate::session::ServerKey::new("test");
        let checkouts = f
            .shell
            .update(cx, |s, cx| s.checkouts(&key, cx))
            .expect("server");
        checkouts
            .read(cx)
            .all()
            .iter()
            .map(|c| {
                (
                    c.project.clone(),
                    crate::pages::settings::projects::status_text(c).0,
                )
            })
            .collect()
    })
}

fn project_dialog(
    cx: &mut TestAppContext,
    f: &Fixture,
) -> Option<Entity<crate::project_dialog::ProjectDialog>> {
    cx.update(|cx| f.shell.read(cx).project_dialog_for_test())
}

/// Types `name` and `path` into the open dialog.
fn fill_dialog(cx: &mut TestAppContext, f: &Fixture, name: &'static str, path: &'static str) {
    let dialog = project_dialog(cx, f).expect("dialog");
    let (n, p) = cx.update(|cx| {
        let d = dialog.read(cx);
        (d.name_input_for_test(), d.path_input_for_test())
    });
    type_into(cx, f, &n, name);
    type_into(cx, f, &p, path);
}

fn saved_projects(f: &Fixture) -> Vec<Vec<crate::session::ProjectSettings>> {
    f.backend.saved_projects.lock().unwrap().clone()
}

#[gpui_kit::test]
fn the_projects_tab_lists_the_checkouts_with_their_status(cx: &mut TestAppContext) {
    let repo = tmp_dir("repo");
    git_init(&repo);
    let plain = tmp_dir("plain");
    let f = projects_fixture(
        cx,
        vec![project("demo/ok", &repo), project("demo/no", &plain)],
    );
    let page = cx.update(|cx| f.shell.read(cx).settings_page_for_test().expect("page"));
    assert_eq!(
        page.read_with(cx, |p, _| p.project_names_for_test().to_vec()),
        ["demo/ok", "demo/no"]
    );
    let st = statuses(cx, &f);
    assert_eq!(st[0], ("demo/ok".into(), "Git checkout".into()));
    assert!(st[1].1.contains("is not a git repository"), "{st:?}");
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&plain);
}

#[gpui_kit::test]
fn showing_the_tab_again_checks_a_failed_checkout_again(cx: &mut TestAppContext) {
    let dir = tmp_dir("again");
    let f = projects_fixture(cx, vec![project("demo/a", &dir)]);
    assert!(statuses(cx, &f)[0].1.contains("not a git repository"));
    git_init(&dir);
    // Nothing changes by itself.
    cx.run_until_parked();
    assert!(statuses(cx, &f)[0].1.contains("not a git repository"));
    show_tab(cx, &f, SettingsTab::Review);
    show_tab(cx, &f, SettingsTab::Projects);
    cx.run_until_parked();
    assert_eq!(statuses(cx, &f)[0].1, "Git checkout");
    let _ = std::fs::remove_dir_all(&dir);
}

#[gpui_kit::test]
fn the_seven_key_shows_the_projects_tab(cx: &mut TestAppContext) {
    let f = fixture(cx, false);
    open_settings(cx, &f);
    press(cx, &f, "7");
    assert_eq!(tab_state(cx, &f).0, SettingsTab::Projects);
}

#[gpui_kit::test]
fn adding_a_checkout_saves_the_list_with_the_path_as_written(cx: &mut TestAppContext) {
    let f = projects_fixture(cx, Vec::new());
    // The empty-state row also adds.
    press(cx, &f, "a");
    assert!(project_dialog(cx, &f).is_some());
    fill_dialog(cx, &f, " demo/new ", "~/src/new");
    press(cx, &f, "enter");
    assert!(project_dialog(cx, &f).is_none());
    assert_eq!(
        saved_projects(&f),
        vec![vec![crate::session::ProjectSettings::new(
            "demo/new",
            "~/src/new"
        )]]
    );
    // The tab follows what the shell saved.
    let page = cx.update(|cx| f.shell.read(cx).settings_page_for_test().expect("page"));
    assert_eq!(
        page.read_with(cx, |p, _| p.project_names_for_test().to_vec()),
        ["demo/new"]
    );
}

#[gpui_kit::test]
fn an_added_checkout_is_checked_at_once(cx: &mut TestAppContext) {
    let repo = tmp_dir("added");
    git_init(&repo);
    let f = projects_fixture(cx, Vec::new());
    press(cx, &f, "a");
    let path = repo.to_string_lossy().into_owned();
    let path: &'static str = Box::leak(path.into_boxed_str());
    fill_dialog(cx, &f, "demo/new", path);
    press(cx, &f, "enter");
    cx.run_until_parked();
    assert_eq!(
        statuses(cx, &f),
        vec![("demo/new".to_owned(), "Git checkout".to_owned())]
    );
    let _ = std::fs::remove_dir_all(&repo);
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn editing_a_checkout_replaces_it_in_place(cx: &mut TestAppContext) {
    let a = tmp_dir("edit-a");
    let b = tmp_dir("edit-b");
    let f = projects_fixture(cx, vec![project("one", &a), project("two", &a)]);
    press(cx, &f, "j");
    press(cx, &f, "e");
    let dialog = project_dialog(cx, &f).expect("dialog");
    assert!(cx.update(|cx| dialog.read(cx).is_edit_for_test()));
    let path = b.to_string_lossy().into_owned();
    let path: &'static str = Box::leak(path.into_boxed_str());
    fill_dialog(cx, &f, "two", path);
    press(cx, &f, "enter");
    assert_eq!(
        saved_projects(&f),
        vec![vec![project("one", &a), project("two", &b)]]
    );
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_duplicate_name_or_an_empty_field_is_not_saved(cx: &mut TestAppContext) {
    let a = tmp_dir("dup");
    let f = projects_fixture(cx, vec![project("one", &a)]);
    press(cx, &f, "a");
    fill_dialog(cx, &f, "one", "/somewhere");
    press(cx, &f, "enter");
    let dialog = project_dialog(cx, &f).expect("still open");
    assert_eq!(
        cx.update(|cx| dialog.read(cx).problem_for_test(cx)),
        Some("This project already has a checkout.")
    );
    fill_dialog(cx, &f, "", "/somewhere");
    press(cx, &f, "enter");
    assert!(
        cx.update(|cx| dialog.read(cx).problem_for_test(cx))
            .is_some()
    );
    fill_dialog(cx, &f, "two", "  ");
    press(cx, &f, "enter");
    assert!(
        cx.update(|cx| dialog.read(cx).problem_for_test(cx))
            .is_some()
    );
    assert!(saved_projects(&f).is_empty());
    // Escape discards.
    press(cx, &f, "escape");
    assert!(project_dialog(cx, &f).is_none());
    assert!(saved_projects(&f).is_empty());
    let _ = std::fs::remove_dir_all(&a);
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn removing_a_checkout_asks_first(cx: &mut TestAppContext) {
    let a = tmp_dir("rm");
    let f = projects_fixture(cx, vec![project("one", &a), project("two", &a)]);
    press(cx, &f, "d");
    press(cx, &f, "n");
    assert!(saved_projects(&f).is_empty());
    press(cx, &f, "d");
    press(cx, &f, "y");
    assert_eq!(saved_projects(&f), vec![vec![project("two", &a)]]);
    let page = cx.update(|cx| f.shell.read(cx).settings_page_for_test().expect("page"));
    assert_eq!(
        page.read_with(cx, |p, _| p.project_names_for_test().to_vec()),
        ["two"]
    );
    let _ = std::fs::remove_dir_all(&a);
}

const SAVE: &str = if cfg!(target_os = "macos") {
    "cmd-enter"
} else {
    "ctrl-enter"
};

/// The open dialog's local options, and whether the box and the vote
/// choice take input.
fn local_state(
    cx: &mut TestAppContext,
    f: &Fixture,
) -> (crate::project_dialog::LocalOptions, bool, bool) {
    let dialog = project_dialog(cx, f).expect("dialog");
    cx.update(|cx| dialog.read(cx).local_for_test())
}

/// The Projects tab's word next to each project's status.
fn local_words(cx: &mut TestAppContext, f: &Fixture) -> Vec<Option<&'static str>> {
    cx.update(|cx| {
        let key = crate::session::ServerKey::new("test");
        let checkouts = f
            .shell
            .update(cx, |s, cx| s.checkouts(&key, cx))
            .expect("server");
        checkouts
            .read(cx)
            .projects()
            .map(crate::pages::settings::projects::local_word)
            .collect()
    })
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn the_local_changes_box_turns_the_vote_choice_on_and_both_are_saved(cx: &mut TestAppContext) {
    use model::LocalApproval;
    let f = projects_fixture_as(cx, Vec::new(), Some("u"));
    press(cx, &f, "a");
    fill_dialog(cx, &f, "demo/new", "/src/new");
    let (local, box_on, choice_on) = local_state(cx, &f);
    assert!(!local.changes && box_on && !choice_on, "{local:?}");
    assert_eq!(local.approval, LocalApproval::Exact);
    // Name, directory, Browse, then the box: Space ticks it.
    for key in ["tab", "tab", "tab", "space"] {
        press(cx, &f, key);
    }
    let (local, _, choice_on) = local_state(cx, &f);
    assert!(local.changes && choice_on);
    // The choice is next; Up goes to the more permissive options.
    press(cx, &f, "tab");
    press(cx, &f, "up");
    assert_eq!(local_state(cx, &f).0.approval, LocalApproval::ExactOrManual);
    press(cx, &f, "up");
    press(cx, &f, "up");
    assert_eq!(local_state(cx, &f).0.approval, LocalApproval::Follow);
    press(cx, &f, "down");
    press(cx, &f, "up");
    // Clearing the box turns the choice off, which keys cannot change
    // and Tab skips.
    press(cx, &f, "shift-tab");
    press(cx, &f, "space");
    let (local, _, choice_on) = local_state(cx, &f);
    assert!(!local.changes && !choice_on);
    assert_eq!(local.approval, LocalApproval::Follow);
    press(cx, &f, "tab");
    press(cx, &f, "down");
    assert_eq!(local_state(cx, &f).0.approval, LocalApproval::Follow);
    // That Tab went to Save: Space presses it, with the box clear.
    press(cx, &f, "space");
    assert!(project_dialog(cx, &f).is_none());
    let off = crate::session::ProjectSettings {
        local_approval: LocalApproval::Follow,
        ..crate::session::ProjectSettings::new("demo/new", "/src/new")
    };
    assert_eq!(saved_projects(&f), vec![vec![off.clone()]]);
    assert_eq!(local_words(cx, &f), [None]);

    // Ticked and saved, the tab says so.
    press(cx, &f, "e");
    for key in ["tab", "tab", "tab", "space"] {
        press(cx, &f, key);
    }
    press(cx, &f, SAVE);
    assert!(project_dialog(cx, &f).is_none());
    let on = crate::session::ProjectSettings {
        local_changes: true,
        ..off
    };
    assert_eq!(saved_projects(&f).last(), Some(&vec![on]));
    assert_eq!(local_words(cx, &f), [Some("local changes")]);
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_click_that_turns_the_choice_off_moves_the_focus_to_the_box(cx: &mut TestAppContext) {
    let f = projects_fixture_as(cx, Vec::new(), Some("u"));
    press(cx, &f, "a");
    fill_dialog(cx, &f, "demo/new", "/src/new");
    for key in ["tab", "tab", "tab", "space", "tab"] {
        press(cx, &f, key);
    }
    let dialog = project_dialog(cx, &f).expect("dialog");
    let focus = |cx: &mut TestAppContext| {
        cx.update_window(f.window, |_, window, cx| {
            dialog.read(cx).approval_focused_for_test(window)
        })
        .expect("window")
    };
    assert_eq!(focus(cx), (true, false));
    assert!(
        cx.update(|cx| dialog.read(cx).footer_hint_for_test())
            .contains("Space ticks")
    );
    cx.update_window(f.window, |_, window, cx| {
        dialog.update(cx, |d, cx| d.toggle_local_for_test(window, cx))
    })
    .expect("window");
    cx.run_until_parked();
    assert_eq!(focus(cx), (false, true));
    // So Tab goes on to Save, which Space presses.
    press(cx, &f, "tab");
    press(cx, &f, "space");
    assert!(project_dialog(cx, &f).is_none());
    assert_eq!(saved_projects(&f).len(), 1);
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn the_local_options_cannot_be_changed_on_an_anonymous_server(cx: &mut TestAppContext) {
    use model::LocalApproval;
    let a = tmp_dir("anon-local");
    // Set by hand in the file.
    let entry = crate::session::ProjectSettings {
        local_changes: true,
        local_approval: LocalApproval::ExactOrManual,
        ..project("one", &a)
    };
    let f = projects_fixture(cx, vec![entry.clone()]);
    press(cx, &f, "e");
    let (local, box_on, choice_on) = local_state(cx, &f);
    assert!(local.changes && !box_on && !choice_on);
    assert_eq!(local.approval, LocalApproval::ExactOrManual);
    let dialog = project_dialog(cx, &f).expect("dialog");
    let hint = cx.update(|cx| dialog.read(cx).footer_hint_for_test());
    assert!(!hint.contains("Space") && !hint.contains("Up"), "{hint}");
    // Tab skips both: after Browse comes Save, and Space presses it.
    for key in ["tab", "tab", "tab", "space"] {
        press(cx, &f, key);
    }
    assert!(project_dialog(cx, &f).is_none());
    // What the entry had is kept.
    assert_eq!(saved_projects(&f), vec![vec![entry]]);
    let _ = std::fs::remove_dir_all(&a);
}

fn gear(cx: &mut TestAppContext, f: &Fixture, project: &'static str) {
    cx.update_window(f.window, |_, window, cx| {
        f.shell.update(cx, |s, cx| {
            s.open_project_settings_for_test(project, window, cx)
        })
    })
    .expect("gear");
    cx.run_until_parked();
}

#[gpui_kit::test]
fn the_gear_opens_the_projects_tab_on_the_project(cx: &mut TestAppContext) {
    let a = tmp_dir("gear");
    let f = projects_fixture(cx, vec![project("one", &a), project("two", &a)]);
    let first = tab_state(cx, &f).1;
    press(cx, &f, "u");
    cx.update(|cx| assert_eq!(f.shell.read(cx).route(), &Route::Dashboard));
    gear(cx, &f, "two");
    cx.update(|cx| assert_eq!(f.shell.read(cx).route(), &Route::Settings));
    assert_eq!(tab_state(cx, &f), (SettingsTab::Projects, first + 1));
    assert!(project_dialog(cx, &f).is_none());
    let _ = std::fs::remove_dir_all(&a);
}

#[gpui_kit::test]
fn the_gear_for_an_unconfigured_project_opens_the_add_dialog(cx: &mut TestAppContext) {
    let f = fixture(cx, false);
    gear(cx, &f, "demo/none");
    cx.update(|cx| assert_eq!(f.shell.read(cx).route(), &Route::Settings));
    assert_eq!(tab_state(cx, &f).0, SettingsTab::Projects);
    let dialog = project_dialog(cx, &f).expect("dialog");
    assert!(!cx.update(|cx| dialog.read(cx).is_edit_for_test()));
    let name = cx.update(|cx| dialog.read(cx).name_input_for_test());
    assert_eq!(
        name.read_with(cx, |s, _| s.value().to_string()),
        "demo/none"
    );
    // Saving adds it.
    fill_dialog(cx, &f, "demo/none", "/src/none");
    press(cx, &f, "enter");
    assert_eq!(
        saved_projects(&f),
        vec![vec![crate::session::ProjectSettings::new(
            "demo/none",
            "/src/none"
        )]]
    );
}

#[gpui_kit::test]
fn moving_a_query_saves_it_and_the_dashboard_follows(cx: &mut TestAppContext) {
    let f = fixture(cx, false);
    open_settings(cx, &f);
    press(cx, &f, "shift-j");
    assert_eq!(names(cx, &f), ["Mine", "Open"]);
    let saved = f.backend.saved_queries.lock().unwrap().clone();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0][0].query, "owner:self");

    // The dashboard shows the new order, rows straight from the cache.
    press(cx, &f, "u");
    cx.update(|cx| {
        let list = f.shell.read(cx).list_page_for_test().expect("dashboard");
        let list = list.read(cx);
        assert_eq!(list.visible_rows(0)[0].number, ChangeNumber(2));
        assert_eq!(list.visible_rows(1)[0].number, ChangeNumber(1));
    });
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn editing_adding_and_deleting(cx: &mut TestAppContext) {
    let f = fixture(cx, false);
    open_settings(cx, &f);

    // Edit the first row's name; an empty name is refused with a reason.
    press(cx, &f, "e");
    let (name, query) = inputs(cx, &f);
    type_into(cx, &f, &name, "");
    press(cx, &f, "enter");
    assert!(f.backend.saved_queries.lock().unwrap().is_empty());
    cx.update(|cx| {
        let page = f.shell.read(cx).settings_page_for_test().expect("page");
        assert!(page.read(cx).editing_for_test());
    });
    type_into(cx, &f, &name, "Opened");
    type_into(cx, &f, &query, "status:open project:x");
    press(cx, &f, "enter");
    assert_eq!(names(cx, &f), ["Opened", "Mine"]);

    // Add a row; Escape cancels it.
    press(cx, &f, "a");
    press(cx, &f, "escape");
    assert_eq!(names(cx, &f).len(), 2);
    press(cx, &f, "a");
    type_into(cx, &f, &name, "New");
    type_into(cx, &f, &query, "is:wip");
    press(cx, &f, "enter");
    assert_eq!(names(cx, &f), ["Opened", "Mine", "New"]);

    // Delete asks first; n keeps it, y removes it.
    press(cx, &f, "d");
    press(cx, &f, "n");
    assert_eq!(names(cx, &f).len(), 3);
    press(cx, &f, "d");
    press(cx, &f, "y");
    // The cursor was on the new row.
    assert_eq!(names(cx, &f), ["Opened", "Mine"]);
    assert_eq!(f.backend.saved_queries.lock().unwrap().len(), 3);
}

fn bookmark_names(cx: &mut TestAppContext, f: &Fixture) -> Vec<String> {
    cx.update(|cx| {
        let page = f.shell.read(cx).settings_page_for_test().expect("page");
        let page = page.read(cx);
        page.bookmarks_for_test()
            .iter()
            .map(|q| q.name.clone())
            .collect()
    })
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn bookmarks_are_added_edited_moved_and_deleted_below_the_queries(cx: &mut TestAppContext) {
    let f = fixture_full(cx, false, StartupPage::Dashboard, Vec::new(), Vec::new());
    open_settings(cx, &f);
    // Two queries, then the empty bookmarks row.
    press(cx, &f, "j");
    press(cx, &f, "j");
    assert_eq!(page_state(cx, &f).0, 2);
    let (name, query) = inputs(cx, &f);
    press(cx, &f, "a");
    type_into(cx, &f, &name, "First");
    type_into(cx, &f, &query, "is:open");
    press(cx, &f, "enter");
    press(cx, &f, "a");
    type_into(cx, &f, &name, "Second");
    type_into(cx, &f, &query, "is:wip");
    press(cx, &f, "enter");
    assert_eq!(bookmark_names(cx, &f), ["First", "Second"]);
    assert_eq!(names(cx, &f), ["Open", "Mine"]);
    // The cursor stays on the bookmark it added.
    assert_eq!(page_state(cx, &f).0, 3);

    press(cx, &f, "shift-k");
    assert_eq!(bookmark_names(cx, &f), ["Second", "First"]);
    assert_eq!(page_state(cx, &f).0, 2);
    // The first bookmark cannot move into the queries.
    press(cx, &f, "shift-k");
    assert_eq!(bookmark_names(cx, &f), ["Second", "First"]);

    press(cx, &f, "e");
    type_into(cx, &f, &name, "Renamed");
    press(cx, &f, "enter");
    assert_eq!(bookmark_names(cx, &f), ["Renamed", "First"]);

    press(cx, &f, "d");
    press(cx, &f, "y");
    assert_eq!(bookmark_names(cx, &f), ["First"]);
    let saved = f.backend.saved_bookmarks.lock().unwrap().clone();
    assert_eq!(saved.len(), 5);
    assert!(f.backend.saved_queries.lock().unwrap().is_empty());
    // The shell, and so the dashboard's row, follows.
    press(cx, &f, "u");
    cx.update(|cx| {
        let list = f.shell.read(cx).list_page_for_test().expect("dashboard");
        assert_eq!(list.read(cx).bookmarks_for_test(), ["First"]);
    });
}

#[gpui_kit::test]
fn a_failed_save_changes_nothing_and_says_so(cx: &mut TestAppContext) {
    let f = fixture(cx, true);
    open_settings(cx, &f);
    press(cx, &f, "shift-j");
    assert_eq!(names(cx, &f), ["Open", "Mine"]);
    cx.update(|cx| {
        let page = f.shell.read(cx).settings_page_for_test().expect("page");
        let (text, error) = page.read(cx).hint_for_test().expect("hint");
        assert!(error && text.contains("disk full"), "{text}");
        assert_eq!(page.read(cx).queries_for_test().len(), 2);
    });
}
fn advance_on(cx: &mut TestAppContext, f: &Fixture) -> bool {
    cx.update(|cx| {
        f.shell
            .read(cx)
            .layout_for_test()
            .read(cx)
            .advance_after_vote()
    })
}

#[gpui_kit::test]
fn toggling_advance_after_vote_saves_it_and_flips_the_layout(cx: &mut TestAppContext) {
    let f = fixture(cx, false);
    open_settings(cx, &f);
    assert!(advance_on(cx, &f));
    show_tab(cx, &f, SettingsTab::Review);
    press(cx, &f, "space");
    assert!(!advance_on(cx, &f));
    assert_eq!(*f.backend.saved_advance.lock().unwrap(), [false]);
    press(cx, &f, "enter");
    assert!(advance_on(cx, &f));
    assert_eq!(*f.backend.saved_advance.lock().unwrap(), [false, true]);
}

#[gpui_kit::test]
fn a_failed_advance_save_keeps_the_old_value(cx: &mut TestAppContext) {
    let f = fixture(cx, true);
    open_settings(cx, &f);
    show_tab(cx, &f, SettingsTab::Review);
    press(cx, &f, "space");
    assert!(advance_on(cx, &f));
    cx.update(|cx| {
        let page = f.shell.read(cx).settings_page_for_test().expect("page");
        let (text, error) = page.read(cx).hint_for_test().expect("hint");
        assert!(error && text.contains("disk full"), "{text}");
    });
}

#[gpui_kit::test]
fn switching_the_theme_saves_and_applies_it(cx: &mut TestAppContext) {
    use crate::theme::{self, ThemeName};
    let f = fixture(cx, false);
    open_settings(cx, &f);
    let applied = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            let name = gpui_kit::component::ActiveTheme::theme(cx).theme_name();
            (theme::current(cx), name.to_string())
        })
    };
    assert_eq!(applied(cx).0, ThemeName::Cwc);
    show_tab(cx, &f, SettingsTab::Appearance);
    press(cx, &f, "space");
    let (now, name) = applied(cx);
    assert_eq!(now, ThemeName::Solarized);
    assert!(name.starts_with("Solarized"), "{name}");
    assert_eq!(
        *f.backend.saved_theme.lock().unwrap(),
        [ThemeName::Solarized]
    );
    press(cx, &f, "enter");
    let (now, name) = applied(cx);
    assert_eq!(now, ThemeName::Cwc);
    assert!(name.starts_with("CWC"), "{name}");
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn switching_the_modes_saves_and_applies_them(cx: &mut TestAppContext) {
    use crate::theme::{ColorMode, ColorModes, color_modes};
    let f = fixture(cx, false);
    open_settings(cx, &f);
    let ui_dark = |cx: &mut TestAppContext| {
        cx.update(|cx| gpui_kit::component::ActiveTheme::theme(cx).is_dark())
    };
    assert_eq!(cx.update(|cx| color_modes(cx)), ColorModes::default());
    // The theme row, then the UI mode row: system, light, dark.
    show_tab(cx, &f, SettingsTab::Appearance);
    press(cx, &f, "j");
    press(cx, &f, "space");
    press(cx, &f, "space");
    assert_eq!(cx.update(|cx| color_modes(cx).ui), ColorMode::Dark);
    assert!(ui_dark(cx));
    press(cx, &f, "space");
    assert_eq!(cx.update(|cx| color_modes(cx).ui), ColorMode::System);
    // The diff row is separate.
    press(cx, &f, "j");
    press(cx, &f, "space");
    assert_eq!(
        cx.update(|cx| color_modes(cx)),
        ColorModes {
            ui: ColorMode::System,
            diff: ColorMode::Light
        }
    );
}

#[gpui_kit::test]
fn a_failed_theme_save_keeps_the_theme(cx: &mut TestAppContext) {
    let f = fixture(cx, true);
    open_settings(cx, &f);
    show_tab(cx, &f, SettingsTab::Appearance);
    press(cx, &f, "space");
    cx.update(|cx| {
        assert_eq!(crate::theme::current(cx), crate::theme::ThemeName::Cwc);
        let page = f.shell.read(cx).settings_page_for_test().expect("page");
        let (text, error) = page.read(cx).hint_for_test().expect("hint");
        assert!(error && text.contains("disk full"), "{text}");
    });
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_startup_query_needs_text_and_is_saved(cx: &mut TestAppContext) {
    let f = fixture(cx, false);
    open_settings(cx, &f);
    for _ in 0..3 {
        press(cx, &f, "j");
    }
    // Space on "dashboard" starts typing the query; empty is refused.
    press(cx, &f, "space");
    let input = cx.update(|cx| {
        let page = f.shell.read(cx).settings_page_for_test().expect("page");
        page.read(cx).startup_input_for_test()
    });
    press(cx, &f, "enter");
    assert!(f.backend.saved_startup.lock().unwrap().is_empty());
    cx.update(|cx| {
        let page = f.shell.read(cx).settings_page_for_test().expect("page");
        assert!(page.read(cx).editing_for_test());
    });
    type_into(cx, &f, &input, " is:open owner:self ");
    press(cx, &f, "enter");
    let want = StartupPage::Search("is:open owner:self".into());
    assert_eq!(
        *f.backend.saved_startup.lock().unwrap(),
        std::slice::from_ref(&want)
    );
    cx.update(|cx| {
        let page = f.shell.read(cx).settings_page_for_test().expect("page");
        assert_eq!(page.read(cx).start_page_for_test(), &want);
        assert!(!page.read(cx).editing_for_test());
    });

    // Space again goes back to the dashboard.
    press(cx, &f, "space");
    assert_eq!(
        f.backend.saved_startup.lock().unwrap().last(),
        Some(&StartupPage::Dashboard)
    );

    // The shell remembers it for the next time the page opens.
    press(cx, &f, "u");
    open_settings(cx, &f);
    cx.update(|cx| {
        let page = f.shell.read(cx).settings_page_for_test().expect("page");
        assert_eq!(page.read(cx).start_page_for_test(), &StartupPage::Dashboard);
    });
}

#[gpui_kit::test]
fn the_shell_opens_on_the_configured_search(cx: &mut TestAppContext) {
    let f = fixture_at(cx, false, StartupPage::Search("owner:self".into()));
    cx.update(|cx| {
        assert_eq!(
            f.shell.read(cx).route(),
            &Route::Search("owner:self".into())
        );
    });
    // The dashboard is still one key away.
    press(cx, &f, "u");
    cx.update(|cx| assert_eq!(f.shell.read(cx).route(), &Route::Dashboard));
    let f = fixture(cx, false);
    cx.update(|cx| assert_eq!(f.shell.read(cx).route(), &Route::Dashboard));
}

fn rule(name: &str, action: FilterAction) -> FileFilter {
    FileFilter {
        name: name.into(),
        globs: vec![format!("*.{name}")],
        projects: Vec::new(),
        servers: Vec::new(),
        action,
        enabled: true,
    }
}

fn two_rules() -> Vec<FileFilter> {
    vec![
        rule("gen", FilterAction::Dim),
        rule("lock", FilterAction::Hide),
    ]
}

fn page_state(cx: &mut TestAppContext, f: &Fixture) -> (usize, Vec<String>) {
    cx.update(|cx| {
        let page = f.shell.read(cx).settings_page_for_test().expect("page");
        let page = page.read(cx);
        (
            page.cursor_for_test(),
            page.filter_names_for_test().to_vec(),
        )
    })
}

/// The dialog's cursor and mode, if it is open.
fn dialog_state(cx: &mut TestAppContext, f: &Fixture) -> Option<(usize, &'static str)> {
    cx.update(|cx| {
        let d = f.shell.read(cx).filters_dialog_for_test()?;
        let d = d.read(cx);
        Some((d.cursor_for_test(), d.mode_for_test()))
    })
}

/// Cursor on the first filter row (row 8: 2 queries, the empty
/// bookmarks row, startup, advance, theme, two modes before it).
fn to_filters(cx: &mut TestAppContext, f: &Fixture) {
    show_tab(cx, f, SettingsTab::Filters);
}

fn tab_state(cx: &mut TestAppContext, f: &Fixture) -> (SettingsTab, usize) {
    cx.update(|cx| {
        let page = f.shell.read(cx).settings_page_for_test().expect("page");
        let page = page.read(cx);
        (page.tab_for_test(), page.cursor_for_test())
    })
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn the_cursor_stays_inside_the_tab_and_jumps_to_its_first_row_on_a_switch(cx: &mut TestAppContext) {
    let f = fixture_with(cx, false, StartupPage::Dashboard, two_rules());
    open_settings(cx, &f);
    // Queries: 2 queries, the empty bookmarks row, the startup row.
    assert_eq!(tab_state(cx, &f), (SettingsTab::Queries, 0));
    for _ in 0..9 {
        press(cx, &f, "j");
    }
    assert_eq!(tab_state(cx, &f).1, 3);
    press(cx, &f, "k");
    for _ in 0..9 {
        press(cx, &f, "k");
    }
    assert_eq!(tab_state(cx, &f).1, 0);

    // Appearance: the theme row and the two mode rows.
    show_tab(cx, &f, SettingsTab::Appearance);
    assert_eq!(tab_state(cx, &f), (SettingsTab::Appearance, 5));
    press(cx, &f, "k");
    assert_eq!(tab_state(cx, &f).1, 5);
    for _ in 0..5 {
        press(cx, &f, "j");
    }
    assert_eq!(tab_state(cx, &f).1, 7);

    // Review has the one row; a does not add a query there.
    show_tab(cx, &f, SettingsTab::Review);
    assert_eq!(tab_state(cx, &f), (SettingsTab::Review, 4));
    press(cx, &f, "j");
    press(cx, &f, "a");
    assert_eq!(tab_state(cx, &f).1, 4);
    cx.update(|cx| {
        let page = f.shell.read(cx).settings_page_for_test().expect("page");
        assert!(!page.read(cx).editing_for_test());
    });

    show_tab(cx, &f, SettingsTab::Servers);
    assert_eq!(tab_state(cx, &f), (SettingsTab::Servers, 10));
    press(cx, &f, "j");
    assert_eq!(tab_state(cx, &f).1, 10);
}

fn tabs_strip(cx: &mut TestAppContext, f: &Fixture) -> gpui_kit::ScrollHandle {
    cx.update(|cx| {
        let page = f.shell.read(cx).settings_page_for_test().expect("page");
        page.read(cx).tabs_scroll_for_test()
    })
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn l_h_the_arrows_and_the_digits_switch_tabs(cx: &mut TestAppContext) {
    let f = fixture_with(cx, false, StartupPage::Dashboard, two_rules());
    open_settings(cx, &f);
    let tab = |cx: &mut TestAppContext| tab_state(cx, &f).0;
    assert_eq!(tab(cx), SettingsTab::Queries);
    press(cx, &f, "l");
    assert_eq!(tab(cx), SettingsTab::Review);
    press(cx, &f, "right");
    assert_eq!(tab(cx), SettingsTab::Appearance);
    press(cx, &f, "l");
    assert_eq!(tab(cx), SettingsTab::Filters);
    press(cx, &f, "l");
    assert_eq!(tab(cx), SettingsTab::Notifications);
    press(cx, &f, "l");
    assert_eq!(tab(cx), SettingsTab::Projects);
    // The strip has ends.
    press(cx, &f, "l");
    assert_eq!(tab(cx), SettingsTab::Projects);
    press(cx, &f, "h");
    assert_eq!(tab(cx), SettingsTab::Notifications);
    press(cx, &f, "h");
    assert_eq!(tab(cx), SettingsTab::Filters);
    press(cx, &f, "h");
    assert_eq!(tab(cx), SettingsTab::Appearance);
    press(cx, &f, "left");
    assert_eq!(tab(cx), SettingsTab::Review);
    for (key, want) in [
        ("1", SettingsTab::Servers),
        ("7", SettingsTab::Projects),
        ("6", SettingsTab::Notifications),
        ("5", SettingsTab::Filters),
        ("2", SettingsTab::Queries),
        ("4", SettingsTab::Appearance),
        ("3", SettingsTab::Review),
    ] {
        press(cx, &f, key);
        assert_eq!(tab(cx), want, "key {key}");
    }
    press(cx, &f, "1");
    press(cx, &f, "h");
    assert_eq!(tab(cx), SettingsTab::Servers);
    // Switching puts the cursor on the first row of the tab.
    press(cx, &f, "5");
    assert_eq!(tab_state(cx, &f).1, 8);
}

#[gpui_kit::test]
fn clicking_a_tab_shows_it(cx: &mut TestAppContext) {
    let f = fixture(cx, false);
    open_settings(cx, &f);
    let strip = tabs_strip(cx, &f);
    for (i, want) in [(3, SettingsTab::Appearance), (0, SettingsTab::Servers)] {
        let at = strip.bounds_for_item(i).expect("tab").center();
        gpui_kit::VisualTestContext::from_window(f.window, cx)
            .simulate_click(at, Default::default());
        cx.run_until_parked();
        assert_eq!(tab_state(cx, &f).0, want);
    }
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn the_tab_keys_are_not_swallowed_from_inputs_and_wait_for_answers(cx: &mut TestAppContext) {
    let f = fixture(cx, false);
    open_settings(cx, &f);
    // While a row is edited the keys are typed, not bound.
    press(cx, &f, "e");
    press(cx, &f, "l");
    press(cx, &f, "2");
    press(cx, &f, "left");
    assert_eq!(tab_state(cx, &f), (SettingsTab::Queries, 0));
    cx.update(|cx| {
        let page = f.shell.read(cx).settings_page_for_test().expect("page");
        assert!(page.read(cx).editing_for_test());
    });
    // A click on a tab does not leave the edit either.
    let strip = tabs_strip(cx, &f);
    let at = strip.bounds_for_item(2).expect("tab").center();
    gpui_kit::VisualTestContext::from_window(f.window, cx).simulate_click(at, Default::default());
    cx.run_until_parked();
    assert_eq!(tab_state(cx, &f).0, SettingsTab::Queries);
    press(cx, &f, "escape");

    // A pending delete is answered first: the keys do nothing, and the
    // question is still there afterwards.
    press(cx, &f, "d");
    press(cx, &f, "l");
    press(cx, &f, "3");
    assert_eq!(tab_state(cx, &f).0, SettingsTab::Queries);
    press(cx, &f, "n");
    assert_eq!(names(cx, &f).len(), 2);
    press(cx, &f, "l");
    assert_eq!(tab_state(cx, &f).0, SettingsTab::Review);
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn the_page_opens_on_the_tab_it_was_left_on(cx: &mut TestAppContext) {
    let f = fixture_with(cx, false, StartupPage::Dashboard, two_rules());
    open_settings(cx, &f);
    press(cx, &f, "4");
    assert_eq!(tab_state(cx, &f), (SettingsTab::Appearance, 5));
    press(cx, &f, "j");
    press(cx, &f, "u");
    // The tab is remembered, the cursor starts on the tab's first row.
    open_settings(cx, &f);
    assert_eq!(tab_state(cx, &f), (SettingsTab::Appearance, 5));
    // Every visit updates it.
    press(cx, &f, "5");
    press(cx, &f, "u");
    open_settings(cx, &f);
    assert_eq!(tab_state(cx, &f).0, SettingsTab::Filters);
}

#[gpui_kit::test]
fn the_filter_rows_follow_the_cursor_after_the_other_rows(cx: &mut TestAppContext) {
    let f = fixture_with(cx, false, StartupPage::Dashboard, two_rules());
    open_settings(cx, &f);
    assert_eq!(page_state(cx, &f).1, ["gen", "lock"]);
    to_filters(cx, &f);
    assert_eq!(page_state(cx, &f).0, 8);
    press(cx, &f, "j");
    // The last rule; the cursor stops there, the server row after it is
    // on another tab.
    assert_eq!(page_state(cx, &f).0, 9);
    press(cx, &f, "j");
    assert_eq!(page_state(cx, &f).0, 9);
    press(cx, &f, "k");
    press(cx, &f, "k");
    assert_eq!(page_state(cx, &f).0, 8);
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn enter_and_d_on_a_rule_open_the_dialog_on_it(cx: &mut TestAppContext) {
    let f = fixture_with(cx, false, StartupPage::Dashboard, two_rules());
    open_settings(cx, &f);
    to_filters(cx, &f);
    press(cx, &f, "j");
    press(cx, &f, "enter");
    assert_eq!(dialog_state(cx, &f), Some((1, "browsing")));
    press(cx, &f, "escape");
    assert_eq!(dialog_state(cx, &f), None);
    // Focus is back on the page: the cursor still works.
    press(cx, &f, "k");
    assert_eq!(page_state(cx, &f).0, 8);
    press(cx, &f, "d");
    // Deleting is the dialog's business, with its confirmation.
    assert_eq!(dialog_state(cx, &f), Some((0, "browsing")));
    press(cx, &f, "escape");
    press(cx, &f, "e");
    assert_eq!(dialog_state(cx, &f), Some((0, "browsing")));
}

#[gpui_kit::test]
fn a_adds_a_rule_in_the_filters_and_a_query_elsewhere(cx: &mut TestAppContext) {
    let f = fixture_with(cx, false, StartupPage::Dashboard, two_rules());
    open_settings(cx, &f);
    // On a query row: a new query, no dialog.
    press(cx, &f, "a");
    assert_eq!(dialog_state(cx, &f), None);
    cx.update(|cx| {
        let page = f.shell.read(cx).settings_page_for_test().expect("page");
        assert!(page.read(cx).editing_for_test());
    });
    press(cx, &f, "escape");
    // In the filters section: the dialog, on a new rule.
    to_filters(cx, &f);
    press(cx, &f, "a");
    assert_eq!(dialog_state(cx, &f), Some((2, "adding")));
}

#[gpui_kit::test]
fn without_rules_one_row_says_so_and_enter_adds(cx: &mut TestAppContext) {
    let f = fixture(cx, false);
    open_settings(cx, &f);
    to_filters(cx, &f);
    // The empty-state row is the only one of the filters.
    press(cx, &f, "j");
    assert_eq!(page_state(cx, &f), (8, Vec::new()));
    press(cx, &f, "enter");
    assert_eq!(dialog_state(cx, &f), Some((0, "adding")));
}

#[gpui_kit::test]
fn the_section_follows_the_shared_rules(cx: &mut TestAppContext) {
    let f = fixture_with(cx, false, StartupPage::Dashboard, two_rules());
    open_settings(cx, &f);
    to_filters(cx, &f);
    press(cx, &f, "j");
    // A reorder keeps the cursor on its rule.
    let filters = cx.update(|cx| f.shell.read(cx).filters_for_test().clone());
    cx.update(|cx| {
        filters.update(cx, |ff, cx| {
            ff.set_rules(
                vec![
                    rule("lock", FilterAction::Hide),
                    rule("gen", FilterAction::Dim),
                ],
                cx,
            )
        })
    });
    cx.run_until_parked();
    assert_eq!(page_state(cx, &f), (8, vec!["lock".into(), "gen".into()]));
    // Removing the rule under the cursor clamps it.
    cx.update(|cx| {
        filters.update(cx, |ff, cx| {
            ff.set_rules(vec![rule("gen", FilterAction::Dim)], cx)
        })
    });
    cx.run_until_parked();
    assert_eq!(page_state(cx, &f), (8, vec!["gen".into()]));
    cx.update(|cx| filters.update(cx, |ff, cx| ff.set_rules(Vec::new(), cx)));
    cx.run_until_parked();
    assert_eq!(page_state(cx, &f), (8, Vec::new()));
}

mod notifications {
    use sync::notify::NotificationSettings;

    use super::*;

    /// The first row of the tab: the fixture has no filters (one empty row)
    /// and one server.
    const FIRST: usize = 10;

    fn show(cx: &mut TestAppContext, f: &Fixture) {
        open_settings(cx, f);
        press(cx, f, "6");
    }

    fn page_settings(cx: &mut TestAppContext, f: &Fixture) -> NotificationSettings {
        cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            page.read(cx).notify_for_test().clone()
        })
    }

    fn shell_settings(cx: &mut TestAppContext, f: &Fixture) -> NotificationSettings {
        cx.update(|cx| f.shell.read(cx).notify_settings_for_test().clone())
    }

    fn writes(f: &Fixture) -> Vec<NotificationSettings> {
        f.backend.saved_notifications.lock().unwrap().clone()
    }

    fn notify_input(cx: &mut TestAppContext, f: &Fixture) -> Entity<InputState> {
        cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            page.read(cx).notify_input_for_test()
        })
    }

    fn editing(cx: &mut TestAppContext, f: &Fixture) -> bool {
        cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            page.read(cx).editing_for_test()
        })
    }

    #[gpui_kit::test]
    #[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
    fn six_shows_the_tab_with_its_rows_after_the_servers(cx: &mut TestAppContext) {
        let f = fixture(cx, false);
        show(cx, &f);
        assert_eq!(tab_state(cx, &f), (SettingsTab::Notifications, FIRST));
        let first = cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            page.read(cx).notify_row_for_test()
        });
        assert_eq!(first, FIRST);
        // The cursor stays in the tab: the test row ends it, after the
        // options, the timing rows, the four default rules and the reset
        // row.
        for _ in 0..20 {
            press(cx, &f, "j");
        }
        assert_eq!(tab_state(cx, &f).1, FIRST + 11);
        press(cx, &f, "1");
        assert_eq!(tab_state(cx, &f), (SettingsTab::Servers, 9));
    }

    #[gpui_kit::test]
    #[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
    fn space_flips_each_option_saves_it_and_the_shell_follows(cx: &mut TestAppContext) {
        let f = fixture(cx, false);
        show(cx, &f);
        assert!(page_settings(cx, &f).enabled);
        press(cx, &f, "space");
        assert!(!page_settings(cx, &f).enabled);
        assert!(!shell_settings(cx, &f).enabled);
        press(cx, &f, "j");
        press(cx, &f, "space");
        press(cx, &f, "j");
        press(cx, &f, "space");
        press(cx, &f, "j");
        press(cx, &f, "space");
        let s = page_settings(cx, &f);
        assert!(!s.enabled && !s.only_when_unfocused);
        assert!(!s.suppress_current_change && !s.ignore_own_actions);
        assert_eq!(writes(&f).len(), 4);
        // Enter flips as well, back on.
        press(cx, &f, "enter");
        assert!(page_settings(cx, &f).ignore_own_actions);
        assert_eq!(shell_settings(cx, &f), page_settings(cx, &f));
    }

    #[gpui_kit::test]
    fn a_failed_save_changes_nothing_and_says_so(cx: &mut TestAppContext) {
        let f = fixture(cx, true);
        show(cx, &f);
        press(cx, &f, "space");
        assert!(page_settings(cx, &f).enabled);
        assert!(shell_settings(cx, &f).enabled);
        cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            let (text, error) = page.read(cx).hint_for_test().expect("hint");
            assert!(error && text.contains("disk full"), "{text}");
        });
    }

    #[gpui_kit::test]
    #[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
    fn the_group_window_takes_whole_seconds(cx: &mut TestAppContext) {
        let f = fixture(cx, false);
        show(cx, &f);
        for _ in 0..4 {
            press(cx, &f, "j");
        }
        press(cx, &f, "enter");
        assert!(editing(cx, &f));
        let input = notify_input(cx, &f);
        // The input starts on the current value.
        assert_eq!(cx.update(|cx| input.read(cx).value().to_string()), "20");
        for bad in ["", "soon", "-3", "1.5", "99999"] {
            type_into(cx, &f, &input, bad);
            press(cx, &f, "enter");
            assert!(editing(cx, &f), "{bad:?}");
        }
        assert!(writes(&f).is_empty());
        type_into(cx, &f, &input, " 45 ");
        press(cx, &f, "enter");
        assert!(!editing(cx, &f));
        assert_eq!(page_settings(cx, &f).group_window.as_secs(), 45);
        assert_eq!(shell_settings(cx, &f).group_window.as_secs(), 45);
        // Zero: every event on its own.
        press(cx, &f, "e");
        type_into(cx, &f, &input, "0");
        press(cx, &f, "enter");
        assert_eq!(page_settings(cx, &f).group_window.as_secs(), 0);
        assert_eq!(writes(&f).len(), 2);
    }

    #[gpui_kit::test]
    #[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
    fn quiet_hours_are_validated_and_can_be_cleared(cx: &mut TestAppContext) {
        let f = fixture(cx, false);
        show(cx, &f);
        for _ in 0..5 {
            press(cx, &f, "j");
        }
        press(cx, &f, "e");
        let input = notify_input(cx, &f);
        for bad in ["22-07", "25:00-07:00", "22:00", "22:00-7:00"] {
            type_into(cx, &f, &input, bad);
            press(cx, &f, "enter");
            assert!(editing(cx, &f), "{bad:?}");
        }
        assert!(writes(&f).is_empty());
        type_into(cx, &f, &input, "22:00-07:00");
        press(cx, &f, "enter");
        assert_eq!(
            page_settings(cx, &f).quiet_hours.map(|q| q.format()),
            Some("22:00-07:00".to_owned())
        );
        // The next edit starts on the saved text; empty clears it.
        press(cx, &f, "space");
        assert_eq!(
            cx.update(|cx| input.read(cx).value().to_string()),
            "22:00-07:00"
        );
        type_into(cx, &f, &input, "");
        press(cx, &f, "enter");
        assert_eq!(page_settings(cx, &f).quiet_hours, None);
        // Escape drops an edit.
        press(cx, &f, "e");
        type_into(cx, &f, &input, "01:00-02:00");
        press(cx, &f, "escape");
        assert!(!editing(cx, &f));
        assert_eq!(page_settings(cx, &f).quiet_hours, None);
        assert_eq!(writes(&f).len(), 2);
    }

    /// Where the rules start: the toggles and the timing rows are before.
    const RULES: usize = FIRST + 6;

    fn rule_titles(cx: &mut TestAppContext, f: &Fixture) -> Vec<String> {
        page_settings(cx, f)
            .rules
            .iter()
            .map(|r| r.title())
            .collect()
    }

    fn at(cx: &mut TestAppContext, f: &Fixture, row: usize) {
        cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            page.update(cx, |p, cx| p.select_row_for_test(row, cx));
        });
    }

    fn hint(cx: &mut TestAppContext, f: &Fixture) -> Option<(String, bool)> {
        cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            page.read(cx).hint_for_test()
        })
    }

    fn defaults(cx: &mut TestAppContext, f: &Fixture) -> bool {
        cx.update(|cx| {
            let page = f.shell.read(cx).settings_page_for_test().expect("page");
            page.read(cx).rules_are_defaults_for_test()
        })
    }

    #[gpui_kit::test]
    #[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
    fn the_default_rules_are_listed_and_the_first_change_writes_them_all(cx: &mut TestAppContext) {
        let f = fixture(cx, false);
        show(cx, &f);
        assert!(defaults(cx, &f));
        assert_eq!(rule_titles(cx, &f).len(), 4);
        assert_eq!(rule_titles(cx, &f)[1], "Negative Code-Review on my changes");
        // A global option writes no rules of its own: the defaults stay
        // the defaults.
        assert!(writes(&f).is_empty());
        // Switching a rule off writes the whole list, with that one off.
        at(cx, &f, RULES);
        assert_eq!(tab_state(cx, &f).1, RULES);
        press(cx, &f, "space");
        let written = writes(&f);
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].rules.len(), 4);
        assert!(!written[0].rules[0].enabled && written[0].rules[1].enabled);
        assert!(!defaults(cx, &f));
        assert!(!shell_settings(cx, &f).rules[0].enabled);
        // And back on.
        press(cx, &f, "space");
        assert!(page_settings(cx, &f).rules[0].enabled);
    }

    #[gpui_kit::test]
    #[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
    fn rules_move_with_shift_j_and_k_and_the_cursor_follows(cx: &mut TestAppContext) {
        let f = fixture(cx, false);
        show(cx, &f);
        at(cx, &f, RULES);
        let before = rule_titles(cx, &f);
        press(cx, &f, "shift-j");
        let after = rule_titles(cx, &f);
        assert_eq!(after[1], before[0]);
        assert_eq!(after[0], before[1]);
        assert_eq!(tab_state(cx, &f).1, RULES + 1);
        press(cx, &f, "shift-k");
        assert_eq!(rule_titles(cx, &f), before);
        assert_eq!(tab_state(cx, &f).1, RULES);
        // The first rule cannot move up, the last cannot move down.
        press(cx, &f, "shift-k");
        assert_eq!(writes(&f).len(), 2);
        at(cx, &f, RULES + 3);
        press(cx, &f, "shift-j");
        assert_eq!(writes(&f).len(), 2);
    }

    #[gpui_kit::test]
    #[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
    fn deleting_a_rule_asks_first_and_the_last_one_brings_the_defaults_back(
        cx: &mut TestAppContext,
    ) {
        let f = fixture(cx, false);
        show(cx, &f);
        at(cx, &f, RULES + 1);
        press(cx, &f, "d");
        press(cx, &f, "n");
        assert_eq!(rule_titles(cx, &f).len(), 4);
        assert!(writes(&f).is_empty());
        press(cx, &f, "d");
        press(cx, &f, "y");
        assert_eq!(rule_titles(cx, &f).len(), 3);
        assert_eq!(writes(&f).len(), 1);
        assert!(!defaults(cx, &f));
        // The cursor stays on the place of the deleted rule.
        assert_eq!(tab_state(cx, &f).1, RULES + 1);
        // Down to no rules: the file then has none, and the defaults
        // apply again, which the page says.
        for _ in 0..3 {
            at(cx, &f, RULES);
            press(cx, &f, "d");
            press(cx, &f, "enter");
        }
        assert!(writes(&f).last().expect("write").rules.is_empty());
        assert!(defaults(cx, &f));
        let (text, error) = hint(cx, &f).expect("hint");
        assert!(!error && text.contains("default rules"), "{text}");
    }

    #[gpui_kit::test]
    #[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
    fn a_failed_delete_changes_nothing_and_says_so(cx: &mut TestAppContext) {
        let f = fixture(cx, true);
        show(cx, &f);
        at(cx, &f, RULES);
        press(cx, &f, "d");
        press(cx, &f, "y");
        assert_eq!(rule_titles(cx, &f).len(), 4);
        let (text, error) = hint(cx, &f).expect("hint");
        assert!(error && text.contains("disk full"), "{text}");
        // The question is over: a key works again.
        press(cx, &f, "j");
        assert_eq!(tab_state(cx, &f).1, RULES + 1);
    }

    #[gpui_kit::test]
    #[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
    fn reset_to_defaults_asks_and_is_idle_on_the_defaults(cx: &mut TestAppContext) {
        let f = fixture(cx, false);
        show(cx, &f);
        let reset = RULES + 4;
        at(cx, &f, reset);
        press(cx, &f, "enter");
        let (text, _) = hint(cx, &f).expect("hint");
        assert!(text.contains("already"), "{text}");
        assert!(writes(&f).is_empty());

        at(cx, &f, RULES);
        press(cx, &f, "space");
        assert!(!defaults(cx, &f));
        at(cx, &f, reset);
        press(cx, &f, "enter");
        press(cx, &f, "n");
        assert!(!defaults(cx, &f));
        press(cx, &f, "enter");
        press(cx, &f, "y");
        assert!(defaults(cx, &f));
        assert!(page_settings(cx, &f).rules[0].enabled);
    }

    fn with_notifier(
        cx: &mut TestAppContext,
        f: &Fixture,
    ) -> Arc<crate::testing::RecordingNotifier> {
        let notifier = Arc::new(crate::testing::RecordingNotifier::default());
        // Off, so that nothing is watched and only the test is shown.
        let settings = NotificationSettings {
            enabled: false,
            ..Default::default()
        };
        let n = notifier.clone();
        cx.update_window(f.window, |_, window, cx| {
            f.shell
                .update(cx, |s, cx| s.set_notifications(n, settings, window, cx))
        })
        .expect("window");
        cx.run_until_parked();
        notifier
    }

    #[gpui_kit::test]
    #[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
    fn the_test_row_and_t_show_a_notification_through_the_notifier(cx: &mut TestAppContext) {
        let f = fixture(cx, false);
        let notifier = with_notifier(cx, &f);
        show(cx, &f);
        at(cx, &f, RULES + 5);
        press(cx, &f, "enter");
        assert_eq!(notifier.titles(), ["Test notification"]);
        let (text, error) = hint(cx, &f).expect("hint");
        assert!(!error && text.starts_with("Sent"), "{text}");
        assert!(text.contains("daemon"), "{text}");
        // `t` works from any row of the tab, and while the master
        // switch is off.
        at(cx, &f, FIRST);
        press(cx, &f, "t");
        assert_eq!(notifier.shown().len(), 2);
    }

    #[gpui_kit::test]
    fn without_a_notifier_the_test_says_so(cx: &mut TestAppContext) {
        let f = fixture(cx, false);
        show(cx, &f);
        press(cx, &f, "t");
        let (text, error) = hint(cx, &f).expect("hint");
        assert!(error && text.contains("not available"), "{text}");
    }

    #[gpui_kit::test]
    #[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
    fn tab_and_row_keys_are_typed_while_an_input_is_edited(cx: &mut TestAppContext) {
        let f = fixture(cx, false);
        show(cx, &f);
        for _ in 0..4 {
            press(cx, &f, "j");
        }
        press(cx, &f, "e");
        for key in ["h", "l", "6", "1", "j", "space"] {
            press(cx, &f, key);
        }
        assert_eq!(tab_state(cx, &f), (SettingsTab::Notifications, FIRST + 4));
        assert!(editing(cx, &f));
        assert!(writes(&f).is_empty());
    }
}
