//! The per-file "reviewed" mark (`r`), shared by the change page and the
//! diff page.
//!
//! The pages flip the mark in memory at once, then send the request on the
//! background executor with [`send`]. `Syncer::set_reviewed` keeps the cache
//! in step and puts it back when the request fails; the page does the same
//! for what it shows. A 401 goes to the login flow, any other failure (a 403
//! included, which must not log the user out) becomes a short note.

use model::{ChangeNumber, RevisionId};
use sync::Syncer;

use crate::session::{Session, is_unauthorized};

pub enum Outcome {
    Done,
    Unauthorized,
    Failed(String),
}

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

/// Sets or clears the mark on the server and in the cache.
pub async fn send(
    syncer: Syncer,
    change: ChangeNumber,
    revision: RevisionId,
    path: String,
    reviewed: bool,
) -> Outcome {
    match syncer
        .set_reviewed(change, &revision, &path, reviewed)
        .await
    {
        Ok(()) => Outcome::Done,
        Err(e) if is_unauthorized(&e) => Outcome::Unauthorized,
        Err(e) => Outcome::Failed(e.to_string()),
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
