//! VectorFlow3D Scene Graph Service — Phase 1 library.
//!
//! Exposes the Logical Scene Graph index ([`lsg`]), the USD import path
//! ([`import`]), the SQLite Twin Overlay ([`overlay`]), and the synthetic
//! world generator ([`synth`]) so both the `sgs` binary and the integration
//! tests share one implementation.

pub mod alert;
pub mod bridge;
pub mod dsl;
pub mod fake_bridge;
pub mod geomstore;
pub mod hydrate;
pub mod import;
pub mod interest;
pub mod lsg;
pub mod opinion;
pub mod overlay;
pub mod resolver;
pub mod rsg;
pub mod serve;
pub mod spatial;
pub mod synth;
