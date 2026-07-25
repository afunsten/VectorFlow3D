//! Twin Overlay: a small, durable SQLite store for runtime-authored opinions
//! that must not thrash the USD asset layers (spec §3.0 step 6, §4.5).
//!
//! Scope:
//! - **Pins** (Phase 1): committed transform overrides, stronger than the
//!   authored USD transform default.
//! - **DSL opinions** (Phase 4): the twin semantics the Flow3D DSL lowers to
//!   (bindings/tags/meta/anchors/edges), plus per-source **compile stamps** so
//!   a recompile can diff against the last run and apply a minimal patch (spec
//!   §3.9 / §4.5). Stored one [`Opinion`](crate::opinion::Opinion) per row,
//!   keyed by its stable [`key`](crate::opinion::Opinion::key); the `kind`
//!   column + JSON payload keep the single table self-describing rather than
//!   fanning out into five parallel tables.
//!
//! Precedence (spec §4.1, total order): authored USD default < telemetry-driven
//! override < committed pin/overlay opinion. Mutations go through one monotonic
//! revision counter (spec §4.6). USD files are never written.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

use crate::lsg::{Entity, EntityId, Transform};
use crate::opinion::{self, Opinion, OpinionDiff};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    k TEXT PRIMARY KEY,
    v TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS pins (
    entity_id      TEXT PRIMARY KEY,
    prim_path      TEXT,
    transform_json TEXT NOT NULL,
    pinned_by      TEXT,
    at_ms          INTEGER NOT NULL,
    revision       INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS dsl_opinions (
    source       TEXT NOT NULL,
    key          TEXT NOT NULL,
    kind         TEXT NOT NULL,
    entity_id    TEXT,
    hash         TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    revision     INTEGER NOT NULL,
    PRIMARY KEY (source, key)
);
CREATE TABLE IF NOT EXISTS compile_stamps (
    source        TEXT PRIMARY KEY,
    source_hash   TEXT NOT NULL,
    opinion_count INTEGER NOT NULL,
    revision      INTEGER NOT NULL,
    at_ms         INTEGER NOT NULL
);
"#;

/// A record of the last DSL compile for a given source file (spec §4.5).
#[derive(Debug, Clone)]
pub struct CompileStamp {
    pub source_hash: String,
    pub opinion_count: usize,
    pub revision: u64,
    pub at_ms: i64,
}

/// A committed transform pin.
#[derive(Debug, Clone)]
pub struct Pin {
    pub transform: Transform,
    pub pinned_by: Option<String>,
    pub at_ms: i64,
    pub revision: u64,
}

pub struct TwinOverlay {
    conn: Connection,
}

impl TwinOverlay {
    /// Open (creating if needed) the overlay database at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening Twin Overlay at {}", path.display()))?;
        conn.execute_batch(SCHEMA)
            .context("initializing Twin Overlay schema")?;
        let overlay = TwinOverlay { conn };
        overlay.ensure_meta()?;
        Ok(overlay)
    }

    /// In-memory overlay (ephemeral; useful for tests and dry runs).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        let overlay = TwinOverlay { conn };
        overlay.ensure_meta()?;
        Ok(overlay)
    }

    fn ensure_meta(&self) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO meta (k, v) VALUES ('scene_revision', '0')",
            [],
        )?;
        Ok(())
    }

    /// Current scene revision (bumped on every committed mutation).
    pub fn revision(&self) -> Result<u64> {
        let v: String = self
            .conn
            .query_row("SELECT v FROM meta WHERE k = 'scene_revision'", [], |r| {
                r.get(0)
            })
            .context("reading scene_revision")?;
        Ok(v.parse().unwrap_or(0))
    }

    fn bump_revision(conn: &Connection) -> Result<u64> {
        let cur: String = conn.query_row(
            "SELECT v FROM meta WHERE k = 'scene_revision'",
            [],
            |r| r.get(0),
        )?;
        let next = cur.parse::<u64>().unwrap_or(0) + 1;
        conn.execute(
            "UPDATE meta SET v = ?1 WHERE k = 'scene_revision'",
            [next.to_string()],
        )?;
        Ok(next)
    }

    /// Commit a transform pin for `entity_id`. Returns the new revision.
    pub fn pin(
        &mut self,
        entity_id: EntityId,
        prim_path: &str,
        transform: Transform,
        pinned_by: Option<&str>,
    ) -> Result<u64> {
        let tx = self.conn.transaction()?;
        let revision = Self::bump_revision(&tx)?;
        let at_ms = now_ms();
        let transform_json = serde_json::to_string(&transform)?;
        tx.execute(
            "INSERT INTO pins (entity_id, prim_path, transform_json, pinned_by, at_ms, revision)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(entity_id) DO UPDATE SET
                prim_path = excluded.prim_path,
                transform_json = excluded.transform_json,
                pinned_by = excluded.pinned_by,
                at_ms = excluded.at_ms,
                revision = excluded.revision",
            rusqlite::params![
                entity_id.as_hex(),
                prim_path,
                transform_json,
                pinned_by,
                at_ms,
                revision,
            ],
        )?;
        tx.commit()?;
        Ok(revision)
    }

    /// Remove a pin. Returns `(new_revision, existed)`.
    pub fn unpin(&mut self, entity_id: EntityId) -> Result<(u64, bool)> {
        let tx = self.conn.transaction()?;
        let revision = Self::bump_revision(&tx)?;
        let n = tx.execute(
            "DELETE FROM pins WHERE entity_id = ?1",
            [entity_id.as_hex()],
        )?;
        tx.commit()?;
        Ok((revision, n > 0))
    }

    /// Read a pin for `entity_id`, if any.
    pub fn get_pin(&self, entity_id: EntityId) -> Result<Option<Pin>> {
        let row = self
            .conn
            .query_row(
                "SELECT transform_json, pinned_by, at_ms, revision FROM pins WHERE entity_id = ?1",
                [entity_id.as_hex()],
                |r| {
                    let transform_json: String = r.get(0)?;
                    let pinned_by: Option<String> = r.get(1)?;
                    let at_ms: i64 = r.get(2)?;
                    let revision: i64 = r.get(3)?;
                    Ok((transform_json, pinned_by, at_ms, revision))
                },
            )
            .optional()
            .context("reading pin")?;

        match row {
            Some((transform_json, pinned_by, at_ms, revision)) => {
                let transform: Transform = serde_json::from_str(&transform_json)?;
                Ok(Some(Pin {
                    transform,
                    pinned_by,
                    at_ms,
                    revision: revision as u64,
                }))
            }
            None => Ok(None),
        }
    }

    pub fn pin_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM pins", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    /// Resolve the effective transform for an entity: the pinned override if
    /// present, otherwise the authored USD default (spec §4.1 precedence).
    pub fn resolved_transform(&self, entity: &Entity) -> Result<Transform> {
        Ok(match self.get_pin(entity.id)? {
            Some(pin) => pin.transform,
            None => entity.transform_default,
        })
    }

    // ---- DSL opinions (Phase 4, spec §3.9 / §4.5) ---------------------------

    /// Previously-persisted opinion hashes for `source` (stable key → hash),
    /// the input to the incremental reload diff.
    pub fn opinion_hashes(&self, source: &str) -> Result<HashMap<String, String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT key, hash FROM dsl_opinions WHERE source = ?1")?;
        let rows = stmt.query_map([source], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (k, h) = row?;
            out.insert(k, h);
        }
        Ok(out)
    }

    /// Load all persisted opinions for `source`, deserialized (used to re-seed
    /// the LSG on startup without recompiling the DSL text).
    pub fn load_opinions(&self, source: &str) -> Result<Vec<Opinion>> {
        let mut stmt = self
            .conn
            .prepare("SELECT payload_json FROM dsl_opinions WHERE source = ?1 ORDER BY key")?;
        let rows = stmt.query_map([source], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            let json = row?;
            let op: Opinion = serde_json::from_str(&json)
                .context("deserializing a stored DSL opinion")?;
            out.push(op);
        }
        Ok(out)
    }

    pub fn compile_stamp(&self, source: &str) -> Result<Option<CompileStamp>> {
        self.conn
            .query_row(
                "SELECT source_hash, opinion_count, revision, at_ms
                   FROM compile_stamps WHERE source = ?1",
                [source],
                |r| {
                    Ok(CompileStamp {
                        source_hash: r.get(0)?,
                        opinion_count: r.get::<_, i64>(1)? as usize,
                        revision: r.get::<_, i64>(2)? as u64,
                        at_ms: r.get(3)?,
                    })
                },
            )
            .optional()
            .context("reading compile stamp")
    }

    /// Persist a freshly-compiled opinion set for `source` as a **minimal
    /// patch**: only added/changed rows are written and removed rows deleted,
    /// leaving unchanged opinions (and their revision) untouched. Bumps the
    /// scene revision once and records a compile stamp. Returns the diff so the
    /// caller can apply the same minimal patch to the in-memory LSG.
    pub fn apply_opinions(
        &mut self,
        source: &str,
        source_hash: &str,
        opinions: &[Opinion],
    ) -> Result<(u64, OpinionDiff)> {
        let prev = self.opinion_hashes(source)?;
        let diff = opinion::diff(opinions, &prev);

        // Index the new set by key for row upserts.
        let by_key: HashMap<String, &Opinion> =
            opinions.iter().map(|o| (o.key(), o)).collect();

        let tx = self.conn.transaction()?;
        let revision = Self::bump_revision(&tx)?;
        let at_ms = now_ms();

        let upsert = |op: &Opinion| -> Result<()> {
            let payload_json = serde_json::to_string(op)?;
            tx.execute(
                "INSERT INTO dsl_opinions (source, key, kind, entity_id, hash, payload_json, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(source, key) DO UPDATE SET
                    kind = excluded.kind,
                    entity_id = excluded.entity_id,
                    hash = excluded.hash,
                    payload_json = excluded.payload_json,
                    revision = excluded.revision",
                rusqlite::params![
                    source,
                    op.key(),
                    op.kind(),
                    op.primary_entity().as_hex(),
                    op.content_hash(),
                    payload_json,
                    revision,
                ],
            )?;
            Ok(())
        };

        for key in diff.added.iter().chain(diff.changed.iter()) {
            if let Some(op) = by_key.get(key) {
                upsert(op)?;
            }
        }
        for key in &diff.removed {
            tx.execute(
                "DELETE FROM dsl_opinions WHERE source = ?1 AND key = ?2",
                rusqlite::params![source, key],
            )?;
        }

        tx.execute(
            "INSERT INTO compile_stamps (source, source_hash, opinion_count, revision, at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(source) DO UPDATE SET
                source_hash = excluded.source_hash,
                opinion_count = excluded.opinion_count,
                revision = excluded.revision,
                at_ms = excluded.at_ms",
            rusqlite::params![source, source_hash, opinions.len() as i64, revision, at_ms],
        )?;

        tx.commit()?;
        Ok((revision, diff))
    }

    /// Count of persisted opinions for a source (diagnostics).
    pub fn opinion_count(&self, source: &str) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM dsl_opinions WHERE source = ?1",
            [source],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsg::{Aabb, Entity};

    fn entity(path: &str, t: [f64; 3]) -> Entity {
        Entity {
            id: EntityId::from_prim_path(path),
            prim_path: path.to_string(),
            parent: None,
            children: vec![],
            kind: None,
            tags: vec![],
            vf: Default::default(),
            transform_default: Transform::from_translation(t),
            extents: Aabb::zero(),
            geom_ref: None,
            bindings: vec![],
        }
    }

    #[test]
    fn pin_precedence_and_revision() {
        let mut ov = TwinOverlay::open_in_memory().unwrap();
        let e = entity("/Root/Pump_01", [0.0, 0.0, 0.0]);
        assert_eq!(ov.revision().unwrap(), 0);

        // No pin -> resolves to authored default.
        assert_eq!(ov.resolved_transform(&e).unwrap().translation(), [0.0; 3]);

        let r1 = ov
            .pin(e.id, &e.prim_path, Transform::from_translation([9.0, 8.0, 7.0]), Some("adam"))
            .unwrap();
        assert_eq!(r1, 1);
        // Pin wins over authored default.
        assert_eq!(
            ov.resolved_transform(&e).unwrap().translation(),
            [9.0, 8.0, 7.0]
        );

        let (r2, existed) = ov.unpin(e.id).unwrap();
        assert!(existed);
        assert_eq!(r2, 2);
        // Back to authored default; revision is monotonic.
        assert_eq!(ov.resolved_transform(&e).unwrap().translation(), [0.0; 3]);
        assert!(ov.revision().unwrap() >= r2);
    }

    fn binding(entity: EntityId, attr: &str, query: &str) -> Opinion {
        Opinion::Binding {
            entity,
            binding: crate::lsg::TelemetryBinding {
                attribute: attr.to_string(),
                source_id: "victoriametrics".into(),
                query: query.to_string(),
                unit: String::new(),
                ttl_ms: 5000.0,
                priority: "background".into(),
                quality_policy: "stale_ok".into(),
            },
        }
    }

    #[test]
    fn opinions_persist_and_reload_is_a_minimal_patch() {
        let mut ov = TwinOverlay::open_in_memory().unwrap();
        let e = EntityId::from_prim_path("/PS/Pump_01");
        let src = "pump.flow3d";

        let v1 = vec![
            binding(e, "flow", "q1"),
            Opinion::Tag { entity: e, tag: "duty".into() },
        ];
        let (_rev1, d1) = ov.apply_opinions(src, "hash1", &v1).unwrap();
        assert_eq!(d1.added.len(), 2);
        assert_eq!(ov.opinion_count(src).unwrap(), 2);

        // Reload identical source: zero touched, rows unchanged.
        let (_rev2, d2) = ov.apply_opinions(src, "hash1", &v1).unwrap();
        assert!(d2.is_empty(), "{d2:?}");
        assert_eq!(d2.touched(), 0);

        // Change one binding + drop the tag: exactly one changed, one removed.
        let v3 = vec![binding(e, "flow", "q2")];
        let (_rev3, d3) = ov.apply_opinions(src, "hash3", &v3).unwrap();
        assert_eq!(d3.changed.len(), 1, "{d3:?}");
        assert_eq!(d3.removed.len(), 1, "{d3:?}");
        assert_eq!(ov.opinion_count(src).unwrap(), 1);

        // Reloaded opinions round-trip.
        let loaded = ov.load_opinions(src).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(matches!(&loaded[0], Opinion::Binding { binding, .. } if binding.query == "q2"));

        let stamp = ov.compile_stamp(src).unwrap().unwrap();
        assert_eq!(stamp.source_hash, "hash3");
        assert_eq!(stamp.opinion_count, 1);
    }
}
