//! Notification rules: which events are worth telling the user about.
//!
//! Pure data and functions, no I/O and no clock. The app reads these
//! types from `[notifications]` in the config file; the matcher decides
//! per event which rules fire, and [`watch_query`] builds the one list
//! query the watch pass needs to see the changes those rules care about.
//! Delivery, grouping and the focus rules belong to the caller.

mod matcher;
mod rules;

pub use matcher::{MatchContext, NoQueries, QueryMembership, matches, watch_query};
pub use rules::{
    NotificationSettings, QuietHours, QuietHoursError, Rule, RuleEvent, Scope, default_rules,
};

#[cfg(test)]
mod tests;
