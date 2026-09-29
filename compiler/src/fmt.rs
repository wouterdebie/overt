//! `ovt fmt` and `ovt outline`.
//!
//! Like gofmt, the formatter keeps the line structure the author chose (a block
//! written on one line stays on one line, a call split over lines stays split)
//! and normalizes everything else: indentation, spacing, commas. Blank lines
//! are kept, at most one at a time. Comments are kept; a trailing comment keeps
//! the spacing before it, so aligned comments stay aligned.

use crate::ast::*;
use crate::lexer::Comment;
use crate::source::{Source, Span};

pub fn format(src: &Source, file: &File, comments: &[Comment]) -> String {
    let mut p = Printer::new(src, comments);
    for item in &file.items {
        p.leading(item.span.lo);
        p.item(item);
        p.nl(item.span.hi);
    }
    for s in &file.stmts {
        p.leading(s.span.lo);
        p.stmt(s);
        p.nl(s.span.hi);
    }
    p.leading(u32::MAX);
    p.out
}

/// Declarations without bodies: signatures, docs, `pre` and `ex` lines.
/// `module` names the module when its functions are called as `module.name(...)`.
pub fn outline(src: &Source, file: &File, comments: &[Comment], module: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(m) = module {
        out.push_str(&format!("// module `{m}`: call its functions as `{m}.name(...)`\n"));
    }
    // The file's own introduction: comments before the first declaration's docs.
    let first = file.items.first().map(|i| i.span.lo).unwrap_or(u32::MAX);
    let first_docs = doc_comments(src, comments, first).first().map(|c| c.span.lo).unwrap_or(first);
    for c in comments.iter().filter(|c| c.own_line && c.span.lo < first_docs) {
        out.push_str(src.slice(c.span));
        out.push('\n');
    }
    for item in &file.items {
        if matches!(item.kind, ItemKind::Test(_)) || is_private(item) {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        for c in doc_comments(src, comments, item.span.lo) {
            out.push_str(src.slice(c.span));
            out.push('\n');
        }
        let mut p = Printer::new(src, &[]);
        match &item.kind {
            ItemKind::Fn(f) => {
                p.fn_sig(f);
                for e in &f.pre {
                    p.nl(0);
                    p.w("  pre ");
                    p.expr(e);
                }
                for ex in &f.ex {
                    p.nl(0);
                    p.w("  ");
                    p.example(ex);
                }
            }
            ItemKind::Drop(d) => {
                p.w("drop ");
                p.ty(&d.ty);
            }
            ItemKind::Type(t) => {
                // Private fields are an implementation detail.
                let mut t = t.clone();
                if let TypeBody::Struct { fields, one_line } = &mut t.body {
                    fields.retain(|f| !f.name.name.starts_with('_'));
                    *one_line = true;
                    if fields.is_empty() {
                        p.w("type ");
                        p.w(&t.name.name);
                        p.generics(&t.generics);
                        p.nl(0);
                        out.push_str(&p.out);
                        continue;
                    }
                }
                let it = Item { kind: ItemKind::Type(t), span: item.span };
                p.item(&it);
            }
            _ => p.item(item),
        }
        p.nl(0);
        out.push_str(&p.out);
    }
    out
}

fn is_private(item: &Item) -> bool {
    match &item.kind {
        ItemKind::Fn(f) => f.name.name.starts_with('_'),
        ItemKind::Const(c) => c.name.name.starts_with('_'),
        ItemKind::Type(t) => t.name.name.starts_with('_'),
        ItemKind::Enum(e) => e.name.name.starts_with('_'),
        _ => false,
    }
}

/// Own-line comments directly above `pos`, with no blank line in between.
fn doc_comments<'c>(src: &Source, comments: &'c [Comment], pos: u32) -> Vec<&'c Comment> {
    let mut docs = Vec::new();
    let mut line = src.line(pos);
    for c in comments.iter().rev().filter(|c| c.span.lo < pos) {
        let cl = src.line(c.span.lo);
        if !c.own_line || cl + 1 != line {
            break;
        }
        docs.push(c);
        line = cl;
    }
    docs.reverse();
    docs
}

