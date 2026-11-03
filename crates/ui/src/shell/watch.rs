//! Watching servers: desktop notifications (the watch loops and what
//! their events become) and the attention counts of the servers not on
//! show.

use std::sync::Arc;

use futures::StreamExt as _;
use gpui_kit::*;
use sync::notify::{Notification, NotificationSettings, Screen};

use crate::{
    attention::{self, Polled},
    inbox::{self, InboxOp},
    nav::Route,
    notifier::Notifier,
    servers::Connection,
    session::ServerKey,
    watching::{self, Inputs, Ran},
};

use super::Shell;

/// What the notification watch does after a pass.
enum WatchStep {
    Done,
    Failed,
    /// The credentials are refused; the watch ends.
    Stop,
}

impl Shell {
    /// Turns on desktop notifications: every signed-in server is watched
    /// for the events `settings` asks for, and `notifier` shows them.
    pub fn set_notifications(
        &mut self,
        notifier: Arc<dyn Notifier>,
        settings: NotificationSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.notifier = Some(notifier);
        let (clicks, mut clicked) = futures::channel::mpsc::unbounded();
        self.clicks = Some(clicks);
        self.click_task = Some(cx.spawn_in(window, async move |this, cx| {
            while let Some(notification) = clicked.next().await {
                let opened = this.update_in(cx, |this, window, cx| {
                    this.notification_clicked(&notification, window, cx)
                });
                if opened.is_err() {
                    return;
                }
            }
        }));
        self.set_notification_settings(settings, window, cx);
    }

    /// The user clicked a notification: bring the window forward and open
    /// what it is about, the change if it names one, else the server.
    fn notification_clicked(
        &mut self,
        notification: &Notification,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.activate_window();
        let key = ServerKey::new(&notification.server);
        match notification.change {
            Some(change) => self.go_to(&key, Route::Change(change), None, window, cx),
            None => self.switch_to(&key, window, cx),
        }
    }

    /// Takes new rules: the watches follow the master switch, and the
    /// running ones read the rules again at their next pass.
    pub(crate) fn set_notification_settings(
        &mut self,
        settings: NotificationSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.notify_settings = settings;
        self.sync_notification_watches(window, cx);
    }

    /// Shows a sample notification, whatever the rules say, so that the user
    /// can see whether the desktop shows one. Clicking it opens the active
    /// server. False when notifications are not set up in this run.
    pub(crate) fn test_notification(&mut self) -> bool {
        let Some(notifier) = &self.notifier else {
            return false;
        };
        let notification = Notification {
            title: "Test notification".to_owned(),
            body: format!("This is what {} notifications look like.", self.app_name),
            server: self
                .servers
                .active_key()
                .map(ToString::to_string)
                .unwrap_or_default(),
            change: None,
        };
        let clicks = self.clicks.clone();
        let target = notification.clone();
        notifier.show(
            &notification,
            Box::new(move || {
                if let Some(clicks) = clicks {
                    clicks.unbounded_send(target).ok();
                }
            }),
        );
        true
    }

    /// Starts and stops the background polls so that exactly the servers
    /// that are not shown, are signed in and can talk to the network are
    /// polled. Cheap and idempotent: called whenever one of these changes.
    pub(super) fn sync_attention_polls(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.servers.active_key().cloned();
        for key in self.servers.keys() {
            let Some(entry) = self.servers.get(&key) else {
                continue;
            };
            let syncer = entry
                .session
                .as_ref()
                .filter(|s| s.can_sync() && s.username.is_some())
                .filter(|_| active.as_ref() != Some(&key))
                .filter(|_| entry.connection != Connection::Unauthorized)
                .map(|s| s.syncer().clone());
            let running = entry.attention_task.is_some();
            let task = match syncer {
                Some(_) if running => continue,
                Some(syncer) => Some(self.attention_poll(key.clone(), syncer, window, cx)),
                None => None,
            };
            if let Some(entry) = self.servers.get_mut(&key) {
                entry.attention_task = task;
            }
        }
        self.sync_notification_watches(window, cx);
    }

