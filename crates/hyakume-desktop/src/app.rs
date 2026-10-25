//! The window's root view: owns what's loaded, the camera, and what is
//! focused or selected, and turns pointer and key input into changes.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, Bounds, ClickEvent, Context, CursorStyle, FocusHandle, FontWeight, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollWheelEvent,
    SharedString, Subscription, Task, Window, canvas, div, prelude::*, px,
};
use tokio::sync::mpsc;

use crate::canvas::{self, Camera, Frame, Hit};
use crate::clock::LocalTime;
use crate::inspector::{self, Detail};
use crate::layout::{COLUMN_HEAD, Layout, Placement, Vec2};
use crate::meme;
use crate::runtime::{Command, Runtime, Update};
use crate::scene::{NodeKey, Scene, Shape, ThreadKind};
use crate::sources::{self, SourceKey, SourceState, Status};
use crate::theme::Theme;

const HEADER_H: f32 = 64.0;
/// Below this canvas width the legend no longer fits beside the zoom
/// controls, so it moves to its own row above them.
const ROOMY_FOR_LEGEND: f32 = 1100.0;
/// The same for the shorter hint shown while a project is focused.
const ROOMY_FOR_HINT: f32 = 820.0;
/// Rough width of the legend's contents, to guess how many rows it wraps to.
const LEGEND_W: f32 = 760.0;

/// The inspector gives up some width in small windows so the canvas stays usable.
fn inspector_width(window_w: f32) -> f32 {
    (window_w * 0.42).clamp(300.0, 400.0)
}
/// A press that moves less than this is a click, not a pan.
const CLICK_SLOP: f32 = 4.0;

struct Drag {
    start: Point<Pixels>,
    last: Point<Pixels>,
    moved: bool,
}

pub struct Hyakume {
    runtime: Runtime,
    /// The local time zone as sampled at startup; comment times use it.
    local: Rc<LocalTime>,
    theme: Theme,
    sources: Vec<SourceState>,
    scene: Rc<Scene>,
    layout: Layout,
    placement: Rc<Placement>,
    camera: Camera,
    /// Where the overview was before focusing, so Escape goes back there.
    overview_camera: Option<(Camera, bool)>,
    focus: Option<NodeKey>,
    stubs: Rc<Vec<(usize, Vec2)>>,
    selected: Option<NodeKey>,
    details: HashMap<NodeKey, Detail>,
    /// Number of the newest detail request; answers to older ones are dropped.
    detail_request: u64,
    drag: Option<Drag>,
    /// Set while painting, read by input handlers to turn window coordinates
    /// into canvas coordinates.
    canvas_bounds: Rc<Cell<Bounds<Pixels>>>,
    focus_handle: FocusHandle,
    /// Until the person pans, zooms, or focuses, the view keeps fitting
    /// everything: after the first answers, new projects, and window resizes.
    user_moved: bool,
    fitted_for: Option<Vec2>,
    /// Canvas size at the last render, to notice resizes.
    last_canvas: Option<Vec2>,
    fatal: Option<String>,
    _tasks: Vec<Task<()>>,
    _appearance: Subscription,
}

