//! Parser: recursive descent, with precedence climbing for binary operators.
//!
//! Errors name what was expected, and common habits from other languages
//! (`;`, `::`, `let mut`, `&x`, `Vec<T>`) get a message saying what to write instead.

use crate::ast::*;
use crate::diag::Diag;
use crate::lexer::{Kw, P, StrSeg, Tok, Token};
use crate::source::{Source, Span};

type R<T> = Result<T, Diag>;

/// Parses a file. With `allow_snippet`, a file that doesn't start with a
/// declaration is parsed as a list of statements (used for code examples).
pub fn parse(src: &Source, tokens: Vec<Token>, allow_snippet: bool) -> (File, Vec<Diag>) {
    let mut p = Parser::new(src, tokens);
    p.skip_newlines();
    let mut file = File::default();
    if allow_snippet && !p.at_item_start() && !p.at_eof() {
        while !p.at_eof() {
            match p.stmt().and_then(|s| p.end_line().map(|_| s)) {
                Ok(s) => file.stmts.push(s),
                Err(d) => {
                    p.diags.push(d);
                    break;
                }
            }
            p.skip_newlines();
        }
        return (file, p.diags);
    }
    while !p.at_eof() {
        match p.item() {
            Ok(it) => file.items.push(it),
            Err(d) => {
                p.diags.push(d);
                p.recover_item();
            }
        }
        p.skip_newlines();
    }
    (file, p.diags)
}

pub fn describe(t: &Tok) -> String {
    match t {
        Tok::Ident(n) => format!("`{n}`"),
        Tok::Kw(k) => format!("`{}`", k.text()),
        Tok::Int(s) | Tok::Float(s) | Tok::Dur(s) => format!("`{s}`"),
        Tok::Byte(_) => "a byte literal".into(),
        Tok::Str { .. } => "a string".into(),
        Tok::P(p) => format!("`{}`", p.text()),
        Tok::Newline => "the end of the line".into(),
        Tok::Eof => "the end of the file".into(),
    }
}

fn binop(p: P) -> Option<BinOp> {
    Some(match p {
        P::Plus => BinOp::Add,
        P::Minus => BinOp::Sub,
        P::Star => BinOp::Mul,
        P::Slash => BinOp::Div,
        P::Percent => BinOp::Rem,
        P::PlusW => BinOp::AddW,
        P::MinusW => BinOp::SubW,
        P::StarW => BinOp::MulW,
        P::EqEq => BinOp::Eq,
        P::Ne => BinOp::Ne,
        P::Lt => BinOp::Lt,
        P::Le => BinOp::Le,
        P::Gt => BinOp::Gt,
        P::Ge => BinOp::Ge,
        P::AndAnd => BinOp::And,
        P::OrOr => BinOp::Or,
        P::Amp => BinOp::BitAnd,
        P::Pipe => BinOp::BitOr,
        P::Caret => BinOp::BitXor,
        P::Shl => BinOp::Shl,
        P::Shr => BinOp::Shr,
        _ => return None,
    })
}

fn assign_op(p: P) -> Option<AssignOp> {
    Some(match p {
        P::Eq => AssignOp::Set,
        P::PlusEq => AssignOp::Add,
        P::MinusEq => AssignOp::Sub,
        P::StarEq => AssignOp::Mul,
        P::SlashEq => AssignOp::Div,
        P::PercentEq => AssignOp::Rem,
        _ => return None,
    })
}

fn is_effect_name(name: &str) -> bool {
    name == "io" || name == "fail" || (!name.is_empty() && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()))
}

struct Parser<'a> {
    src: &'a Source,
    toks: Vec<Token>,
    pos: usize,
    last_hi: u32,
    diags: Vec<Diag>,
}

