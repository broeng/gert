//! The registry of configured servers: per server its [`Session`], its
//! draft outbox and its connection state, plus which one is active.
//!
//! The registry is plain data owned by the shell; it does not subscribe to
//! anything itself (the shell wires the outbox events, since they need its
//! context). Every server has its own entry, so one that is offline or
//! unauthorized never takes the others with it, and its drafts keep being
//! uploaded when it is reachable even while another server is shown.
//!
//! Only one server is active at a time. The last active one is stored in the
//! cache database (`app_state`, key [`ACTIVE_SERVER_STATE`]) and passed back
//! in at the next start as `Startup::active`.

use gpui_kit::{Entity, Subscription, Task};

use crate::{
    outbox::DraftOutbox,
    session::{ServerKey, ServerSetup, Session},
};

/// The `app_state` key under which the slug of the last active server is
/// stored.
pub const ACTIVE_SERVER_STATE: &str = "active_server";

/// What is known about reaching a server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Connection {
    /// No answer yet: the password is still being read, or nothing has been
    /// fetched since the start.
    #[default]
    Connecting,
    /// The last request worked.
    Online,
    /// The server could not be reached; the cache is served.
    Offline,
    /// The server refused the credentials (401), or the password is gone.
    Unauthorized,
}

/// One configured server.
pub(crate) struct ServerEntry {
    /// Name, URL and user as configured, with the saved queries and
    /// bookmarks as they are now (settings and the dashboard change them).
    pub setup: ServerSetup,
    /// `None` when the configuration is unusable, or after the server
    /// refused the credentials.
    pub session: Option<Session>,
    /// The drafts of this server; present exactly when `session` is.
    pub outbox: Option<Entity<DraftOutbox>>,
    pub connection: Connection,
    /// The shell's subscriptions to `outbox`.
    pub outbox_subs: Vec<Subscription>,
    /// Reading the password from the keyring.
    pub credentials_task: Option<Task<()>>,
    /// Saving the bookmarks.
    pub bookmarks_task: Option<Task<()>>,
}

impl ServerEntry {
    fn new(setup: ServerSetup) -> Self {
        Self {
            setup,
            session: None,
            outbox: None,
            connection: Connection::Connecting,
            outbox_subs: Vec::new(),
            credentials_task: None,
            bookmarks_task: None,
        }
    }
}

/// The configured servers, in config order, and the active one.
pub(crate) struct Servers {
    entries: Vec<ServerEntry>,
    active: Option<ServerKey>,
}

impl Servers {
    /// `active` names the server to start on; the first one if `None` or
    /// unknown.
    pub fn new(setups: Vec<ServerSetup>, active: Option<ServerKey>) -> Self {
        let entries: Vec<ServerEntry> = setups.into_iter().map(ServerEntry::new).collect();
        let active = active
            .filter(|key| entries.iter().any(|e| e.setup.key == *key))
            .or_else(|| entries.first().map(|e| e.setup.key.clone()));
        Self { entries, active }
    }

    pub fn active_key(&self) -> Option<&ServerKey> {
        self.active.as_ref()
    }

    pub fn active(&self) -> Option<&ServerEntry> {
        self.get(self.active.as_ref()?)
    }

    pub fn active_mut(&mut self) -> Option<&mut ServerEntry> {
        let key = self.active.clone()?;
        self.get_mut(&key)
    }

    pub fn get(&self, key: &ServerKey) -> Option<&ServerEntry> {
        self.entries.iter().find(|e| e.setup.key == *key)
    }

    pub fn get_mut(&mut self, key: &ServerKey) -> Option<&mut ServerEntry> {
        self.entries.iter_mut().find(|e| e.setup.key == *key)
    }

    pub fn keys(&self) -> Vec<ServerKey> {
        self.entries.iter().map(|e| e.setup.key.clone()).collect()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Makes `key` the active server. False (and no change) if there is no
    /// such server; `changed` is whether it was not active already.
    pub fn set_active(&mut self, key: &ServerKey) -> Option<bool> {
        self.get(key)?;
        let changed = self.active.as_ref() != Some(key);
        self.active = Some(key.clone());
        Some(changed)
    }

    /// Adds a server, or replaces the setup of the one with the same key
    /// (it keeps its place, session and outbox).
    pub fn upsert(&mut self, setup: ServerSetup) -> &mut ServerEntry {
        let i = match self.entries.iter().position(|e| e.setup.key == setup.key) {
            Some(i) => {
                self.entries[i].setup = setup;
                i
            }
            None => {
                self.entries.push(ServerEntry::new(setup));
                self.entries.len() - 1
            }
        };
        &mut self.entries[i]
    }

    #[cfg(test)]
    pub fn connection(&self, key: &ServerKey) -> Option<Connection> {
        self.get(key).map(|e| e.connection)
    }

    pub fn set_connection(&mut self, key: &ServerKey, connection: Connection) {
        if let Some(e) = self.get_mut(key) {
            e.connection = connection;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::setup;

    fn server(key: &str) -> ServerSetup {
        ServerSetup {
            key: ServerKey::new(key),
            name: key.into(),
            ..setup()
        }
    }

    #[test]
    fn the_active_server_is_the_given_one_else_the_first() {
        let key = ServerKey::new;
        let list = || vec![server("a"), server("b")];
        assert_eq!(Servers::new(list(), None).active_key(), Some(&key("a")));
        assert_eq!(
            Servers::new(list(), Some(key("b"))).active_key(),
            Some(&key("b"))
        );
        // A server that was removed from the config falls back to the first.
        assert_eq!(
            Servers::new(list(), Some(key("gone"))).active_key(),
            Some(&key("a"))
        );
        assert_eq!(Servers::new(Vec::new(), Some(key("a"))).active_key(), None);
    }

    #[test]
    fn switching_reports_whether_anything_changed() {
        let key = ServerKey::new;
        let mut s = Servers::new(vec![server("a"), server("b")], None);
        assert_eq!(s.set_active(&key("a")), Some(false));
        assert_eq!(s.set_active(&key("b")), Some(true));
        assert_eq!(s.set_active(&key("nope")), None);
        assert_eq!(s.active_key(), Some(&key("b")));
        assert_eq!(s.active().map(|e| e.setup.name.as_str()), Some("b"));
    }

    #[test]
    fn connection_states_are_per_server() {
        let key = ServerKey::new;
        let mut s = Servers::new(vec![server("a"), server("b")], None);
        assert_eq!(s.connection(&key("a")), Some(Connection::Connecting));
        s.set_connection(&key("a"), Connection::Unauthorized);
        s.set_connection(&key("b"), Connection::Online);
        assert_eq!(s.connection(&key("a")), Some(Connection::Unauthorized));
        assert_eq!(s.connection(&key("b")), Some(Connection::Online));
        assert_eq!(s.connection(&key("c")), None);
    }

    #[test]
    fn upsert_keeps_the_order_and_the_other_entries() {
        let key = ServerKey::new;
        let mut s = Servers::new(vec![server("a"), server("b")], None);
        s.get_mut(&key("b")).expect("b").setup.queries = vec![crate::session::SavedQuery {
            name: "Q".into(),
            query: "q".into(),
        }];
        s.upsert(ServerSetup {
            name: "A2".into(),
            ..server("a")
        });
        s.upsert(server("c"));
        assert_eq!(s.keys(), [key("a"), key("b"), key("c")]);
        assert_eq!(s.get(&key("a")).map(|e| e.setup.name.as_str()), Some("A2"));
        assert_eq!(s.get(&key("b")).map(|e| e.setup.queries.len()), Some(1));
        assert_eq!(s.len(), 3);
    }
}
