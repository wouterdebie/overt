//! After a function is checked: replace inference variables with the types
//! they stand for, default number literals, check literal ranges, and check
//! that type arguments satisfy their constraints.

use super::body::FnCx;
use super::*;

impl<'a> Checker<'a> {
    /// Finishes a function body and the closures made while checking it.
    pub fn finish(&mut self, cx: &mut FnCx, body: &mut Body) {
        cx.infer.default_literals();
        let had_errors = self.diags.len() > cx.errors_before;
        let mut z = Zonk { cx, errors: Vec::new(), reported_unknown: had_errors };
        z.body(body);
        let closures = z.cx.closures.clone();
        for id in closures {
            let mut b = std::mem::replace(&mut self.closures[id].body, Body { locals: Vec::new(), params: Vec::new(), pre: Vec::new(), block: TBlock { stmts: Vec::new(), value: None } });
            z.body(&mut b);
            self.closures[id].body = b;
            self.closures[id].ret = z.ty(&self.closures[id].ret.clone(), Span::default());
            self.closures[id].eff = z.cx.infer.resolve_eff(&self.closures[id].eff);
            self.closures[id].eff.vars.clear();
        }
        let errors = std::mem::take(&mut z.errors);
        let file = cx.file;
        for (span, msg) in errors {
            self.err(file, span, msg);
        }
        self.check_obligations(cx);
    }

    pub fn finish_expr(&mut self, cx: &mut FnCx, e: &mut TExpr) {
        cx.infer.default_literals();
        let mut z = Zonk { cx, errors: Vec::new(), reported_unknown: false };
        z.expr(e);
        let errors = std::mem::take(&mut z.errors);
        let file = cx.file;
        for (span, msg) in errors {
            self.err(file, span, msg);
        }
        self.check_obligations(cx);
    }

    fn check_obligations(&mut self, cx: &mut FnCx) {
        let obs = std::mem::take(&mut cx.obligations);
        let names = type_param_names(&cx.generics);
        for ob in obs {
            let ty = cx.infer.resolve(&ob.ty);
            if let Some(why) = self.lacks(&ty, ob.bound, &cx.generics) {
                let shown = self.ty_name_with(&ty, &names);
                let what = match ob.bound {
                    "Eq" => "compared with `==`",
                    "Ord" => "ordered with `<` or sorted",
                    _ => "hashed, so it can't be a map key or set element",
                };
                let msg = format!("`{shown}` can't be {what}{why}");
                self.err(cx.file, ob.span, msg);
            }
        }
    }

    /// Why `ty` doesn't satisfy `bound`, or `None` if it does.
    fn lacks(&self, ty: &Ty, bound: &str, generics: &[GenericDef]) -> Option<String> {
        let mut seen = HashSet::new();
        self.lacks_in(ty, bound, generics, &mut seen)
    }

    fn lacks_in(&self, ty: &Ty, bound: &str, generics: &[GenericDef], seen: &mut HashSet<Ty>) -> Option<String> {
        if !seen.insert(ty.clone()) {
            return None;
        }
        let all = |ts: &[Ty], seen: &mut HashSet<Ty>| ts.iter().find_map(|t| self.lacks_in(t, bound, generics, seen));
        match ty {
            Ty::Error | Ty::Never | Ty::Var(_) => None,
            Ty::Int(_) | Ty::Float(_) | Ty::Bool | Ty::Str | Ty::Dur | Ty::Unit => None,
            Ty::Param(i) => {
                let g = generics.iter().find(|g| matches!(g.kind, GenericKind::Type(_)) && g.index == *i)?;
                let GenericKind::Type(b) = &g.kind else { return None };
                let has = match bound {
                    "Eq" => b.eq,
                    "Ord" => b.ord,
                    _ => b.hash,
                };
                (!has).then(|| format!(" (add the constraint: `{}: {bound}`)", g.name))
            }
            Ty::Fn(_) => Some(" (it's a function)".into()),
            Ty::Array(e) | Ty::Opt(e) => self.lacks_in(e, bound, generics, seen),
            Ty::Tuple(ts) => all(ts, seen),
            Ty::Adt(id, targs) => {
                let adt = &self.adts[*id];
                if bound == "Ord" {
                    return Some(format!(" (only numbers, strings, tuples and arrays are ordered; sort `{}` values with `sort_by`)", adt.name));
                }
                if (*id == self.known.map || *id == self.known.set) && bound == "Hash" {
                    return Some(String::new());
                }
                let tys: Vec<Ty> = match &adt.kind {
                    AdtKind::Struct(fs) => fs.iter().map(|f| f.ty.subst(targs, &[])).collect(),
                    AdtKind::Enum(vs) => vs.iter().flat_map(|v| v.fields.iter()).map(|f| f.ty.subst(targs, &[])).collect(),
                };
                all(&tys, seen)
            }
        }
    }
}

