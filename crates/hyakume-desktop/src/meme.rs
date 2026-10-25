//! Meme, drawn with the same painter as the canvas. A port of the approved
//! mascot mockup: same 200×200 geometry, colors from the theme. GPUI 0.2.2
//! swaps red and blue in SVG images, so Meme isn't shipped as one.

use std::sync::OnceLock;

use gpui::{
    Bounds, Hsla, PathBuilder, Pixels, Point, Window, linear_color_stop, linear_gradient, point, px,
};

use crate::glyphs::circle;
use crate::theme::MemeColors;

/// One of the 99 small eyes, in mockup coordinates.
#[derive(Clone, Copy, Debug)]
pub struct Tiny {
    pub x: f32,
    pub y: f32,
    pub r: f32,
    /// Mid-blink: drawn as a closed arc.
    pub closed: bool,
}

/// The eyes that sit on Meme's face; the small ones keep clear of them.
const KEEP_OUT: [(f32, f32, f32); 6] = [
    (100.0, 108.0, 35.0),
    (80.0, 71.0, 11.0),
    (120.0, 71.0, 11.0),
    (55.0, 134.0, 14.0),
    (145.0, 134.0, 14.0),
    (100.0, 153.0, 13.0),
];

/// Exactly 99, as PROJECT.md asks. They start on a golden-angle spiral, then
/// push each other apart for a while so they spread evenly instead of clumping.
pub fn tiny_eyes() -> &'static [Tiny] {
    static EYES: OnceLock<Vec<Tiny>> = OnceLock::new();
    EYES.get_or_init(|| {
        let blocked = |x: f32, y: f32| {
            KEEP_OUT
                .iter()
                .any(|&(kx, ky, kr)| (x - kx).hypot(y - ky) < kr)
        };
        let mut pts: Vec<(f32, f32)> = Vec::with_capacity(99);
        let mut i = 0;
        while pts.len() < 99 {
            let r = ((i as f32 + 0.5) / 160.0).sqrt();
            let a = i as f32 * 2.399_963;
            let (x, y) = (100.0 + a.cos() * r * 62.0, 117.0 + a.sin() * r * 52.0);
            if !blocked(x, y) {
                pts.push((x, y));
            }
            i += 1;
        }
        const SPACING: f32 = 7.8;
        for _ in 0..120 {
            let pushed: Vec<(f32, f32)> = pts
                .iter()
                .enumerate()
                .map(|(i, &(x, y))| {
                    let (mut nx, mut ny) = (x, y);
                    for (j, &(ox, oy)) in pts.iter().enumerate() {
                        let d = (x - ox).hypot(y - oy);
                        if j != i && d < SPACING && d > 0.0 {
                            nx += (x - ox) / d * (SPACING - d) * 0.3;
                            ny += (y - oy) / d * (SPACING - d) * 0.3;
                        }
                    }
                    (nx, ny)
                })
                .collect();
            pts = pushed
                .into_iter()
                .map(|(mut x, mut y)| {
                    for &(kx, ky, kr) in &KEEP_OUT {
                        let d = (x - kx).hypot(y - ky);
                        if d < kr && d > 0.0 {
                            x = kx + (x - kx) / d * kr;
                            y = ky + (y - ky) / d * kr;
                        }
                    }
                    let e = ((x - 100.0) / 64.0).hypot((y - 117.0) / 55.0);
                    if e > 1.0 {
                        x = 100.0 + (x - 100.0) / e;
                        y = 117.0 + (y - 117.0) / e;
                    }
                    (x, y)
                })
                .collect();
        }
        pts.into_iter()
            .enumerate()
            .map(|(i, (x, y))| Tiny {
                x,
                y,
                // A little larger near the big eye.
                r: (2.8 - ((x - 100.0).hypot(y - 108.0) - 35.0) * 0.045).clamp(1.8, 2.8),
                closed: i % 9 == 4,
            })
            .collect()
    })
}

