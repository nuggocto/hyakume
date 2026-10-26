//! The infinite canvas: a camera over world coordinates, and the painting
//! of columns, webs, threads, and eyes with detail that depends on zoom.

use std::collections::BTreeMap;
use std::rc::Rc;

use gpui::{
    App, BorderStyle, Bounds, FontWeight, Hsla, PathBuilder, Pixels, Point, Window, point, px,
    quad, size,
};

use crate::glyphs::{self, Align, Curve, EYE_ASPECT, EYE_BODY, EYE_REACH, ThreadStyle};
use crate::layout::{COLUMN_HEAD, EYE_W, Placement, Vec2};
use crate::scene::{Scene, Shape, ThreadKind};
use crate::sources::Status;
use crate::theme::Theme;

/// Below this zoom eyes are dots; above it they get their shapes.
const SHAPES_FROM: f32 = 0.45;
/// From this zoom on, eyes carry their reference, title, and stage.
const LABELS_FROM: f32 = 0.95;
/// How far outside a focused web its links to other projects are drawn.
const STUB_DISTANCE: f32 = 150.0;
/// Screen height of a column header: chip, then status line.
const HEADER_TEXT_H: f32 = 58.0;
/// Where the row of column headers ends on the overview, in canvas pixels.
/// Headers share one row, so the first column's says it for all of them.
pub fn headers_bottom(frame: &Frame) -> f32 {
    if frame.scene.columns.is_empty() {
        return 0.0;
    }
    let (head, _) = column_head(Bounds::default(), frame, 0);
    f32::from(head.y) + HEADER_TEXT_H
}
/// Screen space a web's count needs between it and the next web's name.
const COUNT_ROOM: f32 = 64.0;
/// Radius of the small torn web a stub sits on.
const STUB_WEB: f32 = 50.0;
/// Room between neighbouring stubs along their circle, in radians.
const STUB_GAP: f32 = 0.5;
/// Stubs stay this far (in radians) off straight up and straight down, where
/// the web's name and count sit; a thread from there would have to detour.
const STUB_CLEAR: f32 = 0.52;
/// Clear space a thread keeps around text, in screen pixels.
const LABEL_PAD: f32 = 6.0;
/// How far a thread's middle may be pulled sideways to get around text, and
/// in what steps. Past that it takes whichever route crosses the least.
const MAX_BEND: f32 = 640.0;
const BEND_STEP: f32 = 12.0;
/// Threads that aren't lit while something else is.
const FADED_THREAD: f32 = 0.12;
/// Bundles stand for several links each, so they rest a little quieter.
const BUNDLE_ALPHA: f32 = 0.6;
/// How far off the line between two webs, in radians, a bundle leaves one
/// and arrives at the other.
const BUNDLE_TILT: f32 = 0.8;
/// An opened project with more threads than this draws them fainter, so a
/// busy web reads as texture until something is lit.
const BUSY_THREADS: f32 = 12.0;
/// The overview never zooms out further than this, even with room to spare:
/// columns are made wide enough for their names at this zoom (`name_widths`),
/// and below it that would take very wide columns.
const OVERVIEW_MIN_ZOOM: f32 = 0.18;
/// Screen height a web's name needs above its web: the line sits 30 px over
/// the edge, plus padding, clear of the edge of the web above.
const NAME_BAND: f32 = 45.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    /// The world point at the canvas's top-left corner.
    pub origin: Vec2,
    pub zoom: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            origin: Vec2::default(),
            zoom: 0.6,
        }
    }
}

impl Camera {
    pub const MIN_ZOOM: f32 = 0.1;
    pub const MAX_ZOOM: f32 = 4.0;

    pub fn to_screen(self, world: Vec2) -> Vec2 {
        (world - self.origin) * self.zoom
    }

    pub fn to_world(self, screen: Vec2) -> Vec2 {
        screen * (1.0 / self.zoom) + self.origin
    }

    /// Zooms by `factor`, keeping the world point under `anchor` where it
    /// is, and never below `floor`.
    pub fn zoom_at(&mut self, anchor: Vec2, factor: f32, floor: f32) {
        let held = self.to_world(anchor);
        let floor = floor.clamp(Self::MIN_ZOOM, Self::MAX_ZOOM);
        self.zoom = (self.zoom * factor).clamp(floor, Self::MAX_ZOOM);
        self.origin = held - anchor * (1.0 / self.zoom);
    }

    pub fn pan(&mut self, screen_delta: Vec2) {
        self.origin -= screen_delta * (1.0 / self.zoom);
    }

    /// The camera that shows the world box `min..max` inside `viewport`.
    pub fn fit(min: Vec2, max: Vec2, viewport: Vec2, max_zoom: f32) -> Self {
        let pad = 48.0;
        let span = Vec2::new((max.x - min.x).max(1.0), (max.y - min.y).max(1.0));
        let zoom = ((viewport.x - 2.0 * pad) / span.x)
            .min((viewport.y - 2.0 * pad) / span.y)
            .clamp(Self::MIN_ZOOM, max_zoom);
        let middle = Vec2::new((min.x + max.x) / 2.0, (min.y + max.y) / 2.0);
        Self {
            origin: middle - viewport * (0.5 / zoom),
            zoom,
        }
    }
}

/// Everything one paint needs. Cheap to clone into GPUI's paint closure.
#[derive(Clone)]
pub struct Frame {
    pub scene: Rc<Scene>,
    pub placement: Rc<Placement>,
    pub camera: Camera,
    pub theme: Theme,
    /// The focused project's cluster, if not on the overview.
    pub focus: Option<usize>,
    pub selected: Option<usize>,
    /// While focused: eyes from other projects and where to draw them.
    pub stubs: Rc<Vec<(usize, Vec2)>>,
    /// The web the arrow keys are on, while the overview has no eye selected.
    pub web_cursor: Option<usize>,
    /// The task highlighted in Find, before it's picked.
    pub preview: Option<usize>,
    /// While Find is open: which eyes match. The rest are faded.
    pub matches: Option<Rc<Vec<bool>>>,
    /// Eyes whose stage is filtered out. They keep their spot but aren't
    /// drawn, hit, or reachable with the arrows.
    pub hidden: Option<Rc<Vec<bool>>>,
    /// Drawn around the canvas while it has keyboard focus.
    pub ring: Option<Hsla>,
    /// What's hovered, or else selected: lit with its links, the rest faded.
    pub spot: Option<Spot>,
}

impl Frame {
    /// Where an eye is drawn in this view, if it is drawn at all.
    pub fn eye_at(&self, eye: usize) -> Option<Vec2> {
        if self.is_hidden(eye) {
            return None;
        }
        match self.focus {
            Some(c) if self.scene.eyes[eye].cluster != c => self
                .stubs
                .iter()
                .find(|(e, _)| *e == eye)
                .map(|(_, at)| *at),
            _ => Some(self.placement.eyes[eye]),
        }
    }

    /// Every eye drawn in this view with its world position: all of them on
    /// the overview; the project's own plus its stubs while focused.
    pub fn drawn_eyes(&self) -> impl Iterator<Item = (usize, Vec2)> + '_ {
        (0..self.scene.eyes.len()).filter_map(|eye| self.eye_at(eye).map(|at| (eye, at)))
    }

    pub fn is_hidden(&self, eye: usize) -> bool {
        self.hidden
            .as_ref()
            .is_some_and(|h| h.get(eye).copied().unwrap_or(false))
    }

    fn shows_cluster(&self, cluster: usize) -> bool {
        self.focus.is_none_or(|c| c == cluster)
    }

    fn dimmed(&self, cluster: usize) -> bool {
        let column = self.scene.clusters[cluster].column;
        !matches!(
            self.scene.columns[column].status,
            Status::Live | Status::Loading
        )
    }
}

/// Where a focused project's links to other projects get drawn: outside the
/// web, roughly toward the project they come from, spread so they don't stack.
pub fn stub_positions(scene: &Scene, placement: &Placement, cluster: usize) -> Vec<(usize, Vec2)> {
    let (center, r) = placement.clusters[cluster];
    // Spread along the circle with the cones cut out, so a stub pushed along
    // to make room hops over a cone instead of landing in it.
    let mut along: Vec<(usize, f32)> = scene
        .outside_ends(cluster)
        .into_iter()
        .map(|eye| {
            let toward = placement.eyes[eye] - center;
            (eye, to_clear_arc(clear_of_text(toward.y.atan2(toward.x))))
        })
        .collect();
    along.sort_by(|a, b| a.1.total_cmp(&b.1));
    // With more stubs than fit at the usual spacing, they get closer, and
    // their circle grows so the small webs still don't touch.
    let total = 2.0 * CLEAR_ARC;
    let gap = STUB_GAP.min(total / along.len().max(1) as f32);
    for i in 1..along.len() {
        if along[i].1 - along[i - 1].1 < gap {
            along[i].1 = along[i - 1].1 + gap;
        }
    }
    // Pushed all the way round, the last ones would land on the first, so
    // then they share the arcs out evenly instead.
    if let (Some(&(_, first)), Some(&(_, last))) = (along.first(), along.last())
        && last - first > total - gap
    {
        let step = total / along.len() as f32;
        for (k, stub) in along.iter_mut().enumerate() {
            stub.1 = first + k as f32 * step;
        }
    }
    let reach = (r + STUB_DISTANCE).max(2.4 * STUB_WEB / gap);
    along
        .into_iter()
        .map(|(eye, u)| {
            let a = from_clear_arc(u);
            (eye, center + Vec2::new(a.cos(), a.sin()) * reach)
        })
        .collect()
}

/// Length of each of the two arcs, right and left, that stubs may use.
const CLEAR_ARC: f32 = std::f32::consts::PI - 2.0 * STUB_CLEAR;

/// An angle clear of both cones, as a distance along the clear arcs: the
/// right one from the top cone's edge, then the left one from the bottom's.
fn to_clear_arc(angle: f32) -> f32 {
    use std::f32::consts::{FRAC_PI_2, TAU};
    let right = angle - (STUB_CLEAR - FRAC_PI_2);
    // A little slack: an angle set on a cone's edge may round just past it.
    if (-1e-4..=CLEAR_ARC + 1e-4).contains(&right) {
        return right.clamp(0.0, CLEAR_ARC);
    }
    let left = angle.rem_euclid(TAU) - (FRAC_PI_2 + STUB_CLEAR);
    CLEAR_ARC + left.clamp(0.0, CLEAR_ARC)
}

