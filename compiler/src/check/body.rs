//! Checking function bodies: blocks, statements and expressions.
//!
//! Types are inferred with unification inside each function (see `infer`).
//! Closures are checked in their own frame, sharing the function's inference
//! variables; the names they use from outside become captures.

use super::infer::{Infer, VarKind};
use super::*;
use crate::ast::{AssignOp, BinOp, Cond, Expr, ExprKind, StmtKind, StrPart, UnOp};

pub struct Frame {
    pub locals: Vec<LocalDef>,
    pub spans: Vec<Span>,
    pub mutable: Vec<bool>,
    pub scopes: Vec<Vec<(String, LocalId)>>,
    pub ret: Ty,
    /// Declared effects; `None` for closures, whose effects are inferred into `used`.
    pub declared: Option<Eff>,
    pub used: Eff,
    pub captures: Vec<(LocalId, LocalId)>,
    /// For each enclosing loop, whether it has a `break`.
    pub loops: Vec<bool>,
    pub name: String,
    /// Variables being changed by an enclosing `for inout`.
    pub iter_roots: Vec<LocalId>,
}

impl Frame {
    pub fn new(name: String, ret: Ty, declared: Option<Eff>) -> Frame {
        Frame {
            locals: Vec::new(),
            spans: Vec::new(),
            mutable: Vec::new(),
            scopes: vec![Vec::new()],
            ret,
            declared,
            used: Eff::pure(),
            captures: Vec::new(),
            loops: Vec::new(),
            name,
            iter_roots: Vec::new(),
        }
    }
}

pub struct Obligation {
    pub ty: Ty,
    pub bound: &'static str,
    pub span: Span,
}

pub struct FnCx {
    pub file: FileId,
    pub owner: FnId,
    pub generics: Vec<GenericDef>,
    pub frames: Vec<Frame>,
    pub infer: Infer,
    pub obligations: Vec<Obligation>,
    pub closures: Vec<ClosureId>,
    /// Constants, defaults and `ex` lines: no effects allowed, except
    /// that `ex` lines may fail (`fail_ok`).
    pub pure_only: bool,
    pub fail_ok: bool,
    /// How many errors existed before this function was checked; if it adds
    /// any, "can't tell the type" is left out as a likely consequence.
    pub errors_before: usize,
}

impl FnCx {
    pub fn new(file: FileId, owner: FnId, generics: Vec<GenericDef>) -> FnCx {
        FnCx { file, owner, generics, frames: Vec::new(), infer: Infer::default(), obligations: Vec::new(), closures: Vec::new(), pure_only: false, fail_ok: false, errors_before: 0 }
    }

    pub fn frame(&mut self) -> &mut Frame {
        self.frames.last_mut().unwrap()
    }

    pub fn fr(&self) -> &Frame {
        self.frames.last().unwrap()
    }

    pub fn local_ty(&self, id: LocalId) -> Ty {
        self.fr().locals[id].ty.clone()
    }
}

/// How a name in an expression resolved.
pub enum Named {
    Local(LocalId),
    Fn(FnId),
    Const(usize),
    Type(TypeRef),
    Builtin(Ty),
    Module(String),
    Unknown,
}

pub fn is_builtin_intrinsic(name: &str) -> bool {
    matches!(name, "print" | "eprint" | "dbg" | "trap" | "assert" | "todo" | "fail" | "hash")
}

impl<'a> Checker<'a> {
    // ---- entry points ----

    pub fn check_fn_body(&mut self, id: FnId, d: &ast::FnDecl, file: FileId) {
        // Parameter defaults are constant expressions of the parameter's type.
        for (i, p) in d.params.iter().enumerate() {
            if let Some(def) = &p.default {
                let ty = self.fns[id].params[i].ty.clone();
                if ty.has_params() {
                    self.err(file, def.span, "a parameter whose type is a type parameter can't have a default");
                    continue;
                }
                let v = self.check_const_expr(def, file, Some(&ty));
                self.fns[id].params[i].default = Some(v);
            }
        }
        let Some(block) = &d.body else { return };
        let f = &self.fns[id];
        let mut cx = FnCx::new(file, id, f.generics.clone());
        cx.errors_before = self.diags.len();
        cx.frames.push(Frame::new(f.name.clone(), f.ret.clone(), Some(f.eff.clone())));
        let params: Vec<(String, Mode, Ty, Span)> =
            d.params.iter().zip(&f.params).map(|(a, p)| (p.name.clone(), p.mode, p.ty.clone(), a.name.span)).collect();
        let mut param_ids = Vec::new();
        for (name, mode, ty, span) in params {
            let kind = match mode {
                Mode::Read => LocalKind::Borrowed,
                Mode::Inout => LocalKind::Ref,
                Mode::Sink => LocalKind::Owned,
            };
            param_ids.push(self.declare(&mut cx, &name, ty, kind, mode == Mode::Inout, span));
        }
        let mut pre = Vec::new();
        for e in &d.pre {
            let t = self.expr(&mut cx, e, Some(&Ty::Bool));
            let t = self.expect(&mut cx, t, &Ty::Bool, "a `pre` condition");
            pre.push((t, self.src(file).slice(e.span).to_string()));
        }
        let (tb, ty) = self.block(&mut cx, block, false, None);
        let ret = self.fns[id].ret.clone();
        if ret != Ty::Unit && ty != Ty::Never {
            let end = Span { lo: block.span.hi.saturating_sub(1), hi: block.span.hi };
            let name = self.fns[id].name.clone();
            self.err(file, end, format!("`{name}` returns `{}`, but can reach the end without `return`", self.show(&cx, &ret)));
        }
        let frame = cx.frames.pop().unwrap();
        let mut body = Body { locals: frame.locals, params: param_ids, pre, block: tb };
        self.finish(&mut cx, &mut body);
        self.fns[id].body = Some(body);
    }

    /// A constant expression: a `const`, a default, or part of an `ex` line.
    pub fn check_const_expr(&mut self, e: &Expr, file: FileId, want: Option<&Ty>) -> TExpr {
        let mut cx = FnCx::new(file, usize::MAX, Vec::new());
        cx.pure_only = true;
        cx.frames.push(Frame::new("a constant".into(), Ty::Error, Some(Eff::pure())));
        let t = self.expr(&mut cx, e, want);
        let mut t = match want {
            Some(w) => self.expect(&mut cx, t, w, "the value"),
            None => t,
        };
        let frame = cx.frames.pop().unwrap();
        if !frame.locals.is_empty() {
            // Map literals and the like need locals, which constants can't hold.
            let mut body = Body { locals: frame.locals, params: Vec::new(), pre: Vec::new(), block: TBlock { stmts: Vec::new(), value: Some(Box::new(t)) } };
            self.finish(&mut cx, &mut body);
            let span = e.span;
            let v = body.block.value.take().unwrap();
            let ty = v.ty.clone();
            // Wrap in a function so the locals have a home.
            return self.const_fn(file, span, ty, body.locals, *v);
        }
        self.finish_expr(&mut cx, &mut t);
        t
    }

    /// A zero-argument function holding a constant that needs locals; the constant is a call to it.
    fn const_fn(&mut self, file: FileId, span: Span, ty: Ty, locals: Vec<LocalDef>, value: TExpr) -> TExpr {
        let id = self.fns.len();
        let module = self.files[file].module.clone();
        self.fns.push(FnDef {
            name: "<constant>".into(),
            symbol: format!("{module}.const.{id}"),
            module,
            file,
            generics: Vec::new(),
            params: Vec::new(),
            ret: ty.clone(),
            eff: Eff::pure(),
            has_self: false,
            intrinsic: None,
            body: Some(Body { locals, params: Vec::new(), pre: Vec::new(), block: TBlock { stmts: Vec::new(), value: Some(Box::new(value)) } }),
            span,
        });
        TExpr { kind: TK::Call { f: id, targs: Vec::new(), eargs: Vec::new(), args: Vec::new() }, ty, span }
    }

    /// Turns an `ex` line into a test function.
    pub fn check_example(&mut self, owner: FnId, ex: &ast::Example, file: FileId) {
        let text = self.src(file).slice(ex.span).to_string();
        let mut cx = FnCx::new(file, usize::MAX, Vec::new());
        cx.pure_only = true;
        cx.fail_ok = true;
        cx.frames.push(Frame::new("an `ex` line".into(), Ty::Unit, Some(Eff { fail: true, ..Eff::pure() })));
        let span = ex.span;
        let t = match &ex.fails {
            Some(kind) => {
                let value = self.expr_inner(&mut cx, &ex.expr, None, true);
                if !self.failable(&cx, &value) {
                    self.err(file, ex.expr.span, "`fails` needs a call to a function that can fail");
                }
                let kind = kind.as_ref().map(|k| {
                    let want = Ty::Adt(self.known.err_kind, Vec::new());
                    let t = self.expr(&mut cx, k, Some(&want));
                    Box::new(self.expect(&mut cx, t, &want, "the error kind"))
                });
                TExpr { kind: TK::ExpectFail { value: Box::new(value), kind }, ty: Ty::Unit, span }
            }
            None => match &ex.expr.kind {
                ExprKind::Binary(BinOp::Eq, l, r) => {
                    let lt = self.ex_side(&mut cx, l, None);
                    let lty = lt.ty.clone();
                    let rt = self.ex_side(&mut cx, r, Some(&lty));
                    let rt = self.expect(&mut cx, rt, &lty, "the right side of `==`");
                    self.require_bound(&mut cx, &lty, "Eq", span);
                    TExpr { kind: TK::ExpectEq { left: Box::new(lt), right: Box::new(rt) }, ty: Ty::Unit, span }
                }
                _ => {
                    let c = self.ex_side(&mut cx, &ex.expr, Some(&Ty::Bool));
                    let c = self.expect(&mut cx, c, &Ty::Bool, "an `ex` line");
                    TExpr { kind: TK::ExpectTrue { cond: Box::new(c) }, ty: Ty::Unit, span }
                }
            },
        };
        let frame = cx.frames.pop().unwrap();
        let mut body = Body { locals: frame.locals, params: Vec::new(), pre: Vec::new(), block: TBlock { stmts: vec![TStmt::Expr(t)], value: None } };
        self.finish(&mut cx, &mut body);
        let owner_name = self.fns[owner].name.clone();
        let func = self.test_fn(file, span, body, Eff { fail: true, ..Eff::pure() });
        self.tests.push(TestDef { name: format!("{owner_name}: {text}"), func });
    }

