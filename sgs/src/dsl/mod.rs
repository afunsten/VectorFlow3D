//! Flow3D DSL (spec §3.9) — a small, line-oriented, hand-authored language that
//! declares twin semantics (parts, tags, metadata, anchors, pipes, telemetry
//! bindings) and lowers them into stable-id Twin-Overlay [`Opinion`]s.
//!
//! Pipeline: [`lexer`] → [`parser`] → [`compile`], with [`diag`] providing
//! line/column-anchored, caret-rendered diagnostics throughout. The DSL never
//! emits live telemetry values or renderer-specific objects; it produces
//! declarative bindings and index opinions only.
//!
//! [`Opinion`]: crate::opinion::Opinion

pub mod ast;
pub mod compile;
pub mod diag;
pub mod lexer;
pub mod parser;

pub use compile::{compile, CompileResult};
pub use diag::{render, render_all, Diagnostic, Severity, Span};
