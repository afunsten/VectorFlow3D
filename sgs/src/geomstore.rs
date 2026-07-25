//! VF geometry store (spec §3.1 / §4.2, Phase 5.6): the content-addressed store
//! that realizes the *"(future) VF geometry store"* named on [`GeomRef`](crate::lsg::GeomRef).
//!
//! Authored USD geometry is triangulated **out-of-process** by the same
//! `usd_export.py` helper (a `--mesh` pass) so OpenUSD stays off the Rust build
//! and off the runtime hot path — the same rule the Phase 1 import and the
//! Phase 2 payload hydration already obey. Each mesh is encoded as **glTF 2.0 /
//! GLB** (Khronos open standard; engine-neutral so the Phase 6 O3DE bridge and
//! any future Unreal/WebGPU tier reuse it) with a small focused writer/reader —
//! no heavyweight glTF engine dependency.
//!
//! The store is keyed by [`GeomRef::content_hash`]: identical payloads
//! tessellate and encode **once** (mirroring the refcounted payload cache in
//! [`crate::hydrate`]). It is READ-ONLY and derived — never written back to USD,
//! the LSG, or the Twin Overlay, and fully rebuildable from USD. Synthetic /
//! NDJSON worlds have no on-disk mesh assets, so their loader yields no mesh and
//! the observer keeps the AABB proxy box (spec §3.6 WebGPU tier LOD-0 fallback).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::import;
use crate::lsg::{GeomRef, Lsg};

/// A triangulated, indexed mesh in the target component prim's local space.
/// Positions/normals are parallel per-vertex arrays; `indices` are triangle
/// list indices into them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Mesh {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub indices: Vec<u32>,
}

impl Mesh {
    pub fn vertex_count(&self) -> usize {
        self.positions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.positions.is_empty() || self.indices.is_empty()
    }
}

/// Wire record emitted by `usd_export.py --mesh` (one JSON object).
#[derive(Debug, Deserialize)]
struct MeshRecord {
    #[serde(default)]
    points: Vec<[f32; 3]>,
    #[serde(default)]
    normals: Vec<[f32; 3]>,
    #[serde(default)]
    indices: Vec<u32>,
}

impl From<MeshRecord> for Mesh {
    fn from(r: MeshRecord) -> Self {
        Mesh {
            positions: r.points,
            normals: r.normals,
            indices: r.indices,
        }
    }
}

// ---- GLB 2.0 (binary glTF) reader/writer ------------------------------------

const GLB_MAGIC: u32 = 0x4654_6C67; // "glTF"
const GLB_VERSION: u32 = 2;
const CHUNK_JSON: u32 = 0x4E4F_534A; // "JSON"
const CHUNK_BIN: u32 = 0x004E_4942; // "BIN\0"

const COMPONENT_FLOAT: u32 = 5126;
const COMPONENT_UINT: u32 = 5125;
const TARGET_ARRAY_BUFFER: u32 = 34962;
const TARGET_ELEMENT_ARRAY_BUFFER: u32 = 34963;

fn pad_to_4(v: &mut Vec<u8>, fill: u8) {
    while !v.len().is_multiple_of(4) {
        v.push(fill);
    }
}

