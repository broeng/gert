//! A short note a page shows for a few seconds: why a key did nothing, or
//! that a request failed. The diff view has its own strip for its keys; this
//! is for the pages' keys (`r`, `a`), which have no view to say it in.

use std::time::Duration;

use gpui_kit::*;

/// How long a note stays.
const SHOWN_FOR: Duration = Duration::from_secs(6);

#[derive(Default)]
pub struct Hint {
    current: Option<Note>,
    task: Option<Task<()>>,
}

#[derive(Clone)]
struct Note {
    text: SharedString,
    error: bool,
}

impl Hint {
    /// Shows `text` (red when `error`) for a few seconds. `get` finds the
    /// hint again in the owner when the time is up.
    pub fn say<T: 'static>(
        &mut self,
        text: impl Into<SharedString>,
        error: bool,
        cx: &mut Context<T>,
        get: fn(&mut T) -> &mut Hint,
    ) {
        self.current = Some(Note {
            text: text.into(),
            error,
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SHOWN_FOR).await;
            this.update(cx, |this, cx| {
                get(this).current = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The note and whether it is an error.
    pub fn current(&self) -> Option<(&str, bool)> {
        self.current.as_ref().map(|n| (n.text.as_ref(), n.error))
    }
}
