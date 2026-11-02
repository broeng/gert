use model::{CommentRange, DiffChunk, LineMap, RevisionId, Whitespace};

use crate::testutil::TestRepo;
use crate::{Local, NotLocal, Repo};

fn rev(s: &str) -> RevisionId {
    RevisionId(s.to_owned())
}

fn map(repo: &Repo, from: &str, to: &str, path: &str) -> LineMap {
    repo.line_map(&rev(from), &rev(to), path)
        .expect("line map")
        .available()
        .expect("local")
}

/// Two commits of `f.txt`: `old`, then `new`. Returns the map from the
/// first to the second.
fn two_versions(old: &[u8], new: &[u8]) -> LineMap {
    let t = TestRepo::new();
    t.write("f.txt", old);
    let first = t.commit("one");
    t.write("f.txt", new);
    let second = t.commit("two");
    let repo = Repo::open(t.path()).expect("open");
    map(&repo, &first, &second, "f.txt")
}

/// `map_line` for every line of a file with `n` lines, and one past it.
fn lines(m: &LineMap, n: u32) -> Vec<Option<u32>> {
    (1..=n + 1).map(|l| m.map_line(l)).collect()
}

fn range(start_line: u32, end_line: u32) -> CommentRange {
    CommentRange {
        start_line,
        start_character: 1,
        end_line,
        end_character: 3,
    }
}

#[test]
fn an_unchanged_file_is_an_identity_map() {
    let t = TestRepo::new();
    t.write("f.txt", b"a\nb\n");
    t.write("other.txt", b"1\n");
    let first = t.commit("one");
    t.write("other.txt", b"2\n");
    let second = t.commit("two");
    let repo = Repo::open(t.path()).expect("open");
    let m = map(&repo, &first, &second, "f.txt");
    assert!(m.is_identity());
    assert_eq!(m.target_path(), Some("f.txt"));
    // Three lines as Gerrit counts them: the final newline adds one.
    assert_eq!(lines(&m, 3), [Some(1), Some(2), Some(3), None]);
    // The same commit on both sides, too.
    assert!(map(&repo, &first, &first, "f.txt").is_identity());
}

#[test]
fn insertions_above_and_below_shift_only_the_lines_after_them() {
    let m = two_versions(b"a\nb\nc\n", b"new 1\nnew 2\na\nb\nnew 3\nc\nnew 4\n");
    assert!(!m.is_identity());
    // a, b, c and the empty last line.
    assert_eq!(lines(&m, 4), [Some(3), Some(4), Some(6), Some(8), None]);
}

#[test]
fn deleted_lines_map_to_nothing() {
    let m = two_versions(b"a\nb\nc\nd\n", b"a\nd\n");
    assert_eq!(lines(&m, 5), [Some(1), None, None, Some(2), Some(3), None]);
}

#[test]
fn changed_lines_map_to_nothing() {
    let m = two_versions(b"a\nb\nc\n", b"a\nB\nc\n");
    assert_eq!(lines(&m, 4), [Some(1), None, Some(3), Some(4), None]);
}

#[test]
fn a_whitespace_only_change_counts_as_changed() {
    let m = two_versions(b"fn x() {\n  body\n}\n", b"fn x() {\n    body\n}\n");
    assert_eq!(lines(&m, 4), [Some(1), None, Some(3), Some(4), None]);
    let m = two_versions(b"trailing\nkeep\n", b"trailing  \nkeep\n");
    assert_eq!(m.map_line(1), None);
    assert_eq!(m.map_line(2), Some(2));
}

#[test]
fn ranges_inside_unchanged_lines_move_and_ranges_across_a_change_do_not() {
    // Line 3 changed, a line inserted after line 5.
    let m = two_versions(
        b"1\n2\n3\n4\n5\n6\n7\n",
        b"0\n1\n2\nthree\n4\n5\nnew\n6\n7\n",
    );
    assert_eq!(m.map_range(range(1, 2)), Some(range(2, 3)));
    assert_eq!(m.map_range(range(6, 7)), Some(range(8, 9)));
    assert_eq!(m.map_range(range(4, 4)), Some(range(5, 5)));
    // Over the changed line.
    assert_eq!(m.map_range(range(2, 4)), None);
    assert_eq!(m.map_range(range(3, 3)), None);
    // Both ends unchanged, but a line was inserted between them.
    assert_eq!(m.map_range(range(5, 6)), None);
}