/// Encode a [`Mesh`] as a self-contained GLB (single buffer, single mesh /
/// primitive with POSITION + NORMAL + indices). Deterministic given the mesh.
pub fn encode_glb(mesh: &Mesh) -> Vec<u8> {
    let nverts = mesh.positions.len();
    let nidx = mesh.indices.len();

    // Binary buffer: positions (VEC3 f32), normals (VEC3 f32), indices (u32).
    let pos_bytes = nverts * 12;
    let nrm_bytes = nverts * 12;
    let idx_bytes = nidx * 4;
    let mut bin: Vec<u8> = Vec::with_capacity(pos_bytes + nrm_bytes + idx_bytes);
    for p in &mesh.positions {
        for c in p {
            bin.extend_from_slice(&c.to_le_bytes());
        }
    }
    for n in &mesh.normals {
        for c in n {
            bin.extend_from_slice(&c.to_le_bytes());
        }
    }
    for i in &mesh.indices {
        bin.extend_from_slice(&i.to_le_bytes());
    }
    pad_to_4(&mut bin, 0);

    // POSITION accessors require min/max bounds (glTF 2.0 spec).
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for p in &mesh.positions {
        for k in 0..3 {
            min[k] = min[k].min(p[k]);
            max[k] = max[k].max(p[k]);
        }
    }
    if nverts == 0 {
        min = [0.0; 3];
        max = [0.0; 3];
    }

    let json = serde_json::json!({
        "asset": { "version": "2.0", "generator": "vectorflow-sgs geomstore" },
        "scene": 0,
        "scenes": [ { "nodes": [0] } ],
        "nodes": [ { "mesh": 0 } ],
        "meshes": [ {
            "primitives": [ {
                "attributes": { "POSITION": 0, "NORMAL": 1 },
                "indices": 2,
                "mode": 4
            } ]
        } ],
        "buffers": [ { "byteLength": bin.len() } ],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": pos_bytes, "target": TARGET_ARRAY_BUFFER },
            { "buffer": 0, "byteOffset": pos_bytes, "byteLength": nrm_bytes, "target": TARGET_ARRAY_BUFFER },
            { "buffer": 0, "byteOffset": pos_bytes + nrm_bytes, "byteLength": idx_bytes, "target": TARGET_ELEMENT_ARRAY_BUFFER }
        ],
        "accessors": [
            { "bufferView": 0, "componentType": COMPONENT_FLOAT, "count": nverts, "type": "VEC3", "min": min, "max": max },
            { "bufferView": 1, "componentType": COMPONENT_FLOAT, "count": nverts, "type": "VEC3" },
            { "bufferView": 2, "componentType": COMPONENT_UINT, "count": nidx, "type": "SCALAR" }
        ]
    });

    let mut json_bytes = serde_json::to_vec(&json).expect("serialize glTF JSON");
    pad_to_4(&mut json_bytes, b' ');

    let total = 12 + 8 + json_bytes.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&GLB_MAGIC.to_le_bytes());
    out.extend_from_slice(&GLB_VERSION.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    // JSON chunk.
    out.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&CHUNK_JSON.to_le_bytes());
    out.extend_from_slice(&json_bytes);
    // BIN chunk.
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(&CHUNK_BIN.to_le_bytes());
    out.extend_from_slice(&bin);
    out
}

fn read_u32(bytes: &[u8], off: usize) -> Result<u32> {
    anyhow::ensure!(off + 4 <= bytes.len(), "GLB truncated at offset {off}");
    Ok(u32::from_le_bytes([
        bytes[off],
        bytes[off + 1],
        bytes[off + 2],
        bytes[off + 3],
    ]))
}