fn from_clear_arc(along: f32) -> f32 {
    use std::f32::consts::FRAC_PI_2;
    let along = along.rem_euclid(2.0 * CLEAR_ARC);
    if along <= CLEAR_ARC {
        along + STUB_CLEAR - FRAC_PI_2
    } else {
        along - CLEAR_ARC + FRAC_PI_2 + STUB_CLEAR
    }
}

/// Below this zoom some web's name would touch the web above, since names
/// keep their size while webs shrink. The overview never goes further out,
/// so names never hide, and a bundle leaving the web above starts clear.
pub fn names_zoom(scene: &Scene, placement: &Placement) -> f32 {
    let mut zoom = OVERVIEW_MIN_ZOOM;
    for column in &scene.columns {
        for lower in column.clusters.clone() {
            let (center, r) = placement.clusters[lower];
            for upper in column.clusters.clone() {
                let (above, ar) = placement.clusters[upper];
                let room = (center.y - r) - (above.y + ar);
                if above.y < center.y && room > 0.0 {
                    zoom = zoom.max(NAME_BAND / room);
                }
            }
        }
    }
    zoom.min(Camera::MAX_ZOOM)
}

/// `angle` moved to the nearer edge of the cone above or below the web, if
/// it points into one. The side it was on is kept, so it still leans
/// toward the project it comes from.
fn clear_of_text(angle: f32) -> f32 {
    use std::f32::consts::FRAC_PI_2;
    for text in [-FRAC_PI_2, FRAC_PI_2] {
        let off = angle - text;
        if off.abs() < STUB_CLEAR {
            return text + STUB_CLEAR.copysign(off);
        }
    }
    angle
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Up,
    Down,
}

/// The candidate an arrow key lands on: ahead of `from` within 45° of the
/// arrow, nearest first, with sideways distance counting double so a
/// straight neighbour beats a closer diagonal one. If nothing is in that
/// cone, anything ahead will do, so no item is unreachable.
pub fn nearest(
    from: Vec2,
    side: Side,
    candidates: impl Iterator<Item = (usize, Vec2)>,
) -> Option<usize> {
    let mut in_cone: Option<(usize, f32)> = None;
    let mut ahead: Option<(usize, f32)> = None;
    for (i, at) in candidates {
        let d = at - from;
        let (along, across) = match side {
            Side::Left => (-d.x, d.y),
            Side::Right => (d.x, d.y),
            Side::Up => (-d.y, d.x),
            Side::Down => (d.y, d.x),
        };
        if along <= 0.5 {
            continue;
        }
        let score = along + 2.0 * across.abs();
        let best = if across.abs() <= along {
            &mut in_cone
        } else {
            &mut ahead
        };
        if best.is_none_or(|(_, s)| score < s) {
            *best = Some((i, score));
        }
    }
    in_cone.or(ahead).map(|(i, _)| i)
}

pub enum Hit {
    Eye(usize),
    /// While focused: a task from another project, or the small web it sits on.
    Stub(usize),
    Cluster(usize),
    /// A pinned header's band, which covers whatever is under it.
    Header,
    Nothing,
}

/// What's under a point given in canvas-local screen pixels.
pub fn hit(frame: &Frame, at: Vec2) -> Hit {
    // A pinned header's band hides what's under it, so it catches the pointer.
    if frame.focus.is_none() {
        let canvas = Bounds::default();
        let point = point(px(at.x), px(at.y));
        for column in 0..frame.scene.columns.len() {
            let (head, pinned) = column_head(canvas, frame, column);
            if pinned && header_band(canvas, frame, column, head).contains(&point) {
                return Hit::Header;
            }
        }
    }
    let world = frame.camera.to_world(at);
    let reach = (EYE_W * 0.7).max(10.0 / frame.camera.zoom);
    // Eyes too small to have shapes are too small to aim at; the web wins.
    let eyes = if frame.focus.is_none() && frame.camera.zoom < SHAPES_FROM {
        0
    } else {
        frame.scene.eyes.len()
    };
    let mut best: Option<(usize, f32)> = None;
    for eye in 0..eyes {
        let Some(pos) = frame.eye_at(eye) else {
            continue;
        };
        let d = (pos - world).len();
        if d < reach && best.is_none_or(|(_, bd)| d < bd) {
            best = Some((eye, d));
        }
    }
    if let Some((eye, _)) = best {
        let stub = frame
            .focus
            .is_some_and(|c| frame.scene.eyes[eye].cluster != c);
        return if stub { Hit::Stub(eye) } else { Hit::Eye(eye) };
    }
    for &(eye, at) in frame.stubs.iter() {
        if (world - at).len() < STUB_WEB {
            return Hit::Stub(eye);
        }
    }
    for (c, &(center, r)) in frame.placement.clusters.iter().enumerate() {
        if frame.shows_cluster(c) && (world - center).len() < r {
            return Hit::Cluster(c);
        }
    }
    Hit::Nothing
}

fn screen(bounds: Bounds<Pixels>, frame: &Frame, world: Vec2) -> Point<Pixels> {
    let s = frame.camera.to_screen(world);
    point(bounds.origin.x + px(s.x), bounds.origin.y + px(s.y))
}

pub fn paint(bounds: Bounds<Pixels>, frame: &Frame, window: &mut Window, cx: &mut App) {
    let theme = &frame.theme;
    window.paint_quad(gpui::fill(bounds, theme.bg));
    // Find's fading wins over the spotlight while it's open.
    let lit = frame
        .spot
        .filter(|_| frame.matches.is_none())
        .and_then(|spot| light(&frame.scene, spot));
    window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
        let mut texts = text_boxes(bounds, frame, window);
        let taken = [texts.headings.as_slice(), &texts.bands].concat();
        let placed = place_labels(bounds, frame, lit.as_ref(), &taken, window);
        texts.labels = placed.iter().map(Placed::rect).collect();
        dots(bounds, frame, window);
        let mut tops = Vec::new();
        if frame.focus.is_none() {
            columns(bounds, frame, window, cx);
            tops = bundle_threads(bounds, frame, lit.as_ref(), &texts, window);
        }
        for c in 0..frame.scene.clusters.len() {
            if frame.shows_cluster(c) {
                cluster(bounds, frame, c, window, cx);
            }
        }
        if frame.focus.is_some() {
            for &(_, at) in frame.stubs.iter() {
                let r = STUB_WEB * frame.camera.zoom;
                glyphs::web(window, screen(bounds, frame, at), r, true, 0.7, theme);
            }
        }
        // Threads pass under labels, but their arrowheads and names go on top.
        tops.extend(threads(bounds, frame, lit.as_ref(), &texts, window));
        let (picked, rest): (Vec<_>, Vec<_>) = placed.into_iter().partition(|p| p.picked);
        labels(bounds, frame, rest, window, cx);
        thread_tops(&tops, &texts.labels, &frame.theme, window, cx);
        eyes(bounds, frame, lit.as_ref(), window, cx);
        labels(bounds, frame, picked, window, cx);
        if frame.focus.is_none() {
            headers(bounds, frame, window, cx);
        }
    });
    if let Some(ring) = frame.ring {
        window.paint_quad(quad(
            bounds,
            px(0.0),
            gpui::transparent_black(),
            px(2.0),
            ring,
            BorderStyle::Solid,
        ));
    }
}

/// A dot grid anchored to the world, so panning visibly moves the canvas.
fn dots(bounds: Bounds<Pixels>, frame: &Frame, window: &mut Window) {
    let step = 28.0 * frame.camera.zoom;
    if step < 14.0 {
        return;
    }
    let w = f32::from(bounds.size.width);
    let h = f32::from(bounds.size.height);
    let first = frame.camera.to_screen(Vec2::new(
        (frame.camera.origin.x / 28.0).floor() * 28.0,
        (frame.camera.origin.y / 28.0).floor() * 28.0,
    ));
    let mut y = first.y;
    while y < h {
        let mut x = first.x;
        while x < w {
            let at = point(bounds.origin.x + px(x), bounds.origin.y + px(y));
            window.paint_quad(gpui::fill(
                Bounds::new(at, size(px(1.5), px(1.5))),
                frame.theme.dot,
            ));
            x += step;
        }
        y += step;
    }
}

/// Dividers between columns, and the placeholder web of an empty one. The
/// headers come last, in `headers`, so they can stay on top when pinned.
fn columns(bounds: Bounds<Pixels>, frame: &Frame, window: &mut Window, cx: &mut App) {
    let theme = &frame.theme;
    let bottom = frame.placement.bounds().1.y;
    for (i, column) in frame.scene.columns.iter().enumerate() {
        let (x, width) = frame.placement.columns[i];
        if i > 0 {
            let top = screen(bounds, frame, Vec2::new(x, 0.0));
            let end = screen(bounds, frame, Vec2::new(x, bottom));
            let mut divider = PathBuilder::stroke(px(1.0)).dash_array(&[px(4.0), px(4.0)]);
            divider.move_to(top);
            divider.line_to(end);
            if let Ok(path) = divider.build() {
                window.paint_path(path, theme.line);
            }
        }
        if column.clusters.is_empty() {
            // Nothing loaded from here (yet): a faded torn web holds its place,
            // under the header even when zoomed far out, where the header
            // keeps its size and the web shrinks.
            let r = 120.0 * frame.camera.zoom;
            let mut at = screen(
                bounds,
                frame,
                Vec2::new(x + width / 2.0, COLUMN_HEAD + 140.0),
            );
            let head = column_head(bounds, frame, i).0;
            at.y = at.y.max(head.y + px(HEADER_TEXT_H + r + 6.0));
            let live = matches!(column.status, Status::Live);
            glyphs::web(window, at, r, !live, if live { 1.0 } else { 0.6 }, theme);
            if live {
                let hint = "No projects yet";
                glyphs::text(
                    window,
                    cx,
                    hint,
                    point(at.x, at.y - px(8.0)),
                    13.0,
                    FontWeight::NORMAL,
                    theme.muted,
                    Align::Center,
                    Some(theme.bg),
                );
            }
        }
    }
}

