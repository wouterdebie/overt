//! Name resolution and type checking, producing a typed program for codegen.
//!
//! Milestone 0 covers a core subset: `int`, `bool` and `str` values, functions,
//! `let`/`var`, `if`, `while`, `for` over ranges, `return`, `pre`, checked
//! arithmetic, and the `io` effect. Everything else in the spec is parsed but
//! rejected here with a "not supported yet" error, so a program never silently
//! means something other than what the spec says.

use crate::ast::*;
use crate::diag::{closest, Diag};
use crate::source::{Source, Span};
use std::collections::HashMap;

// ---- the typed program ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ty {
    Int,
    Bool,
    Str,
    Unit,
    /// The type of `return`, `trap(...)` and other expressions that don't finish.
    Never,
    /// An expression that already produced an error; accepted everywhere so
    /// one mistake doesn't cause a cascade of messages.
    Error,
}

impl Ty {
    pub fn name(&self) -> &'static str {
        match self {
            Ty::Int => "int",
            Ty::Bool => "bool",
            Ty::Str => "str",
            Ty::Unit => "nothing",
            Ty::Never => "never",
            Ty::Error => "unknown",
        }
    }
}

pub type LocalId = usize;

pub struct Local {
    pub name: String,
    pub ty: Ty,
}

pub struct Func {
    pub mangled: String,
    pub params: Vec<LocalId>,
    pub locals: Vec<Local>,
    pub ret: Ty,
    /// Each precondition, with its source text for the trap message.
    pub pre: Vec<(TExpr, String)>,
    pub body: TBlock,
}

pub struct Program {
    pub funcs: Vec<Func>,
    pub main: usize,
}

pub struct TBlock {
    pub stmts: Vec<TStmt>,
    /// The last line, when the block is used as a value.
    pub value: Option<Box<TExpr>>,
}

pub enum TStmt {
    Let(LocalId, TExpr),
    Assign(LocalId, AssignOp, TExpr, Span),
    Expr(TExpr),
    While(TExpr, TBlock),
    ForRange { var: LocalId, lo: TExpr, hi: TExpr, inclusive: bool, body: TBlock },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Builtin {
    Print,
    Trap,
    Assert,
    Todo,
}

pub struct TExpr {
    pub kind: TK,
    pub ty: Ty,
    pub span: Span,
}

pub enum TK {
    Int(i64),
    Bool(bool),
    Str(Vec<u8>),
    Local(LocalId),
    Call(usize, Vec<TExpr>),
    /// Builtin calls; `Assert` carries the source text of its condition.
    Builtin(Builtin, Vec<TExpr>, String),
    Bin(BinOp, Box<TExpr>, Box<TExpr>),
    Neg(Box<TExpr>),
    Not(Box<TExpr>),
    If(Box<TExpr>, TBlock, Option<TBlock>),
    Return(Option<Box<TExpr>>),
    Break,
    Continue,
}

// ---- checking ----

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Effects {
    io: bool,
    fail: bool,
}

impl Effects {
    /// The effect names for a sentence: "the `io` effect".
    fn describe(self) -> String {
        match (self.io, self.fail) {
            (true, true) => "the `io` and `fail` effects".into(),
            (true, false) => "the `io` effect".into(),
            _ => "the `fail` effect".into(),
        }
    }

