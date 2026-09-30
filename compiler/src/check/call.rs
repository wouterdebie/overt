//! Calls: functions, methods, closures, construction of structs and
//! variants, builtins like `print` and `fail`, and numeric conversions.

use super::body::{is_builtin_intrinsic, FnCx, Named};
use super::infer::VarKind;
use super::*;
use crate::ast::{Arg, Expr, ExprKind, Ident};

/// A parameter as argument matching sees it.
struct PInfo {
    name: String,
    mode: Mode,
    ty: Ty,
    /// The declared type before instantiation, for the same-type naming rule.
    decl_ty: Ty,
    default: Option<TExpr>,
}

/// Method names from other languages, and what to write in Overt.
fn method_hint(name: &str) -> Option<&'static str> {
    Some(match name {
        "clone" | "copy" | "to_owned" | "dup" => "not needed: assigning or passing a value copies it",
        "unwrap" | "expect" | "unwrap_or" | "unwrap_or_default" => "use `x else default`, `?`, or `if let v = x`",
        "to_string" | "toString" | "str" | "string" => "use `\"${x}\"`",
        "iter" | "into_iter" | "iter_mut" | "values_mut" => "loop over it directly: `for x in xs`",
        "collect" => "not needed: `map` and `filter` return arrays",
        "push_str" | "append" | "concat" => "use `+=`",
        "length" | "size" | "count" => "use `len()`",
        "chars" => "use `bytes()` or `runes()`",
        "is_some" | "is_none" => "compare with `none`: `x != none`",
        "add" | "append_item" => "use `insert` for sets and `push` for arrays",
        "lowercase" | "lower" | "to_lowercase" | "toLowerCase" => "use `to_lower()`",
        "uppercase" | "upper" | "to_uppercase" | "toUpperCase" => "use `to_upper()`",
        "split_whitespace" => "use `split(\" \")` or loop over `bytes()`",
        "len_utf8" => "use `len()` for bytes or `runes().len()` for code points",
        "sort_unstable" => "use `sort()`",
        "sort_by_key" => "use `sort_by(|x| key)`",
        "sorted" => "copy the array, then `sort()` it",
        "keys_sorted" => "`keys()` returns them in insertion order; sort the result",
        "get_or" | "get_or_default" => "use `m.get(k) else default`",
        "insert" => "for maps, use `m[k] = v` or `m.set(k, v)`",
        "has" | "contains_key" | "has_key" => "use `contains`",
        "delete" => "use `remove`",
        "items" | "entries" => "for maps, use `pairs()`, or `for k, v in m`",
        "join_with" => "use `join(sep)`",
        _ => return None,
    })
}

impl<'a> Checker<'a> {
    /// Fresh type and effect arguments for a generic function, with its constraints recorded.
    pub fn instantiate(&mut self, cx: &mut FnCx, f: FnId, span: Span) -> (Vec<Ty>, Vec<Eff>) {
        let gens = self.fns[f].generics.clone();
        let mut targs = vec![Ty::Error; gens.iter().filter(|g| matches!(g.kind, GenericKind::Type(_))).count()];
        let mut eargs = vec![Eff::pure(); gens.iter().filter(|g| matches!(g.kind, GenericKind::Effect)).count()];
        for g in &gens {
            match &g.kind {
                GenericKind::Type(b) => {
                    let v = cx.infer.fresh(VarKind::Any);
                    if b.eq {
                        self.require_bound(cx, &v, "Eq", span);
                    }
                    if b.ord {
                        self.require_bound(cx, &v, "Ord", span);
                    }
                    if b.hash {
                        self.require_bound(cx, &v, "Hash", span);
                    }
                    if b.json {
                        self.require_bound(cx, &v, "Json", span);
                    }
                    targs[g.index as usize] = v;
                }
                GenericKind::Effect => eargs[g.index as usize] = cx.infer.fresh_eff(),
            }
        }
        (targs, eargs)
    }

    fn adt_targs(&mut self, cx: &mut FnCx, adt: AdtId, span: Span) -> Vec<Ty> {
        let gens = self.adts[adt].generics.clone();
        let mut out = Vec::new();
        for g in &gens {
            if let GenericKind::Type(b) = &g.kind {
                let v = cx.infer.fresh(VarKind::Any);
                if b.eq {
                    self.require_bound(cx, &v, "Eq", span);
                }
                if b.ord {
                    self.require_bound(cx, &v, "Ord", span);
                }
                if b.hash {
                    self.require_bound(cx, &v, "Hash", span);
                }
                if b.json {
                    self.require_bound(cx, &v, "Json", span);
                }
                out.push(v);
            }
        }
        out
    }

    /// A dotted name like `api.users`, if the expression is one.
    fn dotted(&self, e: &Expr) -> Option<String> {
        match &e.kind {
            ExprKind::Ident(n) => Some(n.clone()),
            ExprKind::Field(b, n) => Some(format!("{}.{}", self.dotted(b)?, n.name)),
            _ => None,
        }
    }

    /// The module an expression names, if it names one (and isn't a local).
    fn module_of(&mut self, cx: &mut FnCx, e: &Expr) -> Option<String> {
        let path = self.dotted(e)?;
        let root = path.split('.').next().unwrap();
        if self.lookup_local(cx, root).is_some() {
            return None;
        }
        if self.modules.contains_key(&path) {
            return Some(path);
        }
        if stdlib::MODULE_NAMES.contains(&path.as_str()) {
            return Some(path);
        }
        None
    }

