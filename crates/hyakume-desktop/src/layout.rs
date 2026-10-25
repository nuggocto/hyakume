//! Where things sit, in world units. Positions stick: a refresh only places
//! what is new, and only an explicit re-layout moves what was already there.
//! Everything is deterministic, so the same data always lands the same way.

use std::collections::HashMap;
use std::ops::{Add, AddAssign, Mul, Sub, SubAssign};

use crate::scene::{NodeKey, Scene};

/// Eye width in world units; the eye is 28×20 in its own drawing units.
pub const EYE_W: f32 = 24.0;
/// Space under a column header before its first web.
pub const COLUMN_HEAD: f32 = 150.0;
const COLUMN_MIN_W: f32 = 380.0;
/// Between one web's edge and the next: its count below, the next name above.
const CLUSTER_GAP: f32 = 110.0;
/// How much bigger than it needs to be a web may get before it grows. A web
/// that grew would move every eye on it, so it waits until it's crowded.
const GROWTH_SLACK: f32 = 24.0;
/// Eyes push apart until they're about this far from each other.
const EYE_SPACING: f32 = 104.0;
const LINK_LENGTH: f32 = 96.0;
const GOLDEN_ANGLE: f32 = 2.399_963;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    pub fn len(self) -> f32 {
        self.x.hypot(self.y)
    }

    fn polar(angle: f32, radius: f32) -> Self {
        Self::new(angle.cos() * radius, angle.sin() * radius)
    }
}

impl Add for Vec2 {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        Self::new(self.x + o.x, self.y + o.y)
    }
}

impl Sub for Vec2 {
    type Output = Self;
    fn sub(self, o: Self) -> Self {
        Self::new(self.x - o.x, self.y - o.y)
    }
}

impl Mul<f32> for Vec2 {
    type Output = Self;
    fn mul(self, k: f32) -> Self {
        Self::new(self.x * k, self.y * k)
    }
}

impl AddAssign for Vec2 {
    fn add_assign(&mut self, o: Self) {
        *self = *self + o;
    }
}

impl SubAssign for Vec2 {
    fn sub_assign(&mut self, o: Self) {
        *self = *self - o;
    }
}

/// Big enough that the eyes keep their spacing inside the web.
pub fn cluster_radius(eyes: usize) -> f32 {
    90.0 + 52.0 * (eyes as f32).sqrt()
}

/// Absolute positions, index-aligned with the scene they were placed for.
#[derive(Default)]
pub struct Placement {
    /// Left edge and width.
    pub columns: Vec<(f32, f32)>,
    /// Center and radius.
    pub clusters: Vec<(Vec2, f32)>,
    pub eyes: Vec<Vec2>,
}

impl Placement {
    /// Top-left and bottom-right corners around everything placed.
    pub fn bounds(&self) -> (Vec2, Vec2) {
        let mut min = Vec2::new(f32::MAX, f32::MAX);
        let mut max = Vec2::new(f32::MIN, f32::MIN);
        for &(x, w) in &self.columns {
            min.x = min.x.min(x);
            max.x = max.x.max(x + w);
            min.y = min.y.min(0.0);
            // Room for the placeholder web an empty column shows.
            max.y = max.y.max(COLUMN_HEAD + 280.0);
        }
        for &(c, r) in &self.clusters {
            min.y = min.y.min(c.y - r - 40.0);
            max.y = max.y.max(c.y + r + 40.0);
        }
        (min, max)
    }
}

#[derive(Default)]
pub struct Layout {
    /// Left edge of each column.
    columns: HashMap<NodeKey, f32>,
    /// Web center (x relative to its column's left edge) and radius.
    clusters: HashMap<NodeKey, (Vec2, f32)>,
    /// Eye position relative to its web's center.
    eyes: HashMap<NodeKey, Vec2>,
}

impl Layout {
    /// Forgets every position, then places everything fresh.
    pub fn relayout(&mut self, scene: &Scene) -> Placement {
        *self = Self::default();
        self.place(scene)
    }

