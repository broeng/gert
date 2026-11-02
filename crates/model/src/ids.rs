use std::fmt;

use serde::{Deserialize, Serialize};

/// The per-server numeric id of a change (`_number`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChangeNumber(pub u64);

/// Identifies one Gerrit server inside the cache database. It lives in
/// `model` so that events can name their server; `store` hands them out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ServerId(pub i64);

/// The numeric id of an account (`_account_id`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(pub u64);

/// A patch set number within one change (`_number` of a revision), counting
/// from 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PatchSetNumber(pub u32);

/// A revision's commit SHA. This is what identifies a patch set's content:
/// a patch set number can be reused after the change was deleted and
/// re-imported, the SHA cannot, so caches key on it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RevisionId(pub String);

impl RevisionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first `n` characters, for display (`abc123def456`).
    pub fn short(&self, n: usize) -> &str {
        match self.0.char_indices().nth(n) {
            Some((i, _)) => &self.0[..i],
            None => &self.0,
        }
    }
}

impl From<String> for RevisionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for RevisionId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl ChangeNumber {
    /// The first number of the range reserved for changes that exist only
    /// in a local checkout (`1 << 48`). Gerrit's change numbers are 32-bit
    /// signed integers, so no server ever hands one of these out, and none
    /// of them is ever sent to a server.
    pub const LOCAL_BASE: u64 = 1 << 48;

    /// Whether this is the number of a local change (see
    /// [`ChangeNumber::LOCAL_BASE`]).
    pub fn is_local(self) -> bool {
        self.0 >= Self::LOCAL_BASE
    }
}

impl PatchSetNumber {
    /// The first number of the range reserved for patch sets that exist
    /// only in a local checkout (`1 << 24`), far above any real patch set
    /// count. None of them is ever sent to a server.
    pub const LOCAL_BASE: u32 = 1 << 24;

    /// Whether this is the number of a local patch set (see
    /// [`PatchSetNumber::LOCAL_BASE`]).
    pub fn is_local(self) -> bool {
        self.0 >= Self::LOCAL_BASE
    }
}

impl fmt::Display for PatchSetNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for RevisionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for ChangeNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_serialize_as_plain_numbers() {
        assert_eq!(serde_json::to_string(&ChangeNumber(42)).unwrap(), "42");
        assert_eq!(
            serde_json::from_str::<AccountId>("7").unwrap(),
            AccountId(7)
        );
        assert_eq!(ChangeNumber(42).to_string(), "42");
        assert_eq!(serde_json::to_string(&PatchSetNumber(3)).unwrap(), "3");
        assert_eq!(
            serde_json::to_string(&RevisionId("abc".into())).unwrap(),
            "\"abc\""
        );
    }

    #[test]
    fn local_numbers_start_at_their_bases() {
        assert!(!ChangeNumber(i32::MAX as u64).is_local());
        assert!(!ChangeNumber(ChangeNumber::LOCAL_BASE - 1).is_local());
        assert!(ChangeNumber(ChangeNumber::LOCAL_BASE).is_local());
        assert!(ChangeNumber(ChangeNumber::LOCAL_BASE + 7).is_local());
        assert!(!PatchSetNumber(1).is_local());
        assert!(!PatchSetNumber(PatchSetNumber::LOCAL_BASE - 1).is_local());
        assert!(PatchSetNumber(PatchSetNumber::LOCAL_BASE).is_local());
    }

    #[test]
    fn revision_short_handles_short_and_multibyte_input() {
        let sha = RevisionId::from("0123456789abcdef");
        assert_eq!(sha.short(7), "0123456");
        assert_eq!(sha.short(100), "0123456789abcdef");
        assert_eq!(RevisionId::from("äöü").short(2), "äö");
    }
}