    /// One side of an `ex` line: its outermost call may fail, which fails the example.
    fn ex_side(&mut self, cx: &mut FnCx, e: &Expr, want: Option<&Ty>) -> TExpr {
        let t = self.expr_inner(cx, e, want, true);
        if self.failable(cx, &t) {
            let (ty, span) = (t.ty.clone(), t.span);
            return TExpr { kind: TK::Try(Box::new(t)), ty, span };
        }
        t
    }

    pub fn check_test_block(&mut self, t: &ast::TestDecl, file: FileId) {
        let mut cx = FnCx::new(file, usize::MAX, Vec::new());
        let eff = self.resolve_effects(&t.effects, file, &[]);
        let name = self.decode_plain(&t.name, file).unwrap_or_default();
        cx.frames.push(Frame::new(format!("test \"{name}\""), Ty::Unit, Some(eff.clone())));
        let (tb, _) = self.block(&mut cx, &t.body, false, None);
        let frame = cx.frames.pop().unwrap();
        let mut body = Body { locals: frame.locals, params: Vec::new(), pre: Vec::new(), block: tb };
        self.finish(&mut cx, &mut body);
        let func = self.test_fn(file, t.name.span, body, eff);
        self.tests.push(TestDef { name, func });
    }

    fn test_fn(&mut self, file: FileId, span: Span, body: Body, eff: Eff) -> FnId {
        let id = self.fns.len();
        let module = self.files[file].module.clone();
        self.fns.push(FnDef {
            name: "<test>".into(),
            symbol: format!("{module}.test.{id}"),
            module,
            file,
            generics: Vec::new(),
            params: Vec::new(),
            ret: Ty::Unit,
            eff,
            has_self: false,
            intrinsic: None,
            body: Some(body),
            span,
        });
        // Closures made while checking the test belong to its function.
        for c in &mut self.closures {
            if c.owner == usize::MAX {
                c.owner = id;
            }
        }
        id
    }

    // ---- helpers ----

    pub fn show(&self, cx: &FnCx, t: &Ty) -> String {
        let names = type_param_names(&cx.generics);
        // Literals whose type isn't settled yet show as their default type.
        let t = defaults_for_display(&cx.infer, &cx.infer.resolve(t));
        self.ty_name_with(&t, &names)
    }

    pub fn declare(&mut self, cx: &mut FnCx, name: &str, ty: Ty, kind: LocalKind, mutable: bool, span: Span) -> LocalId {
        let file = cx.file;
        if name != "_" && !name.starts_with('_') {
            // Shadowing: any visible local, including ones from enclosing closures' functions.
            let visible = cx.frames.iter().rev().find_map(|f| {
                f.scopes.iter().rev().flat_map(|s| s.iter().rev()).find(|(n, _)| n == name).map(|(_, id)| (f.spans[*id], ()))
            });
            if let Some((prev, _)) = visible {
                let (line, _) = self.src(file).line_col(prev.lo);
                self.err(file, span, format!("`{name}` is already declared on line {line}; pick a new name (Overt has no shadowing)"));
            } else if stdlib::MODULE_NAMES.contains(&name) || self.modules.contains_key(name) {
                self.err(file, span, format!("`{name}` is the name of a module; pick another name for the variable"));
            }
            if !is_snake(name) && name != "self" {
                self.err(file, span, format!("variable names are snake_case: rename `{name}` to `{}`", to_snake(name)));
            }
        }
        let f = cx.frame();
        let id = f.locals.len();
        f.locals.push(LocalDef { name: name.to_string(), ty, kind });
        f.spans.push(span);
        f.mutable.push(mutable);
        f.scopes.last_mut().unwrap().push((name.to_string(), id));
        id
    }

    /// Declares a hidden local for compiler-made code (map literals and the like).
    pub fn temp_local(&mut self, cx: &mut FnCx, ty: Ty, span: Span) -> LocalId {
        let f = cx.frame();
        let id = f.locals.len();
        f.locals.push(LocalDef { name: format!("<tmp{id}>"), ty, kind: LocalKind::Owned });
        f.spans.push(span);
        f.mutable.push(true);
        id
    }

    /// Finds a local by name in the current frame, capturing it from
    /// enclosing frames (closures) if needed.
    pub fn lookup_local(&mut self, cx: &mut FnCx, name: &str) -> Option<LocalId> {
        let n = cx.frames.len();
        for depth in (0..n).rev() {
            let found = cx.frames[depth].scopes.iter().rev().flat_map(|s| s.iter().rev()).find(|(nm, _)| nm == name).map(|(_, id)| *id);
            if let Some(mut id) = found {
                // Capture through each closure frame between there and here.
                for d in depth + 1..n {
                    let existing = cx.frames[d].captures.iter().find(|(outer, _)| *outer == id).map(|(_, inner)| *inner);
                    id = match existing {
                        Some(inner) => inner,
                        None => {
                            let ty = cx.frames[d - 1].locals[id].ty.clone();
                            let span = cx.frames[d - 1].spans[id];
                            let f = &mut cx.frames[d];
                            let inner = f.locals.len();
                            f.locals.push(LocalDef { name: name.to_string(), ty, kind: LocalKind::Borrowed });
                            f.spans.push(span);
                            f.mutable.push(false);
                            f.captures.push((id, inner));
                            inner
                        }
                    };
                }
                return Some(id);
            }
        }
        None
    }

    pub fn resolve_name(&mut self, cx: &mut FnCx, name: &str) -> Named {
        if let Some(id) = self.lookup_local(cx, name) {
            return Named::Local(id);
        }
        let file = cx.file;
        let module = self.files[file].module.clone();
        let open = self.files[file].open;
        let scopes: [Option<&Scope>; 2] = [if open { None } else { self.modules.get(&module) }, Some(&self.global)];
        for s in scopes.into_iter().flatten() {
            if let Some(&c) = s.consts.get(name) {
                return Named::Const(c);
            }
            if let Some(&f) = s.fns.get(name) {
                return Named::Fn(f);
            }
            if let Some(t) = s.types.get(name) {
                return Named::Type(t.clone());
            }
        }
        if let Some(k) = IntTy::from_name(name) {
            return Named::Builtin(Ty::Int(k));
        }
        match name {
            "f64" => return Named::Builtin(Ty::F64),
            "f32" => return Named::Builtin(Ty::Float(FloatTy::F32)),
            "str" => return Named::Builtin(Ty::Str),
            "bool" => return Named::Builtin(Ty::Bool),
            _ => {}
        }
        if self.modules.contains_key(name) && !self.files.iter().any(|f| f.open && f.module == name) {
            return Named::Module(name.to_string());
        }
        if stdlib::MODULE_NAMES.contains(&name) {
            return Named::Module(name.to_string());
        }
        // A module path's first segment, like `api` in `api.users.find`.
        if self.modules.keys().any(|m| m.starts_with(&format!("{name}."))) {
            return Named::Module(name.to_string());
        }
        Named::Unknown
    }

    pub fn unknown_name(&mut self, cx: &mut FnCx, name: &str, span: Span) {
        let mut candidates: Vec<String> = Vec::new();
        for f in &cx.frames {
            for s in &f.scopes {
                candidates.extend(s.iter().map(|(n, _)| n.clone()));
            }
        }
        let module = self.files[cx.file].module.clone();
        if let Some(s) = self.modules.get(&module) {
            candidates.extend(s.fns.keys().cloned());
            candidates.extend(s.consts.keys().cloned());
        }
        candidates.extend(self.global.fns.keys().cloned());
        let hint = match name {
            "println" | "printf" | "puts" | "console" => "; use `print(x)`".to_string(),
            "len" => "; use the method: `x.len()`".to_string(),
            "Some" => "; a value converts to an optional by itself, so write just the value".to_string(),
            "None" | "null" | "nil" => "; the empty optional is `none`".to_string(),
            "Ok" => "; return the value itself; failure uses `fail(.Kind, msg)`".to_string(),
            "Err" => "; fail with `fail(.Kind, msg)`".to_string(),
            "True" | "False" => "; booleans are `true` and `false`".to_string(),
            "String" | "string" => "; build a string with `\"${x}\"`".to_string(),
            _ => match closest(name, candidates.iter().map(|s| s.as_str())) {
                Some(s) => {
                    let s = s.to_string();
                    let shown = match self.resolve_name(cx, &s) {
                        Named::Fn(f) => self.fn_sig_text(f),
                        _ => s,
                    };
                    format!("; did you mean `{shown}`?")
                }
                None => String::new(),
            },
        };
        self.err(cx.file, span, format!("unknown name `{name}`{hint}"));
    }

    pub fn error_expr(&self, span: Span) -> TExpr {
        TExpr { kind: TK::Unit, ty: Ty::Error, span }
    }

    /// Checks that `t` fits `want`, converting a value to an optional where one is expected.
    pub fn expect(&mut self, cx: &mut FnCx, t: TExpr, want: &Ty, what: &str) -> TExpr {
        let w = cx.infer.shallow(want);
        let a = cx.infer.shallow(&t.ty);
        if let Ty::Opt(inner) = &w {
            let plain_var = matches!(a, Ty::Var(v) if cx.infer.kind(v) == Some(VarKind::Any));
            if !matches!(a, Ty::Opt(_) | Ty::Error | Ty::Never) && !plain_var {
                if cx.infer.unify(&t.ty, inner) {
                    let span = t.span;
                    return TExpr { kind: TK::Some(Box::new(t)), ty: w.clone(), span };
                }
            }
        }
        if !cx.infer.unify(&t.ty, want) {
            let msg = format!("{what} must be `{}`, found `{}`", self.show(cx, want), self.show(cx, &t.ty));
            self.err(cx.file, t.span, msg);
        }
        t
    }

    pub fn require_bound(&mut self, cx: &mut FnCx, ty: &Ty, bound: &'static str, span: Span) {
        cx.obligations.push(Obligation { ty: ty.clone(), bound, span });
    }

