//! Abstract syntax tree for the Flow3D DSL. Every node retains the [`Span`] of
//! its defining token so the compiler can anchor semantic diagnostics (e.g.
//! "unknown part") back to the exact source location.

use super::diag::Span;

#[derive(Debug, Clone)]
pub struct Scene {
    pub name: String,
    pub name_span: Span,
    pub parts: Vec<Part>,
    pub pipes: Vec<Pipe>,
}

#[derive(Debug, Clone)]
pub struct Part {
    /// Selector: an `assetTag` (e.g. `PUMP-01`) or a prim path (`/A/B/C`).
    pub selector: String,
    pub selector_span: Span,
    pub tags: Vec<Tag>,
    pub metas: Vec<Meta>,
    pub anchors: Vec<Anchor>,
    pub bindings: Vec<Binding>,
}

#[derive(Debug, Clone)]
pub struct Tag {
    pub value: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Meta {
    pub key: String,
    pub value: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Anchor {
    pub name: String,
    pub pos: [f64; 3],
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Binding {
    pub attribute: String,
    pub query: String,
    pub unit: Option<String>,
    pub ttl_ms: Option<f64>,
    pub priority: Option<String>,
    /// Span of the bound attribute name (for diagnostics).
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Pipe {
    pub from: Endpoint,
    pub to: Endpoint,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub part: String,
    pub anchor: String,
    pub span: Span,
}
