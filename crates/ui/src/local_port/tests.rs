use std::{path::Path, sync::Arc};

use local_git::testutil::TestRepo;
use model::{LocalApproval, RevisionId};
use sync::{NotLocalPath, PathMap, PortError, PortSource};

use super::*;

fn checkout(project: &str, path: &Path, state: CheckoutState) -> Checkout {
    Checkout {
        project: project.into(),
        path: path.to_owned(),
        state,
        local_changes: true,
        local_approval: LocalApproval::Exact,
    }
}

/// A base commit, then `amended` (line 2 changed, b.txt deleted) and
/// `pushed` (a line inserted on top), both on the base.
struct Fx {
    repo: TestRepo,
    base: RevisionId,
    amended: RevisionId,
    pushed: RevisionId,
}

fn fx() -> Fx {
    let repo = TestRepo::new();
    repo.write("a.txt", b"one\ntwo\nthree\n");
    repo.write("b.txt", b"bee\n");
    let base: RevisionId = repo.commit("base").into();
    repo.write("a.txt", b"one\nTWO\nthree\n");
    repo.remove("b.txt");
    let amended: RevisionId = repo.commit("amended").into();
    repo.git(&["reset", "-q", "--hard", base.as_str()]);
    repo.write("a.txt", b"zero\none\ntwo\nthree\n");
    let pushed: RevisionId = repo.commit("pushed").into();
    Fx {
        repo,
        base,
        amended,
        pushed,
    }
}

#[test]
fn a_checkout_answers_commits_and_line_maps() {
    let f = fx();
    // Not opened yet: opened on first use; and opened already.
    let opened = Repo::open(f.repo.path()).expect("open");
    for state in [CheckoutState::Opening, CheckoutState::Open(opened)] {
        let ports = CheckoutPorts::new(vec![checkout("p", f.repo.path(), state)]);
        let c = ports
            .commit("p", &f.pushed)
            .expect("commit")
            .expect("there");
        assert_eq!(c.parent.as_ref(), Some(&f.base));
        assert_eq!(
            ports
                .commit("p", &RevisionId("0".repeat(40)))
                .expect("asked"),
            None
        );

        let paths = [
            "a.txt".to_owned(),
            "b.txt".to_owned(),
            "/COMMIT_MSG".to_owned(),
        ];
        let maps = ports
            .line_maps("p", &f.amended, &f.pushed, &paths)
            .expect("maps");
        let PathMap::Lines(a) = &maps["a.txt"] else {
            panic!("{maps:?}");
        };
        // Line 1 moved down by one; line 2 changed.
        assert_eq!((a.map_line(1), a.map_line(2)), (Some(2), None));
        // Deleted by the amended version.
        assert_eq!(maps["b.txt"], PathMap::NotLocal(NotLocalPath::NotAFile));
        assert!(matches!(maps["/COMMIT_MSG"], PathMap::Lines(_)));

        // A commit the checkout does not have.
        let gone = RevisionId("1".repeat(40));
        assert_eq!(
            ports.line_maps("p", &gone, &f.pushed, &paths),
            Err(PortError::MissingCommit(gone.clone()))
        );
    }
}

#[test]
fn without_a_usable_checkout_there_is_none() {
    let f = fx();
    let missing = f.repo.path().join("nowhere");
    let failed = Arc::new(local_git::Error::NotARepository(missing.clone()));
    let ports = CheckoutPorts::new(vec![
        checkout("broken", &missing, CheckoutState::Opening),
        checkout("failed", f.repo.path(), CheckoutState::Failed(failed)),
    ]);
    for project in ["broken", "failed", "unknown"] {
        assert_eq!(ports.commit(project, &f.base), Err(PortError::NoCheckout));
        assert_eq!(
            ports.line_maps(project, &f.base, &f.pushed, &["a.txt".to_owned()]),
            Err(PortError::NoCheckout)
        );
    }
}