#[test]
fn a_rename_with_edits_is_followed() {
    let t = TestRepo::new();
    let body: String = (1..=10).map(|i| format!("line {i}\n")).collect();
    t.write("old/name.rs", body.as_bytes());
    let first = t.commit("one");
    t.remove("old/name.rs");
    t.write(
        "new/name.rs",
        body.replace("line 4\n", "line four\n").as_bytes(),
    );
    let second = t.commit("two");
    let repo = Repo::open(t.path()).expect("open");
    let m = map(&repo, &first, &second, "old/name.rs");
    assert_eq!(m.target_path(), Some("new/name.rs"));
    assert_eq!(m.map_line(3), Some(3));
    assert_eq!(m.map_line(4), None);
    assert_eq!(m.map_line(10), Some(10));

    // A pure rename is an identity map under the new name.
    t.git(&["mv", "new/name.rs", "renamed.rs"]);
    let third = t.commit("three");
    let m = map(&repo, &second, &third, "new/name.rs");
    assert!(m.is_identity());
    assert_eq!(m.target_path(), Some("renamed.rs"));
}

#[test]
fn a_file_deleted_in_to_is_gone() {
    let t = TestRepo::new();
    t.write("doomed.txt", b"a\n");
    t.write("keep.txt", b"k\n");
    let first = t.commit("one");
    t.remove("doomed.txt");
    let second = t.commit("two");
    let repo = Repo::open(t.path()).expect("open");
    let m = map(&repo, &first, &second, "doomed.txt");
    assert!(m.is_file_gone());
    assert_eq!(m.target_path(), None);
    assert_eq!(m.map_line(1), None);
    // Not in `from` at all: nothing to map.
    assert_eq!(
        repo.line_map(&rev(&first), &rev(&second), "never.txt")
            .expect("line map"),
        Local::NotLocal(NotLocal::NotAFile("never.txt".into()))
    );
}

#[test]
fn a_file_added_in_to_is_not_in_from() {
    let t = TestRepo::new();
    t.write("keep.txt", b"k\n");
    let first = t.commit("one");
    t.write("added.txt", b"a\n");
    let second = t.commit("two");
    let repo = Repo::open(t.path()).expect("open");
    assert_eq!(
        repo.line_map(&rev(&first), &rev(&second), "added.txt")
            .expect("line map"),
        Local::NotLocal(NotLocal::NotAFile("added.txt".into()))
    );
}

#[test]
fn crlf_lines_keep_their_cr_and_compare_exactly() {
    // `\r` stays on its line, so a line that only lost its `\r` changed.
    let m = two_versions(b"a\r\nb\r\nc\r\n", b"x\r\na\r\nb\nc\r\n");
    assert_eq!(lines(&m, 4), [Some(2), None, Some(4), Some(5), None]);
}

#[test]
fn the_empty_last_line_maps_only_when_both_sides_end_in_a_newline() {
    // A final newline added: the last line changes (Gerrit shows `c` and
    // `c`, ``), and there was no empty last line before.
    let m = two_versions(b"a\nb\nc", b"a\nb\nc\n");
    assert_eq!(lines(&m, 3), [Some(1), Some(2), None, None]);
    // Removed: the last line and the empty one after it changed.
    let m = two_versions(b"a\nb\n", b"a\nb");
    assert_eq!(lines(&m, 3), [Some(1), None, None, None]);
    // Kept: the empty line maps to the new empty last line.
    let m = two_versions(b"a\nb\n", b"z\na\nb\n");
    assert_eq!(lines(&m, 3), [Some(2), Some(3), Some(4), None]);
}

#[test]
fn a_changed_binary_file_has_no_line_maps() {
    let t = TestRepo::new();
    t.write("logo.png", b"\x89PNG\0\x01\n\x02");
    t.write("same.bin", b"\0same");
    let first = t.commit("one");
    t.write("logo.png", b"\x89PNG\0\x03\n\x04");
    let second = t.commit("two");
    let repo = Repo::open(t.path()).expect("open");
    let m = map(&repo, &first, &second, "logo.png");
    assert_eq!(m, LineMap::opaque("logo.png"));
    assert_eq!(m.map_line(1), None);
    // An unchanged one is still the same file.
    assert!(map(&repo, &first, &second, "same.bin").is_identity());
}

