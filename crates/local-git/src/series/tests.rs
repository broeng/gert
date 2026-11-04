use model::RevisionId;

use super::Series;
use crate::testutil::TestRepo;
use crate::{Local, NotLocal, Repo};

fn rev(s: &str) -> RevisionId {
    RevisionId(s.to_owned())
}

/// A repository with one commit on `main`, pushed to a bare `origin`.
/// Returns the remote (kept alive), the repository and the pushed commit.
fn pushed() -> (TestRepo, TestRepo, String) {
    let remote = TestRepo::bare();
    let r = TestRepo::new();
    let base = r.commit("Base");
    let url = remote.path().to_str().expect("utf-8 temp path").to_owned();
    r.git(&["remote", "add", "origin", &url]);
    r.git(&["push", "-q", "-u", "origin", "main"]);
    (remote, r, base)
}

fn series(r: &TestRepo, sha: &str, limit: usize) -> Series {
    match Repo::open(r.path())
        .expect("open")
        .series(&rev(sha), limit)
        .expect("series")
    {
        Local::Available(s) => s,
        Local::NotLocal(why) => panic!("not local: {why}"),
    }
}

fn shas(commits: &[model::CommitInfo]) -> Vec<&str> {
    commits.iter().map(|c| c.sha.as_str()).collect()
}

#[test]
fn ancestors_stop_before_the_remote_and_descendants_follow_the_branch() {
    let (_remote, r, _base) = pushed();
    let c1 = r.commit("One");
    let c2 = r.commit("Two");
    let c3 = r.commit("Three");
    let s = series(&r, &c2, 10);
    assert_eq!(shas(&s.ancestors), [c2.as_str(), c1.as_str()]);
    assert_eq!(shas(&s.descendants), [c3.as_str()]);
    // The commits come with their data.
    assert_eq!(s.ancestors[1].subject(), "One");
    assert_eq!(s.ancestors[0].parents, [rev(&c1)]);
}

#[test]
fn descendants_on_every_branch_that_contains_it_newest_first() {
    let (_remote, r, _base) = pushed();
    let c1 = r.commit("One");
    let c2 = r.commit("Two");
    r.git(&["checkout", "-q", "-b", "topic", &c1]);
    let t1 = r.commit("Topic one");
    let t2 = r.commit("Topic two");
    // Built on c1 but on no branch: not part of the series. With HEAD
    // detached, no branch is the checked-out one, so all count.
    r.git(&["checkout", "-q", "--detach"]);
    let detached = r.commit("Detached");
    let s = series(&r, &c1, 10);
    assert_eq!(shas(&s.ancestors), [c1.as_str()]);
    let d = shas(&s.descendants);
    assert_eq!(d.len(), 3, "{d:?}");
    for sha in [&c2, &t1, &t2] {
        assert!(d.contains(&sha.as_str()), "{sha} missing from {d:?}");
    }
    assert!(!d.contains(&detached.as_str()));
    let at = |sha: &str| d.iter().position(|s| *s == sha);
    assert!(at(&t2) < at(&t1), "a child comes before its parent");
}

#[test]
fn descendants_follow_the_checked_out_branch_when_it_contains_the_commit() {
    let (_remote, r, base) = pushed();
    let c1 = r.commit("One");
    let w1 = r.commit("Wip");
    // A branch left behind at the first version of the reordered commit.
    r.git(&["branch", "old"]);
    r.git(&["reset", "-q", "--hard", &c1]);
    let c2 = r.commit("Two");
    r.write("wip", b"again\n");
    let w2 = r.commit("Wip");
    let s = series(&r, &c1, 10);
    assert_eq!(shas(&s.descendants), [w2.as_str(), c2.as_str()]);

    // On a branch without the commit, every branch that has it counts.
    r.git(&["checkout", "-q", "-b", "elsewhere", &base]);
    let d = shas(&series(&r, &c1, 10).descendants)
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(d.len(), 3, "{d:?}");
    for sha in [&w1, &c2, &w2] {
        assert!(d.contains(sha), "{sha} missing from {d:?}");
    }
}

