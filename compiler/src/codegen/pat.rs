//! `match`. Each arm tests its pattern against the scrutinee (held in a
//! slot), binds its names, checks its guard, and stores its value.
//! Testing has no side effects, so a failed test just moves to the next arm.

use super::*;
use crate::source::Span;

impl<'p> Gen<'p> {
    pub fn match_expr(&mut self, ty: &Ty, scrut: &TExpr, arms: &[TArm], span: Span) -> (V, bool) {
        let sty = self.sub(&scrut.ty);
        self.push_scope(false);
        let sv = self.borrow(scrut);
        let sp = self.spill(&sv);
        let has_value = !matches!(ty, Ty::Unit | Ty::Never);
        let lt = self.lty(ty);
        let result = if has_value { Some(self.alloca(&lt)) } else { None };
        let end = self.label("endmatch");
        for arm in arms {
            let next = self.label("nextarm");
            self.pat_test(&arm.pat, &sp, &sty, &next);
            self.push_scope(true);
            self.pat_bind(&arm.pat, &sp, &sty);
            if let Some(g) = &arm.guard {
                self.push_scope(false);
                let c = self.borrow(g);
                self.pop_scope();
                let pass = self.label("guard");
                let fail = self.label("guardfail");
                self.term(&format!("br i1 {}, label %{pass}, label %{fail}", c.repr));
                self.start(&fail);
                // Leaving the arm: drop what its bindings hold.
                let depth = self.f.scopes.len() - 1;
                self.cleanup_from(depth);
                self.term(&format!("br label %{next}"));
                self.start(&pass);
            }
            self.push_scope(false);
            let v = if has_value { Some(self.owned(&arm.body)) } else {
                let (v, owned) = self.value(&arm.body);
                if owned && !self.f.terminated {
                    let bty = self.sub(&arm.body.ty);
                    self.drop_value(&v, &bty);
                }
                None
            };
            self.pop_scope();
            if !self.f.terminated {
                if let (Some(r), Some(v)) = (&result, v) {
                    self.inst(&format!("store {}, ptr {r}", v.op()));
                }
            }
            self.pop_scope();
            if !self.f.terminated {
                self.term(&format!("br label %{end}"));
            }
            self.start(&next);
        }
        // Exhaustiveness is checked, but guards can make every arm miss.
        self.trap("no `match` arm matched", span);
        self.start(&end);
        self.pop_scope();
        if *ty == Ty::Never {
            self.term("unreachable");
            return (V::unit(), false);
        }
        match result {
            Some(r) => (self.load(&lt, &r), true),
            None => (V::unit(), false),
        }
    }