struct Zonk<'x> {
    cx: &'x mut FnCx,
    errors: Vec<(Span, String)>,
    reported_unknown: bool,
}

impl Zonk<'_> {
    fn ty(&mut self, t: &Ty, span: Span) -> Ty {
        let r = self.cx.infer.resolve(t);
        if r.has_vars() {
            if !self.reported_unknown {
                self.reported_unknown = true;
                self.errors.push((span, "can't tell the type of this value; add a type annotation, like `let x: [int] = []`".into()));
            }
            return subst_vars(&r);
        }
        r
    }

    fn eff(&mut self, e: &Eff) -> Eff {
        let mut r = self.cx.infer.resolve_eff(e);
        r.vars.clear();
        r
    }

    fn body(&mut self, b: &mut Body) {
        for l in &mut b.locals {
            l.ty = self.cx.infer.resolve(&l.ty);
            if l.ty.has_vars() {
                // Reported where the value is used; don't fail on unused ones.
                l.ty = subst_vars(&l.ty);
            }
        }
        for (e, _) in &mut b.pre {
            self.expr(e);
        }
        self.block(&mut b.block);
    }

    fn block(&mut self, b: &mut TBlock) {
        for s in &mut b.stmts {
            self.stmt(s);
        }
        if let Some(v) = &mut b.value {
            self.expr(v);
        }
    }

    fn stmt(&mut self, s: &mut TStmt) {
        match s {
            TStmt::Let { value, .. } | TStmt::LetTuple { value, .. } | TStmt::Expr(value) => self.expr(value),
            TStmt::Assign { place, value, .. } => {
                self.place(place);
                self.expr(value);
            }
            TStmt::While { cond, body } => {
                self.expr(cond);
                self.block(body);
            }
            TStmt::WhileLet { value, body, .. } => {
                self.expr(value);
                self.block(body);
            }
            TStmt::ForRange { lo, hi, body, .. } => {
                self.expr(lo);
                self.expr(hi);
                self.block(body);
            }
            TStmt::ForArray { array, place, body, .. } => {
                self.expr(array);
                if let Some(p) = place {
                    self.place(p);
                }
                self.block(body);
            }
            TStmt::ForMap { map, body, .. } => {
                self.expr(map);
                self.block(body);
            }
            TStmt::Par { branches } => {
                for b in branches {
                    self.expr(&mut b.closure);
                }
            }
        }
    }

    fn place(&mut self, p: &mut TPlace) {
        match p {
            TPlace::Local(_) => {}
            TPlace::Field(b, _) => self.place(b),
            TPlace::Index(b, i) => {
                self.place(b);
                self.expr(i);
            }
        }
    }

    fn expr(&mut self, e: &mut TExpr) {
        e.ty = self.ty(&e.ty.clone(), e.span);
        match &mut e.kind {
            TK::Int(v) => match &e.ty {
                Ty::Int(k) => {
                    if *v < k.min_value() || *v > k.max_value() {
                        self.errors.push((e.span, format!("`{v}` doesn't fit in `{}`", k.name())));
                    }
                }
                Ty::Float(_) => e.kind = TK::Float(*v as f64),
                _ => {}
            },
            TK::Float(_) | TK::Bool(_) | TK::Str(_) | TK::Unit | TK::Local(_) | TK::None | TK::Break | TK::Continue | TK::Todo | TK::Closure { .. } => {}
            TK::Call { targs, eargs, args, .. } => {
                for t in targs.iter_mut() {
                    *t = self.ty(&t.clone(), e.span);
                }
                for x in eargs.iter_mut() {
                    *x = self.eff(&x.clone());
                }
                for a in args {
                    self.expr(&mut a.expr);
                }
            }
            TK::CallValue { callee, args } => {
                self.expr(callee);
                for a in args {
                    self.expr(&mut a.expr);
                }
            }
            TK::FnValue { targs, eargs, .. } => {
                for t in targs.iter_mut() {
                    *t = self.ty(&t.clone(), e.span);
                }
                for x in eargs.iter_mut() {
                    *x = self.eff(&x.clone());
                }
            }
            TK::Struct { targs, fields, .. } | TK::Variant { targs, fields, .. } => {
                for t in targs.iter_mut() {
                    *t = self.ty(&t.clone(), e.span);
                }
                for f in fields {
                    self.expr(f);
                }
            }
            TK::Tuple(items) | TK::Array(items) | TK::Interp(items) => {
                for i in items {
                    self.expr(i);
                }
            }
            TK::Field { base, .. } => self.expr(base),
            TK::Index { base, index } => {
                self.expr(base);
                self.expr(index);
            }
            TK::Slice { base, lo, hi } => {
                self.expr(base);
                if let Some(l) = lo {
                    self.expr(l);
                }
                if let Some(h) = hi {
                    self.expr(h);
                }
            }
            TK::Some(x) | TK::Unary(_, x) | TK::Convert(x) | TK::Try(x) | TK::Trap(x) | TK::Hash(x) => self.expr(x),
            TK::Print { value, .. } | TK::Dbg { value, .. } => self.expr(value),
            TK::Binary(_, l, r) => {
                self.expr(l);
                self.expr(r);
            }
            TK::If { cond, then, els } => {
                self.expr(cond);
                self.block(then);
                if let Some(b) = els {
                    self.block(b);
                }
            }
            TK::IfLet { value, then, els, .. } => {
                self.expr(value);
                self.block(then);
                if let Some(b) = els {
                    self.block(b);
                }
            }
            TK::Match { scrut, arms } => {
                self.expr(scrut);
                for a in arms {
                    if let Some(g) = &mut a.guard {
                        self.expr(g);
                    }
                    self.expr(&mut a.body);
                }
            }
            TK::Block(b) => self.block(b),
            TK::Return(v) => {
                if let Some(v) = v {
                    self.expr(v);
                }
            }
            TK::ElseOpt { value, alt } | TK::ElseFail { value, alt } => {
                self.expr(value);
                self.expr(alt);
            }
            TK::Catch { value, body, .. } => {
                self.expr(value);
                self.block(body);
            }
            TK::Fail { kind, msg } => {
                self.expr(kind);
                self.expr(msg);
            }
            TK::Assert { cond, msg, .. } => {
                self.expr(cond);
                if let Some(m) = msg {
                    self.expr(m);
                }
            }
            TK::ExpectEq { left, right, .. } => {
                self.expr(left);
                self.expr(right);
            }
            TK::ExpectFail { value, kind, .. } => {
                self.expr(value);
                if let Some(k) = kind {
                    self.expr(k);
                }
            }
            TK::ExpectTrue { cond, .. } => self.expr(cond),
        }
    }
}

