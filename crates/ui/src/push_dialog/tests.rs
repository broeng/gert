use std::sync::{Arc, Mutex};

use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
use local_git::testutil::{TestRepo, git_in};
use model::RevisionId;

use super::{PushDialog, PushEvent, PushRequest};
use crate::{checkouts::Checkouts, session::ProjectSettings, tests::open};

struct Fx {
    window: AnyWindowHandle,
    dialog: Entity<PushDialog>,
    events: Arc<Mutex<Vec<String>>>,
    remote: TestRepo,
    sha: String,
    _repo: TestRepo,
}

/// A checkout of project `p` with remotes `origin` (no push rule) and
/// `review` (a bare repository, `rule`), and a local commit on `main`
/// that neither has; the dialog to push it.
fn dialog(cx: &mut TestAppContext, rule: &str) -> Fx {
    let remote = TestRepo::bare();
    let repo = TestRepo::new();
    repo.write("a", b"a\n");
    repo.commit("Base");
    let url = remote.path().to_string_lossy().into_owned();
    repo.git(&["remote", "add", "origin", "https://example.com/p"]);
    repo.git(&["remote", "add", "review", url.as_str()]);
    repo.git(&["push", "-q", "review", "main"]);
    repo.git(&["fetch", "-q", "review"]);
    repo.git(&["config", "remote.review.push", rule]);
    repo.write("a", b"b\n");
    let sha = repo.commit("Change it\n\nChange-Id: I1");
    let projects = vec![ProjectSettings::new("p", repo.path().to_string_lossy())];
    let request = PushRequest {
        project: "p".into(),
        sha: RevisionId(sha.clone()),
        version: "local 1".into(),
        subject: "Change it".into(),
        branch: Some("main".into()),
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let log = events.clone();
    let (window, dialog) = open(cx, |window, cx| {
        let checkouts = cx.new(|_| Checkouts::new(projects));
        let dialog = cx.new(|cx| {
            PushDialog::new(
                request,
                checkouts,
                "https://review.example.com".into(),
                window,
                cx,
            )
        });
        cx.subscribe(&dialog, move |_, ev: &PushEvent, _| {
            let text = match ev {
                PushEvent::Close => "close".to_owned(),
                PushEvent::Pushed(note) => format!("pushed: {note}"),
            };
            log.lock().expect("log").push(text);
        })
        .detach();
        dialog
    });
    cx.run_until_parked();
    Fx {
        window,
        dialog,
        events,
        remote,
        sha,
        _repo: repo,
    }
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_push_follows_the_remote_s_rule_with_the_commit_for_head(cx: &mut TestAppContext) {
    let f = dialog(cx, "HEAD:refs/for/main");
    f.dialog.read_with(cx, |d, _| {
        let (choices, without, selected) = d.choices_for_test();
        assert_eq!(choices.len(), 1);
        assert_eq!(
            (choices[0].remote.as_str(), choices[0].rule.as_str()),
            ("review", "HEAD:refs/for/main")
        );
        assert_eq!(without, ["origin"]);
        assert_eq!(selected, 0);
        let outgoing = d.outgoing_for_test().expect("the commits");
        assert_eq!(outgoing.commits.len(), 1);
        assert_eq!(outgoing.commits[0].sha.as_str(), f.sha);
    });
    cx.simulate_keystrokes(f.window, "enter");
    cx.run_until_parked();
    assert_eq!(
        f.events.lock().expect("log").clone(),
        ["pushed: Pushed local 1 to review."]
    );
    assert_eq!(
        git_in(f.remote.path(), &["rev-parse", "refs/for/main"]),
        f.sha
    );
}

#[gpui_kit::test]
#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]
fn a_refused_push_keeps_the_dialog_open_with_git_s_words(cx: &mut TestAppContext) {
    // git refuses a destination that is no valid ref name.
    let f = dialog(cx, "HEAD:refs/for/main..x");
    cx.simulate_keystrokes(f.window, "enter");
    cx.run_until_parked();
    assert!(f.events.lock().expect("log").is_empty());
    let failure = f
        .dialog
        .read_with(cx, |d, _| d.failure_for_test().map(str::to_owned))
        .expect("a failure");
    assert!(!failure.is_empty());
    cx.simulate_keystrokes(f.window, "escape");
    assert_eq!(f.events.lock().expect("log").clone(), ["close"]);
}
