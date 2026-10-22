//! Window setup shared by the app and the demo examples: the application
//! (with the icon assets the title bar's buttons need) and the window
//! options.
//!
//! # Decorations
//!
//! We follow what gpui-component's `TitleBar` and the GPUI window-shadow
//! example do, and let the platform decide:
//!
//! - **Linux:** GPUI asks for server-side decorations (the default when
//!   [`WindowOptions::window_decorations`] is unset). A Wayland compositor
//!   or X11 window manager that draws them keeps doing so, and our bar
//!   is then a plain bar without window buttons. If the platform says
//!   client-side instead (GNOME's Mutter has no server-side decorations
//!   on Wayland, X11 without a compositor cannot do client-side), the
//!   title bar draws minimize, maximize/restore and close, moves the window
//!   when dragged, toggles maximize on a double click, and the
//!   [`window_border`](gpui_kit::component::window_border) around the
//!   root draws the frame and shadow and provides the resize edges.
//! - **macOS:** native traffic lights over a transparent title bar; our bar
//!   sits beside them and draws no buttons.
//! - **Windows:** the buttons are drawn, but the OS hit-tests them through
//!   `WindowControlArea`, so dragging, snapping and the maximize flyout are
//!   native.

use gpui_kit::component::TitleBar;
use gpui_kit::*;

/// The smallest window the layout works in.
const MIN_SIZE: Size<Pixels> = size(px(640.), px(400.));

/// The GPUI application with the default icon set loaded. The title bar's
/// window buttons are icons, so every binary that shows the shell must
/// start from this.
pub fn application() -> Application {
    gpui_kit::application().with_assets(gpui_kit::assets::Assets)
}

/// Options of the main window: centered, `size` big, `title` as its title
/// and `app_id` as the Wayland app id / X11 class, and set up for our own
/// [`TitleBar`] (see the module docs).
pub fn window_options(title: &str, app_id: &str, size: Size<Pixels>, cx: &App) -> WindowOptions {
    let mut titlebar = TitleBar::title_bar_options();
    titlebar.title = Some(title.to_owned().into());
    WindowOptions {
        window_bounds: Some(WindowBounds::centered(size, cx)),
        titlebar: Some(titlebar),
        window_min_size: Some(MIN_SIZE),
        app_id: Some(app_id.to_owned()),
        // The client-side frame's shadow inset is transparent pixels. On an
        // opaque surface the GPU ignores alpha and shows them as a solid
        // frame. Elsewhere the shell paints every pixel, so the bit is only
        // needed where `window_border` draws a shadow.
        window_background: if cfg!(target_os = "linux") {
            WindowBackgroundAppearance::Transparent
        } else {
            WindowBackgroundAppearance::Opaque
        },
        ..TitleBar::window_options()
    }
}
