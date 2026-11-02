use std::collections::BTreeMap;

use model::{BranchTargets, LocalCommit, LocalScan, RevisionId, Timestamp, Upstream};

use super::change_id_trailer;
use crate::testutil::{TEST_EMAIL, TestRepo};
use crate::{Local, NotLocal, Repo};

const ID_A: &str = "I0123456789abcdef0123456789abcdef01234567";
const ID_B: &str = "Ifedcba9876543210fedcba9876543210fedcba98";

fn rev(s: &str) -> RevisionId {
    RevisionId(s.to_owned())
}

fn open(r: &TestRepo) -> Repo {
    Repo::open(r.path()).expect("open")
}

/// A repository with one commit on `main`, pushed to a bare `origin`
/// with `main` tracking `origin/main`. Returns the remote (kept alive),
/// the repository and the pushed commit.
fn pushed() -> (TestRepo, TestRepo, String) {
    let remote = TestRepo::bare();
    let r = TestRepo::new();
    let base = r.commit("Base");
    let url = remote.path().to_str().expect("utf-8 temp path").to_owned();
    r.git(&["remote", "add", "origin", &url]);
    r.git(&["push", "-q", "-u", "origin", "main"]);
    (remote, r, base)
}

/// Writes a commit object by hand (fixed tree, the given parents, author
/// and committer at `time`) so tests control dates and message bytes.
fn raw_commit(r: &TestRepo, parents: &[&str], time: i64, message: &str) -> String {
    let tree = r.git(&["write-tree"]);
    let mut object = format!("tree {tree}\n");
    for p in parents {
        object.push_str(&format!("parent {p}\n"));
    }
    object.push_str(&format!(
        "author A U Thor <thor@example.com> {time} +0200\n\
         committer C O Mitter <mitter@example.com> {time} -0130\n\n{message}"
    ));
    let file = r.path().join(".git").join("raw-commit");
    std::fs::write(&file, object).expect("write object");
    let file = file.to_str().expect("utf-8 temp path");
    r.git(&["hash-object", "-t", "commit", "-w", file])
}

/// The commits of a scan that must not have been cut by the limit.
fn complete(repo: &Repo, limit: usize) -> Vec<LocalCommit> {
    let scan = repo.scan(limit).expect("scan");
    assert!(scan.is_complete(), "truncated: {}", scan.truncated);
    scan.commits
}

fn shas(commits: &[LocalCommit]) -> Vec<&str> {
    commits.iter().map(|c| c.commit.sha.as_str()).collect()
}

fn branches_of<'a>(commits: &'a [LocalCommit], sha: &str) -> Vec<&'a str> {
    let c = commits
        .iter()
        .find(|c| c.commit.sha.as_str() == sha)
        .expect("commit listed");
    c.branches.iter().map(String::as_str).collect()
}

#[test]
fn unpushed_commits_on_named_branches_are_listed_with_their_branches() {
    let (_remote, r, base) = pushed();
    let c1 = r.commit(&format!("Main one\n\nChange-Id: {ID_A}"));
    r.git(&["checkout", "-q", "-b", "topic"]);
    let c2 = r.commit("Topic one");
    // Only reachable from HEAD: not a local change.
    r.git(&["checkout", "-q", "--detach"]);
    let c3 = r.commit("Detached");
    let repo = open(&r);

    let scan = repo.scan(200).expect("scan");
    assert!(!scan.truncated && !scan.no_remotes);
    assert_eq!(
        scan.fingerprint,
        repo.refs_fingerprint().expect("fingerprint")
    );
    let local = scan.commits;
    assert_eq!(shas(&local), [c2.as_str(), c1.as_str()]);
    assert!(!shas(&local).contains(&base.as_str()));
    assert!(!shas(&local).contains(&c3.as_str()));
    assert_eq!(branches_of(&local, &c1), ["main", "topic"]);
    assert_eq!(branches_of(&local, &c2), ["topic"]);
    assert_eq!(local[1].commit.change_id.as_deref(), Some(ID_A));
    assert_eq!(local[1].commit.subject(), "Main one");
    assert_eq!(local[1].commit.parents, [rev(&base)]);
    assert_eq!(local[0].commit.change_id, None);
    assert!(local.iter().all(|c| !c.is_merge()));

    // The limit keeps the newest and says it cut something.
    let scan = repo.scan(1).expect("limit 1");
    assert_eq!(shas(&scan.commits), [c2.as_str()]);
    assert!(scan.truncated);
    let scan = repo.scan(0).expect("limit 0");
    assert!(scan.commits.is_empty() && scan.truncated);
    assert_eq!(complete(&repo, 2).len(), 2);

    // Once on origin, a commit is no longer local.
    r.git(&["push", "-q", "origin", "main"]);
    let local = complete(&repo, 200);
    assert_eq!(shas(&local), [c2.as_str()]);
    assert_eq!(branches_of(&local, &c2), ["topic"]);
}