    /// Records effects used at `span`: closures collect them, functions must declare them.
    pub fn use_effects(&mut self, cx: &mut FnCx, eff: &Eff, span: Span, callee: &str) {
        let eff = cx.infer.resolve_eff(eff);
        if cx.pure_only && cx.fail_ok && eff.io {
            let what = cx.fr().name.clone();
            self.err(cx.file, span, format!("{what} can't use `io`, but `{callee}` has the `io` effect"));
            return;
        }
        if cx.pure_only && !cx.fail_ok && (eff.io || eff.fail) {
            let what = cx.fr().name.clone();
            self.err(cx.file, span, format!("{what} must be pure, but `{callee}` has effects"));
            return;
        }
        let frame = cx.frame();
        match &frame.declared {
            None => frame.used = frame.used.union(&Eff { vars: Vec::new(), ..eff }),
            Some(decl) => {
                let missing = Eff {
                    io: eff.io && !decl.io,
                    fail: eff.fail && !decl.fail,
                    params: eff.params.iter().filter(|p| !decl.params.contains(p)).copied().collect(),
                    vars: Vec::new(),
                };
                if !missing.is_pure() {
                    let name = frame.name.clone();
                    let add = self.eff_text(&missing, &cx.generics);
                    let desc = match (missing.io, missing.fail) {
                        (true, true) => "the `io` and `fail` effects".to_string(),
                        (true, false) => "the `io` effect".to_string(),
                        (false, true) => "the `fail` effect".to_string(),
                        _ => format!("the effect{add}"),
                    };
                    self.err(cx.file, span, format!("`{callee}` has {desc}, so `{name}` must declare it too: add `{}` to its signature", add.trim_start()));
                }
            }
        }
    }

    pub fn failable(&self, cx: &FnCx, t: &TExpr) -> bool {
        match &t.kind {
            TK::Call { f, eargs, .. } => {
                let e = self.fns[*f].eff.subst(eargs);
                cx.infer.resolve_eff(&e).fail
            }
            TK::CallValue { callee, .. } => match cx.infer.resolve(&callee.ty) {
                Ty::Fn(ft) => cx.infer.resolve_eff(&ft.eff).fail,
                _ => false,
            },
            _ => false,
        }
    }

    fn callee_name(&self, t: &TExpr) -> String {
        match &t.kind {
            TK::Call { f, .. } => self.fns[*f].name.clone(),
            _ => "this function".into(),
        }
    }

    // ---- blocks and statements ----

    /// Checks a block. With `value`, its last line is the block's value.
    pub fn block(&mut self, cx: &mut FnCx, b: &ast::Block, value: bool, want: Option<&Ty>) -> (TBlock, Ty) {
        cx.frame().scopes.push(Vec::new());
        let mut stmts = Vec::new();
        let mut ty = Ty::Unit;
        let mut val = None;
        let mut diverged = false;
        let n = b.stmts.len();
        for (i, s) in b.stmts.iter().enumerate() {
            if value && i + 1 == n {
                if let StmtKind::Expr(e) = &s.kind {
                    let t = match want {
                        Some(w) => {
                            let t = self.expr(cx, e, Some(w));
                            self.expect(cx, t, w, "the value of this block")
                        }
                        None => self.expr(cx, e, None),
                    };
                    ty = if diverged { Ty::Never } else { t.ty.clone() };
                    val = Some(Box::new(t));
                    break;
                }
            }
            let (st, div) = self.stmt(cx, s);
            if let Some(st) = st {
                stmts.push(st);
            }
            diverged |= div;
        }
        cx.frame().scopes.pop();
        if diverged {
            ty = Ty::Never;
        }
        (TBlock { stmts, value: val }, ty)
    }

    /// Returns the statement and whether it never finishes (returns, traps, ...).
    fn stmt(&mut self, cx: &mut FnCx, s: &ast::Stmt) -> (Option<TStmt>, bool) {
        let file = cx.file;
        match &s.kind {
            StmtKind::Let { mutable, pat, ty, value } => {
                let want = ty.as_ref().map(|t| {
                    let g = cx.generics.clone();
                    self.resolve_type(t, file, &g)
                });
                let v = self.expr(cx, value, want.as_ref());
                let v = match &want {
                    Some(w) => self.expect(cx, v, w, "the value"),
                    None => v,
                };
                let diverges = v.ty == Ty::Never;
                if cx.infer.shallow(&v.ty) == Ty::Unit {
                    self.err(file, value.span, "this returns nothing, so there's no value to bind");
                }
                let lty = want.unwrap_or_else(|| v.ty.clone());
                match &pat.kind {
                    ast::PatKind::Bind(name) => {
                        let id = self.declare(cx, name, lty, LocalKind::Owned, *mutable, pat.span);
                        (Some(TStmt::Let { local: id, value: v }), diverges)
                    }
                    ast::PatKind::Wild => (Some(TStmt::Expr(v)), diverges),
                    ast::PatKind::Tuple(items) => {
                        let tys: Vec<Ty> = items.iter().map(|_| cx.infer.fresh(VarKind::Any)).collect();
                        if !cx.infer.unify(&lty, &Ty::Tuple(tys.clone())) {
                            let msg = format!("this value is `{}`, not a tuple of {} values", self.show(cx, &lty), items.len());
                            self.err(file, value.span, msg);
                        }
                        let mut locals = Vec::new();
                        for (p, t) in items.iter().zip(tys) {
                            match &p.kind {
                                ast::PatKind::Bind(name) => locals.push(Some(self.declare(cx, name, t, LocalKind::Owned, *mutable, p.span))),
                                ast::PatKind::Wild => locals.push(None),
                                _ => {
                                    self.err(file, p.span, "`let` takes names here, like `let (a, b) = pair`");
                                    locals.push(None);
                                }
                            }
                        }
                        (Some(TStmt::LetTuple { locals, value: v }), diverges)
                    }
                    _ => {
                        self.err(file, pat.span, "`let` takes a name or a tuple of names; use `match` for other patterns");
                        (None, diverges)
                    }
                }
            }
            StmtKind::Assign { target, op, value } => self.assign(cx, target, *op, value, s.span),
            StmtKind::Expr(e) => {
                let t = self.stmt_expr(cx, e);
                let t_ty = cx.infer.shallow(&t.ty);
                let ignorable = matches!(t.kind, TK::Dbg { .. });
                if !matches!(t_ty, Ty::Unit | Ty::Never | Ty::Error) && !ignorable {
                    let what = if matches!(e.kind, ExprKind::Call { .. }) { "the result of this call" } else { "this value" };
                    let hint = if let TK::Call { f, .. } = &t.kind {
                        let n = &self.fns[*f].name;
                        if matches!(n.as_str(), "str.trim" | "str.to_lower" | "str.to_upper" | "str.replace" | "str.trim_start" | "str.trim_end") {
                            " (it returns a new string: `s = s.trim()`)".to_string()
                        } else {
                            String::new()
                        }
                    } else {
                        String::new()
                    };
                    self.err(file, e.span, format!("{what} is unused{hint}; use it, or discard it with `_ = ...`"));
                }
                let diverges = t.ty == Ty::Never;
                (Some(TStmt::Expr(t)), diverges)
            }
            StmtKind::While { cond, body } => match cond {
                Cond::Expr(c) => {
                    let forever = matches!(c.kind, ExprKind::Bool(true));
                    let c = self.expr(cx, c, Some(&Ty::Bool));
                    let c = self.expect(cx, c, &Ty::Bool, "a `while` condition");
                    cx.frame().loops.push(false);
                    let (b, _) = self.block(cx, body, false, None);
                    let broke = cx.frame().loops.pop().unwrap();
                    (Some(TStmt::While { cond: c, body: b }), forever && !broke)
                }
                Cond::Let { name, value } => {
                    let v = self.expr(cx, value, None);
                    let inner = self.opt_inner(cx, &v, "`while let`");
                    cx.frame().scopes.push(Vec::new());
                    let local = self.declare(cx, &name.name, inner, LocalKind::Borrowed, false, name.span);
                    cx.frame().loops.push(false);
                    let (b, _) = self.block(cx, body, false, None);
                    cx.frame().loops.pop();
                    cx.frame().scopes.pop();
                    (Some(TStmt::WhileLet { local, value: v, body: b }), false)
                }
            },
            StmtKind::For { inout, pats, iter, body } => self.for_stmt(cx, *inout, pats, iter, body, s.span),
            StmtKind::Par(_) => {
                self.err(file, s.span, "`par` isn't supported by this compiler yet (planned for milestone 2)");
                (None, false)
            }
        }
    }

    /// The type inside an optional, reporting an error if `v` isn't one.
    pub fn opt_inner(&mut self, cx: &mut FnCx, v: &TExpr, what: &str) -> Ty {
        let inner = cx.infer.fresh(VarKind::Any);
        let t = cx.infer.shallow(&v.ty);
        if t == Ty::Error {
            return Ty::Error;
        }
        if !cx.infer.unify(&v.ty, &Ty::opt(inner.clone())) {
            let msg = format!("{what} needs an optional (`?T`), but this is `{}`", self.show(cx, &v.ty));
            self.err(cx.file, v.span, msg);
            return Ty::Error;
        }
        inner
    }

