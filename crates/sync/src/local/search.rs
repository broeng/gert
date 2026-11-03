//! The part of Gerrit's query language that local changes are matched
//! against, so a search page can list the purely local changes that fit
//! its query above Gerrit's results. Gerrit never sees them, so the query
//! is evaluated here.
//!
//! The subset (the user's decision):
//!
//! * `project:NAME` and `branch:NAME` (exact; `branch:refs/heads/NAME`
//!   too). `branch:` matches the change's target branch, and a change
//!   whose target branch is unknown matches no branch.
//! * `is:open` and `status:open`: every local change is open.
//! * `owner:self`: every local change is the user's own (discovery takes
//!   only the user's commits).
//! * `change:I…`, a full Change-Id.
//! * Plain words, matched against the subject only, case-insensitively and
//!   by whole words: fewer hits than Gerrit's default field, which looks
//!   at more than the subject, but never ones it would not give. A quoted
//!   phrase is its words in a row; a word like `foo-bar` is the words
//!   `foo` and `bar` in a row, as Gerrit splits it.
//! * Implicit AND, and an explicit `AND`.
//!
//! Anything else (another operator or value, `OR`, `NOT`, `-` negation,
//! parentheses, braces, regular expressions, a quote that does not close,
//! a backslash) makes the whole query unsupported: no local results,
//! rather than wrong ones.

use model::LocalChange;

/// A query in the supported subset. See the module comment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalQuery {
    terms: Vec<Term>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Term {
    Project(String),
    Branch(String),
    /// `is:open`, `status:open`.
    Open,
    /// `owner:self`.
    OwnerSelf,
    ChangeId(String),
    /// Lowercase words that the subject has in a row.
    Words(Vec<String>),
}

impl LocalQuery {
    /// The query, or `None` when it uses anything outside the subset (or
    /// is empty).
    pub fn parse(query: &str) -> Option<Self> {
        let mut terms = Vec::new();
        for token in tokens(query)? {
            match token {
                Token::Bare(word) if word == "AND" => {}
                Token::Bare(word) if word == "OR" || word == "NOT" => return None,
                Token::Bare(word) | Token::Quoted(word) => terms.push(words(&word)?),
                Token::Operator(op, value) => terms.push(operator(&op, &value)?),
            }
        }
        (!terms.is_empty()).then_some(Self { terms })
    }

    /// Whether `change` fits every term.
    pub fn matches(&self, change: &LocalChange) -> bool {
        self.terms.iter().all(|t| match t {
            Term::Project(p) => change.project == *p,
            Term::Branch(b) => change.branch.as_deref() == Some(b.as_str()),
            Term::Open | Term::OwnerSelf => true,
            Term::ChangeId(id) => change.change_id == *id,
            Term::Words(w) => has_run(&split_words(&change.subject), w),
        })
    }
}

/// The changes of `changes` that fit `query`, in their order; none when
/// the query is outside the subset.
pub fn matching(query: &str, changes: Vec<LocalChange>) -> Vec<LocalChange> {
    match LocalQuery::parse(query) {
        Some(q) => changes.into_iter().filter(|c| q.matches(c)).collect(),
        None => Vec::new(),
    }
}

enum Token {
    /// A word as typed.
    Bare(String),
    /// A quoted phrase, without the quotes.
    Quoted(String),
    /// `op:value`, the value without quotes.
    Operator(String, String),
}

/// The query split at whitespace outside quotes; `None` for syntax the
/// subset does not handle.
fn tokens(query: &str) -> Option<Vec<Token>> {
    if query.contains(['(', ')', '{', '}', '\\']) {
        return None;
    }
    let mut out = Vec::new();
    let mut chars = query.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(&first) = chars.peek() else {
            return Some(out);
        };
        if first == '-' || first == '!' {
            return None;
        }
        // Up to the next whitespace outside quotes.
        let mut raw = String::new();
        let mut quoted = false;
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() && !quoted {
                break;
            }
            if c == '"' {
                quoted = !quoted;
            }
            raw.push(c);
            chars.next();
        }
        if quoted {
            return None;
        }
        out.push(token(&raw)?);
    }
}

/// One whitespace-separated piece: a quote may only open at its start or
/// right after the operator's colon, and close at its end.
fn token(raw: &str) -> Option<Token> {
    // `"text"` as `text`, a string without quotes as it is.
    let unquote = |s: &str| -> Option<String> {
        match s.strip_prefix('"') {
            Some(rest) => {
                let inner = rest.strip_suffix('"')?;
                (!inner.contains('"')).then(|| inner.to_owned())
            }
            None => (!s.contains('"')).then(|| s.to_owned()),
        }
    };
    if raw.starts_with('"') {
        return unquote(raw).map(Token::Quoted);
    }
    match raw.split_once(':') {
        Some((op, value)) if !op.contains('"') => {
            Some(Token::Operator(op.to_owned(), unquote(value)?))
        }
        Some(_) => None,
        None => unquote(raw).map(Token::Bare),
    }
}