/// Column headers, painted after everything else. Scrolled down, a header
/// stays pinned to the top edge, and then it gets a band of background so
/// webs, names, and threads pass under it instead of through its text.
fn headers(bounds: Bounds<Pixels>, frame: &Frame, window: &mut Window, cx: &mut App) {
    let theme = &frame.theme;
    for (i, column) in frame.scene.columns.iter().enumerate() {
        let HeadText {
            head,
            pinned,
            chip_w,
            title,
            status,
            dot,
        } = head_text(bounds, frame, i, window);
        if pinned {
            let band = header_band(bounds, frame, i, head);
            window.paint_quad(gpui::fill(band, theme.bg));
            window.paint_quad(gpui::fill(
                Bounds::new(band.bottom_left(), size(band.size.width, px(1.0))),
                theme.line,
            ));
        }
        let chip = Bounds::new(point(head.x, head.y), size(px(chip_w), px(22.0)));
        window.paint_quad(quad(
            chip,
            px(11.0),
            theme.surface,
            px(1.0),
            theme.line,
            BorderStyle::Solid,
        ));
        glyphs::text(
            window,
            cx,
            &column.chip,
            point(head.x + px(8.0), head.y + px(3.0)),
            12.0,
            FontWeight::SEMIBOLD,
            theme.ink,
            Align::Left,
            None,
        );
        if let Some(title) = title {
            glyphs::text(
                window,
                cx,
                &title,
                point(head.x + px(chip_w + 8.0), head.y + px(1.0)),
                15.0,
                FontWeight::SEMIBOLD,
                theme.ink,
                Align::Left,
                None,
            );
        }
        let line_y = head.y + px(30.0);
        glyphs::circle(
            window,
            point(head.x + px(4.0), line_y + px(8.0)),
            3.5,
            dot,
            None,
        );
        glyphs::text(
            window,
            cx,
            &status,
            point(head.x + px(14.0), line_y),
            12.0,
            FontWeight::NORMAL,
            theme.muted,
            Align::Left,
            None,
        );
    }
}

/// The strip a pinned header covers: its column's width, from the top edge
/// to just under the status line.
fn header_band(
    bounds: Bounds<Pixels>,
    frame: &Frame,
    column: usize,
    head: Point<Pixels>,
) -> Bounds<Pixels> {
    let (x, w) = frame.placement.columns[column];
    let left = screen(bounds, frame, Vec2::new(x, 0.0)).x;
    let right = screen(bounds, frame, Vec2::new(x + w, 0.0)).x;
    Bounds::from_corners(
        point(left, bounds.top()),
        point(right, head.y + px(HEADER_TEXT_H - 8.0)),
    )
}

pub fn status_line(status: Status, address: &str, theme: &Theme) -> (Hsla, String) {
    match status {
        Status::Loading => (theme.muted, format!("{address} · loading")),
        Status::Live if address == "On this computer" => (theme.live, address.to_owned()),
        Status::Live => (theme.live, format!("{address} · live")),
        Status::Offline { .. } => (theme.off, format!("{address} · offline, read-only")),
        Status::SignedOut(_) => (theme.off, format!("{address} · not signed in")),
        Status::Failed(_) => (theme.off, format!("{address} · error")),
    }
}

/// A column header's text, fitted to the column. Threads route around the
/// same text, so this one function decides what shows.
struct HeadText {
    head: Point<Pixels>,
    /// Held at the top edge because the column is scrolled down.
    pinned: bool,
    chip_w: f32,
    title: Option<String>,
    status: String,
    dot: Hsla,
}

fn head_text(bounds: Bounds<Pixels>, frame: &Frame, i: usize, window: &Window) -> HeadText {
    let column = &frame.scene.columns[i];
    let (_, width) = frame.placement.columns[i];
    // Header text keeps its size at any zoom, so it's shortened to the
    // column's on-screen width rather than spilling into the next column.
    let room = (width - 40.0) * frame.camera.zoom - 12.0;
    let chip_w = glyph_width(window, &column.chip, 12.0) + 16.0;
    let title = fit_text(window, &column.title, 15.0, room - chip_w - 8.0);
    let (dot, status) = status_line(column.status.clone(), &column.address, &frame.theme);
    let status = fit_text(window, &status, 12.0, room - 14.0).unwrap_or_default();
    let (head, pinned) = column_head(bounds, frame, i);
    HeadText {
        head,
        pinned,
        chip_w,
        title,
        status,
        dot,
    }
}

/// Where a column's header starts. Normally fixed in the world; zoomed out it
/// moves up to stay clear of the highest web's name, but never off the
/// canvas. Every header moves together, so they stay on one row, empty
/// columns included. Also says whether it's pinned to the top edge.
fn column_head(bounds: Bounds<Pixels>, frame: &Frame, column: usize) -> (Point<Pixels>, bool) {
    let (x, _) = frame.placement.columns[column];
    let mut head = screen(bounds, frame, Vec2::new(x + 40.0, 30.0));
    let zoom = frame.camera.zoom;
    let highest_name = frame
        .placement
        .clusters
        .iter()
        .map(|&(center, r)| screen(bounds, frame, center).y - px(r * zoom + 30.0))
        .min();
    if let Some(title) = highest_name {
        head.y = head.y.min(title - px(HEADER_TEXT_H));
    }
    let top = bounds.origin.y + px(8.0);
    let pinned = head.y < top;
    head.y = head.y.max(top);
    (head, pinned)
}

/// How wide each column has to be, in world units, for its workspace title
/// and every web name to show in full at `zoom`. It mirrors the room that
/// `head_text` and `web_text` give them.
pub fn name_widths(window: &Window, scene: &Scene, zoom: f32) -> Vec<f32> {
    scene
        .columns
        .iter()
        .map(|column| {
            let chip_w = glyph_width(window, &column.chip, 12.0) + 16.0;
            let title_w = glyph_width(window, &column.title, 15.0) * 1.05;
            let header = (chip_w + 8.0 + title_w + 12.0) / zoom + 40.0;
            column
                .clusters
                .clone()
                .map(|c| {
                    let cluster = &scene.clusters[c];
                    let key_w = glyph_width(window, &cluster.project_key, 13.0);
                    let name_w = glyph_width(window, &cluster.name, 13.0);
                    (key_w + 6.0 + name_w + 16.0) / zoom
                })
                .fold(header, f32::max)
        })
        .collect()
}

/// `text` cut short with "…" to fit `max` pixels, or `None` if not even that fits.
fn fit_text(window: &Window, text: &str, size_px: f32, max: f32) -> Option<String> {
    // Measured at normal weight; a little slack covers semibold text.
    let fits = |s: &str| glyph_width(window, s, size_px) * 1.05 <= max;
    if fits(text) {
        return Some(text.to_owned());
    }
    let chars: Vec<char> = text.chars().collect();
    (1..chars.len())
        .rev()
        .map(|n| chars[..n].iter().collect::<String>() + "…")
        .find(|s| fits(s))
}

fn glyph_width(window: &Window, text: &str, size_px: f32) -> f32 {
    let font = window.text_style().font();
    let run = gpui::TextRun {
        len: text.len(),
        font,
        color: gpui::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    f32::from(
        window
            .text_system()
            .shape_line(text.to_owned().into(), px(size_px), &[run], None)
            .width,
    )
}

fn cluster(bounds: Bounds<Pixels>, frame: &Frame, c: usize, window: &mut Window, cx: &mut App) {
    let theme = &frame.theme;
    let (center, r) = frame.placement.clusters[c];
    let alpha = if frame.dimmed(c) { 0.55 } else { 1.0 };
    let at = screen(bounds, frame, center);
    let rs = r * frame.camera.zoom;
    glyphs::web(window, at, rs, frame.dimmed(c), alpha, theme);
    if frame.web_cursor == Some(c) {
        glyphs::circle(
            window,
            at,
            rs + 8.0,
            gpui::transparent_black(),
            Some((2.0, theme.accent, BorderStyle::Solid)),
        );
    }

    let cluster = &frame.scene.clusters[c];
    let ink = theme.ink.opacity(alpha);
    let muted = theme.muted.opacity(alpha);
    let text = web_text(bounds, frame, c, window);
    if let Some(title) = text.title {
        glyphs::text(
            window,
            cx,
            &cluster.project_key,
            title.at,
            13.0,
            FontWeight::BOLD,
            ink,
            Align::Left,
            None,
        );
        glyphs::text(
            window,
            cx,
            title.name,
            point(title.at.x + px(title.key_w + 6.0), title.at.y),
            13.0,
            FontWeight::NORMAL,
            muted,
            Align::Left,
            None,
        );
    }
    if let Some(count) = text.count {
        glyphs::text(
            window,
            cx,
            &count.text,
            count.at,
            12.0,
            FontWeight::NORMAL,
            muted,
            Align::Center,
            None,
        );
    }
}

/// A web's name line above it and task count below, or `None` where they
/// don't fit. Painting and thread routing both read this.
struct WebText<'a> {
    title: Option<WebTitle<'a>>,
    count: Option<WebCount>,
}

struct WebTitle<'a> {
    /// Top left of the line.
    at: Point<Pixels>,
    key_w: f32,
    /// Empty when only the project key fits.
    name: &'a str,
    width: f32,
}

struct WebCount {
    /// Top middle of the line.
    at: Point<Pixels>,
    text: String,
}

