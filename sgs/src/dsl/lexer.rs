//! Hand-written lexer for the Flow3D DSL.
//!
//! Produces a flat token stream where every token carries a [`Span`] so the
//! parser and compiler can point at exact source locations. Whitespace and
//! `#` line comments are skipped; newlines only advance the line counter.

use super::diag::{Diagnostic, Span};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // Keywords.
    Scene,
    Part,
    Anchor,
    Bind,
    Metric,
    Unit,
    Ttl,
    Priority,
    Pipe,
    Tag,
    Meta,
    At,
    // Literals / identifiers.
    Ident(String),
    Str(String),
    Num(f64),
    /// A duration literal in milliseconds (e.g. `5000ms`).
    Dur(f64),
    // Punctuation.
    Arrow,
    LBrace,
    RBrace,
    LParen,
    RParen,
    Comma,
    Eq,
    Dot,
    Eof,
}

impl Tok {
    /// Human-readable description for diagnostics.
    pub fn describe(&self) -> String {
        match self {
            Tok::Scene => "`scene`".into(),
            Tok::Part => "`part`".into(),
            Tok::Anchor => "`anchor`".into(),
            Tok::Bind => "`bind`".into(),
            Tok::Metric => "`metric`".into(),
            Tok::Unit => "`unit`".into(),
            Tok::Ttl => "`ttl`".into(),
            Tok::Priority => "`priority`".into(),
            Tok::Pipe => "`pipe`".into(),
            Tok::Tag => "`tag`".into(),
            Tok::Meta => "`meta`".into(),
            Tok::At => "`at`".into(),
            Tok::Ident(s) => format!("identifier `{s}`"),
            Tok::Str(_) => "string literal".into(),
            Tok::Num(_) => "number".into(),
            Tok::Dur(_) => "duration".into(),
            Tok::Arrow => "`->`".into(),
            Tok::LBrace => "`{`".into(),
            Tok::RBrace => "`}`".into(),
            Tok::LParen => "`(`".into(),
            Tok::RParen => "`)`".into(),
            Tok::Comma => "`,`".into(),
            Tok::Eq => "`=`".into(),
            Tok::Dot => "`.`".into(),
            Tok::Eof => "end of input".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}

struct Lexer<'a> {
    src: &'a [u8],
    text: &'a str,
    pos: usize,
    line: u32,
    col: u32,
}

fn keyword(word: &str) -> Option<Tok> {
    Some(match word {
        "scene" => Tok::Scene,
        "part" => Tok::Part,
        "anchor" => Tok::Anchor,
        "bind" => Tok::Bind,
        "metric" => Tok::Metric,
        "unit" => Tok::Unit,
        "ttl" => Tok::Ttl,
        "priority" => Tok::Priority,
        "pipe" => Tok::Pipe,
        "tag" => Tok::Tag,
        "meta" => Tok::Meta,
        "at" => Tok::At,
        _ => return None,
    })
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'/'
}

fn is_ident_continue(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'/' || c == b':'
}

impl<'a> Lexer<'a> {
    fn new(text: &'a str) -> Self {
        Lexer {
            src: text.as_bytes(),
            text,
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<u8> {
        self.src.get(self.pos + 1).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.pos += 1;
        if c == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn here(&self) -> (u32, u32, usize) {
        (self.line, self.col, self.pos)
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') => {
                    self.bump();
                }
                Some(b'#') => {
                    while let Some(c) = self.peek() {
                        if c == b'\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                _ => break,
            }
        }
    }

    fn make(&self, tok: Tok, line: u32, col: u32, offset: usize) -> Token {
        Token {
            tok,
            span: Span::new(line, col, offset, self.pos - offset),
        }
    }

    fn next_token(&mut self, diags: &mut Vec<Diagnostic>) -> Token {
        self.skip_trivia();
        let (line, col, offset) = self.here();
        let Some(c) = self.peek() else {
            return Token {
                tok: Tok::Eof,
                span: Span::point(line, col, offset),
            };
        };

        // Punctuation and operators.
        match c {
            b'{' => {
                self.bump();
                return self.make(Tok::LBrace, line, col, offset);
            }
            b'}' => {
                self.bump();
                return self.make(Tok::RBrace, line, col, offset);
            }
            b'(' => {
                self.bump();
                return self.make(Tok::LParen, line, col, offset);
            }
            b')' => {
                self.bump();
                return self.make(Tok::RParen, line, col, offset);
            }
            b',' => {
                self.bump();
                return self.make(Tok::Comma, line, col, offset);
            }
            b'=' => {
                self.bump();
                return self.make(Tok::Eq, line, col, offset);
            }
            b'.' => {
                // A `.` followed by a digit is a number like `.5`; otherwise Dot.
                if self.peek2().map(|d| d.is_ascii_digit()).unwrap_or(false) {
                    return self.lex_number(diags, line, col, offset);
                }
                self.bump();
                return self.make(Tok::Dot, line, col, offset);
            }
            b'"' => return self.lex_string(diags, line, col, offset),
            b'-' => {
                if self.peek2() == Some(b'>') {
                    self.bump();
                    self.bump();
                    return self.make(Tok::Arrow, line, col, offset);
                }
                if self.peek2().map(|d| d.is_ascii_digit() || d == b'.').unwrap_or(false) {
                    return self.lex_number(diags, line, col, offset);
                }
                // Stray `-`.
                self.bump();
                diags.push(Diagnostic::error(
                    "unexpected `-` (expected `->` or a number)",
                    Span::new(line, col, offset, 1),
                ));
                // Recover by emitting it as an identifier-ish ident to keep going.
                return self.make(Tok::Ident("-".into()), line, col, offset);
            }
            _ => {}
        }

        if c.is_ascii_digit() {
            return self.lex_number(diags, line, col, offset);
        }
        if is_ident_start(c) {
            return self.lex_ident(line, col, offset);
        }

        // Unknown byte: report and skip one char to make progress.
        self.bump();
        diags.push(Diagnostic::error(
            format!("unexpected character `{}`", c as char),
            Span::new(line, col, offset, 1),
        ));
        self.next_token(diags)
    }

    fn lex_ident(&mut self, line: u32, col: u32, offset: usize) -> Token {
        while let Some(c) = self.peek() {
            if is_ident_continue(c) {
                self.bump();
            } else {
                break;
            }
        }
        let word = &self.text[offset..self.pos];
        match keyword(word) {
            Some(kw) => self.make(kw, line, col, offset),
            None => self.make(Tok::Ident(word.to_string()), line, col, offset),
        }
    }

    fn lex_number(
        &mut self,
        diags: &mut Vec<Diagnostic>,
        line: u32,
        col: u32,
        offset: usize,
    ) -> Token {
        if self.peek() == Some(b'-') {
            self.bump();
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.bump();
            } else {
                break;
            }
        }
        if self.peek() == Some(b'.') {
            self.bump();
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    self.bump();
                } else {
                    break;
                }
            }
        }
        let num_text = &self.text[offset..self.pos];
        // Duration suffix: `ms` directly after the number (not part of an ident).
        let is_dur = self.peek() == Some(b'm')
            && self.peek2() == Some(b's')
            && !self
                .src
                .get(self.pos + 2)
                .map(|&c| is_ident_continue(c))
                .unwrap_or(false);
        let value: f64 = num_text.parse().unwrap_or(f64::NAN);
        if value.is_nan() {
            diags.push(Diagnostic::error(
                format!("invalid number `{num_text}`"),
                Span::new(line, col, offset, self.pos - offset),
            ));
        }
        if is_dur {
            self.bump();
            self.bump();
            self.make(Tok::Dur(value), line, col, offset)
        } else {
            self.make(Tok::Num(value), line, col, offset)
        }
    }