    /// `base.name` without a call.
    pub fn field_expr(&mut self, cx: &mut FnCx, base: &Expr, name: &Ident, span: Span, want: Option<&Ty>) -> TExpr {
        let file = cx.file;
        if let Some(m) = self.module_of(cx, base) {
            return self.module_member(cx, &m, name, span, want);
        }
        if let Some((adt, targs)) = self.type_expr_of(cx, base) {
            if self.adts[adt].is_enum() {
                return self.variant_value(cx, Some((adt, targs)), name, None, span, want);
            }
            if let Some(f) = self.methods.get(&(RecvKey::Adt(adt), name.name.clone())).and_then(|v| v.first().copied()) {
                return self.fn_value(cx, f, span, want);
            }
            let n = self.adts[adt].name.clone();
            self.err(file, name.span, format!("`{n}` has no function `{}`", name.name));
            return self.error_expr(span);
        }
        if let ExprKind::Ident(tn) = &base.kind {
            if let Named::Builtin(ty) = self.resolve_name(cx, tn) {
                let key = RecvKey::of(&ty).unwrap();
                if let Some(f) = self.methods.get(&(key, name.name.clone())).and_then(|v| v.first().copied()) {
                    return self.fn_value(cx, f, span, want);
                }
                self.err(file, name.span, format!("`{tn}` has no function `{}`", name.name));
                return self.error_expr(span);
            }
        }
        let b = self.expr(cx, base, None);
        let bty = cx.infer.shallow(&b.ty);
        match &bty {
            Ty::Tuple(ts) => {
                if let Ok(i) = name.name.parse::<usize>() {
                    if i < ts.len() {
                        let ty = ts[i].clone();
                        return TExpr { kind: TK::Field { base: Box::new(b), index: i as u32 }, ty, span };
                    }
                    self.err(file, name.span, format!("this tuple has {} elements, so `.{i}` doesn't exist", ts.len()));
                } else {
                    self.err(file, name.span, format!("tuple elements are numbered: `.0`, `.1`; `{}` isn't one", name.name));
                }
                return self.error_expr(span);
            }
            Ty::Adt(id, targs) if !self.adts[*id].is_enum() => {
                let fields = self.adts[*id].fields();
                if let Some(i) = fields.iter().position(|f| f.name == name.name) {
                    if name.name.starts_with('_') && self.adts[*id].module != self.files[file].module {
                        let n = self.adts[*id].name.clone();
                        self.err(file, name.span, format!("`{}` is private to `{n}`", name.name));
                        return self.error_expr(span);
                    }
                    let ty = fields[i].ty.subst(targs, &[]);
                    return TExpr { kind: TK::Field { base: Box::new(b), index: i as u32 }, ty, span };
                }
            }
            Ty::Opt(_) => {
                let msg = format!("this is an optional (`{}`); get the value first with `else`, or `if let v = x`", self.show(cx, &b.ty));
                self.err(file, span, msg);
                return self.error_expr(span);
            }
            Ty::Error => return self.error_expr(span),
            _ => {}
        }
        if let Some(key) = RecvKey::of(&bty) {
            if self.methods.contains_key(&(key, name.name.clone())) {
                self.err(file, name.span, format!("`{}` is a method; call it: `.{}()`", name.name, name.name));
                return self.error_expr(span);
            }
        }
        self.no_member(cx, &b, name);
        self.error_expr(span)
    }

    /// Reports an unknown field or method, with the closest name.
    fn no_member(&mut self, cx: &mut FnCx, b: &TExpr, name: &Ident) {
        let bty = cx.infer.shallow(&b.ty);
        let shown = self.show(cx, &bty);
        if let Ty::Var(_) = bty {
            self.err(cx.file, b.span, format!("the type of this value isn't known yet, so `.{}` can't be looked up; add a type annotation", name.name));
            return;
        }
        if let Ty::Param(_) = bty {
            self.err(cx.file, name.span, format!("`{shown}` is a type parameter, so it has no fields or methods; only `==`, `<` and `hash` work on it, with the matching constraint"));
            return;
        }
        let mut candidates: Vec<String> = Vec::new();
        if let Ty::Adt(id, _) = &bty {
            candidates.extend(self.adts[*id].fields().iter().map(|f| f.name.clone()).filter(|n| !n.starts_with('_')));
        }
        if let Some(key) = RecvKey::of(&bty) {
            candidates.extend(self.methods.keys().filter(|(k, n)| *k == key && !n.starts_with('_')).map(|(_, n)| n.clone()));
        }
        let hint = if let Some(h) = method_hint(&name.name).filter(|_| !candidates.contains(&name.name)) {
            format!("; {h}")
        } else if let Some(c) = closest(&name.name, candidates.iter().map(|s| s.as_str())) {
            let c = c.to_string();
            let sig = RecvKey::of(&bty)
                .and_then(|k| self.methods.get(&(k, c.clone())))
                .and_then(|v| v.first().copied())
                .map(|f| self.fn_sig_text(f))
                .unwrap_or(c);
            format!("; did you mean `{sig}`?")
        } else {
            String::new()
        };
        let what = if matches!(bty, Ty::Adt(..)) { "field or method" } else { "method" };
        self.err(cx.file, name.span, format!("`{shown}` has no {what} `{}`{hint}", name.name));
    }

    fn module_member(&mut self, cx: &mut FnCx, m: &str, name: &Ident, span: Span, want: Option<&Ty>) -> TExpr {
        let file = cx.file;
        let Some(scope) = self.modules.get(m) else {
            self.err(file, span, format!("the `{m}` module isn't available in this compiler yet (planned for milestone {})", stdlib::planned_milestone(&m)));
            return self.error_expr(span);
        };
        if name.name.starts_with('_') && m != self.files[file].module {
            self.err(file, name.span, format!("`{}` is private to the `{m}` module", name.name));
            return self.error_expr(span);
        }
        if let Some(&c) = scope.consts.get(&name.name) {
            return match self.const_value(c) {
                Some(mut v) => {
                    v.span = span;
                    v
                }
                None => self.error_expr(span),
            };
        }
        if let Some(&f) = scope.fns.get(&name.name) {
            return self.fn_value(cx, f, span, want);
        }
        if scope.types.contains_key(&name.name) {
            self.err(file, span, format!("`{m}.{}` is a type, not a value", name.name));
            return self.error_expr(span);
        }
        let names: Vec<String> = scope.fns.keys().chain(scope.consts.keys()).filter(|n| !n.starts_with('_')).cloned().collect();
        let hint = closest(&name.name, names.iter().map(|s| s.as_str())).map(|s| format!("; did you mean `{m}.{s}`?")).unwrap_or_default();
        self.err(file, name.span, format!("the `{m}` module has no `{}`{hint}", name.name));
        self.error_expr(span)
    }

    /// If the expression names a struct or enum type (maybe with type
    /// arguments, like `Page[int]`), the type and its explicit arguments.
    fn type_expr_of(&mut self, cx: &mut FnCx, e: &Expr) -> Option<(AdtId, Option<Vec<Ty>>)> {
        let (path_expr, targs) = match &e.kind {
            ExprKind::Index { base, args } => (base.as_ref(), Some(args)),
            _ => (e, None),
        };
        let adt = match &path_expr.kind {
            ExprKind::Ident(n) => {
                if !n.starts_with(|c: char| c.is_ascii_uppercase()) {
                    return None;
                }
                match self.resolve_name(cx, n) {
                    Named::Type(TypeRef::Adt(id)) => id,
                    Named::Type(TypeRef::Alias(Ty::Adt(id, _))) => id,
                    _ => return None,
                }
            }
            ExprKind::Field(b, n) => {
                let m = self.module_of(cx, b)?;
                match self.modules.get(&m).and_then(|s| s.types.get(&n.name)) {
                    Some(TypeRef::Adt(id)) => *id,
                    _ => return None,
                }
            }
            _ => return None,
        };
        let targs = targs.map(|args| {
            args.iter()
                .map(|a| {
                    let texpr = expr_as_type(a);
                    match texpr {
                        Some(t) => {
                            let g = cx.generics.clone();
                            self.resolve_type(&t, cx.file, &g)
                        }
                        None => {
                            self.err(cx.file, a.span, "expected a type here");
                            Ty::Error
                        }
                    }
                })
                .collect()
        });
        Some((adt, targs))
    }

