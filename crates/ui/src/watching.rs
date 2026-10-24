//! The notification watch of one server: finds the events its rules care
//! about, off the UI thread. The shell runs it on a timer, shows what
//! [`sync::notify::plan`] makes of the result and acknowledges the events.
//!
//! A pass asks the server for the changes the rules watch, caches those
//! that moved (the events come out of that, see `sync::watch`), and matches
//! every pending event against the rules. All of it blocks on SQLite and
//! the network: call it from the background executor.

use model::{Account, AccountId, ChangeNumber, Event};
use sync::{
    Syncer, WatchOptions,
    notify::{
        MatchContext, NotificationSettings, QueryMembership, matches, pick_rule, watch_query,
    },
};

/// The signed-in user of a server, as the matcher needs them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Me {
    pub id: AccountId,
    /// What a mention looks like after the `@`: username and email.
    pub handles: Vec<String>,
}

impl Me {
    fn of(account: &Account) -> Self {
        Self {
            id: account.id,
            handles: [&account.username, &account.email]
                .into_iter()
                .flatten()
                .filter(|h| !h.is_empty())
                .cloned()
                .collect(),
        }
    }
}

/// What a pass needs to know, read from the shell at its start.
#[derive(Clone)]
pub(crate) struct Inputs {
    /// Slug of the server.
    pub server: String,
    pub settings: NotificationSettings,
    /// The saved queries of the server, `(name, query)`.
    pub saved: Vec<(String, String)>,
    /// Known from an earlier pass.
    pub me: Option<Me>,
}

/// What a pass found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Found {
    /// The events some rule fired for, with the index of that rule.
    pub matched: Vec<(Event, usize)>,
    /// Every event that was pending, matched or not: all of them are to be
    /// acknowledged, so that none comes back.
    pub seen: Vec<Event>,
}

/// The result of [`run`]: `me` is to be remembered.
pub(crate) struct Ran {
    pub me: Me,
    pub found: Found,
}

/// Runs one pass (`watch` set), or only collects what refreshes of single
/// changes queued since (not set). `None` when there is nothing to watch:
/// the switch is off or no rule applies.
pub(crate) async fn run(
    syncer: &Syncer,
    inputs: &Inputs,
    watch: bool,
) -> Result<Option<Ran>, sync::Error> {
    let query = watch_query(&inputs.settings, &inputs.server, &inputs.saved);
    if query.is_empty() {
        return Ok(None);
    }
    let me = match &inputs.me {
        Some(me) if !me.handles.is_empty() => me.clone(),
        known => match syncer.account_self().await {
            Ok(account) => Me::of(&account),
            // Keep what we know rather than failing the pass over it.
            Err(e) => known.clone().ok_or(e)?,
        },
    };
    let events = if watch {
        syncer.watch_pass(&query, WatchOptions::default()).await?
    } else {
        syncer.pending_events()?
    };
    let found = classify(syncer, inputs, &me, events);
    Ok(Some(Ran { me, found }))
}

fn classify(syncer: &Syncer, inputs: &Inputs, me: &Me, events: Vec<Event>) -> Found {
    let queries = CachedMembership {
        syncer,
        saved: &inputs.saved,
    };
    let mut matched = Vec::new();
    for event in &events {
        // The cache has the detail the event came from; without it there
        // is nothing to judge the scope by.
        let Ok(Some(cached)) = syncer.cached_change(event.change) else {
            continue;
        };
        let ctx = MatchContext {
            me: me.id,
            handles: &me.handles,
            server: &inputs.server,
            detail: &cached.detail,
            queries: &queries,
        };
        let fired = matches(&inputs.settings, event, &ctx);
        if let Some(rule) = pick_rule(&inputs.settings, &fired) {
            matched.push((event.clone(), rule));
        }
    }
    Found {
        matched,
        seen: events,
    }
}

/// Answers "is this change in that saved query" from the cached results.
struct CachedMembership<'a> {
    syncer: &'a Syncer,
    saved: &'a [(String, String)],
}

impl CachedMembership<'_> {
    fn cached_contains(&self, query: &str, change: ChangeNumber) -> bool {
        self.syncer
            .cached_query(query)
            .ok()
            .flatten()
            .is_some_and(|c| c.changes.iter().any(|s| s.number == change))
    }
}

impl QueryMembership for CachedMembership<'_> {
    fn in_saved_query(&self, name: &str, change: ChangeNumber) -> bool {
        self.saved
            .iter()
            .find(|(n, _)| n == name)
            .is_some_and(|(_, q)| self.cached_contains(q, change))
    }

    /// From the cached `is:starred` query, when the user has looked at it.
    fn starred(&self, change: ChangeNumber) -> bool {
        self.cached_contains("is:starred", change)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_are_the_username_and_the_email() {
        let account = |username: Option<&str>, email: Option<&str>| Account {
            id: AccountId(3),
            name: Some("Me".into()),
            email: email.map(str::to_owned),
            username: username.map(str::to_owned),
            display_name: None,
        };
        let me = Me::of(&account(Some("me"), Some("me@example.com")));
        assert_eq!(me.id, AccountId(3));
        assert_eq!(me.handles, ["me", "me@example.com"]);
        assert!(Me::of(&account(Some(""), None)).handles.is_empty());
    }
}
