//! Deciding which rules fire for an event, and which changes to watch.

use model::{Account, AccountId, ChangeDetail, ChangeNumber, Event, EventKind};

use super::rules::{NotificationSettings, Rule, RuleEvent, Scope};

/// Whether a change is in the cached result of a saved query, or starred.
/// The app answers from its cache; the matcher never fetches.
pub trait QueryMembership {
    /// The change is in the cached result of the saved query `name`.
    fn in_saved_query(&self, name: &str, change: ChangeNumber) -> bool;
    fn starred(&self, change: ChangeNumber) -> bool;
}

/// Knows of no saved query and no star.
pub struct NoQueries;

impl QueryMembership for NoQueries {
    fn in_saved_query(&self, _: &str, _: ChangeNumber) -> bool {
        false
    }
    fn starred(&self, _: ChangeNumber) -> bool {
        false
    }
}

/// What the matcher needs to know besides the event.
pub struct MatchContext<'a> {
    /// The signed-in user of the event's server.
    pub me: AccountId,
    /// What a mention of the user looks like after the `@`: username and
    /// email. Case-insensitive. Empty: no mention is ever recognized.
    pub handles: &'a [String],
    /// Slug of the event's server.
    pub server: &'a str,
    /// The cached change the event is about.
    pub detail: &'a ChangeDetail,
    pub queries: &'a dyn QueryMembership,
}

/// Indices into `settings.rules` of the rules that fire for `event`, in
/// order. Empty when notifications are off, or the user did it themselves
/// and `ignore_own_actions` is set.
pub fn matches(
    settings: &NotificationSettings,
    event: &Event,
    ctx: &MatchContext<'_>,
) -> Vec<usize> {
    if !settings.enabled {
        return Vec::new();
    }
    if settings.ignore_own_actions && event.actor.as_ref().is_some_and(|a| a.id == ctx.me) {
        return Vec::new();
    }
    settings
        .rules
        .iter()
        .enumerate()
        .filter(|(_, rule)| rule_matches(rule, event, ctx))
        .map(|(i, _)| i)
        .collect()
}

fn rule_matches(rule: &Rule, event: &Event, ctx: &MatchContext<'_>) -> bool {
    rule.enabled
        && (rule.servers.is_empty() || rule.servers.iter().any(|s| s == ctx.server))
        && kind_matches(rule, event, ctx)
        && in_scope(&rule.scope, event.change, ctx)
        && actor_matches(rule, event.actor.as_ref())
}

fn kind_matches(rule: &Rule, event: &Event, ctx: &MatchContext<'_>) -> bool {
    match (&event.kind, rule.event) {
        (EventKind::Vote { label, value, .. }, RuleEvent::Vote) => {
            (rule.labels.is_empty() || rule.labels.iter().any(|l| l == "*" || l == label))
                && (rule.values.is_empty() || rule.values.contains(value))
        }
        // `only_unresolved` and `replies_to_me` are not evaluated, see
        // `Rule`.
        (EventKind::Comment { .. }, RuleEvent::Comment) => true,
        (EventKind::Comment { snippet, .. }, RuleEvent::Mention) => {
            ctx.handles.iter().any(|h| mentions(snippet, h))
        }
        // The event is about one account; a rule on my attention set is
        // not about somebody else's.
        (EventKind::AttentionAdded, RuleEvent::AttentionAdded) => {
            event.subject_account == Some(ctx.me)
        }
        (EventKind::ReviewerAdded { .. }, RuleEvent::ReviewerAdded) => true,
        (EventKind::NewPatchSet { .. }, RuleEvent::PatchSet) => true,
        (EventKind::Merged, RuleEvent::Merged)
        | (EventKind::Abandoned, RuleEvent::Abandoned)
        | (EventKind::ReadyToSubmit, RuleEvent::ReadyToSubmit)
        | (EventKind::MergeConflict, RuleEvent::MergeConflict) => true,
        _ => false,
    }
}

/// Whether `text` contains `@handle` not followed by more name characters.
fn mentions(text: &str, handle: &str) -> bool {
    if handle.is_empty() {
        return false;
    }
    let text = text.to_lowercase();
    let needle = format!("@{}", handle.to_lowercase());
    text.match_indices(&needle).any(|(i, m)| {
        let mut rest = text[i + m.len()..].chars();
        match rest.next() {
            None => true,
            // A full stop ends a sentence unless the name goes on.
            Some('.') => !rest.next().is_some_and(char::is_alphanumeric),
            Some(c) => !(c.is_alphanumeric() || matches!(c, '_' | '-' | '@')),
        }
    })
}

