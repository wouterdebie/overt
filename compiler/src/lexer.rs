//! Lexer.
//!
//! Newlines end statements, so the lexer decides which line breaks matter:
//! - inside `(` and `[` they never do;
//! - after a token that can't end an expression (an operator, `,`, an open bracket) they don't;
//! - before a line starting with `.name`, `.0`, `else` or `catch` they don't.
//! Every other line break becomes one `Newline` token (runs are collapsed).
//!
//! Literals keep their source text so `ovt fmt` can print them unchanged; values
//! are decoded later.

use crate::diag::Diag;
use crate::source::Span;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kw {
    Fn,
    Type,
    Enum,
    Const,
    Let,
    Var,
    If,
    Else,
    Match,
    For,
    In,
    While,
    Break,
    Continue,
    Return,
    Inout,
    Sink,
    SelfKw,
    Par,
    Lock,
    As,
    Extern,
    Unsafe,
    None,
    True,
    False,
    Simd,
}

impl Kw {
    pub fn from_str(s: &str) -> Option<Kw> {
        Some(match s {
            "fn" => Kw::Fn,
            "type" => Kw::Type,
            "enum" => Kw::Enum,
            "const" => Kw::Const,
            "let" => Kw::Let,
            "var" => Kw::Var,
            "if" => Kw::If,
            "else" => Kw::Else,
            "match" => Kw::Match,
            "for" => Kw::For,
            "in" => Kw::In,
            "while" => Kw::While,
            "break" => Kw::Break,
            "continue" => Kw::Continue,
            "return" => Kw::Return,
            "inout" => Kw::Inout,
            "sink" => Kw::Sink,
            "self" => Kw::SelfKw,
            "par" => Kw::Par,
            "lock" => Kw::Lock,
            "as" => Kw::As,
            "extern" => Kw::Extern,
            "unsafe" => Kw::Unsafe,
            "none" => Kw::None,
            "true" => Kw::True,
            "false" => Kw::False,
            "simd" => Kw::Simd,
            _ => return None,
        })
    }

