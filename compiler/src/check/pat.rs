//! `match`: patterns, and checking that every case is handled.
//!
//! Exhaustiveness uses the usefulness algorithm: a `match` is exhaustive when
//! a wildcard after the last arm could never match anything. The same search
//! produces an example of a missing case for the error message.

use super::body::FnCx;
use super::infer::VarKind;
use super::*;
use crate::ast::{Expr, ExprKind, PatKind};

impl<'a> Checker<'a> {
    pub fn match_expr(&mut self, cx: &mut FnCx, scrutinee: &Expr, arms: &[ast::Arm], span: Span, want: Option<&Ty>, value: bool) -> TExpr {
        let file = cx.file;
        let s = self.expr(cx, scrutinee, None);
        let sty = s.ty.clone();
        let result = if value { Some(want.cloned().unwrap_or_else(|| cx.infer.fresh(VarKind::Any))) } else { None };
        let mut tarms = Vec::new();
        let mut all_never = !arms.is_empty();
        for arm in arms {
            cx.frame().scopes.push(Vec::new());
            let pat = self.pattern(cx, &arm.pat, &sty);
            let guard = arm.guard.as_ref().map(|g| {
                let t = self.expr(cx, g, Some(&Ty::Bool));
                self.expect(cx, t, &Ty::Bool, "a guard")
            });
            let body = match (&arm.body.kind, &result) {
                (ExprKind::Block(b), _) => {
                    let (tb, t) = self.block(cx, b, value, result.as_ref());
                    TExpr { kind: TK::Block(tb), ty: t, span: arm.body.span }
                }
                (_, Some(r)) => {
                    let t = self.expr(cx, &arm.body, Some(r));
                    if t.ty == Ty::Never { t } else { self.expect(cx, t, r, "this arm's value") }
                }
                (_, None) => {
                    let t = self.expr_inner(cx, &arm.body, None, false);
                    if self.failable(cx, &t) {
                        self.err(file, t.span, "this call can fail; handle it with `?`, `else` or `catch`");
                    }
                    let ty = cx.infer.shallow(&t.ty);
                    if !matches!(ty, Ty::Unit | Ty::Never | Ty::Error) && !matches!(t.kind, TK::Dbg { .. }) {
                        self.err(file, t.span, "this value is unused; use it, or discard it with `_ = ...`");
                    }
                    t
                }
            };
            if body.ty != Ty::Never {
                all_never = false;
            }
            cx.frame().scopes.pop();
            tarms.push(TArm { pat, guard, body });
        }
        self.check_exhaustive(cx, &sty, &tarms, span);
        let ty = if all_never {
            Ty::Never
        } else if let Some(r) = result {
            r
        } else {
            Ty::Unit
        };
        TExpr { kind: TK::Match { scrut: Box::new(s), arms: tarms }, ty, span }
    }

