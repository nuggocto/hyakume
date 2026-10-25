//! The infinite canvas: a camera over world coordinates, and the painting
//! of columns, webs, threads, and eyes with detail that depends on zoom.

use std::rc::Rc;

use gpui::{
    App, BorderStyle, Bounds, FontWeight, Hsla, PathBuilder, Pixels, Point, Window, point, px,
    quad, size,
};

use crate::glyphs::{self, Align, EYE_ASPECT, ThreadStyle};
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
/// Screen space a web's count needs between it and the next web's name.
const COUNT_ROOM: f32 = 64.0;
/// Radius of the small torn web a stub sits on.
const STUB_WEB: f32 = 50.0;

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

    /// Zooms by `factor`, keeping the world point under `anchor` where it is.
    pub fn zoom_at(&mut self, anchor: Vec2, factor: f32) {
        let held = self.to_world(anchor);
        self.zoom = (self.zoom * factor).clamp(Self::MIN_ZOOM, Self::MAX_ZOOM);
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
}

impl Frame {
    /// Where an eye is drawn in this view, if it is drawn at all.
    pub fn eye_at(&self, eye: usize) -> Option<Vec2> {
        match self.focus {
            Some(c) if self.scene.eyes[eye].cluster != c => self
                .stubs
                .iter()
                .find(|(e, _)| *e == eye)
                .map(|(_, at)| *at),
            _ => Some(self.placement.eyes[eye]),
        }
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
    let mut angles: Vec<(usize, f32)> = scene
        .outside_ends(cluster)
        .into_iter()
        .map(|eye| {
            let toward = placement.eyes[eye] - center;
            (eye, toward.y.atan2(toward.x))
        })
        .collect();
    angles.sort_by(|a, b| a.1.total_cmp(&b.1));
    let min_gap = 0.5_f32;
    for i in 1..angles.len() {
        if angles[i].1 - angles[i - 1].1 < min_gap {
            angles[i].1 = angles[i - 1].1 + min_gap;
        }
    }
    angles
        .into_iter()
        .map(|(eye, a)| {
            (
                eye,
                center + Vec2::new(a.cos(), a.sin()) * (r + STUB_DISTANCE),
            )
        })
        .collect()
}

pub enum Hit {
    Eye(usize),
    /// While focused: a task from another project, or the small web it sits on.
    Stub(usize),
    Cluster(usize),
    Nothing,
}

/// What's under a point given in canvas-local screen pixels.
pub fn hit(frame: &Frame, at: Vec2) -> Hit {
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
    window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
        dots(bounds, frame, window);
        if frame.focus.is_none() {
            columns(bounds, frame, window, cx);
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
        threads(bounds, frame, window, cx);
        eyes(bounds, frame, window, cx);
    });
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

fn columns(bounds: Bounds<Pixels>, frame: &Frame, window: &mut Window, cx: &mut App) {
    let theme = &frame.theme;
    let bottom = frame.placement.bounds().1.y;
    for (i, column) in frame.scene.columns.iter().enumerate() {
        let (x, width) = frame.placement.columns[i];
        let head = column_head(bounds, frame, i);
        // Header text keeps its size at any zoom, so it's shortened to the
        // column's on-screen width rather than spilling into the next column.
        let room = (width - 40.0) * frame.camera.zoom - 12.0;
        let chip_w = glyph_width(window, &column.chip, 12.0) + 16.0;
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
        if let Some(title) = fit_text(window, &column.title, 15.0, room - chip_w - 8.0) {
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
        let (dot, status) = status_line(column.status.clone(), &column.address, theme);
        let status = fit_text(window, &status, 12.0, room - 14.0).unwrap_or_default();
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
            // Nothing loaded from here (yet): a faded torn web holds its place.
            let at = screen(
                bounds,
                frame,
                Vec2::new(x + width / 2.0, COLUMN_HEAD + 140.0),
            );
            let live = matches!(column.status, Status::Live);
            glyphs::web(
                window,
                at,
                120.0 * frame.camera.zoom,
                !live,
                if live { 1.0 } else { 0.6 },
                theme,
            );
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

/// Where a column's header starts. Normally fixed in the world; zoomed out it
/// moves up to stay clear of the first web's name, but never off the canvas.
fn column_head(bounds: Bounds<Pixels>, frame: &Frame, column: usize) -> Point<Pixels> {
    let (x, _) = frame.placement.columns[column];
    let mut head = screen(bounds, frame, Vec2::new(x + 40.0, 30.0));
    if let Some(first) = frame.scene.columns[column].clusters.clone().next() {
        let (center, r) = frame.placement.clusters[first];
        let title = screen(bounds, frame, center).y - px(r * frame.camera.zoom + 30.0);
        head.y = head.y.min(title - px(HEADER_TEXT_H));
    }
    head.y = head.y.max(bounds.origin.y + px(8.0));
    head
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

    let cluster = &frame.scene.clusters[c];
    let ink = theme.ink.opacity(alpha);
    let muted = theme.muted.opacity(alpha);
    let title_y = at.y - px(rs + 30.0);
    // Zoomed out, the name may not fit its column, or may run into the header.
    let room = frame.placement.columns[cluster.column].1 * frame.camera.zoom - 16.0;
    let first = frame.scene.columns[cluster.column].clusters.start == c;
    let under_header = first
        && frame.focus.is_none()
        && title_y < column_head(bounds, frame, cluster.column).y + px(HEADER_TEXT_H);
    let key_w = glyph_width(window, &cluster.project_key, 13.0);
    let full_w = key_w + 6.0 + glyph_width(window, &cluster.name, 13.0);
    let name = if full_w <= room {
        cluster.name.as_str()
    } else {
        ""
    };
    let shown_w = if name.is_empty() { key_w } else { full_w };
    let left = at.x - px(shown_w / 2.0);
    if key_w <= room && !under_header {
        glyphs::text(
            window,
            cx,
            &cluster.project_key,
            point(left, title_y),
            13.0,
            FontWeight::BOLD,
            ink,
            Align::Left,
            None,
        );
        glyphs::text(
            window,
            cx,
            name,
            point(left + px(key_w + 6.0), title_y),
            13.0,
            FontWeight::NORMAL,
            muted,
            Align::Left,
            None,
        );
    }

    // Skip the count when the next web's name would sit on it (zoomed out).
    let column = &frame.scene.columns[cluster.column];
    let crowded = column.clusters.clone().any(|other| {
        let (oc, or) = frame.placement.clusters[other];
        let gap = ((oc.y - or) - (center.y + r)) * frame.camera.zoom;
        oc.y > center.y && gap < COUNT_ROOM
    });
    if crowded {
        return;
    }
    let n = cluster.eyes.len();
    let mut count = format!("{n} task{}", if n == 1 { "" } else { "s" });
    if cluster.blocked > 0 {
        count.push_str(&format!(" · {} blocked", cluster.blocked));
    }
    glyphs::text(
        window,
        cx,
        &count,
        point(at.x, at.y + px(rs + 10.0)),
        12.0,
        FontWeight::NORMAL,
        muted,
        Align::Center,
        None,
    );
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

fn threads(bounds: Bounds<Pixels>, frame: &Frame, window: &mut Window, cx: &mut App) {
    let theme = &frame.theme;
    let zoom = frame.camera.zoom;
    let gap = (EYE_W * 0.8 * zoom).max(4.0);
    for t in &frame.scene.threads {
        let (Some(a), Some(b)) = (frame.eye_at(t.from), frame.eye_at(t.to)) else {
            continue;
        };
        let mid = glyphs::thread(
            window,
            screen(bounds, frame, a),
            screen(bounds, frame, b),
            gap,
            thread_style(t.kind, theme),
        );
        let crosses = frame.scene.eyes[t.from].cluster != frame.scene.eyes[t.to].cluster;
        if let (Some(mid), true, true) =
            (mid, crosses, zoom >= LABELS_FROM && frame.focus.is_some())
        {
            let w = glyph_width(window, &t.name, 11.0) + 16.0;
            let pill = Bounds::new(
                point(mid.x - px(w / 2.0), mid.y - px(10.0)),
                size(px(w), px(20.0)),
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
                &t.name,
                point(mid.x, mid.y - px(7.0)),
                11.0,
                FontWeight::NORMAL,
                theme.muted,
                Align::Center,
                None,
            );
        }
    }
}

fn eyes(bounds: Bounds<Pixels>, frame: &Frame, window: &mut Window, cx: &mut App) {
    let theme = &frame.theme;
    let zoom = frame.camera.zoom;
    let w = if zoom < SHAPES_FROM {
        6.0
    } else {
        EYE_W * zoom
    };
    let h = w * EYE_ASPECT;
    let labels = zoom >= LABELS_FROM;
    let view = Bounds::new(
        bounds.origin - point(px(200.0), px(200.0)),
        bounds.size + size(px(400.0), px(400.0)),
    );
    for (i, eye) in frame.scene.eyes.iter().enumerate() {
        let Some(world) = frame.eye_at(i) else {
            continue;
        };
        let at = screen(bounds, frame, world);
        if !view.contains(&at) {
            continue;
        }
        let stub = frame.focus.is_some_and(|c| c != eye.cluster);
        if frame.selected == Some(i) {
            let ring = (w * 0.75).max(10.0);
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
            let badge = point(at.x + px(w / 2.0 + 1.0), at.y - px(h / 2.0 + 3.0));
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
        if labels {
            label(
                window,
                cx,
                frame,
                i,
                stub,
                point(at.x, at.y + px(h / 2.0 + 6.0)),
            );
        }
    }
}

fn label(
    window: &mut Window,
    cx: &mut App,
    frame: &Frame,
    i: usize,
    stub: bool,
    at: Point<Pixels>,
) {
    let theme = &frame.theme;
    let eye = &frame.scene.eyes[i];
    let halo = Some(theme.bg);
    let mut y = at.y;
    let mut line = |window: &mut Window, cx: &mut App, text: &str, size: f32, weight, color| {
        glyphs::text(
            window,
            cx,
            text,
            point(at.x, y),
            size,
            weight,
            color,
            Align::Center,
            halo,
        );
        y += px(size * 1.35);
    };
    line(
        window,
        cx,
        &eye.task.reference,
        11.0,
        FontWeight::NORMAL,
        theme.muted,
    );
    line(
        window,
        cx,
        &glyphs::shorten(&eye.task.fields.title, 28),
        13.0,
        FontWeight::SEMIBOLD,
        theme.ink,
    );
    line(
        window,
        cx,
        &eye.stage,
        11.0,
        FontWeight::NORMAL,
        theme.muted,
    );
    if stub {
        let cluster = &frame.scene.clusters[eye.cluster];
        let from = format!("in {} · {}", cluster.project_key, cluster.name);
        line(window, cx, &from, 11.0, FontWeight::NORMAL, theme.muted);
    } else if let Some(first) = eye.blocked_by.first() {
        let more = eye.blocked_by.len() - 1;
        let text = if more == 0 {
            format!("Blocked by {}", first.reference)
        } else {
            format!("Blocked by {} +{more}", first.reference)
        };
        line(window, cx, &text, 11.0, FontWeight::SEMIBOLD, theme.blocked);
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
    fn zooming_keeps_the_point_under_the_pointer_still() {
        let mut camera = Camera {
            origin: Vec2::new(-120.0, 40.0),
            zoom: 0.6,
        };
        let pointer = Vec2::new(300.0, 200.0);
        let before = camera.to_world(pointer);
        camera.zoom_at(pointer, 1.8);
        let after = camera.to_world(pointer);
        assert!(
            (after - before).len() < 1e-3,
            "{before:?} drifted to {after:?}"
        );
        assert!((camera.zoom - 1.08).abs() < 1e-5);
    }
}