    pub fn text(self) -> &'static str {
        match self {
            Kw::Fn => "fn",
            Kw::Type => "type",
            Kw::Enum => "enum",
            Kw::Const => "const",
            Kw::Let => "let",
            Kw::Var => "var",
            Kw::If => "if",
            Kw::Else => "else",
            Kw::Match => "match",
            Kw::For => "for",
            Kw::In => "in",
            Kw::While => "while",
            Kw::Break => "break",
            Kw::Continue => "continue",
            Kw::Return => "return",
            Kw::Inout => "inout",
            Kw::Sink => "sink",
            Kw::SelfKw => "self",
            Kw::Par => "par",
            Kw::Lock => "lock",
            Kw::As => "as",
            Kw::Extern => "extern",
            Kw::Unsafe => "unsafe",
            Kw::None => "none",
            Kw::True => "true",
            Kw::False => "false",
            Kw::Simd => "simd",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum P {
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Colon,
    Semi,
    Dot,
    DotDot,
    DotDotEq,
    FatArrow,
    Arrow,
    Bang,
    Question,
    Eq,
    EqEq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    PlusW,
    MinusW,
    StarW,
    Amp,
    Pipe,
    Caret,
    Shl,
    Shr,
    AndAnd,
    OrOr,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    PlusWEq,
    MinusWEq,
    StarWEq,
    AmpEq,
    PipeEq,
    CaretEq,
    ShlEq,
    ShrEq,
    Underscore,
}

impl P {
    pub fn text(self) -> &'static str {
        match self {
            P::LParen => "(",
            P::RParen => ")",
            P::LBracket => "[",
            P::RBracket => "]",
            P::LBrace => "{",
            P::RBrace => "}",
            P::Comma => ",",
            P::Colon => ":",
            P::Semi => ";",
            P::Dot => ".",
            P::DotDot => "..",
            P::DotDotEq => "..=",
            P::FatArrow => "=>",
            P::Arrow => "->",
            P::Bang => "!",
            P::Question => "?",
            P::Eq => "=",
            P::EqEq => "==",
            P::Ne => "!=",
            P::Lt => "<",
            P::Le => "<=",
            P::Gt => ">",
            P::Ge => ">=",
            P::Plus => "+",
            P::Minus => "-",
            P::Star => "*",
            P::Slash => "/",
            P::Percent => "%",
            P::PlusW => "+%",
            P::MinusW => "-%",
            P::StarW => "*%",
            P::Amp => "&",
            P::Pipe => "|",
            P::Caret => "^",
            P::Shl => "<<",
            P::Shr => ">>",
            P::AndAnd => "&&",
            P::OrOr => "||",
            P::PlusEq => "+=",
            P::MinusEq => "-=",
            P::StarEq => "*=",
            P::SlashEq => "/=",
            P::PercentEq => "%=",
            P::PlusWEq => "+%=",
            P::MinusWEq => "-%=",
            P::StarWEq => "*%=",
            P::AmpEq => "&=",
            P::PipeEq => "|=",
            P::CaretEq => "^=",
            P::ShlEq => "<<=",
            P::ShrEq => ">>=",
            P::Underscore => "_",
        }
    }
}

// Longest first, so "..=" wins over "..", and ".." over ".".
const PUNCTS: &[(&str, P)] = &[
    ("..=", P::DotDotEq),
    ("+%=", P::PlusWEq),
    ("-%=", P::MinusWEq),
    ("*%=", P::StarWEq),
    ("<<=", P::ShlEq),
    (">>=", P::ShrEq),
    ("&=", P::AmpEq),
    ("|=", P::PipeEq),
    ("^=", P::CaretEq),
    ("..", P::DotDot),
    ("=>", P::FatArrow),
    ("->", P::Arrow),
    ("==", P::EqEq),
    ("!=", P::Ne),
    ("<=", P::Le),
    (">=", P::Ge),
    ("<<", P::Shl),
    (">>", P::Shr),
    ("&&", P::AndAnd),
    ("||", P::OrOr),
    ("+=", P::PlusEq),
    ("-=", P::MinusEq),
    ("*=", P::StarEq),
    ("/=", P::SlashEq),
    ("%=", P::PercentEq),
    ("+%", P::PlusW),
    ("-%", P::MinusW),
    ("*%", P::StarW),
    ("(", P::LParen),
    (")", P::RParen),
    ("[", P::LBracket),
    ("]", P::RBracket),
    ("{", P::LBrace),
    ("}", P::RBrace),
    (",", P::Comma),
    (":", P::Colon),
    (";", P::Semi),
    (".", P::Dot),
    ("!", P::Bang),
    ("?", P::Question),
    ("=", P::Eq),
    ("<", P::Lt),
    (">", P::Gt),
    ("+", P::Plus),
    ("-", P::Minus),
    ("*", P::Star),
    ("/", P::Slash),
    ("%", P::Percent),
    ("&", P::Amp),
    ("|", P::Pipe),
    ("^", P::Caret),
];

#[derive(Clone, Debug, PartialEq)]
pub enum StrSeg {
    /// Literal text exactly as written in the source (escapes not decoded).
    Text(String),
    /// The tokens of `${ ... }`, and the span of the expression inside.
    Interp(Vec<Token>, Span),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Ident(String),
    Kw(Kw),
    Int(String),
    Float(String),
    Dur(String),
    /// Source text between the quotes of a byte literal.
    Byte(String),
    Str { triple: bool, segs: Vec<StrSeg> },
    P(P),
    Newline,
    Eof,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Comment {
    pub span: Span,
    /// True when only whitespace precedes the comment on its line.
    pub own_line: bool,
}

pub struct Lexed {
    pub tokens: Vec<Token>,
    pub comments: Vec<Comment>,
    pub diags: Vec<Diag>,
}

pub fn lex(text: &str) -> Lexed {
    let mut lx = Lexer::new(text, 0, text.len(), false);
    lx.run();
    Lexed { tokens: lx.toks, comments: lx.comments, diags: lx.diags }
}

struct Lexer<'a> {
    s: &'a [u8],
    text: &'a str,
    pos: usize,
    end: usize,
    toks: Vec<Token>,
    comments: Vec<Comment>,
    diags: Vec<Diag>,
    /// Open brackets: b'(', b'[' or b'{'.
    stack: Vec<u8>,
    /// Lexing the inside of `${ ... }`: newlines never matter.
    interp: bool,
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Tokens after which a line break doesn't end the statement.
fn continues_line(t: &Tok) -> bool {
    match t {
        Tok::P(p) => !matches!(
            p,
            P::RParen | P::RBracket | P::RBrace | P::Question | P::Underscore | P::Semi
        ),
        Tok::Kw(k) => matches!(k, Kw::In | Kw::As | Kw::Else),
        _ => false,
    }
}

impl<'a> Lexer<'a> {
    fn new(text: &'a str, start: usize, end: usize, interp: bool) -> Lexer<'a> {
        Lexer {
            s: text.as_bytes(),
            text,
            pos: start,
            end,
            toks: Vec::new(),
            comments: Vec::new(),
            diags: Vec::new(),
            stack: Vec::new(),
            interp,
        }
    }

    fn peek_at(&self, p: usize) -> u8 {
        if p < self.end { self.s[p] } else { 0 }
    }

    fn starts_with(&self, p: usize, lit: &str) -> bool {
        self.s[p..self.end].starts_with(lit.as_bytes())
    }

    fn push(&mut self, tok: Tok, lo: usize, hi: usize) {
        self.toks.push(Token { tok, span: Span::new(lo, hi) });
    }

    fn err(&mut self, lo: usize, hi: usize, msg: impl Into<String>) {
        self.diags.push(Diag::new(Span::new(lo, hi), msg));
    }

    fn run(&mut self) {
        while self.pos < self.end {
            let c = self.s[self.pos];
            match c {
                b' ' | b'\t' | b'\r' => self.pos += 1,
                b'\n' => {
                    self.newline();
                    self.pos += 1;
                }
                b'/' if self.peek_at(self.pos + 1) == b'/' => self.comment(),
                c if is_ident_start(c) => self.ident(),
                c if c.is_ascii_digit() => self.number(),
                b'"' => self.string(),
                b'\'' => self.byte_lit(),
                _ => self.punct(),
            }
        }
        if !self.interp {
            if !matches!(self.toks.last(), None | Some(Token { tok: Tok::Newline, .. })) {
                self.push(Tok::Newline, self.end, self.end);
            }
            self.push(Tok::Eof, self.end, self.end);
        }
    }

    fn newline(&mut self) {
        if self.interp || matches!(self.stack.last(), Some(b'(') | Some(b'[')) {
            return;
        }
        match self.toks.last() {
            None => return,
            Some(t) if t.tok == Tok::Newline || continues_line(&t.tok) => return,
            _ => {}
        }
        // Look at how the next non-blank line starts.
        let mut p = self.pos + 1;
        while p < self.end && matches!(self.s[p], b' ' | b'\t' | b'\r' | b'\n') {
            p += 1;
        }
        if self.peek_at(p) == b'.' {
            let n = self.peek_at(p + 1);
            if n.is_ascii_lowercase() || n == b'_' || n.is_ascii_digit() {
                return;
            }
        }
        for word in ["else", "catch"] {
            if self.starts_with(p, word) && !is_ident_char(self.peek_at(p + word.len())) {
                return;
            }
        }
        self.push(Tok::Newline, self.pos, self.pos + 1);
    }

    fn comment(&mut self) {
        let lo = self.pos;
        let line_start = self.text[..lo].rfind('\n').map_or(0, |i| i + 1);
        let own_line = self.text[line_start..lo].trim().is_empty();
        while self.pos < self.end && self.s[self.pos] != b'\n' {
            self.pos += 1;
        }
        self.comments.push(Comment { span: Span::new(lo, self.pos), own_line });
    }

    fn ident(&mut self) {
        let lo = self.pos;
        while self.pos < self.end && is_ident_char(self.s[self.pos]) {
            self.pos += 1;
        }
        let word = &self.text[lo..self.pos];
        let tok = if word == "_" {
            Tok::P(P::Underscore)
        } else if let Some(k) = Kw::from_str(word) {
            Tok::Kw(k)
        } else {
            Tok::Ident(word.to_string())
        };
        self.push(tok, lo, self.pos);
    }

    fn number(&mut self) {
        let lo = self.pos;
        let after_dot = matches!(self.toks.last(), Some(Token { tok: Tok::P(P::Dot), .. }));
        let c1 = self.peek_at(self.pos + 1);
        if self.s[self.pos] == b'0' && (c1 == b'x' || c1 == b'b') {
            self.pos += 2;
            let hex = c1 == b'x';
            while self.pos < self.end
                && (self.s[self.pos] == b'_'
                    || if hex { self.s[self.pos].is_ascii_hexdigit() } else { matches!(self.s[self.pos], b'0' | b'1') })
            {
                self.pos += 1;
            }
            self.finish_number(lo, Tok::Int(self.text[lo..self.pos].to_string()));
            return;
        }
        self.digits();
        let mut float = false;
        if !after_dot && self.peek_at(self.pos) == b'.' && self.peek_at(self.pos + 1).is_ascii_digit() {
            float = true;
            self.pos += 1;
            self.digits();
        }
        if !after_dot && matches!(self.peek_at(self.pos), b'e' | b'E') {
            let n = self.peek_at(self.pos + 1);
            let sign = n == b'+' || n == b'-';
            if n.is_ascii_digit() || (sign && self.peek_at(self.pos + 2).is_ascii_digit()) {
                float = true;
                self.pos += if sign { 2 } else { 1 };
                self.digits();
            }
        }
        if float {
            self.finish_number(lo, Tok::Float(self.text[lo..self.pos].to_string()));
            return;
        }
        for unit in ["ns", "us", "ms", "s", "m", "h"] {
            if self.starts_with(self.pos, unit) && !is_ident_char(self.peek_at(self.pos + unit.len())) {
                self.pos += unit.len();
                self.push(Tok::Dur(self.text[lo..self.pos].to_string()), lo, self.pos);
                return;
            }
        }
        self.finish_number(lo, Tok::Int(self.text[lo..self.pos].to_string()));
    }

    fn digits(&mut self) {
        while self.pos < self.end && (self.s[self.pos].is_ascii_digit() || self.s[self.pos] == b'_') {
            self.pos += 1;
        }
    }

    fn finish_number(&mut self, lo: usize, tok: Tok) {
        if is_ident_char(self.peek_at(self.pos)) {
            let bad = self.pos;
            while self.pos < self.end && is_ident_char(self.s[self.pos]) {
                self.pos += 1;
            }
            self.err(bad, self.pos, format!("unexpected `{}` after a number", &self.text[bad..self.pos]));
        }
        let hi = self.pos;
        self.push(tok, lo, hi);
    }

    fn byte_lit(&mut self) {
        let lo = self.pos;
        self.pos += 1;
        let inner = self.pos;
        match self.peek_at(self.pos) {
            b'\\' => {
                self.pos += if self.peek_at(self.pos + 1) == b'x' { 4 } else { 2 };
            }
            c if c.is_ascii() && c != b'\'' && c != b'\n' && c != 0 => self.pos += 1,
            _ => {
                self.err(lo, self.pos + 1, "a byte literal holds one ASCII character, like 'a' or '\\n'");
                while self.pos < self.end && self.s[self.pos] != b'\'' && self.s[self.pos] != b'\n' {
                    self.pos += 1;
                }
            }
        }
        let text = self.text[inner..self.pos.min(self.end)].to_string();
        if self.peek_at(self.pos) == b'\'' {
            self.pos += 1;
        } else {
            self.err(lo, self.pos, "unterminated byte literal; expected `'`");
        }
        self.push(Tok::Byte(text), lo, self.pos);
    }

    fn string(&mut self) {
        let lo = self.pos;
        let triple = self.starts_with(self.pos, "\"\"\"");
        self.pos += if triple { 3 } else { 1 };
        let mut segs = Vec::new();
        let mut text_start = self.pos;
        loop {
            if self.pos >= self.end {
                self.err(lo, self.pos, "unterminated string");
                break;
            }
            let c = self.s[self.pos];
            if triple && self.starts_with(self.pos, "\"\"\"") {
                segs.push(StrSeg::Text(self.text[text_start..self.pos].to_string()));
                self.pos += 3;
                break;
            }
            if !triple && c == b'"' {
                segs.push(StrSeg::Text(self.text[text_start..self.pos].to_string()));
                self.pos += 1;
                break;
            }
            if !triple && c == b'\n' {
                self.err(lo, self.pos, "unterminated string; use \"\"\" for strings that span lines");
                segs.push(StrSeg::Text(self.text[text_start..self.pos].to_string()));
                break;
            }
            if c == b'\\' {
                if triple {
                    self.pos += if self.peek_at(self.pos + 1) == b'$' { 2 } else { 1 };
                } else {
                    let e = self.peek_at(self.pos + 1);
                    if !matches!(e, b'n' | b't' | b'r' | b'0' | b'\\' | b'"' | b'\'' | b'$' | b'x' | b'u') {
                        self.err(self.pos, self.pos + 2, "unknown escape; use \\n \\t \\r \\0 \\\\ \\\" \\$ \\xHH or \\u{...}");
                    }
                    self.pos += 2;
                }
                continue;
            }
            if c == b'$' && self.peek_at(self.pos + 1) == b'{' {
                segs.push(StrSeg::Text(self.text[text_start..self.pos].to_string()));
                let inner = self.pos + 2;
                match self.interp_end(inner) {
                    Some(close) => {
                        let mut sub = Lexer::new(self.text, inner, close, true);
                        sub.run();
                        self.diags.append(&mut sub.diags);
                        self.comments.append(&mut sub.comments);
                        segs.push(StrSeg::Interp(sub.toks, Span::new(inner, close)));
                        self.pos = close + 1;
                    }
                    None => {
                        self.err(self.pos, self.end, "unterminated `${` in string");
                        self.pos = self.end;
                        break;
                    }
                }
                text_start = self.pos;
                continue;
            }
            self.pos += 1;
        }
        self.push(Tok::Str { triple, segs }, lo, self.pos);
    }

    /// Position of the `}` closing an interpolation that starts at `p`.
    fn interp_end(&self, mut p: usize) -> Option<usize> {
        let mut depth = 0;
        while p < self.end {
            match self.s[p] {
                b'{' => depth += 1,
                b'}' if depth == 0 => return Some(p),
                b'}' => depth -= 1,
                b'"' => {
                    p = self.skip_string(p)?;
                    continue;
                }
                b'\'' => {
                    p += if self.peek_at(p + 1) == b'\\' { 4 } else { 3 };
                    continue;
                }
                _ => {}
            }
            p += 1;
        }
        None
    }

    /// Position just after the string starting at `p`.
    fn skip_string(&self, mut p: usize) -> Option<usize> {
        let triple = self.starts_with(p, "\"\"\"");
        p += if triple { 3 } else { 1 };
        while p < self.end {
            if triple && self.starts_with(p, "\"\"\"") {
                return Some(p + 3);
            }
            match self.s[p] {
                b'"' if !triple => return Some(p + 1),
                b'\\' => p += 2,
                b'$' if self.peek_at(p + 1) == b'{' => p = self.interp_end(p + 2)? + 1,
                _ => p += 1,
            }
        }
        None
    }

    fn punct(&mut self) {
        let lo = self.pos;
        let found = PUNCTS.iter().find(|(text, _)| self.starts_with(lo, text));
        let Some(&(text, p)) = found else {
            let ch = self.text[lo..].chars().next().unwrap();
            self.pos += ch.len_utf8();
            self.err(lo, self.pos, format!("unexpected character `{ch}`"));
            return;
        };
        self.pos += text.len();
        match p {
            P::LParen => self.stack.push(b'('),
            P::LBracket => self.stack.push(b'['),
            P::LBrace => self.stack.push(b'{'),
            P::RParen | P::RBracket | P::RBrace => {
                self.stack.pop();
            }
            _ => {}
        }
        self.push(Tok::P(p), lo, self.pos);
    }
}