    pub fn pattern(&mut self, cx: &mut FnCx, p: &ast::Pattern, ty: &Ty) -> TPat {
        let file = cx.file;
        let t = cx.infer.shallow(ty);
        let mismatch = |c: &mut Checker, cx: &mut FnCx, what: &str| {
            let msg = format!("this pattern is {what}, but the value is `{}`", c.show(cx, ty));
            c.err(file, p.span, msg);
            TPat::Wild
        };
        match &p.kind {
            PatKind::Wild => TPat::Wild,
            PatKind::Bind(name) => {
                if let Ty::Adt(id, _) = &t {
                    if self.adts[*id].is_enum() && self.adts[*id].variants().iter().any(|v| v.name == *name) {
                        self.err(file, p.span, format!("write `.{name}` to match the variant; a bare name binds the whole value"));
                        return TPat::Wild;
                    }
                }
                let id = self.declare(cx, name, ty.clone(), LocalKind::Borrowed, false, p.span);
                TPat::Bind(id)
            }
            PatKind::Lit(e) => self.lit_pattern(cx, e, ty, p.span),
            PatKind::Range { lo, hi, inclusive } => {
                let (Some(l), Some(h)) = (self.pat_int(cx, lo, ty), self.pat_int(cx, hi, ty)) else { return TPat::Wild };
                if l > h || (l == h && !inclusive) {
                    self.err(file, p.span, "this range matches nothing");
                }
                TPat::Range { lo: l, hi: h, inclusive: *inclusive }
            }
            PatKind::Tuple(items) => {
                let tys: Vec<Ty> = match &t {
                    Ty::Tuple(ts) if ts.len() == items.len() => ts.clone(),
                    Ty::Var(_) => {
                        let ts: Vec<Ty> = items.iter().map(|_| cx.infer.fresh(VarKind::Any)).collect();
                        cx.infer.unify(ty, &Ty::Tuple(ts.clone()));
                        ts
                    }
                    Ty::Error => return TPat::Wild,
                    _ => return mismatch(self, cx, &format!("a tuple of {}", items.len())),
                };
                TPat::Tuple(items.iter().zip(&tys).map(|(i, t)| self.pattern(cx, i, t)).collect())
            }
            PatKind::Array(items) => {
                let elem = match &t {
                    Ty::Array(e) => (**e).clone(),
                    Ty::Error => return TPat::Wild,
                    _ => return mismatch(self, cx, "an array"),
                };
                let mut before = Vec::new();
                let mut after = Vec::new();
                let mut rest = None;
                for it in items {
                    if let PatKind::Rest(name) = &it.kind {
                        if rest.is_some() {
                            self.err(file, it.span, "an array pattern can have one `..`");
                            continue;
                        }
                        let bind = name.as_ref().map(|n| self.declare(cx, &n.name, ty.clone(), LocalKind::Owned, false, n.span));
                        rest = Some(bind);
                        continue;
                    }
                    let tp = self.pattern(cx, it, &elem);
                    if rest.is_some() { after.push(tp) } else { before.push(tp) }
                }
                TPat::Array { before, rest, after }
            }
            PatKind::Rest(_) => {
                self.err(file, p.span, "`..` only goes inside an array pattern, like `[first, ..rest]`");
                TPat::Wild
            }
            PatKind::Variant { ty: path, name, fields } => {
                let Ty::Adt(adt, targs) = &t else {
                    if t == Ty::Error {
                        return TPat::Wild;
                    }
                    return mismatch(self, cx, &format!("the variant `.{}`", name.name));
                };
                if !self.adts[*adt].is_enum() {
                    return mismatch(self, cx, &format!("the variant `.{}`", name.name));
                }
                if let Some(first) = path.first() {
                    if first.name != self.adts[*adt].name {
                        let n = self.adts[*adt].name.clone();
                        self.err(file, first.span, format!("the value is a `{n}`, so write `.{}` or `{n}.{}`", name.name, name.name));
                    }
                }
                let Some(vi) = self.adts[*adt].variants().iter().position(|v| v.name == name.name) else {
                    let tname = self.adts[*adt].name.clone();
                    let names: Vec<String> = self.adts[*adt].variants().iter().map(|v| v.name.clone()).collect();
                    let hint = closest(&name.name, names.iter().map(|s| s.as_str())).map(|s| format!("; did you mean `.{s}`?")).unwrap_or_default();
                    self.err(file, name.span, format!("`{tname}` has no variant `{}`{hint}", name.name));
                    return TPat::Wild;
                };
                let vfields: Vec<(String, Ty)> = self.adts[*adt].variants()[vi].fields.iter().map(|f| (f.name.clone(), f.ty.subst(targs, &[]))).collect();
                let mut out = Vec::new();
                match fields {
                    None if !vfields.is_empty() => {}
                    None => {}
                    Some(fps) => {
                        if vfields.is_empty() {
                            self.err(file, p.span, format!("`.{}` has no fields; write `.{}`", name.name, name.name));
                        }
                        for fp in fps {
                            let Some(fi) = vfields.iter().position(|(n, _)| *n == fp.name.name) else {
                                let hint = closest(&fp.name.name, vfields.iter().map(|(n, _)| n.as_str())).map(|s| format!("; did you mean `{s}`?")).unwrap_or_default();
                                self.err(file, fp.name.span, format!("`.{}` has no field `{}`{hint}", name.name, fp.name.name));
                                continue;
                            };
                            let fty = vfields[fi].1.clone();
                            let sub = match &fp.pat {
                                Some(sp) => self.pattern(cx, sp, &fty),
                                None => {
                                    let id = self.declare(cx, &fp.name.name, fty, LocalKind::Borrowed, false, fp.name.span);
                                    TPat::Bind(id)
                                }
                            };
                            out.push((fi as u32, sub));
                        }
                    }
                }
                TPat::Variant { variant: vi as u32, fields: out }
            }
            PatKind::Or(alts) => {
                let before = cx.fr().locals.len();
                let pats: Vec<TPat> = alts.iter().map(|a| self.pattern(cx, a, ty)).collect();
                if cx.fr().locals.len() != before {
                    self.err(file, p.span, "patterns joined with `|` can't bind names; use separate arms");
                }
                TPat::Or(pats)
            }
        }
    }