    /// The function `e` names, if it's a function name or `module.name`.
    fn fn_named(&mut self, cx: &mut FnCx, e: &Expr) -> Option<FnId> {
        match &e.kind {
            ExprKind::Ident(n) => match self.resolve_name(cx, n) {
                Named::Fn(f) => Some(f),
                _ => None,
            },
            ExprKind::Field(b, n) => {
                let m = self.module_of(cx, b)?;
                if n.name.starts_with('_') && m != self.files[cx.file].module {
                    return None;
                }
                self.modules.get(&m).and_then(|s| s.fns.get(&n.name)).copied()
            }
            _ => None,
        }
    }

    /// Types written in brackets, like `[User]` in `json.decode[User](s)`.
    fn explicit_types(&mut self, cx: &mut FnCx, args: &[Expr]) -> Vec<Ty> {
        args.iter()
            .map(|a| match expr_as_type(a) {
                Some(t) => {
                    let g = cx.generics.clone();
                    self.resolve_type(&t, cx.file, &g)
                }
                None => {
                    self.err(cx.file, a.span, "expected a type here");
                    Ty::Error
                }
            })
            .collect()
    }

    pub fn call_expr(&mut self, cx: &mut FnCx, callee: &Expr, args: &[Arg], span: Span, want: Option<&Ty>) -> TExpr {
        let file = cx.file;
        match &callee.kind {
            ExprKind::Ident(name) => match self.resolve_name(cx, name) {
                Named::Local(_) => {
                    let c = self.expr(cx, callee, None);
                    self.call_value(cx, c, args, span)
                }
                Named::Fn(f) => {
                    if is_builtin_intrinsic(name) && self.files[self.fns[f].file].std && self.files[self.fns[f].file].open {
                        return self.builtin_call(cx, name, args, span, want);
                    }
                    self.call_fn(cx, f, None, args, span, want, None)
                }
                Named::Type(TypeRef::Adt(id)) => self.construct(cx, id, None, args, span, want),
                Named::Type(TypeRef::Alias(Ty::Adt(id, targs))) => self.construct(cx, id, Some(targs), args, span, want),
                Named::Builtin(ty) if ty.is_numeric() => self.convert(cx, ty, args, span),
                Named::Builtin(ty) => {
                    let msg = match ty {
                        Ty::Str => "build a string with `\"${x}\"`".to_string(),
                        _ => format!("`{name}` isn't a conversion; compare instead, like `x != 0`"),
                    };
                    self.err(file, callee.span, msg);
                    self.error_expr(span)
                }
                Named::Const(_) | Named::Type(_) => {
                    self.err(file, callee.span, format!("`{name}` isn't a function"));
                    self.error_expr(span)
                }
                Named::Module(m) => {
                    self.err(file, callee.span, format!("`{m}` is a module; call one of its functions, like `{m}.name(...)`"));
                    self.error_expr(span)
                }
                Named::Unknown => {
                    self.unknown_name(cx, name, callee.span);
                    for a in args {
                        self.expr(cx, &a.value, None);
                    }
                    self.error_expr(span)
                }
            },
            ExprKind::Variant(name) => self.variant_value(cx, None, name, Some(args), span, want),
            ExprKind::Field(base, name) => self.path_call(cx, base, name, args, span, want),
            ExprKind::Index { base, args: targs } => {
                if let Some((adt, targs)) = self.type_expr_of(cx, callee) {
                    return self.construct(cx, adt, targs, args, span, want);
                }
                // A generic function with explicit type arguments: `json.decode[User](s)`.
                if let Some(f) = self.fn_named(cx, base) {
                    let n = self.fns[f].generics.iter().filter(|g| matches!(g.kind, GenericKind::Type(_))).count();
                    if targs.len() != n {
                        let def = &self.fns[f];
                        let other = !self.files[def.file].open && def.module != self.files[file].module;
                        let fname = if other { format!("{}.{}", def.module, def.name) } else { def.name.clone() };
                        let what = if n == 1 { "1 type argument".to_string() } else { format!("{n} type arguments") };
                        self.err(file, callee.span, format!("`{fname}` takes {what}, not {}", targs.len()));
                    }
                    let tys = self.explicit_types(cx, targs);
                    return self.call_fn(cx, f, None, args, span, want, Some(tys));
                }
                let c = self.expr(cx, callee, None);
                self.call_value(cx, c, args, span)
            }
            _ => {
                let c = self.expr(cx, callee, None);
                self.call_value(cx, c, args, span)
            }
        }
    }

    /// `base.name(args)`: a module function, a type's function or variant, or a method.
    fn path_call(&mut self, cx: &mut FnCx, base: &Expr, name: &Ident, args: &[Arg], span: Span, want: Option<&Ty>) -> TExpr {
        let file = cx.file;
        if let Some(m) = self.module_of(cx, base) {
            let Some(scope) = self.modules.get(&m) else {
                self.err(file, span, format!("the `{m}` module isn't available in this compiler yet (planned for milestone {})", stdlib::planned_milestone(&m)));
                return self.error_expr(span);
            };
            if name.name.starts_with('_') && m != self.files[file].module {
                self.err(file, name.span, format!("`{}` is private to the `{m}` module", name.name));
                return self.error_expr(span);
            }
            if let Some(&f) = scope.fns.get(&name.name) {
                return self.call_fn(cx, f, None, args, span, want, None);
            }
            if let Some(TypeRef::Adt(id)) = scope.types.get(&name.name).cloned() {
                return self.construct(cx, id, None, args, span, want);
            }
            if let Some((_, _, ms)) = LATER_FNS.iter().find(|(mm, f, _)| *mm == m && *f == name.name) {
                // The arguments aren't checked: they'd only add follow-on errors.
                self.err(file, name.span, format!("`{m}.{}` isn't supported by this compiler yet (planned for milestone {ms})", name.name));
                return self.error_expr(span);
            }
            let names: Vec<String> = scope.fns.keys().filter(|n| !n.starts_with('_')).cloned().collect();
            let hint = closest(&name.name, names.iter().map(|s| s.as_str()))
                .map(|s| {
                    let f = self.modules[&m].fns[s];
                    format!("; did you mean `{m}.{}`?", self.fn_sig_text(f))
                })
                .unwrap_or_default();
            self.err(file, name.span, format!("the `{m}` module has no function `{}`{hint}", name.name));
            for a in args {
                self.expr(cx, &a.value, None);
            }
            return self.error_expr(span);
        }
        if let Some((adt, targs)) = self.type_expr_of(cx, base) {
            if self.adts[adt].is_enum() && self.adts[adt].variants().iter().any(|v| v.name == name.name) {
                return self.variant_value(cx, Some((adt, targs)), name, Some(args), span, want);
            }
            return self.type_fn_call(cx, RecvKey::Adt(adt), targs, name, args, span, want);
        }
        if let ExprKind::Ident(tn) = &base.kind {
            if let Named::Builtin(ty) = self.resolve_name(cx, tn) {
                return self.type_fn_call(cx, RecvKey::of(&ty).unwrap(), None, name, args, span, want);
            }
        }
        // A method call on a value.
        let recv = self.expr(cx, base, None);
        self.method_call(cx, recv, name, args, span, want)
    }

