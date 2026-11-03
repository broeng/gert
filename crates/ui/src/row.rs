//! View-model helpers for change rows. Pure functions of the model.

use model::{ChangeNumber, ChangeStatus, ChangeSummary, LocalChange, LocalChangeState};

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
/// `None` for a local change, which Gerrit does not have (its number is
/// synthetic and must never reach the server).
pub fn browser_url(base: &str, project: &str, number: ChangeNumber) -> Option<String> {
    (!number.is_local()).then(|| {
        format!(
            "{}/c/{}/+/{number}",
            base.trim_end_matches('/'),
            encode_path(project)
        )
    })
}

/// What the row of a local change says about where it stands; `None` for
/// one that is simply local.
pub fn local_state_note(state: LocalChangeState) -> Option<&'static str> {
    match state {
        LocalChangeState::Local | LocalChangeState::Consolidated => None,
        LocalChangeState::Checking => Some("checking with Gerrit\u{2026}"),
        LocalChangeState::Vanished => {
            Some("gone from the checkout; kept until Gerrit has been asked")
        }
        LocalChangeState::Ambiguous => Some("several Gerrit changes match"),
        LocalChangeState::Closed => Some("closed on Gerrit; review held"),
    }
}

/// What the dashboard row of local change `c` says about it: its state
/// (see [`local_state_note`]), or, for a row with a Gerrit change (listed
/// for its held review), that its review is held.
pub fn local_row_note(c: &LocalChange) -> Option<&'static str> {
    match (c.gerrit_number, c.state) {
        (Some(_), LocalChangeState::Closed) | (None, _) => local_state_note(c.state),
        (Some(_), _) => Some("review held"),
    }
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
            Some("https://g.example.com/c/platform/foo%20bar/+/42".to_owned())
        );
    }

    #[test]
    fn local_changes_have_no_browser_url() {
        let local = ChangeNumber::local(3).expect("local number");
        assert_eq!(browser_url("https://g.example.com", "p", local), None);
    }

    #[test]
    fn every_local_state_but_the_plain_one_has_a_note() {
        use LocalChangeState as S;
        assert_eq!(local_state_note(S::Local), None);
        assert_eq!(
            local_state_note(S::Checking),
            Some("checking with Gerrit\u{2026}")
        );
        assert_eq!(
            local_state_note(S::Vanished),
            Some("gone from the checkout; kept until Gerrit has been asked")
        );
        assert_eq!(
            local_state_note(S::Ambiguous),
            Some("several Gerrit changes match")
        );
        assert_eq!(
            local_state_note(S::Closed),
            Some("closed on Gerrit; review held")
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