/// Decode a GLB produced by [`encode_glb`] back into a [`Mesh`]. Minimal: it
/// reads the first mesh primitive's POSITION / NORMAL / indices accessors (the
/// shape this store writes), enough for the round-trip test and any consumer.
pub fn decode_glb(bytes: &[u8]) -> Result<Mesh> {
    anyhow::ensure!(bytes.len() >= 12, "GLB too short");
    anyhow::ensure!(read_u32(bytes, 0)? == GLB_MAGIC, "not a GLB (bad magic)");

    // Walk chunks after the 12-byte header.
    let mut off = 12;
    let mut json: Option<&[u8]> = None;
    let mut bin: Option<&[u8]> = None;
    while off + 8 <= bytes.len() {
        let len = read_u32(bytes, off)? as usize;
        let kind = read_u32(bytes, off + 4)?;
        let start = off + 8;
        anyhow::ensure!(start + len <= bytes.len(), "GLB chunk overruns buffer");
        let data = &bytes[start..start + len];
        match kind {
            CHUNK_JSON => json = Some(data),
            CHUNK_BIN => bin = Some(data),
            _ => {}
        }
        off = start + len;
    }
    let json = json.context("GLB missing JSON chunk")?;
    let bin = bin.unwrap_or(&[]);
    let doc: serde_json::Value =
        serde_json::from_slice(json).context("parsing GLB JSON chunk")?;

    let accessors = doc["accessors"].as_array().context("no accessors")?;
    let views = doc["bufferViews"].as_array().context("no bufferViews")?;
    let prim = &doc["meshes"][0]["primitives"][0];
    let pos_idx = prim["attributes"]["POSITION"].as_u64().context("no POSITION")? as usize;
    let nrm_idx = prim["attributes"]["NORMAL"].as_u64();
    let idx_acc = prim["indices"].as_u64().context("no indices")? as usize;

    let read_vec3 = |acc_idx: usize| -> Result<Vec<[f32; 3]>> {
        let acc = &accessors[acc_idx];
        let count = acc["count"].as_u64().unwrap_or(0) as usize;
        let view = &views[acc["bufferView"].as_u64().context("accessor bufferView")? as usize];
        let base = view["byteOffset"].as_u64().unwrap_or(0) as usize;
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let o = base + i * 12;
            let x = f32::from_le_bytes([bin[o], bin[o + 1], bin[o + 2], bin[o + 3]]);
            let y = f32::from_le_bytes([bin[o + 4], bin[o + 5], bin[o + 6], bin[o + 7]]);
            let z = f32::from_le_bytes([bin[o + 8], bin[o + 9], bin[o + 10], bin[o + 11]]);
            out.push([x, y, z]);
        }
        Ok(out)
    };

    let positions = read_vec3(pos_idx)?;
    let normals = match nrm_idx {
        Some(i) => read_vec3(i as usize)?,
        None => Vec::new(),
    };

    let acc = &accessors[idx_acc];
    let count = acc["count"].as_u64().unwrap_or(0) as usize;
    let view = &views[acc["bufferView"].as_u64().context("index bufferView")? as usize];
    let base = view["byteOffset"].as_u64().unwrap_or(0) as usize;
    let mut indices = Vec::with_capacity(count);
    for i in 0..count {
        let o = base + i * 4;
        indices.push(u32::from_le_bytes([bin[o], bin[o + 1], bin[o + 2], bin[o + 3]]));
    }

    Ok(Mesh {
        positions,
        normals,
        indices,
    })
}

// ---- Mesh loaders (out-of-process tessellation) -----------------------------

/// Strategy for turning a [`GeomRef`] into a tessellated [`Mesh`]. Returns
/// `Ok(None)` when the payload has no on-disk mesh (synthetic worlds).
pub trait MeshLoader: Send {
    fn load(&self, geom: &GeomRef) -> Result<Option<Mesh>>;
}

/// Loader that tessellates the real payload via the Python USD helper
/// (`usd_export.py --mesh`). Mirrors [`crate::hydrate::UsdPayloadLoader`].
pub struct UsdMeshLoader {
    /// Directory the (relative) payload URIs resolve against — the USD root's dir.
    pub base_dir: PathBuf,
    pub python: Option<String>,
}

impl UsdMeshLoader {
    pub fn new(base_dir: PathBuf, python: Option<String>) -> Self {
        UsdMeshLoader { base_dir, python }
    }
}