fn operator(op: &str, value: &str) -> Option<Term> {
    // A leading `^` is a regular expression in project: and branch:.
    let name = |v: &str| (!v.is_empty() && !v.starts_with('^')).then(|| v.to_owned());
    match op {
        "project" => name(value).map(Term::Project),
        "branch" => name(value.strip_prefix("refs/heads/").unwrap_or(value)).map(Term::Branch),
        "is" | "status" if value.eq_ignore_ascii_case("open") => Some(Term::Open),
        "owner" if value == "self" => Some(Term::OwnerSelf),
        "change" if is_change_id(value) => Some(Term::ChangeId(value.to_owned())),
        _ => None,
    }
}

/// `I` and 40 hex digits.
fn is_change_id(value: &str) -> bool {
    value.len() == 41 && value.starts_with('I') && value[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// A plain word or phrase as words; `None` when it has none (only
/// punctuation), which is no word the subject could have.
fn words(text: &str) -> Option<Term> {
    let w = split_words(text);
    (!w.is_empty()).then_some(Term::Words(w))
}

/// Lowercase runs of letters and digits.
fn split_words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Whether `run` occurs in `words` as consecutive words.
fn has_run(words: &[String], run: &[String]) -> bool {
    !run.is_empty() && words.windows(run.len()).any(|w| w == run)
}

#[cfg(test)]
mod tests {
    use model::{ChangeNumber, LocalChangeId, LocalChangeState, Timestamp};

    use super::*;

    const ID: &str = "I0123456789abcdef0123456789abcdef01234567";

    fn change(project: &str, branch: Option<&str>, subject: &str) -> LocalChange {
        LocalChange {
            id: LocalChangeId(1),
            number: ChangeNumber(ChangeNumber::LOCAL_BASE + 1),
            project: project.into(),
            branch: branch.map(str::to_owned),
            change_id: ID.into(),
            gerrit_number: None,
            subject: subject.into(),
            author_name: "Me".into(),
            author_email: "me@example.com".into(),
            updated: Timestamp::UNIX_EPOCH,
            state: LocalChangeState::Local,
            first_seen: Timestamp::UNIX_EPOCH,
        }
    }

    #[test]
    fn the_supported_subset_matches_as_gerrit_would() {
        let c = change("tools/gert", Some("main"), "Fix the diff-view scroll");
        let no_branch = change("tools/gert", None, "Fix the diff-view scroll");
        // (query, matches c, matches the change without a target branch)
        let table: &[(&str, bool, bool)] = &[
            ("project:tools/gert", true, true),
            ("project:tools", false, false),
            ("project:\"tools/gert\"", true, true),
            ("branch:main", true, false),
            ("branch:refs/heads/main", true, false),
            ("branch:stable", false, false),
            ("is:open", true, true),
            ("status:open", true, true),
            ("status:OPEN", true, true),
            ("owner:self", true, true),
            (&format!("change:{ID}"), true, true),
            // Change-Ids are lowercase; another spelling is not taken as one.
            (&format!("change:{}", ID.to_uppercase()), false, false),
            (
                "change:I00000000000000000000000000000000000000ff",
                false,
                false,
            ),
            // Words: whole words, any case, in the subject only.
            ("fix", true, true),
            ("FIX", true, true),
            ("fi", false, false),
            ("scroll fix", true, true),
            ("diff-view", true, true),
            ("view-diff", false, false),
            ("\"the diff\"", true, true),
            ("\"diff the\"", false, false),
            ("gert", false, false),
            // Implicit and explicit AND.
            ("project:tools/gert branch:main fix", true, false),
            ("project:tools/gert AND is:open", true, true),
            ("project:tools/gert unrelated", false, false),
        ];
        for (query, want, want_no_branch) in table {
            let q = LocalQuery::parse(query).unwrap_or_else(|| panic!("{query:?} is supported"));
            assert_eq!(q.matches(&c), *want, "{query:?}");
            assert_eq!(
                q.matches(&no_branch),
                *want_no_branch,
                "{query:?} without a branch"
            );
        }
    }

    #[test]
    fn anything_outside_the_subset_gives_no_local_results() {
        let c = change("p", Some("main"), "Fix it");
        for query in [
            "",
            "   ",
            "fix OR it",
            "NOT fix",
            "-fix",
            "-is:open",
            "!fix",
            "(fix)",
            "project:p (fix)",
            "{fix it}",
            "project:^p.*",
            "branch:^ma.*",
            "project:",
            "is:closed",
            "is:merged",
            "status:merged",
            "is:wip",
            "owner:me@example.com",
            "change:123",
            "change:I0123",
            "message:fix",
            "topic:x",
            "label:Code-Review+2",
            "\"fix it",
            "fix\"it\"",
            "project:\"p",
            "\"a\"b\"",
            "fix\\ it",
            "---",
            "http://example.com",
        ] {
            assert_eq!(LocalQuery::parse(query), None, "{query:?}");
            assert!(matching(query, vec![c.clone()]).is_empty(), "{query:?}");
        }
    }

    #[test]
    fn branch_never_matches_an_unknown_target_branch() {
        let unknown = change("p", None, "Fix it");
        // Not even the empty placeholder a synthesised summary carries.
        assert_eq!(LocalQuery::parse("branch:"), None);
        assert!(matching("branch:main", vec![unknown.clone()]).is_empty());
        assert_eq!(matching("fix", vec![unknown.clone()]), vec![unknown]);
    }
}
