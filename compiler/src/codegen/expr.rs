//! Expressions.
//!
//! `value` produces a value and says whether it's owned (the caller now
//! holds a reference) or borrowed (valid until the end of the statement).
//! `owned` and `borrow` adapt: `owned` copies a borrowed value, and `borrow`
//! registers an owned temporary to be dropped at the end of the statement.

use super::*;
use crate::ast::{BinOp, Mode, UnOp};
use crate::source::Span;

impl<'p> Gen<'p> {
    pub fn owned(&mut self, e: &TExpr) -> V {
        let (v, owned) = self.value(e);
        if !owned && !self.f.terminated {
            let ty = self.sub(&e.ty);
            self.dup_value(&v, &ty);
        }
        v
    }

    pub fn borrow(&mut self, e: &TExpr) -> V {
        let (v, owned) = self.value(e);
        if owned && !self.f.terminated {
            let ty = self.sub(&e.ty);
            self.keep(&v, &ty);
        }
        v
    }

    fn never(&mut self) -> (V, bool) {
        (V::unit(), false)
    }

    pub fn float_const(v: f64, ty: &Ty) -> String {
        match ty {
            Ty::Float(FloatTy::F32) => format!("0x{:016X}", (v as f32 as f64).to_bits()),
            _ => format!("0x{:016X}", v.to_bits()),
        }
    }

