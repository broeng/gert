use std::time::Duration;

use model::RevisionId;

use super::{OutgoingCommit, head_rule};
use crate::Repo;
use crate::testutil::{TestRepo, git_in};

fn rev(s: &str) -> RevisionId {
    RevisionId(s.to_owned())
}

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

#[test]
fn a_head_rule_pushes_the_commit_with_the_rest_of_the_rule() {
    assert_eq!(
        head_rule("HEAD:refs/for/main", &rev(SHA)).as_deref(),
        Some(format!("{SHA}:refs/for/main").as_str())
    );
    assert_eq!(
        head_rule("+HEAD:refs/for/main%topic=x,wip", &rev(SHA)).as_deref(),
        Some(format!("+{SHA}:refs/for/main%topic=x,wip").as_str())
    );
    for other in [
        "refs/heads/*:refs/for/*",
        "HEAD",
        "HEAD:",
        "HEADS:refs/for/main",
        "main:refs/for/main",
    ] {
        assert_eq!(head_rule(other, &rev(SHA)), None, "{other}");
    }
}

#[test]
fn remotes_come_with_their_push_url_and_rules() {
    let repo = TestRepo::new();
    repo.git(&["remote", "add", "origin", "https://example.com/p"]);
    repo.git(&["remote", "add", "gerrit.review", "ssh://me@review:29418/p"]);
    repo.git(&[
        "config",
        "--add",
        "remote.gerrit.review.push",
        "HEAD:refs/for/main",
    ]);
    repo.git(&[
        "config",
        "--add",
        "remote.gerrit.review.push",
        "HEAD:refs/for/stable",
    ]);
    repo.git(&["config", "remote.origin.pushurl", "short:p"]);
    repo.git(&["config", "url.ssh://push.example.com/.insteadOf", "short:"]);
    let remotes = Repo::open(repo.path()).unwrap().remotes().unwrap();
    let names: Vec<_> = remotes.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["origin", "gerrit.review"]);
    assert_eq!(remotes[0].push_url, "ssh://push.example.com/p");
    assert!(remotes[0].push_rules.is_empty());
    assert_eq!(remotes[1].push_url, "ssh://me@review:29418/p");
    assert_eq!(
        remotes[1].push_rules,
        ["HEAD:refs/for/main", "HEAD:refs/for/stable"]
    );
}

#[test]
fn a_repository_without_remotes_has_none() {
    let repo = TestRepo::new();
    assert!(
        Repo::open(repo.path())
            .unwrap()
            .remotes()
            .unwrap()
            .is_empty()
    );
}

/// A checkout with a bare "remote" holding `main`, and two local commits
/// on top: (checkout, remote, first, second).
fn with_remote() -> (TestRepo, TestRepo, String, String) {
    let remote = TestRepo::bare();
    let repo = TestRepo::new();
    repo.write("a", b"a\n");
    repo.commit("base");
    let url = remote.path().to_string_lossy().into_owned();
    repo.git(&["remote", "add", "r", url.as_str()]);
    repo.git(&["push", "-q", "r", "main"]);
    repo.git(&["fetch", "-q", "r"]);
    repo.write("a", b"b\n");
    let first = repo.commit("first");
    repo.write("a", b"c\n");
    let second = repo.commit("second");
    (repo, remote, first, second)
}

#[test]
fn outgoing_lists_what_the_remote_does_not_have() {
    let (repo, _remote, first, second) = with_remote();
    let r = Repo::open(repo.path()).unwrap();
    let out = r.outgoing(&rev(&second), "r", 10).unwrap();
    assert_eq!(
        out.commits,
        [
            OutgoingCommit {
                sha: rev(&second),
                subject: "second".into()
            },
            OutgoingCommit {
                sha: rev(&first),
                subject: "first".into()
            },
        ]
    );
    assert!(!out.more);
    let capped = r.outgoing(&rev(&second), "r", 1).unwrap();
    assert_eq!(capped.commits.len(), 1);
    assert!(capped.more);
}

#[test]
fn a_push_sends_the_commit_by_the_rule_and_leaves_the_config_alone() {
    let (repo, remote, first, _second) = with_remote();
    repo.git(&["config", "remote.r.push", "HEAD:refs/for/main"]);
    let config = std::fs::read(repo.path().join(".git/config")).unwrap();
    let r = Repo::open(repo.path()).unwrap();
    let out = r
        .push(
            "r",
            &rev(&first),
            "HEAD:refs/for/main",
            Duration::from_secs(60),
        )
        .unwrap();
    assert!(out.success, "{}", out.output);
    assert_eq!(
        git_in(remote.path(), &["rev-parse", "refs/for/main"]),
        first
    );
    // The branch on the remote did not move, and nothing local changed.
    assert_ne!(git_in(remote.path(), &["rev-parse", "main"]), first);
    assert_eq!(
        std::fs::read(repo.path().join(".git/config")).unwrap(),
        config
    );
}

#[test]
fn a_refused_push_is_an_outcome_with_git_s_words() {
    let (repo, remote, first, second) = with_remote();
    let r = Repo::open(repo.path()).unwrap();
    let timeout = Duration::from_secs(60);
    assert!(
        r.push("r", &rev(&second), "HEAD:refs/for/main", timeout)
            .unwrap()
            .success
    );
    // Not a fast-forward of what is there now.
    let out = r
        .push("r", &rev(&first), "HEAD:refs/for/main", timeout)
        .unwrap();
    assert!(!out.success);
    assert!(out.output.contains("rejected"), "{}", out.output);
    assert_eq!(
        git_in(remote.path(), &["rev-parse", "refs/for/main"]),
        second
    );
}

#[test]
fn a_push_refuses_odd_arguments() {
    let (repo, _remote, first, _) = with_remote();
    let r = Repo::open(repo.path()).unwrap();
    let t = Duration::from_secs(60);
    assert!(
        r.push("--mirror", &rev(&first), "HEAD:refs/for/main", t)
            .is_err()
    );
    assert!(
        r.push("r", &rev("HEAD~1"), "HEAD:refs/for/main", t)
            .is_err()
    );
    assert!(r.push("r", &rev(&first), "main:refs/for/main", t).is_err());
}
