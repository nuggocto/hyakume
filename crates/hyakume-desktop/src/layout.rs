//! Where things sit, in world units. Positions stick: a refresh only places
//! what is new, and only an explicit re-layout moves what was already there,
//! never what the person placed by hand. Same data, same layout, every time.

use std::collections::{HashMap, HashSet};
use std::ops::{Add, AddAssign, Mul, Sub, SubAssign};

use hyakume_core::api::{LayoutNode, NodeKind};

use crate::scene::{NodeKey, Scene};
use crate::sources::{SourceKey, SourceState};

/// Eye width in world units; the eye is 28×20 in its own drawing units.
pub const EYE_W: f32 = 24.0;
/// Space under a column header before its first web.
pub const COLUMN_HEAD: f32 = 150.0;
const COLUMN_MIN_W: f32 = 380.0;
/// Between one web's edge and the next: its count below, the next name
/// above. Wide enough that the name still fits when a big workspace beside
/// it zooms the overview out to about 20%.
const CLUSTER_GAP: f32 = 240.0;
/// How much bigger than it needs to be a web may get before it grows. A web
/// that grew would move every eye on it, so it waits until it's crowded.
const GROWTH_SLACK: f32 = 24.0;
/// Eyes push apart until they're about this far from each other.
const EYE_SPACING: f32 = 104.0;
const LINK_LENGTH: f32 = 96.0;
const GOLDEN_ANGLE: f32 = 2.399_963;
/// The most a web sits off its column's middle, for the zigzag.
const ZIGZAG: f32 = 60.0;

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
    /// Eyes the person placed by hand, which re-layout leaves alone.
    pub pinned: Vec<bool>,
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
    /// How wide each column must be for its names; text keeps its size
    /// while the canvas zooms, so only the painter can measure this.
    min_widths: HashMap<NodeKey, f32>,
    /// How wide each column came out last time, so `widen` knows how far
    /// to slide its webs to keep them in the middle.
    widths: HashMap<NodeKey, f32>,
    /// Eyes and webs placed by hand. Re-layout keeps them where they are.
    pinned: HashSet<NodeKey>,
    /// Saved positions as last applied, so a refresh that carries the same
    /// saved layout doesn't drag back something moved since.
    applied: HashMap<NodeKey, LayoutNode>,
    /// The last position this device sent per node, until it comes back.
    /// Any other saved value seen meanwhile is older than it.
    sent: HashMap<NodeKey, LayoutNode>,
}

impl Layout {
    /// Forgets every position, then places everything fresh. The widths the
    /// names need are measurements, not positions, so they stay. Also hands
    /// back the saved, unpinned positions it moved: unsaved, the next start
    /// or another device would put them back.
    pub fn relayout(&mut self, scene: &Scene) -> (Placement, Vec<(NodeKey, LayoutNode)>) {
        let mut saved: HashMap<NodeKey, LayoutNode> = self.applied.clone();
        saved.extend(self.sent.clone());
        let pinned = std::mem::take(&mut self.pinned);
        let mut eyes = std::mem::take(&mut self.eyes);
        let mut clusters = std::mem::take(&mut self.clusters);
        eyes.retain(|k, _| pinned.contains(k));
        clusters.retain(|k, _| pinned.contains(k));
        *self = Self {
            min_widths: std::mem::take(&mut self.min_widths),
            applied: std::mem::take(&mut self.applied),
            sent: std::mem::take(&mut self.sent),
            eyes,
            clusters,
            pinned,
            ..Self::default()
        };
        let placement = self.place(scene);
        let mut moved = Vec::new();
        for (key, node) in saved.into_iter().filter(|(_, n)| !n.pinned) {
            let at = match node.kind {
                NodeKind::Task => self.eyes.get(&key).copied(),
                NodeKind::Project => self.clusters.get(&key).map(|&(at, _)| at),
            };
            if let Some(at) = at.filter(|at| (f64::from(at.x), f64::from(at.y)) != (node.x, node.y))
            {
                moved.push((key.clone(), self.remember(key, node.kind, at, false)));
            }
        }
        (placement, moved)
    }