    /// `Type.name(args)`: a static function, or a method with the receiver as the first argument.
    fn type_fn_call(&mut self, cx: &mut FnCx, key: RecvKey, targs: Option<Vec<Ty>>, name: &Ident, args: &[Arg], span: Span, want: Option<&Ty>) -> TExpr {
        let file = cx.file;
        let Some(cands) = self.methods.get(&(key, name.name.clone())).cloned() else {
            let tname = self.key_name(key);
            let names: Vec<String> = self.methods.keys().filter(|(k, n)| *k == key && !n.starts_with('_')).map(|(_, n)| n.clone()).collect();
            let hint = closest(&name.name, names.iter().map(|s| s.as_str())).map(|s| format!("; did you mean `{tname}.{s}`?")).unwrap_or_default();
            self.err(file, name.span, format!("`{tname}` has no function `{}`{hint}", name.name));
            for a in args {
                self.expr(cx, &a.value, None);
            }
            return self.error_expr(span);
        };
        let f = cands[0];
        if name.name.starts_with('_') && self.fns[f].module != self.files[file].module {
            self.err(file, name.span, format!("`{}` is private", name.name));
            return self.error_expr(span);
        }
        if self.fns[f].has_self {
            // `str.find(line, pat: "=")`: the first argument is the receiver.
            let Some(first) = args.first().filter(|a| a.name.is_none()) else {
                let sig = self.fn_sig_text(f);
                self.err(file, span, format!("`{}` is a method; pass the receiver first: `{sig}`", self.fns[f].name));
                return self.error_expr(span);
            };
            let recv = self.expr(cx, &first.value, None);
            return self.method_call_with(cx, recv, &cands, name, &args[1..], span, want, true);
        }
        self.call_fn(cx, f, None, args, span, want, targs)
    }

    fn key_name(&self, key: RecvKey) -> String {
        match key {
            RecvKey::Adt(id) => self.adts[id].name.clone(),
            RecvKey::Str => "str".into(),
            RecvKey::Array => "array".into(),
            RecvKey::Int(k) => k.name().into(),
            RecvKey::Float(k) => k.name().into(),
            RecvKey::Bool => "bool".into(),
        }
    }

    fn method_call(&mut self, cx: &mut FnCx, recv: TExpr, name: &Ident, args: &[Arg], span: Span, want: Option<&Ty>) -> TExpr {
        let file = cx.file;
        // A literal receiver like `3.pow(2)` takes its default type.
        if let Ty::Var(v) = cx.infer.shallow(&recv.ty) {
            match cx.infer.kind(v) {
                Some(VarKind::IntLit) => {
                    cx.infer.unify(&recv.ty, &Ty::INT);
                }
                Some(VarKind::FloatLit) => {
                    cx.infer.unify(&recv.ty, &Ty::F64);
                }
                _ => {}
            }
        }
        let rty = cx.infer.shallow(&recv.ty);
        match &rty {
            Ty::Error => {
                for a in args {
                    self.expr(cx, &a.value, None);
                }
                return self.error_expr(span);
            }
            Ty::Opt(_) => {
                let msg = format!("this is an optional (`{}`); get the value first with `else`, or `if let v = x`", self.show(cx, &recv.ty));
                self.err(file, span, msg);
                return self.error_expr(span);
            }
            Ty::Fn(_) => {
                self.err(file, name.span, "functions have no methods; call it with `f(...)`");
                return self.error_expr(span);
            }
            _ => {}
        }
        let cands = RecvKey::of(&rty).and_then(|k| self.methods.get(&(k, name.name.clone())).cloned());
        match cands {
            Some(c) => self.method_call_with(cx, recv, &c, name, args, span, want, false),
            None => {
                // A struct field holding a function.
                if let Ty::Adt(id, targs) = &rty {
                    if let Some(i) = self.adts[*id].fields().iter().position(|f| f.name == name.name) {
                        let fty = self.adts[*id].fields()[i].ty.subst(targs, &[]);
                        if matches!(fty, Ty::Fn(_)) {
                            let field = TExpr { kind: TK::Field { base: Box::new(recv), index: i as u32 }, ty: fty, span };
                            return self.call_value(cx, field, args, span);
                        }
                    }
                }
                self.no_member(cx, &recv, name);
                for a in args {
                    self.expr(cx, &a.value, None);
                }
                self.error_expr(span)
            }
        }
    }

    fn method_call_with(&mut self, cx: &mut FnCx, recv: TExpr, cands: &[FnId], name: &Ident, args: &[Arg], span: Span, want: Option<&Ty>, via_type: bool) -> TExpr {
        let file = cx.file;
        let rty = cx.infer.resolve(&recv.ty);
        // Pick the declaration whose receiver fits: `[str].join` only for `[str]`.
        let chosen = cands.iter().copied().find(|f| match self.fn_recv.get(f) {
            Some(pat) => recv_matches(pat, &rty),
            None => true,
        });
        let Some(f) = chosen else {
            let shown = self.show(cx, &rty);
            let pat = self.fn_recv.get(&cands[0]).map(|p| self.ty_name_with(p, &type_param_names(&self.fns[cands[0]].generics))).unwrap_or_default();
            self.err(file, name.span, format!("`{}` works on `{pat}`, but this is `{shown}`", name.name));
            return self.error_expr(span);
        };
        if name.name.starts_with('_') && self.fns[f].module != self.files[file].module {
            self.err(file, name.span, format!("`{}` is private", name.name));
            return self.error_expr(span);
        }
        if !self.fns[f].has_self {
            let owner = self.fns[f].name.clone();
            self.err(file, name.span, format!("`{owner}` isn't a method; call it on the type: `{owner}(...)`"));
            return self.error_expr(span);
        }
        let _ = via_type;
        self.call_fn(cx, f, Some(recv), args, span, want, None)
    }