    fn for_stmt(&mut self, cx: &mut FnCx, inout: bool, pats: &[ast::Pattern], iter: &Expr, body: &ast::Block, span: Span) -> (Option<TStmt>, bool) {
        let file = cx.file;
        // `for i in a..b`
        if let ExprKind::Range { lo, hi, inclusive } = &iter.kind {
            let (Some(lo), Some(hi)) = (lo, hi) else {
                self.err(file, iter.span, "a `for` range needs both ends, like `0..n`");
                return (None, false);
            };
            if inout || pats.len() != 1 {
                self.err(file, span, "a `for` over a range takes one variable: `for i in 0..n`");
                return (None, false);
            }
            let int_var = cx.infer.fresh(VarKind::IntLit);
            let lo = self.expr(cx, lo, Some(&int_var));
            let lo = self.expect(cx, lo, &int_var, "the start of a range");
            let hi = self.expr(cx, hi, Some(&int_var));
            let hi = self.expect(cx, hi, &int_var, "the end of a range");
            if !matches!(cx.infer.shallow(&int_var), Ty::Int(_) | Ty::Var(_) | Ty::Error) {
                self.err(file, iter.span, "a range needs integers");
            }
            cx.frame().scopes.push(Vec::new());
            let local = self.bind_for_var(cx, &pats[0], int_var.clone());
            cx.frame().loops.push(false);
            let (b, _) = self.block(cx, body, false, None);
            cx.frame().loops.pop();
            cx.frame().scopes.pop();
            let local = local.unwrap_or_else(|| self.temp_local(cx, int_var, span));
            return (Some(TStmt::ForRange { local, lo, hi, inclusive: *inclusive, body: b }), false);
        }
        let it = self.expr(cx, iter, None);
        let ity = cx.infer.shallow(&it.ty);
        match ity {
            Ty::Array(elem) => {
                let (index_pat, elem_pat) = match pats {
                    [p] => (None, p),
                    [i, p] => (Some(i), p),
                    _ => unreachable!(),
                };
                let place = if inout {
                    match self.to_place(cx, &it, "`for inout`") {
                        Some(p) => Some(p),
                        None => return (None, false),
                    }
                } else {
                    None
                };
                cx.frame().scopes.push(Vec::new());
                let index = index_pat.and_then(|p| self.bind_for_var(cx, p, Ty::INT));
                let kind = if inout { LocalKind::Ref } else { LocalKind::Borrowed };
                let (elem_local, destructure) = match &elem_pat.kind {
                    ast::PatKind::Tuple(items) if !inout => {
                        let hidden = self.temp_local(cx, (*elem).clone(), elem_pat.span);
                        cx.frame().locals[hidden].kind = LocalKind::Borrowed;
                        (hidden, Some(items))
                    }
                    ast::PatKind::Bind(name) => (self.declare(cx, name, (*elem).clone(), kind, inout, elem_pat.span), None),
                    ast::PatKind::Wild => {
                        let hidden = self.temp_local(cx, (*elem).clone(), elem_pat.span);
                        cx.frame().locals[hidden].kind = kind;
                        (hidden, None)
                    }
                    _ => {
                        self.err(file, elem_pat.span, "`for` takes a name here, or a tuple of names");
                        cx.frame().scopes.pop();
                        return (None, false);
                    }
                };
                let mut pre_stmts = Vec::new();
                if let Some(items) = destructure {
                    let tys: Vec<Ty> = items.iter().map(|_| cx.infer.fresh(VarKind::Any)).collect();
                    if !cx.infer.unify(&elem, &Ty::Tuple(tys.clone())) {
                        let msg = format!("the elements are `{}`, not tuples", self.show(cx, &elem));
                        self.err(file, elem_pat.span, msg);
                    }
                    let mut locals = Vec::new();
                    for (p, t) in items.iter().zip(tys) {
                        match &p.kind {
                            ast::PatKind::Bind(name) => locals.push(Some(self.declare(cx, name, t, LocalKind::Owned, false, p.span))),
                            _ => locals.push(None),
                        }
                    }
                    let value = TExpr { kind: TK::Local(elem_local), ty: (*elem).clone(), span: elem_pat.span };
                    pre_stmts.push(TStmt::LetTuple { locals, value });
                }
                let root = place.as_ref().map(place_root);
                if let Some(r) = root {
                    cx.frame().iter_roots.push(r);
                }
                cx.frame().loops.push(false);
                let (mut b, _) = self.block(cx, body, false, None);
                cx.frame().loops.pop();
                if root.is_some() {
                    cx.frame().iter_roots.pop();
                }
                cx.frame().scopes.pop();
                if !pre_stmts.is_empty() {
                    pre_stmts.append(&mut b.stmts);
                    b.stmts = pre_stmts;
                }
                (Some(TStmt::ForArray { elem: elem_local, index, array: it, place, body: b }), false)
            }
            Ty::Adt(id, args) if id == self.known.map || id == self.known.set => {
                let is_set = id == self.known.set;
                if inout {
                    self.err(file, span, "`for inout` works on arrays; to change a map, set entries with `m[k] = v`");
                    return (None, false);
                }
                let (kty, vty) = if is_set { (args[0].clone(), None) } else { (args[0].clone(), Some(args[1].clone())) };
                cx.frame().scopes.push(Vec::new());
                let (key, value) = match (pats, &vty) {
                    ([k], _) => (self.bind_for_var(cx, k, kty.clone()), None),
                    ([k, v], Some(vt)) => (self.bind_for_var(cx, k, kty.clone()), self.bind_for_var(cx, v, vt.clone())),
                    _ => {
                        self.err(file, span, "a `for` over a set takes one variable: `for x in s`");
                        cx.frame().scopes.pop();
                        return (None, false);
                    }
                };
                cx.frame().loops.push(false);
                let (b, _) = self.block(cx, body, false, None);
                cx.frame().loops.pop();
                cx.frame().scopes.pop();
                let key = key.unwrap_or_else(|| {
                    let t = self.temp_local(cx, kty, span);
                    cx.frame().locals[t].kind = LocalKind::Borrowed;
                    t
                });
                if let Some(v) = value {
                    cx.frame().locals[v].kind = LocalKind::Borrowed;
                }
                cx.frame().locals[key].kind = LocalKind::Borrowed;
                (Some(TStmt::ForMap { key, value, map: it, is_set, body: b }), false)
            }
            Ty::Str => {
                self.err(file, iter.span, "to loop over a string, use `for b in s.bytes()` or `for r in s.runes()`");
                (None, false)
            }
            Ty::Error => (None, false),
            other => {
                let msg = format!("`for` works on arrays, maps, sets and ranges, but this is `{}`", self.show(cx, &other));
                self.err(file, iter.span, msg);
                (None, false)
            }
        }
    }

    fn bind_for_var(&mut self, cx: &mut FnCx, p: &ast::Pattern, ty: Ty) -> Option<LocalId> {
        match &p.kind {
            ast::PatKind::Bind(name) => Some(self.declare(cx, name, ty, LocalKind::Borrowed, false, p.span)),
            ast::PatKind::Wild => None,
            _ => {
                self.err(cx.file, p.span, "`for` takes a name here");
                None
            }
        }
    }

    fn assign(&mut self, cx: &mut FnCx, target: &Expr, op: AssignOp, value: &Expr, span: Span) -> (Option<TStmt>, bool) {
        let file = cx.file;
        if let ExprKind::Hole = target.kind {
            let v = self.expr(cx, value, None);
            let div = v.ty == Ty::Never;
            return (Some(TStmt::Expr(v)), div);
        }
        let bop = match op {
            AssignOp::Set => None,
            AssignOp::Add => Some(BinOp::Add),
            AssignOp::Sub => Some(BinOp::Sub),
            AssignOp::Mul => Some(BinOp::Mul),
            AssignOp::Div => Some(BinOp::Div),
            AssignOp::Rem => Some(BinOp::Rem),
        };
        // `m[k] = v` and `m[k] += v` on maps become calls to `set`.
        if let ExprKind::Index { base, args } = &target.kind {
            let b = self.expr(cx, base, None);
            if let Ty::Adt(id, targs) = cx.infer.shallow(&b.ty) {
                if id == self.known.map && args.len() == 1 {
                    return self.map_assign(cx, b, targs, &args[0], bop, value, span);
                }
            }
        }
        let t = self.expr(cx, target, None);
        let Some(place) = self.to_place(cx, &t, "assignment") else { return (None, false) };
        let ty = t.ty.clone();
        let v = self.expr(cx, value, Some(&ty));
        let v = self.expect(cx, v, &ty, "the new value");
        if let Some(op) = bop {
            let rt = cx.infer.shallow(&ty);
            let ok = match op {
                BinOp::Add => rt.is_numeric() || rt == Ty::Str || matches!(rt, Ty::Var(_)),
                _ => rt.is_numeric() || matches!(rt, Ty::Var(_)),
            };
            if !ok && rt != Ty::Error {
                let msg = format!("`{}` needs a number{}, but this is `{}`", op_assign_text(op), if op == BinOp::Add { " or a string" } else { "" }, self.show(cx, &ty));
                self.err(file, span, msg);
            }
        }
        let div = v.ty == Ty::Never;
        (Some(TStmt::Assign { place, op: bop, value: v, span }), div)
    }

    fn map_assign(&mut self, cx: &mut FnCx, map: TExpr, targs: Vec<Ty>, key: &Expr, op: Option<BinOp>, value: &Expr, span: Span) -> (Option<TStmt>, bool) {
        let Some(_) = self.to_place(cx, &map, "changing a map entry") else { return (None, false) };
        let (kty, vty) = (targs[0].clone(), targs[1].clone());
        let k = self.expr(cx, key, Some(&kty));
        let k = self.expect(cx, k, &kty, "the key");
        let v = self.expr(cx, value, Some(&vty));
        let v = self.expect(cx, v, &vty, "the value");
        let new_value = match op {
            None => v,
            Some(op) => {
                let old = self.map_index(cx, map.clone(), k.clone(), targs.clone(), span);
                let ty = vty.clone();
                if !matches!(cx.infer.shallow(&ty), Ty::Int(_) | Ty::Float(_) | Ty::Str | Ty::Var(_)) {
                    let msg = format!("`{}` needs a number, but the map's values are `{}`", op_assign_text(op), self.show(cx, &ty));
                    self.err(cx.file, span, msg);
                }
                TExpr { kind: TK::Binary(op, Box::new(old), Box::new(v)), ty, span }
            }
        };
        // Evaluate the new value first, then store it: `let tmp = value; m.set(k, tmp)`.
        let tmp = self.temp_local(cx, vty.clone(), span);
        let key_tmp = self.temp_local(cx, kty.clone(), span);
        let set = TExpr {
            kind: TK::Call {
                f: self.known.map_set,
                targs,
                eargs: Vec::new(),
                args: vec![
                    TArg { mode: Mode::Inout, expr: map, copy: false },
                    TArg { mode: Mode::Sink, expr: TExpr { kind: TK::Local(key_tmp), ty: kty, span }, copy: false },
                    TArg { mode: Mode::Sink, expr: TExpr { kind: TK::Local(tmp), ty: vty, span }, copy: false },
                ],
            },
            ty: Ty::Unit,
            span,
        };
        let block = TBlock {
            stmts: vec![TStmt::Let { local: key_tmp, value: k }, TStmt::Let { local: tmp, value: new_value }, TStmt::Expr(set)],
            value: None,
        };
        (Some(TStmt::Expr(TExpr { kind: TK::Block(block), ty: Ty::Unit, span })), false)
    }

    /// `m[k]`: the value, or a trap naming the missing key.
    pub fn map_index(&mut self, _cx: &mut FnCx, map: TExpr, key: TExpr, targs: Vec<Ty>, span: Span) -> TExpr {
        let vty = targs[1].clone();
        let key_shown = TExpr { kind: key.kind.clone(), ty: key.ty.clone(), span };
        let get = TExpr {
            kind: TK::Call {
                f: self.known.map_get,
                targs,
                eargs: Vec::new(),
                args: vec![TArg { mode: Mode::Read, expr: map, copy: false }, TArg { mode: Mode::Read, expr: key, copy: false }],
            },
            ty: Ty::opt(vty.clone()),
            span,
        };
        let msg = TExpr {
            kind: TK::Interp(vec![TExpr { kind: TK::Str(b"key not found in map: ".to_vec()), ty: Ty::Str, span }, key_shown]),
            ty: Ty::Str,
            span,
        };
        let trap = TExpr { kind: TK::Trap(Box::new(msg)), ty: Ty::Never, span };
        TExpr { kind: TK::ElseOpt { value: Box::new(get), alt: Box::new(trap) }, ty: vty, span }
    }