    /// Branches to `fail` unless the value at `p` matches `pat`.
    fn pat_test(&mut self, pat: &TPat, p: &str, ty: &Ty, fail: &str) {
        let check = |g: &mut Self, ok: String| {
            let next = g.label("match");
            g.term(&format!("br i1 {ok}, label %{next}, label %{fail}"));
            g.start(&next);
        };
        match pat {
            TPat::Wild | TPat::Bind(_) => {}
            TPat::Int(v) => {
                let lt = self.lty(ty);
                let x = self.load(&lt, p);
                let t = self.tmp();
                let repr = if *v > i64::MAX as i128 { (*v as u64 as i64).to_string() } else { v.to_string() };
                self.inst(&format!("{t} = icmp eq {lt} {}, {repr}", x.repr));
                check(self, t);
            }
            TPat::Float(v) => {
                let lt = self.lty(ty);
                let x = self.load(&lt, p);
                let t = self.tmp();
                self.inst(&format!("{t} = fcmp oeq {lt} {}, {}", x.repr, Self::float_const(*v, ty)));
                check(self, t);
            }
            TPat::Bool(b) => {
                let x = self.load("i1", p);
                let t = self.tmp();
                self.inst(&format!("{t} = icmp eq i1 {}, {b}", x.repr));
                check(self, t);
            }
            TPat::Str(bytes) => {
                let c = self.str_const(bytes);
                let cp = self.spill(&c);
                let r = self.tmp();
                self.inst(&format!("{r} = call i32 @ovt_str_eq(ptr {p}, ptr {cp})"));
                let t = self.tmp();
                self.inst(&format!("{t} = icmp ne i32 {r}, 0"));
                check(self, t);
            }
            TPat::None => {
                let lt = self.lty(ty);
                let tp = self.gep(&lt, p, &[0, 0]);
                let tag = self.load("i1", &tp);
                let t = self.tmp();
                self.inst(&format!("{t} = xor i1 {}, true", tag.repr));
                check(self, t);
            }
            TPat::Range { lo, hi, inclusive } => {
                let lt = self.lty(ty);
                let signed = int_ty(ty).is_none_or(|k| k.signed());
                let x = self.load(&lt, p);
                let a = self.tmp();
                self.inst(&format!("{a} = icmp {} {lt} {}, {lo}", if signed { "sge" } else { "uge" }, x.repr));
                let b = self.tmp();
                let cc = match (inclusive, signed) {
                    (true, true) => "sle",
                    (true, false) => "ule",
                    (false, true) => "slt",
                    (false, false) => "ult",
                };
                self.inst(&format!("{b} = icmp {cc} {lt} {}, {hi}", x.repr));
                let t = self.tmp();
                self.inst(&format!("{t} = and i1 {a}, {b}"));
                check(self, t);
            }
            TPat::Tuple(items) => {
                let Ty::Tuple(ts) = ty else { unreachable!() };
                let ts = ts.clone();
                let lt = self.lty(ty);
                for (i, (sub, t)) in items.iter().zip(&ts).enumerate() {
                    let fp = self.gep(&lt, p, &[0, i]);
                    self.pat_test(sub, &fp, t, fail);
                }
            }
            TPat::Array { before, rest, after } => {
                let Ty::Array(e) = ty else { unreachable!() };
                let e = (**e).clone();
                let a = self.load("%ovt.arr", p);
                let len = self.extract(&a, 2, "i64");
                let fixed = before.len() + after.len();
                let t = self.tmp();
                if rest.is_some() {
                    self.inst(&format!("{t} = icmp sge i64 {}, {fixed}", len.repr));
                } else {
                    self.inst(&format!("{t} = icmp eq i64 {}, {fixed}", len.repr));
                }
                check(self, t);
                for (i, sub) in before.iter().enumerate() {
                    let ep = self.elem_ptr(p, &i.to_string(), &e);
                    self.pat_test(sub, &ep, &e, fail);
                }
                for (j, sub) in after.iter().enumerate() {
                    let idx = self.tmp();
                    self.inst(&format!("{idx} = sub i64 {}, {}", len.repr, after.len() - j));
                    let ep = self.elem_ptr(p, &idx, &e);
                    self.pat_test(sub, &ep, &e, fail);
                }
            }
            TPat::Variant { variant, fields } => {
                let Ty::Adt(id, targs) = ty else { unreachable!() };
                let (id, targs) = (*id, targs.clone());
                if self.is_payloadless_enum(id) {
                    let x = self.load("i32", p);
                    let t = self.tmp();
                    self.inst(&format!("{t} = icmp eq i32 {}, {variant}", x.repr));
                    check(self, t);
                    return;
                }
                let lt = self.lty(ty);
                let tp = self.gep(&lt, p, &[0, 0]);
                let tag = self.load("i32", &tp);
                let t = self.tmp();
                self.inst(&format!("{t} = icmp eq i32 {}, {variant}", tag.repr));
                check(self, t);
                let payload = self.gep(&lt, p, &[0, 1]);
                let vlt = self.variant_lty(id, &targs, *variant as usize);
                let defs: Vec<(bool, Ty)> = self.p.adts[id].variants()[*variant as usize].fields.iter().map(|f| (f.boxed, f.ty.subst(&targs, &[]))).collect();
                for (fi, sub) in fields {
                    let (boxed, fty) = defs[*fi as usize].clone();
                    let mut fp = self.gep(&vlt, &payload, &[0, *fi as usize]);
                    if boxed {
                        let bx = self.load("ptr", &fp);
                        let vp = self.tmp();
                        self.inst(&format!("{vp} = getelementptr inbounds i8, ptr {}, i64 8", bx.repr));
                        fp = vp;
                    }
                    self.pat_test(sub, &fp, &fty, fail);
                }
            }
            TPat::Or(alts) => {
                let matched = self.label("ormatch");
                for alt in alts {
                    let next = self.label("oralt");
                    self.pat_test(alt, p, ty, &next);
                    self.term(&format!("br label %{matched}"));
                    self.start(&next);
                }
                self.term(&format!("br label %{fail}"));
                self.start(&matched);
            }
        }
    }