    fn lex_string(
        &mut self,
        diags: &mut Vec<Diagnostic>,
        line: u32,
        col: u32,
        offset: usize,
    ) -> Token {
        self.bump(); // opening quote
        let mut value = String::new();
        loop {
            match self.peek() {
                None | Some(b'\n') => {
                    diags.push(Diagnostic::error(
                        "unterminated string literal",
                        Span::new(line, col, offset, self.pos - offset),
                    ));
                    break;
                }
                Some(b'"') => {
                    self.bump();
                    break;
                }
                Some(b'\\') => {
                    self.bump();
                    match self.peek() {
                        Some(b'"') => {
                            value.push('"');
                            self.bump();
                        }
                        Some(b'\\') => {
                            value.push('\\');
                            self.bump();
                        }
                        Some(b'n') => {
                            value.push('\n');
                            self.bump();
                        }
                        Some(b't') => {
                            value.push('\t');
                            self.bump();
                        }
                        Some(other) => {
                            value.push(other as char);
                            self.bump();
                        }
                        None => {}
                    }
                }
                Some(_) => {
                    // Consume one UTF-8 char worth of bytes.
                    let start = self.pos;
                    self.bump();
                    while let Some(c) = self.peek() {
                        if (c & 0b1100_0000) == 0b1000_0000 {
                            self.bump();
                        } else {
                            break;
                        }
                    }
                    value.push_str(&self.text[start..self.pos]);
                }
            }
        }
        self.make(Tok::Str(value), line, col, offset)
    }
}

/// Tokenize `text`, returning the tokens (always ending in `Eof`) plus any
/// lexical diagnostics encountered.
pub fn lex(text: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    let mut lexer = Lexer::new(text);
    let mut diags = Vec::new();
    let mut tokens = Vec::new();
    loop {
        let t = lexer.next_token(&mut diags);
        let is_eof = t.tok == Tok::Eof;
        tokens.push(t);
        if is_eof {
            break;
        }
    }
    (tokens, diags)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<Tok> {
        lex(src).0.into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn lexes_keywords_idents_and_punct() {
        let ks = kinds("scene part PUMP-01 { }");
        assert_eq!(ks[0], Tok::Scene);
        assert_eq!(ks[1], Tok::Part);
        assert_eq!(ks[2], Tok::Ident("PUMP-01".into()));
        assert_eq!(ks[3], Tok::LBrace);
        assert_eq!(ks[4], Tok::RBrace);
        assert_eq!(ks[5], Tok::Eof);
    }

    #[test]
    fn distinguishes_arrow_from_negative_number() {
        let ks = kinds("PUMP-01.a -> TANK-A.b  at (-1.5, 0, 0)");
        assert!(ks.contains(&Tok::Arrow));
        assert!(ks.contains(&Tok::Dot));
        assert!(ks.contains(&Tok::Num(-1.5)));
        assert!(ks.contains(&Tok::Ident("PUMP-01".into())));
    }

    #[test]
    fn lexes_duration_and_string_escapes() {
        let ks = kinds("ttl 5000ms metric(\"m{asset=\\\"PUMP-01\\\"}\")");
        assert!(ks.contains(&Tok::Dur(5000.0)));
        assert!(ks.iter().any(|t| matches!(t, Tok::Str(s) if s == "m{asset=\"PUMP-01\"}")));
    }

    #[test]
    fn tracks_line_and_column() {
        let (toks, _) = lex("scene\n  part");
        assert_eq!(toks[1].span.line, 2);
        assert_eq!(toks[1].span.col, 3);
    }
}
