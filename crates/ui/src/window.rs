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
//!   root draws a 1px frame and provides the resize edges. The shell asks
//!   for no shadow: a shadow is a transparent inset around the content, and
//!   some drivers and compositors (NVIDIA's proprietary Wayland driver)
//!   do not composite the surface's alpha, so the inset showed as a wide
//!   opaque frame. Requesting a transparent window does not help, since
//!   GPUI already treats client-decorated Wayland windows as transparent,
//!   and it turns off subpixel text, which needs an opaque window. So the
//!   window stays `Opaque` and the frame is exactly the 1px border.
//! - **macOS:** native traffic lights over a transparent title bar; our bar
//!   sits beside them and draws no buttons.
//! - **Windows:** the buttons are drawn, but the OS hit-tests them through
//!   `WindowControlArea`, so dragging, snapping and the maximize flyout are
//!   native.

use gpui_kit::component::TitleBar;
use gpui_kit::*;

/// The smallest window the layout works in.
const MIN_SIZE: Size<Pixels> = size(px(640.), px(400.));

gpui_kit::assets::icon_assets!(ExtraIcons, [Trash]);

/// The default icon set plus the few icons of our own that it lacks.
struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        match ExtraIcons.load(path)? {
            Some(bytes) => Ok(Some(bytes)),
            None => gpui_kit::assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

/// The GPUI application with the icon set loaded. The title bar's window
/// buttons are icons, so every binary that shows the shell must start from
/// this.
pub fn application() -> Application {
    gpui_kit::application().with_assets(AppAssets)
}

/// Options of the main window: maximized, `size` big when restored and
/// centered then, `title` as its title and `app_id` as the Wayland app id /
/// X11 class, and set up for our own [`TitleBar`] (see the module docs).
pub fn window_options(title: &str, app_id: &str, size: Size<Pixels>, cx: &App) -> WindowOptions {
    let mut titlebar = TitleBar::title_bar_options();
    titlebar.title = Some(title.to_owned().into());
    WindowOptions {
        window_bounds: Some(WindowBounds::Maximized(
            WindowBounds::centered(size, cx).get_bounds(),
        )),
        titlebar: Some(titlebar),
        window_min_size: Some(MIN_SIZE),
        app_id: Some(app_id.to_owned()),
        ..TitleBar::window_options()
    }
}

/// How long the window manager gets to maximize a new window before
/// [`ensure_maximized`] asks again.
const MAXIMIZE_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// Asks again for maximizing a window that was opened with
/// [`WindowBounds::Maximized`] but is not maximized after a moment.
///
/// GPUI asks for it while it creates the window: on Wayland that is
/// `xdg_toplevel.set_maximized` right after the first commit, which
/// compositors honour. On X11 it is an `_NET_WM_STATE` client message sent
/// *before* the window is mapped, which EWMH says a window manager need not
/// look at. A window manager that did not maximize (or never does, like a
/// tiling one) is not harmed by a second request; one that did is left
/// alone, since it is maximized by then.
pub fn ensure_maximized(window: AnyWindowHandle, cx: &mut App) {
    cx.spawn(async move |cx| {
        cx.background_executor().timer(MAXIMIZE_GRACE).await;
        cx.update(|cx| {
            window
                .update(cx, |_, window, _| {
                    if !window.is_maximized() && !window.is_fullscreen() {
                        window.zoom_window();
                    }
                })
                .ok();
        });
    })
    .detach();
}