    pub fn value(&mut self, e: &TExpr) -> (V, bool) {
        let ty = self.sub(&e.ty);
        let span = e.span;
        match &e.kind {
            TK::Int(v) => {
                let lt = self.lty(&ty);
                if let Ty::Float(_) = ty {
                    return (V::new(lt, Self::float_const(*v as f64, &ty)), false);
                }
                // Unsigned 64-bit values above i64::MAX are written as their two's-complement form.
                let repr = if *v > i64::MAX as i128 { (*v as u64 as i64).to_string() } else { v.to_string() };
                (V::new(lt, repr), false)
            }
            TK::Float(f) => {
                let lt = self.lty(&ty);
                (V::new(lt, Self::float_const(*f, &ty)), false)
            }
            TK::Bool(b) => (V::new("i1", if *b { "true" } else { "false" }), false),
            TK::Str(bytes) => (self.str_const(bytes), false),
            TK::Unit => (V::unit(), false),
            TK::None => {
                let lt = self.lty(&ty);
                (V::new(lt, "zeroinitializer"), false)
            }
            TK::Local(id) => {
                let slot = self.f.slots[*id].clone();
                let lty = self.f.local_tys[*id].clone();
                let lt = self.lty(&lty);
                if self.f.local_kinds[*id] == LocalKind::Ref {
                    let p = self.load("ptr", &slot);
                    (self.load(&lt, &p.repr), false)
                } else {
                    (self.load(&lt, &slot), false)
                }
            }
            TK::Call { .. } | TK::CallValue { .. } => {
                let (r, failable, rty) = self.call_raw(e);
                if failable {
                    // Failure passing through an effect parameter (`fn(T) -> U ! E`): pass it on.
                    let v = self.propagate(&r, &rty, span);
                    return (v, true);
                }
                (r, true)
            }
            TK::FnValue { f, targs, eargs } => {
                let targs: Vec<Ty> = targs.iter().map(|t| self.sub(t)).collect();
                let eargs: Vec<Eff> = eargs.iter().map(|x| x.subst(&self.f.eargs)).collect();
                let failable = matches!(&ty, Ty::Fn(ft) if ft.eff.fail);
                let th = self.thunk(*f, &targs, &eargs, failable);
                (V::new("%ovt.fn", format!("{{ ptr {th}, ptr null }}")), false)
            }
            TK::Closure { id } => (self.make_closure(*id, &ty), true),
            TK::Struct { adt, fields, .. } => {
                let lt = self.lty(&ty);
                let Ty::Adt(_, targs) = &ty else { unreachable!() };
                let targs = targs.clone();
                let mut agg = V::new(lt.clone(), "undef");
                let defs: Vec<(bool, Ty)> = self.p.adts[*adt].fields().iter().map(|f| (f.boxed, f.ty.subst(&targs, &[]))).collect();
                if defs.is_empty() {
                    return (V::new(lt, "zeroinitializer"), false);
                }
                for (i, (fe, (boxed, fty))) in fields.iter().zip(defs).enumerate() {
                    let v = self.owned(fe);
                    let v = if boxed { self.make_box(&v, &fty) } else { v };
                    agg = self.insert(&agg, &v, i);
                }
                (agg, true)
            }
            TK::Variant { adt, variant, fields, .. } => {
                let Ty::Adt(_, targs) = &ty else { unreachable!() };
                let targs = targs.clone();
                if self.is_payloadless_enum(*adt) {
                    return (V::new("i32", variant.to_string()), false);
                }
                let lt = self.lty(&ty);
                let slot = self.alloca(&lt);
                self.inst(&format!("store {lt} zeroinitializer, ptr {slot}"));
                let tag = self.gep(&lt, &slot, &[0, 0]);
                self.inst(&format!("store i32 {variant}, ptr {tag}"));
                let vlt = self.variant_lty(*adt, &targs, *variant as usize);
                let payload = self.gep(&lt, &slot, &[0, 1]);
                let defs: Vec<(bool, Ty)> = self.p.adts[*adt].variants()[*variant as usize].fields.iter().map(|f| (f.boxed, f.ty.subst(&targs, &[]))).collect();
                for (i, (fe, (boxed, fty))) in fields.iter().zip(defs).enumerate() {
                    let v = self.owned(fe);
                    let v = if boxed { self.make_box(&v, &fty) } else { v };
                    let fp = self.gep(&vlt, &payload, &[0, i]);
                    self.inst(&format!("store {}, ptr {fp}", v.op()));
                }
                (self.load(&lt, &slot), true)
            }
            TK::Tuple(items) => {
                let lt = self.lty(&ty);
                let mut agg = V::new(lt, "undef");
                for (i, it) in items.iter().enumerate() {
                    let v = self.owned(it);
                    agg = self.insert(&agg, &v, i);
                }
                (agg, true)
            }
            TK::Array(items) => {
                if items.is_empty() {
                    return (V::new("%ovt.arr", "zeroinitializer"), false);
                }
                let Ty::Array(et) = &ty else { unreachable!() };
                let et = (**et).clone();
                let slot = self.alloca("%ovt.arr");
                self.inst(&format!("store %ovt.arr zeroinitializer, ptr {slot}"));
                let size = self.size_const(&et);
                let elt = self.lty(&et);
                for it in items {
                    let v = self.owned(it);
                    let p = self.tmp();
                    self.inst(&format!("{p} = call ptr @ovt_arr_push(ptr {slot}, i64 {size}, ptr null, ptr null)"));
                    self.inst(&format!("store {elt} {}, ptr {p}", v.repr));
                }
                (self.load("%ovt.arr", &slot), true)
            }
            TK::Field { base, index } => {
                let bty = self.sub(&base.ty);
                let b = self.borrow(base);
                let v = self.field_of(&b, &bty, *index as usize);
                (v, false)
            }
            TK::Index { base, index } => {
                let bty = self.sub(&base.ty);
                let b = self.borrow(base);
                let i = self.borrow(index);
                let bp = self.spill(&b);
                let len = self.extract(&b, 2, "i64");
                self.bounds_check(&i.repr, &len.repr, span);
                let et = match &bty {
                    Ty::Array(et) => (**et).clone(),
                    _ => Ty::U8,
                };
                let p = self.elem_ptr(&bp, &i.repr, &et);
                let lt = self.lty(&et);
                (self.load(&lt, &p), false)
            }
            TK::Slice { base, lo, hi } => {
                let bty = self.sub(&base.ty);
                let b = self.borrow(base);
                let bp = self.spill(&b);
                let lo = match lo {
                    Some(l) => self.borrow(l).repr,
                    None => "0".into(),
                };
                let hi = match hi {
                    Some(h) => self.borrow(h).repr,
                    None => self.extract(&b, 2, "i64").repr,
                };
                let out = self.alloca("%ovt.arr");
                let loc = self.loc_args(span);
                let f = if bty == Ty::Str { "ovt_str_slice" } else { "ovt_arr_slice" };
                self.inst(&format!("call void @{f}(ptr {out}, ptr {bp}, i64 {lo}, i64 {hi}, {loc})"));
                (self.load("%ovt.arr", &out), true)
            }
            TK::Some(x) => {
                let lt = self.lty(&ty);
                let v = self.owned(x);
                let agg = V::new(lt, "undef");
                let agg = self.insert(&agg, &V::new("i1", "true"), 0);
                (self.insert(&agg, &v, 1), true)
            }
            TK::Unary(op, x) => {
                let v = self.borrow(x);
                let t = self.tmp();
                match (op, &ty) {
                    (UnOp::Not, _) => self.inst(&format!("{t} = xor i1 {}, true", v.repr)),
                    (UnOp::Neg, Ty::Float(_)) => self.inst(&format!("{t} = fneg {}", v.op())),
                    (UnOp::Neg, _) => {
                        let zero = V::new(v.ty.clone(), "0");
                        return (self.arith(BinOp::Sub, &ty, &zero, &v, span), false);
                    }
                }
                (V::new(v.ty, t), false)
            }
            TK::Binary(op, l, r) => self.binary(*op, l, r, &ty, span),
            TK::Convert(x) => {
                let from = self.sub(&x.ty);
                let v = self.borrow(x);
                (self.convert(&v, &from, &ty, span), false)
            }
            TK::Interp(parts) => {
                let sb = self.alloca("%ovt.arr");
                self.inst(&format!("store %ovt.arr zeroinitializer, ptr {sb}"));
                for p in parts {
                    if let TK::Str(bytes) = &p.kind {
                        let c = self.cstr(bytes);
                        self.inst(&format!("call void @ovt_sb_cstr(ptr {sb}, ptr {c}, i64 {})", bytes.len()));
                        continue;
                    }
                    let pty = self.sub(&p.ty);
                    let v = self.borrow(p);
                    let vp = self.spill(&v);
                    if pty == Ty::Str {
                        self.inst(&format!("call void @ovt_str_append(ptr {sb}, ptr {vp})"));
                    } else {
                        let h = self.helper(Helper::Show, &pty);
                        self.inst(&format!("call void {h}(ptr {vp}, ptr {sb})"));
                    }
                }
                (self.load("%ovt.arr", &sb), true)
            }
            TK::If { cond, then, els } => {
                let c = {
                    self.push_scope(false);
                    let c = self.borrow(cond);
                    self.pop_scope();
                    c
                };
                self.branches(&ty, &c.repr, then, els.as_ref())
            }
            TK::IfLet { local, value, then, els } => {
                self.push_scope(false);
                let oty = self.sub(&value.ty);
                let v = self.owned(value);
                let slot = self.spill(&v);
                self.add_drop(&slot, &oty);
                let is_some = self.extract(&v, 0, "i1");
                let lty = self.f.local_tys[*local].clone();
                let lt = self.lty(&lty);
                let inner = self.extract(&v, 1, &lt);
                let lslot = self.f.slots[*local].clone();
                self.inst(&format!("store {}, ptr {lslot}", inner.op()));
                let r = self.branches(&ty, &is_some.repr, then, els.as_ref());
                self.pop_scope();
                r
            }
            TK::Match { scrut, arms } => self.match_expr(&ty, scrut, arms, span),
            TK::Block(b) => match self.block_value(b) {
                Some(v) => (v, true),
                None => self.never(),
            },
            TK::Return(v) => {
                let val = v.as_ref().map(|v| self.owned(v));
                if !self.f.terminated {
                    let rt = self.f.ret.clone();
                    let val = if rt == Ty::Unit && !self.f.failable { None } else { val };
                    self.emit_return(val);
                }
                self.never()
            }
            TK::Break | TK::Continue => {
                let l = self.f.loops.last().expect("checked: inside a loop");
                let (target, depth) = if matches!(e.kind, TK::Break) { (l.brk.clone(), l.brk_depth) } else { (l.cont.clone(), l.cont_depth) };
                self.cleanup_from(depth);
                self.term(&format!("br label %{target}"));
                self.never()
            }
            TK::Try(x) => {
                let (r, failable, rty) = self.call_raw(x);
                if !failable {
                    return (r, true);
                }
                (self.propagate(&r, &rty, span), true)
            }
            TK::ElseOpt { value, alt } => {
                let oty = self.sub(&value.ty);
                let v = self.owned(value);
                let is_some = self.extract(&v, 0, "i1");
                let lt = self.lty(&ty);
                let result = self.alloca(&lt);
                let yes = self.label("some");
                let no = self.label("none");
                let end = self.label("end");
                self.term(&format!("br i1 {}, label %{yes}, label %{no}", is_some.repr));
                self.start(&yes);
                // The value moves out of the optional, so the optional isn't dropped.
                let inner = self.extract(&v, 1, &lt);
                self.inst(&format!("store {}, ptr {result}", inner.op()));
                self.term(&format!("br label %{end}"));
                self.start(&no);
                let _ = oty;
                self.push_scope(false);
                let a = self.owned(alt);
                self.pop_scope();
                if !self.f.terminated {
                    self.inst(&format!("store {}, ptr {result}", a.op()));
                    self.term(&format!("br label %{end}"));
                }
                self.start(&end);
                (self.load(&lt, &result), true)
            }
            TK::ElseFail { value, alt } => {
                let (r, _, rty) = self.call_raw(value);
                let lt = self.lty(&ty);
                let result = self.alloca(&lt);
                let failed = self.extract(&r, 0, "i1");
                let bad = self.label("failed");
                let good = self.label("ok");
                let end = self.label("end");
                self.term(&format!("br i1 {}, label %{bad}, label %{good}", failed.repr));
                self.start(&good);
                let v = self.call_value_as(&r, &rty, &ty);
                self.inst(&format!("store {}, ptr {result}", v.op()));
                self.term(&format!("br label %{end}"));
                self.start(&bad);
                let et = self.err_lty();
                let err = self.extract(&r, 2, &et);
                let ety = self.err_ty();
                self.drop_value(&err, &ety);
                self.push_scope(false);
                let a = self.owned(alt);
                self.pop_scope();
                if !self.f.terminated {
                    self.inst(&format!("store {}, ptr {result}", a.op()));
                    self.term(&format!("br label %{end}"));
                }
                self.start(&end);
                (self.load(&lt, &result), true)
            }
            TK::Catch { value, local, body } => {
                let (r, _, rty) = self.call_raw(value);
                let lt = self.lty(&ty);
                let result = self.alloca(&lt);
                let failed = self.extract(&r, 0, "i1");
                let bad = self.label("failed");
                let good = self.label("ok");
                let end = self.label("end");
                self.term(&format!("br i1 {}, label %{bad}, label %{good}", failed.repr));
                self.start(&good);
                let v = self.call_value_as(&r, &rty, &ty);
                self.inst(&format!("store {}, ptr {result}", v.op()));
                self.term(&format!("br label %{end}"));
                self.start(&bad);
                let et = self.err_lty();
                let err = self.extract(&r, 2, &et);
                self.push_scope(true);
                let eslot = self.f.slots[*local].clone();
                self.inst(&format!("store {}, ptr {eslot}", err.op()));
                let ety = self.err_ty();
                self.add_local_drop(&eslot, &ety);
                let bv = self.block_value(body);
                self.pop_scope();
                if let Some(bv) = bv {
                    if !self.f.terminated {
                        self.inst(&format!("store {}, ptr {result}", bv.op()));
                        self.term(&format!("br label %{end}"));
                    }
                }
                self.start(&end);
                (self.load(&lt, &result), true)
            }
            TK::Fail { kind, msg } => {
                let k = self.borrow(kind);
                let m = self.owned(msg);
                let et = self.err_lty();
                let err = V::new(et, "undef");
                let err = self.insert(&err, &k, 0);
                let err = self.insert(&err, &m, 1);
                if !self.f.terminated {
                    self.emit_fail(err);
                }
                self.never()
            }
            TK::Trap(msg) => {
                let m = self.borrow(msg);
                let p = self.spill(&m);
                let loc = self.loc_args(span);
                self.inst(&format!("call void @ovt_trap_str(ptr {p}, {loc})"));
                self.term("unreachable");
                self.never()
            }
            TK::Todo => {
                self.trap("reached `todo()`", span);
                self.never()
            }
            TK::Assert { cond, msg, text } => {
                self.assert(cond, msg.as_deref(), text, span);
                (V::unit(), false)
            }
            TK::Print { value, stderr } => {
                let vty = self.sub(&value.ty);
                let v = self.borrow(value);
                let to = if *stderr { 1 } else { 0 };
                if vty == Ty::Str {
                    let p = self.spill(&v);
                    self.inst(&format!("call void @ovt_print(ptr {p}, i32 {to})"));
                } else {
                    let sb = self.show_to_str(&v, &vty);
                    self.inst(&format!("call void @ovt_print(ptr {sb}, i32 {to})"));
                    self.drop_ptr(&sb, &Ty::Str);
                }
                (V::unit(), false)
            }
            TK::Dbg { value, text } => {
                let vty = self.sub(&value.ty);
                let v = self.owned(value);
                let sb = self.show_to_str(&v, &vty);
                let (line, col) = self.loc(span);
                let file = self.file_cstr(self.f.file);
                let t = self.cstr(text.as_bytes());
                self.inst(&format!("call void @ovt_dbg(ptr {sb}, ptr {file}, i32 {line}, i32 {col}, ptr {t})"));
                self.drop_ptr(&sb, &Ty::Str);
                (v, true)
            }
            TK::Hash(x) => {
                let xty = self.sub(&x.ty);
                let v = self.borrow(x);
                let p = self.spill(&v);
                let h = self.helper(Helper::Hash, &xty);
                let t = self.tmp();
                self.inst(&format!("{t} = call i64 {h}(ptr {p})"));
                (V::new("i64", t), false)
            }
            TK::ExpectEq { left, right } => {
                let lty = self.sub(&left.ty);
                let l = self.borrow(left);
                let r = self.borrow(right);
                let eq = self.equal(&l, &r, &lty);
                let bad = self.label("mismatch");
                let ok = self.label("ok");
                self.term(&format!("br i1 {}, label %{ok}, label %{bad}", eq.repr));
                self.start(&bad);
                let ls = self.show_to_str(&l, &lty);
                let rs = self.show_to_str(&r, &lty);
                self.inst(&format!("call void @ovt_expect_failed(ptr {ls}, ptr {rs})"));
                self.inst("call void @ovt_os_exit(i64 1)");
                self.term("unreachable");
                self.start(&ok);
                (V::unit(), false)
            }
            TK::ExpectFail { value, kind, .. } => {
                let (r, _, _) = self.call_raw(value);
                let failed = self.extract(&r, 0, "i1");
                let no = self.label("nofail");
                let yes = self.label("failed");
                self.term(&format!("br i1 {}, label %{yes}, label %{no}", failed.repr));
                self.start(&no);
                let msg = "expected a failure, but it succeeded".to_string();
                let c = self.cstr(msg.as_bytes());
                self.inst(&format!("call void @ovt_note(ptr {c})"));
                self.inst("call void @ovt_os_exit(i64 1)");
                self.term("unreachable");
                self.start(&yes);
                if let Some(k) = kind {
                    let et = self.err_lty();
                    let err = self.extract(&r, 2, &et);
                    let got = self.extract(&err, 0, "i32");
                    let want = self.borrow(k);
                    let same = self.tmp();
                    self.inst(&format!("{same} = icmp eq i32 {}, {}", got.repr, want.repr));
                    let bad = self.label("wrongkind");
                    let ok = self.label("ok");
                    self.term(&format!("br i1 {same}, label %{ok}, label %{bad}"));
                    self.start(&bad);
                    let kty = Ty::Adt(self.p.known.err_kind, Vec::new());
                    let shown = self.show_to_str(&got, &kty);
                    let msg = "failed with a different kind: ".to_string();
                    let c = self.cstr(msg.as_bytes());
                    self.inst(&format!("call void @ovt_note_str(ptr {c}, ptr {shown})"));
                    self.inst("call void @ovt_os_exit(i64 1)");
                    self.term("unreachable");
                    self.start(&ok);
                }
                (V::unit(), false)
            }
            TK::ExpectTrue { cond, .. } => {
                let c = self.borrow(cond);
                let bad = self.label("false");
                let ok = self.label("ok");
                self.term(&format!("br i1 {}, label %{ok}, label %{bad}", c.repr));
                self.start(&bad);
                let msg = "this is false".to_string();
                let m = self.cstr(msg.as_bytes());
                self.inst(&format!("call void @ovt_note(ptr {m})"));
                self.inst("call void @ovt_os_exit(i64 1)");
                self.term("unreachable");
                self.start(&ok);
                (V::unit(), false)
            }
        }
    }

