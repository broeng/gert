use model::{
    ChangedChunk, Diff, DiffChangeType, DiffChunk, FileEntry, FileStatus, RevisionId, Whitespace,
};

use crate::testutil::{TestRepo, git_in};
use crate::{BaseKind, Comparison, Error, Local, NotLocal, Repo};

fn rev(s: &str) -> RevisionId {
    RevisionId(s.to_owned())
}

fn resolve(repo: &Repo, target: &str, base: Option<&str>) -> Local<Comparison> {
    let base = base.map(rev);
    repo.resolve(&rev(target), base.as_ref()).expect("resolve")
}

fn comparison(repo: &Repo, target: &str, base: Option<&str>) -> Comparison {
    resolve(repo, target, base)
        .available()
        .expect("comparison is local")
}

fn files(repo: &Repo, cmp: &Comparison) -> Vec<FileEntry> {
    repo.file_list(cmp)
        .expect("file list")
        .available()
        .expect("local")
}

fn diff(repo: &Repo, cmp: &Comparison, path: &str, old: Option<&str>, ws: &Whitespace) -> Diff {
    repo.diff(cmp, path, old, ws)
        .expect("diff")
        .available()
        .expect("local")
}

fn s(l: &[&str]) -> Vec<String> {
    l.iter().map(|x| (*x).to_owned()).collect()
}

fn common(l: &[&str]) -> DiffChunk {
    DiffChunk::Common { lines: s(l) }
}

fn changed(a: &[&str], b: &[&str]) -> DiffChunk {
    DiffChunk::Changed(ChangedChunk {
        a: s(a),
        b: s(b),
        ..Default::default()
    })
}

/// A repo with two commits: the first adds a, b, c, d and a binary file,
/// the second modifies a, deletes b, renames c, adds e and changes the
/// binary file.
fn two_commits() -> (TestRepo, String, String) {
    let t = TestRepo::new();
    t.write("a.txt", b"one\ntwo\nthree\n");
    t.write("b.txt", b"to be deleted\n");
    t.write(
        "dir/c.txt",
        b"line 1\nline 2\nline 3\nline 4\nline 5\nline 6\n",
    );
    t.write("bin.dat", b"\0\x01\x02");
    let first = t.commit("first\n\nBody.\n");
    t.write("a.txt", b"one\n2\nthree\nfour\n");
    t.remove("b.txt");
    t.git(&["mv", "dir/c.txt", "dir/renamed.txt"]);
    t.write(
        "dir/renamed.txt",
        b"line 1\nline 2\nline 3\nline 4\nline 5\nline six\n",
    );
    t.write("e.txt", b"new\n");
    t.write("bin.dat", b"\0\x01\x02\x03\x04");
    let second = t.commit("second\n\nChange-Id: I1\n");
    (t, first, second)
}

#[test]
fn open_rejects_a_plain_directory() {
    let dir = tempfile::tempdir().expect("dir");
    let err = Repo::open(dir.path()).expect_err("not a repo");
    assert!(matches!(err, Error::NotARepository(_)), "{err}");
    assert!(err.to_string().contains("is not a git repository"));
    let missing = dir.path().join("nope");
    assert!(matches!(Repo::open(missing), Err(Error::NotARepository(_))));
}

#[test]
fn open_accepts_work_trees_bare_repos_and_linked_worktrees() {
    let t = TestRepo::new();
    t.write("x", b"x\n");
    t.commit("init");
    assert!(Repo::open(t.path()).is_ok());

    let bare = tempfile::tempdir().expect("dir");
    git_in(
        t.path(),
        &[
            "clone",
            "-q",
            "--bare",
            ".",
            bare.path().to_str().expect("utf8"),
        ],
    );
    assert!(Repo::open(bare.path()).is_ok());

    let linked = tempfile::tempdir().expect("dir");
    let linked_path = linked.path().join("wt");
    t.git(&["worktree", "add", "-q", linked_path.to_str().expect("utf8")]);
    assert!(Repo::open(&linked_path).is_ok());
}

#[test]
fn has_commit_finds_present_and_absent_commits() {
    let (t, first, _) = two_commits();
    let repo = Repo::open(t.path()).expect("open");
    assert!(repo.has_commit(&rev(&first)).expect("has"));
    assert!(!repo.has_commit(&rev(&"0".repeat(40))).expect("has"));
    assert!(!repo.has_commit(&rev(&"ab".repeat(20))).expect("has"));
    // Not an object id at all.
    assert!(!repo.has_commit(&rev("HEAD")).expect("has"));
    assert!(!repo.has_commit(&rev("--version")).expect("has"));
    // A blob is not a commit.
    let blob = t.git(&["rev-parse", "HEAD:a.txt"]);
    assert!(!repo.has_commit(&rev(&blob)).expect("has"));
}