fn web_text<'a>(
    bounds: Bounds<Pixels>,
    frame: &'a Frame,
    c: usize,
    window: &Window,
) -> WebText<'a> {
    let (center, r) = frame.placement.clusters[c];
    let at = screen(bounds, frame, center);
    let rs = r * frame.camera.zoom;
    let cluster = &frame.scene.clusters[c];
    let title_y = at.y - px(rs + 30.0);
    // Zoomed out, the name may not fit its column, or may run into the header.
    let room = frame.placement.columns[cluster.column].1 * frame.camera.zoom - 16.0;
    let first = frame.scene.columns[cluster.column].clusters.start == c;
    let under_header = first
        && frame.focus.is_none()
        && title_y < column_head(bounds, frame, cluster.column).0.y + px(HEADER_TEXT_H);
    let key_w = glyph_width(window, &cluster.project_key, 13.0);
    let full_w = key_w + 6.0 + glyph_width(window, &cluster.name, 13.0);
    let (name, width) = if full_w <= room {
        (cluster.name.as_str(), full_w)
    } else {
        ("", key_w)
    };
    let title = (key_w <= room && !under_header).then(|| WebTitle {
        at: point(at.x - px(width / 2.0), title_y),
        key_w,
        name,
        width,
    });

    // Skip the count when the next web's name would sit on it (zoomed out).
    // An opened project shows no other web, so nothing can.
    let column = &frame.scene.columns[cluster.column];
    let crowded = frame.focus.is_none()
        && column.clusters.clone().any(|other| {
            let (oc, or) = frame.placement.clusters[other];
            let gap = ((oc.y - or) - (center.y + r)) * frame.camera.zoom;
            oc.y > center.y && gap < COUNT_ROOM
        });
    let count = (!crowded).then(|| {
        let n = cluster.eyes.len();
        let shown = cluster
            .eyes
            .clone()
            .filter(|&e| !frame.is_hidden(e))
            .count();
        let mut text = match shown == n {
            true => format!("{n} task{}", if n == 1 { "" } else { "s" }),
            false => format!("{shown} of {n} tasks"),
        };
        if cluster.blocked > 0 {
            text.push_str(&format!(" · {} blocked", cluster.blocked));
        }
        WebCount {
            at: point(at.x, at.y + px(rs + 10.0)),
            text,
        }
    });
    WebText { title, count }
}

/// A box of text that threads keep clear of, padded, in screen pixels.
#[derive(Clone, Copy, Debug)]
struct Rect {
    min: (f32, f32),
    max: (f32, f32),
}

impl Rect {
    fn around(top_left: Point<Pixels>, w: f32, h: f32) -> Self {
        let (x, y) = (f32::from(top_left.x), f32::from(top_left.y));
        Self {
            min: (x - LABEL_PAD, y - LABEL_PAD),
            max: (x + w + LABEL_PAD, y + h + LABEL_PAD),
        }
    }

    fn contains(&self, (x, y): (f32, f32)) -> bool {
        (self.min.0..=self.max.0).contains(&x) && (self.min.1..=self.max.1).contains(&y)
    }

    /// How far `at` is from the box; zero inside it.
    fn distance(&self, at: Point<Pixels>) -> f32 {
        let (x, y) = (f32::from(at.x), f32::from(at.y));
        let dx = (self.min.0 - x).max(x - self.max.0).max(0.0);
        let dy = (self.min.1 - y).max(y - self.max.1).max(0.0);
        dx.hypot(dy)
    }

    fn overlaps(&self, other: &Self) -> bool {
        self.min.0 <= other.max.0
            && other.min.0 <= self.max.0
            && self.min.1 <= other.max.1
            && other.min.1 <= self.max.1
    }

    /// Whether it touches the circle around `center` with radius `r`.
    fn meets_circle(&self, (cx, cy): (f32, f32), r: f32) -> bool {
        let x = cx.clamp(self.min.0, self.max.0);
        let y = cy.clamp(self.min.1, self.max.1);
        (x - cx).hypot(y - cy) < r
    }
}

/// The text in this view. Threads never cross headings: column headers, web
/// names, and counts. They do pass behind eye labels, which sit on plates,
/// but the name pill on a link keeps off both.
struct Texts {
    headings: Vec<Rect>,
    labels: Vec<Rect>,
    /// Pinned headers' bands: nothing is placed under them, since it would
    /// be hidden, but threads may pass beneath.
    bands: Vec<Rect>,
}

fn text_boxes(bounds: Bounds<Pixels>, frame: &Frame, window: &Window) -> Texts {
    let mut headings = Vec::new();
    let mut bands = Vec::new();
    if frame.focus.is_none() {
        for i in 0..frame.scene.columns.len() {
            let head = head_text(bounds, frame, i, window);
            if head.pinned {
                let band = header_band(bounds, frame, i, head.head);
                bands.push(Rect {
                    min: (f32::from(band.left()), f32::from(band.top())),
                    max: (f32::from(band.right()), f32::from(band.bottom())),
                });
            }
            let title_w = head
                .title
                .as_deref()
                .map_or(0.0, |t| 8.0 + glyph_width(window, t, 15.0));
            let status_w = 14.0 + glyph_width(window, &head.status, 12.0);
            let w = (head.chip_w + title_w).max(status_w);
            headings.push(Rect::around(head.head, w, 30.0 + 12.0 * 1.3));
        }
    }
    for c in (0..frame.scene.clusters.len()).filter(|&c| frame.shows_cluster(c)) {
        let text = web_text(bounds, frame, c, window);
        if let Some(title) = text.title {
            headings.push(Rect::around(title.at, title.width, 13.0 * 1.3));
        }
        if let Some(count) = text.count {
            let w = glyph_width(window, &count.text, 12.0);
            let at = point(count.at.x - px(w / 2.0), count.at.y);
            headings.push(Rect::around(at, w, 12.0 * 1.3));
        }
    }
    Texts {
        headings,
        labels: Vec::new(),
        bands,
    }
}

/// An eye's label that made it onto the screen, and where.
struct Placed {
    eye: usize,
    stub: bool,
    block: LabelBlock,
    /// Top left of the text.
    top: Point<Pixels>,
    /// The selected or hovered task's, painted over the eyes it may cover.
    picked: bool,
    /// Set when the label is a card out beside the web, joined to its eye
    /// at this point by a thin line.
    leader: Option<Point<Pixels>>,
}

impl Placed {
    fn rect(&self) -> Rect {
        Rect::around(self.top, self.block.width, self.block.height)
    }
}

/// Labels go in order (picked task, lit ones, the rest), and one that would
/// cover a heading, an eye, or a placed label is left out; zooming in makes
/// room. The selected or hovered task's shows at any zoom, like a tooltip,
/// and may cover eyes but never a heading.
fn place_labels(
    bounds: Bounds<Pixels>,
    frame: &Frame,
    lit: Option<&Light>,
    headings: &[Rect],
    window: &Window,
) -> Vec<Placed> {
    let zoom = frame.camera.zoom;
    let (w, h) = eye_size(zoom);
    let picked = |i: usize| {
        frame.selected == Some(i) || frame.preview == Some(i) || frame.spot == Some(Spot::Eye(i))
    };
    // Every eye on screen is in the way, even below the zoom where only a
    // picked one gets a label.
    let visible: Vec<(usize, Point<Pixels>, bool)> = visible_eyes(bounds, frame).collect();
    let mut labelled: Vec<(usize, Point<Pixels>, bool)> = visible
        .iter()
        .copied()
        .filter(|&(i, _, _)| zoom >= LABELS_FROM || picked(i))
        .collect();
    let eye_box = |at: Point<Pixels>| Rect {
        min: (f32::from(at.x) - w / 2.0, f32::from(at.y) - h / 2.0),
        max: (f32::from(at.x) + w / 2.0, f32::from(at.y) + h / 2.0),
    };
    let first = |i: usize| (!picked(i), !lit.is_some_and(|l| l.eyes[i]), i);
    labelled.sort_by_key(|&(i, _, _)| first(i));
    let mut placed: Vec<Placed> = Vec::new();
    for &(i, at, stub) in &labelled {
        let theme = eye_theme(frame, lit, i);
        // Zoomed out only a picked task has a label: one line covers less.
        let compact = zoom < LABELS_FROM;
        let block = label_block(window, frame, &theme, i, stub, compact);
        let (bw, bh) = (block.width, block.height);
        // Under the eye reads best; above, right, left, then the corners.
        let room = 6.0 + LABEL_PAD;
        let (below, above) = (at.y + px(h / 2.0 + 6.0), at.y - px(h / 2.0 + room + bh));
        let (right, left) = (at.x + px(w / 2.0 + room), at.x - px(w / 2.0 + room + bw));
        let middle = at.x - px(bw / 2.0);
        let spots = [
            point(middle, below),
            point(middle, above),
            point(right, at.y - px(bh / 2.0)),
            point(left, at.y - px(bh / 2.0)),
            point(right, below),
            point(left, below),
            point(right, above),
            point(left, above),
        ];
        // How many eyes a spot would cover, a lit one counting for many;
        // `None` if it would cover a heading or a label already placed.
        let covers = |top: Point<Pixels>| {
            let rect = Rect::around(top, bw, bh);
            if headings.iter().any(|b| b.overlaps(&rect))
                || placed.iter().any(|p| p.rect().overlaps(&rect))
            {
                return None;
            }
            let eyes = visible
                .iter()
                .filter(|&&(other, o, _)| other != i && eye_box(o).overlaps(&rect))
                .map(|&(other, _, _)| {
                    if lit.is_some_and(|l| l.eyes[other]) {
                        100
                    } else {
                        1
                    }
                })
                .sum::<usize>();
            Some(eyes)
        };
        // A label has to sit nearer its own eye than any other, or it reads
        // as a neighbour's: a corner spot can end up right under the next eye.
        let own = |top: Point<Pixels>| {
            let text = Rect {
                min: (f32::from(top.x), f32::from(top.y)),
                max: (f32::from(top.x) + bw, f32::from(top.y) + bh),
            };
            let mine = text.distance(at);
            visible
                .iter()
                .all(|&(other, o, _)| other == i || text.distance(o) > mine)
        };
        let mut leader = None;
        let spot = match picked(i) {
            // A tooltip keeps off lit eyes first, then reads as its own,
            // then covers as little as it can.
            true => {
                let best = spots
                    .into_iter()
                    .filter_map(|top| Some((covers(top)?, top)))
                    .min_by_key(|&(n, top)| (n >= 100, !own(top), n));
                match best {
                    // Nowhere near the eye works: out beside the web instead.
                    Some((n, top)) if (n >= 100 || !own(top)) && !stub => {
                        // Nor over the eye's own links, which it's there to explain.
                        let links: Vec<_> = links_of(bounds, frame, i);
                        let taken = |rect: &Rect| {
                            headings.iter().any(|b| b.overlaps(rect))
                                || placed.iter().any(|p| p.rect().overlaps(rect))
                                || links.iter().any(|&(a, b)| {
                                    (0..=40).any(|k| {
                                        let t = k as f32 / 40.0;
                                        rect.contains((
                                            a.0 + (b.0 - a.0) * t,
                                            a.1 + (b.1 - a.1) * t,
                                        ))
                                    })
                                })
                        };
                        let card = card_spot(bounds, frame, i, at, (bw, bh), taken);
                        leader = card.map(|_| at);
                        card.or(Some(top))
                    }
                    best => best.map(|(_, top)| top),
                }
            }
            false => spots
                .into_iter()
                .find(|&top| covers(top) == Some(0) && own(top)),
        };
        if let Some(top) = spot {
            placed.push(Placed {
                eye: i,
                stub,
                block,
                top,
                picked: picked(i),
                leader,
            });
        }
    }
    placed
}

