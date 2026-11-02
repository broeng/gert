//! What happens to votes given on a local change when what reaches Gerrit
//! is not the commit they were given on.

use serde::{Deserialize, Serialize};

/// How votes on a change that exists only locally are treated when the
/// commit was amended after the vote and before the upload, so that the
/// patch set on Gerrit is not the commit voted on (`local_approval` of a
/// `[[servers.projects]]` entry). A vote on the exact commit that is
/// uploaded is cast in every mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocalApproval {
    /// Cast the votes on what is uploaded.
    Follow,
    /// Only on the exact commit; otherwise hold an approval for the user
    /// to publish or drop.
    ExactOrManual,
    /// Only on the exact commit; otherwise an approval can only be dropped.
    #[default]
    Exact,
}

impl LocalApproval {
    /// Every mode, from the most to the least permissive.
    pub const ALL: [Self; 3] = [Self::Follow, Self::ExactOrManual, Self::Exact];

    /// The spelling in the config file.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Follow => "follow",
            Self::ExactOrManual => "exact-or-manual",
            Self::Exact => "exact",
        }
    }

    /// The mode spelled `s` ([`LocalApproval::as_str`]), ignoring case and
    /// surrounding blanks; `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        Self::ALL
            .into_iter()
            .find(|m| m.as_str().eq_ignore_ascii_case(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spellings_round_trip_and_unknown_ones_are_none() {
        for m in LocalApproval::ALL {
            assert_eq!(LocalApproval::parse(m.as_str()), Some(m));
            assert_eq!(
                serde_json::to_string(&m).unwrap(),
                format!("\"{}\"", m.as_str())
            );
        }
        assert_eq!(
            LocalApproval::parse(" Exact-Or-Manual "),
            Some(LocalApproval::ExactOrManual)
        );
        assert_eq!(LocalApproval::parse("strict"), None);
        assert_eq!(LocalApproval::parse(""), None);
        assert_eq!(LocalApproval::default(), LocalApproval::Exact);
    }
}