#[test]
fn file_list_covers_every_status() {
    let (t, _, second) = two_commits();
    let repo = Repo::open(t.path()).expect("open");
    let cmp = comparison(&repo, &second, None);
    assert_eq!(cmp.base_kind(), BaseKind::Parent);
    let list = files(&repo, &cmp);
    let paths: Vec<_> = list.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/COMMIT_MSG",
            "a.txt",
            "b.txt",
            "bin.dat",
            "dir/renamed.txt",
            "e.txt"
        ]
    );
    let by = |p: &str| list.iter().find(|f| f.path == p).expect("entry").clone();

    let msg = by("/COMMIT_MSG");
    assert_eq!(msg.status, FileStatus::Added);
    // Five header lines, a blank line and a two-line message with a blank.
    assert_eq!((msg.lines_inserted, msg.lines_deleted), (9, 0));

    let a = by("a.txt");
    assert_eq!(a.status, FileStatus::Modified);
    assert_eq!((a.lines_inserted, a.lines_deleted), (2, 1));
    assert_eq!(a.size, 17);
    assert_eq!(a.size_delta, 3);

    let b = by("b.txt");
    assert_eq!(b.status, FileStatus::Deleted);
    assert_eq!((b.lines_inserted, b.lines_deleted), (0, 1));
    assert_eq!((b.size, b.size_delta), (0, -14));

    let r = by("dir/renamed.txt");
    assert_eq!(r.status, FileStatus::Renamed);
    assert_eq!(r.old_path.as_deref(), Some("dir/c.txt"));
    assert_eq!((r.lines_inserted, r.lines_deleted), (1, 1));

    let e = by("e.txt");
    assert_eq!(e.status, FileStatus::Added);
    assert_eq!((e.size, e.size_delta), (4, 4));

    let bin = by("bin.dat");
    assert!(bin.binary);
    assert_eq!(bin.status, FileStatus::Modified);
    assert_eq!((bin.size, bin.size_delta), (5, 2));
    assert!(!a.binary);
}

#[test]
fn file_list_of_a_root_commit_is_against_the_empty_tree() {
    let (t, first, _) = two_commits();
    let repo = Repo::open(t.path()).expect("open");
    let cmp = comparison(&repo, &first, None);
    let list = files(&repo, &cmp);
    assert_eq!(list.len(), 5);
    assert!(list.iter().skip(1).all(|f| f.status == FileStatus::Added));
}

#[test]
fn resolver_handles_merges_parents_and_missing_commits() {
    let t = TestRepo::new();
    t.write("f", b"base\n");
    t.commit("base");
    t.git(&["checkout", "-q", "-b", "side"]);
    t.write("g", b"side\n");
    t.commit("side");
    t.git(&["checkout", "-q", "main"]);
    t.write("h", b"main\n");
    let main = t.commit("main");
    t.git(&["merge", "-q", "--no-ff", "-m", "merge", "side"]);
    let merge = t.git(&["rev-parse", "HEAD"]);
    // A rewritten version of `main` with the same parent.
    t.git(&["checkout", "-q", "-b", "amend", &main]);
    t.write("h", b"main amended\n");
    t.git(&["commit", "-q", "-a", "--amend", "-m", "main v2"]);
    let amended = t.git(&["rev-parse", "HEAD"]);

    let repo = Repo::open(t.path()).expect("open");

    assert_eq!(
        resolve(&repo, &merge, None),
        Local::NotLocal(NotLocal::MergeCommit)
    );
    assert_eq!(
        resolve(&repo, &merge, Some(&main)),
        Local::NotLocal(NotLocal::ParentsDiffer)
    );
    let cmp = comparison(&repo, &amended, Some(&main));
    assert_eq!(cmp.base_kind(), BaseKind::PatchSet);
    let list = files(&repo, &cmp);
    assert_eq!(
        list.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
        ["/COMMIT_MSG", "h"]
    );
    // The subject line changed: one line replaced.
    assert_eq!(list[0].status, FileStatus::Modified);
    assert_eq!((list[0].lines_inserted, list[0].lines_deleted), (1, 1));

    let missing = "1".repeat(40);
    assert_eq!(
        resolve(&repo, &missing, None),
        Local::NotLocal(NotLocal::MissingCommit(missing.clone()))
    );
    assert_eq!(
        resolve(&repo, &main, Some(&missing)),
        Local::NotLocal(NotLocal::MissingCommit(missing))
    );
}

