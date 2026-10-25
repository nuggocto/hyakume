//! `hyakume-desktop`: the native GPUI app. Only this crate depends on GPUI.
//! For now it opens a placeholder window; the canvas comes in a later step.

use gpui::{
    App, Application, Bounds, Context, Window, WindowBounds, WindowOptions, div, prelude::*, px,
    rgb, size,
};

/// Wayland app id and X11 class, used by window managers for rules and icons.
const APP_ID: &str = "hyakume";

struct Root;

impl Render for Root {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .justify_center()
            .items_center()
            .bg(rgb(0x202030))
            .text_color(rgb(0xffffff))
            .child("Hyakume")
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(800.), px(600.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            app_id: Some(APP_ID.into()),
            ..Default::default()
        };
        let opened = cx.open_window(options, |window, cx| {
            // GPUI 0.2.2 ignores TitlebarOptions::title on Wayland, so set it here.
            window.set_window_title("Hyakume");
            cx.new(|_| Root)
        });
        if let Err(err) = opened {
            eprintln!("error: cannot open a window: {err:#}");
            cx.quit();
            return;
        }
        cx.activate(true);
    });
}