/// Leftover variables become `Error`, after the "can't tell" message.
fn subst_vars(t: &Ty) -> Ty {
    match t {
        Ty::Var(_) => Ty::Error,
        Ty::Array(e) => Ty::Array(Box::new(subst_vars(e))),
        Ty::Opt(e) => Ty::Opt(Box::new(subst_vars(e))),
        Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(subst_vars).collect()),
        Ty::Adt(id, ts) => Ty::Adt(*id, ts.iter().map(subst_vars).collect()),
        Ty::Fn(f) => Ty::Fn(Box::new(FnTy { params: f.params.iter().map(subst_vars).collect(), ret: subst_vars(&f.ret), eff: f.eff.clone() })),
        other => other.clone(),
    }
}

/// Calls `f` on every expression inside `e`, including `e`.
pub fn walk_expr(e: &TExpr, f: &mut dyn FnMut(&TExpr)) {
    f(e);
    let block = |b: &TBlock, f: &mut dyn FnMut(&TExpr)| walk_block(b, f);
    match &e.kind {
        TK::Call { args, .. } | TK::CallValue { args, .. } => {
            if let TK::CallValue { callee, .. } = &e.kind {
                walk_expr(callee, f);
            }
            for a in args {
                walk_expr(&a.expr, f);
            }
        }
        TK::Struct { fields, .. } | TK::Variant { fields, .. } => fields.iter().for_each(|x| walk_expr(x, f)),
        TK::Tuple(items) | TK::Array(items) | TK::Interp(items) => items.iter().for_each(|x| walk_expr(x, f)),
        TK::Field { base, .. } => walk_expr(base, f),
        TK::Index { base, index } => {
            walk_expr(base, f);
            walk_expr(index, f);
        }
        TK::Slice { base, lo, hi } => {
            walk_expr(base, f);
            if let Some(l) = lo {
                walk_expr(l, f);
            }
            if let Some(h) = hi {
                walk_expr(h, f);
            }
        }
        TK::Some(x) | TK::Unary(_, x) | TK::Convert(x) | TK::Try(x) | TK::Trap(x) | TK::Hash(x) => walk_expr(x, f),
        TK::Print { value, .. } | TK::Dbg { value, .. } => walk_expr(value, f),
        TK::Binary(_, l, r) | TK::ElseOpt { value: l, alt: r } | TK::ElseFail { value: l, alt: r } => {
            walk_expr(l, f);
            walk_expr(r, f);
        }
        TK::If { cond, then, els } | TK::IfLet { value: cond, then, els, .. } => {
            walk_expr(cond, f);
            block(then, f);
            if let Some(b) = els {
                block(b, f);
            }
        }
        TK::Match { scrut, arms } => {
            walk_expr(scrut, f);
            for a in arms {
                if let Some(g) = &a.guard {
                    walk_expr(g, f);
                }
                walk_expr(&a.body, f);
            }
        }
        TK::Block(b) => block(b, f),
        TK::Return(Some(v)) => walk_expr(v, f),
        TK::Catch { value, body, .. } => {
            walk_expr(value, f);
            block(body, f);
        }
        TK::Fail { kind, msg } => {
            walk_expr(kind, f);
            walk_expr(msg, f);
        }
        TK::Assert { cond, msg, .. } => {
            walk_expr(cond, f);
            if let Some(m) = msg {
                walk_expr(m, f);
            }
        }
        TK::ExpectEq { left, right, .. } => {
            walk_expr(left, f);
            walk_expr(right, f);
        }
        TK::ExpectFail { value, kind, .. } => {
            walk_expr(value, f);
            if let Some(k) = kind {
                walk_expr(k, f);
            }
        }
        TK::ExpectTrue { cond, .. } => walk_expr(cond, f),
        _ => {}
    }
}

pub fn walk_block(b: &TBlock, f: &mut dyn FnMut(&TExpr)) {
    for s in &b.stmts {
        match s {
            TStmt::Let { value, .. } | TStmt::LetTuple { value, .. } | TStmt::Expr(value) => walk_expr(value, f),
            TStmt::Assign { value, .. } => walk_expr(value, f),
            TStmt::While { cond, body } => {
                walk_expr(cond, f);
                walk_block(body, f);
            }
            TStmt::WhileLet { value, body, .. } => {
                walk_expr(value, f);
                walk_block(body, f);
            }
            TStmt::ForRange { lo, hi, body, .. } => {
                walk_expr(lo, f);
                walk_expr(hi, f);
                walk_block(body, f);
            }
            TStmt::ForArray { array, body, .. } => {
                walk_expr(array, f);
                walk_block(body, f);
            }
            TStmt::ForMap { map, body, .. } => {
                walk_expr(map, f);
                walk_block(body, f);
            }
            TStmt::Par { branches } => {
                for b in branches {
                    walk_expr(&b.closure, f);
                }
            }
        }
    }
    if let Some(v) = &b.value {
        walk_expr(v, f);
    }
}