#[test]
fn a_missing_parent_in_a_shallow_checkout_is_not_local() {
    let t = TestRepo::new();
    t.write("f", b"1\n");
    t.commit("one");
    t.write("f", b"2\n");
    let two = t.commit("two");
    let clone = tempfile::tempdir().expect("dir");
    let src = format!("file://{}", t.path().display());
    git_in(clone.path(), &["clone", "-q", "--depth", "1", &src, "."]);
    let repo = Repo::open(clone.path()).expect("open");
    assert!(matches!(
        resolve(&repo, &two, None),
        Local::NotLocal(NotLocal::MissingCommit(_))
    ));
}

#[test]
fn diff_of_a_modified_file_alternates_common_and_changed() {
    let (t, _, second) = two_commits();
    let repo = Repo::open(t.path()).expect("open");
    let cmp = comparison(&repo, &second, None);
    let d = diff(&repo, &cmp, "a.txt", None, &Whitespace::IgnoreNone);
    assert_eq!(d.change_type, Some(DiffChangeType::Modified));
    assert!(!d.binary);
    assert_eq!(
        d.chunks,
        [
            common(&["one"]),
            changed(&["two"], &["2"]),
            common(&["three"]),
            changed(&[], &["four"]),
        ]
    );
    let (ma, mb) = (d.meta_a.clone().expect("a"), d.meta_b.clone().expect("b"));
    assert_eq!((ma.name.as_str(), ma.lines), ("a.txt", 3));
    assert_eq!((mb.name.as_str(), mb.lines), ("a.txt", 4));
    assert_eq!(d.a_len(), 3);
    assert_eq!(d.b_len(), 4);
    assert!(d.intraline_status.is_none());
    assert_eq!(d.diff_header[0], "diff --git a/a.txt b/a.txt");
}

#[test]
fn diff_of_added_deleted_and_renamed_files() {
    let (t, _, second) = two_commits();
    let repo = Repo::open(t.path()).expect("open");
    let cmp = comparison(&repo, &second, None);

    let added = diff(&repo, &cmp, "e.txt", None, &Whitespace::IgnoreNone);
    assert_eq!(added.change_type, Some(DiffChangeType::Added));
    assert!(added.meta_a.is_none());
    assert_eq!(added.meta_b.expect("b").lines, 1);
    assert_eq!(added.chunks, [changed(&[], &["new"])]);

    let deleted = diff(&repo, &cmp, "b.txt", None, &Whitespace::IgnoreNone);
    assert_eq!(deleted.change_type, Some(DiffChangeType::Deleted));
    assert!(deleted.meta_b.is_none());
    assert_eq!(deleted.chunks, [changed(&["to be deleted"], &[])]);

    let renamed = diff(
        &repo,
        &cmp,
        "dir/renamed.txt",
        Some("dir/c.txt"),
        &Whitespace::IgnoreNone,
    );
    assert_eq!(renamed.change_type, Some(DiffChangeType::Renamed));
    assert_eq!(renamed.meta_a.expect("a").name, "dir/c.txt");
    assert_eq!(renamed.meta_b.expect("b").name, "dir/renamed.txt");
    assert_eq!(
        renamed.chunks,
        [
            common(&["line 1", "line 2", "line 3", "line 4", "line 5"]),
            changed(&["line 6"], &["line six"]),
        ]
    );
    assert!(
        renamed
            .diff_header
            .contains(&"rename from dir/c.txt".to_owned())
    );
}

#[test]
fn binary_files_have_no_chunks() {
    let (t, _, second) = two_commits();
    let repo = Repo::open(t.path()).expect("open");
    let cmp = comparison(&repo, &second, None);
    let d = diff(&repo, &cmp, "bin.dat", None, &Whitespace::IgnoreNone);
    assert!(d.binary);
    assert!(d.chunks.is_empty());
    assert_eq!(d.change_type, Some(DiffChangeType::Modified));
}