    /// `if`: both branches store an owned value into a result slot.
    fn branches(&mut self, ty: &Ty, cond: &str, then: &TBlock, els: Option<&TBlock>) -> (V, bool) {
        let has_value = !matches!(ty, Ty::Unit | Ty::Never);
        let lt = self.lty(ty);
        let result = if has_value { Some(self.alloca(&lt)) } else { None };
        let tl = self.label("then");
        let el = self.label("else");
        let end = self.label("endif");
        self.term(&format!("br i1 {cond}, label %{tl}, label %{}", if els.is_some() { &el } else { &end }));
        self.start(&tl);
        let v = self.block_value(then);
        self.finish_branch(v, result.as_deref(), ty, &end);
        if let Some(els) = els {
            self.start(&el);
            let v = self.block_value(els);
            self.finish_branch(v, result.as_deref(), ty, &end);
        }
        self.start(&end);
        if *ty == Ty::Never {
            self.term("unreachable");
            return self.never();
        }
        match result {
            Some(r) => (self.load(&lt, &r), true),
            None => (V::unit(), false),
        }
    }

    fn finish_branch(&mut self, v: Option<V>, result: Option<&str>, ty: &Ty, end: &str) {
        if self.f.terminated {
            return;
        }
        match (v, result) {
            (Some(v), Some(r)) => self.inst(&format!("store {}, ptr {r}", v.op())),
            (Some(v), None) => {
                // A statement `if` whose branch ended in a value: drop it.
                let _ = ty;
                if v.ty != "{}" {
                    self.drop_unknown(&v);
                }
            }
            _ => {}
        }
        self.term(&format!("br label %{end}"));
    }