    /// Calls function `f`, checking arguments against its parameters.
    pub fn call_fn(&mut self, cx: &mut FnCx, f: FnId, recv: Option<TExpr>, args: &[Arg], span: Span, want: Option<&Ty>, explicit: Option<Vec<Ty>>) -> TExpr {
        let file = cx.file;
        let (targs, eargs) = self.instantiate(cx, f, span);
        if let Some(ex) = explicit {
            for (i, t) in ex.iter().enumerate() {
                if i < targs.len() {
                    cx.infer.unify(&targs[i], t);
                }
            }
        }
        let def = &self.fns[f];
        let fname = def.name.clone();
        let text = self.fn_sig_text(f);
        let def = &self.fns[f];
        let mut params: Vec<PInfo> = def
            .params
            .iter()
            .map(|p| PInfo { name: p.name.clone(), mode: p.mode, ty: p.ty.subst(&targs, &eargs), decl_ty: p.ty.clone(), default: p.default.clone() })
            .collect();
        let ret = def.ret.subst(&targs, &eargs);
        let eff = def.eff.subst(&eargs);
        let mut out = Vec::new();
        if let Some(r) = recv {
            let p0 = params.remove(0);
            if !cx.infer.unify(&r.ty, &p0.ty) {
                let msg = format!("`{fname}` takes `{}`, but this is `{}`", self.show(cx, &p0.ty), self.show(cx, &r.ty));
                self.err(file, r.span, msg);
            }
            if p0.mode == Mode::Inout && self.to_place(cx, &r, &format!("calling `{fname}`, which changes its receiver")).is_none() {
                return self.error_expr(span);
            }
            out.push(TArg { mode: p0.mode, expr: r, copy: false });
        }
        // Let an expected result type guide inference (`let m: Map[str, int] = Map.new()`).
        if let Some(w) = want {
            let (r, w2) = (cx.infer.shallow(&ret), cx.infer.shallow(w));
            let same_head = matches!((&r, &w2), (Ty::Adt(a, _), Ty::Adt(b, _)) if a == b)
                || matches!((&r, &w2), (Ty::Array(_), Ty::Array(_)) | (Ty::Opt(_), Ty::Opt(_)) | (Ty::Tuple(_), Ty::Tuple(_)));
            if same_head && ret.has_vars() {
                cx.infer.unify(&ret, w);
            }
        }
        let Some(slots) = self.match_args(cx, &fname, &text, &params, args, span) else {
            for a in args {
                self.expr(cx, &a.value, None);
            }
            return TExpr { kind: TK::Unit, ty: Ty::Error, span };
        };
        for (p, slot) in params.iter().zip(slots) {
            match slot {
                Some(a) => out.push(self.check_arg(cx, &fname, p, a)),
                None => {
                    let mut d = p.default.clone().expect("match_args checked defaults");
                    d.span = span;
                    out.push(TArg { mode: p.mode, expr: d, copy: false });
                }
            }
        }
        self.exclusivity(cx, &mut out, &fname);
        let resolved = cx.infer.resolve_eff(&eff);
        self.use_effects(cx, &Eff { fail: false, ..resolved }, span, &fname);
        TExpr { kind: TK::Call { f, targs, eargs, args: out }, ty: ret, span }
    }

    fn check_arg(&mut self, cx: &mut FnCx, fname: &str, p: &PInfo, a: &Arg) -> TArg {
        let file = cx.file;
        if p.mode == Mode::Inout {
            let t = self.expr(cx, &a.value, Some(&p.ty));
            if !a.inout {
                let shown = self.src(file).slice(a.value.span).to_string();
                self.err(file, a.value.span, format!("`{fname}` changes this argument; mark it: `inout {shown}`"));
            }
            if !cx.infer.unify(&t.ty, &p.ty) {
                let msg = format!("argument `{}` of `{fname}` must be `{}`, found `{}`", p.name, self.show(cx, &p.ty), self.show(cx, &t.ty));
                self.err(file, t.span, msg);
            }
            self.to_place(cx, &t, "an `inout` argument");
            return TArg { mode: Mode::Inout, expr: t, copy: false };
        }
        if a.inout {
            self.err(file, a.span, format!("remove `inout`: `{fname}` doesn't change its `{}` argument", p.name));
        }
        let t = self.expr(cx, &a.value, Some(&p.ty));
        let t = if matches!(cx.infer.shallow(&p.ty), Ty::Fn(_)) && matches!(cx.infer.shallow(&t.ty), Ty::Fn(_)) {
            self.coerce_fn(cx, t, &p.ty)
        } else {
            self.expect(cx, t, &p.ty, &format!("argument `{}` of `{fname}`", p.name))
        };
        TArg { mode: p.mode, expr: t, copy: false }
    }

