use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One thing the signed-in user may be able to do with a change or a
/// revision (`ActionInfo`), e.g. `submit` or `abandon`.
///
/// The action's id is the key of the map it comes in (see
/// [`ChangeDetail::action`](crate::ChangeDetail::action)); it is kept as a
/// string because servers and plugins add ids (`revert`, `cherrypick`,
/// `followup`, `private.delete`, ...) and an unknown one must not fail a
/// page. Every field is optional on the wire.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionInfo {
    /// The HTTP method of the endpoint behind the action (`POST`, ...).
    pub method: Option<String>,
    /// Short button text, e.g. "Submit".
    pub label: Option<String>,
    /// Tooltip, e.g. why the action is disabled.
    pub title: Option<String>,
    /// Whether the action is allowed right now. Gerrit omits the field when
    /// it is false.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub enabled_options: Vec<String>,
}

/// Actions keyed by id.
pub type Actions = BTreeMap<String, ActionInfo>;

/// Well-known action ids.
pub mod action_id {
    pub const SUBMIT: &str = "submit";
    pub const REBASE: &str = "rebase";
    pub const ABANDON: &str = "abandon";
    pub const RESTORE: &str = "restore";
    pub const WIP: &str = "wip";
    pub const READY: &str = "ready";
    pub const PRIVATE: &str = "private";
    pub const PRIVATE_DELETE: &str = "private.delete";
    pub const REVERT: &str = "revert";
    pub const CHERRYPICK: &str = "cherrypick";
    pub const TOPIC: &str = "topic";
    pub const HASHTAGS: &str = "hashtags";
}