#[test]
fn a_missing_commit_or_a_special_path_is_not_local() {
    let t = TestRepo::new();
    t.write("f.txt", b"a\n");
    let first = t.commit("one");
    let repo = Repo::open(t.path()).expect("open");
    let missing = "1".repeat(40);
    for (from, to) in [(&missing, &first), (&first, &missing)] {
        assert_eq!(
            repo.line_map(&rev(from), &rev(to), "f.txt")
                .expect("line map"),
            Local::NotLocal(NotLocal::MissingCommit(missing.clone()))
        );
    }
    assert!(matches!(
        repo.line_map(&rev(&first), &rev(&first), "/MERGE_LIST")
            .expect("line map"),
        Local::NotLocal(NotLocal::SpecialFile(_))
    ));
    assert!(matches!(
        repo.line_map(&rev(&first), &rev(&first), "a\nb")
            .expect("line map"),
        Local::NotLocal(NotLocal::UnsupportedPath(_))
    ));
}

#[test]
fn commit_message_lines_map_through_the_synthetic_text() {
    let t = TestRepo::new();
    t.write("f.txt", b"a\n");
    t.commit("base");
    t.write("f.txt", b"b\n");
    let first = t.commit("Subject\n\nFirst paragraph.\n\nChange-Id: I1\n");
    t.git(&[
        "commit",
        "-q",
        "--amend",
        "-m",
        "Subject\n\nAn inserted paragraph.\n\nFirst paragraph.\n\nChange-Id: I1",
    ]);
    let second = t.git(&["rev-parse", "HEAD"]);
    let repo = Repo::open(t.path()).expect("open");
    let m = map(&repo, &first, &second, "/COMMIT_MSG");
    assert_eq!(m.target_path(), Some("/COMMIT_MSG"));
    // Header (5 lines; the test repo's dates do not change), blank line,
    // subject, blank, `First paragraph.`, blank, Change-Id, empty last line.
    assert_eq!(
        lines(&m, 12),
        [
            Some(1),
            Some(2),
            Some(3),
            Some(4),
            Some(5),
            Some(6),
            Some(7),
            Some(8),
            Some(11),
            Some(12),
            Some(13),
            Some(14),
            None
        ]
    );
    // Unchanged commit message: identity.
    assert!(map(&repo, &first, &first, "/COMMIT_MSG").is_identity());
}

/// The map agrees with the diff Gerrit would show: every line of a
/// `Common` chunk maps to its counterpart, every line of a `Changed` chunk
/// outside the boring lines maps to nothing.
#[test]
fn the_map_agrees_with_the_diff() {
    let t = TestRepo::new();
    let old = "use a;\n\nfn one() {\n    1\n}\n\nfn two() {\n    2\n}\n\nfn three() {\n    3\n}\n";
    let new = "use a;\nuse b;\n\nfn one() {\n    1\n}\n\nfn three() {\n    30\n}\n\nfn four() {}\n";
    t.write("f.rs", old.as_bytes());
    let first = t.commit("one");
    t.write("f.rs", new.as_bytes());
    let second = t.commit("two");
    let repo = Repo::open(t.path()).expect("open");
    let m = map(&repo, &first, &second, "f.rs");
    let cmp = repo
        .resolve(&rev(&second), None)
        .expect("resolve")
        .available()
        .expect("local");
    let d = repo
        .diff(&cmp, "f.rs", None, &Whitespace::IgnoreNone)
        .expect("diff")
        .available()
        .expect("local");
    let (mut la, mut lb) = (1_u32, 1_u32);
    for chunk in &d.chunks {
        match chunk {
            DiffChunk::Common { lines } => {
                for _ in lines {
                    assert_eq!(m.map_line(la), Some(lb), "line {la}");
                    la += 1;
                    lb += 1;
                }
            }
            DiffChunk::Changed(c) => {
                for (i, text) in c.a.iter().enumerate() {
                    let mapped = m.map_line(la + i as u32);
                    // A boring line joining two regions may be unchanged.
                    if let Some(to) = mapped {
                        assert_eq!(c.b.get((to - lb) as usize), Some(text));
                    }
                }
                la += c.a.len() as u32;
                lb += c.b.len() as u32;
            }
            DiffChunk::Skip { .. } => panic!("no skip without a whitespace option"),
        }
    }
    assert_eq!(la - 1, d.a_len());
}