    /// Converts an expression that names a place (a variable, a field, an
    /// array element) into that place, checking it can be changed.
    pub fn to_place(&mut self, cx: &mut FnCx, t: &TExpr, what: &str) -> Option<TPlace> {
        let file = cx.file;
        match &t.kind {
            TK::Local(id) => {
                let f = cx.fr();
                let name = f.locals[*id].name.clone();
                if f.iter_roots.contains(id) {
                    self.err(file, t.span, format!("`{name}` is being changed by the enclosing `for inout`, so it can't be used here"));
                    return None;
                }
                if f.captures.iter().any(|(_, inner)| inner == id) {
                    self.err(file, t.span, format!("closures capture copies, so this closure can't change `{name}`; return a value instead"));
                    return None;
                }
                if !f.mutable[*id] {
                    let msg = match f.locals[*id].kind {
                        LocalKind::Borrowed if name == "self" => "`self` is read-only here; declare the method with `inout self` to change it".to_string(),
                        LocalKind::Borrowed | LocalKind::Owned if cx.frames.len() == 1 && self.is_param(cx, *id) => {
                            format!("`{name}` is a read-only parameter; copy it into a variable (`var {name}2 = {name}`), or declare it `{name}: inout T`")
                        }
                        _ => format!("`{name}` can't change: declare it with `var {name} = ...`"),
                    };
                    let msg = if what == "assignment" { msg } else { format!("{what}: {msg}") };
                    self.err(file, t.span, msg);
                    return None;
                }
                Some(TPlace::Local(*id))
            }
            TK::Field { base, index } => {
                if matches!(cx.infer.shallow(&base.ty), Ty::Tuple(_)) {
                    self.err(file, t.span, "tuples can't be changed in place; build a new one");
                    return None;
                }
                let b = self.to_place(cx, base, what)?;
                Some(TPlace::Field(Box::new(b), *index))
            }
            TK::Index { base, index } => {
                if cx.infer.shallow(&base.ty) == Ty::Str {
                    self.err(file, t.span, "strings can't be changed in place; build a new one with `+` or `\"${...}\"`");
                    return None;
                }
                let b = self.to_place(cx, base, what)?;
                Some(TPlace::Index(Box::new(b), index.clone()))
            }
            TK::ElseOpt { .. } => {
                self.err(file, t.span, "a map entry can't be changed in place; copy it out with `var e = m[k]`, change `e`, then `m[k] = e`");
                None
            }
            _ => {
                self.err(file, t.span, format!("{what} needs a variable, a field or an array element here"));
                None
            }
        }
    }

    fn is_param(&self, cx: &FnCx, id: LocalId) -> bool {
        let owner = cx.owner;
        owner != usize::MAX && id < self.fns[owner].params.len()
    }

    // ---- expressions ----

    /// An expression whose value is used. A call that can fail must be handled.
    pub fn expr(&mut self, cx: &mut FnCx, e: &Expr, want: Option<&Ty>) -> TExpr {
        let t = self.expr_inner(cx, e, want, true);
        self.check_handled(cx, &t);
        t
    }

    /// An expression used as a statement: `if` and `match` needn't produce values.
    fn stmt_expr(&mut self, cx: &mut FnCx, e: &Expr) -> TExpr {
        let t = self.expr_inner(cx, e, None, false);
        self.check_handled(cx, &t);
        t
    }

    fn check_handled(&mut self, cx: &mut FnCx, t: &TExpr) {
        if self.failable(cx, t) {
            let name = self.callee_name(t);
            self.err(cx.file, t.span, format!("`{name}` can fail; handle it with `?`, `else` or `catch`"));
        }
    }

    pub fn expr_inner(&mut self, cx: &mut FnCx, e: &Expr, want: Option<&Ty>, value: bool) -> TExpr {
        let span = e.span;
        let file = cx.file;
        let mk = |kind, ty| TExpr { kind, ty, span };
        match &e.kind {
            ExprKind::Int(s) => {
                let Some(v) = parse_int(s) else {
                    self.err(file, span, format!("`{s}` is too large for any integer type"));
                    return self.error_expr(span);
                };
                let ty = match want.map(|w| cx.infer.shallow(w)) {
                    Some(t @ (Ty::Int(_) | Ty::Float(_))) => t,
                    _ => cx.infer.fresh(VarKind::IntLit),
                };
                mk(TK::Int(v), ty)
            }
            ExprKind::Float(s) => {
                let v: f64 = s.replace('_', "").parse().unwrap_or(0.0);
                let ty = match want.map(|w| cx.infer.shallow(w)) {
                    Some(t @ Ty::Float(_)) => t,
                    _ => cx.infer.fresh(VarKind::FloatLit),
                };
                mk(TK::Float(v), ty)
            }
            ExprKind::Dur(s) => match parse_dur(s) {
                Some(ns) => mk(TK::Int(ns), Ty::Dur),
                None => {
                    self.err(file, span, format!("`{s}` is too large for a duration"));
                    self.error_expr(span)
                }
            },
            ExprKind::Byte(s) => match decode_byte(s) {
                Some(b) => mk(TK::Int(b as i128), Ty::U8),
                None => {
                    self.err(file, span, "a byte literal holds one ASCII character, like 'a' or '\\n'");
                    self.error_expr(span)
                }
            },
            ExprKind::Str(s) => self.str_lit(cx, s),
            ExprKind::Bool(b) => mk(TK::Bool(*b), Ty::Bool),
            ExprKind::None => {
                let inner = cx.infer.fresh(VarKind::Any);
                mk(TK::None, Ty::opt(inner))
            }
            ExprKind::SelfRef => match self.lookup_local(cx, "self") {
                Some(id) => mk(TK::Local(id), cx.local_ty(id)),
                None => {
                    self.err(file, span, "`self` only exists in methods");
                    self.error_expr(span)
                }
            },
            ExprKind::Ident(name) => self.ident_expr(cx, name, span, want),
            ExprKind::Hole => {
                let fits: Vec<String> = cx
                    .fr()
                    .scopes
                    .iter()
                    .flatten()
                    .filter(|(_, id)| want.is_none_or(|w| cx.infer.resolve(&cx.fr().locals[*id].ty) == cx.infer.resolve(w)))
                    .map(|(n, _)| n.clone())
                    .filter(|n| !n.starts_with('<'))
                    .collect();
                let ty = want.map(|w| format!("a value of type `{}`", self.show(cx, w))).unwrap_or_else(|| "a value".into());
                let fits = if fits.is_empty() { "nothing in scope fits".into() } else { format!("in scope: {}", fits.join(", ")) };
                self.err(file, span, format!("hole: {ty} goes here; {fits}"));
                self.error_expr(span)
            }
            ExprKind::Variant(name) => self.variant_value(cx, None, name, None, span, want),
            ExprKind::Paren(inner) => self.expr_inner(cx, inner, want, value),
            ExprKind::Tuple(items) => {
                if items.is_empty() {
                    return mk(TK::Unit, Ty::Unit);
                }
                let wants: Vec<Option<Ty>> = match want.map(|w| cx.infer.shallow(w)) {
                    Some(Ty::Tuple(ts)) if ts.len() == items.len() => ts.into_iter().map(Some).collect(),
                    _ => vec![None; items.len()],
                };
                let mut out = Vec::new();
                for (it, w) in items.iter().zip(wants) {
                    let t = self.expr(cx, it, w.as_ref());
                    let t = match &w {
                        Some(w) => self.expect(cx, t, w, "this element"),
                        None => t,
                    };
                    out.push(t);
                }
                let ty = Ty::Tuple(out.iter().map(|t| t.ty.clone()).collect());
                mk(TK::Tuple(out), ty)
            }
            ExprKind::Array { items, .. } => {
                let elem = match want.map(|w| cx.infer.shallow(w)) {
                    Some(Ty::Array(e)) => *e,
                    _ => cx.infer.fresh(VarKind::Any),
                };
                let mut out = Vec::new();
                for it in items {
                    let t = self.expr(cx, it, Some(&elem));
                    out.push(self.expect(cx, t, &elem, "an array element"));
                }
                mk(TK::Array(out), Ty::array(elem))
            }
            ExprKind::Map { entries, .. } => self.map_literal(cx, entries, want, span),
            ExprKind::Set { items, .. } => self.set_literal(cx, items, want, span),
            ExprKind::EmptyBraces => {
                let w = want.map(|w| cx.infer.shallow(w));
                match w {
                    Some(Ty::Adt(id, _)) if id == self.known.map => self.map_literal(cx, &[], want, span),
                    Some(Ty::Adt(id, _)) if id == self.known.set => self.set_literal(cx, &[], want, span),
                    _ => {
                        self.err(file, span, "`{}` needs a known type here, like `let m: Map[str, int] = {}`");
                        self.error_expr(span)
                    }
                }
            }
            ExprKind::Field(base, name) => self.field_expr(cx, base, name, span, want),
            ExprKind::Call { callee, args, .. } => self.call_expr(cx, callee, args, span, want),
            ExprKind::Index { base, args } => self.index_expr(cx, base, args, span),
            ExprKind::Type(_) => {
                self.err(file, span, "a type can't be used as a value here");
                self.error_expr(span)
            }
            ExprKind::Unary(op, inner) => {
                if let (UnOp::Neg, ExprKind::Int(s)) = (op, &inner.kind) {
                    let Some(v) = parse_int(s) else { return self.error_expr(span) };
                    let ty = match want.map(|w| cx.infer.shallow(w)) {
                        Some(t @ (Ty::Int(_) | Ty::Float(_))) => t,
                        _ => cx.infer.fresh(VarKind::IntLit),
                    };
                    return mk(TK::Int(-v), ty);
                }
                if let (UnOp::Neg, ExprKind::Float(s)) = (op, &inner.kind) {
                    let v: f64 = s.replace('_', "").parse().unwrap_or(0.0);
                    let ty = match want.map(|w| cx.infer.shallow(w)) {
                        Some(t @ Ty::Float(_)) => t,
                        _ => cx.infer.fresh(VarKind::FloatLit),
                    };
                    return mk(TK::Float(-v), ty);
                }
                let t = self.expr(cx, inner, want);
                let ty = cx.infer.shallow(&t.ty);
                match op {
                    UnOp::Neg => {
                        let ok = match &ty {
                            Ty::Int(k) => k.signed(),
                            Ty::Float(_) | Ty::Var(_) | Ty::Error => true,
                            _ => false,
                        };
                        if !ok {
                            let msg = format!("`-` needs a signed number, but this is `{}`", self.show(cx, &ty));
                            self.err(file, span, msg);
                        }
                    }
                    UnOp::Not => {
                        let t2 = &t;
                        if !cx.infer.unify(&t2.ty, &Ty::Bool) {
                            let msg = format!("`!` needs a `bool`, but this is `{}`", self.show(cx, &ty));
                            self.err(file, span, msg);
                        }
                    }
                }
                let rty = t.ty.clone();
                mk(TK::Unary(*op, Box::new(t)), rty)
            }
            ExprKind::Binary(op, l, r) => self.binary(cx, *op, l, r, span),
            ExprKind::Range { .. } => {
                self.err(file, span, "ranges are only used in `for` and in slices like `xs[a..b]`");
                self.error_expr(span)
            }
            ExprKind::Try(inner) => {
                let t = self.expr_inner(cx, inner, want, true);
                if t.ty == Ty::Error {
                    return t;
                }
                if !self.failable(cx, &t) {
                    let what = if matches!(cx.infer.shallow(&t.ty), Ty::Opt(_)) {
                        "`?` passes on failures, and this is an optional; use `else` to handle `none`"
                    } else {
                        "`?` goes after a call to a function that can fail"
                    };
                    self.err(file, span, what);
                    return t;
                }
                let name = self.callee_name(&t);
                self.use_effects(cx, &Eff { fail: true, ..Eff::pure() }, span, &name);
                let ty = t.ty.clone();
                mk(TK::Try(Box::new(t)), ty)
            }
            ExprKind::Else(l, r) => {
                let t = self.expr_inner(cx, l, None, true);
                if self.failable(cx, &t) {
                    // `int.parse(s) else none` where an optional is expected: the result is optional.
                    let ty = self.optional_result(cx, &t.ty, want).unwrap_or_else(|| t.ty.clone());
                    let alt = self.alt_expr(cx, r, &ty);
                    return mk(TK::ElseFail { value: Box::new(t), alt: Box::new(alt) }, ty);
                }
                let inner = self.opt_inner(cx, &t, "`else`");
                let alt = self.alt_expr(cx, r, &inner);
                mk(TK::ElseOpt { value: Box::new(t), alt: Box::new(alt) }, inner)
            }
            ExprKind::Catch { expr, name, body } => {
                let t = self.expr_inner(cx, expr, want, true);
                if !self.failable(cx, &t) {
                    if t.ty != Ty::Error {
                        self.err(file, span, "`catch` goes after a call to a function that can fail");
                    }
                    return t;
                }
                let ty = self.optional_result(cx, &t.ty, want).unwrap_or_else(|| t.ty.clone());
                cx.frame().scopes.push(Vec::new());
                let err_ty = Ty::Adt(self.known.err, Vec::new());
                let local = self.declare(cx, &name.name, err_ty, LocalKind::Owned, false, name.span);
                let (b, _) = self.block(cx, body, true, Some(&ty));
                cx.frame().scopes.pop();
                mk(TK::Catch { value: Box::new(t), local, body: b }, ty)
            }
            ExprKind::If { cond, then, els } => self.if_expr(cx, cond, then, els.as_deref(), span, want, value),
            ExprKind::Match { scrutinee, arms, .. } => self.match_expr(cx, scrutinee, arms, span, want, value),
            ExprKind::Lock { .. } => {
                self.err(file, span, "`lock` isn't supported by this compiler yet (planned for milestone 2)");
                self.error_expr(span)
            }
            ExprKind::Unsafe(_) => {
                self.err(file, span, "`unsafe` isn't supported by this compiler yet (planned for milestone 5)");
                self.error_expr(span)
            }
            ExprKind::Block(b) => {
                let (tb, ty) = self.block(cx, b, value, want);
                mk(TK::Block(tb), ty)
            }
            ExprKind::Closure { params, body } => self.closure(cx, params, body, span, want),
            ExprKind::Return(v) => {
                let ret = cx.fr().ret.clone();
                let tv = v.as_ref().map(|v| {
                    let t = self.expr(cx, v, Some(&ret));
                    self.expect(cx, t, &ret, "the returned value")
                });
                let name = cx.fr().name.clone();
                match (&tv, cx.infer.shallow(&ret)) {
                    (None, Ty::Unit | Ty::Error) => {}
                    (None, Ty::Var(_)) => {
                        cx.infer.unify(&ret, &Ty::Unit);
                    }
                    (None, r) => {
                        let msg = format!("`{name}` returns `{}`; write `return <value>`", self.show(cx, &r));
                        self.err(file, span, msg);
                    }
                    (Some(t), Ty::Unit) if !t.ty.is_bad() => {
                        let msg = format!("`{name}` returns nothing; remove the value, or add `-> {}` to its signature", self.show(cx, &t.ty));
                        self.err(file, t.span, msg);
                    }
                    _ => {}
                }
                mk(TK::Return(tv.map(Box::new)), Ty::Never)
            }
            ExprKind::Break | ExprKind::Continue => {
                if cx.fr().loops.is_empty() {
                    self.err(file, span, "`break` and `continue` only work inside a loop");
                } else if matches!(e.kind, ExprKind::Break) {
                    *cx.frame().loops.last_mut().unwrap() = true;
                }
                mk(if matches!(e.kind, ExprKind::Break) { TK::Break } else { TK::Continue }, Ty::Never)
            }
        }
    }