    /// Drops a value whose Overt type isn't at hand; only used for unit-like leftovers.
    fn drop_unknown(&mut self, _v: &V) {}

    /// A field of a struct or tuple value.
    pub fn field_of(&mut self, b: &V, bty: &Ty, index: usize) -> V {
        match bty {
            Ty::Tuple(ts) => {
                let lt = self.lty(&ts[index]);
                self.extract(b, index, &lt)
            }
            Ty::Adt(id, targs) => {
                let f = &self.p.adts[*id].fields()[index];
                let (boxed, fty) = (f.boxed, f.ty.subst(targs, &[]));
                let lt = self.lty(&fty);
                if boxed {
                    let bp = self.extract(b, index, "ptr");
                    let vp = self.tmp();
                    self.inst(&format!("{vp} = getelementptr inbounds i8, ptr {}, i64 8", bp.repr));
                    self.load(&lt, &vp)
                } else {
                    self.extract(b, index, &lt)
                }
            }
            _ => unreachable!("field of a non-struct"),
        }
    }

    /// Puts an owned value in a new box: `{ i64 count, T value }`.
    pub fn make_box(&mut self, v: &V, ty: &Ty) -> V {
        let size = self.size_const(ty);
        let total = self.tmp();
        self.inst(&format!("{total} = add i64 {size}, 8"));
        let p = self.tmp();
        self.inst(&format!("{p} = call ptr @ovt_alloc(i64 {total})"));
        self.inst(&format!("store i64 1, ptr {p}"));
        let vp = self.tmp();
        self.inst(&format!("{vp} = getelementptr inbounds i8, ptr {p}, i64 8"));
        self.inst(&format!("store {}, ptr {vp}", v.op()));
        V::new("ptr", p)
    }

    pub fn bounds_check(&mut self, i: &str, len: &str, span: Span) {
        let bad = self.tmp();
        self.inst(&format!("{bad} = icmp uge i64 {i}, {len}"));
        let ok = self.label("inbounds");
        let fail = self.label("outofbounds");
        self.term(&format!("br i1 {bad}, label %{fail}, label %{ok}"));
        self.start(&fail);
        let loc = self.loc_args(span);
        self.inst(&format!("call void @ovt_trap_index(i64 {i}, i64 {len}, {loc})"));
        self.term("unreachable");
        self.start(&ok);
    }

    /// Shows a value into a new string, returning a pointer to its slot (owned).
    pub fn show_to_str(&mut self, v: &V, ty: &Ty) -> String {
        let sb = self.alloca("%ovt.arr");
        self.inst(&format!("store %ovt.arr zeroinitializer, ptr {sb}"));
        let vp = self.spill(v);
        let h = self.helper(Helper::Show, ty);
        self.inst(&format!("call void {h}(ptr {vp}, ptr {sb})"));
        sb
    }

