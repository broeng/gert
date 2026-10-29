//! View-model helpers for change rows. Pure functions of the model.

use model::{ChangeStatus, ChangeSummary};

/// Small status badges in front of a subject. Nothing is shown for an
/// ordinary active change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chip {
    Merged,
    Abandoned,
    Wip,
    Private,
    MergeConflict,
}

impl Chip {
    pub fn text(self) -> &'static str {
        match self {
            Chip::Merged => "Merged",
            Chip::Abandoned => "Abandoned",
            Chip::Wip => "WIP",
            Chip::Private => "Private",
            Chip::MergeConflict => "Merge conflict",
        }
    }
}

pub fn chips(c: &ChangeSummary) -> Vec<Chip> {
    let mut v = Vec::new();
    match c.status {
        ChangeStatus::Merged => v.push(Chip::Merged),
        ChangeStatus::Abandoned => v.push(Chip::Abandoned),
        ChangeStatus::New | ChangeStatus::Unknown(_) => {}
    }
    if has_merge_conflict(c) {
        v.push(Chip::MergeConflict);
    }
    if c.work_in_progress {
        v.push(Chip::Wip);
    }
    if c.is_private {
        v.push(Chip::Private);
    }
    v
}

/// Whether Gerrit found that the open change does not merge cleanly into
/// its branch. Unknown counts as no.
pub fn has_merge_conflict(c: &ChangeSummary) -> bool {
    c.status == ChangeStatus::New && c.mergeable == Some(false)
}

/// `(+insertions, -deletions)` as shown in the size column.
pub fn size_text(c: &ChangeSummary) -> (String, String) {
    (
        format!("+{}", c.insertions),
        format!("\u{2212}{}", c.deletions),
    )
}

/// The change's page on the web UI: `{base}/c/{project}/+/{number}`.
pub fn browser_url(base: &str, project: &str, number: model::ChangeNumber) -> String {
    format!(
        "{}/c/{}/+/{number}",
        base.trim_end_matches('/'),
        encode_path(project)
    )
}

/// Percent-encodes a project name for a URL path; `/` stays.
fn encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `https://gerrit.example.com/r/` -> `gerrit.example.com/r`.
pub fn server_label(url: &str) -> String {
    url.trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::summary;
    use model::ChangeNumber;

    #[test]
    fn chips_per_state() {
        let mut c = summary(1);
        assert!(chips(&c).is_empty());
        c.status = ChangeStatus::Merged;
        assert_eq!(chips(&c), [Chip::Merged]);
        c.status = ChangeStatus::Abandoned;
        c.work_in_progress = true;
        c.is_private = true;
        assert_eq!(chips(&c), [Chip::Abandoned, Chip::Wip, Chip::Private]);
        c.status = ChangeStatus::Unknown("DRAFT".into());
        assert_eq!(chips(&c), [Chip::Wip, Chip::Private]);
    }

    #[test]
    fn merge_conflict_only_for_open_unmergeable_changes() {
        let mut c = summary(1);
        assert!(!has_merge_conflict(&c), "not tested yet");
        c.mergeable = Some(true);
        assert!(!has_merge_conflict(&c));
        c.mergeable = Some(false);
        assert!(has_merge_conflict(&c));
        assert_eq!(chips(&c), [Chip::MergeConflict]);
        c.status = ChangeStatus::Abandoned;
        assert!(!has_merge_conflict(&c));
    }

    #[test]
    fn size() {
        let c = summary(1);
        assert_eq!(size_text(&c), ("+10".into(), "\u{2212}2".into()));
    }

    #[test]
    fn browser_url_encodes_project() {
        assert_eq!(
            browser_url(
                "https://g.example.com/",
                "platform/foo bar",
                ChangeNumber(42)
            ),
            "https://g.example.com/c/platform/foo%20bar/+/42"
        );
    }

    #[test]
    fn server_labels() {
        assert_eq!(
            server_label("https://gerrit.example.com/"),
            "gerrit.example.com"
        );
        assert_eq!(server_label("http://h/r"), "h/r");
    }
}