    /// Takes in positions saved on the server or in local.db: ones this
    /// device hasn't applied yet, such as a move made on another device.
    pub fn apply_saved(&mut self, scene: &Scene, sources: &[SourceState]) {
        for source in sources {
            for node in source.workspaces.iter().flat_map(|w| &w.layout) {
                let key = NodeKey {
                    source: source.key.clone(),
                    id: node.id,
                };
                match self.sent.get(&key) {
                    Some(sent) if sent != node => continue,
                    Some(_) => _ = self.sent.remove(&key),
                    None => {}
                }
                if self.applied.get(&key) == Some(node) {
                    continue;
                }
                let at = Vec2::new(node.x as f32, node.y as f32);
                match node.kind {
                    NodeKind::Task => {
                        self.eyes.insert(key.clone(), at);
                    }
                    NodeKind::Project => {
                        // A web keeps its size; only where it sits was saved.
                        let r = match self.clusters.get(&key) {
                            Some(&(_, r)) => r,
                            None => scene
                                .cluster(&key)
                                .map_or(0.0, |c| cluster_radius(scene.clusters[c].eyes.len())),
                        };
                        self.clusters.insert(key.clone(), (at, r));
                    }
                }
                match node.pinned {
                    true => self.pinned.insert(key.clone()),
                    false => self.pinned.remove(&key),
                };
                self.applied.insert(key, node.clone());
            }
        }
    }

    /// A save that failed won't come back, so other devices' moves of these
    /// nodes may land again. A newer save of the same node still waits.
    pub fn unsent(&mut self, source: &SourceKey, nodes: &[LayoutNode]) {
        for node in nodes {
            let key = NodeKey {
                source: source.clone(),
                id: node.id,
            };
            if self.sent.get(&key) == Some(node) {
                self.sent.remove(&key);
            }
        }
    }

    /// Puts eye `eye` at `world`, kept on its own web, and pins it. Returns
    /// the position to save.
    pub fn move_eye(
        &mut self,
        scene: &Scene,
        placement: &Placement,
        eye: usize,
        world: Vec2,
    ) -> LayoutNode {
        let (center, r) = placement.clusters[scene.eyes[eye].cluster];
        let at = clamp_len(world - center, r * 0.8);
        let key = scene.eyes[eye].key.clone();
        self.eyes.insert(key.clone(), at);
        self.pinned.insert(key.clone());
        self.remember(key, NodeKind::Task, at, true)
    }

    /// Puts web `cluster` with its center at `world` and pins it.
    pub fn move_cluster(
        &mut self,
        scene: &Scene,
        placement: &Placement,
        cluster: usize,
        world: Vec2,
    ) -> LayoutNode {
        let (left, _) = placement.columns[scene.clusters[cluster].column];
        let (_, r) = placement.clusters[cluster];
        // The column grows rightward to hold a pinned web, never leftward.
        let at = Vec2::new((world.x - left).max(r + 30.0), world.y);
        let key = scene.clusters[cluster].key.clone();
        self.clusters.insert(key.clone(), (at, r));
        self.pinned.insert(key.clone());
        self.remember(key, NodeKind::Project, at, true)
    }

    /// Pins or unpins an eye where it is now. Returns the position to save.
    pub fn toggle_pin(&mut self, scene: &Scene, eye: usize) -> Option<LayoutNode> {
        let key = scene.eyes[eye].key.clone();
        let at = *self.eyes.get(&key)?;
        let pinned = !self.pinned.remove(&key);
        if pinned {
            self.pinned.insert(key.clone());
        }
        Some(self.remember(key, NodeKind::Task, at, pinned))
    }

    /// The position to save, held in `sent` until its echo comes back: a
    /// refresh fetched before the save landed, or the echo of an earlier
    /// nudge, still carries an older spot, which must not win.
    fn remember(&mut self, key: NodeKey, kind: NodeKind, at: Vec2, pinned: bool) -> LayoutNode {
        let node = LayoutNode {
            kind,
            id: key.id,
            x: f64::from(at.x),
            y: f64::from(at.y),
            pinned,
        };
        self.sent.insert(key, node.clone());
        node
    }

    /// Asks for column `column` of `scene` to be at least `width` wide from
    /// the next `place`. True if that's wider than before. Its webs slide by
    /// half the growth to stay mid-column; pinned ones stay where they were put.
    pub fn widen(&mut self, scene: &Scene, column: usize, width: f32) -> bool {
        let column = &scene.columns[column];
        let known = self.min_widths.get(&column.key).copied().unwrap_or(0.0);
        if width <= known + 0.5 {
            return false;
        }
        self.min_widths.insert(column.key.clone(), width);
        let drawn = self.widths.get(&column.key).copied().unwrap_or(width);
        let slide = ((width - drawn) / 2.0).max(0.0);
        for c in column.clusters.clone() {
            let key = &scene.clusters[c].key;
            if self.pinned.contains(key) {
                continue;
            }
            if let Some((at, _)) = self.clusters.get_mut(key) {
                at.x += slide;
            }
        }
        true
    }