    fn assert(&mut self, cond: &TExpr, msg: Option<&TExpr>, text: &str, span: Span) {
        // `assert(a == b)`: on failure, show both sides.
        if let (TK::Binary(BinOp::Eq, l, r), None) = (&cond.kind, msg) {
            if !matches!(r.kind, TK::None) {
                let lty = self.sub(&l.ty);
                let lv = self.borrow(l);
                let rv = self.borrow(r);
                let eq = self.equal(&lv, &rv, &lty);
                let bad = self.label("assert");
                let ok = self.label("ok");
                self.term(&format!("br i1 {}, label %{ok}, label %{bad}", eq.repr));
                self.start(&bad);
                let ls = self.show_to_str(&lv, &lty);
                let rs = self.show_to_str(&rv, &lty);
                let sb = self.alloca("%ovt.arr");
                self.inst(&format!("store %ovt.arr zeroinitializer, ptr {sb}"));
                let head = format!("assertion failed: {text}\n  left:  ");
                let c = self.cstr(head.as_bytes());
                self.inst(&format!("call void @ovt_sb_cstr(ptr {sb}, ptr {c}, i64 {})", head.len()));
                self.inst(&format!("call void @ovt_str_append(ptr {sb}, ptr {ls})"));
                let mid = "\n  right: ";
                let c = self.cstr(mid.as_bytes());
                self.inst(&format!("call void @ovt_sb_cstr(ptr {sb}, ptr {c}, i64 {})", mid.len()));
                self.inst(&format!("call void @ovt_str_append(ptr {sb}, ptr {rs})"));
                let loc = self.loc_args(span);
                self.inst(&format!("call void @ovt_trap_str(ptr {sb}, {loc})"));
                self.term("unreachable");
                self.start(&ok);
                return;
            }
        }
        let c = self.borrow(cond);
        let bad = self.label("assert");
        let ok = self.label("ok");
        self.term(&format!("br i1 {}, label %{ok}, label %{bad}", c.repr));
        self.start(&bad);
        match msg {
            Some(m) => {
                let mv = self.borrow(m);
                let p = self.spill(&mv);
                let loc = self.loc_args(span);
                self.inst(&format!("call void @ovt_trap_str(ptr {p}, {loc})"));
                self.term("unreachable");
            }
            None => self.trap(&format!("assertion failed: {text}"), span),
        }
        self.start(&ok);
    }

    // ---- calls ----

    /// Evaluates the arguments of a call.
    fn args(&mut self, args: &[TArg]) -> Vec<String> {
        let mut out = Vec::new();
        for a in args {
            match a.mode {
                Mode::Inout => {
                    let p = self.expr_place_ptr(&a.expr, true);
                    out.push(format!("ptr {p}"));
                }
                Mode::Sink => {
                    let v = self.owned(&a.expr);
                    out.push(v.op());
                }
                Mode::Read if a.copy => {
                    // It reads a variable another argument changes: copy it first.
                    let v = self.owned(&a.expr);
                    let ty = self.sub(&a.expr.ty);
                    self.keep(&v, &ty);
                    out.push(v.op());
                }
                Mode::Read => {
                    let v = self.borrow(&a.expr);
                    out.push(v.op());
                }
            }
        }
        out
    }

    /// Makes a call and returns what the function returns: for failable
    /// functions, the `{ i1, T, Err }` result. Also returns whether it's
    /// failable, and the value type.
    pub fn call_raw(&mut self, e: &TExpr) -> (V, bool, Ty) {
        let rty = self.sub(&e.ty);
        match &e.kind {
            TK::Call { f, targs, eargs, args } => {
                let targs: Vec<Ty> = targs.iter().map(|t| self.sub(t)).collect();
                let eargs: Vec<Eff> = eargs.iter().map(|x| x.subst(&self.f.eargs)).collect();
                let sym = self.fn_inst(*f, &targs, &eargs);
                let failable = self.fn_failable(*f, &eargs);
                let ops = self.args(args);
                let rt = self.ret_lty(&rty, failable);
                if self.f.terminated {
                    return (V::unit(), failable, rty);
                }
                if rt == "void" {
                    self.inst(&format!("call void {sym}({})", ops.join(", ")));
                    if self.p.fns[*f].ret == Ty::Never {
                        self.term("unreachable");
                    }
                    return (V::unit(), false, rty);
                }
                let t = self.tmp();
                self.inst(&format!("{t} = call {rt} {sym}({})", ops.join(", ")));
                (V::new(rt, t), failable, rty)
            }
            TK::CallValue { callee, args } => {
                let cty = self.sub(&callee.ty);
                let Ty::Fn(ft) = &cty else { unreachable!("checked: a function") };
                let failable = ft.eff.fail;
                let c = self.borrow(callee);
                let fp = self.extract(&c, 0, "ptr");
                let env = self.extract(&c, 1, "ptr");
                let mut ops = vec![format!("ptr {}", env.repr)];
                ops.extend(self.args(args));
                let rt = self.ret_lty(&rty, failable);
                if self.f.terminated {
                    return (V::unit(), failable, rty);
                }
                if rt == "void" {
                    self.inst(&format!("call void {}({})", fp.repr, ops.join(", ")));
                    return (V::unit(), false, rty);
                }
                let t = self.tmp();
                self.inst(&format!("{t} = call {rt} {}({})", fp.repr, ops.join(", ")));
                (V::new(rt, t), failable, rty)
            }
            _ => {
                let v = self.owned(e);
                (v, false, rty)
            }
        }
    }

    /// The value of a successful failable call, wrapped when the result is an optional.
    fn call_value_as(&mut self, r: &V, rty: &Ty, ty: &Ty) -> V {
        let vlt = self.lty(rty);
        let v = self.extract(r, 1, &vlt);
        if matches!(ty, Ty::Opt(_)) && !matches!(rty, Ty::Opt(_)) {
            let lt = self.lty(ty);
            let o = V::new(lt, "undef");
            let o = self.insert(&o, &V::new("i1", "true"), 0);
            return self.insert(&o, &v, 1);
        }
        v
    }

    /// `?`: on failure, return the error from this function; otherwise the value.
    pub fn propagate(&mut self, r: &V, rty: &Ty, span: Span) -> V {
        let _ = span;
        if self.f.terminated {
            return V::unit();
        }
        let failed = self.extract(r, 0, "i1");
        let bad = self.label("failed");
        let ok = self.label("ok");
        self.term(&format!("br i1 {}, label %{bad}, label %{ok}", failed.repr));
        self.start(&bad);
        let et = self.err_lty();
        let err = self.extract(r, 2, &et);
        self.emit_fail(err);
        self.start(&ok);
        let lt = self.lty(rty);
        if lt == "{}" {
            return V::unit();
        }
        self.extract(r, 1, &lt)
    }