/// Each link of `eye` as a straight line between its two drawn ends, close
/// enough to its curve for keeping a card off it.
fn links_of(bounds: Bounds<Pixels>, frame: &Frame, eye: usize) -> Vec<((f32, f32), (f32, f32))> {
    let at = |e: usize| {
        let p = screen(bounds, frame, frame.eye_at(e)?);
        Some((f32::from(p.x), f32::from(p.y)))
    };
    frame
        .scene
        .threads
        .iter()
        .filter(|t| t.from == eye || t.to == eye)
        .filter_map(|t| Some((at(t.from)?, at(t.to)?)))
        .collect()
}

/// Where a tooltip card goes when there's no room near its eye: just outside
/// the eye's web, in the eye's direction from the middle, or turned a little
/// either way until it's on the canvas and clear of headings and labels.
fn card_spot(
    bounds: Bounds<Pixels>,
    frame: &Frame,
    eye: usize,
    at: Point<Pixels>,
    (bw, bh): (f32, f32),
    taken: impl Fn(&Rect) -> bool,
) -> Option<Point<Pixels>> {
    let (center, r) = frame.placement.clusters[frame.scene.eyes[eye].cluster];
    let c = screen(bounds, frame, center);
    let (cx, cy) = (f32::from(c.x), f32::from(c.y));
    let (dx, dy) = (f32::from(at.x) - cx, f32::from(at.y) - cy);
    let base = if dx.hypot(dy) < 1.0 {
        0.0
    } else {
        dy.atan2(dx)
    };
    let reach = r * frame.camera.zoom + 16.0;
    // Turned a step at a time, alternating sides, all the way round.
    (0..18)
        .map(|k| (k + 1) / 2 * if k % 2 == 0 { 1 } else { -1 })
        .map(|step| step as f32 * 0.35)
        .map(|turn| {
            let (ux, uy) = ((base + turn).cos(), (base + turn).sin());
            // The card's near edge sits on the circle `reach` out.
            let mid = (
                cx + ux * (reach + bw / 2.0 + 8.0),
                cy + uy * (reach + bh / 2.0 + 6.0),
            );
            point(px(mid.0 - bw / 2.0), px(mid.1 - bh / 2.0))
        })
        .find(|&top| {
            let rect = Rect::around(top, bw, bh);
            // All of it on the canvas: a card half under the details panel
            // or off an edge can't be read.
            let inside = rect.min.0 >= f32::from(bounds.left())
                && rect.max.0 <= f32::from(bounds.right())
                && rect.min.1 >= f32::from(bounds.top())
                && rect.max.1 <= f32::from(bounds.bottom());
            inside && !taken(&rect)
        })
}

/// What the pointer or the selection is on. It and everything it links to
/// stay bright while the rest fades back, so one task's links can be read
/// even in a crowded web.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spot {
    Eye(usize),
    Web(usize),
}

/// What a spot lights up, by eye and by thread.
struct Light {
    eyes: Vec<bool>,
    threads: Vec<bool>,
}

/// `None` when the spot has no links: there's nothing to pick out then, and
/// fading everything else would only flicker as the pointer moves.
fn light(scene: &Scene, spot: Spot) -> Option<Light> {
    let touches = |e: usize| match spot {
        Spot::Eye(s) => e == s,
        Spot::Web(c) => scene.eyes[e].cluster == c,
    };
    let threads: Vec<bool> = scene
        .threads
        .iter()
        .map(|t| touches(t.from) || touches(t.to))
        .collect();
    if !threads.contains(&true) {
        return None;
    }
    let mut eyes: Vec<bool> = (0..scene.eyes.len()).map(touches).collect();
    for (t, _) in scene.threads.iter().zip(&threads).filter(|(_, lit)| **lit) {
        eyes[t.from] = true;
        eyes[t.to] = true;
    }
    Some(Light { eyes, threads })
}

/// The links between two projects, counted either way, which the overview
/// draws as one thread instead of one per link.
#[derive(Debug, PartialEq, Eq)]
struct Bundle {
    webs: (usize, usize),
    links: usize,
}

