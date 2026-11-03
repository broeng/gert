//! The Submit warning (the user's decision): before a Gerrit change is
//! submitted, the checkout is asked whether it has a newer version of the
//! change that is not on Gerrit, and if so the confirmation says so.
//!
//! The scanner is asked for a fresh look at the project
//! ([`LocalScanner::scan_now`]), but the user is not kept waiting for it:
//! after [`WAIT`] the store's last answer counts. When the project is not
//! scanned at all (the option is off, no usable checkout) there is no
//! warning.

use std::time::Duration;

use futures::future::Either;
use gpui_kit::{AppContext as _, AsyncApp, Task};
use model::{ChangeNumber, Timestamp};
use store::ReconcileReport;
use sync::Syncer;

use crate::local_scan::ScanNowError;

/// How long the warning waits for a fresh scan before it goes by the store.
pub const WAIT: Duration = Duration::from_secs(2);

/// The newest version of Gerrit change `change` that only the checkout has
/// and that came after its current patch set (created at `since`), by its
/// name ("local 2": 2), once `scan` answered or after `wait`; `None` when
/// there is none, or the project is not scanned for local changes. Errors
/// of the scan or the store are logged and count as what the store has,
/// or as none.
pub async fn newer_local_version(
    scan: Task<Result<ReconcileReport, ScanNowError>>,
    wait: Duration,
    syncer: Syncer,
    change: ChangeNumber,
    since: Option<Timestamp>,
    cx: &mut AsyncApp,
) -> Option<u32> {
    let timer = cx.background_executor().timer(wait);
    match futures::future::select(std::pin::pin!(scan), std::pin::pin!(timer)).await {
        Either::Left((Ok(_), _)) => {}
        Either::Left((Err(e @ (ScanNowError::Inactive | ScanNowError::NotScanned(_))), _)) => {
            log::debug!("no Submit warning for {change}: {e}");
            return None;
        }
        Either::Left((Err(e), _)) => {
            log::warn!("looking for local versions of {change} before submitting: {e}");
        }
        Either::Right(_) => {
            log::info!(
                "the checkout did not answer in {wait:?}; the Submit warning goes by the cache"
            );
        }
    }
    let found = cx
        .background_spawn(async move { syncer.newer_local_version(change, since) })
        .await;
    match found {
        Ok(v) => v.map(|v| v.ordinal),
        Err(e) => {
            log::warn!("reading the local versions of {change}: {e}");
            None
        }
    }
}