struct Printer<'a> {
    src: &'a Source,
    comments: &'a [Comment],
    /// Next comment not yet printed.
    ci: usize,
    out: String,
    indent: usize,
    at_line_start: bool,
    /// Source line of the last line printed; 0 before anything is printed.
    last_line: u32,
    /// Just opened a block: no blank line before its first line.
    fresh: bool,
}

impl<'a> Printer<'a> {
    fn new(src: &'a Source, comments: &'a [Comment]) -> Printer<'a> {
        Printer { src, comments, ci: 0, out: String::new(), indent: 0, at_line_start: true, last_line: 0, fresh: false }
    }

    fn line(&self, pos: u32) -> u32 {
        self.src.line(pos)
    }

    fn w(&mut self, s: &str) {
        if self.at_line_start {
            for _ in 0..self.indent {
                self.out.push_str("  ");
            }
            self.at_line_start = false;
        }
        self.out.push_str(s);
    }

    /// Ends the output line. `end` is the source position where the printed
    /// line ended; a trailing comment on that source line is appended.
    fn nl(&mut self, end: u32) {
        let end_line = self.line(end);
        let mut first = true;
        while let Some(c) = self.comments.get(self.ci) {
            if c.own_line || self.line(c.span.lo) != end_line || c.span.lo < end {
                break;
            }
            // Keep the whitespace written before the comment, so aligned comments stay aligned.
            let before = &self.src.text[..c.span.lo as usize];
            let gap = &before[before.trim_end_matches([' ', '\t']).len()..];
            let gap = if first && !gap.is_empty() { gap } else { " " };
            self.out.push_str(gap);
            self.out.push_str(self.src.slice(c.span));
            self.ci += 1;
            first = false;
        }
        self.out.push('\n');
        self.at_line_start = true;
        self.last_line = end_line;
    }

    /// Before a line starting at `pos`: prints the comments before it, and keeps
    /// one blank line where the source had any.
    fn leading(&mut self, pos: u32) {
        self.comments_before(pos);
        if pos != u32::MAX {
            self.blank_before(self.line(pos));
        }
        self.fresh = false;
    }

    /// Before a closing bracket at `pos`: prints the comments before it, but
    /// never a blank line before the bracket itself.
    fn leading_close(&mut self, pos: u32) {
        self.comments_before(pos);
        self.fresh = false;
    }

    fn comments_before(&mut self, pos: u32) {
        while let Some(c) = self.comments.get(self.ci) {
            if c.span.lo >= pos {
                break;
            }
            let cl = self.line(c.span.lo);
            self.blank_before(cl);
            let text = self.src.slice(c.span);
            self.w(text);
            self.out.push('\n');
            self.at_line_start = true;
            self.last_line = cl;
            self.ci += 1;
        }
    }

    fn blank_before(&mut self, line: u32) {
        if !self.fresh && self.last_line > 0 && line > self.last_line + 1 {
            self.out.push('\n');
        }
        self.fresh = false;
    }

    fn has_comments_in(&self, span: Span) -> bool {
        self.comments[self.ci..].iter().any(|c| c.span.lo >= span.lo && c.span.lo < span.hi)
    }

    // ---- declarations ----

    fn item(&mut self, item: &Item) {
        match &item.kind {
            ItemKind::Const(c) => {
                self.w("const ");
                self.w(&c.name.name);
                if let Some(t) = &c.ty {
                    self.w(": ");
                    self.ty(t);
                }
                self.w(" = ");
                self.expr(&c.value);
            }
            ItemKind::Type(t) => {
                self.w("type ");
                self.w(&t.name.name);
                self.generics(&t.generics);
                match &t.body {
                    TypeBody::Struct { fields, one_line } => {
                        self.w(" ");
                        self.fields(fields, *one_line, item.span, "{", "}");
                    }
                    TypeBody::Alias(ty) => {
                        self.w(" = ");
                        self.ty(ty);
                    }
                    TypeBody::Opaque => {}
                }
            }
            ItemKind::Enum(e) => {
                self.w("enum ");
                self.w(&e.name.name);
                self.generics(&e.generics);
                if e.one_line && !self.has_comments_in(item.span) {
                    self.w(" { ");
                    for (i, v) in e.variants.iter().enumerate() {
                        if i > 0 {
                            self.w(", ");
                        }
                        self.variant(v);
                    }
                    self.w(" }");
                } else {
                    self.w(" {");
                    self.nl(item.span.lo);
                    self.indent += 1;
                    self.fresh = true;
                    for v in &e.variants {
                        self.leading(v.span.lo);
                        self.variant(v);
                        self.nl(v.span.hi);
                    }
                    self.leading_close(item.span.hi);
                    self.indent -= 1;
                    self.w("}");
                }
            }
            ItemKind::Fn(f) => self.fn_decl(f),
            ItemKind::Extern(x) => {
                if x.blocking {
                    self.w("blocking ");
                }
                self.w("extern ");
                self.str_lit(&x.lib);
                if let Some(h) = &x.header {
                    self.w(" header ");
                    self.str_lit(h);
                }
                if let Some(items) = &x.items {
                    self.w(" {");
                    self.nl(item.span.lo);
                    self.indent += 1;
                    self.fresh = true;
                    for it in items {
                        self.leading(it.span.lo);
                        self.item(it);
                        self.nl(it.span.hi);
                    }
                    self.leading_close(item.span.hi);
                    self.indent -= 1;
                    self.w("}");
                }
            }
            ItemKind::Drop(d) => {
                self.w("drop ");
                self.ty(&d.ty);
                self.w(" ");
                self.block(&d.body);
            }
            ItemKind::Test(t) => {
                self.w("test ");
                self.str_lit(&t.name);
                self.effects(&t.effects);
                self.w(" ");
                self.block(&t.body);
            }
        }
    }

    fn fields(&mut self, fields: &[Field], one_line: bool, span: Span, open: &str, close: &str) {
        if one_line && !self.has_comments_in(span) {
            self.w(open);
            if !fields.is_empty() && open == "{" {
                self.w(" ");
            }
            for (i, f) in fields.iter().enumerate() {
                if i > 0 {
                    self.w(", ");
                }
                self.field(f);
            }
            if !fields.is_empty() && close == "}" {
                self.w(" ");
            }
            self.w(close);
        } else {
            self.w(open);
            self.nl(span.lo);
            self.indent += 1;
            self.fresh = true;
            for f in fields {
                self.leading(f.span.lo);
                self.field(f);
                self.nl(f.span.hi);
            }
            self.leading_close(span.hi);
            self.indent -= 1;
            self.w(close);
        }
    }

    fn field(&mut self, f: &Field) {
        self.w(&f.name.name);
        self.w(": ");
        self.ty(&f.ty);
        if let Some(d) = &f.default {
            self.w(" = ");
            self.expr(d);
        }
    }

    fn variant(&mut self, v: &Variant) {
        self.w(&v.name.name);
        if let Some(fields) = &v.fields {
            self.w("(");
            for (i, f) in fields.iter().enumerate() {
                if i > 0 {
                    self.w(", ");
                }
                self.field(f);
            }
            self.w(")");
        }
    }

    fn generics(&mut self, g: &[GenericParam]) {
        if g.is_empty() {
            return;
        }
        self.w("[");
        for (i, p) in g.iter().enumerate() {
            if i > 0 {
                self.w(", ");
            }
            match p {
                GenericParam::Type { name, bounds } => {
                    self.w(&name.name);
                    for (j, b) in bounds.iter().enumerate() {
                        self.w(if j == 0 { ": " } else { " + " });
                        self.w(&b.name);
                    }
                }
                GenericParam::Effect { name } => {
                    self.w("!");
                    self.w(&name.name);
                }
            }
        }
        self.w("]");
    }

    fn effects(&mut self, effects: &[Ident]) {
        for (i, e) in effects.iter().enumerate() {
            self.w(if i == 0 { " ! " } else { ", " });
            self.w(&e.name);
        }
    }

    fn fn_sig(&mut self, f: &FnDecl) {
        if f.is_unsafe {
            self.w("unsafe ");
        }
        if f.blocking {
            self.w("blocking ");
        }
        self.w("fn ");
        if let Some((ty, g)) = &f.recv {
            if ty.name == "[]" {
                self.generics(g);
            } else {
                self.w(&ty.name);
                self.generics(g);
            }
            self.w(".");
        }
        self.w(&f.name.name);
        self.generics(&f.generics);
        self.w("(");
        for (i, p) in f.params.iter().enumerate() {
            if i > 0 {
                self.w(", ");
            }
            let mode = match p.mode {
                Mode::Read => "",
                Mode::Inout => "inout ",
                Mode::Sink => "sink ",
            };
            match &p.ty {
                None => {
                    self.w(mode);
                    self.w("self");
                }
                Some(t) => {
                    self.w(&p.name.name);
                    self.w(": ");
                    self.w(mode);
                    self.ty(t);
                }
            }
            if let Some(d) = &p.default {
                self.w(" = ");
                self.expr(d);
            }
        }
        self.w(")");
        if let Some(r) = &f.ret {
            self.w(" -> ");
            self.ty(r);
        }
        self.effects(&f.effects);
    }

    fn fn_decl(&mut self, f: &FnDecl) {
        self.fn_sig(f);
        let Some(body) = &f.body else {
            // An intrinsic (standard library only): its clauses, and no body.
            for e in &f.pre {
                self.nl(f.sig_span.hi);
                self.w("  pre ");
                self.expr(e);
            }
            for ex in &f.ex {
                self.nl(f.sig_span.hi);
                self.w("  ");
                self.example(ex);
            }
            return;
        };
        if f.pre.is_empty() && f.ex.is_empty() {
            self.w(" ");
            self.block(body);
            return;
        }
        self.nl(f.sig_span.hi);
        self.indent += 1;
        for e in &f.pre {
            self.leading(e.span.lo);
            self.w("pre ");
            self.expr(e);
            self.nl(e.span.hi);
        }
        for ex in &f.ex {
            self.leading(ex.span.lo);
            self.example(ex);
            self.nl(ex.span.hi);
        }
        self.indent -= 1;
        self.leading(body.span.lo);
        self.block_multiline(body);
    }

    fn example(&mut self, ex: &Example) {
        self.w("ex ");
        self.expr(&ex.expr);
        if let Some(kind) = &ex.fails {
            self.w(" fails");
            if let Some(k) = kind {
                self.w(" ");
                self.expr(k);
            }
        }
    }

    // ---- types ----

    fn ty(&mut self, t: &TypeExpr) {
        match &t.kind {
            TypeKind::Named { path, args } => {
                for (i, seg) in path.iter().enumerate() {
                    if i > 0 {
                        self.w(".");
                    }
                    self.w(&seg.name);
                }
                if !args.is_empty() {
                    self.w("[");
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            self.w(", ");
                        }
                        self.ty(a);
                    }
                    self.w("]");
                }
            }
            TypeKind::Array(e) => {
                self.w("[");
                self.ty(e);
                self.w("]");
            }
            TypeKind::Fixed(e, n) => {
                self.w("[");
                self.ty(e);
                self.w("; ");
                self.expr(n);
                self.w("]");
            }
            TypeKind::Tuple(items) => {
                self.w("(");
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        self.w(", ");
                    }
                    self.ty(it);
                }
                self.w(")");
            }
            TypeKind::Optional(e) => {
                self.w("?");
                self.ty(e);
            }
            TypeKind::Fn { params, ret, effects } => {
                self.w("fn(");
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        self.w(", ");
                    }
                    self.ty(p);
                }
                self.w(")");
                if let Some(r) = ret {
                    self.w(" -> ");
                    self.ty(r);
                }
                self.effects(effects);
            }
            TypeKind::Ptr(e) => {
                self.w("*");
                self.ty(e);
            }
        }
    }

    // ---- statements ----

    fn block(&mut self, b: &Block) {
        if b.stmts.is_empty() && !self.has_comments_in(b.span) {
            self.w("{}");
        } else if b.stmts.len() == 1 && self.line(b.span.lo) == self.line(b.span.hi) && !self.has_comments_in(b.span) {
            self.w("{ ");
            self.stmt(&b.stmts[0]);
            self.w(" }");
        } else {
            self.block_multiline(b);
        }
    }

    fn block_multiline(&mut self, b: &Block) {
        self.w("{");
        self.nl(b.span.lo);
        self.indent += 1;
        self.fresh = true;
        for s in &b.stmts {
            self.leading(s.span.lo);
            self.stmt(s);
            self.nl(s.span.hi);
        }
        self.fresh = false;
        self.leading_close(b.span.hi.saturating_sub(1));
        self.indent -= 1;
        self.w("}");
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Let { mutable, pat, ty, value } => {
                self.w(if *mutable { "var " } else { "let " });
                self.pattern(pat);
                if let Some(t) = ty {
                    self.w(": ");
                    self.ty(t);
                }
                self.w(" = ");
                self.expr(value);
            }
            StmtKind::Assign { target, op, value } => {
                self.expr(target);
                self.w(" ");
                self.w(op.text());
                self.w(" ");
                self.expr(value);
            }
            StmtKind::Expr(e) => self.expr(e),
            StmtKind::For { inout, pats, iter, body } => {
                self.w("for ");
                if *inout {
                    self.w("inout ");
                }
                for (i, p) in pats.iter().enumerate() {
                    if i > 0 {
                        self.w(", ");
                    }
                    self.pattern(p);
                }
                self.w(" in ");
                self.expr(iter);
                self.w(" ");
                self.block(body);
            }
            StmtKind::While { cond, body } => {
                self.w("while ");
                self.cond(cond);
                self.w(" ");
                self.block(body);
            }
            StmtKind::Par(b) => {
                self.w("par ");
                self.block(b);
            }
        }
    }

    fn cond(&mut self, c: &Cond) {
        match c {
            Cond::Expr(e) => self.expr(e),
            Cond::Let { name, value } => {
                self.w("let ");
                self.w(&name.name);
                self.w(" = ");
                self.expr(value);
            }
        }
    }

    // ---- expressions ----

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Int(s) | ExprKind::Float(s) | ExprKind::Dur(s) => self.w(s),
            ExprKind::Byte(s) => {
                self.w("'");
                self.w(s);
                self.w("'");
            }
            ExprKind::Str(s) => self.str_lit(s),
            ExprKind::Bool(b) => self.w(if *b { "true" } else { "false" }),
            ExprKind::None => self.w("none"),
            ExprKind::Ident(n) => self.w(n),
            ExprKind::SelfRef => self.w("self"),
            ExprKind::Hole => self.w("_"),
            ExprKind::Variant(n) => {
                self.w(".");
                self.w(&n.name);
            }
            ExprKind::Paren(inner) => {
                self.w("(");
                self.expr(inner);
                self.w(")");
            }
            ExprKind::Tuple(items) => {
                self.w("(");
                self.list(items, false, e.span, |p, x| p.expr(x));
                self.w(")");
            }
            ExprKind::Array { items, multiline } => {
                self.w("[");
                self.list(items, *multiline, e.span, |p, x| p.expr(x));
                self.w("]");
            }
            ExprKind::Map { entries, multiline } => {
                self.w("{");
                self.list(entries, *multiline, e.span, |p, (k, v)| {
                    p.expr(k);
                    p.w(": ");
                    p.expr(v);
                });
                self.w("}");
            }
            ExprKind::Set { items, multiline } => {
                self.w("{");
                self.list(items, *multiline, e.span, |p, x| p.expr(x));
                self.w("}");
            }
            ExprKind::EmptyBraces => self.w("{}"),
            ExprKind::Field(base, name) => {
                self.expr(base);
                self.w(".");
                self.w(&name.name);
            }
            ExprKind::Call { callee, args, multiline } => {
                self.expr(callee);
                self.w("(");
                self.list(args, *multiline, e.span, |p, a| {
                    if let Some(n) = &a.name {
                        p.w(&n.name);
                        p.w(": ");
                    }
                    if a.inout {
                        p.w("inout ");
                    }
                    p.expr(&a.value);
                });
                self.w(")");
            }
            ExprKind::Index { base, args } => {
                self.expr(base);
                self.w("[");
                self.list(args, false, e.span, |p, x| p.expr(x));
                self.w("]");
            }
            ExprKind::Type(t) => self.ty(t),
            ExprKind::Unary(op, inner) => {
                self.w(match op {
                    UnOp::Neg => "-",
                    UnOp::Not => "!",
                });
                self.expr(inner);
            }
            ExprKind::Binary(op, l, r) => {
                self.expr(l);
                self.w(" ");
                self.w(op.text());
                self.w(" ");
                self.expr(r);
            }
            ExprKind::Range { lo, hi, inclusive } => {
                if let Some(l) = lo {
                    self.expr(l);
                }
                self.w(if *inclusive { "..=" } else { ".." });
                if let Some(h) = hi {
                    self.expr(h);
                }
            }
            ExprKind::Try(inner) => {
                self.expr(inner);
                self.w("?");
            }
            ExprKind::Else(l, r) => {
                self.expr(l);
                self.w(" else ");
                self.expr(r);
            }
            ExprKind::Catch { expr, name, body } => {
                self.expr(expr);
                self.w(" catch ");
                self.w(&name.name);
                self.w(" ");
                self.block(body);
            }
            ExprKind::If { cond, then, els } => {
                self.w("if ");
                self.cond(cond);
                self.w(" ");
                self.block(then);
                if let Some(els) = els {
                    self.w(" else ");
                    self.expr(els);
                }
            }
            ExprKind::Match { scrutinee, arms, one_line } => {
                self.w("match ");
                self.expr(scrutinee);
                if *one_line && arms.len() == 1 && !self.has_comments_in(e.span) {
                    self.w(" { ");
                    self.arm(&arms[0]);
                    self.w(" }");
                    return;
                }
                self.w(" {");
                self.nl(scrutinee.span.hi);
                self.indent += 1;
                self.fresh = true;
                for arm in arms {
                    self.leading(arm.span.lo);
                    self.arm(arm);
                    self.nl(arm.span.hi);
                }
                self.fresh = false;
                self.leading_close(e.span.hi.saturating_sub(1));
                self.indent -= 1;
                self.w("}");
            }
            ExprKind::Lock { target, name, body } => {
                self.w("lock ");
                self.expr(target);
                self.w(" as ");
                self.w(&name.name);
                self.w(" ");
                self.block(body);
            }
            ExprKind::Unsafe(b) => {
                self.w("unsafe ");
                self.block(b);
            }
            ExprKind::Block(b) => self.block(b),
            ExprKind::Closure { params, body } => {
                if params.is_empty() {
                    self.w("||");
                } else {
                    self.w("|");
                    for (i, p) in params.iter().enumerate() {
                        if i > 0 {
                            self.w(", ");
                        }
                        self.w(&p.name.name);
                        if let Some(t) = &p.ty {
                            self.w(": ");
                            self.ty(t);
                        }
                    }
                    self.w("|");
                }
                self.w(" ");
                self.expr(body);
            }
            ExprKind::Return(v) => {
                self.w("return");
                if let Some(v) = v {
                    self.w(" ");
                    self.expr(v);
                }
            }
            ExprKind::Break => self.w("break"),
            ExprKind::Continue => self.w("continue"),
        }
    }

    /// A comma-separated list; one element per line when `multiline`.
    fn list<T>(&mut self, items: &[T], multiline: bool, span: Span, mut each: impl FnMut(&mut Self, &T))
    where
        T: HasSpan,
    {
        if !multiline {
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    self.w(", ");
                }
                each(self, it);
            }
            return;
        }
        self.nl(span.lo);
        self.indent += 1;
        self.fresh = true;
        for it in items {
            self.leading(it.span().lo);
            each(self, it);
            self.w(",");
            self.nl(it.span().hi);
        }
        self.fresh = false;
        self.leading_close(span.hi.saturating_sub(1));
        self.indent -= 1;
    }

    fn arm(&mut self, arm: &Arm) {
        self.pattern(&arm.pat);
        if let Some(g) = &arm.guard {
            self.w(" if ");
            self.expr(g);
        }
        self.w(" => ");
        self.expr(&arm.body);
    }

    fn str_lit(&mut self, s: &StrLit) {
        let q = if s.triple { "\"\"\"" } else { "\"" };
        self.w(q);
        for part in &s.parts {
            match part {
                StrPart::Text(t) => self.w(t),
                StrPart::Interp(e) => {
                    self.w("${");
                    self.expr(e);
                    self.w("}");
                }
            }
        }
        self.w(q);
    }

    fn pattern(&mut self, p: &Pattern) {
        match &p.kind {
            PatKind::Wild => self.w("_"),
            PatKind::Bind(n) => self.w(n),
            PatKind::Lit(e) => self.expr(e),
            PatKind::Range { lo, hi, inclusive } => {
                self.expr(lo);
                self.w(if *inclusive { "..=" } else { ".." });
                self.expr(hi);
            }
            PatKind::Tuple(items) => {
                self.w("(");
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        self.w(", ");
                    }
                    self.pattern(it);
                }
                self.w(")");
            }
            PatKind::Array(items) => {
                self.w("[");
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        self.w(", ");
                    }
                    self.pattern(it);
                }
                self.w("]");
            }
            PatKind::Rest(name) => {
                self.w("..");
                if let Some(n) = name {
                    self.w(&n.name);
                }
            }
            PatKind::Variant { ty, name, fields } => {
                if ty.is_empty() {
                    self.w(".");
                } else {
                    for seg in ty {
                        self.w(&seg.name);
                        self.w(".");
                    }
                }
                self.w(&name.name);
                if let Some(fields) = fields {
                    self.w("(");
                    for (i, f) in fields.iter().enumerate() {
                        if i > 0 {
                            self.w(", ");
                        }
                        self.w(&f.name.name);
                        if let Some(fp) = &f.pat {
                            self.w(": ");
                            self.pattern(fp);
                        }
                    }
                    self.w(")");
                }
            }
            PatKind::Or(alts) => {
                for (i, a) in alts.iter().enumerate() {
                    if i > 0 {
                        self.w(" | ");
                    }
                    self.pattern(a);
                }
            }
        }
    }
}

trait HasSpan {
    fn span(&self) -> Span;
}

impl HasSpan for Expr {
    fn span(&self) -> Span {
        self.span
    }
}

impl HasSpan for Arg {
    fn span(&self) -> Span {
        self.span
    }
}

impl HasSpan for (Expr, Expr) {
    fn span(&self) -> Span {
        self.0.span.to(self.1.span)
    }
}