#[test]
fn whitespace_modes_compare_normalized_lines() {
    let t = TestRepo::new();
    t.write(
        "w.txt",
        b"keep\nfoo bar  \n  indented\nspace  between\nreal\n",
    );
    t.commit("one");
    t.write(
        "w.txt",
        b"keep\nfoo bar\nindented\nspace between\nchanged\n",
    );
    let second = t.commit("two");
    let repo = Repo::open(t.path()).expect("open");
    let cmp = comparison(&repo, &second, None);
    let run = |ws: Whitespace| diff(&repo, &cmp, "w.txt", None, &ws).chunks;

    assert_eq!(
        run(Whitespace::IgnoreNone),
        [
            common(&["keep"]),
            changed(
                &["foo bar  ", "  indented", "space  between", "real"],
                &["foo bar", "indented", "space between", "changed"]
            ),
        ]
    );
    assert_eq!(
        run(Whitespace::IgnoreTrailing),
        [
            common(&["keep", "foo bar"]),
            changed(
                &["  indented", "space  between", "real"],
                &["indented", "space between", "changed"]
            ),
        ]
    );
    assert_eq!(
        run(Whitespace::IgnoreLeadingAndTrailing),
        [
            common(&["keep", "foo bar", "indented"]),
            changed(&["space  between", "real"], &["space between", "changed"]),
        ]
    );
    // Common lines carry the new side's text.
    assert_eq!(
        run(Whitespace::IgnoreAll),
        [
            common(&["keep", "foo bar", "indented", "space between"]),
            changed(&["real"], &["changed"]),
        ]
    );
}

#[test]
fn line_endings_and_a_missing_final_newline() {
    let t = TestRepo::new();
    t.write("n.txt", b"a\r\nb\nc");
    t.commit("one");
    t.write("n.txt", b"a\r\nb\nc\n");
    let second = t.commit("two");
    t.write("empty.txt", b"");
    t.write("n.txt", b"");
    let third = t.commit("three");
    let repo = Repo::open(t.path()).expect("open");

    // Same lines either way: the newline at the end is not a line.
    let cmp = comparison(&repo, &second, None);
    let d = diff(&repo, &cmp, "n.txt", None, &Whitespace::IgnoreNone);
    // `\r` stays on its line; the last line is unchanged text-wise.
    assert_eq!(d.chunks, [common(&["a\r", "b", "c"])]);
    assert_eq!(d.meta_a.expect("a").lines, 3);

    let cmp = comparison(&repo, &third, None);
    let d = diff(&repo, &cmp, "n.txt", None, &Whitespace::IgnoreNone);
    assert_eq!(d.meta_b.expect("b").lines, 0);
    assert_eq!(d.chunks, [changed(&["a\r", "b", "c"], &[])]);
    let d = diff(&repo, &cmp, "empty.txt", None, &Whitespace::IgnoreNone);
    assert!(d.chunks.is_empty());
}

#[test]
fn diff_of_a_root_commit_uses_the_empty_tree() {
    let (t, first, _) = two_commits();
    let repo = Repo::open(t.path()).expect("open");
    let cmp = comparison(&repo, &first, None);
    let d = diff(&repo, &cmp, "a.txt", None, &Whitespace::IgnoreNone);
    assert_eq!(d.change_type, Some(DiffChangeType::Added));
    assert_eq!(d.chunks, [changed(&[], &["one", "two", "three"])]);
}

#[test]
fn diffs_that_gerrit_synthesizes_or_we_cannot_name_are_not_local() {
    let (t, _, second) = two_commits();
    let repo = Repo::open(t.path()).expect("open");
    let cmp = comparison(&repo, &second, None);
    let none = Whitespace::IgnoreNone;
    for path in ["/COMMIT_MSG", "/MERGE_LIST"] {
        assert!(matches!(
            repo.diff(&cmp, path, None, &none).expect("diff"),
            Local::NotLocal(NotLocal::SpecialFile(_))
        ));
    }
    assert_eq!(
        repo.diff(&cmp, "a.txt", None, &Whitespace::Unknown("X".into()))
            .expect("diff"),
        Local::NotLocal(NotLocal::UnknownWhitespace)
    );
    assert!(matches!(
        repo.diff(&cmp, "a\nb", None, &none).expect("diff"),
        Local::NotLocal(NotLocal::UnsupportedPath(_))
    ));
    // A directory is not a file.
    assert!(matches!(
        repo.diff(&cmp, "dir", None, &none).expect("diff"),
        Local::NotLocal(NotLocal::NotAFile(_))
    ));
    // Absent on both sides.
    assert!(matches!(
        repo.diff(&cmp, "nope.txt", None, &none).expect("diff"),
        Local::NotLocal(NotLocal::NotAFile(_))
    ));
}