    /// When an optional `?T` is expected and a call returns a plain `T`, the
    /// optional type: the call's value is wrapped, and the fallback may be `none`.
    fn optional_result(&mut self, cx: &mut FnCx, value_ty: &Ty, want: Option<&Ty>) -> Option<Ty> {
        let w = cx.infer.shallow(want?);
        let Ty::Opt(inner) = &w else { return None };
        let v = cx.infer.shallow(value_ty);
        if matches!(v, Ty::Opt(_) | Ty::Var(_) | Ty::Error | Ty::Never) {
            return None;
        }
        cx.infer.unify(value_ty, inner).then_some(w)
    }

    /// The right side of `else`: a value of type `ty`, a block, or something that leaves.
    fn alt_expr(&mut self, cx: &mut FnCx, r: &Expr, ty: &Ty) -> TExpr {
        let t = match &r.kind {
            ExprKind::Block(b) => {
                let (tb, bty) = self.block(cx, b, true, Some(ty));
                TExpr { kind: TK::Block(tb), ty: bty, span: r.span }
            }
            _ => self.expr(cx, r, Some(ty)),
        };
        if t.ty == Ty::Never {
            return t;
        }
        self.expect(cx, t, ty, "the value after `else`")
    }

    fn ident_expr(&mut self, cx: &mut FnCx, name: &str, span: Span, want: Option<&Ty>) -> TExpr {
        let file = cx.file;
        match self.resolve_name(cx, name) {
            Named::Local(id) => TExpr { kind: TK::Local(id), ty: cx.local_ty(id), span },
            Named::Const(c) => match self.const_value(c) {
                Some(mut v) => {
                    v.span = span;
                    v
                }
                None => self.error_expr(span),
            },
            Named::Fn(f) => {
                if is_builtin_intrinsic(&self.fns[f].name) && self.files[self.fns[f].file].std {
                    self.err(file, span, format!("`{name}` must be called: `{name}(...)`"));
                    return self.error_expr(span);
                }
                self.fn_value(cx, f, span, want)
            }
            Named::Type(TypeRef::Adt(id)) => {
                let n = self.adts[id].name.clone();
                if self.adts[id].is_enum() {
                    self.err(file, span, format!("`{n}` is a type; its values are its variants, like `{n}.{}`", self.adts[id].variants().first().map(|v| v.name.as_str()).unwrap_or("Name")));
                } else {
                    self.err(file, span, format!("`{n}` is a type; make a value with `{n}(field: ...)`"));
                }
                self.error_expr(span)
            }
            Named::Type(TypeRef::Alias(_)) | Named::Builtin(_) => {
                self.err(file, span, format!("`{name}` is a type, not a value"));
                self.error_expr(span)
            }
            Named::Module(m) => {
                self.err(file, span, format!("`{m}` is a module; use one of its functions, like `{m}.name(...)`"));
                self.error_expr(span)
            }
            Named::Unknown => {
                self.unknown_name(cx, name, span);
                self.error_expr(span)
            }
        }
    }

    /// A named function used as a value.
    pub fn fn_value(&mut self, cx: &mut FnCx, f: FnId, span: Span, want: Option<&Ty>) -> TExpr {
        let (targs, eargs) = self.instantiate(cx, f, span);
        let def = &self.fns[f];
        let ty = Ty::Fn(Box::new(FnTy {
            params: def.params.iter().map(|p| p.ty.subst(&targs, &eargs)).collect(),
            ret: def.ret.subst(&targs, &eargs),
            eff: def.eff.subst(&eargs),
        }));
        if def.params.iter().any(|p| p.mode == Mode::Inout) {
            let n = def.name.clone();
            self.err(cx.file, span, format!("`{n}` changes an argument (`inout`), so it can't be used as a value"));
        }
        let t = TExpr { kind: TK::FnValue { f, targs, eargs }, ty, span };
        if let Some(w) = want {
            if let Ty::Fn(_) = cx.infer.shallow(w) {
                return self.coerce_fn(cx, t, w);
            }
        }
        t
    }

    /// Fits a function value to an expected function type: effects may be fewer.
    pub fn coerce_fn(&mut self, cx: &mut FnCx, t: TExpr, want: &Ty) -> TExpr {
        let (Ty::Fn(a), Ty::Fn(w)) = (cx.infer.shallow(&t.ty), cx.infer.shallow(want)) else { return t };
        let mut ok = a.params.len() == w.params.len();
        if ok {
            for (x, y) in a.params.iter().zip(&w.params) {
                ok &= cx.infer.unify(x, y);
            }
            ok &= cx.infer.unify(&a.ret, &w.ret);
        }
        if !ok {
            let msg = format!("expected a function `{}`, found `{}`", self.show(cx, want), self.show(cx, &t.ty));
            self.err(cx.file, t.span, msg);
            return t;
        }
        if !cx.infer.sub_eff(&a.eff, &w.eff) {
            let msg = format!(
                "this function has effects that aren't allowed here: expected `{}`, found `{}`",
                self.show(cx, want),
                self.show(cx, &t.ty)
            );
            self.err(cx.file, t.span, msg);
        }
        TExpr { ty: want.clone(), ..t }
    }

