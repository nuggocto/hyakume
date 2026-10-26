//! The window's root view: owns what's loaded, the camera, and what is
//! focused or selected, and turns pointer and key input into changes.

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    Action, AnyElement, App, Bounds, ClickEvent, Context, CursorStyle, FocusHandle, FontWeight,
    Hsla, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollHandle,
    ScrollWheelEvent, SharedString, Subscription, Task, UniformListScrollHandle, Window, canvas,
    div, point, prelude::*, px,
};
use hyakume_core::model::is_unsafe_char;
use tokio::sync::mpsc;

use crate::canvas::{self, Camera, Frame, Hit, Side, Spot};
use crate::clock::LocalTime;
use crate::glyphs;
use crate::inspector::{self, Detail};
use crate::keys::{self, *};
use crate::layout::{COLUMN_HEAD, Layout, Placement, Vec2};
use crate::meme;
use crate::picker::{self, Item, Mode, Pick, Picker};
use crate::runtime::{Command, Runtime, Update};
use crate::scene::{Dir, NodeKey, Scene, Shape, ThreadKind};
use crate::sources::{self, SourceKey, SourceState, Status};
use crate::text_input::{Changed, TextInput};
use crate::theme::Theme;

const HEADER_H: f32 = 64.0;
/// Below this canvas width the legend no longer fits beside the zoom
/// controls, so it moves to its own row above them.
const ROOMY_FOR_LEGEND: f32 = 1100.0;
/// The same for the shorter hint shown while a project is focused.
const ROOMY_FOR_HINT: f32 = 820.0;
/// Rough width of the legend's contents, to guess how many rows it wraps to.
const LEGEND_W: f32 = 760.0;
/// Narrower than this, a note drops its message: wrapped to a few words a
/// line it can't be read anyway.
const NOTE_MIN_W: f32 = 200.0;

/// Roughly how tall an offline note `w` wide gets, its title and message
/// wrapped, to keep its Retry button clear of the controls at the bottom.
/// Over rather than under: about 6.5 px a character at 12 px, 7.5 at 15.
fn note_height(title: &str, message: &str, w: f32) -> f32 {
    let text_w = (w - 36.0).max(40.0);
    let lines =
        |text: &str, per_char: f32| (text.chars().count() as f32 * per_char / text_w).ceil();
    32.0 + 22.0 * lines(title, 7.5) + 18.0 * lines(message, 6.5) + 8.0 + 36.0
}

/// Pixels kept above the topmost webs when fitting the overview, on top of
/// the usual padding: a column header, then a web's name under it.
const HEAD_ROOM: f32 = 56.0;
/// Below this window width the header buttons drop their shortcut text.
const ROOMY_FOR_SHORTCUTS: f32 = 900.0;
/// How long a short message such as "no outgoing links" stays up.
const NOTICE_FOR: Duration = Duration::from_secs(3);
/// A broken `keys.toml` takes longer to read than "no links".
const KEYS_NOTICE_FOR: Duration = Duration::from_secs(10);

/// The inspector gives up some width in small windows so the canvas stays usable.
fn inspector_width(window_w: f32) -> f32 {
    (window_w * 0.42).clamp(300.0, 400.0)
}
/// A press that moves less than this is a click, not a pan.
const CLICK_SLOP: f32 = 4.0;
/// How far Shift+arrow pans, in screen pixels.
const PAN_STEP: f32 = 120.0;

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
    /// The eye with the ring. Arrows move it; the inspector shows it when open.
    selected: Option<NodeKey>,
    inspecting: bool,
    /// The web the arrows are on, on the overview before an eye is picked.
    web_cursor: Option<NodeKey>,
    /// What the pointer is over; it lights up its links like a selection.
    hovered: Option<Spot>,
    /// A hover that stays dark: the eye just clicked to deselect it, which
    /// the pointer is still on. Moving off it ends this.
    hover_muted: Option<Spot>,
    details: HashMap<NodeKey, Detail>,
    /// Number of the newest detail request; answers to older ones are dropped.
    detail_request: u64,
    details_scroll: ScrollHandle,
    inspector_stops: inspector::Stops,
    picker: Option<Picker>,
    /// Where focus goes back to when the palette is dismissed.
    picker_return: Option<FocusHandle>,
    /// The view before Find opened; dismissing Find puts it back.
    before_find: Option<(Camera, bool)>,
    /// While Find is open: which eyes match the query.
    matches: Option<Rc<Vec<bool>>>,
    /// Name keys of the stages filtered out, and the eyes that hides. Only
    /// what's drawn changes; every position stays where it was.
    hidden_stages: BTreeSet<String>,
    hidden: Option<Rc<Vec<bool>>>,
    _picker_input: Option<Subscription>,
    /// A short message and when it goes away.
    notice: Option<(SharedString, Instant)>,
    /// Where `keys.toml` is, for reloading it; none if there's no profile.
    keys_file: Option<PathBuf>,
    /// True after a key press, false after a click: focus rings only show
    /// for keyboard use, the way browsers treat `:focus-visible`.
    keyboard: bool,
    drag: Option<Drag>,
    /// Set while painting, read by input handlers to turn window coordinates
    /// into canvas coordinates.
    canvas_bounds: Rc<Cell<Bounds<Pixels>>>,
    focus_handle: FocusHandle,
    /// Until the person pans, zooms, or focuses, the view keeps fitting
    /// everything: after the first answers, new projects, and window resizes.
    user_moved: bool,
    fitted_for: Option<Vec2>,
    /// Whether the columns were measured for their names since the last rebuild.
    names_measured: bool,
    /// Window size at the last render, to tell a resize from the details
    /// panel opening or closing.
    last_window: Option<Vec2>,
    /// Canvas size at the last render, to notice resizes.
    last_canvas: Option<Vec2>,
    fatal: Option<String>,
    _tasks: Vec<Task<()>>,
    _appearance: Subscription,
    _keystrokes: Subscription,
    _focus_lost: Subscription,
}

