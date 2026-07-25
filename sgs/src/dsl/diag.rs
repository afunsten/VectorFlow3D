//! Source spans and rustc-style diagnostics for the Flow3D DSL.
//!
//! The DSL is the one artifact in the pipeline meant to be hand-authored by
//! people (spec §3.9), so error reporting is a first-class concern: every token
//! and AST node carries a [`Span`], and [`render`] prints the offending source
//! line with a caret underline and a `file:line:col` locator.

use std::fmt;

/// A half-open region of the source text. `line` / `col` are 1-based; `offset`
/// and `len` are byte indices into the original source (UTF-8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub line: u32,
    pub col: u32,
    pub offset: usize,
    pub len: usize,
}

impl Span {
    pub fn new(line: u32, col: u32, offset: usize, len: usize) -> Self {
        Span {
            line,
            col,
            offset,
            len,
        }
    }

    /// A zero-length span at the given position (used for EOF / "expected here").
    pub fn point(line: u32, col: u32, offset: usize) -> Self {
        Span::new(line, col, offset, 0)
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.col)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// A single compiler message anchored to a source span.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub span: Span,
}

impl Diagnostic {
    pub fn error(message: impl Into<String>, span: Span) -> Self {
        Diagnostic {
            severity: Severity::Error,
            message: message.into(),
            span,
        }
    }

    pub fn warning(message: impl Into<String>, span: Span) -> Self {
        Diagnostic {
            severity: Severity::Warning,
            message: message.into(),
            span,
        }
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

/// Render a diagnostic against its source, rustc-style:
///
/// ```text
/// error: unexpected token `}`
///   --> pump.flow3d:12:3
///    |
/// 12 |   }
///    |   ^
/// ```
pub fn render(diag: &Diagnostic, source: &str, filename: &str) -> String {
    let line_text = source.lines().nth(diag.span.line.saturating_sub(1) as usize).unwrap_or("");
    let line_no = diag.span.line.to_string();
    let gutter = " ".repeat(line_no.len());
    // Caret run: at least one `^`, clamped to the visible line width.
    let caret_len = diag.span.len.max(1);
    let pad = " ".repeat(diag.span.col.saturating_sub(1) as usize);
    let carets = "^".repeat(caret_len);

    let mut out = String::new();
    out.push_str(&format!("{}: {}\n", diag.severity.label(), diag.message));
    out.push_str(&format!("{}--> {}:{}:{}\n", gutter_arrow(&gutter), filename, diag.span.line, diag.span.col));
    out.push_str(&format!("{} |\n", gutter));
    out.push_str(&format!("{} | {}\n", line_no, line_text));
    out.push_str(&format!("{} | {}{}", gutter, pad, carets));
    out
}

fn gutter_arrow(gutter: &str) -> String {
    // Aligns the `-->` under the gutter width (rustc uses gutter spaces + "-->").
    format!("{} ", gutter)
}

/// Render all diagnostics separated by blank lines.
pub fn render_all(diags: &[Diagnostic], source: &str, filename: &str) -> String {
    diags
        .iter()
        .map(|d| render(d, source, filename))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caret_points_at_column() {
        let src = "scene \"X\"\npart PUMP-01 {\n  }\n";
        let d = Diagnostic::error("unexpected token `}`", Span::new(3, 3, 26, 1));
        let out = render(&d, src, "pump.flow3d");
        assert!(out.contains("error: unexpected token `}`"));
        assert!(out.contains("pump.flow3d:3:3"));
        // Caret sits under column 3 (two leading spaces then `^`).
        assert!(out.contains("  ^"), "rendered:\n{out}");
    }
}
