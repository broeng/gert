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

impl ActionInfo {
    /// Whether the server lists `option` among the options it can do for
    /// this action (`rebase_on_behalf_of_uploader`, ...).
    pub fn has_option(&self, option: &str) -> bool {
        self.enabled_options.iter().any(|o| o == option)
    }
}

/// The `enabled_options` entry of the rebase action that says the caller
/// may rebase on behalf of the uploader.
pub const REBASE_ON_BEHALF_OF_UPLOADER: &str = "rebase_on_behalf_of_uploader";

/// What a rebase may be asked to do besides the base (`RebaseInput`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RebaseOptions {
    /// Make a patch set with conflict markers instead of failing with a
    /// conflict.
    pub allow_conflicts: bool,
    /// Keep the uploader as the author of the new patch set's upload.
    pub on_behalf_of_uploader: bool,
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
