//! The per-file "reviewed" mark (`r`), shared by the change page and the
//! diff page.
//!
//! The pages flip the mark in memory at once, then send the request on the
//! background executor with [`send`]. `Syncer::set_reviewed` keeps the cache
//! in step and, once no request for the mark is open, holds what the server
//! has; `FileList` shows that. A 401 goes to the login flow, any other
//! failure (a 403 included, which must not log the user out) becomes a
//! short note.

use model::{ChangeNumber, RevisionId};
use sync::Syncer;

use crate::session::{Session, is_unauthorized};

/// Why `r` does nothing on a local version: the marks are Gerrit's, and
/// it has no place for them until the version is uploaded.
pub const LOCAL: &str = "Reviewed marks are kept by Gerrit; available once uploaded";

/// Why `r` waits: the list on show is not Gerrit's yet, and Gerrit's,
/// computed before the mark, would replace it.
pub const WAITS: &str = "Reviewed marks come with Gerrit's file list";

/// Why the mark cannot be set now, as a note for the user.
pub fn unavailable(session: &Session) -> Option<&'static str> {
    if !session.can_comment() {
        Some("Sign in to mark files reviewed")
    } else if !session.can_upload() {
        Some("Still signing in, try again in a moment")
    } else {
        None
    }
}

/// What a mark request came to: its own answer, and what the mark settled
/// to once no request for it is open (see `Syncer::set_reviewed`).
pub struct Answer {
    /// The server answered 401: back to the login flow.
    pub unauthorized: bool,
    /// Why the request failed otherwise.
    pub error: Option<String>,
    pub settled: Option<sync::MarkSettled>,
}

/// Sets or clears the mark on the server and in the cache.
pub async fn send(
    syncer: Syncer,
    change: ChangeNumber,
    revision: RevisionId,
    path: String,
    reviewed: bool,
) -> Answer {
    let sent = syncer
        .set_reviewed(change, &revision, &path, reviewed)
        .await;
    let (unauthorized, error) = match &sent.result {
        Ok(()) => (false, None),
        Err(e) if is_unauthorized(e) => (true, None),
        Err(e) => (false, Some(e.to_string())),
    };
    Answer {
        unauthorized,
        error,
        settled: sent.settled,
    }
}

/// The note for a failed request.
pub fn failure_note(path: &str, reviewed: bool, error: &str) -> String {
    let what = if reviewed {
        "mark as reviewed"
    } else {
        "clear the reviewed mark of"
    };
    format!("Could not {what} {path}: {error}")
}
