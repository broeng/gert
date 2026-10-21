//! Domain types shared by every crate. No I/O, no GPUI.
//!
//! Fields are non-optional where a Gerrit 3.x server always sends them in a
//! change query response; everything newer or conditional is `Option` or a
//! defaulted collection. Types here derive serde so `store` can persist them.

mod account;
mod change;
mod diff;
mod file;
mod ids;
mod keys;
mod message;
mod prefs;
mod related;
mod revision;
mod submit;
mod timestamp;

pub use account::Account;
pub use change::{ChangeDetail, ChangeStatus, ChangeSummary, LabelSummary, LabelVote};
pub use diff::{
    ChangedChunk, Diff, DiffChangeType, DiffChunk, DiffFileMeta, IntralineEdit, IntralineStatus,
    line_marks,
};
pub use file::{COMMIT_MSG_PATH, FileEntry, FileStatus, MERGE_LIST_PATH, file_order};
pub use ids::{AccountId, ChangeNumber, PatchSetNumber, RevisionId};
pub use keys::{DiffKey, FilesKey};
pub use message::ChangeMessage;
pub use prefs::{
    DiffContext, DiffPreferences, DiffViewMode, ResolvedDiffPreferences, Whitespace,
    resolve_diff_preferences,
};
pub use related::RelatedChange;
pub use revision::{Commit, CommitParent, GitPerson, PatchSetRef, Revision, RevisionKind};
pub use submit::{ExpressionResult, ExpressionStatus, SubmitRequirement, SubmitRequirementStatus};
pub use timestamp::{Timestamp, TimestampParseError, parse_gerrit_timestamp};
