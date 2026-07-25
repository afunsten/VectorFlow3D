//! Payload hydration for the Runtime Scene Graph (spec §3.3/§4.4, Phase 2).
//!
//! When interest activates an entity, its USD `payload` arc is opened to
//! surface the component-internal defaults the Phase 1 index could NOT see
//! (payloads stayed unloaded): `kind`, `class`, and a coarse geometry bounding
//! box / prim summary. This is done out-of-process by the same `usd_export.py`
//! helper (payload-load mode) so OpenUSD stays off the Rust build and off the
//! runtime hot path.
//!
//! Results are cached by `content_hash` (identical payloads hydrate once) and
//! refcounted, so the same three component files backing millions of instances
//! open only a handful of times. Hydration is READ-ONLY: it never contacts
//! VictoriaMetrics and never writes USD, the LSG, or the Twin Overlay. Surfaced
//! metadata lives only in the RSG.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::import;
use crate::lsg::{Aabb, GeomRef};

/// Component-internal defaults surfaced by opening a payload. Held in the RSG,
/// never written back to the LSG or USD.
#[derive(Debug, Clone, PartialEq)]
pub struct HydratedPayload {
    /// Cache key echoed from the `GeomRef` (content hash, or URI fallback).
    pub content_hash: String,
    pub payload_uri: String,
    /// Component `kind` authored inside the payload (usually `component`).
    pub kind: Option<String>,
    /// `customData.vf.class` authored inside the payload.
    pub class: Option<String>,
    /// Coarse geometry bounds computed from the loaded payload, if available.
    pub bbox: Option<Aabb>,
    /// Number of geometry prims summarized from the payload.
    pub prim_count: usize,
}

/// Strategy for turning a `GeomRef` into a [`HydratedPayload`].
pub trait PayloadLoader: Send {
    fn load(&self, geom: &GeomRef) -> Result<HydratedPayload>;
}

/// Cache key for a payload: prefer the content hash (stable across identical
/// files), fall back to the URI when the hash is unknown (e.g. synthetic).
pub fn cache_key(geom: &GeomRef) -> String {
    if geom.content_hash.is_empty() {
        geom.payload_uri.clone()
    } else {
        geom.content_hash.clone()
    }
}

/// Loader that opens the real payload via the Python USD helper.
pub struct UsdPayloadLoader {
    /// Directory the (relative) payload URIs resolve against — the USD root's
    /// directory.
    pub base_dir: PathBuf,
    pub python: Option<String>,
}