    /// Keeps known positions and finds room for anything new.
    pub fn place(&mut self, scene: &Scene) -> Placement {
        let radii: Vec<f32> = scene
            .clusters
            .iter()
            .map(|c| {
                let needed = cluster_radius(c.eyes.len());
                match self.clusters.get(&c.key) {
                    Some(&(_, r)) if needed <= r + GROWTH_SLACK => r,
                    Some(_) => needed + GROWTH_SLACK,
                    None => needed,
                }
            })
            .collect();
        let mut placement = Placement {
            columns: Vec::with_capacity(scene.columns.len()),
            clusters: vec![(Vec2::default(), 0.0); scene.clusters.len()],
            eyes: vec![Vec2::default(); scene.eyes.len()],
        };

        let mut right = 0.0_f32;
        for column in &scene.columns {
            let widest = column
                .clusters
                .clone()
                .map(|c| 2.0 * radii[c] + 120.0)
                .fold(COLUMN_MIN_W, f32::max);
            // A column that grew pushes the ones after it right, never over them.
            let x = self
                .columns
                .get(&column.key)
                .copied()
                .unwrap_or(right)
                .max(right);
            self.columns.insert(column.key.clone(), x);
            placement.columns.push((x, widest));
            right = x + widest;

            // Known webs keep their spot. New ones start below everything
            // already in the column, whatever order their keys sort in.
            let mut lowest = COLUMN_HEAD - CLUSTER_GAP;
            for c in column.clusters.clone() {
                if let Some(&(at, _)) = self.clusters.get(&scene.clusters[c].key) {
                    lowest = lowest.max(at.y + radii[c]);
                }
            }
            let mut wanted: Vec<(usize, Vec2)> = Vec::with_capacity(column.clusters.len());
            for (n, c) in column.clusters.clone().enumerate() {
                let r = radii[c];
                let room = (widest / 2.0 - r - 30.0).max(0.0);
                let at = match self.clusters.get(&scene.clusters[c].key) {
                    // Anchored to the left edge, so a column that widens leaves it alone.
                    Some(&(at, _)) => {
                        Vec2::new(at.x.clamp(widest / 2.0 - room, widest / 2.0 + room), at.y)
                    }
                    // Zigzag a little so a long column doesn't read as a list.
                    None => {
                        let side = if n % 2 == 0 { -1.0 } else { 1.0 };
                        let y = lowest + CLUSTER_GAP + r;
                        lowest = y + r;
                        Vec2::new(widest / 2.0 + side * room * 0.6, y)
                    }
                };
                wanted.push((c, at));
            }
            // One pass from the top pushes down any web that would overlap the
            // one above it, so neither equal centers nor a web that grew past
            // its neighbour can leave two on top of each other.
            wanted.sort_by(|a, b| a.1.y.total_cmp(&b.1.y).then(a.0.cmp(&b.0)));
            let mut free_from = COLUMN_HEAD;
            for (c, mut at) in wanted {
                let r = radii[c];
                at.y = at.y.max(free_from + r);
                free_from = at.y + r + CLUSTER_GAP;
                self.clusters.insert(scene.clusters[c].key.clone(), (at, r));
                placement.clusters[c] = (Vec2::new(x + at.x, at.y), r);
            }
        }

        for (c, cluster) in scene.clusters.iter().enumerate() {
            let (center, r) = placement.clusters[c];
            let eyes: Vec<usize> = cluster.eyes.clone().collect();
            let local: HashMap<usize, usize> =
                eyes.iter().enumerate().map(|(i, &e)| (e, i)).collect();
            let inner = r * 0.8;
            let mut points = Vec::with_capacity(eyes.len());
            let mut movable = Vec::with_capacity(eyes.len());
            for (i, &e) in eyes.iter().enumerate() {
                match self.eyes.get(&scene.eyes[e].key) {
                    Some(&p) => {
                        points.push(clamp_len(p, inner));
                        movable.push(false);
                    }
                    None => {
                        let k = ((i as f32 + 0.5) / eyes.len() as f32).sqrt();
                        points.push(Vec2::polar(i as f32 * GOLDEN_ANGLE, k * r * 0.72));
                        movable.push(true);
                    }
                }
            }
            if movable.contains(&true) {
                let springs: Vec<(usize, usize)> = scene
                    .threads
                    .iter()
                    .filter_map(|t| Some((*local.get(&t.from)?, *local.get(&t.to)?)))
                    .collect();
                settle(&mut points, &movable, &springs, inner);
            }
            for (i, &e) in eyes.iter().enumerate() {
                self.eyes.insert(scene.eyes[e].key.clone(), points[i]);
                placement.eyes[e] = center + points[i];
            }
        }
        placement
    }
}

fn clamp_len(p: Vec2, max: f32) -> Vec2 {
    let len = p.len();
    if len > max { p * (max / len) } else { p }
}