    /// Starts and stops the notification watches so that exactly the
    /// servers that are signed in and can talk to the network are watched,
    /// and only while notifications are on. Unlike the attention count this
    /// includes the active server. Cheap and idempotent, like
    /// `sync_attention_polls`, which calls it.
    fn sync_notification_watches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let on = self.notifier.is_some() && self.notify_settings.enabled;
        for key in self.servers.keys() {
            let Some(entry) = self.servers.get(&key) else {
                continue;
            };
            let syncer = entry
                .session
                .as_ref()
                .filter(|s| s.can_sync() && s.username.is_some())
                .filter(|_| on && entry.connection != Connection::Unauthorized)
                .map(|s| s.syncer().clone());
            let task = match syncer {
                Some(_) if entry.watch_task.is_some() => continue,
                Some(syncer) => Some(self.notification_watch(key.clone(), syncer, window, cx)),
                None => None,
            };
            if let Some(entry) = self.servers.get_mut(&key) {
                entry.watch_task = task;
            }
        }
    }

    /// What a pass of server `key` needs from the shell; `None` while
    /// notifications are off or the server is gone.
    fn watch_inputs(&self, key: &ServerKey) -> Option<Inputs> {
        let entry = self.servers.get(key)?;
        self.notify_settings.enabled.then(|| Inputs {
            server: key.as_str().to_owned(),
            settings: self.notify_settings.clone(),
            saved: entry
                .setup
                .queries
                .iter()
                .map(|q| (q.name.clone(), q.query.clone()))
                .collect(),
            me: entry.me.clone(),
        })
    }

    /// The task that watches server `key` for notification events until it
    /// is dropped: one pass now, then every `PollPolicy` interval (the
    /// focused one while the window has the focus, longer after failures).
    /// The rules and saved queries are read again at every pass, so the
    /// task only idles while notifications are off.
    fn notification_watch(
        &mut self,
        key: ServerKey,
        syncer: sync::Syncer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let policy = sync::PollPolicy::default();
        cx.spawn_in(window, async move |this, cx| {
            let mut failures = 0u32;
            loop {
                let Ok(inputs) = this.update(cx, |this, _| this.watch_inputs(&key)) else {
                    return;
                };
                let ran = match inputs {
                    Some(inputs) => {
                        let syncer = syncer.clone();
                        Some(
                            cx.background_spawn(async move {
                                watching::run(&syncer, &inputs, true).await
                            })
                            .await,
                        )
                    }
                    None => None,
                };
                let step = this.update_in(cx, |this, window, cx| {
                    let step = match ran {
                        Some(ran) => this.watched(&key, ran, window, cx),
                        None => WatchStep::Done,
                    };
                    (step, this.window_active)
                });
                let Ok((step, focused)) = step else { return };
                failures = match step {
                    WatchStep::Done => 0,
                    WatchStep::Failed => failures.saturating_add(1),
                    WatchStep::Stop => return,
                };
                let state = sync::PollState {
                    last_attempt: None,
                    window_focused: focused,
                    visible: true,
                    consecutive_failures: failures,
                };
                cx.background_executor()
                    .timer(policy.interval(&state))
                    .await;
            }
        })
    }

    /// The result of a pass over server `key`.
    fn watched(
        &mut self,
        key: &ServerKey,
        ran: Result<Option<Ran>, sync::Error>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> WatchStep {
        match ran {
            Ok(Some(ran)) => {
                self.found_events(key, ran, cx);
                WatchStep::Done
            }
            Ok(None) => WatchStep::Done,
            Err(e) if e.is_unauthorized() => {
                self.unauthorized_for(key, window, cx);
                WatchStep::Stop
            }
            Err(e) => {
                log::debug!("watching {key} for notifications: {e}");
                WatchStep::Failed
            }
        }
    }

    /// Shows what a refresh of one change of server `key` (the change page,
    /// an action) queued, without waiting for the next pass.
    pub(super) fn drain_notifications(&mut self, key: &ServerKey, cx: &mut Context<Self>) {
        let Some(entry) = self.servers.get(key).filter(|e| e.watch_task.is_some()) else {
            return;
        };
        let (Some(session), Some(inputs)) = (entry.session.as_ref(), self.watch_inputs(key)) else {
            return;
        };
        let syncer = session.syncer().clone();
        let key = key.clone();
        cx.spawn(async move |this, cx| {
            let ran = cx
                .background_spawn(async move { watching::run(&syncer, &inputs, false).await })
                .await;
            this.update(cx, |this, cx| match ran {
                Ok(Some(ran)) => this.found_events(&key, ran, cx),
                Ok(None) => {}
                Err(e) => log::debug!("collecting notification events of {key}: {e}"),
            })
            .ok();
        })
        .detach();
    }

    /// Decides what to show for the events a pass of server `key` found,
    /// shows it and acknowledges every event the pass saw.
    fn found_events(&mut self, key: &ServerKey, ran: Ran, cx: &mut Context<Self>) {
        let Ran { me, found } = ran;
        let current = self.current_change();
        // The watch read the account (and cached its email) before the
        // shell's own fetch did: the scanner looks again with it.
        if self.servers.get(key).is_some_and(|e| e.me.is_none()) {
            if let Some(entry) = self.servers.get_mut(key) {
                entry.me = Some(me.clone());
            }
            self.scan_local(key, cx);
            self.account_known(key, cx);
        }
        let Some(entry) = self.servers.get_mut(key) else {
            return;
        };
        entry.me = Some(me);
        let Some(syncer) = entry.session.as_ref().map(|s| s.syncer().clone()) else {
            return;
        };
        if found.seen.is_empty() {
            return;
        }
        // Acknowledged below; one that is still in flight is not shown again.
        if entry.notified.len() > 5_000 {
            entry.notified.clear();
        }
        let matched: Vec<_> = found
            .matched
            .into_iter()
            .filter(|(event, _)| entry.notified.insert(event.key()))
            .collect();
        let screen = Screen {
            focused: self.window_active,
            current: current.as_ref().map(|(k, n)| (k.as_str(), *n)),
        };
        // The zone is read once by jiff and cached.
        let now = jiff::Zoned::now().time();
        let plan = sync::notify::plan(
            &self.notify_settings,
            key.as_str(),
            matched,
            &screen,
            now,
            self.started,
        );
        if let Some(notifier) = &self.notifier {
            for notification in &plan.notifications {
                let clicks = self.clicks.clone();
                let target = notification.clone();
                notifier.show(
                    notification,
                    Box::new(move || {
                        if let Some(clicks) = clicks {
                            // The shell may be gone by now; nothing to do then.
                            clicks.unbounded_send(target).ok();
                        }
                    }),
                );
            }
        }
        // The inbox keeps every matched event, the suppressed ones too, and
        // is written before the events are acknowledged, in one
        // transaction: if it fails nothing is acknowledged, so the next
        // pass finds the events again.
        log::debug!(
            "{key}: {} notification events, {} shown",
            plan.delivered.len(),
            plan.notifications.len()
        );
        let items = inbox::items_for(&self.notify_settings, &plan.delivered);
        let (seen, keys) = (found.seen, plan.delivered.iter().map(|d| d.event.key()));
        let keys: Vec<String> = keys.collect();
        let key = key.clone();
        cx.spawn(async move |this, cx| {
            let now = jiff::Timestamp::now();
            let written = cx
                .background_spawn(async move { syncer.deliver_events(&items, &seen, now) })
                .await;
            this.update(cx, |this, cx| match written {
                Ok(0) => {}
                Ok(_) => this.inbox_op(InboxOp::Reload, cx),
                Err(e) => {
                    log::warn!("keeping notifications of {key} in the inbox failed: {e}");
                    // Not in flight any more: the next pass reports them again.
                    if let Some(entry) = this.servers.get_mut(&key) {
                        for k in &keys {
                            entry.notified.remove(k);
                        }
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// The task that polls the attention count of server `key` until it is
    /// dropped: once now, then every `PollPolicy::background` interval
    /// (longer after failures). It starts from the count in the cache.
    fn attention_poll(
        &mut self,
        key: ServerKey,
        syncer: sync::Syncer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let policy = sync::PollPolicy::background();
        cx.spawn_in(window, async move |this, cx| {
            let known = {
                let syncer = syncer.clone();
                cx.background_spawn(async move { attention::cached_count(&syncer) })
                    .await
            };
            let stored = this.update(cx, |this, cx| {
                if let Some(e) = this.servers.get_mut(&key) {
                    e.attention_count = e.attention_count.or(known);
                    cx.notify();
                }
            });
            if stored.is_err() {
                return;
            }
            let mut failures = 0u32;
            loop {
                let polled = {
                    let syncer = syncer.clone();
                    cx.background_spawn(async move { attention::poll(&syncer).await })
                        .await
                };
                failures = if polled.failed() {
                    failures.saturating_add(1)
                } else {
                    0
                };
                let stop = this.update_in(cx, |this, window, cx| {
                    this.attention_polled(&key, polled, window, cx)
                });
                if stop.unwrap_or(true) {
                    return;
                }
                let state = sync::PollState {
                    last_attempt: None,
                    window_focused: true,
                    visible: true,
                    consecutive_failures: failures,
                };
                cx.background_executor()
                    .timer(policy.interval(&state))
                    .await;
            }
        })
    }

    /// The result of a background poll of server `key`. True when polling
    /// should stop (the credentials are refused).
    fn attention_polled(
        &mut self,
        key: &ServerKey,
        polled: Polled,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // The background poll is the cadence of the servers not shown; the
        // scan needs no network, so it runs whatever the poll found.
        self.scan_local(key, cx);
        match polled {
            Polled::Count(n) => {
                if let Some(e) = self.servers.get_mut(key) {
                    e.attention_count = Some(n);
                }
                self.server_reachable(key, window, cx);
            }
            // It answered, so it is reachable even without a result.
            Polled::Refused => self.server_reachable(key, window, cx),
            Polled::Offline => self.servers.set_connection(key, Connection::Offline),
            Polled::Unauthorized => {
                self.unauthorized_for(key, window, cx);
                return true;
            }
        }
        cx.notify();
        false
    }
}
