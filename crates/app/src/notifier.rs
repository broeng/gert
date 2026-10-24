//! Desktop notifications over `notify-rust`.
//!
//! Linux talks to the notification daemon over D-Bus (the pure-Rust zbus
//! backend), macOS and Windows use their own services. Sending and waiting
//! for a click both block, so every notification gets a short-lived thread
//! of its own and the UI thread is never held up.
//!
//! Clicking a notification opens its change on Linux only: macOS and
//! Windows show the notification but do not report clicks back through
//! `notify-rust`.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use sync::notify::Notification;
use ui::{Clicked, Notifier};

use crate::APP_NAME;

/// Shows notifications through the platform's notification service. If
/// there is none (no daemon is running), the first failure is logged as a
/// warning and the rest quietly.
#[derive(Clone, Default)]
pub struct DesktopNotifier {
    warned: Arc<AtomicBool>,
}

impl DesktopNotifier {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Notifier for DesktopNotifier {
    fn show(&self, notification: &Notification, clicked: Clicked) {
        let title = notification.title.clone();
        let body = notification.body.clone();
        let warned = self.warned.clone();
        let spawned = std::thread::Builder::new()
            .name("notification".into())
            .spawn(move || deliver(&title, &body, clicked, &warned));
        if let Err(e) = spawned {
            log::warn!("could not start a thread for a notification: {e}");
        }
    }
}

/// Sends one notification and, on Linux, waits until it is clicked or
/// closed.
fn deliver(title: &str, body: &str, clicked: Clicked, warned: &AtomicBool) {
    let mut n = notify_rust::Notification::new();
    n.appname(APP_NAME);
    n.summary(title);
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // The default action is the click on the notification itself.
        n.body(&escape_markup(body));
        n.action("default", "Open");
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    n.body(body);
    match n.show() {
        Ok(handle) => {
            #[cfg(all(unix, not(target_os = "macos")))]
            handle.wait_for_action(move |action| {
                if action == "default" {
                    clicked();
                }
            });
            #[cfg(not(all(unix, not(target_os = "macos"))))]
            {
                let _ = (handle, clicked);
            }
        }
        Err(e) => {
            if !warned.swap(true, Ordering::Relaxed) {
                log::warn!("showing notifications failed (is a notification daemon running?): {e}");
            } else {
                log::debug!("showing a notification failed: {e}");
            }
        }
    }
}

/// The text for an XDG notification body, which is a small markup
/// language: `<` and `&` in a change subject must not be read as tags.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_characters_are_escaped() {
        assert_eq!(escape_markup("a <b> & c"), "a &lt;b&gt; &amp; c");
    }
}