    fn lit_pattern(&mut self, cx: &mut FnCx, e: &Expr, ty: &Ty, span: Span) -> TPat {
        let file = cx.file;
        let t = cx.infer.shallow(ty);
        let bad = |c: &mut Checker, cx: &mut FnCx, what: &str| {
            let msg = format!("this pattern is {what}, but the value is `{}`", c.show(cx, ty));
            c.err(file, span, msg);
            TPat::Wild
        };
        match &e.kind {
            ExprKind::None => {
                if matches!(t, Ty::Opt(_) | Ty::Error) {
                    TPat::None
                } else {
                    bad(self, cx, "`none`")
                }
            }
            ExprKind::Bool(b) => {
                if cx.infer.unify(ty, &Ty::Bool) {
                    TPat::Bool(*b)
                } else {
                    bad(self, cx, "a `bool`")
                }
            }
            ExprKind::Str(s) => {
                if !cx.infer.unify(ty, &Ty::Str) {
                    return bad(self, cx, "a string");
                }
                match self.decode_plain(s, file) {
                    Some(v) => TPat::Str(v.into_bytes()),
                    None => TPat::Wild,
                }
            }
            ExprKind::Float(s) => {
                if !matches!(t, Ty::Float(_) | Ty::Var(_)) {
                    return bad(self, cx, "a float");
                }
                TPat::Float(s.replace('_', "").parse().unwrap_or(0.0))
            }
            _ => match self.pat_int(cx, e, ty) {
                Some(v) => TPat::Int(v),
                None => TPat::Wild,
            },
        }
    }

    /// An integer or byte literal in a pattern.
    fn pat_int(&mut self, cx: &mut FnCx, e: &Expr, ty: &Ty) -> Option<i128> {
        let file = cx.file;
        let (v, is_byte) = match &e.kind {
            ExprKind::Int(s) => (super::body::parse_int(s)?, false),
            ExprKind::Unary(ast::UnOp::Neg, inner) => match &inner.kind {
                ExprKind::Int(s) => (-super::body::parse_int(s)?, false),
                _ => {
                    self.err(file, e.span, "a pattern takes a literal here");
                    return None;
                }
            },
            ExprKind::Byte(s) => (super::body::decode_byte(s)? as i128, true),
            _ => {
                self.err(file, e.span, "a pattern takes a literal here");
                return None;
            }
        };
        let t = cx.infer.shallow(ty);
        let ok = match &t {
            Ty::Int(IntTy::U8) => true,
            Ty::Int(_) => !is_byte,
            Ty::Var(_) => cx.infer.unify(ty, &if is_byte { Ty::U8 } else { Ty::INT }),
            Ty::Error => true,
            _ => false,
        };
        if !ok {
            let msg = format!("this pattern is a number, but the value is `{}`", self.show(cx, ty));
            self.err(file, e.span, msg);
            return None;
        }
        if let Ty::Int(k) = cx.infer.shallow(ty) {
            if v < k.min_value() || v > k.max_value() {
                self.err(file, e.span, format!("`{v}` doesn't fit in `{}`", k.name()));
            }
        }
        Some(v)
    }

    fn check_exhaustive(&mut self, cx: &mut FnCx, ty: &Ty, arms: &[TArm], span: Span) {
        let ty = cx.infer.resolve(ty);
        if ty == Ty::Error {
            return;
        }
        let rows: Vec<Vec<&TPat>> = arms.iter().filter(|a| a.guard.is_none()).map(|a| vec![&a.pat]).collect();
        let space = Space { c: self, cx };
        if let Some(w) = space.witness(&rows, &[ty]) {
            let example = w.into_iter().next().unwrap_or_else(|| "_".into());
            let msg = if example == "_" {
                "this `match` doesn't handle every value; add a `_ => ...` arm".to_string()
            } else {
                format!("this `match` doesn't handle `{example}`; add an arm for it, or `_ => ...`")
            };
            self.err(cx.file, span, msg);
        }
    }
}

/// Constructors for exhaustiveness: the ways a value of a type can start.
#[derive(Clone, PartialEq, Debug)]
enum Ctor {
    Bool(bool),
    Variant(u32),
    None,
    /// An optional that isn't `none`; no pattern names it, but it completes `?T`.
    Some,
    Tuple,
    /// Arrays of exactly this length, or at least this length when `rest`.
    Len(usize, bool),
    Int(i128),
    Str(Vec<u8>),
    Float(u64),
}