#[test]
fn children_come_before_their_parents_whatever_the_dates() {
    let (_remote, r, base) = pushed();
    // b1 is a child of a0 but older than it, so date order would list a0
    // before b1.
    let a0 = raw_commit(&r, &[&base], 2_000_000_000, "a0\n");
    let a1 = raw_commit(&r, &[&a0], 2_000_003_000, "a1\n");
    let b1 = raw_commit(&r, &[&a0], 1_900_000_000, "b1\n");
    r.git(&["update-ref", "refs/heads/a", &a1]);
    r.git(&["update-ref", "refs/heads/b", &b1]);
    let local = complete(&open(&r), 200);

    assert_eq!(local.len(), 3);
    assert_eq!(local[2].commit.sha.as_str(), a0);
    for (i, c) in local.iter().enumerate() {
        for parent in &c.commit.parents {
            if let Some(j) = local.iter().position(|o| &o.commit.sha == parent) {
                assert!(
                    j > i,
                    "{parent:?} listed before its child {:?}",
                    c.commit.sha
                );
            }
        }
    }
    assert_eq!(branches_of(&local, &a0), ["a", "b"]);
    assert_eq!(branches_of(&local, &b1), ["b"]);
}

#[test]
fn the_limit_keeps_the_newest_commits_of_every_branch() {
    let (_remote, r, base) = pushed();
    // An old line of history with a new tip, and a newer branch.
    let mut old = vec![base.clone()];
    for i in 0..10 {
        let parent = old[old.len() - 1].clone();
        old.push(raw_commit(
            &r,
            &[&parent],
            1_580_000_000 + i * 1000,
            "old\n",
        ));
    }
    let old_last = old[old.len() - 1].clone();
    let old_tip = raw_commit(&r, &[&old_last], 1_790_000_000, "old tip\n");
    let mut new = vec![base];
    for i in 0..3 {
        let parent = new[new.len() - 1].clone();
        new.push(raw_commit(
            &r,
            &[&parent],
            1_789_900_000 + i * 1000,
            "new\n",
        ));
    }
    r.git(&["update-ref", "refs/heads/old", &old_tip]);
    r.git(&["update-ref", "refs/heads/new", &new[3]]);
    let scan = open(&r).scan(5).expect("scan");
    assert!(scan.truncated);
    let local = scan.commits;

    assert_eq!(
        shas(&local),
        [
            old_tip.as_str(),
            new[3].as_str(),
            new[2].as_str(),
            new[1].as_str(),
            old[10].as_str()
        ]
    );
}

#[test]
fn merge_commits_are_listed_and_flagged() {
    let (_remote, r, _base) = pushed();
    r.git(&["checkout", "-q", "-b", "topic"]);
    let t = r.commit("Topic");
    r.git(&["checkout", "-q", "main"]);
    let m1 = r.commit("Main");
    r.git(&["merge", "-q", "--no-ff", "-m", "Merge topic", "topic"]);
    let merge = r.git(&["rev-parse", "HEAD"]);
    let local = complete(&open(&r), 200);

    assert_eq!(local.len(), 3);
    assert_eq!(local[0].commit.sha.as_str(), merge);
    assert!(local[0].is_merge());
    assert_eq!(local[0].commit.parents, [rev(&m1), rev(&t)]);
    assert!(local[1..].iter().all(|c| !c.is_merge()));
    assert_eq!(branches_of(&local, &t), ["main", "topic"]);
    assert_eq!(branches_of(&local, &merge), ["main"]);
}