impl Hyakume {
    pub fn new(
        runtime: Runtime,
        mut updates: mpsc::UnboundedReceiver<Update>,
        local: Rc<LocalTime>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let receive = cx.spawn(async move |this, cx| {
            while let Some(update) = updates.recv().await {
                if this.update(cx, |this, cx| this.apply(update, cx)).is_err() {
                    return;
                }
            }
        });
        // Offline columns count down to their next retry.
        let tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let still_open = this.update(cx, |this, cx| {
                    if this
                        .sources
                        .iter()
                        .any(|s| matches!(s.status, Status::Offline { .. }))
                    {
                        cx.notify();
                    }
                });
                if still_open.is_err() {
                    return;
                }
            }
        });
        let appearance = cx.observe_window_appearance(window, |this, window, cx| {
            this.theme = Theme::for_appearance(window.appearance());
            cx.notify();
        });
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        Self {
            runtime,
            local,
            theme: Theme::for_appearance(window.appearance()),
            sources: Vec::new(),
            scene: Rc::default(),
            layout: Layout::default(),
            placement: Rc::default(),
            camera: Camera::default(),
            overview_camera: None,
            focus: None,
            stubs: Rc::default(),
            selected: None,
            details: HashMap::new(),
            detail_request: 0,
            drag: None,
            canvas_bounds: Rc::default(),
            focus_handle,
            user_moved: false,
            fitted_for: None,
            last_canvas: None,
            fatal: None,
            _tasks: vec![receive, tick],
            _appearance: appearance,
        }
    }

    fn apply(&mut self, update: Update, cx: &mut Context<Self>) {
        match update {
            Update::Source(state) => {
                let key = state.key.clone();
                match self.sources.iter_mut().find(|s| s.key == key) {
                    Some(slot) => *slot = *state,
                    None => self.sources.push(*state),
                }
                self.rebuild();
                // Comments don't bump the task's version, so refetch on any change.
                if let Some(selected) = self.selected.clone().filter(|s| s.source == key) {
                    self.request_detail(&selected);
                }
            }
            Update::Detail {
                source,
                task,
                request,
                detail,
            } => {
                // Requests run side by side and can finish in any order; an
                // older answer must not replace newer comments.
                if request != self.detail_request {
                    return;
                }
                let key = NodeKey { source, id: task.0 };
                let detail = match detail {
                    Ok(detail) => Detail::Loaded(detail),
                    Err(reason) => Detail::Failed(reason),
                };
                self.details.insert(key, detail);
            }
            Update::Fatal(reason) => self.fatal = Some(reason),
        }
        cx.notify();
    }

    /// Rebuilds the scene from what's loaded. Camera, selection, focus, and
    /// every known position survive; only things that vanished are dropped.
    fn rebuild(&mut self) {
        // New webs may have arrived; an untouched view fits them in.
        self.fitted_for = None;
        let scene = Scene::build(&self.sources);
        self.placement = Rc::new(self.layout.place(&scene));
        self.scene = Rc::new(scene);
        if self
            .selected
            .as_ref()
            .is_some_and(|k| self.scene.eye(k).is_none())
        {
            self.selected = None;
        }
        match self.focus.as_ref().and_then(|k| self.scene.cluster(k)) {
            Some(c) => {
                self.stubs = Rc::new(canvas::stub_positions(&self.scene, &self.placement, c))
            }
            None if self.focus.is_some() => self.back_to_overview(),
            None => {}
        }
    }

    fn relayout(&mut self) {
        self.placement = Rc::new(self.layout.relayout(&self.scene));
        if let Some(c) = self.focus.as_ref().and_then(|k| self.scene.cluster(k)) {
            self.stubs = Rc::new(canvas::stub_positions(&self.scene, &self.placement, c));
        }
        self.fit();
    }

    fn viewport(&self, window: &Window) -> Vec2 {
        let size = window.viewport_size();
        let inspector = if self.selected.is_some() {
            inspector_width(f32::from(size.width))
        } else {
            0.0
        };
        Vec2::new(
            f32::from(size.width) - inspector,
            f32::from(size.height) - HEADER_H,
        )
    }

    /// Room to leave at the bottom of the canvas for the controls, and for the
    /// legend or hint when they had to move above the controls.
    fn bottom_reserve(&self, canvas_w: f32) -> f32 {
        const BAR: f32 = 64.0;
        if self.focus.is_some() {
            return if canvas_w >= ROOMY_FOR_HINT {
                BAR
            } else {
                BAR + 64.0
            };
        }
        if canvas_w >= ROOMY_FOR_LEGEND {
            return BAR;
        }
        let rows = (LEGEND_W / (canvas_w - 72.0).max(120.0)).ceil();
        BAR + rows * 28.0 + 28.0
    }

    /// Fits the overview, or the focused project with its outside links.
    fn fit(&mut self) {
        let bounds = self.canvas_bounds.get();
        let viewport = Vec2::new(f32::from(bounds.size.width), f32::from(bounds.size.height));
        self.fit_into(viewport);
    }

    fn fit_into(&mut self, viewport: Vec2) {
        // Keep fitted content clear of the zoom controls and legend at the bottom.
        let viewport = Vec2::new(viewport.x, viewport.y - self.bottom_reserve(viewport.x));
        if viewport.x < 1.0 || viewport.y < 1.0 {
            return;
        }
        self.camera = match self.focus.as_ref().and_then(|k| self.scene.cluster(k)) {
            Some(c) => {
                let (center, r) = self.placement.clusters[c];
                let mut reach = r + 60.0;
                for &(_, at) in self.stubs.iter() {
                    reach = reach.max((at - center).len() + 90.0);
                }
                let half = Vec2::new(reach, reach);
                Camera::fit(center - half, center + half, viewport, 1.6)
            }
            None => {
                let (min, max) = self.placement.bounds();
                if min.x > max.x {
                    return;
                }
                Camera::fit(min, max, viewport, 1.0)
            }
        };
    }

    fn focus_on(&mut self, cluster: usize) {
        if self.focus.is_none() {
            self.overview_camera = Some((self.camera, self.user_moved));
        }
        // A focused project fits itself too, on resizes as well, until moved.
        self.user_moved = false;
        self.fitted_for = None;
        self.focus = Some(self.scene.clusters[cluster].key.clone());
        self.stubs = Rc::new(canvas::stub_positions(
            &self.scene,
            &self.placement,
            cluster,
        ));
        self.fit();
    }

    fn back_to_overview(&mut self) {
        self.focus = None;
        self.stubs = Rc::default();
        match self.overview_camera.take() {
            Some((camera, true)) => {
                self.camera = camera;
                self.user_moved = true;
            }
            // The overview was fitting itself, so it goes back to doing that.
            _ => {
                self.user_moved = false;
                self.fitted_for = None;
            }
        }
    }

    /// Leaves the focused project and frames its whole workspace column on
    /// the overview, as the breadcrumb between "All workspaces" and the project.
    fn show_workspace(&mut self, column: &NodeKey) {
        let Some(i) = self.scene.columns.iter().position(|c| &c.key == column) else {
            return;
        };
        self.focus = None;
        self.stubs = Rc::default();
        self.overview_camera = None;
        self.user_moved = true;
        let (x, width) = self.placement.columns[i];
        let bottom = self.scene.columns[i]
            .clusters
            .clone()
            .map(|c| self.placement.clusters[c].0.y + self.placement.clusters[c].1 + 40.0)
            .fold(COLUMN_HEAD + 280.0, f32::max);
        let b = self.canvas_bounds.get();
        let viewport = Vec2::new(
            f32::from(b.size.width),
            f32::from(b.size.height) - self.bottom_reserve(f32::from(b.size.width)),
        );
        if viewport.x >= 1.0 && viewport.y >= 1.0 {
            self.camera = Camera::fit(
                Vec2::new(x, 0.0),
                Vec2::new(x + width, bottom),
                viewport,
                1.0,
            );
        }
    }

    /// After the canvas shrinks (the inspector opening, say), pans just enough
    /// to bring the selected eye back into view if it ended up hidden.
    fn keep_selected_visible(&mut self, viewport: Vec2) {
        let frame = self.frame();
        let Some(at) = frame.selected.and_then(|eye| frame.eye_at(eye)) else {
            return;
        };
        let s = self.camera.to_screen(at);
        let margin = 60.0;
        let bottom = viewport.y - self.bottom_reserve(viewport.x);
        let push = |v: f32, max: f32| {
            if max - margin < margin {
                0.0
            } else if v < margin {
                margin - v
            } else if v > max - margin {
                max - margin - v
            } else {
                0.0
            }
        };
        self.camera
            .pan(Vec2::new(push(s.x, viewport.x), push(s.y, bottom)));
    }

    fn select(&mut self, eye: usize) {
        let key = self.scene.eyes[eye].key.clone();
        // The inspector narrows the canvas; re-fitting then would move the eye just clicked.
        self.user_moved = true;
        self.details.clear();
        self.details.insert(key.clone(), Detail::Loading);
        self.request_detail(&key);
        self.selected = Some(key);
    }

    fn request_detail(&mut self, key: &NodeKey) {
        let Some(eye) = self.scene.eye(key) else {
            return;
        };
        self.detail_request += 1;
        let task = &self.scene.eyes[eye].task;
        self.runtime.send(Command::LoadDetail {
            source: key.source.clone(),
            task: task.id,
            reference: task.reference.clone(),
            request: self.detail_request,
        });
    }

    fn frame(&self) -> Frame {
        Frame {
            scene: self.scene.clone(),
            placement: self.placement.clone(),
            camera: self.camera,
            theme: self.theme,
            focus: self.focus.as_ref().and_then(|k| self.scene.cluster(k)),
            selected: self.selected.as_ref().and_then(|k| self.scene.eye(k)),
            stubs: self.stubs.clone(),
        }
    }

    fn local(&self, at: Point<Pixels>) -> Vec2 {
        let origin = self.canvas_bounds.get().origin;
        Vec2::new(f32::from(at.x - origin.x), f32::from(at.y - origin.y))
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        self.drag = Some(Drag {
            start: event.position,
            last: event.position,
            moved: false,
        });
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        // A release outside the window never reaches us; this ends that drag.
        if event.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            return;
        }
        let Some(drag) = &mut self.drag else { return };
        let delta = event.position - drag.last;
        drag.last = event.position;
        let travelled = event.position - drag.start;
        if f32::from(travelled.x).hypot(f32::from(travelled.y)) > CLICK_SLOP {
            drag.moved = true;
        }
        if drag.moved {
            self.user_moved = true;
            self.camera
                .pan(Vec2::new(f32::from(delta.x), f32::from(delta.y)));
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else { return };
        if drag.moved {
            return;
        }
        match canvas::hit(&self.frame(), self.local(event.position)) {
            Hit::Eye(eye) => self.select(eye),
            // A linked task from another project: swap focus to its project.
            Hit::Stub(eye) => {
                self.focus_on(self.scene.eyes[eye].cluster);
                self.select(eye);
            }
            Hit::Cluster(c) if self.focus.is_none() => self.focus_on(c),
            Hit::Cluster(_) | Hit::Nothing => self.selected = None,
        }
        cx.notify();
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(px(40.0));
        let (dx, dy) = (f32::from(delta.x), f32::from(delta.y));
        self.user_moved = true;
        if event.modifiers.control || event.modifiers.platform {
            self.camera
                .zoom_at(self.local(event.position), 1.0015_f32.powf(dy));
        } else {
            self.camera.pan(Vec2::new(dx, dy));
        }
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key == "escape" {
            if self.selected.is_some() {
                self.selected = None;
            } else if self.focus.is_some() {
                self.back_to_overview();
            }
            cx.notify();
        }
    }

    fn zoom_by(&mut self, factor: f32) {
        let b = self.canvas_bounds.get();
        let middle = Vec2::new(
            f32::from(b.size.width) / 2.0,
            f32::from(b.size.height) / 2.0,
        );
        self.camera.zoom_at(middle, factor);
        self.user_moved = true;
    }
}