struct Space<'c, 'a, 'x> {
    c: &'c Checker<'a>,
    cx: &'x FnCx,
}

impl Space<'_, '_, '_> {
    fn resolve(&self, t: &Ty) -> Ty {
        self.cx.infer.resolve(t)
    }

    /// Every constructor of a type, or `None` when there are too many to list.
    fn all(&self, ty: &Ty, rows: &[Vec<&TPat>]) -> Option<Vec<Ctor>> {
        match ty {
            Ty::Bool => Some(vec![Ctor::Bool(true), Ctor::Bool(false)]),
            Ty::Adt(id, _) if self.c.adts[*id].is_enum() => Some((0..self.c.adts[*id].variants().len() as u32).map(Ctor::Variant).collect()),
            Ty::Opt(_) => Some(vec![Ctor::None, Ctor::Some]),
            Ty::Tuple(_) | Ty::Adt(..) | Ty::Unit => Some(vec![Ctor::Tuple]),
            Ty::Array(_) => {
                // Lengths up to the longest fixed part, then "that long or longer".
                let mut max = 0;
                for r in rows {
                    for p in expand_or(r[0]) {
                        if let TPat::Array { before, after, rest } = p {
                            max = max.max(before.len() + after.len() + if rest.is_some() { 0 } else { 1 });
                        }
                    }
                }
                let mut out: Vec<Ctor> = (0..max).map(|n| Ctor::Len(n, false)).collect();
                out.push(Ctor::Len(max, true));
                Some(out)
            }
            _ => None,
        }
    }

    fn arity_tys(&self, ty: &Ty, c: &Ctor) -> Vec<Ty> {
        match (ty, c) {
            (Ty::Adt(id, targs), Ctor::Variant(v)) => self.c.adts[*id].variants()[*v as usize].fields.iter().map(|f| f.ty.subst(targs, &[])).collect(),
            (Ty::Tuple(ts), Ctor::Tuple) => ts.clone(),
            (Ty::Array(e), Ctor::Len(n, _)) => vec![(**e).clone(); *n],
            _ => Vec::new(),
        }
    }

    /// The row's patterns after the first, if its first pattern covers `c`.
    fn specialize<'p>(&self, row: &[&'p TPat], c: &Ctor, arity: usize) -> Vec<Vec<&'p TPat>> {
        let rest = &row[1..];
        let mut out = Vec::new();
        for p in expand_or(row[0]) {
            let head: Option<Vec<&TPat>> = match p {
                TPat::Wild | TPat::Bind(_) => Some(vec![&TPat::Wild; arity]),
                TPat::Bool(b) => (*c == Ctor::Bool(*b)).then(Vec::new),
                TPat::None => (*c == Ctor::None).then(Vec::new),
                TPat::Int(v) => (*c == Ctor::Int(*v)).then(Vec::new),
                TPat::Str(s) => (*c == Ctor::Str(s.clone())).then(Vec::new),
                TPat::Float(f) => (*c == Ctor::Float(f.to_bits())).then(Vec::new),
                TPat::Range { lo, hi, inclusive } => match c {
                    Ctor::Int(v) => (*v >= *lo && (if *inclusive { *v <= *hi } else { *v < *hi })).then(Vec::new),
                    _ => None,
                },
                TPat::Tuple(items) => (*c == Ctor::Tuple).then(|| items.iter().collect()),
                TPat::Variant { variant, fields } => match c {
                    Ctor::Variant(v) if v == variant => {
                        let mut cols = vec![&TPat::Wild; arity];
                        for (i, fp) in fields {
                            cols[*i as usize] = fp;
                        }
                        Some(cols)
                    }
                    _ => None,
                },
                TPat::Array { before, rest: r, after } => match c {
                    Ctor::Len(n, open) => {
                        let fixed = before.len() + after.len();
                        let fits = if r.is_some() { *n >= fixed } else { !open && *n == fixed };
                        fits.then(|| {
                            let mut cols: Vec<&TPat> = before.iter().collect();
                            cols.extend(std::iter::repeat_n(&TPat::Wild, n.saturating_sub(fixed)));
                            cols.extend(after.iter());
                            cols
                        })
                    }
                    _ => None,
                },
                TPat::Or(_) => unreachable!("expanded above"),
            };
            if let Some(mut h) = head {
                h.extend_from_slice(rest);
                out.push(h);
            }
        }
        out
    }

    fn head_ctors(&self, rows: &[Vec<&TPat>]) -> Vec<Ctor> {
        let mut out = Vec::new();
        for r in rows {
            for p in expand_or(r[0]) {
                let c = match p {
                    TPat::Bool(b) => Ctor::Bool(*b),
                    TPat::None => Ctor::None,
                    TPat::Int(v) => Ctor::Int(*v),
                    TPat::Str(s) => Ctor::Str(s.clone()),
                    TPat::Float(f) => Ctor::Float(f.to_bits()),
                    TPat::Tuple(_) => Ctor::Tuple,
                    TPat::Variant { variant, .. } => Ctor::Variant(*variant),
                    _ => continue,
                };
                if !out.contains(&c) {
                    out.push(c);
                }
            }
        }
        out
    }

    /// A list of values (shown as patterns) that no row matches, if there is one.
    fn witness(&self, rows: &[Vec<&TPat>], tys: &[Ty]) -> Option<Vec<String>> {
        if tys.is_empty() {
            return rows.is_empty().then(Vec::new);
        }
        let ty = self.resolve(&tys[0]);
        let all = self.all(&ty, rows);
        let used = self.head_ctors(rows);
        let complete = match &all {
            Some(all) => {
                // Arrays are complete by length; `Some` is covered only by wildcards.
                matches!(ty, Ty::Array(_) | Ty::Tuple(_) | Ty::Unit) || matches!(ty, Ty::Adt(id, _) if !self.c.adts[id].is_enum())
                    || all.iter().filter(|c| **c != Ctor::Some).all(|c| used.contains(c)) && !all.contains(&Ctor::Some)
            }
            None => false,
        };
        if complete {
            for c in all.unwrap() {
                let sub = self.arity_tys(&ty, &c);
                let spec: Vec<Vec<&TPat>> = rows.iter().flat_map(|r| self.specialize(r, &c, sub.len())).collect();
                let mut next_tys = sub.clone();
                next_tys.extend_from_slice(&tys[1..]);
                if let Some(w) = self.witness(&spec, &next_tys) {
                    let (args, rest) = w.split_at(sub.len());
                    let mut out = vec![self.show_ctor(&ty, &c, args)];
                    out.extend_from_slice(rest);
                    return Some(out);
                }
            }
            return None;
        }
        // Some constructor is missing: rows starting with a wildcard decide.
        let default: Vec<Vec<&TPat>> = rows
            .iter()
            .flat_map(|r| expand_or(r[0]).into_iter().filter(|p| matches!(p, TPat::Wild | TPat::Bind(_))).map(move |_| r[1..].to_vec()))
            .collect();
        let w = self.witness(&default, &tys[1..])?;
        let head = match &all {
            Some(all) => {
                let missing = all.iter().find(|c| !used.contains(c) && **c != Ctor::Some);
                match missing {
                    Some(c) => {
                        let n = self.arity_tys(&ty, c).len();
                        self.show_ctor(&ty, c, &vec!["_".to_string(); n])
                    }
                    None => "_".to_string(),
                }
            }
            None => "_".to_string(),
        };
        let mut out = vec![head];
        out.extend(w);
        Some(out)
    }

    fn show_ctor(&self, ty: &Ty, c: &Ctor, args: &[String]) -> String {
        match (ty, c) {
            (_, Ctor::Bool(b)) => b.to_string(),
            (_, Ctor::None) => "none".into(),
            (_, Ctor::Some) => "_".into(),
            (Ty::Adt(id, _), Ctor::Variant(v)) => {
                let var = &self.c.adts[*id].variants()[*v as usize];
                if var.fields.is_empty() {
                    format!(".{}", var.name)
                } else {
                    let fs: Vec<String> = var.fields.iter().zip(args).map(|(f, a)| if a == "_" { f.name.clone() } else { format!("{}: {a}", f.name) }).collect();
                    format!(".{}({})", var.name, fs.join(", "))
                }
            }
            (Ty::Tuple(_), Ctor::Tuple) => format!("({})", args.join(", ")),
            (_, Ctor::Tuple) => "_".into(),
            (_, Ctor::Len(_, open)) => {
                let mut items: Vec<String> = args.to_vec();
                if *open {
                    items.push("..".into());
                }
                format!("[{}]", items.join(", "))
            }
            _ => "_".into(),
        }
    }
}

fn expand_or(p: &TPat) -> Vec<&TPat> {
    match p {
        TPat::Or(alts) => alts.iter().flat_map(expand_or).collect(),
        other => vec![other],
    }
}