    /// Puts arguments in parameter order: positional first, then named in
    /// any order, defaults for the rest. Where parameters share a type, all
    /// but the first of their arguments must be named.
    fn match_args<'e>(&mut self, cx: &mut FnCx, fname: &str, text: &str, params: &[PInfo], args: &'e [Arg], span: Span) -> Option<Vec<Option<&'e Arg>>> {
        let file = cx.file;
        let mut slots: Vec<Option<&Arg>> = vec![None; params.len()];
        let mut positional = 0;
        let mut seen_named = false;
        let mut ok = true;
        for a in args {
            match &a.name {
                Some(id) => {
                    seen_named = true;
                    match params.iter().position(|p| p.name == id.name) {
                        Some(i) if slots[i].is_some() => {
                            self.err(file, id.span, format!("`{}` is given twice", id.name));
                            ok = false;
                        }
                        Some(i) => slots[i] = Some(a),
                        None => {
                            let hint = closest(&id.name, params.iter().map(|p| p.name.as_str())).map(|s| format!("; did you mean `{s}`?")).unwrap_or_default();
                            self.err(file, id.span, format!("`{fname}` has no parameter `{}`{hint} ({text})", id.name));
                            ok = false;
                        }
                    }
                }
                None => {
                    if seen_named {
                        self.err(file, a.span, "positional arguments come before named ones");
                        ok = false;
                        continue;
                    }
                    if positional < params.len() {
                        slots[positional] = Some(a);
                    }
                    positional += 1;
                }
            }
        }
        if positional > params.len() {
            let n = params.len();
            self.err(file, span, format!("`{fname}` takes {n} argument{}, got {} ({text})", if n == 1 { "" } else { "s" }, args.len()));
            return None;
        }
        let missing: Vec<String> = params.iter().zip(&slots).filter(|(p, s)| s.is_none() && p.default.is_none()).map(|(p, _)| format!("`{}`", p.name)).collect();
        if !missing.is_empty() {
            self.err(file, span, format!("missing argument{} {} for `{fname}` ({text})", if missing.len() == 1 { "" } else { "s" }, missing.join(", ")));
            return None;
        }
        if !ok {
            return None;
        }
        for i in 0..params.len() {
            let Some(a) = slots[i] else { continue };
            let earlier_same = (0..i).any(|j| slots[j].is_some() && params[j].decl_ty == params[i].decl_ty);
            if !earlier_same {
                continue;
            }
            // A literal can't be a swapped variable, so it counts as named too.
            let literal = matches!(&a.value.kind, ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str(_) | ExprKind::Bool(_) | ExprKind::None)
                || matches!(&a.value.kind, ExprKind::Unary(ast::UnOp::Neg, inner) if matches!(inner.kind, ExprKind::Int(_) | ExprKind::Float(_)));
            let named = literal || a.name.is_some() || matches!(&a.value.kind, ExprKind::Ident(n) if *n == params[i].name);
            if !named {
                let tname = self.show(cx, &params[i].ty);
                self.err(
                    file,
                    a.span,
                    format!("name this argument: `{}: ...`; `{fname}` has several `{tname}` parameters, so all but the first are passed by name ({text})", params[i].name),
                );
            }
        }
        Some(slots)
    }

    /// Two `inout` arguments can't be the same variable. An argument that
    /// reads a variable another argument passes as `inout` is copied first.
    fn exclusivity(&mut self, cx: &mut FnCx, args: &mut [TArg], fname: &str) {
        let roots: Vec<(usize, LocalId)> = args.iter().enumerate().filter(|(_, a)| a.mode == Mode::Inout).filter_map(|(i, a)| expr_root(&a.expr).map(|r| (i, r))).collect();
        for (n, (i, r)) in roots.iter().enumerate() {
            if let Some((_, _)) = roots[..n].iter().find(|(_, r2)| r2 == r) {
                let name = cx.fr().locals[*r].name.clone();
                self.err(cx.file, args[*i].expr.span, format!("`{name}` is passed as `inout` twice in this call to `{fname}`"));
            }
        }
        for (i, a) in args.iter_mut().enumerate() {
            if a.mode != Mode::Inout && roots.iter().any(|(j, r)| *j != i && mentions(&a.expr, *r)) {
                a.copy = true;
            }
        }
    }

    /// Calls a function value (a closure, or a named function passed around).
    pub fn call_value(&mut self, cx: &mut FnCx, callee: TExpr, args: &[Arg], span: Span) -> TExpr {
        let file = cx.file;
        let fty = cx.infer.shallow(&callee.ty);
        let Ty::Fn(ft) = fty else {
            if fty != Ty::Error {
                let msg = format!("this is `{}`, not a function", self.show(cx, &callee.ty));
                self.err(file, callee.span, msg);
            }
            for a in args {
                self.expr(cx, &a.value, None);
            }
            return self.error_expr(span);
        };
        if args.len() != ft.params.len() {
            let n = ft.params.len();
            self.err(file, span, format!("this function takes {n} argument{}, got {}", if n == 1 { "" } else { "s" }, args.len()));
            return self.error_expr(span);
        }
        let mut out = Vec::new();
        for (a, p) in args.iter().zip(&ft.params) {
            if a.name.is_some() {
                self.err(file, a.span, "arguments to a function value can't be named");
            }
            if a.inout {
                self.err(file, a.span, "function values can't take `inout` arguments");
            }
            let t = self.expr(cx, &a.value, Some(p));
            let t = self.expect(cx, t, p, "this argument");
            out.push(TArg { mode: Mode::Read, expr: t, copy: false });
        }
        let eff = cx.infer.resolve_eff(&ft.eff);
        self.use_effects(cx, &Eff { fail: false, ..eff }, span, "this function");
        let ret = ft.ret.clone();
        TExpr { kind: TK::CallValue { callee: Box::new(callee), args: out }, ty: ret, span }
    }

    /// Makes a struct value: `Point(x: 1, y: 2)`.
    fn construct(&mut self, cx: &mut FnCx, adt: AdtId, explicit: Option<Vec<Ty>>, args: &[Arg], span: Span, want: Option<&Ty>) -> TExpr {
        let file = cx.file;
        let name = self.adts[adt].name.clone();
        if self.adts[adt].is_enum() {
            let first = self.adts[adt].variants().first().map(|v| v.name.clone()).unwrap_or_default();
            self.err(file, span, format!("`{name}` is an enum; make a value with one of its variants, like `{name}.{first}(...)` or `.{first}`"));
            return self.error_expr(span);
        }
        let targs = self.adt_targs(cx, adt, span);
        if let Some(ex) = explicit {
            for (t, e) in targs.iter().zip(&ex) {
                cx.infer.unify(t, e);
            }
        }
        let ty = Ty::Adt(adt, targs.clone());
        if let Some(w) = want {
            if matches!(cx.infer.shallow(w), Ty::Adt(id, _) if id == adt) {
                cx.infer.unify(&ty, w);
            }
        }
        let fields: Vec<(String, Ty, Option<TExpr>)> = self.adts[adt].fields().iter().map(|f| (f.name.clone(), f.ty.subst(&targs, &[]), f.default.clone())).collect();
        let private_ok = self.adts[adt].module == self.files[file].module;
        let values = self.field_args(cx, &name, &fields, args, span, private_ok);
        TExpr { kind: TK::Struct { adt, targs, fields: values }, ty, span }
    }

    /// Named field arguments for a struct or variant, in declaration order.
    fn field_args(&mut self, cx: &mut FnCx, what: &str, fields: &[(String, Ty, Option<TExpr>)], args: &[Arg], span: Span, private_ok: bool) -> Vec<TExpr> {
        let file = cx.file;
        let mut given: Vec<Option<TExpr>> = (0..fields.len()).map(|_| None).collect();
        let mut positional = false;
        for a in args {
            let fname = match (&a.name, &a.value.kind) {
                (Some(n), _) => n.name.clone(),
                // A variable with the field's own name counts as named: `Point(x, y)`.
                (None, ExprKind::Ident(n)) if fields.iter().any(|(f, _, _)| f == n) => n.clone(),
                (None, _) => {
                    if !positional {
                        let names: Vec<String> = fields.iter().map(|(n, _, _)| format!("{n}: ...")).collect();
                        self.err(file, a.span, format!("name the fields: `{what}({})`", names.join(", ")));
                    }
                    positional = true;
                    self.expr(cx, &a.value, None);
                    continue;
                }
            };
            if a.inout {
                self.err(file, a.span, "remove `inout`: fields take values");
            }
            let Some(i) = fields.iter().position(|(n, _, _)| *n == fname) else {
                let hint = closest(&fname, fields.iter().map(|(n, _, _)| n.as_str())).map(|s| format!("; did you mean `{s}`?")).unwrap_or_default();
                self.err(file, a.span, format!("`{what}` has no field `{fname}`{hint}"));
                self.expr(cx, &a.value, None);
                continue;
            };
            if fname.starts_with('_') && !private_ok {
                self.err(file, a.span, format!("`{fname}` is private to `{what}`"));
            }
            if given[i].is_some() {
                self.err(file, a.span, format!("`{fname}` is given twice"));
                continue;
            }
            let ty = fields[i].1.clone();
            let t = self.expr(cx, &a.value, Some(&ty));
            let t = if matches!(cx.infer.shallow(&ty), Ty::Fn(_)) && matches!(cx.infer.shallow(&t.ty), Ty::Fn(_)) {
                self.coerce_fn(cx, t, &ty)
            } else {
                self.expect(cx, t, &ty, &format!("field `{fname}`"))
            };
            given[i] = Some(t);
        }
        let mut out = Vec::new();
        let mut missing = Vec::new();
        for (i, g) in given.into_iter().enumerate() {
            match g {
                Some(t) => out.push(t),
                None => match &fields[i].2 {
                    Some(d) => {
                        let mut d = d.clone();
                        d.span = span;
                        out.push(d);
                    }
                    None => {
                        missing.push(format!("`{}`", fields[i].0));
                        out.push(self.error_expr(span));
                    }
                },
            }
        }
        if !missing.is_empty() && !positional {
            self.err(file, span, format!("missing field{} {} for `{what}`", if missing.len() == 1 { "" } else { "s" }, missing.join(", ")));
        }
        out
    }

    /// A variant: `.Circle(r: 1)`, `Shape.Empty`. With `adt` unset, the type comes from `want`.
    pub fn variant_value(&mut self, cx: &mut FnCx, adt: Option<(AdtId, Option<Vec<Ty>>)>, name: &Ident, args: Option<&[Arg]>, span: Span, want: Option<&Ty>) -> TExpr {
        let file = cx.file;
        let (adt, explicit) = match adt {
            Some(a) => a,
            None => match want.map(|w| cx.infer.shallow(w)) {
                Some(Ty::Adt(id, _)) if self.adts[id].is_enum() => (id, None),
                Some(Ty::Opt(inner)) if matches!(cx.infer.shallow(&inner), Ty::Adt(id, _) if self.adts[id].is_enum()) => {
                    let Ty::Adt(id, _) = cx.infer.shallow(&inner) else { unreachable!() };
                    (id, None)
                }
                _ => {
                    self.err(file, span, format!("`.{}` needs a known enum type here; write the type too, like `Kind.{}`", name.name, name.name));
                    if let Some(args) = args {
                        for a in args {
                            self.expr(cx, &a.value, None);
                        }
                    }
                    return self.error_expr(span);
                }
            },
        };
        let tname = self.adts[adt].name.clone();
        let Some(vi) = self.adts[adt].variants().iter().position(|v| v.name == name.name) else {
            let names: Vec<String> = self.adts[adt].variants().iter().map(|v| v.name.clone()).collect();
            let hint = closest(&name.name, names.iter().map(|s| s.as_str())).map(|s| format!("; did you mean `.{s}`?")).unwrap_or_else(|| format!("; the variants are {}", names.iter().map(|n| format!("`.{n}`")).collect::<Vec<_>>().join(", ")));
            self.err(file, name.span, format!("`{tname}` has no variant `{}`{hint}", name.name));
            return self.error_expr(span);
        };
        let targs = self.adt_targs(cx, adt, span);
        if let Some(ex) = explicit {
            for (t, e) in targs.iter().zip(&ex) {
                cx.infer.unify(t, e);
            }
        }
        let ty = Ty::Adt(adt, targs.clone());
        if let Some(w) = want {
            let w = match cx.infer.shallow(w) {
                Ty::Opt(inner) => *inner,
                other => other,
            };
            if matches!(cx.infer.shallow(&w), Ty::Adt(id, _) if id == adt) {
                cx.infer.unify(&ty, &w);
            }
        }
        let fields: Vec<(String, Ty, Option<TExpr>)> = self.adts[adt].variants()[vi].fields.iter().map(|f| (f.name.clone(), f.ty.subst(&targs, &[]), None)).collect();
        let values = match args {
            None if fields.is_empty() => Vec::new(),
            None => {
                let names: Vec<String> = fields.iter().map(|(n, _, _)| format!("{n}: ...")).collect();
                self.err(file, span, format!("`.{}` has fields; write `.{}({})`", name.name, name.name, names.join(", ")));
                return self.error_expr(span);
            }
            Some(args) if fields.is_empty() => {
                if !args.is_empty() {
                    self.err(file, span, format!("`.{}` has no fields; write `.{}` without parentheses", name.name, name.name));
                } else {
                    self.err(file, span, format!("`.{}` has no fields; drop the `()`", name.name));
                }
                Vec::new()
            }
            Some(args) => self.field_args(cx, &format!(".{}", name.name), &fields, args, span, true),
        };
        TExpr { kind: TK::Variant { adt, targs, variant: vi as u32, fields: values }, ty, span }
    }

    /// `int(x)`, `u8(x)`, `f64(x)`: checked conversions between number types.
    fn convert(&mut self, cx: &mut FnCx, to: Ty, args: &[Arg], span: Span) -> TExpr {
        let file = cx.file;
        let tname = self.ty_name(&to);
        if args.len() != 1 || args[0].name.is_some() {
            self.err(file, span, format!("`{tname}(x)` converts one number"));
            return self.error_expr(span);
        }
        let t = self.expr(cx, &args[0].value, None);
        if let Ty::Var(v) = cx.infer.shallow(&t.ty) {
            if matches!(t.kind, TK::Int(_) | TK::Float(_)) {
                // A literal: it simply takes the target type.
                cx.infer.unify(&t.ty, &to);
                return t;
            }
            // A variable holding a literal: it has its default type.
            match cx.infer.kind(v) {
                Some(VarKind::IntLit) => {
                    cx.infer.unify(&t.ty, &Ty::INT);
                }
                Some(VarKind::FloatLit) => {
                    cx.infer.unify(&t.ty, &Ty::F64);
                }
                _ => {}
            }
        }
        let from = cx.infer.shallow(&t.ty);
        match from {
            Ty::Int(_) | Ty::Float(_) => TExpr { kind: TK::Convert(Box::new(t)), ty: to, span },
            Ty::Error => self.error_expr(span),
            Ty::Str => {
                self.err(file, span, format!("`{tname}(s)` doesn't parse strings; use `{tname}.parse(s)`"));
                self.error_expr(span)
            }
            Ty::Bool => {
                self.err(file, span, format!("booleans don't convert to numbers; write `if b {{ 1 }} else {{ 0 }}`"));
                self.error_expr(span)
            }
            other => {
                let msg = format!("`{tname}(x)` converts numbers, but this is `{}`", self.show(cx, &other));
                self.err(file, span, msg);
                self.error_expr(span)
            }
        }
    }

    /// `print`, `eprint`, `dbg`, `trap`, `assert`, `todo`, `fail` and `hash`.
    fn builtin_call(&mut self, cx: &mut FnCx, name: &str, args: &[Arg], span: Span, want: Option<&Ty>) -> TExpr {
        let file = cx.file;
        let _ = want;
        let mk = |kind, ty| TExpr { kind, ty, span };
        let arity = |c: &mut Checker, n: usize, sig: &str| {
            if args.len() != n {
                c.err(file, span, format!("`{sig}` takes {n} argument{}", if n == 1 { "" } else { "s" }));
                false
            } else {
                true
            }
        };
        match name {
            "print" | "eprint" => {
                if !arity(self, 1, &format!("{name}(x)")) {
                    return self.error_expr(span);
                }
                let t = self.expr(cx, &args[0].value, None);
                if cx.infer.shallow(&t.ty) == Ty::Unit {
                    self.err(file, t.span, "this returns nothing, so there's nothing to print");
                }
                self.use_effects(cx, &Eff { io: true, ..Eff::pure() }, span, name);
                mk(TK::Print { value: Box::new(t), stderr: name == "eprint" }, Ty::Unit)
            }
            "dbg" => {
                if !arity(self, 1, "dbg(x)") {
                    return self.error_expr(span);
                }
                let t = self.expr(cx, &args[0].value, None);
                let text = self.src(file).slice(args[0].value.span).to_string();
                let ty = t.ty.clone();
                mk(TK::Dbg { value: Box::new(t), text }, ty)
            }
            "trap" => {
                if !arity(self, 1, "trap(msg: str)") {
                    return self.error_expr(span);
                }
                let t = self.expr(cx, &args[0].value, Some(&Ty::Str));
                let t = self.expect(cx, t, &Ty::Str, "the message");
                mk(TK::Trap(Box::new(t)), Ty::Never)
            }
            "todo" => {
                if !arity(self, 0, "todo()") {
                    return self.error_expr(span);
                }
                mk(TK::Todo, Ty::Never)
            }
            "assert" => {
                if args.is_empty() || args.len() > 2 {
                    self.err(file, span, "`assert(cond, msg: str = \"\")` takes a condition and an optional message");
                    return self.error_expr(span);
                }
                let c = self.expr(cx, &args[0].value, Some(&Ty::Bool));
                let c = self.expect(cx, c, &Ty::Bool, "the condition");
                let msg = args.get(1).map(|a| {
                    let t = self.expr(cx, &a.value, Some(&Ty::Str));
                    Box::new(self.expect(cx, t, &Ty::Str, "the message"))
                });
                let text = self.src(file).slice(args[0].value.span).to_string();
                mk(TK::Assert { cond: Box::new(c), msg, text }, Ty::Unit)
            }
            "fail" => {
                if args.len() != 2 {
                    self.err(file, span, "`fail(kind: ErrKind, msg: str)` takes a kind like `.Invalid` and a message");
                    return self.error_expr(span);
                }
                let kty = Ty::Adt(self.known.err_kind, Vec::new());
                let k = self.expr(cx, &args[0].value, Some(&kty));
                let k = self.expect(cx, k, &kty, "the error kind");
                let m = self.expr(cx, &args[1].value, Some(&Ty::Str));
                let m = self.expect(cx, m, &Ty::Str, "the message");
                self.use_effects(cx, &Eff { fail: true, ..Eff::pure() }, span, "fail");
                mk(TK::Fail { kind: Box::new(k), msg: Box::new(m) }, Ty::Never)
            }
            _ => {
                if !arity(self, 1, "hash(x)") {
                    return self.error_expr(span);
                }
                let t = self.expr(cx, &args[0].value, None);
                let ty = t.ty.clone();
                self.require_bound(cx, &ty, "Hash", span);
                mk(TK::Hash(Box::new(t)), Ty::Int(IntTy::U64))
            }
        }
    }
}