    /// A closure value: its function and an environment holding copies of what it captures.
    fn make_closure(&mut self, id: ClosureId, ty: &Ty) -> V {
        let failable = matches!(ty, Ty::Fn(ft) if ft.eff.fail);
        let targs = self.f.targs.clone();
        let eargs = self.f.eargs.clone();
        let sym = self.closure_inst(id, &targs, &eargs, failable);
        let caps = self.p.closures[id].captures.clone();
        if caps.is_empty() {
            return V::new("%ovt.fn", format!("{{ ptr {sym}, ptr null }}"));
        }
        let env = self.env_lty(id);
        let size = self.tmp();
        self.inst(&format!("{size} = ptrtoint ptr getelementptr ({env}, ptr null, i32 1) to i64"));
        let p = self.tmp();
        self.inst(&format!("{p} = call ptr @ovt_alloc(i64 {size})"));
        self.inst(&format!("store i64 1, ptr {p}"));
        let drop = self.closure_drop(id, &targs, &eargs);
        let dp = self.gep(&env, &p, &[0, 1]);
        self.inst(&format!("store ptr {drop}, ptr {dp}"));
        for (k, (outer, _)) in caps.iter().enumerate() {
            let v = self.owned(&TExpr { kind: TK::Local(*outer), ty: self.f.local_tys[*outer].clone(), span: Span::default() });
            let fp = self.gep(&env, &p, &[0, k + 2]);
            self.inst(&format!("store {}, ptr {fp}", v.op()));
        }
        let agg = V::new("%ovt.fn", "undef");
        let agg = self.insert(&agg, &V::new("ptr", sym), 0);
        self.insert(&agg, &V::new("ptr", p), 1)
    }

    // ---- places ----

    fn place_ty(&self, p: &TPlace) -> Ty {
        match p {
            TPlace::Local(id) => self.f.local_tys[*id].clone(),
            TPlace::Field(b, i) => match self.place_ty(b) {
                Ty::Tuple(ts) => ts[*i as usize].clone(),
                Ty::Adt(id, targs) => self.p.adts[id].fields()[*i as usize].ty.subst(&targs, &[]),
                _ => unreachable!(),
            },
            TPlace::Index(b, _) => match self.place_ty(b) {
                Ty::Array(e) => *e,
                _ => Ty::U8,
            },
        }
    }

    /// A pointer to a place. With `mutable`, shared buffers and boxes on the
    /// way are copied first, so writing through the pointer changes only this value.
    pub fn place_ptr(&mut self, p: &TPlace, mutable: bool) -> String {
        match p {
            TPlace::Local(id) => {
                let slot = self.f.slots[*id].clone();
                if self.f.local_kinds[*id] == LocalKind::Ref { self.load("ptr", &slot).repr } else { slot }
            }
            TPlace::Field(b, i) => {
                let bty = self.place_ty(b);
                let bp = self.place_ptr(b, mutable);
                self.field_ptr(&bp, &bty, *i as usize, mutable)
            }
            TPlace::Index(b, idx) => {
                let bty = self.place_ty(b);
                let bp = self.place_ptr(b, mutable);
                let i = self.borrow(idx);
                self.index_ptr(&bp, &bty, &i.repr, mutable, idx.span)
            }
        }
    }

    /// Like `place_ptr`, for a place written as an expression (an `inout` argument).
    pub fn expr_place_ptr(&mut self, e: &TExpr, mutable: bool) -> String {
        match &e.kind {
            TK::Local(id) => {
                let slot = self.f.slots[*id].clone();
                if self.f.local_kinds[*id] == LocalKind::Ref { self.load("ptr", &slot).repr } else { slot }
            }
            TK::Field { base, index } => {
                let bty = self.sub(&base.ty);
                let bp = self.expr_place_ptr(base, mutable);
                self.field_ptr(&bp, &bty, *index as usize, mutable)
            }
            TK::Index { base, index } => {
                let bty = self.sub(&base.ty);
                let bp = self.expr_place_ptr(base, mutable);
                let i = self.borrow(index);
                self.index_ptr(&bp, &bty, &i.repr, mutable, index.span)
            }
            _ => {
                // Not a place (the checker only allows places here); use a temporary.
                let v = self.owned(e);
                let ty = self.sub(&e.ty);
                let p = self.spill(&v);
                self.add_drop(&p, &ty);
                p
            }
        }
    }

    fn field_ptr(&mut self, bp: &str, bty: &Ty, i: usize, mutable: bool) -> String {
        let lt = self.lty(bty);
        match bty {
            Ty::Adt(id, targs) => {
                let f = &self.p.adts[*id].fields()[i];
                let (boxed, fty) = (f.boxed, f.ty.subst(targs, &[]));
                let fp = self.gep(&lt, bp, &[0, i]);
                if !boxed {
                    return fp;
                }
                if mutable {
                    let size = self.size_const(&fty);
                    let dup = self.dup_fn(&fty);
                    let drop = self.drop_fn(&fty);
                    self.inst(&format!("call void @ovt_box_unique(ptr {fp}, i64 {size}, ptr {dup}, ptr {drop})"));
                }
                let bx = self.load("ptr", &fp);
                let vp = self.tmp();
                self.inst(&format!("{vp} = getelementptr inbounds i8, ptr {}, i64 8", bx.repr));
                vp
            }
            _ => self.gep(&lt, bp, &[0, i]),
        }
    }

    fn index_ptr(&mut self, bp: &str, bty: &Ty, i: &str, mutable: bool, span: Span) -> String {
        let et = match bty {
            Ty::Array(e) => (**e).clone(),
            _ => Ty::U8,
        };
        if mutable {
            self.make_unique(bp, &et);
        }
        let a = self.load("%ovt.arr", bp);
        let len = self.extract(&a, 2, "i64");
        self.bounds_check(i, &len.repr, span);
        self.elem_ptr(bp, i, &et)
    }

    // ---- operators ----