impl<'a> Parser<'a> {
    fn new(src: &'a Source, toks: Vec<Token>) -> Parser<'a> {
        Parser { src, toks, pos: 0, last_hi: 0, diags: Vec::new() }
    }

    // ---- token helpers ----

    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek_n(&self, n: usize) -> &Tok {
        let i = (self.pos + n).min(self.toks.len() - 1);
        &self.toks[i].tok
    }

    fn span(&self) -> Span {
        self.toks[self.pos].span
    }

    fn bump(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        self.last_hi = t.span.hi;
        t
    }

    fn at(&self, p: P) -> bool {
        *self.peek() == Tok::P(p)
    }

    fn at_kw(&self, k: Kw) -> bool {
        *self.peek() == Tok::Kw(k)
    }

    fn at_ident(&self, word: &str) -> bool {
        matches!(self.peek(), Tok::Ident(n) if n == word)
    }

    fn at_eof(&self) -> bool {
        *self.peek() == Tok::Eof
    }

    fn eat(&mut self, p: P) -> bool {
        if self.at(p) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn eat_kw(&mut self, k: Kw) -> bool {
        if self.at_kw(k) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn line(&self, pos: u32) -> u32 {
        self.src.line(pos)
    }

    fn err_here(&self, msg: impl Into<String>) -> Diag {
        Diag::new(self.span(), msg)
    }

    fn expected(&self, what: &str) -> Diag {
        self.err_here(format!("expected {what}, found {}", describe(self.peek())))
    }

    fn expect(&mut self, p: P) -> R<Span> {
        if self.at(p) {
            Ok(self.bump().span)
        } else {
            Err(self.expected(&format!("`{}`", p.text())))
        }
    }

    fn expect_kw(&mut self, k: Kw) -> R<Span> {
        if self.at_kw(k) {
            Ok(self.bump().span)
        } else {
            Err(self.expected(&format!("`{}`", k.text())))
        }
    }

    fn ident(&mut self, what: &str) -> R<Ident> {
        match self.peek().clone() {
            Tok::Ident(name) => {
                let span = self.bump().span;
                Ok(Ident { name, span })
            }
            Tok::Kw(k) => Err(self.err_here(format!("expected {what}, found the keyword `{}`; pick another name", k.text()))),
            _ => Err(self.expected(what)),
        }
    }

    fn skip_newlines(&mut self) {
        while *self.peek() == Tok::Newline {
            self.bump();
        }
    }

    /// The end of a statement or declaration: a newline, or a closing `}` or the end of the file.
    fn end_line(&mut self) -> R<()> {
        match self.peek() {
            Tok::Newline => {
                self.bump();
                Ok(())
            }
            Tok::Eof | Tok::P(P::RBrace) => Ok(()),
            Tok::P(P::Semi) => {
                let span = self.bump().span;
                self.diags.push(Diag::new(span, "Overt has no semicolons; remove the `;`"));
                if *self.peek() == Tok::Newline {
                    self.bump();
                }
                Ok(())
            }
            _ => Err(self.expected("the end of the line")),
        }
    }

    fn at_item_start(&self) -> bool {
        match self.peek() {
            Tok::Kw(Kw::Fn | Kw::Type | Kw::Enum | Kw::Const | Kw::Extern) => true,
            Tok::Kw(Kw::Unsafe) => *self.peek_n(1) == Tok::Kw(Kw::Fn),
            Tok::Ident(w) => {
                (w == "test" && matches!(self.peek_n(1), Tok::Str { .. }))
                    || (w == "drop" && matches!(self.peek_n(1), Tok::Ident(_)))
                    || (w == "blocking" && matches!(self.peek_n(1), Tok::Kw(Kw::Extern | Kw::Fn)))
            }
            _ => false,
        }
    }

    fn recover_item(&mut self) {
        while !self.at_eof() {
            let t = self.bump();
            if t.tok == Tok::Newline && self.at_item_start() {
                break;
            }
        }
    }

    // ---- declarations ----

    fn item(&mut self) -> R<Item> {
        let lo = self.span().lo;
        let kind = match self.peek().clone() {
            Tok::Kw(Kw::Const) => self.const_decl()?,
            Tok::Kw(Kw::Type) => ItemKind::Type(self.type_decl(false)?),
            Tok::Kw(Kw::Enum) => self.enum_decl()?,
            Tok::Kw(Kw::Fn) | Tok::Kw(Kw::Unsafe) => ItemKind::Fn(self.fn_decl(false, false)?),
            Tok::Kw(Kw::Extern) => self.extern_decl(false)?,
            Tok::Ident(w) if w == "blocking" => {
                self.bump();
                if !self.at_kw(Kw::Extern) {
                    return Err(self.err_here("`blocking` goes before `extern`, or before a `fn` inside an `extern` block"));
                }
                self.extern_decl(true)?
            }
            Tok::Ident(w) if w == "drop" => self.drop_decl()?,
            Tok::Ident(w) if w == "test" => self.test_decl()?,
            Tok::Kw(Kw::Let | Kw::Var) => {
                return Err(self.err_here("there are no global variables; use `const` for a constant, or pass a `Shared[T]` as a parameter"));
            }
            t => {
                return Err(self.err_here(format!(
                    "expected a declaration (`fn`, `type`, `enum`, `const`, `extern`, `drop` or `test`), found {}",
                    describe(&t)
                )));
            }
        };
        let span = Span { lo, hi: self.last_hi };
        self.end_line()?;
        Ok(Item { kind, span })
    }

    fn const_decl(&mut self) -> R<ItemKind> {
        self.bump();
        let name = self.ident("a constant name")?;
        let ty = if self.eat(P::Colon) { Some(self.ty()?) } else { None };
        self.expect(P::Eq)?;
        let value = self.expr(0)?;
        Ok(ItemKind::Const(ConstDecl { name, ty, value }))
    }

    fn type_decl(&mut self, in_extern: bool) -> R<TypeDecl> {
        self.bump();
        let name = self.ident("a type name")?;
        let generics = self.generics()?;
        let body = if self.eat(P::Eq) {
            TypeBody::Alias(self.ty()?)
        } else if self.at(P::LBrace) {
            let (fields, one_line) = self.fields(P::LBrace, P::RBrace)?;
            TypeBody::Struct { fields, one_line }
        } else if in_extern {
            TypeBody::Opaque
        } else {
            return Err(self.expected("`{` with fields, or `=` and a type"));
        };
        Ok(TypeDecl { name, generics, body })
    }

    /// Fields separated by commas or newlines, between `open` and `close`.
    fn fields(&mut self, open: P, close: P) -> R<(Vec<Field>, bool)> {
        let lo = self.expect(open)?;
        self.skip_newlines();
        let mut fields = Vec::new();
        while !self.at(close) {
            let name = self.ident("a field name")?;
            self.expect(P::Colon)?;
            let ty = self.ty()?;
            let default = if self.eat(P::Eq) { Some(self.expr(0)?) } else { None };
            let span = name.span.to(Span { lo: name.span.lo, hi: self.last_hi });
            fields.push(Field { name, ty, default, span });
            if !self.list_sep(close)? {
                break;
            }
        }
        let hi = self.expect(close)?;
        Ok((fields, self.line(lo.lo) == self.line(hi.lo)))
    }

    /// After a list element: consumes a `,` or newlines. Returns whether another element may follow.
    fn list_sep(&mut self, close: P) -> R<bool> {
        let comma = self.eat(P::Comma);
        let newline = *self.peek() == Tok::Newline;
        self.skip_newlines();
        if self.at(close) {
            return Ok(false);
        }
        if comma || newline {
            Ok(true)
        } else {
            Err(self.expected(&format!("`,`, a new line or `{}`", close.text())))
        }
    }

    fn enum_decl(&mut self) -> R<ItemKind> {
        self.bump();
        let name = self.ident("an enum name")?;
        let generics = self.generics()?;
        let lo = self.expect(P::LBrace)?;
        self.skip_newlines();
        let mut variants = Vec::new();
        while !self.at(P::RBrace) {
            let vname = self.ident("a variant name")?;
            let fields = if self.at(P::LParen) { Some(self.fields(P::LParen, P::RParen)?.0) } else { None };
            let span = Span { lo: vname.span.lo, hi: self.last_hi };
            variants.push(Variant { name: vname, fields, span });
            if !self.list_sep(P::RBrace)? {
                break;
            }
        }
        let hi = self.expect(P::RBrace)?;
        let one_line = self.line(lo.lo) == self.line(hi.lo);
        Ok(ItemKind::Enum(EnumDecl { name, generics, variants, one_line }))
    }

    fn generics(&mut self) -> R<Vec<GenericParam>> {
        let mut out = Vec::new();
        if !self.eat(P::LBracket) {
            return Ok(out);
        }
        while !self.at(P::RBracket) {
            if self.eat(P::Bang) {
                out.push(GenericParam::Effect { name: self.ident("an effect parameter name")? });
            } else {
                let name = self.ident("a type parameter name")?;
                let mut bounds = Vec::new();
                if self.eat(P::Colon) {
                    bounds.push(self.ident("a constraint (`Eq`, `Ord` or `Hash`)")?);
                    while self.eat(P::Plus) {
                        bounds.push(self.ident("a constraint")?);
                    }
                }
                out.push(GenericParam::Type { name, bounds });
            }
            if !self.eat(P::Comma) {
                break;
            }
        }
        self.expect(P::RBracket)?;
        Ok(out)
    }

    fn fn_decl(&mut self, in_extern: bool, blocking: bool) -> R<FnDecl> {
        let lo = self.span().lo;
        let is_unsafe = self.eat_kw(Kw::Unsafe);
        self.expect_kw(Kw::Fn)?;
        let first = self.ident("a function name")?;
        let first_generics = self.generics()?;
        let (recv, name, generics) = if self.eat(P::Dot) {
            let name = self.ident("a method name")?;
            let generics = self.generics()?;
            (Some((first, first_generics)), name, generics)
        } else {
            (None, first, first_generics)
        };
        let params = self.params()?;
        let ret = if self.eat(P::Arrow) { Some(self.ty()?) } else { None };
        if self.at(P::Colon) {
            return Err(self.err_here("the return type goes after `->`, like `fn f(x: int) -> int`"));
        }
        let effects = self.effects()?;
        let sig_span = Span { lo, hi: self.last_hi };
        let mut f = FnDecl {
            is_unsafe,
            blocking,
            recv,
            name,
            generics,
            params,
            ret,
            effects,
            pre: Vec::new(),
            ex: Vec::new(),
            body: None,
            sig_span,
        };
        if in_extern {
            return Ok(f);
        }
        loop {
            self.skip_newlines();
            if self.at_ident("pre") {
                self.bump();
                f.pre.push(self.expr(0)?);
            } else if self.at_ident("ex") {
                let elo = self.bump().span.lo;
                let expr = self.expr(0)?;
                let fails = if self.at_ident("fails") {
                    self.bump();
                    if self.at(P::Dot) || matches!(self.peek(), Tok::Ident(_)) {
                        let kind = self.primary()?;
                        Some(Some(self.postfix(kind)?))
                    } else {
                        Some(None)
                    }
                } else {
                    None
                };
                f.ex.push(Example { expr, fails, span: Span { lo: elo, hi: self.last_hi } });
            } else if self.at(P::LBrace) {
                f.body = Some(self.block()?);
                return Ok(f);
            } else {
                return Err(self.expected(&format!("the body `{{ ... }}` of `fn {}`", f.name.name)));
            }
            if !matches!(self.peek(), Tok::Newline) {
                return Err(self.expected("the end of the line"));
            }
        }
    }

    fn params(&mut self) -> R<Vec<Param>> {
        self.expect(P::LParen)?;
        let mut out = Vec::new();
        while !self.at(P::RParen) {
            let lo = self.span().lo;
            let self_mode = match (self.peek(), self.peek_n(1)) {
                (Tok::Kw(Kw::SelfKw), _) => Some(Mode::Read),
                (Tok::Kw(Kw::Inout), Tok::Kw(Kw::SelfKw)) => Some(Mode::Inout),
                (Tok::Kw(Kw::Sink), Tok::Kw(Kw::SelfKw)) => Some(Mode::Sink),
                _ => None,
            };
            if let Some(mode) = self_mode {
                if mode != Mode::Read {
                    self.bump();
                }
                let span = self.bump().span;
                out.push(Param { mode, name: Ident { name: "self".into(), span }, ty: None, default: None, span: Span { lo, hi: span.hi } });
            } else {
                if self.at_kw(Kw::Inout) || self.at_kw(Kw::Sink) {
                    return Err(self.err_here("the mode goes after the colon: `x: inout T`"));
                }
                if self.at(P::Amp) {
                    return Err(self.err_here("Overt has no references; write `x: T` to read, or `x: inout T` to change the caller's variable"));
                }
                let name = self.ident("a parameter name")?;
                self.expect(P::Colon)?;
                let mode = if self.eat_kw(Kw::Inout) {
                    Mode::Inout
                } else if self.eat_kw(Kw::Sink) {
                    Mode::Sink
                } else {
                    Mode::Read
                };
                let ty = Some(self.ty()?);
                let default = if self.eat(P::Eq) { Some(self.expr(0)?) } else { None };
                out.push(Param { mode, name, ty, default, span: Span { lo, hi: self.last_hi } });
            }
            if !self.eat(P::Comma) {
                break;
            }
        }
        self.expect(P::RParen)?;
        Ok(out)
    }

    fn effects(&mut self) -> R<Vec<Ident>> {
        let mut out = Vec::new();
        if !self.eat(P::Bang) {
            return Ok(out);
        }
        loop {
            out.push(self.ident("an effect (`io` or `fail`)")?);
            let more = self.at(P::Comma)
                && matches!(self.peek_n(1), Tok::Ident(n) if is_effect_name(n))
                && *self.peek_n(2) != Tok::P(P::Colon);
            if !more {
                return Ok(out);
            }
            self.bump();
        }
    }

    fn extern_decl(&mut self, blocking: bool) -> R<ItemKind> {
        self.expect_kw(Kw::Extern)?;
        let lib = self.str_lit()?;
        let header = if self.at_ident("header") {
            self.bump();
            Some(self.str_lit()?)
        } else {
            None
        };
        let mut items = None;
        if self.eat(P::LBrace) {
            let mut list = Vec::new();
            self.skip_newlines();
            while !self.at(P::RBrace) {
                let lo = self.span().lo;
                let kind = if self.at_kw(Kw::Type) {
                    ItemKind::Type(self.type_decl(true)?)
                } else if self.at_ident("blocking") {
                    self.bump();
                    ItemKind::Fn(self.fn_decl(true, true)?)
                } else if self.at_kw(Kw::Fn) {
                    ItemKind::Fn(self.fn_decl(true, false)?)
                } else {
                    return Err(self.expected("`type`, `fn` or `blocking fn` inside `extern`"));
                };
                list.push(Item { kind, span: Span { lo, hi: self.last_hi } });
                self.end_line()?;
                self.skip_newlines();
            }
            self.expect(P::RBrace)?;
            items = Some(list);
        }
        Ok(ItemKind::Extern(ExternDecl { blocking, lib, header, items }))
    }

    fn drop_decl(&mut self) -> R<ItemKind> {
        self.bump();
        let ty = self.ty()?;
        let body = self.block()?;
        Ok(ItemKind::Drop(DropDecl { ty, body }))
    }

    fn test_decl(&mut self) -> R<ItemKind> {
        self.bump();
        let name = self.str_lit()?;
        let effects = self.effects()?;
        let body = self.block()?;
        Ok(ItemKind::Test(TestDecl { name, effects, body }))
    }

    fn str_lit(&mut self) -> R<StrLit> {
        let Tok::Str { triple, segs } = self.peek().clone() else {
            return Err(self.expected("a string"));
        };
        let span = self.bump().span;
        let mut parts = Vec::new();
        for seg in segs {
            match seg {
                StrSeg::Text(t) => {
                    if !t.is_empty() {
                        parts.push(StrPart::Text(t));
                    }
                }
                StrSeg::Interp(toks, ispan) => parts.push(StrPart::Interp(Box::new(self.interp(toks, ispan)?))),
            }
        }
        Ok(StrLit { triple, parts, span })
    }

    fn interp(&mut self, mut toks: Vec<Token>, span: Span) -> R<Expr> {
        if toks.is_empty() {
            return Err(Diag::new(span, "empty `${}` in string"));
        }
        toks.push(Token { tok: Tok::Eof, span: Span { lo: span.hi, hi: span.hi } });
        let mut sub = Parser::new(self.src, toks);
        let e = sub.expr(0)?;
        if !sub.at_eof() {
            return Err(sub.expected("`}` to end the `${...}`"));
        }
        self.diags.append(&mut sub.diags);
        Ok(e)
    }

    // ---- types ----

    fn ty(&mut self) -> R<TypeExpr> {
        let lo = self.span().lo;
        let kind = match self.peek().clone() {
            Tok::P(P::Question) => {
                self.bump();
                TypeKind::Optional(Box::new(self.ty()?))
            }
            Tok::P(P::Star) => {
                self.bump();
                TypeKind::Ptr(Box::new(self.ty()?))
            }
            Tok::P(P::Amp) => {
                return Err(self.err_here("Overt has no references; use the type itself, and `inout` on a parameter to change it"));
            }
            Tok::P(P::LBracket) => {
                self.bump();
                let elem = self.ty()?;
                if self.eat(P::Semi) {
                    let n = self.expr(0)?;
                    self.expect(P::RBracket)?;
                    TypeKind::Fixed(Box::new(elem), Box::new(n))
                } else {
                    self.expect(P::RBracket)?;
                    TypeKind::Array(Box::new(elem))
                }
            }
            Tok::P(P::LParen) => {
                self.bump();
                let mut items = Vec::new();
                while !self.at(P::RParen) {
                    items.push(self.ty()?);
                    if !self.eat(P::Comma) {
                        break;
                    }
                }
                self.expect(P::RParen)?;
                TypeKind::Tuple(items)
            }
            Tok::Kw(Kw::Fn) => {
                self.bump();
                self.expect(P::LParen)?;
                let mut params = Vec::new();
                while !self.at(P::RParen) {
                    params.push(self.ty()?);
                    if !self.eat(P::Comma) {
                        break;
                    }
                }
                self.expect(P::RParen)?;
                let ret = if self.eat(P::Arrow) { Some(Box::new(self.ty()?)) } else { None };
                let effects = self.effects()?;
                TypeKind::Fn { params, ret, effects }
            }
            Tok::Ident(_) => {
                let mut path = vec![self.ident("a type")?];
                while self.at(P::Dot) && matches!(self.peek_n(1), Tok::Ident(_)) {
                    self.bump();
                    path.push(self.ident("a type")?);
                }
                let mut args = Vec::new();
                if self.at(P::Lt) {
                    return Err(self.err_here(format!(
                        "type arguments use brackets, like `{}[int]`; arrays are `[T]` and optionals `?T`",
                        path[0].name
                    )));
                }
                if self.eat(P::LBracket) {
                    while !self.at(P::RBracket) {
                        args.push(self.ty()?);
                        if !self.eat(P::Comma) {
                            break;
                        }
                    }
                    self.expect(P::RBracket)?;
                }
                TypeKind::Named { path, args }
            }
            _ => return Err(self.expected("a type")),
        };
        Ok(TypeExpr { kind, span: Span { lo, hi: self.last_hi } })
    }

    // ---- statements ----

    fn block(&mut self) -> R<Block> {
        let lo = self.expect(P::LBrace)?.lo;
        self.skip_newlines();
        let mut stmts = Vec::new();
        while !self.at(P::RBrace) {
            if self.at_eof() {
                return Err(self.err_here("missing `}`"));
            }
            stmts.push(self.stmt()?);
            self.end_line()?;
            self.skip_newlines();
        }
        let hi = self.expect(P::RBrace)?.hi;
        Ok(Block { stmts, span: Span { lo, hi } })
    }

    fn stmt(&mut self) -> R<Stmt> {
        let lo = self.span().lo;
        let kind = match self.peek() {
            Tok::Kw(Kw::Let) | Tok::Kw(Kw::Var) => {
                let mutable = self.bump().tok == Tok::Kw(Kw::Var);
                if self.at_ident("mut") {
                    return Err(self.err_here("Overt has no `mut`; declare a variable you change with `var x = ...`"));
                }
                let pat = self.pattern()?;
                let ty = if self.eat(P::Colon) { Some(self.ty()?) } else { None };
                self.expect(P::Eq)?;
                let value = self.expr(0)?;
                StmtKind::Let { mutable, pat, ty, value }
            }
            Tok::Kw(Kw::For) => {
                self.bump();
                let inout = self.eat_kw(Kw::Inout);
                let mut pats = vec![self.pattern()?];
                if self.eat(P::Comma) {
                    pats.push(self.pattern()?);
                }
                self.expect_kw(Kw::In)?;
                let iter = self.expr(0)?;
                let body = self.block()?;
                StmtKind::For { inout, pats, iter, body }
            }
            Tok::Kw(Kw::While) => {
                self.bump();
                let cond = self.cond()?;
                let body = self.block()?;
                StmtKind::While { cond, body }
            }
            Tok::Kw(Kw::Par) => {
                self.bump();
                StmtKind::Par(self.block()?)
            }
            _ => {
                let target = self.expr(0)?;
                if let Tok::P(p) = self.peek() {
                    if let Some(op) = assign_op(*p) {
                        self.bump();
                        let value = self.expr(0)?;
                        return Ok(Stmt {
                            kind: StmtKind::Assign { target, op, value },
                            span: Span { lo, hi: self.last_hi },
                        });
                    }
                }
                StmtKind::Expr(target)
            }
        };
        Ok(Stmt { kind, span: Span { lo, hi: self.last_hi } })
    }

    fn cond(&mut self) -> R<Cond> {
        if self.eat_kw(Kw::Let) {
            let name = self.ident("a name to bind")?;
            self.expect(P::Eq)?;
            let value = self.expr(0)?;
            Ok(Cond::Let { name, value })
        } else {
            Ok(Cond::Expr(self.expr(0)?))
        }
    }

    // ---- expressions ----

    fn expr(&mut self, min: u8) -> R<Expr> {
        let mut lhs = self.unary()?;
        loop {
            match self.peek().clone() {
                Tok::Kw(Kw::Else) if min <= PREC_ELSE => {
                    self.bump();
                    let alt = if self.at(P::LBrace) {
                        let b = self.block()?;
                        Expr { span: b.span, kind: ExprKind::Block(b) }
                    } else {
                        self.expr(PREC_ELSE)?
                    };
                    let span = lhs.span.to(alt.span);
                    lhs = Expr { kind: ExprKind::Else(Box::new(lhs), Box::new(alt)), span };
                }
                Tok::Ident(w) if w == "catch" && min <= PREC_ELSE => {
                    self.bump();
                    let name = self.ident("a name for the error, like `catch e { ... }`")?;
                    let body = self.block()?;
                    let span = lhs.span.to(body.span);
                    lhs = Expr { kind: ExprKind::Catch { expr: Box::new(lhs), name, body }, span };
                }
                Tok::P(P::DotDot) | Tok::P(P::DotDotEq) if min <= PREC_RANGE => {
                    let inclusive = self.bump().tok == Tok::P(P::DotDotEq);
                    let hi = if self.range_end_follows() { None } else { Some(Box::new(self.expr(PREC_RANGE + 1)?)) };
                    let span = Span { lo: lhs.span.lo, hi: self.last_hi };
                    lhs = Expr { kind: ExprKind::Range { lo: Some(Box::new(lhs)), hi, inclusive }, span };
                }
                Tok::P(p) => {
                    let Some(op) = binop(p) else { break };
                    let prec = op.prec();
                    if prec < min {
                        break;
                    }
                    let op_span = self.bump().span;
                    let rhs = self.expr(prec + 1)?;
                    if op.is_comparison() {
                        if let ExprKind::Binary(prev, ..) = &lhs.kind {
                            if prev.is_comparison() {
                                return Err(Diag::new(op_span, "comparisons don't chain; write `a < b && b < c`"));
                            }
                        }
                    }
                    let span = lhs.span.to(rhs.span);
                    lhs = Expr { kind: ExprKind::Binary(op, Box::new(lhs), Box::new(rhs)), span };
                }
                _ => break,
            }
        }
        Ok(lhs)
    }

    fn range_end_follows(&self) -> bool {
        matches!(
            self.peek(),
            Tok::P(P::RBracket | P::RParen | P::Comma | P::LBrace | P::RBrace) | Tok::Newline | Tok::Eof
        )
    }

    fn unary(&mut self) -> R<Expr> {
        let lo = self.span().lo;
        let op = match self.peek() {
            Tok::P(P::Minus) => Some(UnOp::Neg),
            Tok::P(P::Bang) => Some(UnOp::Not),
            Tok::P(P::Amp) => {
                return Err(self.err_here("Overt has no references; pass `inout x` to let a function change `x`, or just `x` to read it"));
            }
            Tok::P(P::DotDot) | Tok::P(P::DotDotEq) => {
                let inclusive = self.bump().tok == Tok::P(P::DotDotEq);
                let hi = if self.range_end_follows() { None } else { Some(Box::new(self.expr(PREC_RANGE + 1)?)) };
                return Ok(Expr { kind: ExprKind::Range { lo: None, hi, inclusive }, span: Span { lo, hi: self.last_hi } });
            }
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let e = self.unary()?;
            let span = Span { lo, hi: e.span.hi };
            return Ok(Expr { kind: ExprKind::Unary(op, Box::new(e)), span });
        }
        let e = self.primary()?;
        self.postfix(e)
    }

    fn postfix(&mut self, mut e: Expr) -> R<Expr> {
        loop {
            match self.peek().clone() {
                Tok::P(P::LParen) => {
                    let (args, multiline) = self.args()?;
                    let span = Span { lo: e.span.lo, hi: self.last_hi };
                    e = Expr { kind: ExprKind::Call { callee: Box::new(e), args, multiline }, span };
                }
                Tok::P(P::LBracket) => {
                    self.bump();
                    let mut args = Vec::new();
                    while !self.at(P::RBracket) {
                        let a = if matches!(self.peek(), Tok::P(P::Question | P::Star) | Tok::Kw(Kw::Fn)) {
                            let t = self.ty()?;
                            Expr { span: t.span, kind: ExprKind::Type(t) }
                        } else {
                            self.expr(0)?
                        };
                        args.push(a);
                        if !self.eat(P::Comma) {
                            break;
                        }
                    }
                    self.expect(P::RBracket)?;
                    let span = Span { lo: e.span.lo, hi: self.last_hi };
                    e = Expr { kind: ExprKind::Index { base: Box::new(e), args }, span };
                }
                Tok::P(P::Dot) => {
                    self.bump();
                    let name = match self.peek().clone() {
                        Tok::Ident(n) | Tok::Int(n) => {
                            let span = self.bump().span;
                            Ident { name: n, span }
                        }
                        _ => return Err(self.expected("a field or method name after `.`")),
                    };
                    let span = Span { lo: e.span.lo, hi: name.span.hi };
                    e = Expr { kind: ExprKind::Field(Box::new(e), name), span };
                }
                Tok::P(P::Question) => {
                    let hi = self.bump().span.hi;
                    let span = Span { lo: e.span.lo, hi };
                    e = Expr { kind: ExprKind::Try(Box::new(e)), span };
                }
                Tok::P(P::Colon) if *self.peek_n(1) == Tok::P(P::Colon) => {
                    return Err(self.err_here("use `.` instead of `::`, like `Shape.Circle` or `str.find`"));
                }
                _ => return Ok(e),
            }
        }
    }

    fn args(&mut self) -> R<(Vec<Arg>, bool)> {
        let open = self.expect(P::LParen)?;
        let mut args = Vec::new();
        while !self.at(P::RParen) {
            let lo = self.span().lo;
            let name = if matches!(self.peek(), Tok::Ident(_)) && *self.peek_n(1) == Tok::P(P::Colon) {
                let n = self.ident("an argument name")?;
                self.bump();
                Some(n)
            } else {
                None
            };
            let inout = self.eat_kw(Kw::Inout);
            let value = self.expr(0)?;
            args.push(Arg { name, inout, value, span: Span { lo, hi: self.last_hi } });
            if !self.eat(P::Comma) {
                break;
            }
        }
        self.expect(P::RParen)?;
        let multiline = args.first().is_some_and(|a| self.line(a.span.lo) > self.line(open.lo));
        Ok((args, multiline))
    }

    fn primary(&mut self) -> R<Expr> {
        let lo = self.span().lo;
        let tok = self.peek().clone();
        let kind = match tok {
            Tok::Int(s) => {
                self.bump();
                ExprKind::Int(s)
            }
            Tok::Float(s) => {
                self.bump();
                ExprKind::Float(s)
            }
            Tok::Dur(s) => {
                self.bump();
                ExprKind::Dur(s)
            }
            Tok::Byte(s) => {
                self.bump();
                ExprKind::Byte(s)
            }
            Tok::Str { .. } => ExprKind::Str(self.str_lit()?),
            Tok::Kw(Kw::True) => {
                self.bump();
                ExprKind::Bool(true)
            }
            Tok::Kw(Kw::False) => {
                self.bump();
                ExprKind::Bool(false)
            }
            Tok::Kw(Kw::None) => {
                self.bump();
                ExprKind::None
            }
            Tok::Kw(Kw::SelfKw) => {
                self.bump();
                ExprKind::SelfRef
            }
            Tok::Ident(n) => {
                self.bump();
                ExprKind::Ident(n)
            }
            Tok::P(P::Underscore) => {
                self.bump();
                ExprKind::Hole
            }
            Tok::P(P::Dot) => {
                self.bump();
                ExprKind::Variant(self.ident("a variant name after `.`")?)
            }
            Tok::P(P::LParen) => {
                self.bump();
                if self.eat(P::RParen) {
                    ExprKind::Tuple(Vec::new())
                } else {
                    let first = self.expr(0)?;
                    if self.eat(P::Comma) {
                        let mut items = vec![first];
                        while !self.at(P::RParen) {
                            items.push(self.expr(0)?);
                            if !self.eat(P::Comma) {
                                break;
                            }
                        }
                        self.expect(P::RParen)?;
                        ExprKind::Tuple(items)
                    } else {
                        self.expect(P::RParen)?;
                        ExprKind::Paren(Box::new(first))
                    }
                }
            }
            Tok::P(P::LBracket) => {
                let open = self.bump().span;
                let mut items = Vec::new();
                while !self.at(P::RBracket) {
                    items.push(self.expr(0)?);
                    if !self.eat(P::Comma) {
                        break;
                    }
                }
                self.expect(P::RBracket)?;
                let multiline = items.first().is_some_and(|i| self.line(i.span.lo) > self.line(open.lo));
                ExprKind::Array { items, multiline }
            }
            Tok::P(P::LBrace) => self.brace_literal()?,
            Tok::Kw(Kw::If) => return self.if_expr(),
            Tok::Kw(Kw::Match) => return self.match_expr(),
            Tok::Kw(Kw::Lock) => {
                self.bump();
                let target = self.expr(0)?;
                self.expect_kw(Kw::As)?;
                let name = self.ident("a name for the locked value, like `lock s as v { ... }`")?;
                let body = self.block()?;
                ExprKind::Lock { target: Box::new(target), name, body }
            }
            Tok::Kw(Kw::Unsafe) => {
                self.bump();
                ExprKind::Unsafe(self.block()?)
            }
            Tok::P(P::Pipe) | Tok::P(P::OrOr) => return self.closure(),
            Tok::Kw(Kw::Return) => {
                self.bump();
                let value = if matches!(
                    self.peek(),
                    Tok::Newline | Tok::Eof | Tok::P(P::RBrace | P::RParen | P::RBracket | P::Comma)
                ) {
                    None
                } else {
                    Some(Box::new(self.expr(0)?))
                };
                ExprKind::Return(value)
            }
            Tok::Kw(Kw::Break) => {
                self.bump();
                ExprKind::Break
            }
            Tok::Kw(Kw::Continue) => {
                self.bump();
                ExprKind::Continue
            }
            t => return Err(self.err_here(format!("expected an expression, found {}", describe(&t)))),
        };
        Ok(Expr { kind, span: Span { lo, hi: self.last_hi } })
    }

    fn brace_literal(&mut self) -> R<ExprKind> {
        let open = self.bump().span;
        self.skip_newlines();
        if self.eat(P::RBrace) {
            return Ok(ExprKind::EmptyBraces);
        }
        let first = self.expr(0)?;
        let multiline = self.line(first.span.lo) > self.line(open.lo);
        if self.eat(P::Colon) {
            let mut entries = vec![(first, self.expr(0)?)];
            while self.list_sep(P::RBrace)? {
                let k = self.expr(0)?;
                self.expect(P::Colon)?;
                entries.push((k, self.expr(0)?));
            }
            self.expect(P::RBrace)?;
            Ok(ExprKind::Map { entries, multiline })
        } else {
            let mut items = vec![first];
            while self.list_sep(P::RBrace)? {
                items.push(self.expr(0)?);
            }
            self.expect(P::RBrace)?;
            Ok(ExprKind::Set { items, multiline })
        }
    }

    fn closure(&mut self) -> R<Expr> {
        let lo = self.span().lo;
        let mut params = Vec::new();
        if !self.eat(P::OrOr) {
            self.expect(P::Pipe)?;
            while !self.at(P::Pipe) {
                let name = self.ident("a closure parameter")?;
                let ty = if self.eat(P::Colon) { Some(self.ty()?) } else { None };
                params.push(ClosureParam { name, ty });
                if !self.eat(P::Comma) {
                    break;
                }
            }
            self.expect(P::Pipe)?;
        }
        let body = if self.at(P::LBrace) {
            let b = self.block()?;
            Expr { span: b.span, kind: ExprKind::Block(b) }
        } else {
            self.expr(0)?
        };
        let span = Span { lo, hi: body.span.hi };
        Ok(Expr { kind: ExprKind::Closure { params, body: Box::new(body) }, span })
    }

    fn if_expr(&mut self) -> R<Expr> {
        let lo = self.bump().span.lo;
        let cond = self.cond()?;
        let then = self.block()?;
        let els = if self.eat_kw(Kw::Else) {
            if self.at_kw(Kw::If) {
                Some(Box::new(self.if_expr()?))
            } else {
                let b = self.block()?;
                Some(Box::new(Expr { span: b.span, kind: ExprKind::Block(b) }))
            }
        } else {
            None
        };
        Ok(Expr { kind: ExprKind::If { cond: Box::new(cond), then, els }, span: Span { lo, hi: self.last_hi } })
    }

    fn match_expr(&mut self) -> R<Expr> {
        let lo = self.bump().span.lo;
        let scrutinee = self.expr(0)?;
        let open = self.expect(P::LBrace)?;
        self.skip_newlines();
        let mut arms = Vec::new();
        while !self.at(P::RBrace) {
            if self.at_eof() {
                return Err(self.err_here("missing `}` to end the `match`"));
            }
            let alo = self.span().lo;
            let pat = self.pattern()?;
            let guard = if self.eat_kw(Kw::If) { Some(self.expr(0)?) } else { None };
            if self.at(P::Colon) {
                return Err(self.err_here("write `pattern => value` for a match arm"));
            }
            self.expect(P::FatArrow)?;
            let body = if self.at(P::LBrace) {
                let b = self.block()?;
                Expr { span: b.span, kind: ExprKind::Block(b) }
            } else {
                self.expr(0)?
            };
            arms.push(Arm { pat, guard, body, span: Span { lo: alo, hi: self.last_hi } });
            // A trailing comma is a Rust habit; accept it, `ovt fmt` removes it.
            self.eat(P::Comma);
            self.end_line()?;
            self.skip_newlines();
        }
        let close = self.expect(P::RBrace)?;
        let one_line = self.line(open.lo) == self.line(close.lo);
        Ok(Expr {
            kind: ExprKind::Match { scrutinee: Box::new(scrutinee), arms, one_line },
            span: Span { lo, hi: close.hi },
        })
    }

    // ---- patterns ----

    fn pattern(&mut self) -> R<Pattern> {
        let first = self.pattern1()?;
        if !self.at(P::Pipe) {
            return Ok(first);
        }
        let lo = first.span.lo;
        let mut alts = vec![first];
        while self.eat(P::Pipe) {
            alts.push(self.pattern1()?);
        }
        Ok(Pattern { kind: PatKind::Or(alts), span: Span { lo, hi: self.last_hi } })
    }

    fn pattern1(&mut self) -> R<Pattern> {
        let lo = self.span().lo;
        let kind = match self.peek().clone() {
            Tok::P(P::Underscore) => {
                self.bump();
                PatKind::Wild
            }
            Tok::Int(_) | Tok::Float(_) | Tok::Str { .. } | Tok::Byte(_) | Tok::Kw(Kw::True | Kw::False | Kw::None) | Tok::P(P::Minus) => {
                let lit = self.unary()?;
                if self.at(P::DotDot) || self.at(P::DotDotEq) {
                    let inclusive = self.bump().tok == Tok::P(P::DotDotEq);
                    let hi = self.unary()?;
                    PatKind::Range { lo: Box::new(lit), hi: Box::new(hi), inclusive }
                } else {
                    PatKind::Lit(Box::new(lit))
                }
            }
            Tok::Ident(n) => {
                let first = self.ident("a pattern")?;
                if self.at(P::Dot) && n.starts_with(|c: char| c.is_ascii_uppercase()) {
                    let mut ty = vec![first];
                    while self.eat(P::Dot) {
                        ty.push(self.ident("a variant name")?);
                    }
                    let name = ty.pop().unwrap();
                    let fields = self.field_pats()?;
                    PatKind::Variant { ty, name, fields }
                } else {
                    PatKind::Bind(n)
                }
            }
            Tok::P(P::Dot) => {
                self.bump();
                let name = self.ident("a variant name after `.`")?;
                let fields = self.field_pats()?;
                PatKind::Variant { ty: Vec::new(), name, fields }
            }
            Tok::P(P::LParen) => {
                self.bump();
                let mut items = Vec::new();
                while !self.at(P::RParen) {
                    items.push(self.pattern()?);
                    if !self.eat(P::Comma) {
                        break;
                    }
                }
                self.expect(P::RParen)?;
                PatKind::Tuple(items)
            }
            Tok::P(P::LBracket) => {
                self.bump();
                let mut items = Vec::new();
                while !self.at(P::RBracket) {
                    if self.at(P::DotDot) {
                        let rlo = self.bump().span.lo;
                        let name = if matches!(self.peek(), Tok::Ident(_)) { Some(self.ident("a name")?) } else { None };
                        items.push(Pattern { kind: PatKind::Rest(name), span: Span { lo: rlo, hi: self.last_hi } });
                    } else {
                        items.push(self.pattern()?);
                    }
                    if !self.eat(P::Comma) {
                        break;
                    }
                }
                self.expect(P::RBracket)?;
                PatKind::Array(items)
            }
            _ => return Err(self.expected("a pattern")),
        };
        Ok(Pattern { kind, span: Span { lo, hi: self.last_hi } })
    }

    fn field_pats(&mut self) -> R<Option<Vec<FieldPat>>> {
        if !self.eat(P::LParen) {
            return Ok(None);
        }
        let mut out = Vec::new();
        while !self.at(P::RParen) {
            let name = self.ident("a field name")?;
            let pat = if self.eat(P::Colon) { Some(self.pattern()?) } else { None };
            out.push(FieldPat { name, pat });
            if !self.eat(P::Comma) {
                break;
            }
        }
        self.expect(P::RParen)?;
        Ok(Some(out))
    }
}
