//! The comments of one change, for the pages that show them.
//!
//! [`ChangeComments`] reads the cache first (so threads are there at once,
//! also offline), then refreshes from the server. It holds the change's
//! threads built from published comments and the user's drafts, and tells
//! its observers when they changed. Pages hand the threads to their diff
//! views; see `DiffView::set_threads`.

use std::sync::Arc;

use gpui_kit::*;
use model::{ChangeNumber, Thread};

use crate::session::{Session, is_unauthorized};

pub enum CommentsEvent {
    /// The threads changed (loaded from the cache, or refreshed).
    Changed,
    /// The server answered 401: back to the login flow. (A 403 only logs.)
    Unauthorized,
}

pub struct ChangeComments {
    session: Session,
    change: ChangeNumber,
    threads: Arc<Vec<Thread>>,
    refreshing: bool,
    /// Another refresh was asked for while one was running.
    again: bool,
    task: Option<Task<()>>,
}

impl EventEmitter<CommentsEvent> for ChangeComments {}

impl ChangeComments {
    /// Reads the cache and, if the session can reach the server, refreshes.
    pub fn new(session: Session, change: ChangeNumber, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            session,
            change,
            threads: Arc::default(),
            refreshing: false,
            again: false,
            task: None,
        };
        this.load(cx);
        this
    }

    pub fn threads(&self) -> Arc<Vec<Thread>> {
        self.threads.clone()
    }

    /// Fetches the comments again (a refresh already running is followed by
    /// one more, so a change that happened meanwhile is not missed).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if !self.session.can_sync() {
            return;
        }
        if self.refreshing {
            self.again = true;
            return;
        }
        self.refreshing = true;
        let syncer = self.session.syncer().clone();
        let change = self.change;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { syncer.refresh_comments(change).await })
                .await;
            this.update(cx, |this, cx| {
                this.refreshing = false;
                match result {
                    Ok(view) => this.apply(view.threads(), cx),
                    Err(e) if is_unauthorized(&e) => cx.emit(CommentsEvent::Unauthorized),
                    Err(e) => log::warn!("refreshing comments of change {change}: {e}"),
                }
                if std::mem::take(&mut this.again) {
                    this.refresh(cx);
                }
            })
            .ok();
        }));
    }

    /// The change's `updated` time moved (a poll saw it): its comments may
    /// have too.
    pub fn change_updated(&mut self, cx: &mut Context<Self>) {
        self.refresh(cx);
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let syncer = self.session.syncer().clone();
        let change = self.change;
        self.refreshing = self.session.can_sync();
        self.task = Some(cx.spawn(async move |this, cx| {
            let cached = cx
                .background_spawn(async move { syncer.cached_comments(change) })
                .await;
            this.update(cx, |this, cx| {
                match cached {
                    Ok(view) => this.apply(view.threads(), cx),
                    Err(e) => log::warn!("reading cached comments of change {change}: {e}"),
                }
                this.refreshing = false;
                this.again = false;
                this.refresh(cx);
            })
            .ok();
        }));
    }

    fn apply(&mut self, threads: Vec<Thread>, cx: &mut Context<Self>) {
        if *self.threads != threads {
            self.threads = Arc::new(threads);
            cx.emit(CommentsEvent::Changed);
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use gerrit_api::{Auth, fake::RouteHttpClient};
    use gpui_kit::{AppContext as _, Entity, TestAppContext};
    use model::{ChangeNumber, Location};
    use store::Store;

    use super::{ChangeComments, CommentsEvent};
    use crate::{
        diff::tests::threads::comment,
        session::Session,
        tests::{URL, env_with, setup},
    };

    fn json(ids: &[&str]) -> String {
        let one = |id: &&str| {
            format!(
                r#"{{"id":"{id}","patch_set":1,"line":3,"message":"m","updated":"2024-06-15 10:00:00.000000000","unresolved":true}}"#
            )
        };
        let all: Vec<_> = ids.iter().map(one).collect();
        format!(r#"{{"a.rs":[{}]}}"#, all.join(","))
    }

    fn entity(
        cx: &mut TestAppContext,
        store: Store,
        http: &RouteHttpClient,
        auth: Option<Auth>,
    ) -> Entity<ChangeComments> {
        let env = env_with(store, http);
        let session = Session::new(&env, &setup(), auth).expect("session");
        cx.new(|cx| ChangeComments::new(session, ChangeNumber(7), cx))
    }

    fn count(http: &RouteHttpClient) -> usize {
        http.requested()
            .iter()
            .filter(|r| r.contains("/comments"))
            .count()
    }

    fn cached_store() -> Store {
        let mut store = Store::open_in_memory().expect("store");
        let server = store.server_id(URL).expect("server");
        let mut cached = comment("cached", Location::Line(3), "from the cache", true);
        cached.path = "a.rs".into();
        store
            .save_comments(server, ChangeNumber(7), &[cached], jiff::Timestamp::now())
            .expect("save");
        store
    }

    fn ids(cx: &mut TestAppContext, c: &Entity<ChangeComments>) -> Vec<String> {
        cx.update(|cx| {
            c.read(cx)
                .threads()
                .iter()
                .map(|t| format!("{:?}", t.id))
                .collect()
        })
    }

    fn count_changes(cx: &mut TestAppContext, c: &Entity<ChangeComments>) -> Arc<Mutex<usize>> {
        let n = Arc::new(Mutex::new(0));
        let seen = n.clone();
        cx.update(|cx| {
            cx.subscribe(c, move |_, ev: &CommentsEvent, _| {
                if matches!(ev, CommentsEvent::Changed) {
                    *seen.lock().expect("count") += 1;
                }
            })
            .detach();
        });
        n
    }

    #[gpui_kit::test]
    fn without_credentials_only_the_cache_is_read(cx: &mut TestAppContext) {
        let http = RouteHttpClient::new();
        http.route_json("/comments", &json(&["one"]));
        let c = entity(cx, cached_store(), &http, None);
        cx.run_until_parked();
        assert_eq!(count(&http), 0);
        assert_eq!(ids(cx, &c), [r#"Comment("cached")"#]);
    }

    #[gpui_kit::test]
    fn the_server_replaces_the_cache_and_a_moved_change_refreshes(cx: &mut TestAppContext) {
        let http = RouteHttpClient::new();
        http.route_json("/comments", &json(&["one"]));
        let c = entity(cx, cached_store(), &http, Some(Auth::Anonymous));
        let changes = count_changes(cx, &c);
        cx.run_until_parked();
        assert_eq!(count(&http), 1);
        assert_eq!(ids(cx, &c), [r#"Comment("one")"#]);

        // The change moved: fetched again; an unchanged answer is no news.
        let before = *changes.lock().expect("count");
        cx.update(|cx| c.update(cx, |c, cx| c.change_updated(cx)));
        cx.run_until_parked();
        assert_eq!(count(&http), 2);
        assert_eq!(*changes.lock().expect("count"), before);
        http.route_json("/comments", &json(&["one", "two"]));
        cx.update(|cx| c.update(cx, |c, cx| c.change_updated(cx)));
        cx.run_until_parked();
        assert_eq!(ids(cx, &c).len(), 2);
        assert_eq!(*changes.lock().expect("count"), before + 1);
    }

    /// Whether a comments request answered with `status` asks for a login.
    fn asks_for_login(cx: &mut TestAppContext, status: u16) -> bool {
        let http = RouteHttpClient::new();
        http.route("/comments", status, "no");
        let auth = Auth::Basic {
            username: "u".into(),
            password: "p".into(),
        };
        let store = Store::open_in_memory().expect("store");
        let c = entity(cx, store, &http, Some(auth));
        let seen = Arc::new(Mutex::new(false));
        let flag = seen.clone();
        cx.update(|cx| {
            cx.subscribe(&c, move |_, ev: &CommentsEvent, _| {
                if matches!(ev, CommentsEvent::Unauthorized) {
                    *flag.lock().expect("flag") = true;
                }
            })
            .detach();
        });
        cx.run_until_parked();
        *seen.lock().expect("flag")
    }

    #[gpui_kit::test]
    fn only_a_401_asks_for_a_new_login(cx: &mut TestAppContext) {
        assert!(asks_for_login(cx, 401));
        assert!(!asks_for_login(cx, 403));
        assert!(!asks_for_login(cx, 500));
    }
}