    fn binary(&mut self, op: BinOp, l: &TExpr, r: &TExpr, ty: &Ty, span: Span) -> (V, bool) {
        if matches!(op, BinOp::And | BinOp::Or) {
            let a = {
                self.push_scope(false);
                let a = self.borrow(l);
                self.pop_scope();
                a
            };
            let from = self.f.cur.clone();
            let rhs = self.label("rhs");
            let end = self.label("logic");
            if op == BinOp::And {
                self.term(&format!("br i1 {}, label %{rhs}, label %{end}", a.repr));
            } else {
                self.term(&format!("br i1 {}, label %{end}, label %{rhs}", a.repr));
            }
            self.start(&rhs);
            self.push_scope(false);
            let b = self.borrow(r);
            self.pop_scope();
            let rhs_end = self.f.cur.clone();
            self.start(&end);
            let t = self.tmp();
            let short = if op == BinOp::And { "false" } else { "true" };
            self.inst(&format!("{t} = phi i1 [ {short}, %{from} ], [ {}, %{rhs_end} ]", b.repr));
            return (V::new("i1", t), false);
        }
        let lty = self.sub(&l.ty);
        if op.is_comparison() {
            // `x == none`: just the tag.
            if matches!(r.kind, TK::None) {
                let a = self.borrow(l);
                let tag = self.extract(&a, 0, "i1");
                let t = self.tmp();
                let v = if op == BinOp::Eq { "false" } else { "true" };
                self.inst(&format!("{t} = icmp eq i1 {}, {v}", tag.repr));
                return (V::new("i1", t), false);
            }
            let a = self.borrow(l);
            let b = self.borrow(r);
            return (self.compare(op, &lty, &a, &b), false);
        }
        if lty == Ty::Str && op == BinOp::Add {
            let a = self.borrow(l);
            let b = self.borrow(r);
            let ap = self.spill(&a);
            let bp = self.spill(&b);
            let out = self.alloca("%ovt.arr");
            self.inst(&format!("call void @ovt_str_concat(ptr {out}, ptr {ap}, ptr {bp})"));
            return (self.load("%ovt.arr", &out), true);
        }
        let a = self.borrow(l);
        let b = self.borrow(r);
        (self.arith(op, ty, &a, &b, span), false)
    }

    fn overflow_intrinsic(&mut self, name: &str, lt: &str) -> String {
        self.decls.insert(format!("declare {{ {lt}, i1 }} @llvm.{name}.with.overflow.{lt}({lt}, {lt})"));
        format!("@llvm.{name}.with.overflow.{lt}")
    }