/// Paints Meme into the square `bounds`. With `thread_from`, it hangs on a
/// silk thread that starts at that height, such as the window's top edge.
pub fn paint(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    c: &MemeColors,
    thread_from: Option<Pixels>,
) {
    let size = f32::from(bounds.size.width.min(bounds.size.height));
    let s = size / 200.0;
    let o = bounds.origin;
    let at = |x: f32, y: f32| point(o.x + px(x * s), o.y + px(y * s));
    // Hairlines stay visible at header size.
    let w = |width: f32| (width * s).max(0.6);
    let fill = |window: &mut Window, b: PathBuilder, color: Hsla| {
        if let Ok(path) = b.build() {
            window.paint_path(path, color);
        }
    };
    let dot = |window: &mut Window, x: f32, y: f32, r: f32, color: Hsla| {
        circle(window, at(x, y), r * s, color, None);
    };
    let edge = c.rim.unwrap_or(c.body);

    if let Some(top) = thread_from {
        let mut thread = PathBuilder::stroke(px((1.6 * s).max(1.0)));
        thread.move_to(point(at(100.0, 0.0).x, top));
        thread.line_to(at(100.0, 41.0));
        fill(window, thread, c.silk);
    }

    // Eight short arched legs with red tips; round ends drawn as dots.
    const LEFT: [[f32; 6]; 4] = [
        [58.0, 90.0, 26.0, 48.0, 14.0, 80.0],
        [46.0, 108.0, 8.0, 92.0, 6.0, 124.0],
        [46.0, 130.0, 8.0, 136.0, 14.0, 162.0],
        [60.0, 150.0, 34.0, 176.0, 38.0, 190.0],
    ];
    let legs: Vec<[f32; 6]> = LEFT
        .iter()
        .flat_map(|&[sx, sy, cx, cy, fx, fy]| {
            [
                [sx, sy, cx, cy, fx, fy],
                [200.0 - sx, sy, 200.0 - cx, cy, 200.0 - fx, fy],
            ]
        })
        .collect();
    for (width, color) in [(11.0, edge), (8.0, c.body)] {
        let mut b = PathBuilder::stroke(px(width * s));
        for &[sx, sy, cx, cy, fx, fy] in &legs {
            b.move_to(at(sx, sy));
            b.curve_to(at(fx, fy), at(cx, cy));
        }
        fill(window, b, color);
        for &[.., fx, fy] in &legs {
            dot(window, fx, fy, width / 2.0, color);
        }
    }
    for &[.., fx, fy] in &legs {
        dot(window, fx, fy, 3.6, c.red);
    }

    // The ahoge, the single curl of hair it hangs from.
    for (width, color) in [(7.5, edge), (4.5, c.body)] {
        let mut b = PathBuilder::stroke(px(width * s));
        b.move_to(at(97.0, 58.0));
        b.curve_to(at(99.5, 41.0), at(92.0, 46.0));
        b.curve_to(at(108.0, 45.0), at(107.0, 37.0));
        fill(window, b, color);
        dot(window, 108.0, 45.0, width / 2.0, color);
    }

    // A soft fuzzy body: small uneven bumps around an ellipse.
    let fur = |k_body: f32| {
        const N: usize = 44;
        let step = std::f32::consts::TAU / N as f32;
        let p = |a: f32, k: f32| {
            at(
                100.0 + a.cos() * 74.0 * k * k_body,
                116.0 + a.sin() * 64.0 * k * k_body,
            )
        };
        let mut b = PathBuilder::fill();
        b.move_to(p(0.0, 0.98));
        for i in 0..N {
            let a = i as f32 * step;
            let bump = 1.04 + 0.014 * (i as f32 * 2.7).sin();
            b.curve_to(p(a + step, 0.98), p(a + step / 2.0, bump));
        }
        b.close();
        b
    };
    if let Some(rim) = c.rim {
        // The rim is the same fur, a touch larger, under the body.
        fill(window, fur(1.0 + 2.0 / 74.0), rim);
    }
    if let Ok(path) = fur(1.0).build() {
        // Lit from the upper left, like the mockup's radial shading.
        let shade = linear_gradient(
            160.0,
            linear_color_stop(c.body_light, 0.0),
            linear_color_stop(c.body_deep, 1.0),
        );
        window.paint_path(path, shade);
    }

    // Blush: stacked ellipses fading out, then three little hatches each side.
    for (cx, cy) in [(55.0, 134.0), (145.0, 134.0)] {
        for k in [1.0, 0.75, 0.5, 0.28] {
            fill(
                window,
                ellipse(&at, cx, cy, 15.0 * k, 9.0 * k, 0.0, s),
                c.red.opacity(0.14),
            );
        }
    }
    let mut hatches = PathBuilder::stroke(px(w(1.2)));
    for (x0, x1) in [(51.0, 53.5), (56.0, 58.5), (146.0, 148.5), (141.0, 143.5)] {
        hatches.move_to(at(x0, 136.5));
        hatches.line_to(at(x1, 131.5));
    }
    fill(window, hatches, c.hatch.opacity(0.8));

    // The 99 small eyes; slits and glints only when there's room for them.
    let detail = size >= 80.0;
    let mut closed = PathBuilder::stroke(px(w(if detail { 1.0 } else { 1.5 })));
    for t in tiny_eyes() {
        if t.closed {
            closed.move_to(at(t.x - t.r, t.y - 0.3));
            closed.curve_to(at(t.x + t.r, t.y - 0.3), at(t.x, t.y + t.r));
            continue;
        }
        dot(window, t.x, t.y, t.r, c.gold);
        if detail && t.r >= 2.2 {
            fill(window, slit(&at, t.x, t.y, t.r * 0.75, t.r * 0.34), c.pupil);
            dot(
                window,
                t.x - t.r * 0.36,
                t.y - t.r * 0.32,
                t.r * 0.26,
                c.glint,
            );
        }
    }
    fill(window, closed, c.gold);

    // Round court-style brows.
    fill(window, ellipse(&at, 80.0, 71.0, 7.5, 4.6, -14.0, s), c.red);
    fill(window, ellipse(&at, 120.0, 71.0, 7.5, 4.6, 14.0, s), c.red);

    // The big lantern eye: socket, iris shading, fibres, slit, glints.
    dot(window, 100.0, 108.0, 29.5, c.socket);
    dot(window, 100.0, 108.0, 27.0, c.gold_edge);
    dot(window, 100.0, 108.0, 25.0, c.gold);
    dot(window, 100.0, 108.0, 19.0, c.gold_light.opacity(0.45));
    dot(window, 100.0, 108.0, 13.0, c.gold_light.opacity(0.75));
    let mut fibres = PathBuilder::stroke(px(w(0.9)));
    for i in 0..28 {
        let a = i as f32 / 28.0 * std::f32::consts::TAU;
        fibres.move_to(at(100.0 + a.cos() * 10.0, 108.0 + a.sin() * 10.0));
        fibres.line_to(at(100.0 + a.cos() * 24.0, 108.0 + a.sin() * 24.0));
    }
    fill(window, fibres, c.gold_deep.opacity(0.35));
    fill(window, slit(&at, 100.0, 108.0, 22.0, 12.5), c.pupil);
    dot(window, 90.0, 97.0, 6.2, c.glint);
    dot(window, 110.5, 119.5, 2.7, c.glint.opacity(0.9));

    // A small ω mouth with one crooked fang (yaeba).
    let mut mouth = PathBuilder::stroke(px(w(2.2)));
    mouth.move_to(at(89.0, 150.0));
    mouth.curve_to(at(100.0, 150.0), at(94.5, 155.5));
    mouth.curve_to(at(111.0, 150.0), at(105.5, 155.5));
    fill(window, mouth, c.bone);
    let mut fang = PathBuilder::fill();
    fang.move_to(at(103.6, 152.6));
    fang.line_to(at(105.6, 158.0));
    fang.line_to(at(107.8, 152.0));
    fang.close();
    fill(window, fang, c.bone);
}

