//! Domain types shared by every crate. No I/O, no GPUI.
//!
//! Fields are non-optional where a Gerrit 3.x server always sends them in a
//! change query response; everything newer or conditional is `Option` or a
//! defaulted collection. Types here derive serde so `store` can persist them.

mod account;
mod change;
mod ids;
mod timestamp;

pub use account::Account;
pub use change::{ChangeDetail, ChangeStatus, ChangeSummary, LabelSummary};
pub use ids::{AccountId, ChangeNumber};
pub use timestamp::{Timestamp, TimestampParseError, parse_gerrit_timestamp};
