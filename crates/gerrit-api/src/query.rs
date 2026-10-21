/// Optional extra data for change queries (`o=` parameters).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeOption {
    Labels,
    DetailedLabels,
    DetailedAccounts,
    CurrentRevision,
    CurrentCommit,
    Submittable,
}

impl ChangeOption {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Labels => "LABELS",
            Self::DetailedLabels => "DETAILED_LABELS",
            Self::DetailedAccounts => "DETAILED_ACCOUNTS",
            Self::CurrentRevision => "CURRENT_REVISION",
            Self::CurrentCommit => "CURRENT_COMMIT",
            Self::Submittable => "SUBMITTABLE",
        }
    }
}

/// A `GET /changes/?q=...` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeQuery {
    /// Gerrit query syntax, e.g. `is:open owner:self`.
    pub query: String,
    /// Page size (`n`). `None` uses the server default.
    pub limit: Option<u32>,
    /// Number of results to skip (`S`).
    pub start: Option<u32>,
    pub options: Vec<ChangeOption>,
}

impl ChangeQuery {
    /// A query with the options needed for list rows.
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            limit: None,
            start: None,
            options: vec![
                ChangeOption::Labels,
                ChangeOption::DetailedAccounts,
                ChangeOption::Submittable,
            ],
        }
    }

    pub fn limit(mut self, n: u32) -> Self {
        self.limit = Some(n);
        self
    }

    pub fn start(mut self, s: u32) -> Self {
        self.start = Some(s);
        self
    }
}

/// Percent-encodes a query-string component (everything but RFC 3986
/// unreserved characters).
pub(crate) fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_gerrit_query_syntax() {
        assert_eq!(
            encode_component(r#"is:open (owner:self OR "a b")+x/ü"#),
            "is%3Aopen%20%28owner%3Aself%20OR%20%22a%20b%22%29%2Bx%2F%C3%BC"
        );
        assert_eq!(encode_component("a-b.c_d~e"), "a-b.c_d~e");
    }
}