/// Whether a method's receiver type (with parameters) fits an actual type.
fn recv_matches(pat: &Ty, actual: &Ty) -> bool {
    match (pat, actual) {
        (Ty::Param(_), _) | (_, Ty::Var(_)) | (_, Ty::Error) => true,
        (Ty::Array(p), Ty::Array(a)) | (Ty::Opt(p), Ty::Opt(a)) => recv_matches(p, a),
        (Ty::Adt(i, ps), Ty::Adt(j, as_)) => i == j && ps.iter().zip(as_).all(|(p, a)| recv_matches(p, a)),
        (Ty::Tuple(ps), Ty::Tuple(as_)) => ps.len() == as_.len() && ps.iter().zip(as_).all(|(p, a)| recv_matches(p, a)),
        (p, a) => p == a,
    }
}

/// The variable at the root of a place-shaped expression.
pub fn expr_root(e: &TExpr) -> Option<LocalId> {
    match &e.kind {
        TK::Local(id) => Some(*id),
        TK::Field { base, .. } | TK::Index { base, .. } => expr_root(base),
        _ => None,
    }
}

/// Whether an expression reads variable `id` anywhere.
fn mentions(e: &TExpr, id: LocalId) -> bool {
    let mut found = false;
    super::zonk::walk_expr(e, &mut |x| {
        if let TK::Local(l) = &x.kind {
            found |= *l == id;
        }
    });
    found
}

/// An expression written where a type is expected (`Page[int]`, `json.decode[User]`).
fn expr_as_type(e: &Expr) -> Option<ast::TypeExpr> {
    let kind = match &e.kind {
        ExprKind::Type(t) => return Some(t.clone()),
        ExprKind::Ident(n) => ast::TypeKind::Named { path: vec![Ident { name: n.clone(), span: e.span }], args: Vec::new() },
        ExprKind::Array { items, .. } if items.len() == 1 => ast::TypeKind::Array(Box::new(expr_as_type(&items[0])?)),
        ExprKind::Index { base, args } => {
            let ExprKind::Ident(n) = &base.kind else { return None };
            let args = args.iter().map(expr_as_type).collect::<Option<Vec<_>>>()?;
            ast::TypeKind::Named { path: vec![Ident { name: n.clone(), span: base.span }], args }
        }
        ExprKind::Tuple(items) => ast::TypeKind::Tuple(items.iter().map(expr_as_type).collect::<Option<Vec<_>>>()?),
        _ => return None,
    };
    Some(ast::TypeExpr { kind, span: e.span })
}