#[test]
fn ancestors_follow_first_parents_only() {
    let (_remote, r, _base) = pushed();
    let c1 = r.commit("One");
    r.git(&["checkout", "-q", "-b", "side"]);
    let side = r.commit("Side");
    r.git(&["checkout", "-q", "main"]);
    let c2 = r.commit("Two");
    r.git(&["merge", "-q", "--no-ff", "-m", "Merge side", "side"]);
    let merge = r.git(&["rev-parse", "HEAD"]);
    let s = series(&r, &merge, 10);
    assert_eq!(
        shas(&s.ancestors),
        [merge.as_str(), c2.as_str(), c1.as_str()]
    );
    assert!(!shas(&s.ancestors).contains(&side.as_str()));
}

#[test]
fn a_pushed_commit_is_only_its_own_ancestor() {
    let (_remote, r, base) = pushed();
    let c1 = r.commit("One");
    let s = series(&r, &base, 10);
    assert_eq!(shas(&s.ancestors), [base.as_str()]);
    assert_eq!(shas(&s.descendants), [c1.as_str()]);
}

#[test]
fn both_sides_are_capped() {
    let (_remote, r, _base) = pushed();
    let commits: Vec<String> = (0..7).map(|i| r.commit(&format!("C{i}"))).collect();
    let s = series(&r, &commits[3], 2);
    assert_eq!(
        shas(&s.ancestors),
        [commits[3].as_str(), commits[2].as_str()]
    );
    // The two nearest the commit, so nothing is missing right above it.
    assert_eq!(
        shas(&s.descendants),
        [commits[5].as_str(), commits[4].as_str()]
    );
    assert!(s.more_ancestors && s.more_descendants);
    // Exactly the limit is not more.
    let s = series(&r, &commits[3], 4);
    assert_eq!(s.ancestors.len(), 4);
    assert_eq!(s.descendants.len(), 3);
    assert!(!s.more_ancestors && !s.more_descendants);
}

#[test]
fn upstream_history_merged_into_the_branch_is_not_built_on_the_commit() {
    let (_remote, r, base) = pushed();
    // origin/main moves on.
    r.git(&["checkout", "-q", "-b", "up", &base]);
    r.commit("Upstream one");
    r.commit("Upstream two");
    r.git(&["push", "-q", "origin", "up:main"]);
    r.git(&["checkout", "-q", "main"]);
    let c1 = r.commit("One");
    r.git(&[
        "merge",
        "-q",
        "--no-ff",
        "-m",
        "Merge upstream",
        "origin/main",
    ]);
    let merge = r.git(&["rev-parse", "HEAD"]);
    let c2 = r.commit("Two");
    let s = series(&r, &c1, 10);
    assert_eq!(shas(&s.descendants), [c2.as_str(), merge.as_str()]);
    assert!(!s.more_descendants);
    assert_eq!(shas(&s.ancestors), [c1.as_str()]);
}

#[test]
fn a_missing_or_malformed_commit_is_not_local() {
    let (_remote, r, _base) = pushed();
    let repo = Repo::open(r.path()).expect("open");
    let missing = "0".repeat(40);
    assert_eq!(
        repo.series(&rev(&missing), 5).expect("series"),
        Local::NotLocal(NotLocal::MissingCommit(missing.clone()))
    );
    assert!(matches!(
        repo.series(&rev("--all"), 5).expect("series"),
        Local::NotLocal(NotLocal::MissingCommit(_))
    ));
}

#[test]
fn without_remotes_the_cap_ends_the_walk() {
    let r = TestRepo::new();
    let c0 = r.commit("Zero");
    let c1 = r.commit("One");
    let c2 = r.commit("Two");
    let s = series(&r, &c2, 2);
    assert_eq!(shas(&s.ancestors), [c2.as_str(), c1.as_str()]);
    let s = series(&r, &c2, 10);
    assert_eq!(shas(&s.ancestors), [c2.as_str(), c1.as_str(), c0.as_str()]);
    assert!(s.descendants.is_empty());
}
