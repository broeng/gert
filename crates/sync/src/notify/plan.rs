//! From matched events to notifications: which are shown on the desktop,
//! and how bursts are folded together. Pure: the caller passes in the
//! local time, the window state and the app start.

use jiff::{Timestamp, civil::Time};
use model::{ChangeNumber, Event};

use super::{NotificationSettings, RuleEvent, text};

/// More catch-up events than this (events from before the app started,
/// found by the first passes) are shown as one summary.
pub const CATCH_UP_LIMIT: usize = 5;

/// How many events a summary lists before "and K more".
const LISTED: usize = 3;

/// What to put on the desktop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub title: String,
    pub body: String,
    /// Slug of the server it is about.
    pub server: String,
    /// The change to open when it is clicked: `None` for a summary of
    /// several changes, which opens the server.
    pub change: Option<ChangeNumber>,
}

/// A matched event and what became of it. The in-app inbox keeps every
/// one of these, shown on the desktop or not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivered {
    pub event: Event,
    /// Index into `settings.rules` of the rule that fired.
    pub rule_index: usize,
    /// Whether it went to the desktop (alone or in a summary). False when
    /// it was suppressed: quiet hours, the window is focused, or it is
    /// the change on screen.
    pub shown_on_os: bool,
}

/// What the user is looking at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Screen<'a> {
    pub focused: bool,
    /// The server and change of the change or diff page on screen.
    pub current: Option<(&'a str, ChangeNumber)>,
}

/// The result of [`plan`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// One per matched event, in the order given.
    pub delivered: Vec<Delivered>,
    pub notifications: Vec<Notification>,
}

/// The rule to credit when several fired: a mention rule wins (it is the
/// most specific reading of a comment), else the first.
pub fn pick_rule(settings: &NotificationSettings, fired: &[usize]) -> Option<usize> {
    fired
        .iter()
        .copied()
        .find(|&i| settings.rules.get(i).map(|r| r.event) == Some(RuleEvent::Mention))
        .or_else(|| fired.first().copied())
}

/// Whether the desktop notification for a matched event of `change` on
/// `server` is held back at `now` (local time).
///
/// The suppression of the change on screen needs the focus as well: in a
/// window the user has left, the change page tells them nothing.
pub fn suppressed(
    settings: &NotificationSettings,
    screen: &Screen<'_>,
    server: &str,
    change: ChangeNumber,
    now: Time,
) -> bool {
    settings.in_quiet_hours(now)
        || (settings.only_when_unfocused && screen.focused)
        || (settings.suppress_current_change
            && screen.focused
            && screen.current == Some((server, change)))
}

/// Decides the notifications for the `matched` events (each with the rule
/// that fired) of `server`, found by one pass.
///
/// Suppressed events are only recorded. Of the others, those from before
/// `started` collapse into one "while you were away" summary if there are
/// more than [`CATCH_UP_LIMIT`]; the rest are grouped: events no further
/// apart than `settings.group_window` become one notification when there
/// are several.
pub fn plan(
    settings: &NotificationSettings,
    server: &str,
    matched: Vec<(Event, usize)>,
    screen: &Screen<'_>,
    now: Time,
    started: Timestamp,
) -> Plan {
    let delivered: Vec<Delivered> = matched
        .into_iter()
        .map(|(event, rule_index)| Delivered {
            shown_on_os: !suppressed(settings, screen, server, event.change, now),
            event,
            rule_index,
        })
        .collect();

    let mut shown: Vec<&Delivered> = delivered.iter().filter(|d| d.shown_on_os).collect();
    shown.sort_by_key(|d| d.event.at);
    let mut notifications = Vec::new();

    let old = shown.iter().filter(|d| d.event.at < started).count();
    if old > CATCH_UP_LIMIT {
        let (old, rest): (Vec<_>, Vec<_>) = shown.into_iter().partition(|d| d.event.at < started);
        let title = format!("{} updates while you were away", old.len());
        notifications.push(summary(settings, server, title, &old));
        shown = rest;
    }

    let mut cluster: Vec<&Delivered> = Vec::new();
    for d in shown {
        let apart = cluster
            .last()
            .and_then(|last| d.event.at.since(last.event.at).ok())
            .and_then(|span| std::time::Duration::try_from(span).ok());
        if apart.is_some_and(|gap| gap > settings.group_window) {
            notifications.push(group(settings, server, &cluster));
            cluster.clear();
        }
        cluster.push(d);
    }
    if !cluster.is_empty() {
        notifications.push(group(settings, server, &cluster));
    }
    Plan {
        delivered,
        notifications,
    }
}

fn is_mention(settings: &NotificationSettings, d: &Delivered) -> bool {
    settings.rules.get(d.rule_index).map(|r| r.event) == Some(RuleEvent::Mention)
}

/// One event alone, or "N updates on M changes".
fn group(settings: &NotificationSettings, server: &str, events: &[&Delivered]) -> Notification {
    if let [d] = events {
        return Notification {
            title: text::title(&d.event, is_mention(settings, d)),
            body: text::body(&d.event),
            server: server.to_owned(),
            change: Some(d.event.change),
        };
    }
    let changes = distinct_changes(events);
    let title = format!(
        "{} updates on {} change{}",
        events.len(),
        changes.len(),
        if changes.len() == 1 { "" } else { "s" }
    );
    summary(settings, server, title, events)
}

/// A notification with a list of what happened: the first few events, one
/// per line, then how many more.
fn summary(
    settings: &NotificationSettings,
    server: &str,
    title: String,
    events: &[&Delivered],
) -> Notification {
    let mut lines: Vec<String> = events
        .iter()
        .take(LISTED)
        .map(|d| {
            let line = text::title(&d.event, is_mention(settings, d));
            let tag = format!("#{}", d.event.change);
            if line.contains(&tag) {
                line
            } else {
                format!("{line} ({tag})")
            }
        })
        .collect();
    if events.len() > LISTED {
        lines.push(format!("and {} more", events.len() - LISTED));
    }
    let changes = distinct_changes(events);
    Notification {
        title,
        body: lines.join("\n"),
        server: server.to_owned(),
        change: (changes.len() == 1).then(|| changes[0]),
    }
}

fn distinct_changes(events: &[&Delivered]) -> Vec<ChangeNumber> {
    let mut changes = Vec::new();
    for d in events {
        if !changes.contains(&d.event.change) {
            changes.push(d.event.change);
        }
    }
    changes
}

#[cfg(test)]
mod tests;
