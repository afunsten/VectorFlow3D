//! Synthetic world generator for the Phase 1 scale gate (spec §6.5): index
//! ~10M asset refs with payloads unloaded, cold build < 30s, RSS bounded.
//!
//! These entities carry the same shape as imported ones (stable EntityId,
//! extents, a `GeomRef` to an unopened payload, a telemetry binding descriptor)
//! but are generated procedurally — no USD compose, no payload load, no live
//! values. This mirrors the "logical world is far larger than GPU memory"
//! envelope: the LSG is an index, not a geometry database.

use crate::lsg::{Aabb, Entity, EntityId, GeomRef, Lsg, TelemetryBinding, Transform};

/// Peak resident set size in bytes for this process, or `None` if unavailable.
///
/// `getrusage` reports `ru_maxrss` in bytes on macOS and in kilobytes on Linux.
pub fn max_rss_bytes() -> Option<u64> {
    // SAFETY: zeroed rusage is a valid initial value; getrusage fills it.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return None;
        }
        let maxrss = usage.ru_maxrss as u64;
        if cfg!(target_os = "macos") {
            Some(maxrss)
        } else {
            Some(maxrss.saturating_mul(1024))
        }
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{:.1} {}", v, UNITS[u])
}

/// Generate a synthetic LSG of `count` entities laid out on a coarse grid.
/// Payloads are referenced but never opened.
pub fn generate(count: usize) -> Lsg {
    let mut lsg = Lsg::new();

    // A handful of shared class templates so bindings/extents look realistic.
    let classes = ["pump", "tank", "distribution_switch"];
    let payloads = [
        "./components/pump.usda",
        "./components/tank.usda",
        "./components/distribution_switch.usda",
    ];
    let metrics = ["flow", "level", "loadCurrent"];

    // Grid spacing to spread extents across a large world volume.
    let side = (count as f64).cbrt().ceil() as usize;
    let side = side.max(1);

    for i in 0..count {
        let prim_path = format!("/Synth/e{i}");
        let id = EntityId::from_prim_path(&prim_path);
        let c = i % classes.len();
        let asset_tag = format!("E{i}");

        let x = (i % side) as f64 * 5.0;
        let y = ((i / side) % side) as f64 * 5.0;
        let z = (i / (side * side)) as f64 * 5.0;

        let entity = Entity {
            id,
            prim_path,
            parent: None,
            children: Vec::new(),
            kind: Some("component".to_string()),
            tags: Vec::new(),
            vf: Default::default(),
            transform_default: Transform::from_translation([x, y, z]),
            extents: Aabb {
                min: [x - 1.0, y - 1.0, z],
                max: [x + 1.0, y + 1.0, z + 3.0],
            },
            geom_ref: Some(GeomRef {
                payload_uri: payloads[c].to_string(),
                prim_path: "/Component".to_string(),
                content_hash: String::new(),
                lod_ladder: Vec::new(),
            }),
            bindings: vec![TelemetryBinding {
                attribute: metrics[c].to_string(),
                source_id: "victoriametrics".to_string(),
                // A convention-shaped query so the resolver can batch by metric
                // (only ~3 distinct metric names across the whole world).
                query: format!("{}{{asset=\"{asset_tag}\"}}", metrics[c]),
                unit: String::new(),
                ttl_ms: 5000.0,
                priority: "background".to_string(),
                quality_policy: "stale_ok".to_string(),
            }],
        };
        lsg.insert(entity);
    }

    lsg.link_hierarchy();
    lsg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_requested_count() {
        let lsg = generate(1000);
        assert_eq!(lsg.len(), 1000);
        assert_eq!(lsg.payload_count(), 1000);
        assert!(lsg.binding_count() >= 1000);
    }

    #[test]
    fn format_bytes_scales() {
        assert_eq!(format_bytes(512), "512.0 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
    }
}
