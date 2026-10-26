//! The drawn shapes: task eyes, spider webs, and silk threads, ported from
//! the approved mockups. Everything here takes screen coordinates.

use gpui::{
    App, BorderStyle, Bounds, FontWeight, Hsla, PathBuilder, Pixels, Point, SharedString, TextRun,
    Window, point, px, quad, size,
};

use hyakume_core::model::is_unsafe_char;

use crate::scene::Shape;
use crate::theme::{EyeColors, Theme};

/// Eyes are drawn in a 28×20 box, like the mockup's SVG viewBox.
const EYE_BOX_W: f32 = 28.0;
pub const EYE_ASPECT: f32 = 20.0 / 28.0;
/// How far an eye's drawing reaches from its center, as a share of its
/// box's width: the body is 7.4 units across the 28-unit box, a review's
/// side beads 11.6. Badges, rings, and thread ends hug these, not the box.
pub const EYE_BODY: f32 = 7.4 / EYE_BOX_W;
pub const EYE_REACH: f32 = 11.6 / EYE_BOX_W;

/// A review's side beads, the spider's other eyes: center and radius in the
/// eye box's units.
const REVIEW_BEADS: [(f32, f32, f32); 4] = [
    (5.0, 6.2, 2.6),
    (23.0, 6.2, 2.6),
    (3.0, 12.0, 1.7),
    (25.0, 12.0, 1.7),
];

/// How far an eye drawn `w` wide reaches from its center toward `dir`, in
/// pixels, so a thread can end right at its outline. It walks in from past
/// the widest reach to the first point on the drawing: a done crescent sits
/// below the middle, a review's beads only stick out to the sides.
pub fn eye_edge(shape: Shape, w: f32, dir: (f32, f32)) -> f32 {
    if w < 9.0 {
        return (w * 0.3).max(1.5);
    }
    let len = dir.0.hypot(dir.1);
    if len < 1e-3 {
        return 0.0;
    }
    let (ux, uy) = (dir.0 / len, dir.1 / len);
    let units = (0..=52)
        .map(|k| 13.0 - k as f32 * 0.25)
        .find(|&t| on_eye(shape, (14.0 + ux * t, 10.5 + uy * t)))
        .unwrap_or(0.0);
    units * w / EYE_BOX_W
}

/// Whether a point, in the eye box's units, is on the drawing `eye` makes.
fn on_eye(shape: Shape, (x, y): (f32, f32)) -> bool {
    let disc = |cx: f32, cy: f32, r: f32| (x - cx).hypot(y - cy) <= r;
    match shape {
        Shape::Backlog => disc(14.0, 10.5, 6.8),
        Shape::Todo => disc(14.0, 10.5, 7.0),
        Shape::Doing => disc(14.0, 10.5, 7.4),
        Shape::Review => {
            disc(14.0, 11.0, 6.1) || REVIEW_BEADS.iter().any(|&(cx, cy, r)| disc(cx, cy, r))
        }
        Shape::Done => {
            // Both edges run from (5.6, 8.4) to (22.4, 8.4) with their control
            // points in the middle, so x moves evenly along them.
            let t = (x - 5.6) / 16.8;
            let edge = |mid: f32| (1.0 - t).powi(2) * 8.4 + 2.0 * t * (1.0 - t) * mid + t * t * 8.4;
            (0.0..=1.0).contains(&t) && (edge(14.2)..=edge(19.0)).contains(&y)
        }
    }
}

pub fn circle(
    window: &mut Window,
    center: Point<Pixels>,
    r: f32,
    fill: Hsla,
    border: Option<(f32, Hsla, BorderStyle)>,
) {
    let (width, color, style) =
        border.unwrap_or((0.0, gpui::transparent_black(), BorderStyle::Solid));
    let bounds = Bounds::new(
        point(center.x - px(r), center.y - px(r)),
        size(px(2.0 * r), px(2.0 * r)),
    );
    window.paint_quad(quad(bounds, px(r), fill, px(width), color, style));
}

