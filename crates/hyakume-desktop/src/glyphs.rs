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
            for (x, y, r) in [
                (5.0, 6.2, 2.6),
                (23.0, 6.2, 2.6),
                (3.0, 12.0, 1.7),
                (25.0, 12.0, 1.7),
            ] {
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
    circle(window, center, r, theme.web_fill.opacity(alpha), None);
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

/// A link as a silk thread that sags under its own weight. `gap` keeps the
/// ends off the eyes. Directed threads get an arrowhead at `to`.
pub fn thread(
    window: &mut Window,
    from: Point<Pixels>,
    to: Point<Pixels>,
    gap: f32,
    style: ThreadStyle,
) -> Option<Point<Pixels>> {
    let ThreadStyle {
        color,
        width,
        dash,
        arrow,
    } = style;
    let (fx, fy, tx, ty) = (
        f32::from(from.x),
        f32::from(from.y),
        f32::from(to.x),
        f32::from(to.y),
    );
    let d = (tx - fx).hypot(ty - fy);
    if d < gap * 2.0 + 4.0 {
        return None;
    }
    let (ux, uy) = ((tx - fx) / d, (ty - fy) / d);
    let (sx, sy) = (fx + ux * gap, fy + uy * gap);
    let (ex, ey) = (tx - ux * gap, ty - uy * gap);
    let (cx, cy) = ((sx + ex) / 2.0, (sy + ey) / 2.0 + (d * 0.14).min(60.0));
    let mut line = PathBuilder::stroke(px(width));
    if let Some((on, off)) = dash {
        line = line.dash_array(&[px(on), px(off)]);
    }
    line.move_to(point(px(sx), px(sy)));
    line.curve_to(point(px(ex), px(ey)), point(px(cx), px(cy)));
    paint(window, line, color);
    if arrow {
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
    // The curve's midpoint, for a label.
    Some(point(
        px(0.25 * sx + 0.5 * cx + 0.25 * ex),
        px(0.25 * sy + 0.5 * cy + 0.25 * ey),
    ))
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