    fn str_lit(&mut self, cx: &mut FnCx, s: &ast::StrLit) -> TExpr {
        let span = s.span;
        let has_interp = s.parts.iter().any(|p| matches!(p, StrPart::Interp(_)));
        let mut parts = Vec::new();
        for p in &s.parts {
            match p {
                StrPart::Text(t) => match self.decode_text(t, s.triple, cx.file, span) {
                    Some(b) => parts.push(TExpr { kind: TK::Str(b), ty: Ty::Str, span }),
                    None => return self.error_expr(span),
                },
                StrPart::Interp(e) => {
                    let t = self.expr(cx, e, None);
                    let ty = cx.infer.shallow(&t.ty);
                    if ty == Ty::Unit {
                        self.err(cx.file, e.span, "this returns nothing, so there's nothing to put in the string");
                    } else if matches!(ty, Ty::Fn(_)) {
                        self.err(cx.file, e.span, "a function can't be put in a string");
                    }
                    parts.push(t);
                }
            }
        }
        if !has_interp {
            let bytes = parts.into_iter().flat_map(|p| if let TK::Str(b) = p.kind { b } else { Vec::new() }).collect();
            return TExpr { kind: TK::Str(bytes), ty: Ty::Str, span };
        }
        TExpr { kind: TK::Interp(parts), ty: Ty::Str, span }
    }

    pub fn decode_plain(&mut self, s: &ast::StrLit, file: FileId) -> Option<String> {
        let mut out = Vec::new();
        for p in &s.parts {
            match p {
                StrPart::Text(t) => out.extend(self.decode_text(t, s.triple, file, s.span)?),
                StrPart::Interp(e) => {
                    self.err(file, e.span, "no `${...}` here; write the text as it is");
                    return None;
                }
            }
        }
        String::from_utf8(out).ok()
    }

    fn decode_text(&mut self, t: &str, triple: bool, file: FileId, span: Span) -> Option<Vec<u8>> {
        let b = t.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] != b'\\' {
                out.push(b[i]);
                i += 1;
                continue;
            }
            let e = b.get(i + 1).copied().unwrap_or(0);
            if triple {
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
                    match t.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                        Some(v) if v < 0x80 => out.push(v),
                        _ => {
                            self.err(file, span, "`\\x` takes two hex digits for an ASCII byte, like `\\x41`");
                            return None;
                        }
                    }
                    i += 2;
                }
                b'u' => {
                    let rest = &t[i..];
                    let parsed = if rest.starts_with('{') {
                        rest.find('}').and_then(|c| u32::from_str_radix(&rest[1..c], 16).ok().and_then(char::from_u32).map(|ch| (ch, c)))
                    } else {
                        None
                    };
                    match parsed {
                        Some((ch, c)) => {
                            let mut buf = [0; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                            i += c + 1;
                        }
                        None => {
                            self.err(file, span, "`\\u` takes a code point in braces, like `\\u{e9}`");
                            return None;
                        }
                    }
                }
                _ => {
                    self.err(file, span, "unknown escape in string");
                    return None;
                }
            }
        }
        Some(out)
    }

    fn map_literal(&mut self, cx: &mut FnCx, entries: &[(Expr, Expr)], want: Option<&Ty>, span: Span) -> TExpr {
        let (kty, vty) = match want.map(|w| cx.infer.shallow(w)) {
            Some(Ty::Adt(id, args)) if id == self.known.map => (args[0].clone(), args[1].clone()),
            _ => (cx.infer.fresh(VarKind::Any), cx.infer.fresh(VarKind::Any)),
        };
        let targs = vec![kty.clone(), vty.clone()];
        let map_ty = Ty::Adt(self.known.map, targs.clone());
        self.require_bound(cx, &kty, "Hash", span);
        self.require_bound(cx, &kty, "Eq", span);
        let m = self.temp_local(cx, map_ty.clone(), span);
        let new = TExpr { kind: TK::Call { f: self.known.map_new, targs: targs.clone(), eargs: Vec::new(), args: Vec::new() }, ty: map_ty.clone(), span };
        let mut stmts = vec![TStmt::Let { local: m, value: new }];
        for (k, v) in entries {
            let kt = self.expr(cx, k, Some(&kty));
            let kt = self.expect(cx, kt, &kty, "a key");
            let vt = self.expr(cx, v, Some(&vty));
            let vt = self.expect(cx, vt, &vty, "a value");
            let call = TK::Call {
                f: self.known.map_set,
                targs: targs.clone(),
                eargs: Vec::new(),
                args: vec![
                    TArg { mode: Mode::Inout, expr: TExpr { kind: TK::Local(m), ty: map_ty.clone(), span }, copy: false },
                    TArg { mode: Mode::Sink, expr: kt, copy: false },
                    TArg { mode: Mode::Sink, expr: vt, copy: false },
                ],
            };
            stmts.push(TStmt::Expr(TExpr { kind: call, ty: Ty::Unit, span }));
        }
        let value = Some(Box::new(TExpr { kind: TK::Local(m), ty: map_ty.clone(), span }));
        TExpr { kind: TK::Block(TBlock { stmts, value }), ty: map_ty, span }
    }

    fn set_literal(&mut self, cx: &mut FnCx, items: &[Expr], want: Option<&Ty>, span: Span) -> TExpr {
        let ety = match want.map(|w| cx.infer.shallow(w)) {
            Some(Ty::Adt(id, args)) if id == self.known.set => args[0].clone(),
            _ => cx.infer.fresh(VarKind::Any),
        };
        let targs = vec![ety.clone()];
        let set_ty = Ty::Adt(self.known.set, targs.clone());
        self.require_bound(cx, &ety, "Hash", span);
        self.require_bound(cx, &ety, "Eq", span);
        let s = self.temp_local(cx, set_ty.clone(), span);
        let new = TExpr { kind: TK::Call { f: self.known.set_new, targs: targs.clone(), eargs: Vec::new(), args: Vec::new() }, ty: set_ty.clone(), span };
        let mut stmts = vec![TStmt::Let { local: s, value: new }];
        for it in items {
            let t = self.expr(cx, it, Some(&ety));
            let t = self.expect(cx, t, &ety, "an element");
            let call = TK::Call {
                f: self.known.set_insert,
                targs: targs.clone(),
                eargs: Vec::new(),
                args: vec![TArg { mode: Mode::Inout, expr: TExpr { kind: TK::Local(s), ty: set_ty.clone(), span }, copy: false }, TArg { mode: Mode::Sink, expr: t, copy: false }],
            };
            stmts.push(TStmt::Expr(TExpr { kind: call, ty: Ty::Bool, span }));
        }
        let value = Some(Box::new(TExpr { kind: TK::Local(s), ty: set_ty.clone(), span }));
        TExpr { kind: TK::Block(TBlock { stmts, value }), ty: set_ty, span }
    }

    fn if_expr(&mut self, cx: &mut FnCx, cond: &Cond, then: &ast::Block, els: Option<&Expr>, span: Span, want: Option<&Ty>, value: bool) -> TExpr {
        let file = cx.file;
        // In a value position the branches must agree on a type.
        let result = if value { Some(want.cloned().unwrap_or_else(|| cx.infer.fresh(VarKind::Any))) } else { None };
        let (then_b, then_ty, local, cond_t) = match cond {
            Cond::Expr(c) => {
                let c = self.expr(cx, c, Some(&Ty::Bool));
                let c = self.expect(cx, c, &Ty::Bool, "an `if` condition");
                let (b, t) = self.block(cx, then, value, result.as_ref());
                (b, t, None, c)
            }
            Cond::Let { name, value: v } => {
                let t = self.expr(cx, v, None);
                let inner = self.opt_inner(cx, &t, "`if let`");
                cx.frame().scopes.push(Vec::new());
                let local = self.declare(cx, &name.name, inner, LocalKind::Borrowed, false, name.span);
                let (b, bt) = self.block(cx, then, value, result.as_ref());
                cx.frame().scopes.pop();
                (b, bt, Some(local), t)
            }
        };
        let (els_b, els_ty) = match els {
            None => {
                if value && result.as_ref().is_some_and(|r| cx.infer.shallow(r) != Ty::Unit) {
                    self.err(file, span, "an `if` used as a value needs an `else`");
                }
                (None, Ty::Unit)
            }
            Some(Expr { kind: ExprKind::Block(b), .. }) => {
                let (b, t) = self.block(cx, b, value, result.as_ref());
                (Some(b), t)
            }
            Some(nested) => {
                let t = self.expr_inner(cx, nested, result.as_ref(), value);
                let ty = t.ty.clone();
                (Some(TBlock { stmts: Vec::new(), value: Some(Box::new(t)) }), ty)
            }
        };
        let ty = if then_ty == Ty::Never && els_ty == Ty::Never && els_b.is_some() {
            Ty::Never
        } else if value {
            result.unwrap()
        } else {
            Ty::Unit
        };
        let kind = match local {
            None => TK::If { cond: Box::new(cond_t), then: then_b, els: els_b },
            Some(l) => TK::IfLet { local: l, value: Box::new(cond_t), then: then_b, els: els_b },
        };
        TExpr { kind, ty, span }
    }

    fn closure(&mut self, cx: &mut FnCx, params: &[ast::ClosureParam], body: &Expr, span: Span, want: Option<&Ty>) -> TExpr {
        let file = cx.file;
        let expected = match want.map(|w| cx.infer.shallow(w)) {
            Some(Ty::Fn(f)) if f.params.len() == params.len() => Some(f),
            Some(Ty::Fn(f)) => {
                self.err(file, span, format!("this closure takes {} parameter{}, but {} {} expected here", params.len(), if params.len() == 1 { "" } else { "s" }, f.params.len(), if f.params.len() == 1 { "is" } else { "are" }));
                None
            }
            _ => None,
        };
        let ret = expected.as_ref().map(|f| f.ret.clone()).unwrap_or_else(|| cx.infer.fresh(VarKind::Any));
        let name = "this closure".to_string();
        cx.frames.push(Frame::new(name, ret.clone(), None));
        let mut ptys = Vec::new();
        let mut pids = Vec::new();
        for (i, p) in params.iter().enumerate() {
            let ty = match &p.ty {
                Some(t) => {
                    let g = cx.generics.clone();
                    let t = self.resolve_type(t, file, &g);
                    if let Some(f) = &expected {
                        cx.infer.unify(&t, &f.params[i]);
                    }
                    t
                }
                None => expected.as_ref().map(|f| f.params[i].clone()).unwrap_or_else(|| cx.infer.fresh(VarKind::Any)),
            };
            ptys.push(ty.clone());
            pids.push(self.declare(cx, &p.name.name, ty, LocalKind::Borrowed, false, p.name.span));
        }
        let (block, value_ty) = match &body.kind {
            ExprKind::Block(b) => {
                let (tb, t) = self.block(cx, b, false, None);
                if t != Ty::Never && cx.infer.shallow(&ret) != Ty::Unit {
                    if matches!(cx.infer.shallow(&ret), Ty::Var(_)) {
                        cx.infer.unify(&ret, &Ty::Unit);
                    } else if !matches!(cx.infer.shallow(&ret), Ty::Error) {
                        let msg = format!("this closure returns `{}`, but can reach the end without `return`", self.show(cx, &ret));
                        self.err(file, body.span, msg);
                    }
                }
                (tb, None)
            }
            _ => {
                let t = self.expr(cx, body, Some(&ret));
                let t = self.expect(cx, t, &ret, "the closure's result");
                let ty = t.ty.clone();
                (TBlock { stmts: Vec::new(), value: Some(Box::new(t)) }, Some(ty))
            }
        };
        let _ = value_ty;
        let frame = cx.frames.pop().unwrap();
        let eff = frame.used.clone();
        // The closure's effects count as used where the closure is called, not here.
        let id = self.closures.len();
        self.closures.push(ClosureDef {
            owner: cx.owner,
            params: pids,
            captures: frame.captures,
            ret: ret.clone(),
            eff: eff.clone(),
            body: Body { locals: frame.locals, params: Vec::new(), pre: Vec::new(), block },
        });
        cx.closures.push(id);
        let ty = Ty::Fn(Box::new(FnTy { params: ptys, ret, eff }));
        let t = TExpr { kind: TK::Closure { id }, ty, span };
        match want {
            Some(w) if matches!(cx.infer.shallow(w), Ty::Fn(_)) => self.coerce_fn(cx, t, w),
            _ => t,
        }
    }

    fn binary(&mut self, cx: &mut FnCx, op: BinOp, l: &Expr, r: &Expr, span: Span) -> TExpr {
        let file = cx.file;
        let mk = |kind, ty| TExpr { kind, ty, span };
        if matches!(op, BinOp::And | BinOp::Or) {
            let lt = self.expr(cx, l, Some(&Ty::Bool));
            let lt = self.expect(cx, lt, &Ty::Bool, &format!("the left side of `{}`", op.text()));
            let rt = self.expr(cx, r, Some(&Ty::Bool));
            let rt = self.expect(cx, rt, &Ty::Bool, &format!("the right side of `{}`", op.text()));
            return mk(TK::Binary(op, Box::new(lt), Box::new(rt)), Ty::Bool);
        }
        let lt = self.expr(cx, l, None);
        let lty = lt.ty.clone();
        let rt = self.expr(cx, r, Some(&lty));
        // `x == none` and comparisons of an optional with a plain value.
        let rt = if op.is_comparison() { self.expect(cx, rt, &lty, &format!("the right side of `{}`", op.text())) } else { rt };
        let ty = cx.infer.shallow(&lty);
        if ty == Ty::Error || cx.infer.shallow(&rt.ty) == Ty::Error {
            let rty = if op.is_comparison() { Ty::Bool } else { Ty::Error };
            return mk(TK::Binary(op, Box::new(lt), Box::new(rt)), rty);
        }
        if op.is_comparison() {
            let bound = if matches!(op, BinOp::Eq | BinOp::Ne) { "Eq" } else { "Ord" };
            // Maps and sets compare with their `equals` method.
            if let Ty::Adt(id, targs) = &ty {
                if (*id == self.known.map || *id == self.known.set) && bound == "Eq" {
                    let f = self.methods[&(RecvKey::Adt(*id), "equals".to_string())][0];
                    if *id == self.known.map {
                        self.require_bound(cx, &targs[1], "Eq", span);
                    }
                    let call = TExpr {
                        kind: TK::Call {
                            f,
                            targs: targs.clone(),
                            eargs: Vec::new(),
                            args: vec![TArg { mode: Mode::Read, expr: lt, copy: false }, TArg { mode: Mode::Read, expr: rt, copy: false }],
                        },
                        ty: Ty::Bool,
                        span,
                    };
                    return if op == BinOp::Ne { mk(TK::Unary(UnOp::Not, Box::new(call)), Ty::Bool) } else { call };
                }
            }
            if !matches!(&rt.kind, TK::None) {
                self.require_bound(cx, &lty, bound, span);
            }
            return mk(TK::Binary(op, Box::new(lt), Box::new(rt)), Ty::Bool);
        }
        let rt = self.expect(cx, rt, &lty, &format!("the right side of `{}`", op.text()));
        let ty = cx.infer.shallow(&lty);
        let ok = match op {
            BinOp::Add => ty.is_numeric() || ty == Ty::Str || ty == Ty::Dur || matches!(ty, Ty::Var(_)),
            BinOp::Sub => ty.is_numeric() || ty == Ty::Dur || matches!(ty, Ty::Var(_)),
            BinOp::Mul | BinOp::Div | BinOp::Rem => ty.is_numeric() || matches!(ty, Ty::Var(_)),
            BinOp::AddW | BinOp::SubW | BinOp::MulW | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
                ty.is_int() || matches!(ty, Ty::Var(v) if cx.infer.kind(v) == Some(VarKind::IntLit))
            }
            _ => false,
        };
        if !ok {
            let what = match op {
                BinOp::Add => "numbers or strings",
                BinOp::AddW | BinOp::SubW | BinOp::MulW | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => "integers",
                _ => "numbers",
            };
            let msg = format!("`{}` needs {what}, but this is `{}`", op.text(), self.show(cx, &lty));
            self.err(file, span, msg);
        }
        mk(TK::Binary(op, Box::new(lt), Box::new(rt)), lty)
    }

    fn index_expr(&mut self, cx: &mut FnCx, base: &Expr, args: &[Expr], span: Span) -> TExpr {
        let file = cx.file;
        let b = self.expr(cx, base, None);
        let bty = cx.infer.shallow(&b.ty);
        if args.len() != 1 {
            self.err(file, span, "indexing takes one index, like `xs[i]`");
            return self.error_expr(span);
        }
        let arg = &args[0];
        if let ExprKind::Range { lo, hi, inclusive } = &arg.kind {
            if !matches!(bty, Ty::Array(_) | Ty::Str | Ty::Error) {
                let msg = format!("slices work on arrays and strings, but this is `{}`", self.show(cx, &b.ty));
                self.err(file, span, msg);
                return self.error_expr(span);
            }
            if *inclusive {
                self.err(file, arg.span, "slices use `a..b` (the end isn't included)");
            }
            let mut bound = |c: &mut Checker, e: &Option<Box<Expr>>| {
                e.as_ref().map(|e| {
                    let t = c.expr(cx, e, Some(&Ty::INT));
                    Box::new(c.expect(cx, t, &Ty::INT, "a slice bound"))
                })
            };
            let lo = bound(self, lo);
            let hi = bound(self, hi);
            let ty = b.ty.clone();
            return TExpr { kind: TK::Slice { base: Box::new(b), lo, hi }, ty, span };
        }
        match bty {
            Ty::Array(elem) => {
                let i = self.expr(cx, arg, Some(&Ty::INT));
                let i = self.expect(cx, i, &Ty::INT, "an index");
                TExpr { kind: TK::Index { base: Box::new(b), index: Box::new(i) }, ty: *elem, span }
            }
            Ty::Str => {
                let i = self.expr(cx, arg, Some(&Ty::INT));
                let i = self.expect(cx, i, &Ty::INT, "an index");
                TExpr { kind: TK::Index { base: Box::new(b), index: Box::new(i) }, ty: Ty::U8, span }
            }
            Ty::Adt(id, targs) if id == self.known.map => {
                let k = self.expr(cx, arg, Some(&targs[0]));
                let k = self.expect(cx, k, &targs[0], "the key");
                self.map_index(cx, b, k, targs, span)
            }
            Ty::Error => self.error_expr(span),
            other => {
                let msg = format!("`[...]` works on arrays, strings and maps, but this is `{}`", self.show(cx, &other));
                self.err(file, span, msg);
                self.error_expr(span)
            }
        }
    }
}