impl UsdPayloadLoader {
    pub fn new(base_dir: PathBuf, python: Option<String>) -> Self {
        UsdPayloadLoader { base_dir, python }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PayloadRecord {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    class: Option<String>,
    #[serde(default)]
    bbox: Option<[[f64; 3]; 2]>,
    #[serde(default)]
    prim_count: usize,
}

impl PayloadLoader for UsdPayloadLoader {
    fn load(&self, geom: &GeomRef) -> Result<HydratedPayload> {
        let tools = import::tools_dir();
        let script = tools.join("usd_export.py");
        anyhow::ensure!(
            script.exists(),
            "USD helper not found at {}",
            script.display()
        );
        let py = import::resolve_python(self.python.as_deref(), &tools);
        let resolved =
            std::fs::canonicalize(self.base_dir.join(&geom.payload_uri)).unwrap_or_else(|_| {
                self.base_dir.join(&geom.payload_uri)
            });

        let mut cmd = Command::new(&py);
        cmd.arg(&script).arg("--payload").arg(&resolved);
        if !geom.prim_path.is_empty() {
            cmd.arg(&geom.prim_path);
        }
        let output = cmd
            .output()
            .with_context(|| format!("launching USD payload helper for {}", resolved.display()))?;
        anyhow::ensure!(
            output.status.success(),
            "USD payload helper exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let line = String::from_utf8_lossy(&output.stdout);
        let line = line.trim();
        anyhow::ensure!(!line.is_empty(), "USD payload helper produced no output");
        let rec: PayloadRecord = serde_json::from_str(line)
            .with_context(|| format!("parsing payload record: {line}"))?;

        Ok(HydratedPayload {
            content_hash: cache_key(geom),
            payload_uri: geom.payload_uri.clone(),
            kind: rec.kind,
            class: rec.class,
            bbox: rec.bbox.map(|[min, max]| Aabb { min, max }),
            prim_count: rec.prim_count,
        })
    }
}

/// Loader used for synthetic worlds and Python-free tests: fabricates a
/// plausible `HydratedPayload` without opening any file. Proves the
/// load/unload + caching lifecycle without a USD environment.
pub struct StubPayloadLoader;

impl PayloadLoader for StubPayloadLoader {
    fn load(&self, geom: &GeomRef) -> Result<HydratedPayload> {
        Ok(HydratedPayload {
            content_hash: cache_key(geom),
            payload_uri: geom.payload_uri.clone(),
            kind: Some("component".to_string()),
            class: None,
            bbox: None,
            prim_count: 0,
        })
    }
}

struct CacheEntry {
    payload: Arc<HydratedPayload>,
    refcount: usize,
}

/// Content-hash-keyed, refcounted cache of hydrated payloads. `acquire` on an
/// already-loaded key is a cheap refcount bump (no loader call); `release`
/// drops the entry when the last reference goes away.
pub struct PayloadCache {
    loader: Box<dyn PayloadLoader>,
    loaded: HashMap<String, CacheEntry>,
    load_calls: usize,
}

impl PayloadCache {
    pub fn new(loader: Box<dyn PayloadLoader>) -> Self {
        PayloadCache {
            loader,
            loaded: HashMap::new(),
            load_calls: 0,
        }
    }

    /// Acquire a reference to the hydrated payload for `geom`, loading it once
    /// if not already cached. Returns the cache key and the shared payload.
    pub fn acquire(&mut self, geom: &GeomRef) -> Result<(String, Arc<HydratedPayload>)> {
        let key = cache_key(geom);
        if let Some(entry) = self.loaded.get_mut(&key) {
            entry.refcount += 1;
            return Ok((key, Arc::clone(&entry.payload)));
        }
        let payload = Arc::new(self.loader.load(geom)?);
        self.load_calls += 1;
        self.loaded.insert(
            key.clone(),
            CacheEntry {
                payload: Arc::clone(&payload),
                refcount: 1,
            },
        );
        Ok((key, payload))
    }

    /// Release one reference to `key`; unloads the payload at zero refs.
    pub fn release(&mut self, key: &str) {
        if let Some(entry) = self.loaded.get_mut(key) {
            entry.refcount = entry.refcount.saturating_sub(1);
            if entry.refcount == 0 {
                self.loaded.remove(key);
            }
        }
    }

    /// Number of distinct payloads currently open (spec §6.3 open payload count).
    pub fn loaded_count(&self) -> usize {
        self.loaded.len()
    }

    /// Total loader invocations so far (cache misses). Cache hits do not bump it.
    pub fn load_calls(&self) -> usize {
        self.load_calls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geom(uri: &str, hash: &str) -> GeomRef {
        GeomRef {
            payload_uri: uri.to_string(),
            prim_path: "/Component".to_string(),
            content_hash: hash.to_string(),
            lod_ladder: vec![],
        }
    }

    #[test]
    fn cache_dedups_by_content_hash() {
        let mut cache = PayloadCache::new(Box::new(StubPayloadLoader));
        let g = geom("./components/pump.usda", "abc");
        let (k1, _) = cache.acquire(&g).unwrap();
        let (k2, _) = cache.acquire(&g).unwrap();
        assert_eq!(k1, k2);
        // Two acquires, one distinct payload, one loader call.
        assert_eq!(cache.loaded_count(), 1);
        assert_eq!(cache.load_calls(), 1);

        cache.release(&k1);
        assert_eq!(cache.loaded_count(), 1); // still one ref
        cache.release(&k2);
        assert_eq!(cache.loaded_count(), 0); // unloaded
    }

    #[test]
    fn empty_hash_falls_back_to_uri() {
        let mut cache = PayloadCache::new(Box::new(StubPayloadLoader));
        let a = geom("./components/tank.usda", "");
        let b = geom("./components/tank.usda", "");
        let (ka, _) = cache.acquire(&a).unwrap();
        let (_kb, _) = cache.acquire(&b).unwrap();
        assert_eq!(ka, "./components/tank.usda");
        assert_eq!(cache.load_calls(), 1); // same URI -> cache hit
    }
}