impl Hyakume {
    pub fn new(
        runtime: Runtime,
        mut updates: mpsc::UnboundedReceiver<Update>,
        local: Rc<LocalTime>,
        keys_file: Option<PathBuf>,
        key_problems: Vec<String>,
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
        // Offline columns count down to their next retry; notices expire.
        let tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let still_open = this.update(cx, |this, cx| {
                    let expired = this
                        .notice
                        .as_ref()
                        .is_some_and(|(_, until)| Instant::now() >= *until);
                    if expired {
                        this.notice = None;
                    }
                    let offline = this
                        .sources
                        .iter()
                        .any(|s| matches!(s.status, Status::Offline { .. }));
                    if expired || offline {
                        cx.notify();
                    }
                });
                if still_open.is_err() {
                    return;
                }
            }
        });
        // Observers see every key, also the ones an action consumed, which
        // key listeners on elements never hear about.
        let keystrokes = cx.observe_keystrokes(|this, _, _, cx| {
            if !this.keyboard {
                this.keyboard = true;
                cx.notify();
            }
        });
        let appearance = cx.observe_window_appearance(window, |this, window, cx| {
            this.theme = Theme::for_appearance(window.appearance());
            cx.notify();
        });
        // A focused control can vanish under the keyboard: a breadcrumb once
        // used, a Retry button when the server comes back by itself. With
        // nothing focused GPUI runs no shortcut at all, so the canvas takes over.
        let focus_lost = cx.on_focus_lost(window, |this, window, _| {
            window.focus(&this.focus_handle);
        });
        let focus_handle = cx.focus_handle().tab_stop(true);
        window.focus(&focus_handle);
        let mut this = Self {
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
            inspecting: false,
            web_cursor: None,
            hovered: None,
            hover_muted: None,
            details: HashMap::new(),
            detail_request: 0,
            details_scroll: ScrollHandle::new(),
            inspector_stops: inspector::Stops::default(),
            picker: None,
            picker_return: None,
            before_find: None,
            matches: None,
            hidden_stages: BTreeSet::new(),
            hidden: None,
            _picker_input: None,
            notice: None,
            keys_file,
            keyboard: false,
            drag: None,
            canvas_bounds: Rc::default(),
            focus_handle,
            user_moved: false,
            fitted_for: None,
            names_measured: false,
            last_window: None,
            last_canvas: None,
            fatal: None,
            _tasks: vec![receive, tick],
            _appearance: appearance,
            _keystrokes: keystrokes,
            _focus_lost: focus_lost,
        };
        this.report_keys(key_problems, false);
        this
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
                self.refresh_picker(cx);
                // Comments don't bump the task's version, so refetch on any change.
                if let Some(selected) = self.selected.clone().filter(|s| s.source == key)
                    && self.inspecting
                {
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
        self.names_measured = false;
        let scene = Scene::build(&self.sources);
        self.placement = Rc::new(self.layout.place(&scene));
        self.scene = Rc::new(scene);
        self.apply_stage_filter();
        // It's indices into the old scene; the next pointer move sets it again.
        self.hovered = None;
        if self
            .selected
            .as_ref()
            .is_some_and(|k| self.scene.eye(k).is_none())
        {
            self.selected = None;
            self.inspecting = false;
        }
        if self
            .web_cursor
            .as_ref()
            .is_some_and(|k| self.scene.cluster(k).is_none())
        {
            self.web_cursor = None;
        }
        match self.focus.as_ref().and_then(|k| self.scene.cluster(k)) {
            Some(c) => {
                self.stubs = Rc::new(canvas::stub_positions(&self.scene, &self.placement, c))
            }
            None if self.focus.is_some() => self.back_to_overview(),
            None => {}
        }
    }

    /// Recomputes which eyes the stage filter hides. A hidden task can't
    /// stay selected: its ring and details would point at nothing drawn.
    fn apply_stage_filter(&mut self) {
        self.hidden = (!self.hidden_stages.is_empty())
            .then(|| Rc::new(self.scene.hidden_by(&self.hidden_stages)));
        let selected = self.selected.as_ref().and_then(|k| self.scene.eye(k));
        if selected.is_some_and(|e| self.is_hidden(e)) {
            self.selected = None;
            self.inspecting = false;
        }
    }

    fn is_hidden(&self, eye: usize) -> bool {
        self.hidden
            .as_ref()
            .is_some_and(|h| h.get(eye).copied().unwrap_or(false))
    }

    fn toggle_stage(&mut self, key: String) {
        if !self.hidden_stages.remove(&key) {
            self.hidden_stages.insert(key);
        }
        self.apply_stage_filter();
    }

    fn show_every_stage(&mut self) {
        self.hidden_stages.clear();
        self.apply_stage_filter();
    }

    fn relayout(&mut self) {
        self.placement = Rc::new(self.layout.relayout(&self.scene));
        self.names_measured = false;
        if let Some(c) = self.focus.as_ref().and_then(|k| self.scene.cluster(k)) {
            self.stubs = Rc::new(canvas::stub_positions(&self.scene, &self.placement, c));
        }
        self.fit();
    }

    fn inspector_open(&self) -> bool {
        self.inspecting && self.selected.is_some()
    }

    fn viewport(&self, window: &Window) -> Vec2 {
        let size = window.viewport_size();
        let inspector = if self.inspector_open() {
            inspector_width(f32::from(size.width))
        } else {
            0.0
        };
        Vec2::new(
            f32::from(size.width) - inspector,
            f32::from(size.height) - HEADER_H,
        )
    }

    /// The canvas size as last painted.
    fn canvas_size(&self) -> Vec2 {
        let b = self.canvas_bounds.get();
        Vec2::new(f32::from(b.size.width), f32::from(b.size.height))
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
        self.fit_into(self.canvas_size());
    }

    fn fit_into(&mut self, viewport: Vec2) {
        if let Some(camera) = self.fitted(viewport) {
            self.camera = camera;
        }
    }

    /// The camera that fits the overview, or the focused project with its
    /// outside links, into `viewport`. The overview stays zoomed in enough
    /// for every web's name to have room, even if that leaves some to pan to.
    fn fitted(&self, viewport: Vec2) -> Option<Camera> {
        // Keep fitted content clear of the zoom controls and legend at the bottom.
        let viewport = Vec2::new(viewport.x, viewport.y - self.bottom_reserve(viewport.x));
        if viewport.x < 1.0 || viewport.y < 1.0 {
            return None;
        }
        if let Some(c) = self.focus.as_ref().and_then(|k| self.scene.cluster(k)) {
            let (center, r) = self.placement.clusters[c];
            let mut reach = r + 60.0;
            for &(_, at) in self.stubs.iter() {
                reach = reach.max((at - center).len() + 90.0);
            }
            let half = Vec2::new(reach, reach);
            return Some(Camera::fit(center - half, center + half, viewport, 1.6));
        }
        let (min, max) = self.placement.bounds();
        if min.x > max.x {
            return None;
        }
        // Column headers and the first webs' names keep their size at any
        // zoom, so their room on top is kept in pixels.
        let top = self
            .placement
            .clusters
            .iter()
            .map(|&(c, r)| c.y - r)
            .fold(f32::MAX, f32::min);
        let min = Vec2::new(min.x, if top < f32::MAX { top } else { min.y });
        let room = Vec2::new(viewport.x, viewport.y - HEAD_ROOM);
        let mut camera = Camera::fit(min, max, room, 1.0);
        camera.origin.y -= HEAD_ROOM / camera.zoom;
        // Zoomed in from the top, so what no longer fits is below to scroll
        // down to, and the top row keeps its headers.
        let names = canvas::names_zoom(&self.scene, &self.placement);
        if camera.zoom < names {
            let top = Vec2::new(viewport.x * 0.5, 0.0);
            camera.zoom_at(top, names / camera.zoom, names);
        }
        Some(camera)
    }

    /// How far out the view may zoom: a little past fitting everything,
    /// where there's nothing more to see, and never so far that a web's
    /// name would land on the web above it.
    fn zoom_floor(&self) -> f32 {
        let fit = self
            .fitted(self.canvas_size())
            .map_or(Camera::MIN_ZOOM, |c| c.zoom * 0.8);
        match self.focus {
            Some(_) => fit,
            None => fit.max(canvas::names_zoom(&self.scene, &self.placement)),
        }
    }

    fn focus_on(&mut self, cluster: usize) {
        if self.focus.is_none() {
            self.overview_camera = Some((self.camera, self.user_moved));
        }
        // A ring left on another project's task would keep its inspector
        // open beside this one, and Enter would jump straight back to it.
        let elsewhere = |e: usize| self.scene.eyes[e].cluster != cluster;
        if self
            .selected
            .as_ref()
            .and_then(|k| self.scene.eye(k))
            .is_some_and(elsewhere)
        {
            self.selected = None;
            self.inspecting = false;
        }
        // What was hovered is somewhere else now; the next move finds out.
        self.hovered = None;
        // A focused project fits itself too, on resizes as well, until moved.
        self.user_moved = false;
        self.fitted_for = None;
        self.web_cursor = None;
        self.focus = Some(self.scene.clusters[cluster].key.clone());
        self.stubs = Rc::new(canvas::stub_positions(
            &self.scene,
            &self.placement,
            cluster,
        ));
        self.fit();
    }

    fn back_to_overview(&mut self) {
        self.hovered = None;
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
        // As in focus_on: a ring on another workspace's task would keep its
        // details open here, and the arrows would carry on from over there.
        let elsewhere = |c: usize| self.scene.clusters[c].column != i;
        if self
            .selected
            .as_ref()
            .and_then(|k| self.scene.eye(k))
            .is_some_and(|e| elsewhere(self.scene.eyes[e].cluster))
        {
            self.selected = None;
            self.inspecting = false;
        }
        if self
            .web_cursor
            .as_ref()
            .and_then(|k| self.scene.cluster(k))
            .is_some_and(elsewhere)
        {
            self.web_cursor = None;
        }
        self.hovered = None;
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
        let size = self.canvas_size();
        let viewport = Vec2::new(size.x, size.y - self.bottom_reserve(size.x));
        if viewport.x >= 1.0 && viewport.y >= 1.0 {
            self.camera = Camera::fit(
                Vec2::new(x, 0.0),
                Vec2::new(x + width, bottom),
                viewport,
                1.0,
            );
        }
    }

    /// Pans just enough to bring `world` inside the canvas, clear of the
    /// edges and the controls at the bottom. A pan makes the view the
    /// person's: a refit would throw the cursor back off screen.
    fn keep_visible(&mut self, world: Vec2, viewport: Vec2) {
        let s = self.camera.to_screen(world);
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
        let by = Vec2::new(push(s.x, viewport.x), push(s.y, bottom));
        if by != Vec2::default() {
            self.camera.pan(by);
            self.user_moved = true;
        }
    }

    /// After the canvas shrinks (the inspector opening, say), pans just enough
    /// to bring the selected eye back into view if it ended up hidden.
    fn keep_selected_visible(&mut self, viewport: Vec2) {
        let frame = self.frame();
        if let Some(at) = frame.selected.and_then(|eye| frame.eye_at(eye)) {
            self.keep_visible(at, viewport);
        }
    }

    /// Puts the ring on `eye`; `inspect` opens the inspector on it too.
    fn select(&mut self, eye: usize, inspect: bool) {
        let key = self.scene.eyes[eye].key.clone();
        // The inspector narrows the canvas; re-fitting then would move the eye just clicked.
        self.user_moved = true;
        self.web_cursor = None;
        if self.selected.as_ref() != Some(&key) {
            self.details_scroll.set_offset(point(px(0.0), px(0.0)));
        }
        self.selected = Some(key);
        self.inspecting = inspect;
        if inspect {
            self.load_detail();
        }
    }

    fn load_detail(&mut self) {
        let Some(key) = self.selected.clone() else {
            return;
        };
        self.details.clear();
        self.details.insert(key.clone(), Detail::Loading);
        self.request_detail(&key);
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

    /// Selects `eye` wherever it is. While focused, an eye in another
    /// project swaps focus to that project, the way clicking its stub does.
    fn go_to(&mut self, eye: usize, inspect: bool) {
        let cluster = self.scene.eyes[eye].cluster;
        let focused = self.focus.as_ref().and_then(|k| self.scene.cluster(k));
        if focused.is_some_and(|c| c != cluster) {
            self.focus_on(cluster);
        }
        self.select(eye, inspect);
        if let Some(at) = self.frame().eye_at(eye) {
            self.keep_visible(at, self.canvas_size());
        }
    }

    fn frame(&self) -> Frame {
        let preview = self
            .picker
            .as_ref()
            .filter(|p| p.mode == Mode::Find)
            .and_then(|p| match p.current().map(|item| &item.pick) {
                Some(Pick::Task(key)) => self.scene.eye(key),
                _ => None,
            });
        // Hidden stubs go too, small web and all.
        let stubs = match self.hidden {
            Some(_) => Rc::new(
                self.stubs
                    .iter()
                    .filter(|(e, _)| !self.is_hidden(*e))
                    .copied()
                    .collect(),
            ),
            None => self.stubs.clone(),
        };
        Frame {
            scene: self.scene.clone(),
            placement: self.placement.clone(),
            camera: self.camera,
            theme: self.theme,
            focus: self.focus.as_ref().and_then(|k| self.scene.cluster(k)),
            selected: self.selected.as_ref().and_then(|k| self.scene.eye(k)),
            stubs,
            web_cursor: self
                .web_cursor
                .as_ref()
                .filter(|_| self.focus.is_none())
                .and_then(|k| self.scene.cluster(k)),
            preview,
            matches: self.matches.clone(),
            hidden: self.hidden.clone(),
            ring: None,
            spot: self.spot(),
        }
    }

    /// What lights up: the pointer's eye or web, else the selection, else
    /// the web the arrows are on.
    fn spot(&self) -> Option<Spot> {
        let selected = self.selected.as_ref().and_then(|k| self.scene.eye(k));
        let cursor = self
            .web_cursor
            .as_ref()
            .filter(|_| self.focus.is_none())
            .and_then(|k| self.scene.cluster(k));
        self.hovered
            .filter(|&h| Some(h) != self.hover_muted)
            .or(selected.map(Spot::Eye))
            .or(cursor.map(Spot::Web))
    }

    fn local(&self, at: Point<Pixels>) -> Vec2 {
        let origin = self.canvas_bounds.get().origin;
        Vec2::new(f32::from(at.x - origin.x), f32::from(at.y - origin.y))
    }

    fn note(&mut self, text: impl Into<SharedString>) {
        self.note_for(text, NOTICE_FOR);
    }

    /// Notices quote task references, which come from servers as plain
    /// strings, so they get the same filtering as every other server text.
    fn note_for(&mut self, text: impl Into<SharedString>, how_long: Duration) {
        let text = inspector::clean(&text.into(), false);
        self.notice = Some((text, Instant::now() + how_long));
    }

    /// Every problem goes to the terminal, but only the first fits in the
    /// notice. The file is the person's own, but a quoted TOML key can still
    /// hold escapes, so they're blanked like server text.
    fn report_keys(&mut self, problems: Vec<String>, reloaded: bool) {
        let file = keys::FILE;
        let clean = |p: &str| -> String {
            p.chars()
                .map(|c| if is_unsafe_char(c) { '\u{FFFD}' } else { c })
                .collect()
        };
        for problem in &problems {
            eprintln!("{file}: {}", clean(problem));
        }
        match problems.as_slice() {
            [] if reloaded => self.note("Shortcuts reloaded"),
            [] => {}
            [one] => self.note_for(format!("{file}: {}", clean(one)), KEYS_NOTICE_FOR),
            [first, rest @ ..] => self.note_for(
                format!(
                    "{file}: {} (and {} more; see the terminal)",
                    clean(first),
                    rest.len()
                ),
                KEYS_NOTICE_FOR,
            ),
        }
    }

    /// A mouse click on a control leaves the keyboard on the canvas, so the
    /// arrows keep working; a keyboard click keeps focus where it is.
    fn after_click(&self, event: &ClickEvent, window: &mut Window) {
        if !event.is_keyboard() {
            window.focus(&self.focus_handle);
        }
    }

    /// Where a palette opened from a button hands focus back: the button
    /// itself after Enter or Space, the canvas after a mouse click.
    fn return_focus(&self, event: &ClickEvent, window: &Window, cx: &App) -> Option<FocusHandle> {
        match event.is_keyboard() {
            true => window.focused(cx),
            false => Some(self.focus_handle.clone()),
        }
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
            let hovered = match canvas::hit(&self.frame(), self.local(event.position)) {
                Hit::Eye(eye) | Hit::Stub(eye) => Some(Spot::Eye(eye)),
                // An opened project's own web would light everything.
                Hit::Cluster(c) if self.focus.is_none() => Some(Spot::Web(c)),
                Hit::Cluster(_) | Hit::Header | Hit::Nothing => None,
            };
            if hovered != self.hovered {
                self.hovered = hovered;
                self.hover_muted = None;
                cx.notify();
            }
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
            // A second click on the selected eye lets go of it, and its
            // links stop being lit even though the pointer is still there.
            Hit::Eye(eye) if self.selected.as_ref() == Some(&self.scene.eyes[eye].key) => {
                self.selected = None;
                self.inspecting = false;
                self.hover_muted = Some(Spot::Eye(eye));
            }
            Hit::Eye(eye) => self.select(eye, true),
            // A linked task from another project: swap focus to its project.
            Hit::Stub(eye) => {
                self.focus_on(self.scene.eyes[eye].cluster);
                self.select(eye, true);
            }
            Hit::Cluster(c) if self.focus.is_none() => self.focus_on(c),
            // The band is a header, not empty canvas: clicking it changes nothing.
            Hit::Header => {}
            Hit::Cluster(_) | Hit::Nothing => {
                self.selected = None;
                self.inspecting = false;
                self.web_cursor = None;
            }
        }
        cx.notify();
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(px(40.0));
        let (dx, dy) = (f32::from(delta.x), f32::from(delta.y));
        self.user_moved = true;
        if event.modifiers.control || event.modifiers.platform {
            let floor = self.zoom_floor();
            self.camera
                .zoom_at(self.local(event.position), 1.0015_f32.powf(dy), floor);
        } else {
            self.camera.pan(Vec2::new(dx, dy));
        }
        cx.notify();
    }

    /// Zooms around the selected eye when it's in view, else the middle.
    fn zoom_by(&mut self, factor: f32) {
        let size = self.canvas_size();
        let frame = self.frame();
        let anchor = frame
            .selected
            .and_then(|eye| frame.eye_at(eye))
            .map(|at| self.camera.to_screen(at))
            .filter(|s| s.x >= 0.0 && s.y >= 0.0 && s.x <= size.x && s.y <= size.y)
            .unwrap_or(size * 0.5);
        let floor = self.zoom_floor();
        self.camera.zoom_at(anchor, factor, floor);
        self.user_moved = true;
    }

    /// Arrow keys. With an eye selected they move the ring to the nearest
    /// eye that way. With none, they start from the middle of the view: on
    /// a web's eyes while focused, on the webs themselves on the overview.
    fn move_cursor(&mut self, side: Side) {
        let frame = self.frame();
        let middle = self.camera.to_world(self.canvas_size() * 0.5);
        let closest = |items: &mut dyn Iterator<Item = (usize, Vec2)>| {
            items
                .min_by(|a, b| (a.1 - middle).len().total_cmp(&(b.1 - middle).len()))
                .map(|(i, _)| i)
        };
        let from = frame.selected.and_then(|eye| frame.eye_at(eye));
        if let Some(from) = from {
            if let Some(next) = canvas::nearest(from, side, frame.drawn_eyes()) {
                self.select(next, self.inspecting);
                if let Some(at) = frame.eye_at(next) {
                    self.keep_visible(at, self.canvas_size());
                }
            }
            return;
        }
        if frame.focus.is_some() {
            if let Some(first) = closest(&mut frame.drawn_eyes()) {
                self.select(first, false);
                if let Some(at) = frame.eye_at(first) {
                    self.keep_visible(at, self.canvas_size());
                }
            }
            return;
        }
        let mut webs = self
            .placement
            .clusters
            .iter()
            .enumerate()
            .map(|(i, &(center, _))| (i, center));
        let next = match frame.web_cursor {
            Some(c) => canvas::nearest(self.placement.clusters[c].0, side, webs),
            None => closest(&mut webs),
        };
        if let Some(c) = next {
            self.web_cursor = Some(self.scene.clusters[c].key.clone());
            self.keep_visible(self.placement.clusters[c].0, self.canvas_size());
        }
    }

    fn pan_by(&mut self, dx: f32, dy: f32) {
        self.camera.pan(Vec2::new(dx, dy));
        self.user_moved = true;
    }

    /// Enter: opens what the ring is on. A stub (another project's task)
    /// swaps focus to its project, the way clicking it does.
    fn open_current(&mut self) {
        let frame = self.frame();
        if let Some(eye) = frame.selected {
            let cluster = self.scene.eyes[eye].cluster;
            if frame.focus.is_some_and(|c| c != cluster) {
                self.go_to(eye, true);
            } else if !self.inspecting {
                self.inspecting = true;
                self.load_detail();
            }
        } else if let Some(c) = frame.web_cursor {
            self.focus_on(c);
        }
    }

    /// Escape steps back one level: the inspector, then the ring, then the
    /// focused project (whose web keeps the arrow cursor), then that cursor.
    fn back_step(&mut self) {
        if self.inspector_open() {
            self.inspecting = false;
        } else if self.selected.is_some() {
            self.selected = None;
        } else if let Some(project) = self.focus.clone() {
            self.back_to_overview();
            self.web_cursor = Some(project);
        } else {
            self.web_cursor = None;
        }
    }

    fn follow(&mut self, dir: Dir, window: &mut Window, cx: &mut Context<Self>) {
        let Some(eye) = self.selected.as_ref().and_then(|k| self.scene.eye(k)) else {
            self.note("Select a task first; its links are what O and I follow");
            return;
        };
        let mut links = self.scene.links(eye, dir);
        links.retain(|&(_, other)| !self.is_hidden(other));
        match links.as_slice() {
            [] => {
                let way = match dir {
                    Dir::Out => "outgoing",
                    Dir::In => "incoming",
                };
                let reference = &self.scene.eyes[eye].task.reference;
                self.note(format!("{reference} has no {way} links"));
            }
            [(_, other)] => self.go_to(*other, self.inspecting),
            _ => self.open_picker(Mode::Follow(dir), window.focused(cx), window, cx),
        }
    }

    fn scroll_details(&mut self, pages: f32) {
        if !self.inspector_open() {
            return;
        }
        let s = &self.details_scroll;
        let page = f32::from(s.bounds().size.height) * 0.8;
        let lowest = -f32::from(s.max_offset().height);
        let y = (f32::from(s.offset().y) - pages * page).clamp(lowest, 0.0);
        s.set_offset(point(s.offset().x, px(y)));
    }

    fn picker_items(&self, mode: Mode, cx: &App) -> Vec<Item> {
        let focused = self.focus.as_ref().and_then(|k| self.scene.cluster(k));
        match mode {
            Mode::Commands => self.command_items(cx),
            Mode::Find => picker::task_items(&self.scene, focused, |e| !self.is_hidden(e)),
            Mode::Follow(dir) => self
                .selected
                .as_ref()
                .and_then(|k| self.scene.eye(k))
                .map(|eye| picker::link_items(&self.scene, eye, dir, |e| !self.is_hidden(e)))
                .unwrap_or_default(),
        }
    }

    /// What the palette offers right now: commands that apply here, then
    /// every project and workspace to jump to, then sources to retry.
    fn command_items(&self, cx: &App) -> Vec<Item> {
        let mut items = Vec::new();
        let mut command = |title: &str, action: Box<dyn Action>| {
            let hint = keys::hint(action.as_ref(), cx).unwrap_or_default();
            items.push(Item::new(title.into(), hint, "", Pick::Action(action)));
        };
        command("Find a task", Box::new(Find));
        if self.inspector_open() {
            command("Close task details", Box::new(Back));
        } else if self.selected.is_some() {
            command("Open task details", Box::new(Open));
        }
        if self.selected.is_some() {
            command("Follow a link out of this task", Box::new(FollowOut));
            command("Follow a link into this task", Box::new(FollowIn));
        }
        if self.focus.is_some() {
            command("Back to all workspaces", Box::new(ShowOverview));
        }
        command("Zoom in", Box::new(ZoomIn));
        command("Zoom out", Box::new(ZoomOut));
        if self.focus.is_some() {
            command("Fit project", Box::new(Fit));
        } else {
            // Not "Fit all": with a tall column in a small window the names
            // floor wins, and the rest of the column is a scroll away.
            command("Fit the overview", Box::new(Fit));
        }
        command("Re-layout", Box::new(Relayout));
        if !self.hidden_stages.is_empty() {
            command("Show every stage", Box::new(ShowAllStages));
        }
        for (key, name) in self.scene.stage_names() {
            let verb = match self.hidden_stages.contains(&key) {
                true => "Show",
                false => "Hide",
            };
            items.push(Item::new(
                format!("{verb} tasks in {name}"),
                "Stage filter".into(),
                "stage filter",
                Pick::Stage(key),
            ));
        }
        if let Some(file) = &self.keys_file {
            items.push(Item::new(
                "Reload keyboard shortcuts".into(),
                file.display().to_string(),
                "keys keybindings keymap",
                Pick::Action(Box::new(ReloadKeys)),
            ));
        }
        for cluster in &self.scene.clusters {
            let column = &self.scene.columns[cluster.column];
            items.push(Item::new(
                format!("Go to {} {}", cluster.project_key, cluster.name),
                format!("{} / {}", column.chip, column.title),
                "project",
                Pick::Project(cluster.key.clone()),
            ));
        }
        for column in self.scene.columns.iter().filter(|c| !c.clusters.is_empty()) {
            items.push(Item::new(
                format!("Show workspace {} / {}", column.chip, column.title),
                column.address.clone(),
                "",
                Pick::Workspace(column.key.clone()),
            ));
        }
        for source in &self.sources {
            if matches!(source.status, Status::Live | Status::Loading) {
                continue;
            }
            let name = match &source.key {
                SourceKey::Local => "local work".to_owned(),
                SourceKey::Server(name) => name.to_string(),
            };
            items.push(Item::new(
                format!("Retry {name} now"),
                source.address.clone(),
                "reconnect",
                Pick::Retry(source.key.clone()),
            ));
        }
        items
    }

    /// Opens the palette in `mode`, replacing one already open. `restore`
    /// gets focus back on dismissal.
    fn open_picker(
        &mut self,
        mode: Mode,
        restore: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let restore = match self.picker.is_some() {
            true => self.picker_return.take(),
            false => restore,
        };
        self.end_find();
        let items = self.picker_items(mode, cx);
        let input = cx.new(|cx| TextInput::new(picker::placeholder(mode), cx));
        self._picker_input = Some(cx.subscribe(&input, |this, _, _: &Changed, cx| {
            if let Some(picker) = &mut this.picker {
                picker.restart(cx);
            }
            this.after_refilter();
            cx.notify();
        }));
        window.focus(input.read(cx).focus_handle());
        let shown = (0..items.len()).collect();
        self.picker = Some(Picker {
            mode,
            input,
            items,
            shown,
            highlighted: 0,
            scroll: UniformListScrollHandle::new(),
        });
        self.picker_return = restore;
        if mode == Mode::Find {
            self.before_find = Some((self.camera, self.user_moved));
        }
        self.after_refilter();
    }

    /// New data while the palette is open: offer what's there now.
    fn refresh_picker(&mut self, cx: &mut Context<Self>) {
        let Some(mode) = self.picker.as_ref().map(|p| p.mode) else {
            return;
        };
        let items = self.picker_items(mode, cx);
        if let Some(picker) = &mut self.picker {
            picker.items = items;
            picker.refilter(cx);
        }
        self.after_refilter();
    }

    /// Find fades what doesn't match and keeps the highlighted task in view.
    fn after_refilter(&mut self) {
        let Some(picker) = self.picker.as_ref().filter(|p| p.mode == Mode::Find) else {
            self.matches = None;
            return;
        };
        let mut matches = vec![false; self.scene.eyes.len()];
        for &i in &picker.shown {
            if let Pick::Task(key) = &picker.items[i].pick
                && let Some(eye) = self.scene.eye(key)
            {
                matches[eye] = true;
            }
        }
        self.matches = Some(Rc::new(matches));
        self.show_preview();
    }

    fn show_preview(&mut self) {
        let frame = self.frame();
        if let Some(at) = frame.preview.and_then(|eye| frame.eye_at(eye)) {
            self.keep_visible(at, self.canvas_size());
        }
    }

    /// Clears Find's fading and puts back the view from before it opened.
    fn end_find(&mut self) {
        self.matches = None;
        if let Some((camera, moved)) = self.before_find.take() {
            self.camera = camera;
            self.user_moved = moved;
        }
    }

    fn dismiss_picker(&mut self, window: &mut Window) {
        self.picker = None;
        self._picker_input = None;
        self.end_find();
        let restore = self.picker_return.take();
        window.focus(restore.as_ref().unwrap_or(&self.focus_handle));
    }

    fn confirm_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        let Some(&i) = picker.shown.get(picker.highlighted) else {
            // Nothing matches; Enter does nothing until something does.
            self.picker = Some(picker);
            return;
        };
        let mode = picker.mode;
        let Some(item) = picker.items.into_iter().nth(i) else {
            return;
        };
        self._picker_input = None;
        self.picker_return = None;
        self.matches = None;
        self.before_find = None;
        // Always the canvas: Enter's key-up would click a focused button.
        window.focus(&self.focus_handle);
        match item.pick {
            Pick::Action(action) => window.dispatch_action(action, cx),
            Pick::Project(key) => {
                if let Some(c) = self.scene.cluster(&key) {
                    self.focus_on(c);
                }
            }
            Pick::Workspace(key) => self.show_workspace(&key),
            Pick::Retry(source) => self.runtime.send(Command::Retry(source)),
            Pick::Stage(key) => self.toggle_stage(key),
            Pick::Task(key) => {
                if let Some(eye) = self.scene.eye(&key) {
                    match mode {
                        // Found tasks open in their project, inspector and all.
                        Mode::Find => {
                            let cluster = self.scene.eyes[eye].cluster;
                            let focused = self.focus.as_ref().and_then(|k| self.scene.cluster(k));
                            if focused != Some(cluster) {
                                self.focus_on(cluster);
                            }
                            self.select(eye, true);
                        }
                        _ => self.go_to(eye, self.inspecting),
                    }
                }
            }
        }
    }

    fn step_picker(&mut self, by: isize) {
        if let Some(picker) = &mut self.picker {
            picker.step(by);
        }
        self.show_preview();
    }
}

