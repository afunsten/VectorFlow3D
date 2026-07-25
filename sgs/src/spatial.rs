//! Broad-phase spatial index over the Logical Scene Graph (spec §3.1/§3.2).
//!
//! The Interest Manager must narrow ~10M entities down to a small candidate set
//! per camera update WITHOUT a linear scan (gate §6.5: interest eval < 2 ms p99
//! for 50k candidates). This is a coarse uniform grid keyed by each entity's
//! extent centroid: a query region yields the entities in the overlapping
//! cells, and the caller applies a precise predicate test.
//!
//! It is derived state, rebuilt from the LSG — the LSG itself stays a pure
//! existence index ([`crate::lsg`]). Nothing here opens payloads or touches
//! telemetry.

use std::collections::HashMap;

use crate::lsg::{Aabb, EntityId, Lsg};

/// Default grid cell size (world units). Chosen to match the synthetic world's
/// 5-unit spacing so a tight AOI touches only a handful of cells.
pub const DEFAULT_CELL_SIZE: f64 = 8.0;

type Cell = (i64, i64, i64);

/// A uniform-grid broad-phase index. Entities are bucketed by the cell of their
/// extent centroid.
#[derive(Debug)]
pub struct SpatialIndex {
    cell_size: f64,
    cells: HashMap<Cell, Vec<EntityId>>,
}

impl SpatialIndex {
    /// Build the index over every entity in the LSG using [`DEFAULT_CELL_SIZE`].
    pub fn build(lsg: &Lsg) -> Self {
        Self::build_with_cell_size(lsg, DEFAULT_CELL_SIZE)
    }

    pub fn build_with_cell_size(lsg: &Lsg, cell_size: f64) -> Self {
        let cell_size = if cell_size > 0.0 { cell_size } else { DEFAULT_CELL_SIZE };
        let mut cells: HashMap<Cell, Vec<EntityId>> = HashMap::new();
        for e in lsg.entities() {
            let c = cell_of(centroid(&e.extents), cell_size);
            cells.entry(c).or_default().push(e.id);
        }
        SpatialIndex { cell_size, cells }
    }

    pub fn cell_size(&self) -> f64 {
        self.cell_size
    }

    /// Number of occupied cells (diagnostics).
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }

    /// Broad-phase: every entity whose centroid cell overlaps the query AABB.
    /// May include false positives just outside the box; the caller applies the
    /// precise test.
    pub fn candidates_in_aabb(&self, min: [f64; 3], max: [f64; 3]) -> Vec<EntityId> {
        let lo = cell_of(min, self.cell_size);
        let hi = cell_of(max, self.cell_size);
        let mut out = Vec::new();
        for cx in lo.0..=hi.0 {
            for cy in lo.1..=hi.1 {
                for cz in lo.2..=hi.2 {
                    if let Some(ids) = self.cells.get(&(cx, cy, cz)) {
                        out.extend_from_slice(ids);
                    }
                }
            }
        }
        out
    }

    /// Broad-phase candidates for a sphere (its enclosing AABB).
    pub fn candidates_for_sphere(&self, center: [f64; 3], radius: f64) -> Vec<EntityId> {
        let r = radius.max(0.0);
        self.candidates_in_aabb(
            [center[0] - r, center[1] - r, center[2] - r],
            [center[0] + r, center[1] + r, center[2] + r],
        )
    }

    /// Broad-phase candidates for a frustum (its precomputed enclosing AABB).
    pub fn candidates_for_frustum(&self, frustum: &Frustum) -> Vec<EntityId> {
        self.candidates_in_aabb(frustum.bounds.min, frustum.bounds.max)
    }
}

/// Centroid of an AABB.
pub fn centroid(a: &Aabb) -> [f64; 3] {
    [
        (a.min[0] + a.max[0]) * 0.5,
        (a.min[1] + a.max[1]) * 0.5,
        (a.min[2] + a.max[2]) * 0.5,
    ]
}

fn cell_of(p: [f64; 3], size: f64) -> Cell {
    (
        (p[0] / size).floor() as i64,
        (p[1] / size).floor() as i64,
        (p[2] / size).floor() as i64,
    )
}

