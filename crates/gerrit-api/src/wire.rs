//! Tolerant decoding of SDK models.
//!
//! The SDK models Gerrit enums as closed Rust enums, so one unknown value
//! would fail a whole page of results. We therefore pull the enum fields we
//! care about out of the JSON first, and drop sub-trees we never read.

use gerrit_client::models::ChangeInfo;
use serde_json::Value;

/// Top-level `ChangeInfo` keys we never convert; dropping them avoids
/// failures on enums we do not model.
const IGNORED_KEYS: &[&str] = &[
    "submit_type",
    "requirements",
    "submit_records",
    "submit_requirements",
    "reviewer_updates",
    "messages",
    "problems",
    "plugins",
    "tracking_ids",
    "actions",
];

/// A decoded change plus the fields we handle outside the SDK model.
pub(crate) struct WireChange {
    pub info: ChangeInfo,
    /// Raw `status` string, so unknown statuses survive.
    pub status: Option<String>,
}

pub(crate) fn parse_change(mut v: Value) -> Result<WireChange, serde_json::Error> {
    let mut status = None;
    if let Some(obj) = v.as_object_mut() {
        status = match obj.remove("status") {
            Some(Value::String(s)) => Some(s),
            _ => None,
        };
        for k in IGNORED_KEYS {
            obj.remove(*k);
        }
        if let Some(Value::Object(revs)) = obj.get_mut("revisions") {
            for rev in revs.values_mut() {
                if let Some(r) = rev.as_object_mut() {
                    r.remove("kind");
                    r.remove("actions");
                }
            }
        }
    }
    Ok(WireChange {
        info: serde_json::from_value(v)?,
        status,
    })
}
