//! Refreshes the local cache from the server. GPUI-free and
//! runtime-agnostic: no spawning here, and no timers except the deadline
//! on consolidation's posts (`async-io`, which works on any executor).
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
pub mod local;
pub mod notify;
mod poll;
mod port;
mod syncer;

pub use error::{Error, PendingDraft};
pub use events::diff_change;
pub use poll::{PollPolicy, PollState};
pub use port::{HasCheckout, NotLocalPath, PathMap, PortCommit, PortError, PortMaps, PortSource};
pub use syncer::{
    CLAIM_STALE, ChainCommit, CommentsView, ConsolidateOptions, ConsolidateReport,
    ConsolidationTiming, DraftCounts, DraftSave, FoundChange, HeldItem, HeldPublished, NewChange,
    POST_DEADLINE, Ported, PostOutcome, PostReport, PublishOutcome, QueryUpdate, RefreshSource,
    RetryReport, ReviewerAdded, SLOW_RECHECK, StopFlag, Syncer, WatchOptions, held_place,
    held_vote_drop_only, purely_local,
};