#[test]
fn change_id_trailer_takes_the_last_one_in_the_last_paragraph() {
    let cases: &[(String, Option<&str>)] = &[
        (
            format!("Subject\n\nBody\n\nChange-Id: {ID_A}\nSigned-off-by: A <a@b>\n"),
            Some(ID_A),
        ),
        (
            format!("Subject\n\nChange-Id: {ID_A}\nChange-Id: {ID_B}\n"),
            Some(ID_B),
        ),
        // In the body only: not a trailer.
        (format!("Subject\n\nChange-Id: {ID_A}\n\nMore text\n"), None),
        (
            format!("Subject\n\nChange-Id: {ID_A}\n\nLink: x\nChange-Id: {ID_B}\n"),
            Some(ID_B),
        ),
        (
            format!("Subject\r\n\r\nBody\r\n\r\nChange-Id: {ID_A}\r\n"),
            Some(ID_A),
        ),
        (format!("Subject\n\nChange-Id: {ID_A}"), Some(ID_A)),
        (format!("Subject\n\nChange-Id: {ID_A}\n\n\n"), Some(ID_A)),
        // A line with only whitespace does not end a paragraph.
        (
            format!("Subject\n\nChange-Id: {ID_A}\n \t\nBody\n"),
            Some(ID_A),
        ),
        (format!("Subject\n\nChange-Id: {ID_A}\n\nBody\n \n"), None),
        (format!("Subject\nChange-Id: {ID_A}\n"), Some(ID_A)),
        (format!("change-id:   {ID_A}  "), None),
        (format!("Subject\n\nchange-id:   {ID_A}  \n"), Some(ID_A)),
        // The subject line is never a trailer.
        (format!("Change-Id: {ID_A}\n"), None),
        ("Subject\n\nChange-Id: I0123\n".to_owned(), None),
        (
            "Subject\n\nChange-Id: I0123456789ABCDEF0123456789ABCDEF01234567\n".to_owned(),
            None,
        ),
        (
            "Subject\n\nChange-Id: 0123456789abcdef0123456789abcdef01234567\n".to_owned(),
            None,
        ),
        (format!("Subject\n\nX-Change-Id: {ID_A}\n"), None),
        (String::new(), None),
        ("\n\n".to_owned(), None),
    ];
    for (message, want) in cases {
        assert_eq!(change_id_trailer(message).as_deref(), *want, "{message:?}");
    }
}

#[test]
fn commit_messages_are_read_verbatim() {
    let (_remote, r, base) = pushed();
    let message = format!("Subject\r\n\r\nBody\r\n\r\nChange-Id: {ID_A}");
    let sha = raw_commit(&r, &[&base], 1_700_000_000, &message);
    r.git(&["update-ref", "refs/heads/main", &sha]);
    let local = complete(&open(&r), 200);

    assert_eq!(local.len(), 1);
    let c = &local[0].commit;
    assert_eq!(c.message, message);
    assert_eq!(c.subject(), "Subject");
    assert_eq!(c.change_id.as_deref(), Some(ID_A));
    assert_eq!(c.author.name, "A U Thor");
    assert_eq!(c.author.email, "thor@example.com");
    assert_eq!(
        c.author.time,
        Timestamp::from_second(1_700_000_000).expect("ts")
    );
    assert_eq!(c.author.tz, 120);
    assert_eq!(c.committer.name, "C O Mitter");
    assert_eq!(c.committer.tz, -90);
}

#[test]
fn commit_reads_present_and_missing_commits() {
    let (_remote, r, base) = pushed();
    let message = format!("Subject\n\nBody\n\nChange-Id: {ID_A}");
    let sha = r.commit(&message);
    let repo = open(&r);

    let Local::Available(c) = repo.commit(&rev(&sha)).expect("commit") else {
        panic!("commit is local");
    };
    assert_eq!(c.sha.as_str(), sha);
    assert_eq!(c.parents, [rev(&base)]);
    assert_eq!(c.message, format!("{message}\n"));
    assert_eq!(c.change_id.as_deref(), Some(ID_A));
    assert_eq!(c.author.name, "Test Author");
    assert_eq!(c.author.email, "author@example.com");
    let when: Timestamp = "2013-02-01T08:59:32Z".parse().expect("timestamp");
    assert_eq!(c.author.time, when);
    assert_eq!(c.author.tz, 60);
    assert_eq!(c.committer.name, "Test Committer");
    assert_eq!(c.committer.email, "committer@example.com");
    assert_eq!(c.committer.time, when);

    let Local::Available(root) = repo.commit(&rev(&base)).expect("root") else {
        panic!("root is local");
    };
    assert!(root.parents.is_empty());

    let tree = r.git(&["rev-parse", "HEAD^{tree}"]);
    for missing in [
        "0123456789abcdef0123456789abcdef01234567",
        "HEAD",
        "",
        &tree,
    ] {
        assert_eq!(
            repo.commit(&rev(missing)).expect("missing"),
            Local::NotLocal(NotLocal::MissingCommit(missing.to_owned())),
            "{missing:?}"
        );
    }
}

