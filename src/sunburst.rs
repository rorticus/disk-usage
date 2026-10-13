//! Radial "sunburst" layout, painting and hit-testing.
//!
//! The focused folder sits in the centre; each ring outward is one level
//! deeper. Angle 0 is 12 o'clock and angles grow clockwise.

use std::f32::consts::TAU;

use eframe::egui::{self, ecolor::Hsva, Color32, Mesh, Pos2, Rect, Vec2};

use crate::model::{Kind, NodeId, Tree};

pub const LEVELS: usize = 7;
/// Children narrower than this many radians are merged into "smaller objects".
const MIN_ANGLE: f32 = 0.6_f32 * TAU / 360.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Node(NodeId),
    /// The children of `parent` from index `first` onward, merged together.
    Smaller { parent: NodeId, first: usize },
}

pub struct Segment {
    pub target: Target,
    pub depth: usize,
    pub a0: f32,
    pub a1: f32,
    pub color: Color32,
}

pub struct Geometry {
    pub center: Pos2,
    pub core: f32,
    /// Ring `i` spans `radii[i]..radii[i + 1]`.
    pub radii: [f32; LEVELS + 1],
}

pub enum Hit {
    Center,
    Segment(usize),
}

impl Geometry {
    pub fn new(rect: Rect) -> Self {
        let outer = (rect.width().min(rect.height()) / 2.0 - 12.0).max(40.0);
        let core = outer * 0.22;
        let weights: Vec<f32> = (0..LEVELS).map(|i| 0.86_f32.powi(i as i32)).collect();
        let total: f32 = weights.iter().sum();
        let mut radii = [core; LEVELS + 1];
        for i in 0..LEVELS {
            radii[i + 1] = radii[i] + (outer - core) * weights[i] / total;
        }
        Self { center: rect.center(), core, radii }
    }

    pub fn point(&self, angle: f32, r: f32) -> Pos2 {
        self.center + Vec2::new(angle.sin(), -angle.cos()) * r
    }

    pub fn hit(&self, segments: &[Segment], pos: Pos2) -> Option<Hit> {
        let d = pos - self.center;
        let r = d.length();
        if r <= self.core {
            return Some(Hit::Center);
        }
        let depth = (0..LEVELS).find(|&i| r >= self.radii[i] && r < self.radii[i + 1])?;
        let a = d.x.atan2(-d.y).rem_euclid(TAU);
        segments
            .iter()
            .position(|s| s.depth == depth && a >= s.a0 && a < s.a1)
            .map(Hit::Segment)
    }
}

fn color_for(kind: Kind, depth: usize, a0: f32, a1: f32) -> Color32 {
    let hue = ((a0 + a1) / 2.0 / TAU).fract();
    let d = depth as f32;
    let (s, v) = match kind {
        Kind::Dir => (0.78 - 0.07 * d, 0.80 + 0.025 * d),
        Kind::File => (0.30 - 0.025 * d, 0.78 + 0.025 * d),
        Kind::Restricted => return Color32::from_gray(90),
        Kind::Hidden => return Color32::from_gray(70),
    };
    Hsva::new(hue, s.max(0.12), v.min(1.0), 1.0).into()
}

pub fn smaller_color(depth: usize) -> Color32 {
    Color32::from_gray(120 + (depth as u8) * 8)
}

pub fn layout(tree: &Tree, focus: NodeId) -> Vec<Segment> {
    let mut out = Vec::new();
    lay(tree, focus, 0, 0.0, TAU, &mut out);
    out
}

fn lay(tree: &Tree, parent: NodeId, depth: usize, a0: f32, a1: f32, out: &mut Vec<Segment>) {
    let p = tree.node(parent);
    if depth >= LEVELS || p.size == 0 {
        return;
    }
    let scale = (a1 - a0) / p.size as f32;
    let mut a = a0;
    for (i, &c) in p.children.iter().enumerate() {
        let child = tree.node(c);
        let span = child.size as f32 * scale;
        if span < MIN_ANGLE {
            // Children are sorted by size, so everything from here on is small.
            // Zero-sized leftovers still get a (zero-width) segment so the
            // list can offer them.
            out.push(Segment {
                target: Target::Smaller { parent, first: i },
                depth,
                a0: a,
                a1,
                color: smaller_color(depth),
            });
            break;
        }
        out.push(Segment {
            target: Target::Node(c),
            depth,
            a0: a,
            a1: a + span,
            color: color_for(child.kind, depth, a, a + span),
        });
        if child.kind == Kind::Dir {
            lay(tree, c, depth + 1, a, a + span, out);
        }
        a += span;
    }
}

fn add_sector(mesh: &mut Mesh, g: &Geometry, r0: f32, r1: f32, a0: f32, a1: f32, color: Color32) {
    let steps = ((a1 - a0) / 0.025).ceil().max(1.0) as u32;
    let base = mesh.vertices.len() as u32;
    for i in 0..=steps {
        let a = a0 + (a1 - a0) * i as f32 / steps as f32;
        mesh.colored_vertex(g.point(a, r0), color);
        mesh.colored_vertex(g.point(a, r1), color);
    }
    for i in 0..steps {
        let k = base + 2 * i;
        mesh.add_triangle(k, k + 1, k + 2);
        mesh.add_triangle(k + 1, k + 3, k + 2);
    }
}

fn lighten(c: Color32, t: f32) -> Color32 {
    let l = |x: u8| (x as f32 + (255.0 - x as f32) * t) as u8;
    Color32::from_rgb(l(c.r()), l(c.g()), l(c.b()))
}

pub fn paint(
    painter: &egui::Painter,
    g: &Geometry,
    tree: &Tree,
    segments: &[Segment],
    highlight: Option<Target>,
) {
    let mut mesh = Mesh::default();
    for s in segments {
        let (r0, r1) = (g.radii[s.depth], g.radii[s.depth + 1]);
        // Leave a hairline gap between neighbouring segments.
        let gap = 0.8 / r1;
        let (a0, a1) = if s.a1 - s.a0 > 2.5 * gap { (s.a0 + gap, s.a1 - gap) } else { (s.a0, s.a1) };
        let lit = match (highlight, s.target) {
            (Some(Target::Node(h)), Target::Node(n)) => tree.contains(h, n),
            (Some(Target::Node(h)), Target::Smaller { parent, .. }) => tree.contains(h, parent),
            (Some(h), t) => h == t,
            (None, _) => false,
        };
        let color = if lit { lighten(s.color, 0.35) } else { s.color };
        add_sector(&mut mesh, g, r0 + 0.8, r1 - 0.8, a0, a1, color);
    }
    painter.add(mesh);
}