/// Only links with both ends `shown` count, so a stage filter thins them too.
fn bundles(scene: &Scene, shown: impl Fn(usize) -> bool) -> Vec<Bundle> {
    let mut counts: BTreeMap<(usize, usize), usize> = BTreeMap::new();
    for t in scene
        .threads
        .iter()
        .filter(|t| shown(t.from) && shown(t.to))
    {
        let (a, b) = (scene.eyes[t.from].cluster, scene.eyes[t.to].cluster);
        if a != b {
            *counts.entry((a.min(b), a.max(b))).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .map(|(webs, links)| Bundle { webs, links })
        .collect()
}

/// How far to pull the middle of a thread sideways so it crosses no heading
/// or web in `webs`, and stays inside `column`, its workspace's left and
/// right edge. A thread with nothing in its way keeps its sag; one that
/// can't get clear takes the route that crosses least.
fn route(
    from: (f32, f32),
    to: (f32, f32),
    gaps: (f32, f32),
    column: Option<(f32, f32)>,
    headings: &[Rect],
    webs: &[((f32, f32), f32)],
) -> f32 {
    // Only text near the thread matters; the widest bend sets how near.
    let reach = MAX_BEND / 2.0 + 60.0;
    let span = Rect {
        min: (from.0.min(to.0) - reach, from.1.min(to.1) - reach),
        max: (from.0.max(to.0) + reach, from.1.max(to.1) + reach),
    };
    let near: Vec<&Rect> = headings.iter().filter(|b| b.overlaps(&span)).collect();
    let near_webs: Vec<_> = webs
        .iter()
        .filter(|&&(c, r)| span.meets_circle(c, r))
        .collect();
    let on_web = |(x, y): (f32, f32)| {
        near_webs
            .iter()
            .any(|&&((cx, cy), r)| (x - cx).hypot(y - cy) < r)
    };
    let outside = |x: f32| column.is_some_and(|(left, right)| x < left || x > right);
    let cost = |bend: f32| -> f32 {
        let Some(curve) = Curve::between(from, to, gaps, bend) else {
            return 0.0;
        };
        // Steps of a few pixels, smaller than any padded box, so none slips
        // between two samples.
        let (s, c, e) = (curve.start, curve.control, curve.end);
        let length = (c.0 - s.0).hypot(c.1 - s.1) + (e.0 - c.0).hypot(e.1 - c.1);
        let steps = (length / 4.0).ceil().max(1.0) as usize;
        let crossed = (0..=steps)
            .map(|i| curve.at(i as f32 / steps as f32))
            .filter(|&p| outside(p.0) || on_web(p) || near.iter().any(|b| b.contains(p)))
            .count();
        // A bend costs less than one crossed sample, so it only breaks ties.
        crossed as f32 + bend.abs() / MAX_BEND * 0.5
    };
    let mut best = (cost(0.0), 0.0);
    let mut bend = BEND_STEP;
    // Once bending alone costs more than the best route, nothing further out wins.
    while bend <= MAX_BEND && bend / MAX_BEND * 0.5 < best.0 {
        for tried in [bend, -bend] {
            let c = cost(tried);
            if c < best.0 {
                best = (c, tried);
            }
        }
        bend += BEND_STEP;
    }
    best.1
}

/// "Blocks" is the firmest thread. Other links use the same ink a little
/// lighter, so they stay readable on the web instead of melting into it.
pub fn thread_style(kind: ThreadKind, theme: &Theme) -> ThreadStyle {
    match kind {
        ThreadKind::Blocks => ThreadStyle {
            color: theme.link,
            width: 1.8,
            dash: None,
            arrow: true,
        },
        ThreadKind::Directed => ThreadStyle {
            color: theme.link.opacity(0.75),
            width: 1.5,
            dash: None,
            arrow: true,
        },
        ThreadKind::Undirected => ThreadStyle {
            color: theme.link.opacity(0.75),
            width: 1.6,
            dash: Some((6.0, 4.0)),
            arrow: false,
        },
    }
}

/// The parts of a thread drawn over the labels: its arrowhead, and a pill
/// with its name or link count where one fits without covering anything.
struct ThreadTop {
    curve: Curve,
    style: ThreadStyle,
    pill: Option<(String, (f32, f32))>,
}

const PILL_H: f32 = 20.0;

/// Where along `curve` a pill `w` wide covers no text and no web, trying
/// the middle first. `None` if nowhere does: no pill beats one on top of
/// something. With a `column`, the pill also stays inside it, slid sideways
/// if it has to, as long as its thread still runs through it.
fn pill_spot(
    curve: &Curve,
    w: f32,
    texts: &Texts,
    webs: &[((f32, f32), f32)],
    column: Option<(f32, f32)>,
) -> Option<(f32, f32)> {
    [0.5, 0.4, 0.6, 0.3, 0.7, 0.2, 0.8]
        .into_iter()
        .find_map(|t| {
            let (on, y) = curve.at(t);
            let x = match column {
                Some((left, right)) if right - left >= w => {
                    on.clamp(left + w / 2.0, right - w / 2.0)
                }
                Some(_) => return None,
                None => on,
            };
            if (x - on).abs() > w / 2.0 - 6.0 {
                return None;
            }
            let pill = Rect {
                min: (x - w / 2.0, y - PILL_H / 2.0),
                max: (x + w / 2.0, y + PILL_H / 2.0),
            };
            let covers = texts
                .headings
                .iter()
                .chain(&texts.labels)
                .chain(&texts.bands)
                .any(|b| b.overlaps(&pill))
                // Eyes sit within 0.8 of a web's radius; its outer silk may go under.
            || webs.iter().any(|&(c, r)| pill.meets_circle(c, r * 0.85));
            (!covers).then_some((x, y))
        })
}

/// Every web on screen, and every stub's small web, as circles.
fn web_circles(bounds: Bounds<Pixels>, frame: &Frame) -> Vec<((f32, f32), f32)> {
    let z = frame.camera.zoom;
    let at = |world: Vec2| {
        let p = screen(bounds, frame, world);
        (f32::from(p.x), f32::from(p.y))
    };
    let webs = (0..frame.scene.clusters.len())
        .filter(|&c| frame.shows_cluster(c))
        .map(|c| {
            (
                at(frame.placement.clusters[c].0),
                frame.placement.clusters[c].1 * z,
            )
        });
    let stubs = frame.stubs.iter().map(|&(_, w)| (at(w), STUB_WEB * z));
    webs.chain(stubs).collect()
}

/// The overview's webs in `column` other than the two a thread joins: it
/// steers around them, or else passes behind them.
fn between_webs(
    frame: &Frame,
    webs: &[((f32, f32), f32)],
    column: usize,
    ends: [usize; 2],
) -> Vec<((f32, f32), f32)> {
    frame.scene.columns[column]
        .clusters
        .clone()
        .filter(|c| !ends.contains(c))
        .map(|c| webs[c])
        .collect()
}

/// A column's left and right edge on screen, a little inside its dividers.
fn column_span(bounds: Bounds<Pixels>, frame: &Frame, column: usize) -> (f32, f32) {
    let (x, w) = frame.placement.columns[column];
    let left = screen(bounds, frame, Vec2::new(x, 0.0)).x;
    let right = screen(bounds, frame, Vec2::new(x + w, 0.0)).x;
    (f32::from(left) + 4.0, f32::from(right) - 4.0)
}

/// Paints `curve` clipped to its column on the overview, so even a thread
/// that couldn't get clear never shows over another workspace.
fn paint_in_column(
    bounds: Bounds<Pixels>,
    column: Option<(f32, f32)>,
    window: &mut Window,
    curve: Curve,
    style: ThreadStyle,
) {
    let Some((left, right)) = column else {
        glyphs::thread(window, curve, style);
        return;
    };
    let mask = Bounds::from_corners(
        point(px(left), bounds.top()),
        point(px(right), bounds.bottom()),
    );
    window.with_content_mask(Some(gpui::ContentMask { bounds: mask }), |window| {
        glyphs::thread(window, curve, style);
    });
}

/// The overview's bundles, painted before the webs so a bundle passing a
/// web in between goes behind it instead of over its eyes. Hovering a web
/// brings its bundles forward and fades the rest.
fn bundle_threads(
    bounds: Bounds<Pixels>,
    frame: &Frame,
    lit: Option<&Light>,
    texts: &Texts,
    window: &mut Window,
) -> Vec<ThreadTop> {
    let lit_web = match frame.spot {
        Some(Spot::Web(c)) if lit.is_some() => Some(c),
        _ => None,
    };
    let spotlit = lit.is_some();
    let webs = web_circles(bounds, frame);
    let mut tops = Vec::new();
    for (n, bundle) in bundles(&frame.scene, |e| !frame.is_hidden(e))
        .into_iter()
        .enumerate()
    {
        let (a, b) = bundle.webs;
        let ((ca, ra), (cb, rb)) = (webs[a], webs[b]);
        let d = (cb.0 - ca.0).hypot(cb.1 - ca.1);
        if d <= ra + rb + 4.0 {
            continue;
        }
        // Leave and arrive off to one side: straight down the middle runs
        // into the name over the lower web. Every other bundle takes the
        // other side, so two from one web don't overlap.
        let tilt = if n % 2 == 0 {
            BUNDLE_TILT
        } else {
            -BUNDLE_TILT
        };
        let turn =
            |(x, y): (f32, f32), a: f32| (x * a.cos() - y * a.sin(), x * a.sin() + y * a.cos());
        let out = turn(((cb.0 - ca.0) / d, (cb.1 - ca.1) / d), tilt);
        let back = turn(((ca.0 - cb.0) / d, (ca.1 - cb.1) / d), -tilt);
        let from = (ca.0 + out.0 * ra, ca.1 + out.1 * ra);
        let to = (cb.0 + back.0 * rb, cb.1 + back.1 * rb);
        let col = frame.scene.clusters[a].column;
        let column = Some(column_span(bounds, frame, col));
        let between = between_webs(frame, &webs, col, [a, b]);
        let bend = route(from, to, (0.0, 0.0), column, &texts.headings, &between);
        let Some(curve) = Curve::new(from, to, 0.0, bend) else {
            continue;
        };
        let alpha = match lit_web {
            Some(c) if bundle.webs.0 == c || bundle.webs.1 == c => 1.0,
            _ if spotlit => FADED_THREAD,
            _ => BUNDLE_ALPHA,
        };
        let style = ThreadStyle {
            color: frame.theme.link.opacity(alpha),
            width: (1.5 + (bundle.links as f32).ln()).min(6.0),
            dash: None,
            arrow: false,
        };
        paint_in_column(bounds, column, window, curve, style);
        let text = match bundle.links {
            1 => "1 link".to_owned(),
            n => format!("{n} links"),
        };
        let w = glyph_width(window, &text, 11.0) + 16.0;
        let pill = (alpha > FADED_THREAD)
            .then(|| pill_spot(&curve, w, texts, &webs, column))
            .flatten()
            .map(|at| (text, at));
        tops.push(ThreadTop { curve, style, pill });
    }
    tops
}

/// One thread per link with both ends drawn. The overview shows only a lit
/// task's own, as bundles stand in for the rest. An opened project shows
/// all of them, fainter the more there are, and a lit task's bright on top.
fn threads(
    bounds: Bounds<Pixels>,
    frame: &Frame,
    light: Option<&Light>,
    texts: &Texts,
    window: &mut Window,
) -> Vec<ThreadTop> {
    let theme = &frame.theme;
    let zoom = frame.camera.zoom;
    let (eye_w, _) = eye_size(zoom);
    // Each end stops just short of its own eye's edge, whatever its shape.
    let gap = |eye: usize, toward: (f32, f32)| {
        glyphs::eye_edge(frame.scene.eyes[eye].shape, eye_w, toward) + 2.0
    };
    let at = |world: Vec2| {
        let p = screen(bounds, frame, world);
        (f32::from(p.x), f32::from(p.y))
    };
    let task_lit = matches!(frame.spot, Some(Spot::Eye(_)));
    let mut shown: Vec<(usize, bool)> = frame
        .scene
        .threads
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            let lit = light.is_some_and(|l| l.threads[i]);
            frame.eye_at(t.from)?;
            frame.eye_at(t.to)?;
            (frame.focus.is_some() || lit && task_lit).then_some((i, lit))
        })
        .collect();
    // The lit ones last, so they're painted over the faded ones.
    shown.sort_by_key(|&(i, lit)| (lit, i));
    let resting = (BUSY_THREADS / shown.len().max(1) as f32).clamp(0.3, 1.0);
    let webs = web_circles(bounds, frame);
    let mut tops = Vec::new();
    for (i, lit) in shown {
        let t = &frame.scene.threads[i];
        let (Some(a), Some(b)) = (frame.eye_at(t.from), frame.eye_at(t.to)) else {
            continue;
        };
        let (a, b) = (at(a), at(b));
        let ends = [t.from, t.to].map(|e| frame.scene.eyes[e].cluster);
        let col = frame.scene.clusters[ends[0]].column;
        let (column, between) = match frame.focus {
            None => (
                Some(column_span(bounds, frame, col)),
                between_webs(frame, &webs, col, ends),
            ),
            Some(_) => (None, Vec::new()),
        };
        let gaps = (
            gap(t.from, (b.0 - a.0, b.1 - a.1)),
            gap(t.to, (a.0 - b.0, a.1 - b.1)),
        );
        let bend = route(a, b, gaps, column, &texts.headings, &between);
        let Some(curve) = Curve::between(a, b, gaps, bend) else {
            continue;
        };
        let alpha = match light {
            Some(_) if lit => 1.0,
            Some(_) => FADED_THREAD,
            None => resting,
        };
        let mut style = thread_style(t.kind, theme);
        style.color = style.color.opacity(alpha);
        paint_in_column(bounds, column, window, curve, style);
        let crosses = frame.scene.eyes[t.from].cluster != frame.scene.eyes[t.to].cluster;
        let named = crosses && zoom >= LABELS_FROM && frame.focus.is_some() && alpha > 0.5;
        let pill = named
            .then(|| {
                let w = glyph_width(window, &t.name, 11.0) + 16.0;
                pill_spot(&curve, w, texts, &webs, column)
            })
            .flatten()
            .map(|at| (t.name.clone(), at));
        tops.push(ThreadTop { curve, style, pill });
    }
    tops
}

fn thread_tops(
    tops: &[ThreadTop],
    labels: &[Rect],
    theme: &Theme,
    window: &mut Window,
    cx: &mut App,
) {
    for top in tops {
        if top.style.arrow {
            // A head landing on a label would cover its text; it stops at
            // the label's edge instead, still pointing at the eye.
            let covered = |t: f32| labels.iter().any(|l| l.contains(top.curve.at(t)));
            let end = (0..=50)
                .map(|i| 1.0 - i as f32 / 50.0)
                .find(|&t| !covered(t))
                .unwrap_or(1.0);
            glyphs::arrowhead(window, top.curve.until(end), top.style.color);
        }
        let Some((name, (x, y))) = &top.pill else {
            continue;
        };
        let w = glyph_width(window, name, 11.0) + 16.0;
        let pill = Bounds::new(
            point(px(x - w / 2.0), px(y - PILL_H / 2.0)),
            size(px(w), px(PILL_H)),
        );
        window.paint_quad(quad(
            pill,
            px(10.0),
            theme.surface,
            px(1.0),
            theme.line,
            BorderStyle::Solid,
        ));
        glyphs::text(
            window,
            cx,
            name,
            point(px(*x), px(y - 7.0)),
            11.0,
            FontWeight::NORMAL,
            theme.muted,
            Align::Center,
            None,
        );
    }
}

