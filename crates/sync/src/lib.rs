//! Refreshes the local cache from the server. GPUI-free and
//! runtime-agnostic: no timers and no spawning here.
//!
//! The UI reads from [`store`] (via [`Syncer::cached_query`] and friends)
//! and calls the `refresh_*` / `load_more` futures to update it. It asks
//! [`PollPolicy`] when to do so and drives the timers itself.
//!
//! Blocking: the futures wait on the network, but also perform short
//! blocking SQLite calls while holding the store mutex (never across an
//! `.await`). Run them on a background executor (GPUI's background pool),
//! not on the UI thread.

mod error;
mod events;
mod poll;
mod syncer;

pub use error::{Error, PendingDraft};
pub use events::diff_change;
pub use poll::{PollPolicy, PollState};
pub use syncer::{
    CommentsView, DraftCounts, DraftSave, NewChange, PublishOutcome, QueryUpdate, RefreshSource,
    RetryReport, ReviewerAdded, Syncer,
};
