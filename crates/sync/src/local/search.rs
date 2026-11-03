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
//!
//!   Both hold only for rows in the `Local` state, the only ones that
//!   match at all: a `Checking` row may turn out to be on Gerrit (a merged
//!   cherry-pick, say), a `Vanished` one may have been pushed and merged,
//!   and an `Ambiguous` one is on Gerrit.
//! * `change:I…`, a full Change-Id.
//! * Plain words, matched against the subject only, case-insensitively and
//!   by whole words: fewer hits than Gerrit's default field, which looks
//!   at more than the subject, but never ones it would not give. A quoted
//!   phrase is its words in a row; a word like `foo-bar` is the words
//!   `foo` and `bar` in a row, as Gerrit splits it (`'` and `,` between
//!   letters or digits stay in the word: `foo's`, `1,000`).
//! * Implicit AND, and an explicit `AND` / `and` between two terms.
//!
//! Anything else makes the whole query unsupported: no local results,
//! rather than wrong ones. That is another operator or value, `OR` /
//! `or`, `NOT` / `not`, `-` or `!` negation, parentheses, braces, regular
//! expressions, a quote that does not close, a backslash, a dangling
//! `AND`, and the characters Gerrit refuses outside quotes (`' , + ; = ?
//! [ ] $ % &`). So is a word that Gerrit's default field takes for a
//! change (a number, a Change-Id prefix like `ifdef`, a triplet) or for a
//! ref (`refs/…`): Gerrit searches those as such, never as text.

use model::{LocalChange, LocalChangeState};

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
        // The last token was an `AND`: a term has to follow.
        let mut and = false;
        for token in tokens(query)? {
            match token {
                Token::Bare(word) if word == "AND" || word == "and" => {
                    if terms.is_empty() || and {
                        return None;
                    }
                    and = true;
                    continue;
                }
                Token::Bare(word) if matches!(word.as_str(), "OR" | "or" | "NOT" | "not") => {
                    return None;
                }
                Token::Bare(word) | Token::Quoted(word) => {
                    if is_change_like(&word) || word.starts_with("refs/") {
                        return None;
                    }
                    terms.push(words(&word)?);
                }
                Token::Operator(op, value) => terms.push(operator(&op, &value)?),
            }
            and = false;
        }
        (!terms.is_empty() && !and).then_some(Self { terms })
    }

    /// Whether `change` fits every term; never for a row that is not in
    /// the `Local` state (see the module comment), nor for one with a
    /// Gerrit change.
    pub fn matches(&self, change: &LocalChange) -> bool {
        // A row with a Gerrit change (listed on the dashboard for its held
        // items) is found by Gerrit's own search.
        change.state == LocalChangeState::Local
            && change.gerrit_number.is_none()
            && self.terms.iter().all(|t| match t {
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

/// What Gerrit's query lexer refuses outside quotes.
const REFUSED: [char; 11] = ['\'', ',', '+', ';', '=', '?', '[', ']', '$', '%', '&'];

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
            } else if !quoted && REFUSED.contains(&c) {
                return None;
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

/// Whether Gerrit's default field takes `term` for a change rather than
/// text: `^(?:[1-9][0-9]*|(?:[^~]+~[^~]+~)?[iI][0-9a-f]{4,}.*)$`.
fn is_change_like(term: &str) -> bool {
    let number = term.starts_with(|c: char| ('1'..='9').contains(&c))
        && term.bytes().all(|b| b.is_ascii_digit());
    let id_prefix = |s: &str| {
        let b = s.as_bytes();
        b.len() >= 5
            && (b[0] == b'i' || b[0] == b'I')
            && b[1..5]
                .iter()
                .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
    };
    let triplet = || {
        let mut parts = term.splitn(3, '~');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(p), Some(b), Some(rest)) => !p.is_empty() && !b.is_empty() && id_prefix(rest),
            _ => false,
        }
    };
    number || id_prefix(term) || triplet()
}

/// Lowercase runs of letters and digits; a `'` or `,` between two of them
/// stays in the word, as in Gerrit's tokenizer.
fn split_words(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut word = String::new();
    for (i, &c) in chars.iter().enumerate() {
        let inner = (c == '\'' || c == ',')
            && i > 0
            && chars[i - 1].is_alphanumeric()
            && chars.get(i + 1).is_some_and(|n| n.is_alphanumeric());
        if c.is_alphanumeric() || inner {
            word.extend(c.to_lowercase());
        } else if !word.is_empty() {
            out.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
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
            ("project:tools/gert and is:open", true, true),
            ("fix and scroll", true, true),
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
            // Lowercase operators.
            "fix or it",
            "not fix",
            // A dangling AND is a parse error on Gerrit.
            "AND",
            "and fix",
            "fix AND",
            "fix AND AND it",
            "fix and AND it",
            // What Gerrit's default field takes for a change or a ref.
            "timeout 300",
            "300",
            "ifdef",
            "iface",
            "Ifdef cleanup",
            "\"ifdef cleanup\"",
            "I0123abcd",
            "p~main~I0123",
            "\"p~main~Iabcd\"",
            "refs/heads/main",
            "\"refs/for/main\"",
            // Characters Gerrit refuses outside quotes.
            "foo's",
            "1,000",
            "a+b",
            "a;b",
            "a=b",
            "why?",
            "[fix]",
            "$fix",
            "50%",
            "a&b",
        ] {
            assert_eq!(LocalQuery::parse(query), None, "{query:?}");
            assert!(matching(query, vec![c.clone()]).is_empty(), "{query:?}");
        }
    }

    #[test]
    fn words_keep_inner_apostrophes_and_commas_and_numbers_stay_text_when_not_change_like() {
        let c = change("p", Some("main"), "Fix foo's limit of 1,000 and 0815");
        for (query, want) in [
            ("\"foo's\"", true),
            ("foo", false),
            ("\"1,000\"", true),
            ("\"of 1,000\"", true),
            // A leading zero is no change number: text.
            ("0815", true),
            // Not a change: fewer than four hex digits after the `i`.
            ("ifa", false),
        ] {
            let q = LocalQuery::parse(query).unwrap_or_else(|| panic!("{query:?} is supported"));
            assert_eq!(q.matches(&c), want, "{query:?}");
        }
    }

    #[test]
    fn only_rows_in_the_local_state_match() {
        let mut c = change("p", Some("main"), "Fix it");
        for state in [
            LocalChangeState::Checking,
            LocalChangeState::Ambiguous,
            LocalChangeState::Vanished,
            LocalChangeState::Closed,
            LocalChangeState::Consolidated,
        ] {
            c.state = state;
            assert!(matching("fix", vec![c.clone()]).is_empty(), "{state:?}");
            assert!(
                matching("is:open owner:self", vec![c.clone()]).is_empty(),
                "{state:?}"
            );
        }
        c.state = LocalChangeState::Local;
        assert_eq!(matching("fix", vec![c.clone()]), vec![c.clone()]);
        // Versions of a Gerrit change (on the dashboard for their held
        // items) are Gerrit's to find.
        c.gerrit_number = Some(model::ChangeNumber(42));
        assert!(matching("fix", vec![c]).is_empty());
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