/// An eye's drawn width and height at `zoom`: a fixed dot zoomed out.
fn eye_size(zoom: f32) -> (f32, f32) {
    let w = if zoom < SHAPES_FROM {
        6.0
    } else {
        EYE_W * zoom
    };
    (w, w * EYE_ASPECT)
}

/// Eyes drawn in this view and on or near the screen, with where they are
/// and whether each is a stub from another project.
fn visible_eyes<'a>(
    bounds: Bounds<Pixels>,
    frame: &'a Frame,
) -> impl Iterator<Item = (usize, Point<Pixels>, bool)> + 'a {
    let view = Bounds::new(
        bounds.origin - point(px(200.0), px(200.0)),
        bounds.size + size(px(400.0), px(400.0)),
    );
    frame.drawn_eyes().filter_map(move |(i, world)| {
        let at = screen(bounds, frame, world);
        let stub = frame
            .focus
            .is_some_and(|c| c != frame.scene.eyes[i].cluster);
        view.contains(&at).then_some((i, at, stub))
    })
}

/// The theme an eye is drawn in: faded while Find is open and it doesn't
/// match, or while something is lit that it isn't linked to.
fn eye_theme(frame: &Frame, lit: Option<&Light>, i: usize) -> Theme {
    match (&frame.matches, lit) {
        (Some(matches), _) if !matches.get(i).copied().unwrap_or(false) => frame.theme.faded(0.25),
        (None, Some(lit)) if !lit.eyes[i] => frame.theme.faded(0.3),
        _ => frame.theme,
    }
}

fn eyes(
    bounds: Bounds<Pixels>,
    frame: &Frame,
    lit: Option<&Light>,
    window: &mut Window,
    cx: &mut App,
) {
    let (w, _) = eye_size(frame.camera.zoom);
    for (i, at, stub) in visible_eyes(bounds, frame) {
        let eye = &frame.scene.eyes[i];
        let theme = &eye_theme(frame, lit, i);
        if frame.selected == Some(i) || frame.preview == Some(i) {
            let ring = (w * EYE_REACH + 4.0).max(10.0);
            glyphs::circle(
                window,
                at,
                ring,
                gpui::transparent_black(),
                Some((2.0, theme.accent, BorderStyle::Solid)),
            );
        }
        glyphs::eye(window, eye.shape, at, w, theme);
        if !eye.blocked_by.is_empty() && !stub && w >= 9.0 {
            // On the body's upper right, so it stays on the eye when zoomed in.
            let d = (w * EYE_BODY + 4.0) * std::f32::consts::FRAC_1_SQRT_2;
            let badge = point(at.x + px(d), at.y - px(d));
            glyphs::circle(window, badge, 7.0, theme.blocked, None);
            glyphs::text(
                window,
                cx,
                "!",
                point(badge.x, badge.y - px(7.0)),
                10.0,
                FontWeight::EXTRA_BOLD,
                theme.on_blocked,
                Align::Center,
                None,
            );
        }
    }
}

/// The labels that got a place. Each sits on a plate of
/// its web's own color: invisible, but threads and silk pass behind it, so
/// no line runs through the text. A stub's label hangs off its small web, so
/// it keeps a thin halo instead.
fn labels(
    bounds: Bounds<Pixels>,
    frame: &Frame,
    placed: Vec<Placed>,
    window: &mut Window,
    cx: &mut App,
) {
    for label in placed {
        let top = label.top.y;
        let middle = label.top.x + px(label.block.width / 2.0);
        let block = label.block;
        if let Some(eye) = label.leader {
            let card = Bounds::new(
                point(label.top.x - px(8.0), top - px(5.0)),
                size(px(block.width + 16.0), px(block.height + 10.0)),
            );
            // From the eye's edge to the nearest point of the card.
            let near = point(
                eye.x.clamp(card.left(), card.right()),
                eye.y.clamp(card.top(), card.bottom()),
            );
            let (dx, dy) = (f32::from(near.x - eye.x), f32::from(near.y - eye.y));
            let length = dx.hypot(dy).max(1.0);
            let start = eye_size(frame.camera.zoom).0 * EYE_REACH;
            let from = point(
                eye.x + px(dx / length * start),
                eye.y + px(dy / length * start),
            );
            let mut line = PathBuilder::stroke(px(1.0));
            line.move_to(from);
            line.line_to(near);
            if let Ok(path) = line.build() {
                window.paint_path(path, frame.theme.muted);
            }
            window.paint_quad(quad(
                card,
                px(8.0),
                frame.theme.surface,
                px(1.0),
                frame.theme.line,
                BorderStyle::Solid,
            ));
        } else if !label.stub {
            let cluster = frame.scene.eyes[label.eye].cluster;
            let alpha = if frame.dimmed(cluster) { 0.55 } else { 1.0 };
            let plate = frame.theme.bg.blend(frame.theme.web_fill.opacity(alpha));
            let area = Bounds::new(
                point(label.top.x - px(6.0), top - px(3.0)),
                size(px(block.width + 12.0), px(block.height + 4.0)),
            );
            // A label can reach past its web's edge, so the plate is the
            // background with the web's own disc cut into it, and matches
            // whatever is under each part.
            window.paint_quad(gpui::fill(area, frame.theme.bg));
            let (center, r) = frame.placement.clusters[cluster];
            let center = screen(bounds, frame, center);
            window.with_content_mask(Some(gpui::ContentMask { bounds: area }), |window| {
                glyphs::circle(window, center, r * frame.camera.zoom, plate, None);
            });
        }
        let halo = label.stub.then_some(frame.theme.bg);
        let mut y = top;
        for line in block.lines {
            glyphs::text(
                window,
                cx,
                &line.text,
                point(middle, y),
                line.size,
                line.weight,
                line.color,
                Align::Center,
                halo,
            );
            y += px(line.size * 1.35);
        }
    }
}

/// One line of the text under an eye.
struct LabelLine {
    text: String,
    size: f32,
    weight: FontWeight,
    color: Hsla,
}

/// What an eye's label says: reference, title, stage, then where a stub
/// comes from or what blocks the task.
fn label_lines(
    frame: &Frame,
    theme: &Theme,
    i: usize,
    stub: bool,
    compact: bool,
) -> Vec<LabelLine> {
    let eye = &frame.scene.eyes[i];
    let line = |text: String, size, weight, color| LabelLine {
        text,
        size,
        weight,
        color,
    };
    if compact {
        let text = format!(
            "{}  {}",
            eye.task.reference,
            glyphs::shorten(&eye.task.fields.title, 28)
        );
        return vec![line(text, 12.0, FontWeight::SEMIBOLD, theme.ink)];
    }
    let mut lines = vec![
        line(
            eye.task.reference.clone(),
            11.0,
            FontWeight::NORMAL,
            theme.muted,
        ),
        line(
            glyphs::shorten(&eye.task.fields.title, 28),
            13.0,
            FontWeight::SEMIBOLD,
            theme.ink,
        ),
        line(eye.stage.clone(), 11.0, FontWeight::NORMAL, theme.muted),
    ];
    if stub {
        let cluster = &frame.scene.clusters[eye.cluster];
        let from = format!("in {} · {}", cluster.project_key, cluster.name);
        lines.push(line(from, 11.0, FontWeight::NORMAL, theme.muted));
    } else if let Some(first) = eye.blocked_by.first() {
        let more = eye.blocked_by.len() - 1;
        let text = if more == 0 {
            format!("Blocked by {}", first.reference)
        } else {
            format!("Blocked by {} +{more}", first.reference)
        };
        lines.push(line(text, 11.0, FontWeight::SEMIBOLD, theme.blocked));
    }
    lines
}

/// An eye's label lines and the space they take.
struct LabelBlock {
    lines: Vec<LabelLine>,
    width: f32,
    height: f32,
}

fn label_block(
    window: &Window,
    frame: &Frame,
    theme: &Theme,
    i: usize,
    stub: bool,
    compact: bool,
) -> LabelBlock {
    let lines = label_lines(frame, theme, i, stub, compact);
    let width = lines
        .iter()
        .map(|l| glyph_width(window, &l.text, l.size))
        .fold(0.0, f32::max);
    let height = lines.iter().map(|l| l.size * 1.35).sum();
    LabelBlock {
        lines,
        width,
        height,
    }
}

