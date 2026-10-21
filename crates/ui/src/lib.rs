//! GPUI views. Built on gpui-kit; no I/O here.

use gpui_kit::component::*;
use gpui_kit::*;

/// Top-level view of the main window.
pub struct RootView;

impl Render for RootView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .child("Gert")
    }
}