/// Precise test: does a point lie within `radius` of `center`?
pub fn point_in_sphere(p: [f64; 3], center: [f64; 3], radius: f64) -> bool {
    let dx = p[0] - center[0];
    let dy = p[1] - center[1];
    let dz = p[2] - center[2];
    dx * dx + dy * dy + dz * dz <= radius * radius
}

/// Precise test: do two AABBs overlap (inclusive)?
pub fn aabb_overlaps(a: &Aabb, b_min: [f64; 3], b_max: [f64; 3]) -> bool {
    a.min[0] <= b_max[0]
        && a.max[0] >= b_min[0]
        && a.min[1] <= b_max[1]
        && a.max[1] >= b_min[1]
        && a.min[2] <= b_max[2]
        && a.max[2] >= b_min[2]
}

/// A view frustum expressed as six inward-facing half-space planes plus a
/// precomputed enclosing AABB for broad-phase. A plane `[a,b,c,d]` counts a
/// point `p` as inside when `a*px + b*py + c*pz + d >= 0`.
#[derive(Debug, Clone)]
pub struct Frustum {
    pub planes: [[f64; 4]; 6],
    pub bounds: Aabb,
}

impl Frustum {
    pub fn new(planes: [[f64; 4]; 6], bounds: Aabb) -> Self {
        Frustum { planes, bounds }
    }

    /// Conservative precise test: is any part of `extents` inside all planes?
    /// Uses the AABB "positive vertex" per plane (no false negatives).
    pub fn intersects_aabb(&self, extents: &Aabb) -> bool {
        for pl in &self.planes {
            // Pick the AABB corner farthest along the plane normal.
            let px = if pl[0] >= 0.0 { extents.max[0] } else { extents.min[0] };
            let py = if pl[1] >= 0.0 { extents.max[1] } else { extents.min[1] };
            let pz = if pl[2] >= 0.0 { extents.max[2] } else { extents.min[2] };
            if pl[0] * px + pl[1] * py + pl[2] * pz + pl[3] < 0.0 {
                return false; // fully outside this plane
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsg::{Entity, Transform};

    fn ent(path: &str, center: [f64; 3]) -> Entity {
        Entity {
            id: EntityId::from_prim_path(path),
            prim_path: path.to_string(),
            parent: None,
            children: vec![],
            kind: Some("component".to_string()),
            tags: vec![],
            vf: Default::default(),
            transform_default: Transform::from_translation(center),
            extents: Aabb {
                min: [center[0] - 0.5, center[1] - 0.5, center[2] - 0.5],
                max: [center[0] + 0.5, center[1] + 0.5, center[2] + 0.5],
            },
            geom_ref: None,
            bindings: vec![],
        }
    }

    fn lsg_with(points: &[[f64; 3]]) -> Lsg {
        let mut lsg = Lsg::new();
        for (i, p) in points.iter().enumerate() {
            lsg.insert(ent(&format!("/e{i}"), *p));
        }
        lsg.link_hierarchy();
        lsg
    }

    #[test]
    fn sphere_broad_phase_finds_near_and_skips_far() {
        let lsg = lsg_with(&[[0.0, 0.0, 0.0], [4.0, 0.0, 0.0], [100.0, 0.0, 0.0]]);
        let idx = SpatialIndex::build(&lsg);
        let cands = idx.candidates_for_sphere([0.0, 0.0, 0.0], 5.0);
        // The far entity at x=100 must not be a candidate.
        assert!(!cands.contains(&EntityId::from_prim_path("/e2")));
        assert!(cands.contains(&EntityId::from_prim_path("/e0")));
    }

    #[test]
    fn precise_sphere_and_aabb() {
        assert!(point_in_sphere([1.0, 0.0, 0.0], [0.0, 0.0, 0.0], 2.0));
        assert!(!point_in_sphere([3.0, 0.0, 0.0], [0.0, 0.0, 0.0], 2.0));
        let a = Aabb { min: [0.0; 3], max: [1.0; 3] };
        assert!(aabb_overlaps(&a, [0.5, 0.5, 0.5], [2.0, 2.0, 2.0]));
        assert!(!aabb_overlaps(&a, [2.0, 2.0, 2.0], [3.0, 3.0, 3.0]));
    }
}