/// The five eyes for the legend, at a fixed small size.
pub fn legend_eye(bounds: Bounds<Pixels>, shape: Shape, theme: &Theme, window: &mut Window) {
    glyphs::eye(window, shape, bounds.center(), 22.0, theme);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Layout;
    use crate::scene::tests::Fixture;

    #[test]
    fn while_focused_a_linked_task_from_another_project_is_a_stub_hit() {
        let mut ws = Fixture::new();
        let web = ws.project("WEB", &[("Landing page", 2)]);
        let api = ws.project("API", &[("Signup endpoint", 3)]);
        ws.link(0, api[0], web[0]);
        let scene = Scene::build(&[ws.source()]);
        let placement = Layout::default().place(&scene);
        let stubs = stub_positions(&scene, &placement, 0);
        let frame = Frame {
            scene: Rc::new(scene),
            placement: Rc::new(placement),
            camera: Camera {
                origin: Vec2::default(),
                zoom: 1.0,
            },
            theme: Theme::light(),
            focus: Some(0),
            selected: None,
            stubs: Rc::new(stubs.clone()),
            web_cursor: None,
            preview: None,
            matches: None,
            hidden: None,
            ring: None,
            spot: None,
        };
        let [(eye, at)] = stubs[..] else {
            panic!("one stub, got {stubs:?}")
        };
        assert_eq!(frame.scene.eyes[eye].task.reference, "API-1");
        // Its eye, and the empty part of its small web, both count.
        assert!(matches!(hit(&frame, at), Hit::Stub(e) if e == eye));
        assert!(matches!(hit(&frame, at + Vec2::new(30.0, 0.0)), Hit::Stub(e) if e == eye));
    }

    #[test]
    fn a_hidden_stage_drops_its_tasks_from_hits_and_arrows() {
        let mut ws = Fixture::new();
        // Stage 4 is Done, stage 2 In progress.
        ws.project("WEB", &[("Shipped", 4), ("Building", 2)]);
        ws.project("API", &[("Also shipped", 4)]);
        // One project spells it differently; the filter treats it as the same stage.
        ws.snapshots[1].stages[4].name = "done".into();
        let scene = Scene::build(&[ws.source()]);
        let placement = Layout::default().place(&scene);
        let hidden = scene.hidden_by(&["done".to_owned()].into());
        assert_eq!(hidden, [true, false, true]);
        let frame = Frame {
            scene: Rc::new(scene),
            placement: Rc::new(placement),
            camera: Camera {
                origin: Vec2::default(),
                zoom: 1.0,
            },
            theme: Theme::light(),
            focus: Some(0),
            selected: None,
            stubs: Rc::default(),
            web_cursor: None,
            preview: None,
            matches: None,
            hidden: Some(Rc::new(hidden)),
            ring: None,
            spot: None,
        };

        let shipped = frame.placement.eyes[0];
        assert!(!matches!(hit(&frame, shipped), Hit::Eye(_)));
        let reachable: Vec<usize> = frame.drawn_eyes().map(|(e, _)| e).collect();
        assert_eq!(reachable, [1], "only WEB-2 is left to land on");
    }

    #[test]
    fn a_thread_bends_around_text_in_its_way_and_only_then() {
        // The screenshot's case: API's eye above, WEB's below, and the
        // "WEB Website" line between them, right where the thread falls.
        let (from, to, gap) = ((425.0, 550.0), (450.0, 1265.0), 12.0);
        let title = Rect::around(point(px(360.0), px(917.0)), 135.0, 17.0);
        let crosses = |bend: f32, b: &Rect| {
            let curve = Curve::new(from, to, gap, bend).unwrap();
            (0..=400).any(|i| b.contains(curve.at(i as f32 / 400.0)))
        };
        assert!(
            crosses(0.0, &title),
            "the test needs a thread through the title"
        );

        let bend = route(from, to, (gap, gap), None, &[title], &[]);
        assert!(!crosses(bend, &title), "bent by {bend} and still crosses");
        // Just far enough: one step less would still touch the text.
        let less = bend - BEND_STEP * bend.signum();
        assert!(crosses(less, &title));

        // Text beside the thread leaves it alone.
        let aside = Rect::around(point(px(600.0), px(917.0)), 135.0, 17.0);
        assert_eq!(route(from, to, (gap, gap), None, &[aside], &[]), 0.0);
        assert_eq!(route(from, to, (gap, gap), None, &[], &[]), 0.0);
    }

    #[test]
    fn a_thread_dodging_text_stays_inside_its_column() {
        let (from, to, gap) = ((425.0, 550.0), (450.0, 1265.0), 12.0);
        let title = Rect::around(point(px(360.0), px(917.0)), 135.0, 17.0);
        let samples = |bend: f32| {
            let curve = Curve::new(from, to, gap, bend).unwrap();
            (0..=400).map(move |i| curve.at(i as f32 / 400.0))
        };
        // Wherever the thread goes round the title on its own, put the
        // column's edge there, so only the other way stays inside.
        let free = route(from, to, (gap, gap), None, &[title], &[]);
        let went_left = samples(free).any(|p| p.0 < title.min.0);
        let column = if went_left {
            (title.min.0 - 2.0, 2000.0)
        } else {
            (0.0, title.max.0 + 2.0)
        };
        let bend = route(from, to, (gap, gap), Some(column), &[title], &[]);
        assert!(
            samples(bend).all(|p| !title.contains(p)),
            "crosses at {bend}"
        );
        assert!(
            samples(bend).all(|p| (column.0..=column.1).contains(&p.0)),
            "leaves its column at {bend}"
        );
    }

    #[test]
    fn a_lit_task_brings_out_its_links_and_their_other_ends_only() {
        let mut ws = Fixture::new();
        let web = ws.project("WEB", &[("Landing page", 2), ("Pricing", 1), ("Footer", 4)]);
        let api = ws.project("API", &[("Signup endpoint", 3)]);
        ws.link(0, api[0], web[0]);
        ws.link(0, web[0], web[1]);
        let scene = Scene::build(&[ws.source()]);
        let eye = |r: &str| {
            scene
                .eyes
                .iter()
                .position(|e| e.task.reference == r)
                .unwrap()
        };
        let lit_eyes = |l: &Light| -> Vec<String> {
            (0..scene.eyes.len())
                .filter(|&e| l.eyes[e])
                .map(|e| scene.eyes[e].task.reference.clone())
                .collect()
        };

        let landing = light(&scene, Spot::Eye(eye("WEB-1"))).unwrap();
        let mut lit = lit_eyes(&landing);
        lit.sort();
        assert_eq!(lit, ["API-1", "WEB-1", "WEB-2"]);
        assert!(landing.threads.iter().all(|&t| t));

        // A web lights its own tasks and whatever links out of it.
        let api_web = scene.eyes[eye("API-1")].cluster;
        let api_lit = light(&scene, Spot::Web(api_web)).unwrap();
        let mut lit = lit_eyes(&api_lit);
        lit.sort();
        assert_eq!(lit, ["API-1", "WEB-1"]);
        assert_eq!(api_lit.threads.iter().filter(|&&t| t).count(), 1);

        // Nothing linked, nothing to pick out: no fading at all.
        assert!(light(&scene, Spot::Eye(eye("WEB-3"))).is_none());
    }

    #[test]
    fn the_overview_bundles_links_between_two_projects_either_way() {
        let mut ws = Fixture::new();
        let web = ws.project("WEB", &[("Landing page", 2), ("Pricing", 1)]);
        let api = ws.project("API", &[("Signup endpoint", 3)]);
        ws.project("DOC", &[("Guide", 1)]);
        ws.link(0, api[0], web[0]);
        ws.link(0, web[1], api[0]);
        ws.link(0, web[0], web[1]);
        let scene = Scene::build(&[ws.source()]);
        let cluster = |key: &str| {
            scene
                .clusters
                .iter()
                .position(|c| c.project_key == key)
                .unwrap()
        };
        let (w, a) = (cluster("WEB"), cluster("API"));
        assert_eq!(
            bundles(&scene, |_| true),
            [Bundle {
                webs: (w.min(a), w.max(a)),
                links: 2
            }]
        );
    }

    #[test]
    fn a_stub_never_sits_over_the_web_name_or_count() {
        // API is laid out right above WEB, so its stubs would land straight
        // above WEB's web, on top of "WEB Website". With three of them,
        // making room pushes some along the circle, and not into the cone.
        let mut ws = Fixture::new();
        let web = ws.project("WEB", &[("Landing page", 2)]);
        let api = ws.project("API", &[("Signup", 3), ("Tokens", 2), ("Docs", 1)]);
        for &task in &api {
            ws.link(0, task, web[0]);
        }
        let scene = Scene::build(&[ws.source()]);
        let placement = Layout::default().place(&scene);
        let web_cluster = scene
            .clusters
            .iter()
            .position(|c| c.project_key == "WEB")
            .unwrap();
        let (center, _) = placement.clusters[web_cluster];
        let stubs = stub_positions(&scene, &placement, web_cluster);
        assert_eq!(stubs.len(), 3);
        for &(_, at) in &stubs {
            let toward = at - center;
            let off_vertical = toward.x.atan2(toward.y.abs());
            assert!(
                off_vertical.abs() >= STUB_CLEAR - 1e-4,
                "stub at {toward:?} is within {off_vertical} rad of straight up or down"
            );
        }
        // And they still don't stack on each other.
        for (i, &(_, a)) in stubs.iter().enumerate() {
            for &(_, b) in &stubs[i + 1..] {
                assert!((a - b).len() > STUB_WEB, "stubs at {a:?} and {b:?} overlap");
            }
        }
    }

    #[test]
    fn many_stubs_around_one_web_never_touch() {
        // More links out than fit around the web at the usual spacing.
        let mut ws = Fixture::new();
        let web = ws.project("WEB", &[("Landing page", 2)]);
        let tasks: Vec<(&str, usize)> = (0..20).map(|_| ("Endpoint", 3)).collect();
        let api = ws.project("API", &tasks);
        for &task in &api {
            ws.link(0, task, web[0]);
        }
        let scene = Scene::build(&[ws.source()]);
        let placement = Layout::default().place(&scene);
        let web_cluster = scene
            .clusters
            .iter()
            .position(|c| c.project_key == "WEB")
            .unwrap();
        let stubs = stub_positions(&scene, &placement, web_cluster);
        assert_eq!(stubs.len(), 20);
        for (i, &(_, a)) in stubs.iter().enumerate() {
            for &(_, b) in &stubs[i + 1..] {
                assert!(
                    (a - b).len() >= 2.0 * STUB_WEB,
                    "stubs at {a:?} and {b:?} touch"
                );
            }
        }
    }

    #[test]
    fn arrows_land_on_the_neighbour_in_their_direction() {
        // . B . . .
        // A x C D .     x is where we are.
        // . . . . .
        // . . . . E
        let at = |x: f32, y: f32| Vec2::new(x * 100.0, y * 100.0);
        let others = [
            (0, at(0.0, 1.0)),
            (1, at(1.0, 0.0)),
            (2, at(2.0, 1.0)),
            (3, at(3.0, 1.0)),
            (4, at(4.0, 3.0)),
        ];
        let go = |side| nearest(at(1.0, 1.0), side, others.into_iter());
        assert_eq!(go(Side::Left), Some(0));
        assert_eq!(go(Side::Up), Some(1));
        assert_eq!(go(Side::Right), Some(2), "C, not the farther D");
        // Nothing lies within 45° below, so the one that's at least below wins.
        assert_eq!(go(Side::Down), Some(4));
        // From E there's nothing further right at all.
        assert_eq!(nearest(at(4.0, 3.0), Side::Right, others.into_iter()), None);
    }

    #[test]
    fn zooming_keeps_the_point_under_the_pointer_still() {
        let mut camera = Camera {
            origin: Vec2::new(-120.0, 40.0),
            zoom: 0.6,
        };
        let pointer = Vec2::new(300.0, 200.0);
        let before = camera.to_world(pointer);
        camera.zoom_at(pointer, 1.8, Camera::MIN_ZOOM);
        let after = camera.to_world(pointer);
        assert!(
            (after - before).len() < 1e-3,
            "{before:?} drifted to {after:?}"
        );
        assert!((camera.zoom - 1.08).abs() < 1e-5);
    }
}