impl Hyakume {
    /// Widens any column too narrow for its names at the lowest zoom the
    /// overview allows. Text needs a window to measure, so this runs here
    /// rather than in `rebuild`. Known positions stay; a wider column only
    /// slides its webs to its middle and pushes the later columns right.
    fn fit_names(&mut self, window: &Window) {
        if self.names_measured {
            return;
        }
        self.names_measured = true;
        let zoom = canvas::names_zoom(&self.scene, &self.placement);
        let widths = canvas::name_widths(window, &self.scene, zoom);
        let mut grew = false;
        for (column, width) in widths.into_iter().enumerate() {
            grew |= self.layout.widen(&self.scene, column, width);
        }
        if !grew {
            return;
        }
        let anchor = self.camera_anchor();
        let was = anchor.map(|c| self.placement.clusters[c].0);
        self.placement = Rc::new(self.layout.place(&self.scene));
        if let Some(c) = self.focus.as_ref().and_then(|k| self.scene.cluster(k)) {
            self.stubs = Rc::new(canvas::stub_positions(&self.scene, &self.placement, c));
        }
        // Whatever the person is looking at stays where it was on screen.
        if let (Some(c), Some(was)) = (anchor, was) {
            let moved = self.placement.clusters[c].0 - was;
            self.camera.pan(moved * -self.camera.zoom);
        }
        self.fitted_for = None;
    }

