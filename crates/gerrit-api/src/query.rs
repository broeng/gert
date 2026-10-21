use model::{DiffContext, DiffKey, PatchSetNumber, Whitespace};

/// Optional extra data for change queries (`o=` parameters).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeOption {
    Labels,
    DetailedLabels,
    DetailedAccounts,
    CurrentRevision,
    CurrentCommit,
    AllRevisions,
    AllCommits,
    Messages,
    SubmitRequirements,
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
            Self::AllRevisions => "ALL_REVISIONS",
            Self::AllCommits => "ALL_COMMITS",
            Self::Messages => "MESSAGES",
            Self::SubmitRequirements => "SUBMIT_REQUIREMENTS",
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

/// Query parameters of the file diff endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffParams {
    /// Compare against this patch set; `None` compares against the parent
    /// commit.
    pub base: Option<PatchSetNumber>,
    pub context: DiffContext,
    /// Ask for intraline edits.
    pub intraline: bool,
    pub whitespace: Whitespace,
}

impl Default for DiffParams {
    fn default() -> Self {
        Self {
            base: None,
            context: DiffContext::Lines(10),
            intraline: true,
            whitespace: Whitespace::IgnoreNone,
        }
    }
}

impl DiffParams {
    /// The parameters for a cached comparison, always with intraline edits.
    pub fn for_key(key: &DiffKey) -> Self {
        Self {
            base: key.base.as_ref().map(|b| b.number),
            context: key.context,
            intraline: true,
            whitespace: key.whitespace.clone(),
        }
    }

    /// As query pairs; a flag is a key with an empty value.
    pub(crate) fn to_query(&self) -> Vec<(&'static str, String)> {
        let mut q = Vec::new();
        if let Some(base) = self.base {
            q.push(("base", base.to_string()));
        }
        q.push(("context", self.context.to_wire()));
        if self.intraline {
            q.push(("intraline", String::new()));
        }
        q.push(("whitespace", self.whitespace.as_wire().to_owned()));
        q
    }
}

/// Percent-encodes a query-string component or path segment (everything
/// but RFC 3986 unreserved characters). Gerrit requires `/` inside a file
/// path to be encoded as `%2F`, which this does.
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