impl MeshLoader for UsdMeshLoader {
    fn load(&self, geom: &GeomRef) -> Result<Option<Mesh>> {
        let tools = import::tools_dir();
        let script = tools.join("usd_export.py");
        anyhow::ensure!(
            script.exists(),
            "USD helper not found at {}",
            script.display()
        );
        let py = import::resolve_python(self.python.as_deref(), &tools);
        let resolved = std::fs::canonicalize(self.base_dir.join(&geom.payload_uri))
            .unwrap_or_else(|_| self.base_dir.join(&geom.payload_uri));
        if !resolved.is_file() {
            return Ok(None); // no on-disk asset -> proxy box stays
        }

        let mut cmd = Command::new(&py);
        cmd.arg(&script).arg("--mesh").arg(&resolved);
        if !geom.prim_path.is_empty() {
            cmd.arg(&geom.prim_path);
        }
        let output = cmd
            .output()
            .with_context(|| format!("launching USD mesh helper for {}", resolved.display()))?;
        anyhow::ensure!(
            output.status.success(),
            "USD mesh helper exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let line = String::from_utf8_lossy(&output.stdout);
        let line = line.trim();
        anyhow::ensure!(!line.is_empty(), "USD mesh helper produced no output");
        let rec: MeshRecord = serde_json::from_str(line)
            .with_context(|| format!("parsing mesh record: {line}"))?;
        let mesh: Mesh = rec.into();
        if mesh.is_empty() {
            Ok(None)
        } else {
            Ok(Some(mesh))
        }
    }
}

/// Loader for synthetic / NDJSON worlds and Python-free tests: never produces a
/// mesh (no on-disk assets), so the observer keeps the proxy box.
pub struct StubMeshLoader;

impl MeshLoader for StubMeshLoader {
    fn load(&self, _geom: &GeomRef) -> Result<Option<Mesh>> {
        Ok(None)
    }
}

// ---- The content-addressed geometry store -----------------------------------

/// Content-addressed, GLB-encoding geometry store keyed by
/// [`GeomRef::content_hash`]. Built from the LSG (`content_hash -> GeomRef`),
/// it tessellates + encodes each unique payload **once** on first
/// [`fetch`](GeomStore::fetch) and caches the GLB bytes. Read-only and derived:
/// it never writes USD, the LSG, or the Twin Overlay.
pub struct GeomStore {
    loader: Box<dyn MeshLoader>,
    /// content_hash -> a GeomRef that produces it (identical hashes dedup).
    index: HashMap<String, GeomRef>,
    /// content_hash -> encoded GLB bytes (tessellated once).
    cache: HashMap<String, Arc<Vec<u8>>>,
    load_calls: usize,
}

impl GeomStore {
    pub fn new(loader: Box<dyn MeshLoader>) -> Self {
        GeomStore {
            loader,
            index: HashMap::new(),
            cache: HashMap::new(),
            load_calls: 0,
        }
    }

    /// Build a store, indexing every content hash referenced by the LSG.
    pub fn from_lsg(loader: Box<dyn MeshLoader>, lsg: &Lsg) -> Self {
        let mut store = GeomStore::new(loader);
        store.index_from_lsg(lsg);
        store
    }

    /// Register every non-empty `content_hash -> GeomRef` from the LSG. First
    /// writer wins so identical payloads collapse to one entry.
    pub fn index_from_lsg(&mut self, lsg: &Lsg) {
        for e in lsg.entities() {
            if let Some(g) = &e.geom_ref {
                self.register(g.clone());
            }
        }
    }

    /// Register a single `GeomRef` under its content hash (no-op for empty hash
    /// or an already-known hash).
    pub fn register(&mut self, geom: GeomRef) {
        if geom.content_hash.is_empty() {
            return;
        }
        self.index.entry(geom.content_hash.clone()).or_insert(geom);
    }

    /// Fetch the GLB bytes for a content hash, tessellating + encoding once and
    /// caching thereafter. Returns `Ok(None)` for an empty/unknown hash or a
    /// payload with no on-disk mesh (the observer keeps its proxy box).
    pub fn fetch(&mut self, content_hash: &str) -> Result<Option<Arc<Vec<u8>>>> {
        if content_hash.is_empty() {
            return Ok(None);
        }
        if let Some(bytes) = self.cache.get(content_hash) {
            return Ok(Some(Arc::clone(bytes)));
        }
        let Some(geom) = self.index.get(content_hash).cloned() else {
            return Ok(None);
        };
        self.load_calls += 1;
        let Some(mesh) = self.loader.load(&geom)? else {
            return Ok(None);
        };
        let bytes = Arc::new(encode_glb(&mesh));
        self.cache.insert(content_hash.to_string(), Arc::clone(&bytes));
        Ok(Some(bytes))
    }