    fn text(self) -> String {
        match (self.io, self.fail) {
            (false, false) => String::new(),
            (true, false) => " ! io".into(),
            (false, true) => " ! fail".into(),
            (true, true) => " ! io, fail".into(),
        }
    }
}

struct Sig {
    name: String,
    params: Vec<(String, Ty)>,
    ret: Ty,
    effects: Effects,
}

impl Sig {
    fn text(&self) -> String {
        let params: Vec<String> = self.params.iter().map(|(n, t)| format!("{n}: {}", t.name())).collect();
        let ret = if self.ret == Ty::Unit { String::new() } else { format!(" -> {}", self.ret.name()) };
        format!("{}({}){ret}{}", self.name, params.join(", "), self.effects.text())
    }
}

const BUILTINS: &[(&str, &str)] = &[
    ("print", "print(x) ! io"),
    ("trap", "trap(msg: str)"),
    ("assert", "assert(cond: bool, msg: str = \"\")"),
    ("todo", "todo()"),
];

const STD_MODULES: &[&str] = &["math", "fs", "os", "time", "net", "http", "json", "log", "task", "ptr", "c"];

/// Types from the spec that this compiler doesn't implement yet.
const LATER_TYPES: &[&str] = &[
    "i32", "i16", "i8", "u64", "u32", "u16", "u8", "f64", "f32", "Dur", "Map", "Set", "Shared", "Atomic", "Chan", "Err",
    "ErrKind",
];

fn later(span: Span, what: &str) -> Diag {
    Diag::new(span, format!("{what} isn't supported by this compiler yet (planned for milestone 1)"))
}

fn is_snake(name: &str) -> bool {
    name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn to_snake(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 && !out.ends_with('_') {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

pub fn check(src: &Source, file: &File) -> Result<Program, Vec<Diag>> {
    let mut c = Checker { src, diags: Vec::new(), sigs: Vec::new(), by_name: HashMap::new() };
    let mut decls = Vec::new();
    for item in &file.items {
        match &item.kind {
            ItemKind::Fn(f) => {
                if let Some(sig) = c.signature(f) {
                    if let Some(&prev) = c.by_name.get(&sig.name) {
                        let _ = prev;
                        c.diags.push(Diag::new(
                            f.name.span,
                            format!("`{}` is declared twice; Overt has no overloading, so give one a different name", sig.name),
                        ));
                        continue;
                    }
                    c.by_name.insert(sig.name.clone(), c.sigs.len());
                    c.sigs.push(sig);
                    decls.push(f);
                }
            }
            ItemKind::Const(_) => c.diags.push(later(item.span, "`const`")),
            ItemKind::Type(_) => c.diags.push(later(item.span, "`type` declarations")),
            ItemKind::Enum(_) => c.diags.push(later(item.span, "`enum` declarations")),
            ItemKind::Extern(_) => c.diags.push(Diag::new(item.span, "`extern` isn't supported by this compiler yet (planned for milestone 5)")),
            ItemKind::Drop(_) => c.diags.push(Diag::new(item.span, "`drop` isn't supported by this compiler yet (planned for milestone 5)")),
            // Tests run under `ovt test`, which arrives with milestone 1.
            ItemKind::Test(_) => {}
        }
    }
    let main = c.by_name.get("main").copied();
    match main {
        None => c.diags.push(Diag::new(Span::default(), "no `fn main`; the program starts at `fn main() ! io, fail`")),
        Some(i) => {
            let sig = &c.sigs[i];
            if !sig.params.is_empty() || sig.ret != Ty::Unit {
                c.diags.push(Diag::new(decls[i].name.span, "`main` takes no parameters and returns nothing; read arguments with `os.args()`"));
            }
        }
    }
    let mut funcs = Vec::new();
    for (i, f) in decls.iter().enumerate() {
        funcs.push(c.function(i, f));
    }
    if c.diags.is_empty() {
        Ok(Program { funcs, main: main.unwrap() })
    } else {
        Err(c.diags)
    }
}

struct Checker<'a> {
    src: &'a Source,
    diags: Vec<Diag>,
    sigs: Vec<Sig>,
    by_name: HashMap<String, usize>,
}

struct Scope {
    names: Vec<(String, LocalId, bool)>,
}

struct FnCx {
    locals: Vec<Local>,
    /// Where each local was declared, for "already declared" messages.
    local_spans: Vec<Span>,
    mutable: Vec<bool>,
    scopes: Vec<Scope>,
    ret: Ty,
    effects: Effects,
    name: String,
    loops: usize,
}

impl FnCx {
    fn lookup(&self, name: &str) -> Option<LocalId> {
        self.scopes.iter().rev().flat_map(|s| s.names.iter().rev()).find(|(n, _, _)| n == name).map(|(_, id, _)| *id)
    }

    fn visible(&self) -> Vec<LocalId> {
        self.scopes.iter().flat_map(|s| s.names.iter().map(|(_, id, _)| *id)).collect()
    }
}

impl<'a> Checker<'a> {
    fn err(&mut self, span: Span, msg: impl Into<String>) {
        self.diags.push(Diag::new(span, msg));
    }

    fn resolve_ty(&mut self, t: &TypeExpr) -> Option<Ty> {
        match &t.kind {
            TypeKind::Named { path, args } if path.len() == 1 && args.is_empty() => match path[0].name.as_str() {
                "int" => Some(Ty::Int),
                "bool" => Some(Ty::Bool),
                "str" => Some(Ty::Str),
                n if LATER_TYPES.contains(&n) => {
                    self.diags.push(later(t.span, &format!("the type `{n}`")));
                    None
                }
                n => {
                    let hint = closest(n, ["int", "bool", "str"]).map(|s| format!("; did you mean `{s}`?")).unwrap_or_default();
                    self.err(t.span, format!("unknown type `{n}`{hint}"));
                    None
                }
            },
            _ => {
                self.diags.push(later(t.span, "this type"));
                None
            }
        }
    }

    fn effects(&mut self, list: &[Ident]) -> Effects {
        let mut e = Effects::default();
        for eff in list {
            match eff.name.as_str() {
                "io" => e.io = true,
                "fail" => e.fail = true,
                other => self.err(eff.span, format!("unknown effect `{other}`; the effects are `io` and `fail`")),
            }
        }
        e
    }

    fn check_snake(&mut self, id: &Ident, what: &str) {
        if !is_snake(&id.name) {
            self.err(id.span, format!("{what} names are snake_case: rename `{}` to `{}`", id.name, to_snake(&id.name)));
        }
    }

    fn signature(&mut self, f: &FnDecl) -> Option<Sig> {
        if f.recv.is_some() {
            self.diags.push(later(f.name.span, "methods"));
            return None;
        }
        if !f.generics.is_empty() {
            self.diags.push(later(f.name.span, "generic functions"));
            return None;
        }
        if f.is_unsafe {
            self.err(f.sig_span, "`unsafe fn` isn't supported by this compiler yet (planned for milestone 5)");
            return None;
        }
        self.check_snake(&f.name, "function");
        let mut params = Vec::new();
        let mut ok = true;
        for p in &f.params {
            if p.mode != Mode::Read {
                self.diags.push(later(p.span, "`inout` and `sink` parameters"));
                ok = false;
            }
            if p.default.is_some() {
                self.diags.push(later(p.span, "parameter defaults"));
                ok = false;
            }
            let Some(t) = &p.ty else {
                self.diags.push(later(p.span, "`self`"));
                ok = false;
                continue;
            };
            self.check_snake(&p.name, "parameter");
            match self.resolve_ty(t) {
                Some(ty) => params.push((p.name.name.clone(), ty)),
                None => ok = false,
            }
        }
        let ret = match &f.ret {
            None => Ty::Unit,
            Some(t) => match self.resolve_ty(t) {
                Some(ty) => ty,
                None => {
                    ok = false;
                    Ty::Unit
                }
            },
        };
        let effects = self.effects(&f.effects);
        ok.then(|| Sig { name: f.name.name.clone(), params, ret, effects })
    }

    fn function(&mut self, index: usize, f: &FnDecl) -> Func {
        let sig = &self.sigs[index];
        let mut cx = FnCx {
            locals: Vec::new(),
            local_spans: Vec::new(),
            mutable: Vec::new(),
            scopes: vec![Scope { names: Vec::new() }],
            ret: sig.ret.clone(),
            effects: sig.effects,
            name: sig.name.clone(),
            loops: 0,
        };
        let mut params = Vec::new();
        let sig_params = sig.params.clone();
        for (p, (name, ty)) in f.params.iter().zip(sig_params) {
            params.push(self.declare(&mut cx, &name, ty, false, p.name.span));
        }
        let mut pre = Vec::new();
        for e in &f.pre {
            let t = self.expr(&mut cx, e, Some(&Ty::Bool), true);
            self.expect_ty(&t, &Ty::Bool, "a `pre` condition");
            pre.push((t, self.src.slice(e.span).to_string()));
        }
        let body_ast = f.body.as_ref().expect("functions outside extern have bodies");
        let (body, ty) = self.block(&mut cx, body_ast, false, None);
        if cx.ret != Ty::Unit && ty != Ty::Never {
            let end = Span { lo: body_ast.span.hi.saturating_sub(1), hi: body_ast.span.hi };
            self.err(end, format!("`{}` returns `{}`, but can reach the end without `return`", cx.name, cx.ret.name()));
        }
        Func {
            mangled: format!("main.{}", f.name.name),
            params,
            locals: cx.locals,
            ret: cx.ret,
            pre,
            body,
        }
    }

    fn declare(&mut self, cx: &mut FnCx, name: &str, ty: Ty, mutable: bool, span: Span) -> LocalId {
        if let Some(prev) = cx.lookup(name) {
            let (line, _) = self.src.line_col(cx.local_spans[prev].lo);
            self.err(span, format!("`{name}` is already declared on line {line}; pick a new name (Overt has no shadowing)"));
        } else if STD_MODULES.contains(&name) {
            self.err(span, format!("`{name}` is the name of a standard module; pick another name for the variable"));
        }
        let id = cx.locals.len();
        cx.locals.push(Local { name: name.to_string(), ty });
        cx.local_spans.push(span);
        cx.mutable.push(mutable);
        cx.scopes.last_mut().unwrap().names.push((name.to_string(), id, mutable));
        id
    }

    fn expect_ty(&mut self, e: &TExpr, want: &Ty, what: &str) {
        if e.ty != *want && e.ty != Ty::Never && e.ty != Ty::Error && *want != Ty::Error {
            self.err(e.span, format!("{what} must be `{}`, found `{}`", want.name(), e.ty.name()));
        }
    }

    /// Checks a block. With `value`, its last line is the block's value.
    fn block(&mut self, cx: &mut FnCx, b: &Block, value: bool, want: Option<&Ty>) -> (TBlock, Ty) {
        cx.scopes.push(Scope { names: Vec::new() });
        let mut stmts = Vec::new();
        let mut ty = Ty::Unit;
        let mut val = None;
        let n = b.stmts.len();
        for (i, s) in b.stmts.iter().enumerate() {
            if value && i + 1 == n {
                if let StmtKind::Expr(e) = &s.kind {
                    let t = self.expr(cx, e, want, true);
                    ty = t.ty.clone();
                    val = Some(Box::new(t));
                    break;
                }
            }
            let (st, diverges) = self.stmt(cx, s);
            stmts.push(st);
            if diverges {
                ty = Ty::Never;
            }
        }
        cx.scopes.pop();
        (TBlock { stmts, value: val }, ty)
    }

    /// Returns the statement and whether it never finishes (it returns, traps, ...).
    fn stmt(&mut self, cx: &mut FnCx, s: &Stmt) -> (TStmt, bool) {
        match &s.kind {
            StmtKind::Let { mutable, pat, ty, value } => {
                let PatKind::Bind(name) = &pat.kind else {
                    self.diags.push(later(pat.span, "destructuring in `let`"));
                    return (TStmt::Expr(self.dummy(s.span)), false);
                };
                if !is_snake(name) {
                    self.err(pat.span, format!("variable names are snake_case: rename `{name}` to `{}`", to_snake(name)));
                }
                let want = ty.as_ref().and_then(|t| self.resolve_ty(t));
                let v = self.expr(cx, value, want.as_ref(), true);
                if let Some(w) = &want {
                    self.expect_ty(&v, w, &format!("the value of `{name}`"));
                }
                if v.ty == Ty::Unit {
                    self.err(value.span, "this returns nothing, so there's no value to bind");
                }
                let lty = want.unwrap_or_else(|| v.ty.clone());
                let diverges = v.ty == Ty::Never;
                let id = self.declare(cx, name, lty, *mutable, pat.span);
                (TStmt::Let(id, v), diverges)
            }
            StmtKind::Assign { target, op, value } => {
                if let ExprKind::Hole = target.kind {
                    let v = self.expr(cx, value, None, true);
                    let diverges = v.ty == Ty::Never;
                    return (TStmt::Expr(v), diverges);
                }
                let ExprKind::Ident(name) = &target.kind else {
                    self.diags.push(later(target.span, "assigning to fields and elements"));
                    return (TStmt::Expr(self.dummy(s.span)), false);
                };
                let Some(id) = cx.lookup(name) else {
                    self.unknown_name(cx, name, target.span);
                    return (TStmt::Expr(self.dummy(s.span)), false);
                };
                if !cx.mutable[id] {
                    self.err(target.span, format!("`{name}` can't change: declare it with `var {name} = ...`"));
                }
                let lty = cx.locals[id].ty.clone();
                let v = self.expr(cx, value, Some(&lty), true);
                if *op != AssignOp::Set && lty != Ty::Int {
                    self.err(s.span, format!("`{}` needs an `int` variable, and `{name}` is `{}`", op.text(), lty.name()));
                }
                self.expect_ty(&v, &lty, &format!("the new value of `{name}`"));
                let diverges = v.ty == Ty::Never;
                (TStmt::Assign(id, *op, v, s.span), diverges)
            }
            StmtKind::Expr(e) => {
                let t = self.expr(cx, e, None, false);
                if !matches!(t.ty, Ty::Unit | Ty::Never | Ty::Error) {
                    let what = if matches!(e.kind, ExprKind::Call { .. }) { "the result of this call" } else { "this value" };
                    self.err(e.span, format!("{what} is unused; use it, or discard it with `_ = ...`"));
                }
                let diverges = t.ty == Ty::Never;
                (TStmt::Expr(t), diverges)
            }
            StmtKind::While { cond, body } => {
                let Cond::Expr(c) = cond else {
                    self.diags.push(later(s.span, "`while let`"));
                    return (TStmt::Expr(self.dummy(s.span)), false);
                };
                let c = self.expr(cx, c, Some(&Ty::Bool), true);
                self.expect_ty(&c, &Ty::Bool, "a `while` condition");
                cx.loops += 1;
                let (b, _) = self.block(cx, body, false, None);
                cx.loops -= 1;
                (TStmt::While(c, b), false)
            }
            StmtKind::For { inout, pats, iter, body } => {
                let ExprKind::Range { lo: Some(lo), hi: Some(hi), inclusive } = &iter.kind else {
                    self.diags.push(later(iter.span, "`for` over anything but a range like `0..n`"));
                    return (TStmt::Expr(self.dummy(s.span)), false);
                };
                if *inout || pats.len() != 1 {
                    self.diags.push(later(s.span, "this form of `for`"));
                    return (TStmt::Expr(self.dummy(s.span)), false);
                }
                let PatKind::Bind(name) = &pats[0].kind else {
                    self.diags.push(later(pats[0].span, "patterns in `for`"));
                    return (TStmt::Expr(self.dummy(s.span)), false);
                };
                let lo = self.expr(cx, lo, Some(&Ty::Int), true);
                let hi = self.expr(cx, hi, Some(&Ty::Int), true);
                self.expect_ty(&lo, &Ty::Int, "the start of a range");
                self.expect_ty(&hi, &Ty::Int, "the end of a range");
                cx.scopes.push(Scope { names: Vec::new() });
                let var = self.declare(cx, name, Ty::Int, false, pats[0].span);
                cx.loops += 1;
                let (b, _) = self.block(cx, body, false, None);
                cx.loops -= 1;
                cx.scopes.pop();
                (TStmt::ForRange { var, lo, hi, inclusive: *inclusive, body: b }, false)
            }
            StmtKind::Par(_) => {
                self.err(s.span, "`par` isn't supported by this compiler yet (planned for milestone 2)");
                (TStmt::Expr(self.dummy(s.span)), false)
            }
        }
    }

    fn dummy(&self, span: Span) -> TExpr {
        TExpr { kind: TK::Int(0), ty: Ty::Error, span }
    }

    fn unknown_name(&mut self, cx: &FnCx, name: &str, span: Span) {
        let locals: Vec<&str> = cx.visible().iter().map(|&id| cx.locals[id].name.as_str()).collect();
        let fns: Vec<&str> = self.sigs.iter().map(|s| s.name.as_str()).collect();
        let builtins = BUILTINS.iter().map(|(n, _)| *n);
        let candidates: Vec<&str> = locals.iter().copied().chain(fns.iter().copied()).chain(builtins).collect();
        let hint = match closest(name, candidates.iter().copied()) {
            Some(s) => {
                let shown = if let Some(&i) = self.by_name.get(s) {
                    self.sigs[i].text()
                } else if let Some((_, sig)) = BUILTINS.iter().find(|(n, _)| *n == s) {
                    sig.to_string()
                } else {
                    s.to_string()
                };
                format!("; did you mean `{shown}`?")
            }
            None => String::new(),
        };
        self.err(span, format!("unknown name `{name}`{hint}"));
    }

    /// Checks an expression. `value` is false when the result is thrown away
    /// (a statement), which lets `if` without `else` through.
    fn expr(&mut self, cx: &mut FnCx, e: &Expr, want: Option<&Ty>, value: bool) -> TExpr {
        let span = e.span;
        let mk = |kind, ty| TExpr { kind, ty, span };
        match &e.kind {
            ExprKind::Int(s) => {
                let clean: String = s.chars().filter(|&c| c != '_').collect();
                let parsed = if let Some(h) = clean.strip_prefix("0x") {
                    i64::from_str_radix(h, 16)
                } else if let Some(b) = clean.strip_prefix("0b") {
                    i64::from_str_radix(b, 2)
                } else {
                    clean.parse::<i64>()
                };
                match parsed {
                    Ok(v) => mk(TK::Int(v), Ty::Int),
                    Err(_) => {
                        self.err(span, format!("`{s}` doesn't fit in an `int` (64-bit signed)"));
                        mk(TK::Int(0), Ty::Int)
                    }
                }
            }
            ExprKind::Bool(b) => mk(TK::Bool(*b), Ty::Bool),
            ExprKind::Str(s) => match self.decode_str(s) {
                Some(bytes) => mk(TK::Str(bytes), Ty::Str),
                None => mk(TK::Str(Vec::new()), Ty::Str),
            },
            ExprKind::Paren(inner) => self.expr(cx, inner, want, value),
            ExprKind::Ident(name) => {
                if let Some(id) = cx.lookup(name) {
                    return mk(TK::Local(id), cx.locals[id].ty.clone());
                }
                if self.by_name.contains_key(name) || BUILTINS.iter().any(|(n, _)| n == name) {
                    self.diags.push(later(span, "using a function as a value"));
                } else if STD_MODULES.contains(&name.as_str()) {
                    self.diags.push(later(span, &format!("the `{name}` module")));
                } else {
                    self.unknown_name(cx, name, span);
                }
                self.dummy(span)
            }
            ExprKind::Hole => {
                let visible = cx.visible();
                let fits: Vec<String> = visible
                    .iter()
                    .filter(|&&id| want.is_none_or(|w| cx.locals[id].ty == *w))
                    .map(|&id| cx.locals[id].name.clone())
                    .collect();
                let ty = want.map(|w| format!("a value of type `{}`", w.name())).unwrap_or_else(|| "a value".into());
                let fits = if fits.is_empty() { "nothing in scope fits".into() } else { format!("in scope: {}", fits.join(", ")) };
                self.err(span, format!("hole: {ty} goes here; {fits}"));
                self.dummy(span)
            }
            ExprKind::Call { callee, args, .. } => self.call(cx, e, callee, args),
            ExprKind::Unary(op, inner) => {
                let t = self.expr(cx, inner, None, true);
                match op {
                    UnOp::Neg => {
                        self.expect_ty(&t, &Ty::Int, "the operand of `-`");
                        mk(TK::Neg(Box::new(t)), Ty::Int)
                    }
                    UnOp::Not => {
                        self.expect_ty(&t, &Ty::Bool, "the operand of `!`");
                        mk(TK::Not(Box::new(t)), Ty::Bool)
                    }
                }
            }
            ExprKind::Binary(op, l, r) => {
                let lt = self.expr(cx, l, None, true);
                let rt = self.expr(cx, r, Some(&lt.ty), true);
                let ty = self.binary(*op, &lt, &rt, span);
                mk(TK::Bin(*op, Box::new(lt), Box::new(rt)), ty)
            }
            ExprKind::If { cond, then, els } => {
                let Cond::Expr(c) = cond.as_ref() else {
                    self.diags.push(later(span, "`if let`"));
                    return self.dummy(span);
                };
                let c = self.expr(cx, c, Some(&Ty::Bool), true);
                self.expect_ty(&c, &Ty::Bool, "an `if` condition");
                let (tb, tty) = self.block(cx, then, value, want);
                let (eb, ety) = match els.as_deref() {
                    None => {
                        if value && want.is_some_and(|w| *w != Ty::Unit) {
                            self.err(span, "an `if` used as a value needs an `else`");
                        }
                        (None, Ty::Unit)
                    }
                    Some(Expr { kind: ExprKind::Block(b), .. }) => {
                        let (b, t) = self.block(cx, b, value, want.or(Some(&tty)));
                        (Some(b), t)
                    }
                    Some(nested) => {
                        let t = self.expr(cx, nested, want.or(Some(&tty)), value);
                        let ty = t.ty.clone();
                        (Some(TBlock { stmts: Vec::new(), value: Some(Box::new(t)) }), ty)
                    }
                };
                let ty = if !value {
                    if tty == Ty::Never && ety == Ty::Never { Ty::Never } else { Ty::Unit }
                } else if tty == Ty::Never {
                    ety
                } else {
                    if ety != tty && !matches!(ety, Ty::Never | Ty::Error) && tty != Ty::Error && eb.is_some() {
                        self.err(span, format!("the branches of this `if` have different types: `{}` and `{}`", tty.name(), ety.name()));
                    }
                    tty
                };
                mk(TK::If(Box::new(c), tb, eb), ty)
            }
            ExprKind::Return(v) => {
                let ret = cx.ret.clone();
                let tv = v.as_ref().map(|v| self.expr(cx, v, Some(&ret), true));
                match (&tv, &ret) {
                    (None, Ty::Unit) => {}
                    (None, r) => self.err(span, format!("`{}` returns `{}`; write `return <value>`", cx.name, r.name())),
                    (Some(t), Ty::Unit) if t.ty != Ty::Never => {
                        self.err(t.span, format!("`{}` returns nothing; remove the value, or add `-> {}` to its signature", cx.name, t.ty.name()))
                    }
                    (Some(t), r) => self.expect_ty(t, r, "the returned value"),
                }
                mk(TK::Return(tv.map(Box::new)), Ty::Never)
            }
            ExprKind::Break | ExprKind::Continue => {
                if cx.loops == 0 {
                    self.err(span, "`break` and `continue` only work inside a loop");
                }
                mk(if matches!(e.kind, ExprKind::Break) { TK::Break } else { TK::Continue }, Ty::Never)
            }
            ExprKind::Block(_) => {
                self.diags.push(later(span, "a block used as a value here"));
                self.dummy(span)
            }
            other => {
                let what = match other {
                    ExprKind::Float(_) => "floating-point numbers",
                    ExprKind::Dur(_) => "durations",
                    ExprKind::Byte(_) => "byte literals",
                    ExprKind::None => "optionals",
                    ExprKind::SelfRef => "`self`",
                    ExprKind::Variant(_) => "enum variants",
                    ExprKind::Tuple(_) => "tuples",
                    ExprKind::Array { .. } => "arrays",
                    ExprKind::Map { .. } | ExprKind::Set { .. } | ExprKind::EmptyBraces => "maps and sets",
                    ExprKind::Field(..) => "fields and methods",
                    ExprKind::Index { .. } => "indexing",
                    ExprKind::Range { .. } => "ranges outside `for`",
                    ExprKind::Try(_) | ExprKind::Catch { .. } => "failure handling",
                    ExprKind::Else(..) => "`else` on optionals and failures",
                    ExprKind::Match { .. } => "`match`",
                    ExprKind::Lock { .. } => "`lock`",
                    ExprKind::Unsafe(_) => "`unsafe`",
                    ExprKind::Closure { .. } => "closures",
                    ExprKind::Type(_) => "types in expressions",
                    _ => "this expression",
                };
                self.diags.push(later(span, what));
                self.dummy(span)
            }
        }
    }

    fn binary(&mut self, op: BinOp, l: &TExpr, r: &TExpr, span: Span) -> Ty {
        let (lt, rt) = (&l.ty, &r.ty);
        if matches!(lt, Ty::Never | Ty::Error) || matches!(rt, Ty::Never | Ty::Error) {
            return if op.is_comparison() || matches!(op, BinOp::And | BinOp::Or) { Ty::Bool } else { Ty::Int };
        }
        let both = |t: &Ty| lt == t && rt == t;
        match op {
            BinOp::And | BinOp::Or => {
                if !both(&Ty::Bool) {
                    self.err(span, format!("`{}` needs two `bool`s, found `{}` and `{}`", op.text(), lt.name(), rt.name()));
                }
                Ty::Bool
            }
            BinOp::Eq | BinOp::Ne => {
                if lt != rt {
                    self.err(span, format!("can't compare `{}` with `{}`", lt.name(), rt.name()));
                } else if *lt == Ty::Str {
                    self.diags.push(later(span, "comparing strings"));
                } else if *lt == Ty::Unit {
                    self.err(span, "can't compare values of nothing");
                }
                Ty::Bool
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                if !both(&Ty::Int) {
                    self.err(span, format!("`{}` needs two `int`s here, found `{}` and `{}`", op.text(), lt.name(), rt.name()));
                }
                Ty::Bool
            }
            BinOp::Add if *lt == Ty::Str || *rt == Ty::Str => {
                self.diags.push(later(span, "joining strings"));
                Ty::Str
            }
            _ => {
                if !both(&Ty::Int) {
                    self.err(span, format!("`{}` needs two `int`s, found `{}` and `{}`", op.text(), lt.name(), rt.name()));
                }
                Ty::Int
            }
        }
    }

    fn call(&mut self, cx: &mut FnCx, e: &Expr, callee: &Expr, args: &[Arg]) -> TExpr {
        let span = e.span;
        let ExprKind::Ident(name) = &callee.kind else {
            self.diags.push(later(callee.span, "calling anything but a function by name"));
            return self.dummy(span);
        };
        if cx.lookup(name).is_some() {
            self.diags.push(later(callee.span, "calling a variable"));
            return self.dummy(span);
        }
        for a in args {
            if a.inout {
                self.diags.push(later(a.span, "`inout` arguments"));
            }
        }
        if let Some((_, text)) = BUILTINS.iter().find(|(n, _)| n == name) {
            return self.builtin(cx, e, name, text, args);
        }
        let Some(&index) = self.by_name.get(name) else {
            self.unknown_name(cx, name, callee.span);
            for a in args {
                self.expr(cx, &a.value, None, true);
            }
            return self.dummy(span);
        };
        let (params, ret, effects, text) = {
            let s = &self.sigs[index];
            (s.params.clone(), s.ret.clone(), s.effects, s.text())
        };
        if (effects.io && !cx.effects.io) || (effects.fail && !cx.effects.fail) {
            let missing = Effects { io: effects.io && !cx.effects.io, fail: effects.fail && !cx.effects.fail };
            self.err(
                callee.span,
                format!(
                    "`{name}` has {}, so `{}` must declare it too: add `{}` to its signature",
                    missing.describe(),
                    cx.name,
                    missing.text().trim_start()
                ),
            );
        }
        let ordered = self.match_args(cx, name, &text, &params, args, span);
        let mut targs = Vec::new();
        for (i, (pname, pty)) in params.iter().enumerate() {
            if let Some(a) = ordered.get(i).copied().flatten() {
                let t = self.expr(cx, &a.value, Some(pty), true);
                self.expect_ty(&t, pty, &format!("argument `{pname}` of `{name}`"));
                targs.push(t);
            }
        }
        if targs.len() != params.len() {
            return self.dummy(span);
        }
        TExpr { kind: TK::Call(index, targs), ty: ret, span }
    }

    /// Puts arguments in parameter order, checking names, counts, and the rule
    /// that arguments to same-typed parameters are named after the first.
    fn match_args<'e>(
        &mut self,
        cx: &mut FnCx,
        name: &str,
        text: &str,
        params: &[(String, Ty)],
        args: &'e [Arg],
        span: Span,
    ) -> Vec<Option<&'e Arg>> {
        let mut slots: Vec<Option<&Arg>> = vec![None; params.len()];
        let mut positional = 0;
        let mut seen_named = false;
        let _ = cx;
        for a in args {
            if let Some(id) = &a.name {
                let n = id.name.as_str();
                seen_named = true;
                match params.iter().position(|(p, _)| p == n) {
                    Some(i) if slots[i].is_some() => self.err(a.span, format!("`{n}` is given twice")),
                    Some(i) => slots[i] = Some(a),
                    None => {
                        let hint = closest(n, params.iter().map(|(p, _)| p.as_str()))
                            .map(|s| format!("; did you mean `{s}`?"))
                            .unwrap_or_default();
                        self.err(id.span, format!("`{name}` has no parameter `{n}`{hint} ({text})"));
                    }
                }
            } else {
                if seen_named {
                    self.err(a.span, "positional arguments come before named ones");
                    continue;
                }
                if positional < params.len() {
                    if slots[positional].is_none() {
                        slots[positional] = Some(a);
                    }
                    positional += 1;
                } else {
                    positional += 1;
                }
            }
        }
        if positional > params.len() || args.len() > params.len() {
            self.err(span, format!("`{name}` takes {} argument{}, got {} ({text})", params.len(), if params.len() == 1 { "" } else { "s" }, args.len()));
            return slots;
        }
        let missing: Vec<&str> = params.iter().zip(&slots).filter(|(_, s)| s.is_none()).map(|((p, _), _)| p.as_str()).collect();
        if !missing.is_empty() {
            self.err(span, format!("missing argument{} {} for `{name}` ({text})", if missing.len() == 1 { "" } else { "s" }, missing.iter().map(|m| format!("`{m}`")).collect::<Vec<_>>().join(", ")));
            return slots;
        }
        // Parameters that share a type: all but the first must be named at the
        // call. A bare variable with the parameter's own name counts as named,
        // so `copy(src, dst)` passes and a swapped `copy(dst, src)` doesn't.
        for (i, (pname, pty)) in params.iter().enumerate() {
            let earlier_same = params[..i].iter().any(|(_, t)| t == pty);
            if !earlier_same {
                continue;
            }
            let a = slots[i].unwrap();
            let named = a.name.is_some() || matches!(&a.value.kind, ExprKind::Ident(n) if n == pname);
            if !named {
                self.err(
                    a.span,
                    format!("name this argument: `{pname}: ...`; `{name}` has several `{}` parameters, so all but the first are passed by name ({text})", pty.name()),
                );
            }
        }
        slots
    }

    fn builtin(&mut self, cx: &mut FnCx, e: &Expr, name: &str, text: &str, args: &[Arg]) -> TExpr {
        let span = e.span;
        let (b, ty) = match name {
            "print" => (Builtin::Print, Ty::Unit),
            "trap" => (Builtin::Trap, Ty::Never),
            "assert" => (Builtin::Assert, Ty::Unit),
            _ => (Builtin::Todo, Ty::Never),
        };
        let (min, max) = match b {
            Builtin::Print | Builtin::Trap => (1, 1),
            Builtin::Assert => (1, 2),
            Builtin::Todo => (0, 0),
        };
        if args.len() < min || args.len() > max {
            self.err(span, format!("wrong number of arguments for `{text}`"));
            return self.dummy(span);
        }
        if b == Builtin::Print && !cx.effects.io {
            self.err(e.span, format!("`print` has the `io` effect, so `{}` must declare it too: add `! io` to its signature", cx.name));
        }
        let mut targs = Vec::new();
        for (i, a) in args.iter().enumerate() {
            let want = match (b, i) {
                (Builtin::Assert, 0) => Some(Ty::Bool),
                (Builtin::Print, _) => None,
                _ => Some(Ty::Str),
            };
            let t = self.expr(cx, &a.value, want.as_ref(), true);
            match &want {
                Some(w) => self.expect_ty(&t, w, &format!("the argument of `{name}`")),
                None if t.ty == Ty::Unit => self.err(t.span, "this returns nothing, so there's nothing to print"),
                None => {}
            }
            targs.push(t);
        }
        let cond_text = if b == Builtin::Assert { self.src.slice(args[0].value.span).to_string() } else { String::new() };
        TExpr { kind: TK::Builtin(b, targs, cond_text), ty, span }
    }

    fn decode_str(&mut self, s: &StrLit) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        for part in &s.parts {
            match part {
                StrPart::Interp(e) => {
                    self.diags.push(later(e.span, "string interpolation"));
                    return None;
                }
                StrPart::Text(t) => {
                    let b = t.as_bytes();
                    let mut i = 0;
                    while i < b.len() {
                        if b[i] != b'\\' {
                            out.push(b[i]);
                            i += 1;
                            continue;
                        }
                        let e = b.get(i + 1).copied().unwrap_or(0);
                        if s.triple {
                            if e == b'$' {
                                out.push(b'$');
                                i += 2;
                            } else {
                                out.push(b'\\');
                                i += 1;
                            }
                            continue;
                        }
                        i += 2;
                        match e {
                            b'n' => out.push(b'\n'),
                            b't' => out.push(b'\t'),
                            b'r' => out.push(b'\r'),
                            b'0' => out.push(0),
                            b'\\' | b'"' | b'\'' | b'$' => out.push(e),
                            b'x' => {
                                let hex = t.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok());
                                match hex {
                                    Some(v) if v < 0x80 => out.push(v),
                                    _ => {
                                        self.err(s.span, "`\\x` takes two hex digits for an ASCII byte, like `\\x41`");
                                        return None;
                                    }
                                }
                                i += 2;
                            }
                            b'u' => {
                                let rest = &t[i..];
                                let parsed = if rest.starts_with('{') {
                                    rest.find('}').and_then(|c| {
                                        u32::from_str_radix(&rest[1..c], 16).ok().and_then(char::from_u32).map(|ch| (ch, c))
                                    })
                                } else {
                                    None
                                };
                                match parsed {
                                    Some((ch, c)) => {
                                        let mut buf = [0; 4];
                                        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                                        i += c + 1;
                                    }
                                    _ => {
                                        self.err(s.span, "`\\u` takes a code point in braces, like `\\u{e9}`");
                                        return None;
                                    }
                                }
                            }
                            _ => {
                                self.err(s.span, "unknown escape in string");
                                return None;
                            }
                        }
                    }
                }
            }
        }
        Some(out)
    }
}
