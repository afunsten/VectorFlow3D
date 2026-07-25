//! USD import path: turn an OpenUSD root layer into an LSG projection.
//!
//! USD is an *initialization* dependency only (spec §3.0), so composition lives
//! in a small out-of-process Python helper (`tools/usd_export.py`) built on the
//! `usd-core` wheel. The helper opens the stage with payloads unloaded and
//! streams one NDJSON record per prim; this module ingests that stream. It can
//! also read a pre-exported NDJSON dump directly (`--from-json`) so the Rust
//! side is testable without a Python environment.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::lsg::{Aabb, Entity, EntityId, GeomRef, Lsg, TelemetryBinding, Transform};

/// Wire record emitted by the helper (one JSON object per line).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrimRecord {
    prim_path: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    transform: Option<TransformRecord>,
    /// `[[minx,miny,minz],[maxx,maxy,maxz]]` when authored.
    #[serde(default)]
    extents_hint: Option<[[f64; 3]; 2]>,
    #[serde(default)]
    vf: std::collections::HashMap<String, serde_json::Value>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    geom_ref: Option<GeomRefRecord>,
    #[serde(default)]
    bindings: Vec<BindingRecord>,
}

#[derive(Debug, Deserialize)]
struct TransformRecord {
    #[serde(default)]
    translate: Option<[f64; 3]>,
    /// Row-major 4x4 if the helper resolved a full local transform.
    #[serde(default)]
    matrix: Option<[f64; 16]>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeomRefRecord {
    payload_uri: String,
    prim_path: String,
    #[serde(default)]
    content_hash: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BindingRecord {
    attribute: String,
    #[serde(default)]
    source_id: String,
    #[serde(default)]
    query: String,
    #[serde(default)]
    unit: String,
    #[serde(default)]
    ttl_ms: f64,
    #[serde(default)]
    priority: String,
    #[serde(default)]
    quality_policy: String,
}

impl PrimRecord {
    fn into_entity(self) -> Entity {
        let id = EntityId::from_prim_path(&self.prim_path);
        let parent = self.parent.as_deref().map(EntityId::from_prim_path);

        let transform_default = match self.transform {
            Some(TransformRecord {
                matrix: Some(m), ..
            }) => Transform { matrix: m },
            Some(TransformRecord {
                translate: Some(t), ..
            }) => Transform::from_translation(t),
            _ => Transform::identity(),
        };

        let extents = match self.extents_hint {
            Some([min, max]) => Aabb { min, max },
            None => Aabb::zero(),
        };

        // tags: prefer explicit field, else pull from vf.tags[].
        let mut tags = self.tags;
        if tags.is_empty() {
            if let Some(arr) = self.vf.get("tags").and_then(|v| v.as_array()) {
                tags = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect();
            }
        }

        let geom_ref = self.geom_ref.map(|g| GeomRef {
            payload_uri: g.payload_uri,
            prim_path: g.prim_path,
            content_hash: g.content_hash,
            lod_ladder: Vec::new(),
        });

        let bindings = self
            .bindings
            .into_iter()
            .map(|b| TelemetryBinding {
                attribute: b.attribute,
                source_id: b.source_id,
                query: b.query,
                unit: b.unit,
                ttl_ms: b.ttl_ms,
                priority: b.priority,
                quality_policy: b.quality_policy,
            })
            .collect();

        Entity {
            id,
            prim_path: self.prim_path,
            parent,
            children: Vec::new(),
            kind: self.kind,
            tags,
            vf: self.vf,
            transform_default,
            extents,
            geom_ref,
            bindings,
        }
    }
}

/// Build an LSG from an NDJSON stream (one prim record per line). Blank lines
/// and lines beginning with `#` are ignored.
pub fn build_from_ndjson<R: Read>(reader: R) -> Result<Lsg> {
    let mut lsg = Lsg::new();
    let buf = BufReader::new(reader);
    for (i, line) in buf.lines().enumerate() {
        let line = line.with_context(|| format!("reading NDJSON line {}", i + 1))?;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let rec: PrimRecord = serde_json::from_str(trimmed)
            .with_context(|| format!("parsing NDJSON line {}: {}", i + 1, trimmed))?;
        lsg.insert(rec.into_entity());
    }
    lsg.link_hierarchy();
    Ok(lsg)
}

/// Locate the Python interpreter for the helper: explicit override, then the
/// project venv, then bare `python3`.
pub fn resolve_python(explicit: Option<&str>, tools_dir: &Path) -> PathBuf {
    if let Some(p) = explicit {
        return PathBuf::from(p);
    }
    if let Ok(env) = std::env::var("VF_USD_PYTHON") {
        if !env.is_empty() {
            return PathBuf::from(env);
        }
    }
    let venv = tools_dir.join(".venv/bin/python3");
    if venv.exists() {
        return venv;
    }
    PathBuf::from("python3")
}

/// Directory containing `usd_export.py`, relative to this crate.
pub fn tools_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tools")
}

/// Run the USD helper over `usd_root` and build the LSG from its NDJSON output.
pub fn import_from_usd(usd_root: &Path, python: Option<&str>) -> Result<Lsg> {
    let tools = tools_dir();
    let script = tools.join("usd_export.py");
    anyhow::ensure!(
        script.exists(),
        "USD helper not found at {} (did you scaffold tools/?)",
        script.display()
    );
    let py = resolve_python(python, &tools);

    let mut child = Command::new(&py)
        .arg(&script)
        .arg(usd_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| {
            format!(
                "failed to launch USD helper: {} {} {} (is usd-core installed in the venv?)",
                py.display(),
                script.display(),
                usd_root.display()
            )
        })?;

    let stdout = child.stdout.take().expect("piped stdout");
    let lsg = build_from_ndjson(stdout)?;

    let status = child.wait().context("waiting for USD helper")?;
    anyhow::ensure!(status.success(), "USD helper exited with {}", status);
    Ok(lsg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingests_ndjson_and_links_hierarchy() {
        let nd = r#"
{"primPath":"/Root","kind":"assembly","vf":{"class":"facility"}}
{"primPath":"/Root/Hall","kind":"group","parent":"/Root"}
{"primPath":"/Root/Hall/Pump_01","kind":"component","parent":"/Root/Hall","transform":{"translate":[0,3,0]},"extentsHint":[[-1.6,-0.7,-0.7],[0.9,0.7,0.6]],"vf":{"assetTag":"PUMP-01","tags":["duty"]},"geomRef":{"payloadUri":"./components/pump.usda","primPath":"/Pump","contentHash":"abc"},"bindings":[{"attribute":"flow","sourceId":"victoriametrics","query":"pump_flow_gpm{asset=\"PUMP-01\"}","unit":"gpm","ttlMs":5000,"priority":"background","qualityPolicy":"stale_ok"}]}
"#;
        let lsg = build_from_ndjson(nd.as_bytes()).unwrap();
        assert_eq!(lsg.len(), 3);
        let pump = lsg.by_asset_tag("PUMP-01").expect("pump indexed by tag");
        assert_eq!(pump.transform_default.translation(), [0.0, 3.0, 0.0]);
        assert_eq!(pump.bindings.len(), 1);
        assert_eq!(pump.tags, vec!["duty".to_string()]);
        assert!(pump.geom_ref.is_some());

        let root = lsg.by_path("/Root").unwrap();
        assert_eq!(root.children.len(), 1);
        assert_eq!(lsg.entities_binding("flow").len(), 1);
    }
}