pub fn place_root(p: &TPlace) -> LocalId {
    match p {
        TPlace::Local(id) => *id,
        TPlace::Field(b, _) | TPlace::Index(b, _) => place_root(b),
    }
}

fn op_assign_text(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+=",
        BinOp::Sub => "-=",
        BinOp::Mul => "*=",
        BinOp::Div => "/=",
        _ => "%=",
    }
}

pub fn parse_int(s: &str) -> Option<i128> {
    let clean: String = s.chars().filter(|&c| c != '_').collect();
    let v = if let Some(h) = clean.strip_prefix("0x") {
        i128::from_str_radix(h, 16).ok()?
    } else if let Some(b) = clean.strip_prefix("0b") {
        i128::from_str_radix(b, 2).ok()?
    } else {
        clean.parse::<i128>().ok()?
    };
    (v <= u64::MAX as i128).then_some(v)
}

fn parse_dur(s: &str) -> Option<i128> {
    let split = s.find(|c: char| c.is_ascii_alphabetic())?;
    let (num, unit) = s.split_at(split);
    let n: i128 = num.replace('_', "").parse().ok()?;
    let mul: i128 = match unit {
        "ns" => 1,
        "us" => 1_000,
        "ms" => 1_000_000,
        "s" => 1_000_000_000,
        "m" => 60_000_000_000,
        "h" => 3_600_000_000_000,
        _ => return None,
    };
    let v = n.checked_mul(mul)?;
    (v <= i64::MAX as i128).then_some(v)
}

pub fn decode_byte(s: &str) -> Option<u8> {
    let b = s.as_bytes();
    match b {
        [c] if c.is_ascii() => Some(*c),
        [b'\\', e] => Some(match e {
            b'n' => b'\n',
            b't' => b'\t',
            b'r' => b'\r',
            b'0' => 0,
            b'\\' => b'\\',
            b'\'' => b'\'',
            b'"' => b'"',
            _ => return None,
        }),
        [b'\\', b'x', h @ ..] if h.len() == 2 => u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok().filter(|v| *v < 0x80),
        _ => None,
    }
}

fn defaults_for_display(infer: &Infer, t: &Ty) -> Ty {
    match t {
        Ty::Var(v) => match infer.kind(*v) {
            Some(VarKind::IntLit) => Ty::INT,
            Some(VarKind::FloatLit) => Ty::F64,
            _ => t.clone(),
        },
        Ty::Array(e) => Ty::array(defaults_for_display(infer, e)),
        Ty::Opt(e) => Ty::opt(defaults_for_display(infer, e)),
        Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| defaults_for_display(infer, t)).collect()),
        Ty::Adt(id, ts) => Ty::Adt(*id, ts.iter().map(|t| defaults_for_display(infer, t)).collect()),
        other => other.clone(),
    }
}