    /// Keeps known positions and finds room for anything new.
    pub fn place(&mut self, scene: &Scene) -> Placement {
        let radii: Vec<f32> = scene
            .clusters
            .iter()
            .map(|c| {
                // Room for every pinned eye where it was put, too: a web that
                // lost tasks would otherwise shrink and pull them inward.
                let held = c
                    .eyes
                    .clone()
                    .map(|e| &scene.eyes[e].key)
                    .filter(|k| self.pinned.contains(*k))
                    .filter_map(|k| self.eyes.get(k))
                    .map(|p| p.len() / 0.8 + 1.0)
                    .fold(0.0, f32::max);
                let needed = cluster_radius(c.eyes.len()).max(held);
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
            pinned: scene
                .eyes
                .iter()
                .map(|e| self.pinned.contains(&e.key))
                .collect(),
        };

        let pinned = |c: usize| self.pinned.contains(&scene.clusters[c].key);
        let mut right = 0.0_f32;
        for column in &scene.columns {
            let named = self.min_widths.get(&column.key).copied().unwrap_or(0.0);
            // Wide enough for the widest web, its names, and every web
            // placed by hand right where it was put.
            let held = |c: usize| match self.clusters.get(&scene.clusters[c].key) {
                Some(&(at, _)) if pinned(c) => at.x + radii[c] + 60.0,
                _ => 0.0,
            };
            let widest = column
                .clusters
                .clone()
                .map(|c| (2.0 * radii[c] + 120.0).max(held(c)))
                .fold(COLUMN_MIN_W.max(named), f32::max);
            // A column that grew pushes the ones after it right, never over them.
            let x = self
                .columns
                .get(&column.key)
                .copied()
                .unwrap_or(right)
                .max(right);
            self.columns.insert(column.key.clone(), x);
            self.widths.insert(column.key.clone(), widest);
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
                    Some(&(at, _)) if pinned(c) => at,
                    // Anchored to the left edge, so a web that grows doesn't drag
                    // its neighbours; only `widen` slides them, for names.
                    Some(&(at, _)) => {
                        Vec2::new(at.x.clamp(widest / 2.0 - room, widest / 2.0 + room), at.y)
                    }
                    // Zigzag a little so a long column doesn't read as a list,
                    // but only a little: a column widened for its names
                    // would otherwise push its webs out to the edges.
                    None => {
                        let side = if n % 2 == 0 { -1.0 } else { 1.0 };
                        let y = lowest + CLUSTER_GAP + r;
                        lowest = y + r;
                        Vec2::new(widest / 2.0 + side * (room * 0.6).min(ZIGZAG), y)
                    }
                };
                wanted.push((c, at));
            }
            // One pass from the top pushes down any web that would overlap the
            // one above it. Pinned webs never move; the rest also slide past
            // any pinned web they'd land on.
            wanted.sort_by(|a, b| a.1.y.total_cmp(&b.1.y).then(a.0.cmp(&b.0)));
            let pins: Vec<(f32, f32)> = wanted
                .iter()
                .filter(|&&(c, _)| pinned(c))
                .map(|&(c, at)| (at.y, radii[c]))
                .collect();
            let mut free_from = COLUMN_HEAD;
            for (c, mut at) in wanted {
                let r = radii[c];
                if !pinned(c) {
                    at.y = at.y.max(free_from + r);
                    // One pass, top down: moving below a pin can only meet the
                    // pins further down, and re-checking the same pin could spin
                    // forever on a rounding error.
                    for &(py, pr) in &pins {
                        if at.y - r < py + pr + CLUSTER_GAP && at.y + r + CLUSTER_GAP > py - pr {
                            at.y = py + pr + CLUSTER_GAP + r;
                        }
                    }
                }
                free_from = free_from.max(at.y + r + CLUSTER_GAP);
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
    fn a_column_widened_for_its_names_keeps_its_webs_in_the_middle_and_its_eyes_still() {
        let mut ws = Fixture::new();
        project(&mut ws, "API", 4);
        project(&mut ws, "WEB", 3);
        let mut layout = Layout::default();
        layout.place(&Scene::build(&[ws.source()]));
        // An eye added by a refresh sits where it found room, which a
        // layout started from scratch wouldn't reproduce.
        ws.add_task(1, "New task", 1);
        let scene = Scene::build(&[ws.source()]);
        let before = layout.place(&scene);
        let narrow = before.columns[0].1;
        assert!(layout.widen(&scene, 0, narrow + 600.0));
        assert!(!layout.widen(&scene, 0, narrow), "never narrows");

        let after = layout.place(&scene);
        let (x, w) = after.columns[0];
        assert!(w >= narrow + 600.0);
        for (c, &(center, _)) in after.clusters.iter().enumerate() {
            let off_middle = center.x - (x + w / 2.0);
            assert!(
                off_middle.abs() <= ZIGZAG + 0.5,
                "web {c} is {off_middle} off the middle"
            );
            assert_eq!(center.y, before.clusters[c].0.y, "web {c} kept its height");
        }
        for (e, eye) in scene.eyes.iter().enumerate() {
            let on_web = |p: &Placement| p.eyes[e] - p.clusters[eye.cluster].0;
            let drift = (on_web(&after) - on_web(&before)).len();
            assert!(drift < 0.01, "eye {e} moved {drift} on its web");
        }
    }

    #[test]
    fn re_layout_moves_everything_but_what_was_placed_by_hand() {
        let mut ws = Fixture::new();
        project(&mut ws, "WEB", 6);
        project(&mut ws, "API", 3);
        let scene = Scene::build(&[ws.source()]);
        let mut layout = Layout::default();
        let before = layout.place(&scene);
        let target = before.clusters[0].0 + Vec2::new(40.0, -30.0);
        layout.move_eye(&scene, &before, 2, target);
        let moved_web = before.clusters[1].0 + Vec2::new(0.0, 300.0);
        layout.move_cluster(&scene, &before, 1, moved_web);
        let placed = layout.place(&scene);

        let (after, _) = layout.relayout(&scene);

        // Eyes sit relative to their web, which re-layout may move.
        let on_web = |p: &Placement, e: usize| p.eyes[e] - p.clusters[0].0;
        assert_eq!(
            on_web(&after, 2),
            on_web(&placed, 2),
            "the dragged eye stays"
        );
        assert_eq!(
            after.clusters[1].0, placed.clusters[1].0,
            "the dragged web stays"
        );
        assert!(after.pinned[2] && !after.pinned[0]);
        let fresh = Layout::default().place(&scene);
        assert_ne!(
            on_web(&after, 0),
            on_web(&placed, 0),
            "the rest is placed again"
        );
        assert_ne!(
            on_web(&fresh, 0),
            on_web(&after, 0),
            "around the pinned eye"
        );
    }

    #[test]
    fn a_position_saved_elsewhere_lands_once_and_never_undoes_a_later_move() {
        let mut ws = Fixture::new();
        project(&mut ws, "WEB", 4);
        let scene = Scene::build(&[ws.source()]);
        let mut layout = Layout::default();
        let first = layout.place(&scene);
        let center = first.clusters[0].0;

        // Another device put WEB-1 here and saved it.
        let mut source = ws.source();
        source.workspaces[0].layout = vec![LayoutNode {
            kind: NodeKind::Task,
            id: scene.eyes[0].key.id,
            x: 50.0,
            y: 20.0,
            pinned: true,
        }];
        layout.apply_saved(&scene, std::slice::from_ref(&source));
        let synced = layout.place(&scene);
        assert_eq!(synced.eyes[0], center + Vec2::new(50.0, 20.0));
        assert!(synced.pinned[0]);

        // Then it's dragged here; a refresh carrying the old save leaves it.
        layout.move_eye(&scene, &synced, 0, center + Vec2::new(-40.0, 10.0));
        layout.apply_saved(&scene, std::slice::from_ref(&source));
        let refreshed = layout.place(&scene);
        assert_eq!(refreshed.eyes[0], center + Vec2::new(-40.0, 10.0));
    }

    #[test]
    fn an_older_save_coming_back_never_undoes_a_newer_move() {
        let mut ws = Fixture::new();
        project(&mut ws, "WEB", 4);
        let scene = Scene::build(&[ws.source()]);
        let mut layout = Layout::default();
        let placed = layout.place(&scene);
        let center = placed.clusters[0].0;
        let echo = |nodes: Vec<LayoutNode>| {
            let mut source = ws.source();
            source.workspaces[0].layout = nodes;
            source
        };

        // Two nudges, both saving; the first save's echo arrives between them.
        let first = layout.move_eye(&scene, &placed, 0, center + Vec2::new(16.0, 0.0));
        let second = layout.move_eye(&scene, &placed, 0, center + Vec2::new(32.0, 0.0));
        layout.apply_saved(&scene, &[echo(vec![first])]);
        assert_eq!(layout.place(&scene).eyes[0], center + Vec2::new(32.0, 0.0));

        // Once its own last save is back, other devices' moves land again.
        layout.apply_saved(&scene, &[echo(vec![second.clone()])]);
        let elsewhere = LayoutNode { x: -20.0, ..second };
        layout.apply_saved(&scene, &[echo(vec![elsewhere])]);
        assert_eq!(layout.place(&scene).eyes[0], center + Vec2::new(-20.0, 0.0));
    }

    #[test]
    fn re_layout_keeps_room_for_an_eye_pinned_near_the_edge_of_a_shrunk_web() {
        let mut ws = Fixture::new();
        project(&mut ws, "WEB", 40);
        let scene = Scene::build(&[ws.source()]);
        let mut layout = Layout::default();
        let placed = layout.place(&scene);
        let (center, r) = placed.clusters[0];
        let key = scene.eyes[0].key.clone();
        let out = Vec2::new(r * 0.75, 0.0);
        layout.move_eye(&scene, &placed, 0, center + out);

        // Most of the project is deleted, so a fresh web would be far smaller.
        ws.snapshots[0].tasks.truncate(3);
        let scene = Scene::build(&[ws.source()]);
        let (after, _) = layout.relayout(&scene);

        let eye = scene.eye(&key).unwrap();
        let on_web = after.eyes[eye] - after.clusters[0].0;
        assert!(
            (on_web - out).len() < 0.01,
            "pinned eye moved to {on_web:?}"
        );
    }

    #[test]
    fn re_layout_hands_back_the_saved_positions_it_moved() {
        let mut ws = Fixture::new();
        project(&mut ws, "WEB", 6);
        let scene = Scene::build(&[ws.source()]);
        let mut layout = Layout::default();
        let placed = layout.place(&scene);
        let center = placed.clusters[0].0;
        // Dragged, then unpinned with P: saved, but re-layout may move it.
        layout.move_eye(&scene, &placed, 2, center + Vec2::new(60.0, 60.0));
        let saved = layout.toggle_pin(&scene, 2).unwrap();
        let mut source = ws.source();
        source.workspaces[0].layout = vec![saved];
        layout.apply_saved(&scene, &[source]);

        let (after, moved) = layout.relayout(&scene);

        let [(key, node)] = moved.as_slice() else {
            panic!("expected one moved node, got {moved:?}");
        };
        assert_eq!(*key, scene.eyes[2].key);
        assert!(!node.pinned);
        let on_web = after.eyes[2] - after.clusters[0].0;
        let saved = Vec2::new(node.x as f32, node.y as f32);
        assert!(
            (saved - on_web).len() < 0.01,
            "saved {saved:?}, drawn {on_web:?}"
        );
        assert_ne!(on_web, Vec2::new(60.0, 60.0), "it did move");
    }

    #[test]
    fn a_pinned_web_stays_put_when_the_web_above_it_moves_closer() {
        let mut ws = Fixture::new();
        project(&mut ws, "API", 3);
        project(&mut ws, "WEB", 3);
        let scene = Scene::build(&[ws.source()]);
        let mut layout = Layout::default();
        let before = layout.place(&scene);
        let (lower, upper) = (before.clusters[1].0, before.clusters[0].0);
        layout.move_cluster(&scene, &before, 1, lower);

        // Nudged down, the upper web ends up closer than the usual gap.
        layout.move_cluster(&scene, &before, 0, upper + Vec2::new(0.0, 16.0));
        let after = layout.place(&scene);

        assert_eq!(after.clusters[1].0, lower, "the pinned web was pushed");
        assert_eq!(after.clusters[0].0, upper + Vec2::new(0.0, 16.0));
    }

    #[test]
    fn a_pinned_web_keeps_its_saved_spot_when_its_column_is_sized() {
        let mut ws = Fixture::new();
        project(&mut ws, "WEB", 3);
        let scene = Scene::build(&[ws.source()]);
        let mut source = ws.source();
        source.workspaces[0].layout = vec![LayoutNode {
            kind: NodeKind::Project,
            id: scene.clusters[0].key.id,
            x: 448.0,
            y: 600.0,
            pinned: true,
        }];
        let mut layout = Layout::default();
        layout.apply_saved(&scene, std::slice::from_ref(&source));
        layout.place(&scene);
        // What the app does once it has measured the names.
        layout.widen(&scene, 0, 900.0);
        let placed = layout.place(&scene);

        let (left, width) = placed.columns[0];
        let (center, r) = placed.clusters[0];
        assert_eq!(center, Vec2::new(left + 448.0, 600.0));
        assert!(
            448.0 + r <= width,
            "the column holds the web it was saved with"
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