    /// The web the view is about: the open project, else the selected
    /// task's, else the one the arrows are on.
    fn camera_anchor(&self) -> Option<usize> {
        let frame = self.frame();
        frame
            .focus
            .or(frame.selected.map(|e| self.scene.eyes[e].cluster))
            .or(frame.web_cursor)
    }
}

impl Render for Hyakume {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.fit_names(window);
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
        let size = window.viewport_size();
        let size = Vec2::new(f32::from(size.width), f32::from(size.height));
        let window_resized = self.last_window.replace(size).is_some_and(|w| w != size);
        if answered && !self.user_moved && self.fitted_for != Some(viewport) {
            self.fit_into(viewport);
            self.fitted_for = Some(viewport);
        } else if let Some(prev) = resized_from
            && window_resized
        {
            // The person placed this view, so keep its middle where it was.
            // The details opening or closing leaves it alone instead: the
            // eye just clicked has to stay under the pointer.
            self.camera.pan((viewport - prev) * 0.5);
        }
        if resized_from.is_some() {
            self.keep_selected_visible(viewport);
        }
        let ring = self.keyboard.then_some(t.accent);
        let window_w = f32::from(window.viewport_size().width);

        let root = div()
            .key_context("Hyakume")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg)
            .text_color(t.ink)
            .text_size(px(13.0))
            .capture_any_mouse_down(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                if this.keyboard {
                    this.keyboard = false;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &OpenPalette, window, cx| {
                this.open_picker(Mode::Commands, window.focused(cx), window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Find, window, cx| {
                this.open_picker(Mode::Find, window.focused(cx), window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| {
                this.zoom_by(1.25);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| {
                this.zoom_by(1.0 / 1.25);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Fit, _, cx| {
                this.fit();
                // Fitting hands the view back to auto-fit.
                this.user_moved = false;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Relayout, _, cx| {
                this.relayout();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ShowAllStages, _, cx| {
                this.show_every_stage();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ShowOverview, window, cx| {
                this.back_to_overview();
                window.focus(&this.focus_handle);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ReloadKeys, _, cx| {
                let problems = keys::install(cx, this.keys_file.as_deref());
                this.report_keys(problems, true);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Back, window, cx| {
                this.back_step();
                // What had focus may be gone now (the inspector's buttons).
                window.focus(&this.focus_handle);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &FocusNext, window, _| {
                window.focus_next();
                this.inspector_stops.reveal(&this.details_scroll, window);
            }))
            .on_action(cx.listener(|this, _: &FocusPrev, window, _| {
                window.focus_prev();
                this.inspector_stops.reveal(&this.details_scroll, window);
            }))
            .on_action(cx.listener(|this, _: &ScrollDetailsUp, _, cx| {
                this.scroll_details(-1.0);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ScrollDetailsDown, _, cx| {
                this.scroll_details(1.0);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Open, _, cx| {
                this.open_current();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &FollowOut, window, cx| {
                this.follow(Dir::Out, window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &FollowIn, window, cx| {
                this.follow(Dir::In, window, cx);
                cx.notify();
            }))
            .child(self.header(window_w, ring, cx));

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

        let mut frame = self.frame();
        frame.ring = ring
            .filter(|_| self.focus_handle.is_focused(window))
            .map(|c| c.opacity(0.7));
        let bounds_cell = self.canvas_bounds.clone();
        let world = canvas(
            move |bounds, _, _| bounds_cell.set(bounds),
            move |bounds, _, window, cx| canvas::paint(bounds, &frame, window, cx),
        )
        .size_full();

        // Only this layer has the Canvas key context, so arrows and Enter
        // reach the canvas and never a focused button over it.
        let keyboard_layer = div()
            .id("canvas-keys")
            .absolute()
            .size_full()
            .key_context("Canvas")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &MoveLeft, _, cx| {
                this.move_cursor(Side::Left);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &MoveRight, _, cx| {
                this.move_cursor(Side::Right);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &MoveUp, _, cx| {
                this.move_cursor(Side::Up);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &MoveDown, _, cx| {
                this.move_cursor(Side::Down);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PanLeft, _, cx| {
                this.pan_by(PAN_STEP, 0.0);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PanRight, _, cx| {
                this.pan_by(-PAN_STEP, 0.0);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PanUp, _, cx| {
                this.pan_by(0.0, PAN_STEP);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PanDown, _, cx| {
                this.pan_by(0.0, -PAN_STEP);
                cx.notify();
            }))
            .child(world);

        let mut area = div()
            .id("canvas")
            .relative()
            .flex_1()
            .h_full()
            .min_w_0()
            .overflow_hidden()
            .cursor(if self.drag.as_ref().is_some_and(|d| d.moved) {
                CursorStyle::ClosedHand
            } else {
                CursorStyle::Arrow
            })
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_hover(cx.listener(|this, inside: &bool, _, cx| {
                if !inside && this.hovered.take().is_some() {
                    cx.notify();
                }
            }))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(keyboard_layer)
            .children(self.source_notes(ring, cx))
            .child(self.zoom_controls(ring, cx));
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
                .child(keys::canvas_hint(cx));
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
        if let Some((text, _)) = &self.notice {
            area = area.child(
                div()
                    .absolute()
                    .top(px(16.0))
                    .w_full()
                    .flex()
                    .justify_center()
                    .child(
                        pill(&t)
                            .px(px(16.0))
                            .py(px(8.0))
                            .max_w(px(canvas_w - 40.0))
                            .text_size(px(12.0))
                            .child(text.clone()),
                    ),
            );
        }

        let mut body = div().flex_1().min_h_0().flex().flex_row().child(area);
        if let Some(eye) = self
            .selected
            .as_ref()
            .and_then(|k| self.scene.eye(k))
            .filter(|_| self.inspecting)
        {
            let detail = self.selected.as_ref().and_then(|k| self.details.get(k));
            let close = cx.listener(|this: &mut Self, _: &ClickEvent, window, cx| {
                this.selected = None;
                this.inspecting = false;
                window.focus(&this.focus_handle);
                cx.notify();
            });
            let follow = cx.listener(|this: &mut Self, eye: &usize, window, cx| {
                this.go_to(*eye, true);
                window.focus(&this.focus_handle);
                cx.notify();
            });
            self.inspector_stops.prepare(&self.scene, eye, cx);
            body = body.child(inspector::panel(
                &self.scene,
                eye,
                detail,
                &t,
                &self.local,
                inspector_width(window_w),
                inspector::Hooks {
                    scroll: self.details_scroll.clone(),
                    stops: self.inspector_stops.clone(),
                    ring,
                    on_close: Box::new(close),
                    on_follow: Rc::new(move |eye, window, cx| follow(&eye, window, cx)),
                },
            ));
        }
        let root = root.child(body);
        match &self.picker {
            Some(picker) => root.child(self.picker_overlay(picker, window_w, cx)),
            None => root,
        }
    }
}

impl Hyakume {
    fn picker_overlay(
        &self,
        picker: &Picker,
        window_w: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let pick = cx.listener(|this: &mut Self, at: &usize, window, cx| {
            if let Some(picker) = &mut this.picker {
                picker.highlighted = *at;
            }
            this.confirm_picker(window, cx);
            cx.notify();
        });
        let width = (window_w - 32.0).min(600.0);
        // A click anywhere outside the palette closes it.
        div()
            .id("picker-backdrop")
            .absolute()
            .size_full()
            .flex()
            .justify_center()
            // Stretched, the panel's box would swallow clicks below it.
            .items_start()
            .pt(px(HEADER_H + 12.0))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.dismiss_picker(window);
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("picker")
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_action(cx.listener(|this, _: &PickerUp, _, cx| {
                        this.step_picker(-1);
                        cx.notify();
                    }))
                    .on_action(cx.listener(|this, _: &PickerDown, _, cx| {
                        this.step_picker(1);
                        cx.notify();
                    }))
                    .on_action(cx.listener(|this, _: &PickerConfirm, window, cx| {
                        this.confirm_picker(window, cx);
                        cx.notify();
                    }))
                    .on_action(cx.listener(|this, _: &PickerDismiss, window, cx| {
                        this.dismiss_picker(window);
                        cx.notify();
                    }))
                    .child(picker::view(
                        picker,
                        &self.theme,
                        width,
                        keys::picker_hint(cx),
                        Rc::new(move |at, window, cx| pick(&at, window, cx)),
                    )),
            )
    }

    fn header(
        &self,
        window_w: f32,
        ring: Option<Hsla>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
        let crumb = |id: &'static str| {
            div()
                .id(id)
                .px(px(4.0))
                .rounded(px(6.0))
                .border_2()
                .border_color(gpui::transparent_black())
                .text_color(t.muted)
                .cursor_pointer()
                .hover(move |s| s.text_color(t.ink))
                .tab_index(0)
                .when_some(ring, |c, ring| c.focus(move |s| s.border_color(ring)))
        };
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
                        crumb("crumb-home")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.back_to_overview();
                                cx.notify();
                            }))
                            .child("All workspaces"),
                    )
                    .child(div().text_color(t.muted).child("›"))
                    .child(
                        crumb("crumb-workspace")
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
        let roomy = window_w >= ROOMY_FOR_SHORTCUTS;
        let opener = |id: &'static str, label: &'static str, action: &dyn Action| {
            let hint = keys::hint(action, cx).filter(|_| roomy);
            button(&t, id, ring)
                .flex_shrink_0()
                .gap(px(8.0))
                .border_1()
                .border_color(t.line)
                .child(label)
                .children(hint.map(|h| div().text_color(t.muted).child(h)))
        };
        // While a stage filter is on, say so where it can't be missed, and
        // let one click (or Enter on it) bring everything back.
        let filter = (!self.hidden_stages.is_empty()).then(|| {
            let names: Vec<String> = self
                .scene
                .stage_names()
                .into_iter()
                .filter(|(key, _)| self.hidden_stages.contains(key))
                .map(|(_, name)| name)
                .collect();
            let what = match names.is_empty() {
                true => "Hiding some stages".to_owned(),
                false => format!("Hiding {}", names.join(", ")),
            };
            button(&t, "show-all-stages", ring)
                .flex_shrink_0()
                .max_w(px(260.0))
                .gap(px(8.0))
                .border_1()
                .border_color(t.accent)
                .on_click(cx.listener(|this, e: &ClickEvent, window, cx| {
                    this.show_every_stage();
                    this.after_click(e, window);
                    cx.notify();
                }))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .child(inspector::clean(&what, false)),
                )
                .child(div().text_color(t.muted).child("Show all"))
        });
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
            .children(filter)
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .gap(px(8.0))
                    .child(opener("open-find", "Find", &Find).on_click(cx.listener(
                        |this, e: &ClickEvent, window, cx| {
                            let back = this.return_focus(e, window, cx);
                            this.open_picker(Mode::Find, back, window, cx);
                            cx.notify();
                        },
                    )))
                    .child(
                        opener("open-palette", "Commands", &OpenPalette).on_click(cx.listener(
                            |this, e: &ClickEvent, window, cx| {
                                let back = this.return_focus(e, window, cx);
                                this.open_picker(Mode::Commands, back, window, cx);
                                cx.notify();
                            },
                        )),
                    ),
            )
    }

    /// Boxes under the header of each source that isn't live, with a retry
    /// button. Placed in canvas coordinates so they move with their column.
    fn source_notes(&self, ring: Option<Hsla>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        // While focused, only the focused project's source matters, and its
        // note sits in the corner since the column header isn't drawn.
        let focused = self
            .focus
            .as_ref()
            .and_then(|k| self.scene.cluster(k))
            .map(|c| self.scene.clusters[c].column);
        let t = self.theme;
        let frame = self.frame();
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
            let Some(&(x, width)) = self.placement.columns.get(i) else {
                continue;
            };
            // Below the column's webs (or its placeholder web), not on top of them.
            let below = column
                .clusters
                .clone()
                .map(|c| self.placement.clusters[c].0.y + self.placement.clusters[c].1 + 50.0)
                .fold(COLUMN_HEAD + 300.0, f32::max);
            // Never wider than its column, so two notes side by side don't
            // overlap. Too narrow for the message to read, the note keeps just
            // its title and Retry; the message is in the terminal too.
            let room = match focused {
                Some(_) => 300.0,
                None => ((width - 40.0) * self.camera.zoom - 16.0).min(300.0),
            };
            let compact = room < NOTE_MIN_W;
            let card_w = room.max(90.0);
            let message = if compact { String::new() } else { message };
            let at = match focused {
                Some(_) => Vec2::new(20.0, 20.0),
                // Below a tall column the note would be out of sight, its
                // Retry button with it, so then it moves up under the
                // first web's name, over the column's own greyed-out webs.
                None => {
                    // Under the last web's task count, which keeps its size.
                    let rim = self.camera.to_screen(Vec2::new(x + 40.0, below - 50.0));
                    let at = self.camera.to_screen(Vec2::new(x + 40.0, below));
                    let at = Vec2::new(at.x, at.y.max(rim.y + 36.0));
                    let canvas = self.canvas_size();
                    let note_h = note_height(&title, &message, card_w);
                    let lowest = canvas.y - self.bottom_reserve(canvas.x) - note_h;
                    // Past the headers and the first row of web names under them.
                    let under_headers = canvas::headers_bottom(&frame) + 28.0;
                    let y = if at.y <= lowest { at.y } else { under_headers };
                    Vec2::new(at.x, y.max(under_headers))
                }
            };
            let key = column.source.clone();
            notes.push(
                div()
                    .block_mouse_except_scroll()
                    .absolute()
                    .left(px(at.x))
                    .top(px(at.y))
                    .w(px(card_w))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .px(px(if compact { 10.0 } else { 18.0 }))
                    .py(px(if compact { 10.0 } else { 16.0 }))
                    .rounded(px(16.0))
                    .border_1()
                    .border_dashed()
                    .border_color(t.line)
                    .bg(t.surface)
                    .child(
                        div()
                            .text_size(px(if compact { 13.0 } else { 15.0 }))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(inspector::clean(&title, false)),
                    )
                    .when(!compact, |note| {
                        note.child(
                            div()
                                .text_size(px(12.0))
                                .line_height(px(18.0))
                                .text_color(t.muted)
                                .child(inspector::clean(&message, false)),
                        )
                    })
                    .child(
                        button(&t, format!("retry-{key}"), ring)
                            // The note goes away once the source starts loading.
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

    fn zoom_controls(&self, ring: Option<Hsla>, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let fit_label = if self.focus.is_some() {
            "Fit project"
        } else {
            "Fit"
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
                button(&t, "zoom-out", ring)
                    .w(px(34.0))
                    .on_click(cx.listener(|this, e: &ClickEvent, window, cx| {
                        this.zoom_by(1.0 / 1.25);
                        this.after_click(e, window);
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
                button(&t, "zoom-in", ring)
                    .w(px(34.0))
                    .on_click(cx.listener(|this, e: &ClickEvent, window, cx| {
                        this.zoom_by(1.25);
                        this.after_click(e, window);
                        cx.notify();
                    }))
                    .child("+"),
            )
            .child(div().w(px(1.0)).h(px(20.0)).mx(px(4.0)).bg(t.line))
            .child(
                button(&t, "fit", ring)
                    .on_click(cx.listener(|this, e: &ClickEvent, window, cx| {
                        this.fit();
                        // Fitting hands the view back to auto-fit.
                        this.user_moved = false;
                        this.after_click(e, window);
                        cx.notify();
                    }))
                    .child(fit_label),
            )
            .child(
                button(&t, "relayout", ring)
                    .on_click(cx.listener(|this, e: &ClickEvent, window, cx| {
                        this.relayout();
                        this.after_click(e, window);
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
                    let (x, y) = (f32::from(b.origin.x), f32::from(b.origin.y));
                    let curve =
                        glyphs::Curve::new((x + 2.0, y + 4.0), (x + 30.0, y + 5.0), 0.0, 0.0);
                    if let Some(curve) = curve {
                        glyphs::thread(window, curve, style);
                        if style.arrow {
                            glyphs::arrowhead(window, curve, style.color);
                        }
                    }
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

/// A button reachable with Tab. `ring` is the focus ring color while the
/// keyboard is in use; Enter or Space on a focused button clicks it.
fn button(t: &Theme, id: impl Into<SharedString>, ring: Option<Hsla>) -> gpui::Stateful<gpui::Div> {
    let raised = t.raised;
    div()
        .id(id.into())
        .h(px(34.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(17.0))
        .border_2()
        .border_color(gpui::transparent_black())
        .text_size(px(12.0))
        .cursor_pointer()
        .hover(move |s| s.bg(raised))
        .tab_index(0)
        .when_some(ring, |b, ring| b.focus(move |s| s.border_color(ring)))
}
