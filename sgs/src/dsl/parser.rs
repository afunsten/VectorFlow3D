//! Recursive-descent parser for the Flow3D DSL.
//!
//! Grammar (informal):
//! ```text
//! program   := "scene" STRING item*
//! item      := part | pipe
//! part      := "part" SELECTOR "{" part_item* "}"
//! part_item := tag | meta | anchor | bind
//! tag       := "tag" STRING
//! meta      := "meta" IDENT "=" (STRING | NUMBER)
//! anchor    := "anchor" IDENT "at" "(" NUM "," NUM "," NUM ")"
//! bind      := "bind" IDENT "metric" "(" STRING ")" bind_opt*
//! bind_opt  := "unit" STRING | "ttl" DURATION | "priority" IDENT
//! pipe      := "pipe" endpoint "->" endpoint
//! endpoint  := SELECTOR "." IDENT
//! ```
//!
//! The parser collects *all* diagnostics rather than bailing on the first error,
//! recovering to the next top-level/part-item boundary so one typo does not mask
//! the rest of the file.

use super::ast::*;
use super::diag::{Diagnostic, Span};
use super::lexer::{Tok, Token};

pub struct Parser {
    toks: Vec<Token>,
    pos: usize,
    diags: Vec<Diagnostic>,
}

/// Parse `tokens` into a [`Scene`] (when a scene header was present) plus any
/// diagnostics. A `None` scene means the input was too broken to anchor.
pub fn parse(tokens: Vec<Token>, lex_diags: Vec<Diagnostic>) -> (Option<Scene>, Vec<Diagnostic>) {
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        diags: lex_diags,
    };
    let scene = p.parse_program();
    (scene, p.diags)
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek_span(&self) -> Span {
        self.toks[self.pos].span
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek(), Tok::Eof)
    }

    fn bump(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        if !matches!(t.tok, Tok::Eof) {
            self.pos += 1;
        }
        t
    }

    fn error(&mut self, message: impl Into<String>, span: Span) {
        self.diags.push(Diagnostic::error(message, span));
    }

    /// Consume the expected token or emit a diagnostic (without consuming) and
    /// return `false` so the caller can recover.
    fn expect(&mut self, want: &Tok) -> bool {
        if std::mem::discriminant(self.peek()) == std::mem::discriminant(want) {
            self.bump();
            true
        } else {
            let msg = format!("expected {}, found {}", want.describe(), self.peek().describe());
            let span = self.peek_span();
            self.error(msg, span);
            false
        }
    }

    fn parse_program(&mut self) -> Option<Scene> {
        // scene header
        if !matches!(self.peek(), Tok::Scene) {
            let span = self.peek_span();
            self.error(
                format!("expected `scene` at the start of the file, found {}", self.peek().describe()),
                span,
            );
            return None;
        }
        self.bump(); // scene
        let (name, name_span) = match self.peek().clone() {
            Tok::Str(s) => {
                let sp = self.peek_span();
                self.bump();
                (s, sp)
            }
            other => {
                let sp = self.peek_span();
                self.error(format!("expected a scene name string, found {}", other.describe()), sp);
                (String::new(), sp)
            }
        };

        let mut parts = Vec::new();
        let mut pipes = Vec::new();
        while !self.at_eof() {
            match self.peek() {
                Tok::Part => {
                    if let Some(part) = self.parse_part() {
                        parts.push(part);
                    }
                }
                Tok::Pipe => {
                    if let Some(pipe) = self.parse_pipe() {
                        pipes.push(pipe);
                    }
                }
                _ => {
                    let span = self.peek_span();
                    self.error(
                        format!("expected `part` or `pipe`, found {}", self.peek().describe()),
                        span,
                    );
                    self.recover_to_top_level();
                }
            }
        }

        Some(Scene {
            name,
            name_span,
            parts,
            pipes,
        })
    }

    /// Skip tokens until the next top-level keyword (or EOF).
    fn recover_to_top_level(&mut self) {
        while !self.at_eof() && !matches!(self.peek(), Tok::Part | Tok::Pipe) {
            self.bump();
        }
    }

    /// Skip tokens until the next part-item keyword or a closing `}`.
    fn recover_in_part(&mut self) {
        while !self.at_eof()
            && !matches!(
                self.peek(),
                Tok::Tag | Tok::Meta | Tok::Anchor | Tok::Bind | Tok::RBrace
            )
        {
            self.bump();
        }
    }

    fn parse_selector(&mut self) -> Option<(String, Span)> {
        match self.peek().clone() {
            Tok::Ident(s) => {
                let sp = self.peek_span();
                self.bump();
                Some((s, sp))
            }
            other => {
                let sp = self.peek_span();
                self.error(format!("expected a part selector (asset tag or prim path), found {}", other.describe()), sp);
                None
            }
        }
    }

    fn parse_part(&mut self) -> Option<Part> {
        self.bump(); // part
        let (selector, selector_span) = self.parse_selector()?;
        if !self.expect(&Tok::LBrace) {
            self.recover_to_top_level();
            return None;
        }

        let mut part = Part {
            selector,
            selector_span,
            tags: Vec::new(),
            metas: Vec::new(),
            anchors: Vec::new(),
            bindings: Vec::new(),
        };

        loop {
            match self.peek() {
                Tok::RBrace => {
                    self.bump();
                    break;
                }
                Tok::Eof => {
                    let sp = self.peek_span();
                    self.error("unexpected end of input: missing `}` to close `part`", sp);
                    break;
                }
                // A top-level keyword here almost always means the `}` was
                // forgotten; stop the part (without consuming) so the outer loop
                // recovers on the next `part`/`pipe`.
                Tok::Part | Tok::Pipe => {
                    let sp = self.peek_span();
                    self.error("missing `}` to close `part` before the next item", sp);
                    break;
                }
                Tok::Tag => {
                    if let Some(t) = self.parse_tag() {
                        part.tags.push(t);
                    } else {
                        self.recover_in_part();
                    }
                }
                Tok::Meta => {
                    if let Some(m) = self.parse_meta() {
                        part.metas.push(m);
                    } else {
                        self.recover_in_part();
                    }
                }
                Tok::Anchor => {
                    if let Some(a) = self.parse_anchor() {
                        part.anchors.push(a);
                    } else {
                        self.recover_in_part();
                    }
                }
                Tok::Bind => {
                    if let Some(b) = self.parse_bind() {
                        part.bindings.push(b);
                    } else {
                        self.recover_in_part();
                    }
                }
                _ => {
                    let sp = self.peek_span();
                    self.error(
                        format!("expected `tag`, `meta`, `anchor`, `bind`, or `}}`, found {}", self.peek().describe()),
                        sp,
                    );
                    self.recover_in_part();
                }
            }
        }

        Some(part)
    }

    fn parse_tag(&mut self) -> Option<Tag> {
        self.bump(); // tag
        match self.peek().clone() {
            Tok::Str(s) => {
                let sp = self.peek_span();
                self.bump();
                Some(Tag { value: s, span: sp })
            }
            other => {
                let sp = self.peek_span();
                self.error(format!("expected a tag string, found {}", other.describe()), sp);
                None
            }
        }
    }

    fn parse_meta(&mut self) -> Option<Meta> {
        let span = self.peek_span();
        self.bump(); // meta
        let key = match self.peek().clone() {
            Tok::Ident(s) => {
                self.bump();
                s
            }
            other => {
                let sp = self.peek_span();
                self.error(format!("expected a metadata key, found {}", other.describe()), sp);
                return None;
            }
        };
        if !self.expect(&Tok::Eq) {
            return None;
        }
        let value = match self.peek().clone() {
            Tok::Str(s) => {
                self.bump();
                s
            }
            Tok::Num(n) => {
                self.bump();
                format_num(n)
            }
            other => {
                let sp = self.peek_span();
                self.error(format!("expected a string or number value, found {}", other.describe()), sp);
                return None;
            }
        };
        Some(Meta { key, value, span })
    }

    fn parse_anchor(&mut self) -> Option<Anchor> {
        let span = self.peek_span();
        self.bump(); // anchor
        let name = match self.peek().clone() {
            Tok::Ident(s) => {
                self.bump();
                s
            }
            other => {
                let sp = self.peek_span();
                self.error(format!("expected an anchor name, found {}", other.describe()), sp);
                return None;
            }
        };
        if !self.expect(&Tok::At) {
            return None;
        }
        if !self.expect(&Tok::LParen) {
            return None;
        }
        let x = self.parse_number()?;
        if !self.expect(&Tok::Comma) {
            return None;
        }
        let y = self.parse_number()?;
        if !self.expect(&Tok::Comma) {
            return None;
        }
        let z = self.parse_number()?;
        if !self.expect(&Tok::RParen) {
            return None;
        }
        Some(Anchor {
            name,
            pos: [x, y, z],
            span,
        })
    }

    fn parse_number(&mut self) -> Option<f64> {
        match self.peek().clone() {
            Tok::Num(n) => {
                self.bump();
                Some(n)
            }
            other => {
                let sp = self.peek_span();
                self.error(format!("expected a number, found {}", other.describe()), sp);
                None
            }
        }
    }

    fn parse_bind(&mut self) -> Option<Binding> {
        self.bump(); // bind
        let (attribute, span) = match self.peek().clone() {
            Tok::Ident(s) => {
                let sp = self.peek_span();
                self.bump();
                (s, sp)
            }
            other => {
                let sp = self.peek_span();
                self.error(format!("expected an attribute name, found {}", other.describe()), sp);
                return None;
            }
        };
        if !self.expect(&Tok::Metric) {
            return None;
        }
        if !self.expect(&Tok::LParen) {
            return None;
        }
        let query = match self.peek().clone() {
            Tok::Str(s) => {
                self.bump();
                s
            }
            other => {
                let sp = self.peek_span();
                self.error(format!("expected a PromQL query string, found {}", other.describe()), sp);
                return None;
            }
        };
        if !self.expect(&Tok::RParen) {
            return None;
        }

        let mut binding = Binding {
            attribute,
            query,
            unit: None,
            ttl_ms: None,
            priority: None,
            span,
        };

        // Optional modifiers in any order until the next part-item boundary.
        loop {
            match self.peek() {
                Tok::Unit => {
                    self.bump();
                    match self.peek().clone() {
                        Tok::Str(s) => {
                            self.bump();
                            binding.unit = Some(s);
                        }
                        other => {
                            let sp = self.peek_span();
                            self.error(format!("expected a unit string, found {}", other.describe()), sp);
                        }
                    }
                }
                Tok::Ttl => {
                    self.bump();
                    match self.peek().clone() {
                        Tok::Dur(ms) => {
                            self.bump();
                            binding.ttl_ms = Some(ms);
                        }
                        Tok::Num(ms) => {
                            // Bare number: treat as milliseconds but warn.
                            let sp = self.peek_span();
                            self.bump();
                            binding.ttl_ms = Some(ms);
                            self.diags.push(Diagnostic::warning(
                                "ttl without a `ms` suffix; interpreting as milliseconds",
                                sp,
                            ));
                        }
                        other => {
                            let sp = self.peek_span();
                            self.error(format!("expected a duration (e.g. `5000ms`), found {}", other.describe()), sp);
                        }
                    }
                }
                Tok::Priority => {
                    self.bump();
                    match self.peek().clone() {
                        Tok::Ident(s) => {
                            let sp = self.peek_span();
                            self.bump();
                            if s != "high" && s != "background" {
                                self.diags.push(Diagnostic::warning(
                                    format!("unknown priority `{s}` (expected `high` or `background`); treating as `background`"),
                                    sp,
                                ));
                            }
                            binding.priority = Some(s);
                        }
                        other => {
                            let sp = self.peek_span();
                            self.error(format!("expected a priority (`high` or `background`), found {}", other.describe()), sp);
                        }
                    }
                }
                _ => break,
            }
        }

        Some(binding)
    }

    fn parse_pipe(&mut self) -> Option<Pipe> {
        let span = self.peek_span();
        self.bump(); // pipe
        let from = self.parse_endpoint()?;
        if !self.expect(&Tok::Arrow) {
            self.recover_to_top_level();
            return None;
        }
        let to = self.parse_endpoint()?;
        Some(Pipe { from, to, span })
    }

    fn parse_endpoint(&mut self) -> Option<Endpoint> {
        let (part, span) = self.parse_selector()?;
        if !self.expect(&Tok::Dot) {
            return None;
        }
        let anchor = match self.peek().clone() {
            Tok::Ident(s) => {
                self.bump();
                s
            }
            other => {
                let sp = self.peek_span();
                self.error(format!("expected an anchor name after `.`, found {}", other.describe()), sp);
                return None;
            }
        };
        Some(Endpoint { part, anchor, span })
    }
}