    /// Arithmetic on numbers. Integer `+ - *` trap on overflow; `/` and `%` trap on zero.
    pub fn arith(&mut self, op: BinOp, ty: &Ty, a: &V, b: &V, span: Span) -> V {
        let lt = self.lty(ty);
        let t = self.tmp();
        if let Ty::Float(_) = ty {
            let ins = match op {
                BinOp::Add => "fadd",
                BinOp::Sub => "fsub",
                BinOp::Mul => "fmul",
                BinOp::Div => "fdiv",
                _ => "frem",
            };
            self.inst(&format!("{t} = {ins} {lt} {}, {}", a.repr, b.repr));
            return V::new(lt, t);
        }
        let (signed, bits) = match ty {
            Ty::Int(k) => (k.signed(), k.bits()),
            _ => (true, 64),
        };
        match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul => {
                let base = match op {
                    BinOp::Add => "add",
                    BinOp::Sub => "sub",
                    _ => "mul",
                };
                let name = format!("{}{base}", if signed { "s" } else { "u" });
                let f = self.overflow_intrinsic(&name, &lt);
                let pair = self.tmp();
                self.inst(&format!("{pair} = call {{ {lt}, i1 }} {f}({lt} {}, {lt} {})", a.repr, b.repr));
                self.inst(&format!("{t} = extractvalue {{ {lt}, i1 }} {pair}, 0"));
                let o = self.tmp();
                self.inst(&format!("{o} = extractvalue {{ {lt}, i1 }} {pair}, 1"));
                let what = match op {
                    BinOp::Add => "addition",
                    BinOp::Sub => "subtraction",
                    _ => "multiplication",
                };
                let hint = if ty.is_int() { format!("; use `{}%` to wrap", op.text()) } else { String::new() };
                self.trap_if(&o, &format!("integer overflow in {what}{hint}"), span);
            }
            BinOp::Div | BinOp::Rem => {
                let z = self.tmp();
                self.inst(&format!("{z} = icmp eq {lt} {}, 0", b.repr));
                self.trap_if(&z, if op == BinOp::Div { "division by zero" } else { "remainder by zero" }, span);
                if signed {
                    let min = -(1i128 << (bits - 1));
                    let m1 = self.tmp();
                    let mn = self.tmp();
                    let both = self.tmp();
                    self.inst(&format!("{m1} = icmp eq {lt} {}, -1", b.repr));
                    self.inst(&format!("{mn} = icmp eq {lt} {}, {min}", a.repr));
                    self.inst(&format!("{both} = and i1 {m1}, {mn}"));
                    self.trap_if(&both, "integer overflow in division", span);
                }
                let ins = match (op, signed) {
                    (BinOp::Div, true) => "sdiv",
                    (BinOp::Div, false) => "udiv",
                    (_, true) => "srem",
                    (_, false) => "urem",
                };
                self.inst(&format!("{t} = {ins} {lt} {}, {}", a.repr, b.repr));
            }
            BinOp::AddW | BinOp::SubW | BinOp::MulW => {
                let ins = match op {
                    BinOp::AddW => "add",
                    BinOp::SubW => "sub",
                    _ => "mul",
                };
                self.inst(&format!("{t} = {ins} {lt} {}, {}", a.repr, b.repr));
            }
            BinOp::Shl | BinOp::Shr => {
                let big = self.tmp();
                self.inst(&format!("{big} = icmp uge {lt} {}, {bits}", b.repr));
                self.trap_if(&big, &format!("shift amount must be between 0 and {}", bits - 1), span);
                let ins = match (op, signed) {
                    (BinOp::Shl, _) => "shl",
                    (_, true) => "ashr",
                    (_, false) => "lshr",
                };
                self.inst(&format!("{t} = {ins} {lt} {}, {}", a.repr, b.repr));
            }
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                let ins = match op {
                    BinOp::BitAnd => "and",
                    BinOp::BitOr => "or",
                    _ => "xor",
                };
                self.inst(&format!("{t} = {ins} {lt} {}, {}", a.repr, b.repr));
            }
            _ => unreachable!("not arithmetic"),
        }
        V::new(lt, t)
    }

    /// `==`, `!=`, `<`, `<=`, `>`, `>=` on any comparable type.
    pub fn compare(&mut self, op: BinOp, ty: &Ty, a: &V, b: &V) -> V {
        let t = self.tmp();
        match ty {
            Ty::Int(_) | Ty::Bool | Ty::Dur => {
                let signed = matches!(ty, Ty::Int(k) if k.signed()) || *ty == Ty::Dur;
                let cc = match op {
                    BinOp::Eq => "eq",
                    BinOp::Ne => "ne",
                    BinOp::Lt => if signed { "slt" } else { "ult" },
                    BinOp::Le => if signed { "sle" } else { "ule" },
                    BinOp::Gt => if signed { "sgt" } else { "ugt" },
                    _ => if signed { "sge" } else { "uge" },
                };
                self.inst(&format!("{t} = icmp {cc} {} {}, {}", a.ty, a.repr, b.repr));
            }
            Ty::Float(_) => {
                let cc = match op {
                    BinOp::Eq => "oeq",
                    BinOp::Ne => "une",
                    BinOp::Lt => "olt",
                    BinOp::Le => "ole",
                    BinOp::Gt => "ogt",
                    _ => "oge",
                };
                self.inst(&format!("{t} = fcmp {cc} {} {}, {}", a.ty, a.repr, b.repr));
            }
            Ty::Adt(id, _) if self.is_payloadless_enum(*id) && matches!(op, BinOp::Eq | BinOp::Ne) => {
                let cc = if op == BinOp::Eq { "eq" } else { "ne" };
                self.inst(&format!("{t} = icmp {cc} i32 {}, {}", a.repr, b.repr));
            }
            _ if matches!(op, BinOp::Eq | BinOp::Ne) => {
                let eq = self.equal(a, b, ty);
                if op == BinOp::Eq {
                    return eq;
                }
                self.inst(&format!("{t} = xor i1 {}, true", eq.repr));
            }
            _ => {
                let c = self.cmp3(a, b, ty);
                let cc = match op {
                    BinOp::Lt => "slt",
                    BinOp::Le => "sle",
                    BinOp::Gt => "sgt",
                    _ => "sge",
                };
                self.inst(&format!("{t} = icmp {cc} i32 {}, 0", c.repr));
            }
        }
        V::new("i1", t)
    }

    /// Equality through the type's `eq` helper (or directly for strings).
    pub fn equal(&mut self, a: &V, b: &V, ty: &Ty) -> V {
        if matches!(ty, Ty::Int(_) | Ty::Bool | Ty::Float(_) | Ty::Dur) || matches!(ty, Ty::Adt(id, _) if self.is_payloadless_enum(*id)) {
            return self.compare(BinOp::Eq, ty, a, b);
        }
        let ap = self.spill(a);
        let bp = self.spill(b);
        let t = self.tmp();
        if *ty == Ty::Str {
            let r = self.tmp();
            self.inst(&format!("{r} = call i32 @ovt_str_eq(ptr {ap}, ptr {bp})"));
            self.inst(&format!("{t} = icmp ne i32 {r}, 0"));
        } else {
            let h = self.helper(Helper::Eq, ty);
            self.inst(&format!("{t} = call i1 {h}(ptr {ap}, ptr {bp})"));
        }
        V::new("i1", t)
    }

    /// -1, 0 or 1 through the type's `cmp` helper.
    pub fn cmp3(&mut self, a: &V, b: &V, ty: &Ty) -> V {
        let ap = self.spill(a);
        let bp = self.spill(b);
        let t = self.tmp();
        if *ty == Ty::Str {
            self.inst(&format!("{t} = call i32 @ovt_str_cmp(ptr {ap}, ptr {bp})"));
        } else {
            let h = self.helper(Helper::Cmp, ty);
            self.inst(&format!("{t} = call i32 {h}(ptr {ap}, ptr {bp})"));
        }
        V::new("i32", t)
    }

    /// A checked conversion between number types.
    pub fn convert(&mut self, v: &V, from: &Ty, to: &Ty, span: Span) -> V {
        let tl = self.lty(to);
        let t = self.tmp();
        match (from, to) {
            (Ty::Int(a), Ty::Int(b)) => {
                let fl = &v.ty;
                // Range check in the source type.
                if a.signed() {
                    if b.min_value() > a.min_value() {
                        let c = self.tmp();
                        self.inst(&format!("{c} = icmp slt {fl} {}, {}", v.repr, b.min_value()));
                        self.trap_if(&c, &format!("the value is too small for `{}`", b.name()), span);
                    }
                    if b.max_value() < a.max_value() {
                        let c = self.tmp();
                        self.inst(&format!("{c} = icmp sgt {fl} {}, {}", v.repr, b.max_value()));
                        self.trap_if(&c, &format!("the value is too large for `{}`", b.name()), span);
                    }
                } else if b.max_value() < a.max_value() {
                    let c = self.tmp();
                    let max = b.max_value();
                    // Unsigned constants above the signed range are written as two's complement.
                    let repr = if max > i64::MAX as i128 { (max as u64 as i64).to_string() } else { max.to_string() };
                    self.inst(&format!("{c} = icmp ugt {fl} {}, {repr}", v.repr));
                    self.trap_if(&c, &format!("the value is too large for `{}`", b.name()), span);
                }
                if b.bits() > a.bits() {
                    let ins = if a.signed() { "sext" } else { "zext" };
                    self.inst(&format!("{t} = {ins} {} to {tl}", v.op()));
                } else if b.bits() < a.bits() {
                    self.inst(&format!("{t} = trunc {} to {tl}", v.op()));
                } else {
                    return V::new(tl, v.repr.clone());
                }
            }
            (Ty::Int(a), Ty::Float(_)) => {
                let ins = if a.signed() { "sitofp" } else { "uitofp" };
                self.inst(&format!("{t} = {ins} {} to {tl}", v.op()));
            }
            (Ty::Float(_), Ty::Int(b)) => {
                // In range, and not NaN: min - 1 < v < max + 1.
                let lo = Self::float_const(b.min_value() as f64 - 1.0, from);
                let hi = Self::float_const(b.max_value() as f64 + 1.0, from);
                let c1 = self.tmp();
                let c2 = self.tmp();
                let ok = self.tmp();
                self.inst(&format!("{c1} = fcmp ogt {} {}, {lo}", v.ty, v.repr));
                self.inst(&format!("{c2} = fcmp olt {} {}, {hi}", v.ty, v.repr));
                self.inst(&format!("{ok} = and i1 {c1}, {c2}"));
                let bad = self.tmp();
                self.inst(&format!("{bad} = xor i1 {ok}, true"));
                self.trap_if(&bad, &format!("the value doesn't fit in `{}`", b.name()), span);
                let ins = if b.signed() { "fptosi" } else { "fptoui" };
                self.inst(&format!("{t} = {ins} {} to {tl}", v.op()));
            }
            (Ty::Float(FloatTy::F32), Ty::Float(FloatTy::F64)) => self.inst(&format!("{t} = fpext {} to {tl}", v.op())),
            (Ty::Float(FloatTy::F64), Ty::Float(FloatTy::F32)) => self.inst(&format!("{t} = fptrunc {} to {tl}", v.op())),
            _ => return V::new(tl, v.repr.clone()),
        }
        V::new(tl, t)
    }
}