#[test]
fn author_emails_take_author_email_then_user_email() {
    let r = TestRepo::new();
    let repo = open(&r);
    let emails = |want: &[&str]| assert_eq!(repo.author_emails().expect("emails"), want);
    // The test repository sets both to the same address.
    emails(&[TEST_EMAIL]);
    r.git(&["config", "author.email", " work@example.com "]);
    emails(&["work@example.com", TEST_EMAIL]);
    r.git(&["config", "--unset", "user.email"]);
    emails(&["work@example.com"]);
    r.git(&["config", "--unset", "author.email"]);
    emails(&[]);
    r.git(&["config", "user.email", "me@example.com"]);
    emails(&["me@example.com"]);
    r.git(&["config", "author.email", ""]);
    emails(&["me@example.com"]);
}

#[test]
fn refs_fingerprint_changes_only_with_the_refs() {
    let (_remote, r, _base) = pushed();
    let repo = open(&r);
    let first = repo.refs_fingerprint().expect("fingerprint");
    assert_eq!(repo.refs_fingerprint().expect("again"), first);
    // Work tree and index changes are not refs.
    r.write("file", b"x\n");
    r.git(&["add", "file"]);
    assert_eq!(repo.refs_fingerprint().expect("unchanged"), first);

    r.commit("Next");
    let second = repo.refs_fingerprint().expect("after commit");
    assert_ne!(second, first);
    r.git(&["branch", "other"]);
    let third = repo.refs_fingerprint().expect("after branch");
    assert_ne!(third, second);
    r.git(&["push", "-q", "origin", "main"]);
    assert_ne!(repo.refs_fingerprint().expect("after push"), third);
}

#[test]
fn branch_targets_read_upstreams_and_remote_defaults() {
    let (_remote, r, _base) = pushed();
    r.git(&["push", "-q", "origin", "main:stable"]);
    r.git(&["branch", "feature"]);
    r.git(&["branch", "-q", "--set-upstream-to=origin/stable", "feature"]);
    r.git(&["branch", "plain"]);
    // Tracks a local branch: ignored.
    r.git(&["branch", "-q", "--track", "localup", "main"]);
    let repo = open(&r);

    let up = |b: &str| Upstream {
        remote: "origin".into(),
        branch: b.into(),
    };
    let targets = repo.scan(200).expect("scan").targets;
    assert_eq!(
        targets,
        BranchTargets {
            upstreams: BTreeMap::from([
                ("feature".to_owned(), up("stable")),
                ("main".to_owned(), up("main")),
            ]),
            defaults: BTreeMap::new(),
        }
    );
    assert_eq!(targets.target_for(&["plain"]), None);
    assert_eq!(targets.target_for(&["localup"]), None);
    assert_eq!(targets.target_for(&["plain", "main"]), Some("main"));
    // Several upstreams: the first branch by name decides.
    assert_eq!(targets.target_for(&["main", "feature"]), Some("stable"));

    r.git(&["remote", "set-head", "origin", "main"]);
    let targets = repo.scan(200).expect("scan with HEAD").targets;
    assert_eq!(
        targets.defaults,
        BTreeMap::from([("origin".to_owned(), "main".to_owned())])
    );
    assert_eq!(targets.target_for(&["plain"]), Some("main"));
    assert_eq!(targets.target_for(&["localup"]), Some("main"));
    assert_eq!(targets.target_for(&["feature", "plain"]), Some("stable"));
}

#[test]
fn an_empty_repository_has_nothing() {
    let r = TestRepo::new();
    let repo = open(&r);
    let scan = repo.scan(200).expect("scan");
    assert_eq!(
        scan,
        LocalScan {
            fingerprint: repo.refs_fingerprint().expect("fingerprint"),
            no_remotes: true,
            ..LocalScan::default()
        }
    );
    assert!(!scan.is_complete());
}

#[test]
fn a_repository_without_remote_tracking_branches_is_not_scanned() {
    let r = TestRepo::new();
    r.commit("One");
    r.git(&["branch", "other"]);
    let scan = open(&r).scan(200).expect("scan");
    assert!(scan.no_remotes);
    assert!(scan.commits.is_empty());
    assert!(!scan.is_complete());
}