    /// Number of distinct content hashes known from the LSG (index size).
    pub fn known_hashes(&self) -> usize {
        self.index.len()
    }

    /// Number of meshes tessellated + cached so far.
    pub fn cached_count(&self) -> usize {
        self.cache.len()
    }

    /// Total loader invocations (cache misses); cache hits do not bump it.
    pub fn load_calls(&self) -> usize {
        self.load_calls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tri_mesh() -> Mesh {
        Mesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0], [0.0, 0.0, 1.0], [0.0, 0.0, 1.0]],
            indices: vec![0, 1, 2],
        }
    }

    fn geom(uri: &str, hash: &str) -> GeomRef {
        GeomRef {
            payload_uri: uri.to_string(),
            prim_path: "/C".to_string(),
            content_hash: hash.to_string(),
            lod_ladder: vec![],
        }
    }

    /// A canned loader that hands back a fixed mesh for known hashes (Python-free).
    struct CannedMeshLoader {
        known: Vec<String>,
    }

    impl MeshLoader for CannedMeshLoader {
        fn load(&self, geom: &GeomRef) -> Result<Option<Mesh>> {
            if self.known.contains(&geom.content_hash) {
                Ok(Some(tri_mesh()))
            } else {
                Ok(None)
            }
        }
    }

    #[test]
    fn glb_round_trips() {
        let mesh = tri_mesh();
        let glb = encode_glb(&mesh);
        assert_eq!(&glb[0..4], b"glTF", "GLB magic");
        let back = decode_glb(&glb).unwrap();
        assert_eq!(back, mesh, "GLB round-trip preserves the mesh");
        assert!(back.vertex_count() > 0);
    }

    #[test]
    fn store_dedups_and_tessellates_once() {
        let loader = Box::new(CannedMeshLoader {
            known: vec!["h1".to_string()],
        });
        let mut store = GeomStore::new(loader);
        // Three instances share one payload hash; a second distinct hash exists.
        store.register(geom("./pump.usda", "h1"));
        store.register(geom("./pump.usda", "h1"));
        store.register(geom("./tank.usda", "h2"));
        assert_eq!(store.known_hashes(), 2, "identical hashes dedup");

        let a = store.fetch("h1").unwrap().expect("known mesh");
        let b = store.fetch("h1").unwrap().expect("cached mesh");
        assert_eq!(a.as_slice(), b.as_slice());
        assert_eq!(store.load_calls(), 1, "tessellate once, then cache hit");
        assert_eq!(store.cached_count(), 1);

        // h2 has no canned mesh -> None (proxy box stays); unknown hash -> None.
        assert!(store.fetch("h2").unwrap().is_none());
        assert!(store.fetch("nope").unwrap().is_none());
        assert!(store.fetch("").unwrap().is_none());
    }

    #[test]
    fn from_lsg_indexes_hashes_without_mutation() {
        let nd = r#"
{"primPath":"/W/a","kind":"component","geomRef":{"payloadUri":"./pump.usda","primPath":"/Pump","contentHash":"h1"}}
{"primPath":"/W/b","kind":"component","geomRef":{"payloadUri":"./pump.usda","primPath":"/Pump","contentHash":"h1"}}
{"primPath":"/W/c","kind":"component","geomRef":{"payloadUri":"./tank.usda","primPath":"/Tank","contentHash":"h2"}}
"#;
        let lsg = crate::import::build_from_ndjson(nd.as_bytes()).unwrap();
        let rev = lsg.revision();
        let store = GeomStore::from_lsg(Box::new(StubMeshLoader), &lsg);
        assert_eq!(store.known_hashes(), 2);
        assert_eq!(lsg.revision(), rev, "building the store never mutates the LSG");
    }
}