impl Render for Hyakume {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let answered = !self.sources.is_empty()
            && self
                .sources
                .iter()
                .all(|s| !matches!(s.status, Status::Loading));
        let viewport = self.viewport(window);
        let resized_from = self
            .last_canvas
            .replace(viewport)
            .filter(|prev| *prev != viewport);
        if answered && !self.user_moved && self.fitted_for != Some(viewport) {
            self.fit_into(viewport);
            self.fitted_for = Some(viewport);
        } else if let Some(prev) = resized_from {
            // The person placed this view, so keep its middle where it was.
            self.camera.pan((viewport - prev) * 0.5);
        }
        if resized_from.is_some() {
            self.keep_selected_visible(viewport);
        }

        let root = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg)
            .text_color(t.ink)
            .text_size(px(13.0))
            .child(self.header(cx));

        if let Some(reason) = &self.fatal {
            return root.child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_size(px(16.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Hyakume can't read its profile"),
                    )
                    .child(
                        div()
                            .text_color(t.muted)
                            .child(inspector::clean(reason, false)),
                    ),
            );
        }

        let frame = self.frame();
        let bounds_cell = self.canvas_bounds.clone();
        let world = canvas(
            move |bounds, _, _| bounds_cell.set(bounds),
            move |bounds, _, window, cx| canvas::paint(bounds, &frame, window, cx),
        )
        .size_full();

        let mut area = div()
            .id("canvas")
            .relative()
            .flex_1()
            .h_full()
            .min_w_0()
            .overflow_hidden()
            .track_focus(&self.focus_handle)
            .cursor(if self.drag.as_ref().is_some_and(|d| d.moved) {
                CursorStyle::ClosedHand
            } else {
                CursorStyle::Arrow
            })
            .on_key_down(cx.listener(Self::on_key))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(div().absolute().size_full().child(world))
            .children(self.source_notes(cx))
            .child(self.zoom_controls(cx));
        let canvas_w = self.viewport(window).x;
        if self.focus.is_none() {
            area = area.child(self.legend(canvas_w));
        } else {
            let hint = pill(&t)
                .absolute()
                .px(px(14.0))
                .py(px(6.0))
                .text_color(t.muted)
                .text_size(px(12.0))
                .child("Esc goes back to all workspaces · click a task to inspect it");
            // Beside the zoom controls when there's room, above them when not.
            area = area.child(if canvas_w >= ROOMY_FOR_HINT {
                hint.right(px(20.0)).bottom(px(26.0))
            } else {
                hint.left(px(20.0))
                    .bottom(px(76.0))
                    .max_w(px(canvas_w - 40.0))
                    .rounded(px(14.0))
            });
        }

        let mut body = div().flex_1().min_h_0().flex().flex_row().child(area);
        if let Some(eye) = self.selected.as_ref().and_then(|k| self.scene.eye(k)) {
            let detail = self.selected.as_ref().and_then(|k| self.details.get(k));
            let close = cx.listener(|this: &mut Self, _: &ClickEvent, _, cx| {
                this.selected = None;
                cx.notify();
            });
            let width = inspector_width(f32::from(window.viewport_size().width));
            body = body.child(inspector::panel(
                &self.scene,
                eye,
                detail,
                &t,
                &self.local,
                width,
                close,
            ));
        }
        root.child(body)
    }
}

