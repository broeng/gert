//! The background count of the servers that are not shown: how many changes
//! have the user in their attention set, from one cheap query per server and
//! a few minutes (see [`sync::PollPolicy::background`]).
//!
//! The query goes through the same cache as the pages' queries, so the last
//! count is still there at the next start and the count shows at once.

use sync::Syncer;

/// The query behind the count.
pub(crate) const ATTENTION_QUERY: &str = "attention:self";

/// How many changes one poll fetches. The count is the number of cached
/// changes, so it stops at this on a server where someone is needed on more.
const PAGE_SIZE: u32 = 100;

/// What one poll found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Polled {
    /// The server answered: this many changes want the user's attention.
    Count(usize),
    /// The server could not be reached.
    Offline,
    /// The server answered, but not with a result (403, 5xx, ...). It is
    /// reachable; the next poll tries again.
    Refused,
    /// The server refused the credentials (401). Polling stops.
    Unauthorized,
}

impl Polled {
    /// Whether the poll failed, which makes the next one wait longer.
    pub fn failed(self) -> bool {
        !matches!(self, Self::Count(_))
    }

    fn of_error(e: &sync::Error) -> Self {
        if e.is_unauthorized() {
            Self::Unauthorized
        } else if e.is_refusal() || matches!(e, sync::Error::Store(_)) {
            Self::Refused
        } else {
            Self::Offline
        }
    }
}

/// The count as of the last poll, possibly from an earlier run. Blocks on
/// SQLite: call it from the background executor.
pub(crate) fn cached_count(syncer: &Syncer) -> Option<usize> {
    syncer
        .cached_query(ATTENTION_QUERY)
        .ok()
        .flatten()
        .map(|c| c.changes.len())
}

/// Asks the server (and updates the cache). Blocks on SQLite as well.
pub(crate) async fn poll(syncer: &Syncer) -> Polled {
    match syncer.refresh_query(ATTENTION_QUERY, PAGE_SIZE).await {
        Ok(update) => Polled::Count(update.cached_count),
        Err(e) => {
            log::debug!("polling {ATTENTION_QUERY}: {e}");
            Polled::of_error(&e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_sorted_by_what_they_say_about_the_server() {
        let api = sync::Error::Api;
        assert_eq!(
            Polled::of_error(&api(gerrit_api::Error::Unauthorized)),
            Polled::Unauthorized
        );
        assert_eq!(
            Polled::of_error(&api(gerrit_api::Error::Forbidden {
                message: "no".into()
            })),
            Polled::Refused
        );
        assert_eq!(
            Polled::of_error(&api(gerrit_api::Error::Status {
                code: 400,
                body: String::new()
            })),
            Polled::Refused
        );
        assert!(Polled::Offline.failed() && !Polled::Count(0).failed());
    }
}
