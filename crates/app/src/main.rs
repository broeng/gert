use gpui_kit::*;

/// Single source of the (working) application name.
pub const APP_NAME: &str = "gert";

fn main() {
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        if let Err(err) = gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| {
            cx.new(|_| ui::RootView)
        }) {
            eprintln!("{APP_NAME}: failed to open window: {err:#}");
            cx.quit();
        }
    });
}