fn paint(window: &mut Window, builder: PathBuilder, color: Hsla) {
    // A degenerate path (zero size at extreme zoom) just isn't drawn.
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// One eye, `w` pixels wide, centered on `center`.
pub fn eye(window: &mut Window, shape: Shape, center: Point<Pixels>, w: f32, theme: &Theme) {
    let c = theme.eye;
    let s = w / EYE_BOX_W;
    // Eye-box coordinates (the mockup's viewBox) to screen.
    let at = |x: f32, y: f32| point(center.x + px((x - 14.0) * s), center.y + px((y - 10.5) * s));

    if w < 9.0 {
        // Too small for a shape to read; a dot in the stage's tone keeps the web legible.
        let fill = match shape {
            Shape::Doing | Shape::Review => c.gold,
            Shape::Todo => c.bead,
            Shape::Backlog | Shape::Done => theme.muted,
        };
        circle(window, center, (w * 0.3).max(1.5), fill, None);
        return;
    }

    match shape {
        Shape::Backlog => {
            let border = Some((1.2 * s, theme.muted, BorderStyle::Dashed));
            circle(
                window,
                at(14.0, 10.5),
                6.6 * s,
                gpui::transparent_black(),
                border,
            );
            let mut wrap = PathBuilder::stroke(px(0.8 * s));
            wrap.move_to(at(8.4, 8.6));
            wrap.curve_to(at(19.6, 7.4), at(14.0, 11.4));
            wrap.move_to(at(8.1, 12.6));
            wrap.curve_to(at(19.9, 11.6), at(14.0, 15.4));
            paint(window, wrap, theme.muted);
        }
        Shape::Todo => {
            bead(window, &c, at(14.0, 10.5), 7.0 * s, s);
            circle(window, at(16.8, 13.4), 0.8 * s, c.glint.opacity(0.8), None);
        }
        Shape::Doing => lantern(window, &c, at(14.0, 10.5), 7.4 * s, s, true),
        Shape::Review => {
            lantern(window, &c, at(14.0, 11.0), 6.1 * s, s, false);
            // The spider's side eyes: more eyes looking at the work.
            for (x, y, r) in REVIEW_BEADS {
                bead(window, &c, at(x, y), r * s, s * 0.6);
            }
        }
        Shape::Done => {
            let mut lid = PathBuilder::fill();
            lid.move_to(at(5.6, 8.4));
            lid.curve_to(at(22.4, 8.4), at(14.0, 19.0));
            lid.curve_to(at(5.6, 8.4), at(14.0, 14.2));
            lid.close();
            paint(window, lid, theme.ink);
        }
    }
}

/// A glossy black spider bead with one glint.
fn bead(window: &mut Window, c: &EyeColors, center: Point<Pixels>, r: f32, s: f32) {
    let rim = c.rim.map(|rim| (0.9 * s, rim, BorderStyle::Solid));
    circle(window, center, r, c.bead, rim);
    let glint = point(center.x - px(r * 0.38), center.y - px(r * 0.4));
    circle(window, glint, r * 0.27, c.glint, None);
}

/// The gold slit eye of something awake and working.
fn lantern(
    window: &mut Window,
    c: &EyeColors,
    center: Point<Pixels>,
    r: f32,
    s: f32,
    fibres: bool,
) {
    circle(
        window,
        center,
        r,
        c.gold,
        Some((1.1 * s, c.gold_rim, BorderStyle::Solid)),
    );
    if fibres && s >= 1.2 {
        let mut lines = PathBuilder::stroke(px(0.45 * s));
        for i in 0..16 {
            let a = i as f32 / 16.0 * std::f32::consts::TAU;
            let p = |k: f32| {
                point(
                    center.x + px(a.cos() * r * k),
                    center.y + px(a.sin() * r * k),
                )
            };
            lines.move_to(p(0.49));
            lines.line_to(p(0.82));
        }
        paint(window, lines, c.gold_rim.opacity(0.55));
    }
    let tall = r * 0.8;
    let wide = r * 0.48;
    let mut slit = PathBuilder::fill();
    slit.move_to(point(center.x, center.y - px(tall)));
    slit.curve_to(
        point(center.x, center.y + px(tall)),
        point(center.x + px(wide), center.y),
    );
    slit.curve_to(
        point(center.x, center.y - px(tall)),
        point(center.x - px(wide), center.y),
    );
    slit.close();
    paint(window, slit, c.pupil);
    let glint = point(center.x - px(r * 0.4), center.y - px(r * 0.39));
    circle(window, glint, r * 0.2, c.glint, None);
}

/// A web: twelve spokes and five rings that sag between them. A torn web
/// (offline, or another project's stub) skips some segments and is dashed.
pub fn web(
    window: &mut Window,
    center: Point<Pixels>,
    r: f32,
    torn: bool,
    alpha: f32,
    theme: &Theme,
) {
    // Opaque, in the color the see-through fill gives over the background,
    // so a thread passing behind a web is hidden rather than showing through.
    let fill = theme.bg.blend(theme.web_fill.opacity(alpha));
    circle(window, center, r, fill, None);
    const SPOKES: usize = 12;
    let angles: Vec<f32> = (0..SPOKES)
        .map(|i| i as f32 / SPOKES as f32 * std::f32::consts::TAU + 0.13)
        .collect();
    let at = |a: f32, k: f32| {
        point(
            center.x + px(a.cos() * r * k),
            center.y + px(a.sin() * r * k),
        )
    };
    let mut lines = PathBuilder::stroke(px(1.0));
    if torn {
        lines = lines.dash_array(&[px(4.0), px(3.0)]);
    }
    for (i, &a) in angles.iter().enumerate() {
        if torn && i % 4 == 1 {
            continue;
        }
        lines.move_to(center);
        lines.line_to(at(a, 0.98));
    }
    for (ki, k) in [0.22_f32, 0.42, 0.62, 0.8, 0.96].into_iter().enumerate() {
        lines.move_to(at(angles[0], k));
        for i in 0..SPOKES {
            let a = angles[i];
            let b = if i + 1 == SPOKES {
                angles[0] + std::f32::consts::TAU
            } else {
                angles[i + 1]
            };
            if torn && (i * 3 + ki * 5) % 7 < 2 {
                lines.move_to(at(b, k));
                continue;
            }
            lines.curve_to(at(b, k), at((a + b) / 2.0, k * 0.9));
        }
    }
    paint(window, lines, theme.web.opacity(alpha));
}

/// How one kind of link is drawn, shared by the canvas and the legend.
#[derive(Clone, Copy)]
pub struct ThreadStyle {
    pub color: Hsla,
    pub width: f32,
    /// Dash and gap length, for links that aren't solid.
    pub dash: Option<(f32, f32)>,
    pub arrow: bool,
}

/// A thread's shape: one quadratic curve in screen pixels. Painting and
/// routing both go through this, so a route that clears a label is exactly
/// the curve that gets drawn.
#[derive(Clone, Copy, Debug)]
pub struct Curve {
    pub start: (f32, f32),
    pub control: (f32, f32),
    pub end: (f32, f32),
}

impl Curve {
    /// The thread from `from` to `to`, its ends kept `gap` off the eyes. It
    /// sags under its own weight, and `bend` pulls its middle sideways, which
    /// is how a thread steers around a label. `None` when the eyes nearly touch.
    pub fn new(from: (f32, f32), to: (f32, f32), gap: f32, bend: f32) -> Option<Self> {
        Self::between(from, to, (gap, gap), bend)
    }

    /// Like `new`, with each end kept its own distance off its eye.
    pub fn between(
        from: (f32, f32),
        to: (f32, f32),
        (gap_from, gap_to): (f32, f32),
        bend: f32,
    ) -> Option<Self> {
        let (fx, fy) = from;
        let (tx, ty) = to;
        let d = (tx - fx).hypot(ty - fy);
        if d < gap_from + gap_to + 4.0 {
            return None;
        }
        let (ux, uy) = ((tx - fx) / d, (ty - fy) / d);
        let start = (fx + ux * gap_from, fy + uy * gap_from);
        let end = (tx - ux * gap_to, ty - uy * gap_to);
        let sag = (d * 0.14).min(60.0);
        let control = (
            (start.0 + end.0) / 2.0 - uy * bend,
            (start.1 + end.1) / 2.0 + sag + ux * bend,
        );
        Some(Self {
            start,
            control,
            end,
        })
    }

    /// The same curve cut short at `t`, for an arrowhead that has to stop
    /// before the end.
    pub fn until(&self, t: f32) -> Self {
        let lerp = |a: (f32, f32), b: (f32, f32)| (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
        Self {
            start: self.start,
            control: lerp(self.start, self.control),
            end: self.at(t),
        }
    }

    pub fn at(&self, t: f32) -> (f32, f32) {
        let (a, b, c) = ((1.0 - t) * (1.0 - t), 2.0 * t * (1.0 - t), t * t);
        (
            a * self.start.0 + b * self.control.0 + c * self.end.0,
            a * self.start.1 + b * self.control.1 + c * self.end.1,
        )
    }
}

/// Paints `curve` as a silk thread. A directed link's arrowhead is painted
/// separately by `arrowhead`, so it can go over labels the thread passes under.
pub fn thread(window: &mut Window, curve: Curve, style: ThreadStyle) {
    let p = |(x, y): (f32, f32)| point(px(x), px(y));
    let mut line = PathBuilder::stroke(px(style.width));
    if let Some((on, off)) = style.dash {
        line = line.dash_array(&[px(on), px(off)]);
    }
    line.move_to(p(curve.start));
    line.curve_to(p(curve.end), p(curve.control));
    paint(window, line, style.color);
}

pub fn arrowhead(window: &mut Window, curve: Curve, color: Hsla) {
    let ((ex, ey), (cx, cy)) = (curve.end, curve.control);
    let tl = (ex - cx).hypot(ey - cy).max(0.01);
    let (dx, dy) = ((ex - cx) / tl, (ey - cy) / tl);
    let (bx, by) = (ex - dx * 9.0, ey - dy * 9.0);
    let mut head = PathBuilder::fill();
    head.move_to(point(px(ex), px(ey)));
    head.line_to(point(px(bx - dy * 4.5), px(by + dx * 4.5)));
    head.line_to(point(px(bx + dy * 4.5), px(by - dx * 4.5)));
    head.close();
    paint(window, head, color);
}

#[derive(Clone, Copy)]
pub enum Align {
    Left,
    Center,
}

/// One line of text, returning its width. `halo` paints the text a few times
/// in that color first, offset by a pixel, so it stays readable over web lines
/// without a box behind it.
#[allow(clippy::too_many_arguments)]
pub fn text(
    window: &mut Window,
    cx: &mut App,
    content: &str,
    at: Point<Pixels>,
    size: f32,
    weight: FontWeight,
    color: Hsla,
    align: Align,
    halo: Option<Hsla>,
) -> f32 {
    if content.is_empty() {
        return 0.0;
    }
    // Servers we don't control can send anything; a newline would also break shaping.
    let content: String = content
        .chars()
        .map(|c| if is_unsafe_char(c) { '\u{FFFD}' } else { c })
        .collect();
    let mut font = window.text_style().font();
    font.weight = weight;
    let shape = |window: &mut Window, color: Hsla| {
        let run = TextRun {
            len: content.len(),
            font: font.clone(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        window
            .text_system()
            .shape_line(SharedString::from(content.clone()), px(size), &[run], None)
    };
    let shaped = shape(window, color);
    let width = f32::from(shaped.width);
    let x = match align {
        Align::Left => at.x,
        Align::Center => at.x - px(width / 2.0),
    };
    let line_height = px(size * 1.3);
    if let Some(halo) = halo {
        let behind = shape(window, halo);
        for (dx, dy) in [(-1.0, 0.0), (1.0, 0.0), (0.0, -1.0), (0.0, 1.0)] {
            let _ = behind.paint(point(x + px(dx), at.y + px(dy)), line_height, window, cx);
        }
    }
    let _ = shaped.paint(point(x, at.y), line_height, window, cx);
    width
}

/// Titles can be up to 300 characters; on the canvas they're cut short.
pub fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max - 1).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_ends_at_the_drawn_outline_not_a_circle_around_it() {
        // One pixel per unit of the eye box.
        let edge = |shape, dir| eye_edge(shape, EYE_BOX_W, dir);
        let (up, down, side) = ((0.0, -1.0), (0.0, 1.0), (1.0, 0.0));
        // A done crescent hangs below the middle: from above a thread comes
        // right in, from below it stops at the crescent's lower edge.
        assert!(edge(Shape::Done, up) < 0.5, "{}", edge(Shape::Done, up));
        assert!((edge(Shape::Done, down) - 3.2).abs() < 0.3);
        // A review's side beads count from the side only.
        assert!(edge(Shape::Review, side) > edge(Shape::Review, up) + 3.0);
        // A round eye reaches as far every way.
        assert_eq!(edge(Shape::Doing, up), edge(Shape::Doing, side));
    }
}
