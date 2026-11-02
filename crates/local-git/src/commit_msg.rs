//! Gerrit's synthetic `/COMMIT_MSG` file.

use model::{CommitInfo, Signature};

/// The text of Gerrit's `/COMMIT_MSG` file for `c` (as Gerrit's
/// `MagicFile.forCommitMessage` builds it; checked against 3.14 by
/// `tests/dev_gerrit.rs`): a header, a blank line and the message as
/// stored.
///
/// The header names the parent (its first 8 hex digits and its subject;
/// nothing for a root commit, a `Merge Of:` line per parent for a merge),
/// then author and committer, each with its date in the commit's own time
/// zone. `parent_subjects` has one subject per parent of `c`, as
/// [`CommitInfo::subject`] gives it; a missing one prints as empty.
///
/// A message without a final newline gets one, as in the texts Gerrit
/// shows (verified on 3.14 with a commit made without one).
pub(crate) fn text(c: &CommitInfo, parent_subjects: &[String]) -> String {
    let mut s = String::new();
    let merge = c.parents.len() > 1;
    for (i, parent) in c.parents.iter().enumerate() {
        let label = match (merge, i) {
            (false, _) => "Parent:     ",
            (true, 0) => "Merge Of:   ",
            (true, _) => "            ",
        };
        // Gerrit abbreviates to the shortest unique prefix of at least 8
        // digits (`abbreviate(p, 8)`), so an ambiguous one is longer
        // there. We always print 8.
        let sha = parent.as_str();
        let subject = parent_subjects.get(i).map_or("", String::as_str);
        s.push_str(&format!(
            "{label}{} ({subject})\n",
            sha.get(..8).unwrap_or(sha)
        ));
    }
    person(&mut s, "Author", &c.author);
    person(&mut s, "Commit", &c.committer);
    s.push('\n');
    s.push_str(&c.message);
    if !c.message.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// `Author:     Name <email>` and `AuthorDate: 2013-02-01 09:59:32 +0100`
/// (Java's `yyyy-MM-dd HH:mm:ss ZZZ` in the signature's own zone).
fn person(s: &mut String, field: &str, p: &Signature) {
    s.push_str(&format!("{field}:     {} <{}>\n", p.name, p.email));
    s.push_str(&format!("{field}Date: {}\n", date(p)));
}

fn date(p: &Signature) -> String {
    let local = p.time.as_second() + i64::from(p.tz) * 60;
    let days = local.div_euclid(86_400);
    let rem = local.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let sign = if p.tz < 0 { '-' } else { '+' };
    let tz = p.tz.unsigned_abs();
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} {sign}{:02}{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        tz / 60,
        tz % 60
    )
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use model::{RevisionId, Timestamp};

    use super::*;

    fn sig(name: &str, secs: i64, tz: i32) -> Signature {
        Signature {
            name: name.into(),
            email: format!("{}@example.com", name.to_lowercase()),
            time: Timestamp::from_second(secs).expect("time"),
            tz,
        }
    }

    fn commit(parents: &[&str], message: &str) -> CommitInfo {
        CommitInfo {
            sha: RevisionId("c".repeat(40)),
            parents: parents.iter().map(|p| RevisionId((*p).into())).collect(),
            author: sig("Alice", 1_700_000_000, 0),
            committer: sig("Bob", 1_700_000_000, 765),
            message: message.into(),
            change_id: None,
        }
    }

    // The expected texts are what Gerrit 3.14 showed for the same commits.
    #[test]
    fn header_dates_use_each_signature_s_own_zone() {
        let mut c = commit(&[&"a".repeat(40)], "Subject\n\nChange-Id: I1");
        c.author = sig("Alice", 1_774_728_000, 330);
        c.committer = sig("Bob", 1_774_780_200, -420);
        assert_eq!(
            text(&c, &["Parent subject that wraps".into()]),
            "Parent:     aaaaaaaa (Parent subject that wraps)\n\
             Author:     Alice <alice@example.com>\n\
             AuthorDate: 2026-03-29 01:30:00 +0530\n\
             Commit:     Bob <bob@example.com>\n\
             CommitDate: 2026-03-29 03:30:00 -0700\n\
             \n\
             Subject\n\nChange-Id: I1\n"
        );
    }

    #[test]
    fn a_zero_offset_prints_as_plus_and_a_missing_newline_is_added() {
        let c = commit(&[&"a".repeat(40)], "No newline");
        let t = text(&c, &[]);
        assert!(t.contains("AuthorDate: 2023-11-14 22:13:20 +0000\n"), "{t}");
        assert!(t.contains("CommitDate: 2023-11-15 10:58:20 +1245\n"), "{t}");
        assert!(t.starts_with("Parent:     aaaaaaaa ()\n"), "{t}");
        assert!(t.ends_with("\n\nNo newline\n"), "{t}");
    }

    #[test]
    fn roots_have_no_parent_line_and_merges_list_every_parent() {
        let root = commit(&[], "Root\n");
        assert!(text(&root, &[]).starts_with("Author:     Alice"));
        let merge = commit(&[&"a".repeat(40), &"b".repeat(40)], "Merge\n");
        assert!(text(&merge, &["Side".into(), "Main".into()]).starts_with(
            "Merge Of:   aaaaaaaa (Side)\n            bbbbbbbb (Main)\nAuthor:     Alice"
        ));
    }
}