/// A vertical lens-shaped slit pupil, `tall` and `wide` from its center.
fn slit(
    at: &impl Fn(f32, f32) -> Point<Pixels>,
    x: f32,
    y: f32,
    tall: f32,
    wide: f32,
) -> PathBuilder {
    let mut b = PathBuilder::fill();
    b.move_to(at(x, y - tall));
    b.curve_to(at(x, y + tall), at(x + wide, y));
    b.curve_to(at(x, y - tall), at(x - wide, y));
    b.close();
    b
}

fn ellipse(
    at: &impl Fn(f32, f32) -> Point<Pixels>,
    cx: f32,
    cy: f32,
    rx: f32,
    ry: f32,
    degrees: f32,
    s: f32,
) -> PathBuilder {
    let (sin, cos) = degrees.to_radians().sin_cos();
    let mut b = PathBuilder::fill();
    let radii = point(px(rx * s), px(ry * s));
    b.move_to(at(cx + rx * cos, cy + rx * sin));
    b.arc_to(
        radii,
        px(degrees),
        false,
        true,
        at(cx - rx * cos, cy - rx * sin),
    );
    b.arc_to(
        radii,
        px(degrees),
        false,
        true,
        at(cx + rx * cos, cy + rx * sin),
    );
    b.close();
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meme_has_exactly_99_small_eyes_on_its_body_and_off_its_face() {
        let eyes = tiny_eyes();
        assert_eq!(eyes.len(), 99);
        for t in eyes {
            let body = ((t.x - 100.0) / 74.0).hypot((t.y - 116.0) / 64.0);
            assert!(body < 0.98, "({}, {}) is off the body", t.x, t.y);
            let from_big_eye = (t.x - 100.0).hypot(t.y - 108.0);
            assert!(
                from_big_eye >= 29.5 + t.r - 0.01,
                "({}, {}) sits on the big eye",
                t.x,
                t.y
            );
        }
    }
}