fn in_scope(scope: &Scope, change: ChangeNumber, ctx: &MatchContext<'_>) -> bool {
    let me = ctx.me;
    let detail = ctx.detail;
    let owner = || detail.summary.owner.id == me;
    let reviewer = || detail.reviewers.iter().any(|a| a.id == me);
    let cc = || detail.cc.iter().any(|a| a.id == me);
    let attention = || detail.summary.attention_set.contains(&me);
    match scope {
        Scope::Owner => owner(),
        Scope::Reviewer => reviewer(),
        Scope::Cc => cc(),
        Scope::Attention => attention(),
        Scope::Starred => ctx.queries.starred(change),
        // Not simply "true": an event also arises for changes the user
        // merely opened, which no watch query asked for.
        Scope::Any => owner() || reviewer() || cc() || attention() || ctx.queries.starred(change),
        Scope::Query(name) => ctx.queries.in_saved_query(name, change),
    }
}

fn actor_matches(rule: &Rule, actor: Option<&Account>) -> bool {
    if rule.exclude_bots && actor.is_some_and(is_bot) {
        return false;
    }
    if rule.from.is_empty() {
        return true;
    }
    let Some(actor) = actor else { return false };
    let names = [
        &actor.username,
        &actor.name,
        &actor.display_name,
        &actor.email,
    ];
    rule.from.iter().any(|wanted| {
        names
            .iter()
            .filter_map(|n| n.as_deref())
            .any(|n| n.eq_ignore_ascii_case(wanted))
    })
}

/// Whether the account is a bot. The model keeps no account tags (Gerrit's
/// `SERVICE_USER`), and guessing from names would silence real people, so
/// nobody is a bot yet. Use a rule's `from` list to pick a bot out, and
/// keep this the one place to improve when accounts carry tags.
fn is_bot(_account: &Account) -> bool {
    false
}

/// The search query for the watch pass: the changes any enabled rule of
/// `server` can be about, as one `OR` of the rules' scopes. `saved` maps
/// saved query names to their query strings; a rule on an unknown name
/// watches nothing.
///
/// Open changes only, unless a merged or abandoned rule exists: closing
/// changes leave `is:open`, so then the open restriction is dropped and
/// replaced by `-age:7d` (updated within a week, which is how long the
/// pass can be away and still see the change close).
///
/// Empty when there is nothing to watch: the switch is off or no rule
/// applies, and the caller should not poll.
pub fn watch_query(
    settings: &NotificationSettings,
    server: &str,
    saved: &[(String, String)],
) -> String {
    if !settings.enabled {
        return String::new();
    }
    let rules = settings
        .rules
        .iter()
        .filter(|r| r.enabled && (r.servers.is_empty() || r.servers.iter().any(|s| s == server)));
    let mut terms: Vec<String> = Vec::new();
    let mut closed = false;
    let mut any_rule = false;
    for rule in rules {
        any_rule = true;
        closed |= rule.event.needs_closed_changes();
        let simple: &[&str] = match &rule.scope {
            Scope::Owner => &["owner:self"],
            Scope::Reviewer => &["reviewer:self"],
            Scope::Cc => &["cc:self"],
            Scope::Attention => &["attention:self"],
            Scope::Starred => &["is:starred"],
            Scope::Any => &[
                "owner:self",
                "reviewer:self",
                "cc:self",
                "attention:self",
                "is:starred",
            ],
            Scope::Query(name) => {
                if let Some((_, q)) = saved.iter().find(|(n, _)| n == name) {
                    let q = q.trim();
                    if !q.is_empty() {
                        terms.push(format!("({q})"));
                    }
                }
                &[]
            }
        };
        terms.extend(simple.iter().map(|t| (*t).to_owned()));
    }
    let mut seen = Vec::new();
    terms.retain(|t| {
        let new = !seen.contains(t);
        seen.push(t.clone());
        new
    });
    if !any_rule || terms.is_empty() {
        return String::new();
    }
    let scope = if terms.len() == 1 {
        terms.remove(0)
    } else {
        format!("({})", terms.join(" OR "))
    };
    let bound = if closed { "-age:7d" } else { "is:open" };
    format!("{bound} {scope}")
}
