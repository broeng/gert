//! Where notifications leave the app. The shell decides what to show and
//! when (see `watching`); a [`Notifier`] only puts it on the desktop.

use sync::notify::Notification;

/// Called when the user clicks a notification. May run on any thread.
pub type Clicked = Box<dyn FnOnce() + Send + 'static>;

/// Shows notifications on the desktop. Implemented in `app` over the
/// platform's notification service; tests use a recording fake.
pub trait Notifier: Send + Sync {
    /// Shows `notification` and returns at once: it must never block the
    /// caller, which is the UI thread. `clicked` runs if the user clicks
    /// it, on platforms where that can be known.
    fn show(&self, notification: &Notification, clicked: Clicked);
}
