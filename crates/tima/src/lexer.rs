use crate::diagnostic::Diagnostic;
use crate::source::{SourceFile, Span};

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    Identifier(String),
    Integer(i64),
    Float(f64),
    String(String),
    Transform,
    Uses,
    Return,
    If,
    Else,
    For,
    In,
    True,
    False,
    Null,
    LeftParen,
    RightParen,
    LeftBrace,
    RightBrace,
    LeftBracket,
    RightBracket,
    Comma,
    Colon,
    Semicolon,
    Dot,
    IdentityHash(String),
    Equal,
    EqualEqual,
    BangEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Arrow,
    Pipe,
    Plus,
    Minus,
    Star,
    StarEqual,
    Slash,
    Newline,
    Eof,
}

pub fn lex(source: &SourceFile) -> Result<Vec<Token>, Vec<Diagnostic>> {
    Lexer {
        text: source.text(),
        position: 0,
        tokens: Vec::new(),
        diagnostics: Vec::new(),
    }
    .run()
}

struct Lexer<'a> {
    text: &'a str,
    position: usize,
    tokens: Vec<Token>,
    diagnostics: Vec<Diagnostic>,
}

impl Lexer<'_> {
    fn run(mut self) -> Result<Vec<Token>, Vec<Diagnostic>> {
        while let Some(byte) = self.current() {
            let start = self.position;
            match byte {
                b' ' | b'\t' | b'\r' => self.position += 1,
                b'\n' => {
                    self.position += 1;
                    self.push(TokenKind::Newline, start);
                }
                b'/' if self.peek() == Some(b'/') => {
                    self.position += 2;
                    while self.current().is_some_and(|value| value != b'\n') {
                        self.position += 1;
                    }
                }
                b'0'..=b'9' => self.number(start),
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.identifier(start),
                b'"' => self.string(start),
                b'(' => self.single(TokenKind::LeftParen, start),
                b')' => self.single(TokenKind::RightParen, start),
                b'{' => self.single(TokenKind::LeftBrace, start),
                b'}' => self.single(TokenKind::RightBrace, start),
                b'[' => self.single(TokenKind::LeftBracket, start),
                b']' => self.single(TokenKind::RightBracket, start),
                b',' => self.single(TokenKind::Comma, start),
                b':' => self.single(TokenKind::Colon, start),
                b';' => self.single(TokenKind::Semicolon, start),
                b'.' => self.single(TokenKind::Dot, start),
                b'#' => self.identity_hash(start),
                b'=' if self.peek() == Some(b'=') => self.double(TokenKind::EqualEqual, start),
                b'=' => self.single(TokenKind::Equal, start),
                b'!' if self.peek() == Some(b'=') => self.double(TokenKind::BangEqual, start),
                b'<' if self.peek() == Some(b'=') => self.double(TokenKind::LessEqual, start),
                b'<' => self.single(TokenKind::Less, start),
                b'>' if self.peek() == Some(b'=') => self.double(TokenKind::GreaterEqual, start),
                b'>' => self.single(TokenKind::Greater, start),
                b'|' => self.single(TokenKind::Pipe, start),
                b'+' => self.single(TokenKind::Plus, start),
                b'*' if self.peek() == Some(b'=') => self.double(TokenKind::StarEqual, start),
                b'*' => self.single(TokenKind::Star, start),
                b'/' => self.single(TokenKind::Slash, start),
                b'-' if self.peek() == Some(b'>') => {
                    self.position += 2;
                    self.push(TokenKind::Arrow, start);
                }
                b'-' => self.single(TokenKind::Minus, start),
                _ => {
                    let ch = self.text[self.position..].chars().next().unwrap();
                    self.position += ch.len_utf8();
                    self.diagnostics.push(Diagnostic::error(
                        format!("unexpected character `{ch}`"),
                        Span::new(start, self.position),
                    ));
                }
            }
        }
        self.tokens.push(Token {
            kind: TokenKind::Eof,
            span: Span::new(self.position, self.position),
        });
        if self.diagnostics.is_empty() {
            Ok(self.tokens)
        } else {
            Err(self.diagnostics)
        }
    }

    fn current(&self) -> Option<u8> {
        self.text.as_bytes().get(self.position).copied()
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.position + 1).copied()
    }

    fn single(&mut self, kind: TokenKind, start: usize) {
        self.position += 1;
        self.push(kind, start);
    }

    fn double(&mut self, kind: TokenKind, start: usize) {
        self.position += 2;
        self.push(kind, start);
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        self.tokens.push(Token {
            kind,
            span: Span::new(start, self.position),
        });
    }

    fn identifier(&mut self, start: usize) {
        while self
            .current()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            self.position += 1;
        }
        let text = &self.text[start..self.position];
        let kind = match text {
            "transform" => TokenKind::Transform,
            "uses" => TokenKind::Uses,
            "return" => TokenKind::Return,
            "if" => TokenKind::If,
            "else" => TokenKind::Else,
            "for" => TokenKind::For,
            "in" => TokenKind::In,
            "true" => TokenKind::True,
            "false" => TokenKind::False,
            "null" => TokenKind::Null,
            _ => TokenKind::Identifier(text.to_owned()),
        };
        self.push(kind, start);
    }

    fn number(&mut self, start: usize) {
        while self.current().is_some_and(|byte| byte.is_ascii_digit()) {
            self.position += 1;
        }
        let is_float =
            self.current() == Some(b'.') && self.peek().is_some_and(|byte| byte.is_ascii_digit());
        if is_float {
            self.position += 1;
            while self.current().is_some_and(|byte| byte.is_ascii_digit()) {
                self.position += 1;
            }
        }
        let text = &self.text[start..self.position];
        let kind = if is_float {
            TokenKind::Float(text.parse().expect("validated float"))
        } else {
            match text.parse() {
                Ok(value) => TokenKind::Integer(value),
                Err(_) => {
                    self.diagnostics.push(Diagnostic::error(
                        "integer literal is outside the i64 range",
                        Span::new(start, self.position),
                    ));
                    return;
                }
            }
        };
        self.push(kind, start);
    }

    fn string(&mut self, start: usize) {
        self.position += 1;
        let mut value = String::new();
        let mut terminated = false;
        while let Some(byte) = self.current() {
            match byte {
                b'"' => {
                    self.position += 1;
                    terminated = true;
                    break;
                }
                b'\\' => {
                    self.position += 1;
                    let Some(escaped) = self.current() else { break };
                    self.position += 1;
                    match escaped {
                        b'n' => value.push('\n'),
                        b'r' => value.push('\r'),
                        b't' => value.push('\t'),
                        b'"' => value.push('"'),
                        b'\\' => value.push('\\'),
                        _ => self.diagnostics.push(Diagnostic::error(
                            "unsupported string escape",
                            Span::new(self.position - 2, self.position),
                        )),
                    }
                }
                b'\n' => break,
                _ => {
                    let ch = self.text[self.position..].chars().next().unwrap();
                    value.push(ch);
                    self.position += ch.len_utf8();
                }
            }
        }
        if terminated {
            self.push(TokenKind::String(value), start);
        } else {
            self.diagnostics.push(Diagnostic::error(
                "unterminated string literal",
                Span::new(start, self.position),
            ));
        }
    }

    fn identity_hash(&mut self, start: usize) {
        self.position += 1;
        let value_start = self.position;
        while self
            .current()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        {
            self.position += 1;
        }
        let value = &self.text[value_start..self.position];
        if value.is_empty() {
            self.diagnostics.push(Diagnostic::error(
                "semantic identity qualifier requires a hexadecimal hash or prefix",
                Span::new(start, self.position),
            ));
        } else if value.len() > 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            self.diagnostics.push(Diagnostic::error(
                "semantic identity qualifier must be 1 to 64 lowercase hexadecimal characters",
                Span::new(start, self.position),
            ));
        } else {
            self.push(TokenKind::IdentityHash(value.to_owned()), start);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_transform_and_pipeline_tokens() {
        let source = SourceFile::new(
            "test",
            "transform f(flag: bool, x: f32) -> f32 { if flag { return x } else { return x } }\n\
             transform fill(img: Image, value: u8) -> Image { for byte in img.bytes { byte = value }; return img }\n\
             transform darken(img: Image, factor: f32) -> Image { for p in img.pixels { p.r *= factor }; return img }\n\
             y = 1 | f\n comparisons = a == b != c < d <= e > f >= g",
        );
        let tokens = lex(&source).unwrap();
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Transform)
        );
        assert!(tokens.iter().any(|token| token.kind == TokenKind::Pipe));
        assert!(tokens.iter().any(|token| token.kind == TokenKind::Arrow));
        assert!(tokens.iter().any(|token| token.kind == TokenKind::If));
        assert!(tokens.iter().any(|token| token.kind == TokenKind::Else));
        assert!(tokens.iter().any(|token| token.kind == TokenKind::For));
        assert!(tokens.iter().any(|token| token.kind == TokenKind::In));
        assert!(tokens.iter().any(|token| token.kind == TokenKind::Dot));
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::StarEqual)
        );
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::EqualEqual)
        );
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::BangEqual)
        );
        assert!(tokens.iter().any(|token| token.kind == TokenKind::Less));
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::LessEqual)
        );
        assert!(tokens.iter().any(|token| token.kind == TokenKind::Greater));
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::GreaterEqual)
        );
    }

    #[test]
    fn recognizes_semantic_identity_hashes_and_prefixes() {
        let source = SourceFile::new("test.tima", "darken#0123abcdef ppm.decode#f\n");
        let tokens = lex(&source).unwrap();
        assert!(
            tokens
                .iter()
                .any(|token| { token.kind == TokenKind::IdentityHash("0123abcdef".to_owned()) })
        );
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::IdentityHash("f".to_owned()))
        );
    }

    #[test]
    fn rejects_invalid_semantic_identity_qualifiers() {
        let inputs = [
            "darken#".to_owned(),
            "darken#ABCD".to_owned(),
            "darken#xyz".to_owned(),
            format!("darken#{}", "a".repeat(65)),
        ];
        for text in inputs {
            let source = SourceFile::new("test.tima", text.clone());
            assert!(lex(&source).is_err(), "expected `{text}` to be rejected");
        }
    }
}