    /// Stores the values a matched pattern binds into their locals.
    fn pat_bind(&mut self, pat: &TPat, p: &str, ty: &Ty) {
        match pat {
            TPat::Bind(id) => {
                let lt = self.lty(ty);
                let v = self.load(&lt, p);
                let slot = self.f.slots[*id].clone();
                self.inst(&format!("store {}, ptr {slot}", v.op()));
            }
            TPat::Tuple(items) => {
                let Ty::Tuple(ts) = ty else { unreachable!() };
                let ts = ts.clone();
                let lt = self.lty(ty);
                for (i, (sub, t)) in items.iter().zip(&ts).enumerate() {
                    if pat_binds(sub) {
                        let fp = self.gep(&lt, p, &[0, i]);
                        self.pat_bind(sub, &fp, t);
                    }
                }
            }
            TPat::Array { before, rest, after } => {
                let Ty::Array(e) = ty else { unreachable!() };
                let e = (**e).clone();
                let a = self.load("%ovt.arr", p);
                let len = self.extract(&a, 2, "i64");
                for (i, sub) in before.iter().enumerate() {
                    if pat_binds(sub) {
                        let ep = self.elem_ptr(p, &i.to_string(), &e);
                        self.pat_bind(sub, &ep, &e);
                    }
                }
                for (j, sub) in after.iter().enumerate() {
                    if pat_binds(sub) {
                        let idx = self.tmp();
                        self.inst(&format!("{idx} = sub i64 {}, {}", len.repr, after.len() - j));
                        let ep = self.elem_ptr(p, &idx, &e);
                        self.pat_bind(sub, &ep, &e);
                    }
                }
                if let Some(Some(id)) = rest {
                    // The middle, as a new view sharing the buffer.
                    let hi = self.tmp();
                    self.inst(&format!("{hi} = sub i64 {}, {}", len.repr, after.len()));
                    let out = self.f.slots[*id].clone();
                    let loc = self.loc_args(Span::default());
                    self.inst(&format!("call void @ovt_arr_slice(ptr {out}, ptr {p}, i64 {}, i64 {hi}, {loc})", before.len()));
                    self.add_local_drop(&out, ty);
                }
            }
            TPat::Variant { variant, fields } => {
                let Ty::Adt(id, targs) = ty else { unreachable!() };
                let (id, targs) = (*id, targs.clone());
                if self.is_payloadless_enum(id) {
                    return;
                }
                let lt = self.lty(ty);
                let payload = self.gep(&lt, p, &[0, 1]);
                let vlt = self.variant_lty(id, &targs, *variant as usize);
                let defs: Vec<(bool, Ty)> = self.p.adts[id].variants()[*variant as usize].fields.iter().map(|f| (f.boxed, f.ty.subst(&targs, &[]))).collect();
                for (fi, sub) in fields {
                    if !pat_binds(sub) {
                        continue;
                    }
                    let (boxed, fty) = defs[*fi as usize].clone();
                    let mut fp = self.gep(&vlt, &payload, &[0, *fi as usize]);
                    if boxed {
                        let bx = self.load("ptr", &fp);
                        let vp = self.tmp();
                        self.inst(&format!("{vp} = getelementptr inbounds i8, ptr {}, i64 8", bx.repr));
                        fp = vp;
                    }
                    self.pat_bind(sub, &fp, &fty);
                }
            }
            _ => {}
        }
    }
}

fn pat_binds(p: &TPat) -> bool {
    match p {
        TPat::Bind(_) => true,
        TPat::Tuple(items) => items.iter().any(pat_binds),
        TPat::Array { before, rest, after } => before.iter().any(pat_binds) || after.iter().any(pat_binds) || matches!(rest, Some(Some(_))),
        TPat::Variant { fields, .. } => fields.iter().any(|(_, p)| pat_binds(p)),
        _ => false,
    }
}
