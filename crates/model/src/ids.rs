use std::fmt;

use serde::{Deserialize, Serialize};

/// The per-server numeric id of a change (`_number`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChangeNumber(pub u64);

/// The numeric id of an account (`_account_id`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(pub u64);

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
    }
}
