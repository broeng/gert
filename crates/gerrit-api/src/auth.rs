use std::fmt;

/// How to authenticate against a Gerrit server.
#[derive(Clone, PartialEq, Eq)]
pub enum Auth {
    /// Read-only access to public data; requests go to `{base}/...`.
    Anonymous,
    /// HTTP password or token; requests go to `{base}/a/...`.
    Basic { username: String, password: String },
}

impl Auth {
    pub(crate) fn is_authenticated(&self) -> bool {
        matches!(self, Auth::Basic { .. })
    }
}

// Hand-written so the password can never end up in logs.
impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Auth::Anonymous => f.write_str("Auth::Anonymous"),
            Auth::Basic { username, .. } => f
                .debug_struct("Auth::Basic")
                .field("username", username)
                .field("password", &"<redacted>")
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_password() {
        let a = Auth::Basic {
            username: "alice".into(),
            password: "hunter2".into(),
        };
        let s = format!("{a:?}");
        assert!(s.contains("alice"));
        assert!(!s.contains("hunter2"));
    }
}