impl Hyakume {
    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        // Clipped rather than allowed to push the header wider than the window.
        let mut crumbs = div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .flex()
            .items_center()
            .gap(px(8.0));
        match self.focus.as_ref().and_then(|k| self.scene.cluster(k)) {
            None => {
                crumbs = crumbs.child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("All workspaces"),
                )
            }
            Some(c) => {
                let cluster = &self.scene.clusters[c];
                let column = &self.scene.columns[cluster.column];
                let place = format!("{} / {}", column.chip, column.title);
                let workspace = column.key.clone();
                crumbs = crumbs
                    .child(
                        div()
                            .id("crumb-home")
                            .text_color(t.muted)
                            .cursor_pointer()
                            .hover(move |s| s.text_color(t.ink))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.back_to_overview();
                                cx.notify();
                            }))
                            .child("All workspaces"),
                    )
                    .child(div().text_color(t.muted).child("›"))
                    .child(
                        div()
                            .id("crumb-workspace")
                            .text_color(t.muted)
                            .cursor_pointer()
                            .hover(move |s| s.text_color(t.ink))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.show_workspace(&workspace);
                                cx.notify();
                            }))
                            .child(inspector::clean(&place, false)),
                    )
                    .child(div().text_color(t.muted).child("›"))
                    .child(
                        div()
                            .flex()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .font_weight(FontWeight::BOLD)
                                    .child(inspector::clean(&cluster.project_key, false)),
                            )
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(inspector::clean(&cluster.name, false)),
                            ),
                    );
            }
        }
        div()
            .h(px(HEADER_H))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(28.0))
            .px(px(20.0))
            .bg(t.surface)
            .border_b_1()
            .border_color(t.line)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .h_full()
                    .child(
                        // Meme hangs on its thread from the window's top edge.
                        canvas(
                            |_, _, _| {},
                            move |bounds, _, window, _| {
                                let body = gpui::Bounds::new(
                                    gpui::point(bounds.origin.x, bounds.origin.y + px(10.0)),
                                    gpui::size(px(50.0), px(50.0)),
                                );
                                meme::paint(window, body, &t.meme, Some(bounds.origin.y));
                            },
                        )
                        .w(px(50.0))
                        .h_full(),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(15.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Hyakume"),
                            )
                            .child(div().text_size(px(11.0)).text_color(t.muted).child("百目")),
                    ),
            )
            .child(crumbs)
    }

    /// Boxes under the header of each source that isn't live, with a retry
    /// button. Placed in canvas coordinates so they move with their column.
    fn source_notes(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        // While focused, only the focused project's source matters, and its
        // note sits in the corner since the column header isn't drawn.
        let focused = self
            .focus
            .as_ref()
            .and_then(|k| self.scene.cluster(k))
            .map(|c| self.scene.clusters[c].column);
        let t = self.theme;
        let mut seen: Vec<&SourceKey> = Vec::new();
        let mut notes = Vec::new();
        for (i, column) in self.scene.columns.iter().enumerate() {
            if focused.is_some_and(|f| f != i) || seen.contains(&&column.source) {
                continue;
            }
            seen.push(&column.source);
            let name = &column.chip;
            let (title, message) = match &column.status {
                Status::Live | Status::Loading => continue,
                Status::Offline { reason, retry_at } => {
                    let wait = sources::seconds_until(*retry_at);
                    (
                        format!("Can't reach {name}"),
                        format!(
                            "{reason}. Trying again in {wait} s. Its webs come back when the server answers."
                        ),
                    )
                }
                Status::SignedOut(reason) => (format!("Not signed in to {name}"), reason.clone()),
                Status::Failed(reason) => (format!("Something broke in {name}"), reason.clone()),
            };
            let Some(&(x, _)) = self.placement.columns.get(i) else {
                continue;
            };
            // Below the column's webs (or its placeholder web), not on top of them.
            let below = column
                .clusters
                .clone()
                .map(|c| self.placement.clusters[c].0.y + self.placement.clusters[c].1 + 50.0)
                .fold(COLUMN_HEAD + 300.0, f32::max);
            let at = match focused {
                Some(_) => Vec2::new(20.0, 20.0),
                None => self.camera.to_screen(Vec2::new(x + 40.0, below)),
            };
            let key = column.source.clone();
            notes.push(
                div()
                    .block_mouse_except_scroll()
                    .absolute()
                    .left(px(at.x))
                    .top(px(at.y))
                    .w(px(300.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .px(px(18.0))
                    .py(px(16.0))
                    .rounded(px(16.0))
                    .border_1()
                    .border_dashed()
                    .border_color(t.line)
                    .bg(t.surface)
                    .child(
                        div()
                            .text_size(px(15.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(inspector::clean(&title, false)),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .line_height(px(18.0))
                            .text_color(t.muted)
                            .child(inspector::clean(&message, false)),
                    )
                    .child(
                        button(&t, format!("retry-{key}"))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                                this.runtime.send(Command::Retry(key.clone()));
                            }))
                            .child("Retry now"),
                    )
                    .into_any_element(),
            );
        }
        notes
    }

    fn zoom_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let fit_label = if self.focus.is_some() {
            "Fit project"
        } else {
            "Fit all"
        };
        pill(&t)
            .absolute()
            .left(px(20.0))
            .bottom(px(20.0))
            .flex()
            .items_center()
            .gap(px(2.0))
            .p(px(4.0))
            .child(
                button(&t, "zoom-out")
                    .w(px(34.0))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.zoom_by(1.0 / 1.25);
                        cx.notify();
                    }))
                    .child("−"),
            )
            .child(
                div()
                    .min_w(px(48.0))
                    .flex()
                    .justify_center()
                    .text_size(px(12.0))
                    .child(SharedString::from(format!(
                        "{:.0}%",
                        self.camera.zoom * 100.0
                    ))),
            )
            .child(
                button(&t, "zoom-in")
                    .w(px(34.0))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.zoom_by(1.25);
                        cx.notify();
                    }))
                    .child("+"),
            )
            .child(div().w(px(1.0)).h(px(20.0)).mx(px(4.0)).bg(t.line))
            .child(
                button(&t, "fit")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.fit();
                        // Fitting hands the view back to auto-fit.
                        this.user_moved = false;
                        cx.notify();
                    }))
                    .child(fit_label),
            )
            .child(
                button(&t, "relayout")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.relayout();
                        cx.notify();
                    }))
                    .child("Re-layout"),
            )
    }

    fn legend(&self, canvas_w: f32) -> impl IntoElement {
        let t = self.theme;
        let stages = [
            (Shape::Backlog, "Backlog"),
            (Shape::Todo, "To do"),
            (Shape::Doing, "In progress"),
            (Shape::Review, "Review"),
            (Shape::Done, "Done"),
        ];
        let line = |kind: ThreadKind| {
            let style = canvas::thread_style(kind, &t);
            canvas(
                |_, _, _| {},
                move |b, _, window, _| {
                    let from = gpui::point(b.origin.x + px(2.0), b.origin.y + px(4.0));
                    let to = gpui::point(b.origin.x + px(30.0), b.origin.y + px(5.0));
                    crate::glyphs::thread(window, from, to, 0.0, style);
                },
            )
            .w(px(32.0))
            .h(px(12.0))
        };
        let legend = pill(&t).absolute();
        // Beside the zoom controls when there's room, wrapped above them when not.
        let legend = if canvas_w >= ROOMY_FOR_LEGEND {
            legend.right(px(20.0)).bottom(px(20.0))
        } else {
            legend
                .left(px(20.0))
                .bottom(px(72.0))
                .max_w(px(canvas_w - 40.0))
                .flex_wrap()
                .rounded(px(16.0))
        };
        legend
            .flex()
            .items_center()
            .gap(px(16.0))
            .px(px(16.0))
            .py(px(10.0))
            .text_size(px(12.0))
            .children(stages.into_iter().map(move |(shape, name)| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        canvas(
                            |_, _, _| {},
                            move |b, _, window, _| canvas::legend_eye(b, shape, &t, window),
                        )
                        .w(px(22.0))
                        .h(px(16.0)),
                    )
                    .child(name)
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(inspector::round_mark(t.blocked, t.on_blocked))
                    .child("Blocked"),
            )
            .child(div().w(px(1.0)).h(px(18.0)).bg(t.line))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(line(ThreadKind::Blocks))
                    .child("Blocks"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(line(ThreadKind::Undirected))
                    .child("Related"),
            )
    }
}

/// Floating controls over the canvas. Clicks on them stay on them, so a
/// button over a web never also focuses the web; scrolling still pans.
fn pill(t: &Theme) -> gpui::Div {
    div()
        .block_mouse_except_scroll()
        .rounded(px(22.0))
        .border_1()
        .border_color(t.line)
        .bg(t.surface)
}

fn button(t: &Theme, id: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    let raised = t.raised;
    div()
        .id(id.into())
        .h(px(34.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(17.0))
        .text_size(px(12.0))
        .cursor_pointer()
        .hover(move |s| s.bg(raised))
}
