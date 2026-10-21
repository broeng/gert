use serde::{Deserialize, Serialize};

use crate::AccountId;

/// A Gerrit account as embedded in changes and reviewer lists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub name: Option<String>,
    pub email: Option<String>,
    pub username: Option<String>,
    pub display_name: Option<String>,
}

impl Account {
    /// The best human-readable label: display name, full name, username,
    /// email, and finally `Account <id>`. Empty strings are skipped.
    pub fn display(&self) -> String {
        [&self.display_name, &self.name, &self.username, &self.email]
            .into_iter()
            .flatten()
            .find(|s| !s.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| format!("Account {}", self.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare() -> Account {
        Account {
            id: AccountId(9),
            name: None,
            email: None,
            username: None,
            display_name: None,
        }
    }

    #[test]
    fn display_falls_back_in_order() {
        let mut a = bare();
        assert_eq!(a.display(), "Account 9");
        a.email = Some("a@example.com".into());
        assert_eq!(a.display(), "a@example.com");
        a.username = Some("alice".into());
        assert_eq!(a.display(), "alice");
        a.name = Some("Alice A".into());
        assert_eq!(a.display(), "Alice A");
        a.display_name = Some("Ali".into());
        assert_eq!(a.display(), "Ali");
        a.display_name = Some(" ".into());
        assert_eq!(a.display(), "Alice A");
    }
}
