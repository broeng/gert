//! The Submit warning (the user's decision): before a Gerrit change is
//! submitted, the checkout is asked whether it has a newer version of the
//! change, or of an open change below it that the Submit takes along, that
//! is not on Gerrit; if so the confirmation says so.
//!
//! The scanner is asked for a fresh look at the project
//! ([`LocalScanner::scan_now`](crate::local_scan::LocalScanner::scan_now)),
//! whose commits tell which versions are still on a branch. The user is not
//! kept waiting for it: after [`WAIT`], or when the scan fails or the
//! checkout cannot be opened now, the store's last answer counts
//! ([`sync::Syncer::newer_local_version`]). The shell hands a scanner over
//! only where the project has the `local_changes` option, so without one
//! (or without a session) there is no warning.

use std::time::Duration;

use futures::future::Either;
use gpui_kit::{AppContext as _, AsyncApp, Task};
use model::{ChangeNumber, RevisionId};
use sync::Syncer;

use crate::local_scan::{ScanNowError, ScanNowReport};

/// How long the warning waits for a fresh scan before it goes by the store.
pub const WAIT: Duration = Duration::from_secs(2);

/// The changes a Submit of Gerrit change `change` at patch set `revision`
/// would take whose newest version is only in the checkout: `change`
/// itself and the open changes below it in its cached relation chain
/// ([`Syncer::newer_local_versions`]), each with that version's name
/// ("local 2": 2), once `scan` answered or after `wait`. Empty when there
/// are none, or the server is not scanned at all. Errors of the scan or the
/// store are logged and count as what the store has, or as none.
pub async fn newer_local_versions(
    scan: Task<Result<ScanNowReport, ScanNowError>>,
    wait: Duration,
    syncer: Syncer,
    change: ChangeNumber,
    revision: Option<RevisionId>,
    cx: &mut AsyncApp,
) -> Vec<(ChangeNumber, u32)> {
    let timer = cx.background_executor().timer(wait);
    let on_branches =
        match futures::future::select(std::pin::pin!(scan), std::pin::pin!(timer)).await {
            Either::Left((Ok(answer), _)) => answer.commits,
            Either::Left((Err(ScanNowError::Inactive), _)) => {
                log::debug!("no Submit warning for {change}: the server is not scanned");
                return Vec::new();
            }
            Either::Left((Err(e), _)) => {
                log::warn!("looking for local versions of {change} before submitting: {e}");
                None
            }
            Either::Right(_) => {
                log::info!(
                    "the checkout did not answer in {wait:?}; the Submit warning goes by the cache"
                );
                None
            }
        };
    let found = cx
        .background_spawn(async move {
            let on_branches = on_branches.as_deref();
            match &revision {
                Some(rev) => syncer.newer_local_versions(change, rev, on_branches),
                None => Ok(syncer
                    .newer_local_version(change, on_branches)?
                    .map(|v| (change, v))
                    .into_iter()
                    .collect()),
            }
        })
        .await;
    match found {
        Ok(v) => v.into_iter().map(|(n, v)| (n, v.ordinal)).collect(),
        Err(e) => {
            log::warn!("reading the local versions of {change}: {e}");
            Vec::new()
        }
    }
}
