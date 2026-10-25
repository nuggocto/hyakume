//! `hyakume-desktop`: the native GPUI app. Only this crate depends on GPUI.
//! It shows local work and every configured server's workspaces on one canvas.

mod app;
mod canvas;
mod clock;
mod glyphs;
mod inspector;
mod layout;
mod meme;
mod runtime;
mod scene;
mod sources;
mod theme;

use gpui::{App, Application, Bounds, WindowBounds, WindowOptions, prelude::*, px, size};

use std::rc::Rc;

use time::OffsetDateTime;

use crate::app::Hyakume;
use crate::clock::LocalTime;
use crate::runtime::Runtime;

/// Wayland app id and X11 class, used by window managers for rules and icons.
const APP_ID: &str = "hyakume";

fn main() {
    // Sampled while this is still the only thread; see clock.rs for why.
    let local = Rc::new(LocalTime::sample(OffsetDateTime::now_utc()));
    let (runtime, updates) = match Runtime::start() {
        Ok(started) => started,
        Err(err) => {
            eprintln!("error: cannot start the background thread: {err}");
            std::process::exit(1);
        }
    };
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            app_id: Some(APP_ID.into()),
            ..Default::default()
        };
        let opened = cx.open_window(options, |window, cx| {
            // GPUI 0.2.2 ignores TitlebarOptions::title on Wayland, so set it here.
            window.set_window_title("Hyakume");
            cx.new(|cx| Hyakume::new(runtime, updates, local, window, cx))
        });
        if let Err(err) = opened {
            eprintln!("error: cannot open a window: {err:#}");
            cx.quit();
            return;
        }
        cx.on_window_closed(|cx| cx.quit()).detach();
        cx.activate(true);
    });
}