fn format_num(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

#[cfg(test)]
mod tests {
    use super::super::lexer::lex;
    use super::*;

    fn parse_src(src: &str) -> (Option<Scene>, Vec<Diagnostic>) {
        let (toks, ld) = lex(src);
        parse(toks, ld)
    }

    #[test]
    fn parses_a_full_part_and_pipe() {
        let src = r#"
scene "PS"
part PUMP-01 {
  tag "duty"
  meta manufacturer = "Acme"
  anchor discharge at (0.9, 0, 0)
  bind flow metric("pump_flow_gpm{asset=\"PUMP-01\"}") unit "gpm" ttl 5000ms priority high
}
pipe PUMP-01.discharge -> TANK-A.inlet
"#;
        let (scene, diags) = parse_src(src);
        assert!(diags.iter().all(|d| !d.is_error()), "diags: {diags:?}");
        let scene = scene.unwrap();
        assert_eq!(scene.name, "PS");
        assert_eq!(scene.parts.len(), 1);
        let p = &scene.parts[0];
        assert_eq!(p.selector, "PUMP-01");
        assert_eq!(p.tags.len(), 1);
        assert_eq!(p.metas.len(), 1);
        assert_eq!(p.anchors.len(), 1);
        assert_eq!(p.bindings.len(), 1);
        assert_eq!(p.bindings[0].unit.as_deref(), Some("gpm"));
        assert_eq!(p.bindings[0].ttl_ms, Some(5000.0));
        assert_eq!(p.bindings[0].priority.as_deref(), Some("high"));
        assert_eq!(scene.pipes.len(), 1);
        assert_eq!(scene.pipes[0].from.anchor, "discharge");
        assert_eq!(scene.pipes[0].to.part, "TANK-A");
    }

    #[test]
    fn reports_error_with_span_and_continues() {
        // Missing closing brace on first part; second part should still parse.
        let src = "scene \"X\"\npart A {\n  tag \"t\"\npart B {\n}\n";
        let (scene, diags) = parse_src(src);
        assert!(diags.iter().any(|d| d.is_error()));
        // Parser recovered enough to see part B.
        let scene = scene.unwrap();
        assert!(scene.parts.iter().any(|p| p.selector == "B"));
    }

    #[test]
    fn missing_scene_header_is_an_error() {
        let (scene, diags) = parse_src("part A { }");
        assert!(scene.is_none());
        assert!(diags[0].message.contains("scene"));
        assert_eq!(diags[0].span.line, 1);
        assert_eq!(diags[0].span.col, 1);
    }
}