/// A small force layout: eyes push apart, linked eyes pull together, and
/// everything drifts gently to the middle. Fixed eyes don't move but still
/// push, so a new eye finds a gap instead of landing on an old one.
fn settle(points: &mut [Vec2], movable: &[bool], springs: &[(usize, usize)], inner: f32) {
    let n = points.len();
    // Pairwise pushing is O(n²); very large projects keep their spiral for now.
    let rounds = match n {
        0..=200 => 160,
        201..=1000 => 30,
        _ => 0,
    };
    for _ in 0..rounds {
        let mut delta = vec![Vec2::default(); n];
        for i in 0..n {
            for j in i + 1..n {
                let mut d = points[i] - points[j];
                let mut dist = d.len();
                if dist < 0.01 {
                    d = Vec2::polar(i as f32 * GOLDEN_ANGLE, 1.0);
                    dist = 1.0;
                }
                if dist < EYE_SPACING {
                    let push = d * ((EYE_SPACING - dist) / dist * 0.5);
                    delta[i] += push;
                    delta[j] -= push;
                }
            }
        }
        for &(a, b) in springs {
            let d = points[b] - points[a];
            let dist = d.len().max(0.01);
            let pull = d * ((dist - LINK_LENGTH) / dist * 0.04);
            delta[a] += pull;
            delta[b] -= pull;
        }
        for i in 0..n {
            if movable[i] {
                let step = clamp_len(delta[i] - points[i] * 0.01, 18.0);
                points[i] = clamp_len(points[i] + step, inner);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::tests::Fixture;

    fn tasks(n: usize) -> Vec<(String, usize)> {
        (0..n).map(|i| (format!("Task {i}"), i % 5)).collect()
    }

    fn project(ws: &mut Fixture, key: &str, n: usize) {
        let tasks = tasks(n);
        let refs: Vec<(&str, usize)> = tasks.iter().map(|(t, s)| (t.as_str(), *s)).collect();
        ws.project(key, &refs);
    }

    #[test]
    fn a_refresh_keeps_old_eyes_still_and_finds_room_for_new_ones() {
        let mut ws = Fixture::new();
        project(&mut ws, "WEB", 8);
        let mut layout = Layout::default();
        let before = layout.place(&Scene::build(&[ws.source()]));

        ws.add_task(0, "New task", 1);
        let after = layout.place(&Scene::build(&[ws.source()]));

        assert_eq!(&after.eyes[..8], &before.eyes[..8], "old eyes stay put");
        let (center, r) = after.clusters[0];
        let new = after.eyes[8];
        assert!((new - center).len() < r, "the new eye is on its web");
        let nearest = before
            .eyes
            .iter()
            .map(|&e| (e - new).len())
            .fold(f32::MAX, f32::min);
        assert!(
            nearest > EYE_W * 1.5,
            "the new eye doesn't sit on an old one ({nearest})"
        );
    }

    #[test]
    fn the_same_data_lands_the_same_way() {
        let mut ws = Fixture::new();
        project(&mut ws, "WEB", 12);
        project(&mut ws, "API", 5);
        let scene = Scene::build(&[ws.source()]);
        let a = Layout::default().place(&scene);
        let b = Layout::default().place(&scene);
        assert_eq!(a.eyes, b.eyes);
        assert_eq!(a.clusters, b.clusters);
    }

    fn assert_apart(placement: &Placement) {
        let webs = &placement.clusters;
        for i in 0..webs.len() {
            for j in i + 1..webs.len() {
                let ((a, ar), (b, br)) = (webs[i], webs[j]);
                assert!(
                    (a - b).len() >= ar + br,
                    "webs {i} and {j} overlap: {webs:?}"
                );
            }
        }
    }

    #[test]
    fn a_new_project_that_sorts_first_doesnt_land_on_an_existing_one() {
        let mut ws = Fixture::new();
        project(&mut ws, "WEB", 0);
        let mut layout = Layout::default();
        layout.place(&Scene::build(&[ws.source()]));

        // API sorts before WEB, so it comes first in the scene.
        let mut api = Fixture::new();
        api.workspace = ws.workspace.clone();
        project(&mut api, "API", 0);
        ws.snapshots.insert(0, api.snapshots.remove(0));
        let after = layout.place(&Scene::build(&[ws.source()]));
        assert_apart(&after);
    }

    #[test]
    fn a_growing_web_pushes_the_next_one_down_instead_of_overlapping() {
        let mut ws = Fixture::new();
        project(&mut ws, "API", 2);
        project(&mut ws, "WEB", 2);
        let mut layout = Layout::default();
        layout.place(&Scene::build(&[ws.source()]));

        for i in 0..57 {
            ws.add_task(0, &format!("More {i}"), 0);
        }
        let after = layout.place(&Scene::build(&[ws.source()]));

        let (api, api_r) = after.clusters[0];
        let (web, web_r) = after.clusters[1];
        assert!(
            (web - api).len() >= api_r + web_r,
            "webs overlap: {api:?} r={api_r}, {web:?} r={web_r}"
        );
        for &eye in &after.eyes[..59] {
            assert!((eye - api).len() < api_r, "API's eyes stay on API's web");
        }
    }
}
