//! Per-type helper functions, generated on demand for each concrete type:
//! `dup` and `drop` (reference counts), `eq`, `cmp` (-1, 0 or 1), `hash`, and
//! `show` (appends the value, as `"${v}"` shows it, to a string being built).
//! Each takes pointers to values and calls the helpers of its component types.

use super::*;

impl<'p> Gen<'p> {
    pub fn emit_helper(&mut self, kind: Helper, ty: &Ty, sym: &str) -> String {
        self.f = Fx { cur: "entry".into(), ..Default::default() };
        let (params, ret) = match kind {
            Helper::Dup | Helper::Drop | Helper::Mark => ("ptr %p", "void"),
            Helper::Eq => ("ptr %a, ptr %b", "i1"),
            Helper::Cmp => ("ptr %a, ptr %b", "i32"),
            Helper::Hash => ("ptr %p", "i64"),
            Helper::Show | Helper::JsonEnc => ("ptr %p, ptr %sb", "void"),
            Helper::JsonDec => ("ptr %jp, ptr %out", "i1"),
        };
        match kind {
            Helper::Dup | Helper::Drop => {
                self.gen_rc(ty, "%p", kind == Helper::Dup);
                self.term("ret void");
            }
            Helper::Mark => {
                self.gen_mark(ty, "%p");
                self.term("ret void");
            }
            Helper::Eq => {
                let v = self.gen_eq(ty, "%a", "%b");
                self.term(&format!("ret i1 {v}"));
            }
            Helper::Cmp => {
                let v = self.gen_cmp(ty, "%a", "%b");
                self.term(&format!("ret i32 {v}"));
            }
            Helper::Hash => {
                let v = self.gen_hash(ty, "%p");
                self.term(&format!("ret i64 {v}"));
            }
            Helper::Show => {
                self.gen_show(ty, "%p", "%sb");
                self.term("ret void");
            }
            Helper::JsonEnc => {
                self.gen_json_enc(ty, "%p", "%sb");
                self.term("ret void");
            }
            Helper::JsonDec => {
                let ok = self.gen_json_dec(ty, "%jp", "%out");
                if !self.f.terminated {
                    self.term(&format!("ret i1 {ok}"));
                }
            }
        }
        let f = std::mem::take(&mut self.f);
        format!("define internal {ret} {sym}({params}) {{\nentry:\n{}{}}}\n", f.allocas, f.code)
    }

    /// Runs `body` only when `cond` holds.
    pub fn when(&mut self, cond: &str, body: impl FnOnce(&mut Self)) {
        let yes = self.label("yes");
        let done = self.label("done");
        self.term(&format!("br i1 {cond}, label %{yes}, label %{done}"));
        self.start(&yes);
        body(self);
        self.start(&done);
    }

    pub fn call_helper(&mut self, kind: Helper, ty: &Ty, ptr: &str) {
        if self.needs_rc(ty) {
            let h = self.helper(kind, ty);
            self.inst(&format!("call void {h}(ptr {ptr})"));
        }
    }

    /// The fields of a struct, tuple or variant: (pointer type, index, boxed, type).
    pub fn components(&mut self, ty: &Ty, variant: Option<usize>) -> (String, Vec<(bool, Ty)>) {
        match (ty, variant) {
            (Ty::Tuple(ts), _) => {
                let lt = self.lty(ty);
                (lt, ts.iter().map(|t| (false, t.clone())).collect())
            }
            (Ty::Adt(id, targs), None) => {
                let lt = self.lty(ty);
                let fs = self.p.adts[*id].fields().iter().map(|f| (f.boxed, f.ty.subst(targs, &[]))).collect();
                (lt, fs)
            }
            (Ty::Adt(id, targs), Some(v)) => {
                let lt = self.variant_lty(*id, targs, v);
                let fs = self.p.adts[*id].variants()[v].fields.iter().map(|f| (f.boxed, f.ty.subst(targs, &[]))).collect();
                (lt, fs)
            }
            _ => unreachable!("no components"),
        }
    }

    /// For each variant with fields: runs `each(variant, payload pointer)` in a switch on the tag.
    pub fn per_variant(&mut self, ty: &Ty, ptr: &str, mut each: impl FnMut(&mut Self, usize, &str)) {
        let Ty::Adt(id, _) = ty else { unreachable!() };
        let lt = self.lty(ty);
        let tagp = self.gep(&lt, ptr, &[0, 0]);
        let tag = self.load("i32", &tagp);
        let payload = self.gep(&lt, ptr, &[0, 1]);
        let n = self.p.adts[*id].variants().len();
        let done = self.label("done");
        let labels: Vec<String> = (0..n).map(|_| self.label("variant")).collect();
        let cases: Vec<String> = labels.iter().enumerate().map(|(i, l)| format!("i32 {i}, label %{l}")).collect();
        self.term(&format!("switch i32 {}, label %{done} [ {} ]", tag.repr, cases.join(" ")));
        for (i, l) in labels.iter().enumerate() {
            self.start(l);
            each(self, i, &payload);
            if !self.f.terminated {
                self.term(&format!("br label %{done}"));
            }
        }
        self.start(&done);
    }

    // ---- dup and drop ----

    fn gen_rc(&mut self, ty: &Ty, p: &str, dup: bool) {
        match ty {
            Ty::Str | Ty::Array(_) => {
                let buf = self.load("ptr", p);
                if dup {
                    self.inc_count(&buf.repr, true);
                } else {
                    let (esize, drop) = match ty {
                        Ty::Array(e) => {
                            let s = self.size_const(e);
                            (s, self.drop_fn(e))
                        }
                        _ => ("1".into(), "null".into()),
                    };
                    // Fast path: owned by this task, with other copies left, so
                    // just decrement. The runtime handles the rest.
                    let nn = self.tmp();
                    self.inst(&format!("{nn} = icmp ne ptr {}, null", buf.repr));
                    let b = buf.repr.clone();
                    self.when(&nn, |g| {
                        let rc = g.load_rc(&b);
                        let shared = g.tmp();
                        g.inst(&format!("{shared} = icmp sgt i64 {}, 1", rc.repr));
                        let dec = g.label("dec");
                        let release = g.label("release");
                        let done = g.label("dropped");
                        g.term(&format!("br i1 {shared}, label %{dec}, label %{release}"));
                        g.start(&dec);
                        let n = g.tmp();
                        g.inst(&format!("{n} = sub i64 {}, 1", rc.repr));
                        g.inst(&format!("store i64 {n}, ptr {b}"));
                        g.term(&format!("br label %{done}"));
                        g.start(&release);
                        g.inst(&format!("call void @ovt_buf_release(ptr {b}, i64 {esize}, ptr {drop})"));
                        g.term(&format!("br label %{done}"));
                        g.start(&done);
                    });
                }
            }
            Ty::Fn(_) => {
                let envp = self.gep("%ovt.fn", p, &[0, 1]);
                let env = self.load("ptr", &envp);
                let nn = self.tmp();
                self.inst(&format!("{nn} = icmp ne ptr {}, null", env.repr));
                let env = env.repr.clone();
                if dup {
                    self.when(&nn, |g| g.inc_count(&env, false));
                } else {
                    self.when(&nn, |g| {
                        let rc = g.load_rc(&env);
                        let many = g.tmp();
                        g.inst(&format!("{many} = icmp sgt i64 {}, 1", rc.repr));
                        let dec = g.label("dec");
                        let release = g.label("release");
                        let done = g.label("dropped");
                        g.term(&format!("br i1 {many}, label %{dec}, label %{release}"));
                        g.start(&dec);
                        let n = g.tmp();
                        g.inst(&format!("{n} = sub i64 {}, 1", rc.repr));
                        g.inst(&format!("store i64 {n}, ptr {env}"));
                        g.term(&format!("br label %{done}"));
                        g.start(&release);
                        g.inst(&format!("call void @ovt_env_release(ptr {env})"));
                        g.term(&format!("br label %{done}"));
                        g.start(&done);
                    });
                }
            }
            Ty::Opt(inner) => {
                let lt = self.lty(ty);
                let tagp = self.gep(&lt, p, &[0, 0]);
                let tag = self.load("i1", &tagp);
                let ip = self.gep(&lt, p, &[0, 1]);
                let inner = (**inner).clone();
                self.when(&tag.repr, |g| g.call_helper(if dup { Helper::Dup } else { Helper::Drop }, &inner, &ip));
            }
            Ty::Tuple(_) => self.rc_fields(ty, None, p, dup),
            Ty::Adt(id, _) if self.p.adts[*id].is_enum() => {
                if self.is_payloadless_enum(*id) {
                    return;
                }
                let tyc = ty.clone();
                self.per_variant(ty, p, |g, v, payload| g.rc_fields(&tyc, Some(v), payload, dup));
            }
            Ty::Adt(..) => self.rc_fields(ty, None, p, dup),
            _ => {}
        }
    }

    fn rc_fields(&mut self, ty: &Ty, variant: Option<usize>, p: &str, dup: bool) {
        let (lt, fields) = self.components(ty, variant);
        for (i, (boxed, fty)) in fields.iter().enumerate() {
            if !boxed && !self.needs_rc(fty) {
                continue;
            }
            let fp = self.gep(&lt, p, &[0, i]);
            if *boxed {
                let bx = self.load("ptr", &fp);
                if dup {
                    self.inc_count(&bx.repr, false);
                } else {
                    let drop = self.drop_fn(fty);
                    self.inst(&format!("call void @ovt_box_release(ptr {}, ptr {drop})", bx.repr));
                }
            } else {
                self.call_helper(if dup { Helper::Dup } else { Helper::Drop }, fty, &fp);
            }
        }
    }

    /// Adds one to the count at the start of `ptr`, unless it's null. A
    /// count owned by this task changes inline; a shared or static one goes
    /// through the runtime.
    fn inc_count(&mut self, ptr: &str, _may_be_static: bool) {
        let nn = self.tmp();
        self.inst(&format!("{nn} = icmp ne ptr {ptr}, null"));
        let ptr = ptr.to_string();
        self.when(&nn, |g| {
            let rc = g.load_rc(&ptr);
            let owned = g.tmp();
            g.inst(&format!("{owned} = icmp sgt i64 {}, 0", rc.repr));
            let inc = g.label("inc");
            let slow = g.label("shared");
            let done = g.label("counted");
            g.term(&format!("br i1 {owned}, label %{inc}, label %{slow}"));
            g.start(&inc);
            let n = g.tmp();
            g.inst(&format!("{n} = add i64 {}, 1", rc.repr));
            g.inst(&format!("store i64 {n}, ptr {ptr}"));
            g.term(&format!("br label %{done}"));
            g.start(&slow);
            g.inst(&format!("call void @ovt_rc_inc(ptr {ptr})"));
            g.term(&format!("br label %{done}"));
            g.start(&done);
        });
    }

    // ---- marking shared ----

    /// Marks the counts of everything the value at `p` holds as shared.
    fn gen_mark(&mut self, ty: &Ty, p: &str) {
        match ty {
            Ty::Str | Ty::Array(_) => {
                let buf = self.load("ptr", p);
                let (esize, mark) = match ty {
                    Ty::Array(e) => {
                        let s = self.size_const(e);
                        (s, self.mark_fn(e))
                    }
                    _ => ("1".into(), "null".into()),
                };
                self.inst(&format!("call void @ovt_mark_buf(ptr {}, i64 {esize}, ptr {mark})", buf.repr));
            }
            Ty::Fn(_) => {
                let envp = self.gep("%ovt.fn", p, &[0, 1]);
                let env = self.load("ptr", &envp);
                self.inst(&format!("call void @ovt_mark_env(ptr {})", env.repr));
            }
            Ty::Opt(inner) => {
                let lt = self.lty(ty);
                let tagp = self.gep(&lt, p, &[0, 0]);
                let tag = self.load("i1", &tagp);
                let ip = self.gep(&lt, p, &[0, 1]);
                let inner = (**inner).clone();
                self.when(&tag.repr, |g| g.mark_value(&inner, &ip));
            }
            Ty::Tuple(_) => self.mark_fields(ty, None, p),
            Ty::Adt(id, _) if self.p.adts[*id].is_enum() => {
                if self.is_payloadless_enum(*id) {
                    return;
                }
                let tyc = ty.clone();
                self.per_variant(ty, p, |g, v, payload| g.mark_fields(&tyc, Some(v), payload));
            }
            Ty::Adt(..) => self.mark_fields(ty, None, p),
            _ => {}
        }
    }

    fn mark_value(&mut self, ty: &Ty, p: &str) {
        if self.needs_rc(ty) {
            let h = self.helper(Helper::Mark, ty);
            self.inst(&format!("call void {h}(ptr {p})"));
        }
    }

    fn mark_fields(&mut self, ty: &Ty, variant: Option<usize>, p: &str) {
        let (lt, fields) = self.components(ty, variant);
        for (i, (boxed, fty)) in fields.iter().enumerate() {
            if !boxed && !self.needs_rc(fty) {
                continue;
            }
            let fp = self.gep(&lt, p, &[0, i]);
            if *boxed {
                let bx = self.load("ptr", &fp);
                let mark = self.mark_fn(fty);
                self.inst(&format!("call void @ovt_mark_box(ptr {}, ptr {mark})", bx.repr));
            } else {
                self.mark_value(fty, &fp);
            }
        }
    }

    // ---- equality ----

    /// Emits code comparing the values at `a` and `b`; returns an `i1` operand.
    fn gen_eq(&mut self, ty: &Ty, a: &str, b: &str) -> String {
        match ty {
            Ty::Int(_) | Ty::Bool | Ty::Float(_) | Ty::Dur => {
                let lt = self.lty(ty);
                let x = self.load(&lt, a);
                let y = self.load(&lt, b);
                self.compare(crate::ast::BinOp::Eq, ty, &x, &y).repr
            }
            Ty::Unit => "true".into(),
            Ty::Fn(_) => "false".into(),
            Ty::Str => {
                let r = self.tmp();
                self.inst(&format!("{r} = call i32 @ovt_str_eq(ptr {a}, ptr {b})"));
                let t = self.tmp();
                self.inst(&format!("{t} = icmp ne i32 {r}, 0"));
                t
            }
            Ty::Adt(id, targs) if *id == self.p.known.map || *id == self.p.known.set => {
                let f = if *id == self.p.known.map { self.p.known.map_equals } else { self.p.known.set_equals };
                let sym = self.fn_inst(f, targs, &[]);
                let lt = self.lty(ty);
                let x = self.load(&lt, a);
                let y = self.load(&lt, b);
                let t = self.tmp();
                self.inst(&format!("{t} = call i1 {sym}({}, {})", x.op(), y.op()));
                t
            }
            Ty::Adt(id, _) if self.is_payloadless_enum(*id) => {
                let x = self.load("i32", a);
                let y = self.load("i32", b);
                let t = self.tmp();
                self.inst(&format!("{t} = icmp eq i32 {}, {}", x.repr, y.repr));
                t
            }
            _ => {
                // Compare piece by piece; any difference jumps to `no`.
                let result = self.alloca("i1");
                self.inst(&format!("store i1 false, ptr {result}"));
                let no = self.label("ne");
                self.eq_parts(ty, a, b, &no);
                self.inst(&format!("store i1 true, ptr {result}"));
                self.start(&no);
                self.load("i1", &result).repr
            }
        }
    }

    /// Branches to `no` if the values differ; falls through if they're equal.
    fn eq_parts(&mut self, ty: &Ty, a: &str, b: &str, no: &str) {
        let check = |g: &mut Self, eq: String, no: &str| {
            let next = g.label("eq");
            g.term(&format!("br i1 {eq}, label %{next}, label %{no}"));
            g.start(&next);
        };
        match ty {
            Ty::Array(e) => {
                let e = (**e).clone();
                let x = self.load("%ovt.arr", a);
                let y = self.load("%ovt.arr", b);
                let lx = self.extract(&x, 2, "i64");
                let ly = self.extract(&y, 2, "i64");
                let same = self.tmp();
                self.inst(&format!("{same} = icmp eq i64 {}, {}", lx.repr, ly.repr));
                check(self, same, no);
                let len = lx.repr;
                self.each_index(&len, |g, i| {
                    let ea = g.elem_ptr(a, i, &e);
                    let eb = g.elem_ptr(b, i, &e);
                    let eq = g.eq_call(&e, &ea, &eb);
                    check(g, eq, no);
                });
            }
            Ty::Opt(inner) => {
                let lt = self.lty(ty);
                let ta = self.gep(&lt, a, &[0, 0]);
                let tb = self.gep(&lt, b, &[0, 0]);
                let x = self.load("i1", &ta);
                let y = self.load("i1", &tb);
                let same = self.tmp();
                self.inst(&format!("{same} = icmp eq i1 {}, {}", x.repr, y.repr));
                check(self, same, no);
                let ia = self.gep(&lt, a, &[0, 1]);
                let ib = self.gep(&lt, b, &[0, 1]);
                let inner = (**inner).clone();
                let tag = x.repr;
                self.when(&tag, |g| {
                    let eq = g.eq_call(&inner, &ia, &ib);
                    check(g, eq, no);
                });
            }
            Ty::Tuple(_) => self.eq_fields(ty, None, a, b, no),
            Ty::Adt(id, _) if self.p.adts[*id].is_enum() => {
                let lt = self.lty(ty);
                let ta = self.gep(&lt, a, &[0, 0]);
                let tb = self.gep(&lt, b, &[0, 0]);
                let x = self.load("i32", &ta);
                let y = self.load("i32", &tb);
                let same = self.tmp();
                self.inst(&format!("{same} = icmp eq i32 {}, {}", x.repr, y.repr));
                check(self, same, no);
                let pb = self.gep(&lt, b, &[0, 1]);
                let tyc = ty.clone();
                self.per_variant(ty, a, |g, v, pa| g.eq_fields(&tyc, Some(v), pa, &pb, no));
            }
            Ty::Adt(..) => self.eq_fields(ty, None, a, b, no),
            _ => {
                let eq = self.eq_call(ty, a, b);
                check(self, eq, no);
            }
        }
    }

    fn eq_fields(&mut self, ty: &Ty, variant: Option<usize>, a: &str, b: &str, no: &str) {
        let (lt, fields) = self.components(ty, variant);
        for (i, (boxed, fty)) in fields.iter().enumerate() {
            let mut fa = self.gep(&lt, a, &[0, i]);
            let mut fb = self.gep(&lt, b, &[0, i]);
            if *boxed {
                fa = self.box_value_ptr(&fa);
                fb = self.box_value_ptr(&fb);
            }
            let eq = self.eq_call(fty, &fa, &fb);
            let next = self.label("eq");
            self.term(&format!("br i1 {eq}, label %{next}, label %{no}"));
            self.start(&next);
        }
    }

    pub fn box_value_ptr(&mut self, slot: &str) -> String {
        let bx = self.load("ptr", slot);
        let vp = self.tmp();
        self.inst(&format!("{vp} = getelementptr inbounds i8, ptr {}, i64 8", bx.repr));
        vp
    }

    /// Equality of the values at two pointers, as an `i1` operand.
    fn eq_call(&mut self, ty: &Ty, a: &str, b: &str) -> String {
        if matches!(ty, Ty::Int(_) | Ty::Bool | Ty::Float(_) | Ty::Dur | Ty::Str | Ty::Unit) {
            return self.gen_eq(ty, a, b);
        }
        let h = self.helper(Helper::Eq, ty);
        let t = self.tmp();
        self.inst(&format!("{t} = call i1 {h}(ptr {a}, ptr {b})"));
        t
    }

    /// Runs `body(i)` for i in 0..len.
    pub fn each_index(&mut self, len: &str, mut body: impl FnMut(&mut Self, &str)) {
        let counter = self.alloca("i64");
        self.inst(&format!("store i64 0, ptr {counter}"));
        let head = self.label("loop");
        let bl = self.label("body");
        let end = self.label("end");
        self.start(&head);
        let i = self.load("i64", &counter);
        let c = self.tmp();
        self.inst(&format!("{c} = icmp slt i64 {}, {len}", i.repr));
        self.term(&format!("br i1 {c}, label %{bl}, label %{end}"));
        self.start(&bl);
        body(self, &i.repr);
        let n = self.tmp();
        self.inst(&format!("{n} = add i64 {}, 1", i.repr));
        self.inst(&format!("store i64 {n}, ptr {counter}"));
        self.term(&format!("br label %{head}"));
        self.start(&end);
    }

    // ---- ordering ----

    fn gen_cmp(&mut self, ty: &Ty, a: &str, b: &str) -> String {
        match ty {
            Ty::Int(_) | Ty::Bool | Ty::Float(_) | Ty::Dur => {
                let lt = self.lty(ty);
                let x = self.load(&lt, a);
                let y = self.load(&lt, b);
                let lo = self.compare(crate::ast::BinOp::Lt, ty, &x, &y);
                let hi = self.compare(crate::ast::BinOp::Gt, ty, &x, &y);
                let s = self.tmp();
                self.inst(&format!("{s} = select i1 {}, i32 1, i32 0", hi.repr));
                let t = self.tmp();
                self.inst(&format!("{t} = select i1 {}, i32 -1, i32 {s}", lo.repr));
                t
            }
            Ty::Str => {
                let t = self.tmp();
                self.inst(&format!("{t} = call i32 @ovt_str_cmp(ptr {a}, ptr {b})"));
                t
            }
            Ty::Array(e) => {
                let e = (**e).clone();
                let result = self.alloca("i32");
                let done = self.label("decided");
                let x = self.load("%ovt.arr", a);
                let y = self.load("%ovt.arr", b);
                let lx = self.extract(&x, 2, "i64");
                let ly = self.extract(&y, 2, "i64");
                let short = self.tmp();
                self.inst(&format!("{short} = icmp slt i64 {}, {}", lx.repr, ly.repr));
                let n = self.tmp();
                self.inst(&format!("{n} = select i1 {short}, i64 {}, i64 {}", lx.repr, ly.repr));
                self.each_index(&n, |g, i| {
                    let ea = g.elem_ptr(a, i, &e);
                    let eb = g.elem_ptr(b, i, &e);
                    let c = g.cmp_call(&e, &ea, &eb);
                    g.decide(&c, &result, &done);
                });
                let lens = self.cmp_ints(&lx.repr, &ly.repr);
                self.inst(&format!("store i32 {lens}, ptr {result}"));
                self.term(&format!("br label %{done}"));
                self.start(&done);
                self.load("i32", &result).repr
            }
            Ty::Tuple(ts) => {
                let ts = ts.clone();
                let lt = self.lty(ty);
                let result = self.alloca("i32");
                let done = self.label("decided");
                for (i, t) in ts.iter().enumerate() {
                    let fa = self.gep(&lt, a, &[0, i]);
                    let fb = self.gep(&lt, b, &[0, i]);
                    let c = self.cmp_call(t, &fa, &fb);
                    self.decide(&c, &result, &done);
                }
                self.inst(&format!("store i32 0, ptr {result}"));
                self.term(&format!("br label %{done}"));
                self.start(&done);
                self.load("i32", &result).repr
            }
            Ty::Opt(inner) => {
                let inner = (**inner).clone();
                let lt = self.lty(ty);
                let ta = self.gep(&lt, a, &[0, 0]);
                let tb = self.gep(&lt, b, &[0, 0]);
                let x = self.load("i1", &ta);
                let y = self.load("i1", &tb);
                let result = self.alloca("i32");
                let tags = self.compare(crate::ast::BinOp::Lt, &Ty::Bool, &x, &y);
                let tags_gt = self.compare(crate::ast::BinOp::Gt, &Ty::Bool, &x, &y);
                let s = self.tmp();
                self.inst(&format!("{s} = select i1 {}, i32 1, i32 0", tags_gt.repr));
                let c = self.tmp();
                self.inst(&format!("{c} = select i1 {}, i32 -1, i32 {s}", tags.repr));
                self.inst(&format!("store i32 {c}, ptr {result}"));
                let both = self.tmp();
                self.inst(&format!("{both} = and i1 {}, {}", x.repr, y.repr));
                let ia = self.gep(&lt, a, &[0, 1]);
                let ib = self.gep(&lt, b, &[0, 1]);
                self.when(&both, |g| {
                    let c = g.cmp_call(&inner, &ia, &ib);
                    g.inst(&format!("store i32 {c}, ptr {result}"));
                });
                self.load("i32", &result).repr
            }
            _ => "0".into(),
        }
    }

    fn cmp_ints(&mut self, x: &str, y: &str) -> String {
        let lo = self.tmp();
        self.inst(&format!("{lo} = icmp slt i64 {x}, {y}"));
        let hi = self.tmp();
        self.inst(&format!("{hi} = icmp sgt i64 {x}, {y}"));
        let s = self.tmp();
        self.inst(&format!("{s} = select i1 {hi}, i32 1, i32 0"));
        let t = self.tmp();
        self.inst(&format!("{t} = select i1 {lo}, i32 -1, i32 {s}"));
        t
    }

    /// If `c` isn't 0, stores it as the result and jumps to `done`.
    fn decide(&mut self, c: &str, result: &str, done: &str) {
        let nz = self.tmp();
        self.inst(&format!("{nz} = icmp ne i32 {c}, 0"));
        let yes = self.label("differs");
        let next = self.label("same");
        self.term(&format!("br i1 {nz}, label %{yes}, label %{next}"));
        self.start(&yes);
        self.inst(&format!("store i32 {c}, ptr {result}"));
        self.term(&format!("br label %{done}"));
        self.start(&next);
    }

    fn cmp_call(&mut self, ty: &Ty, a: &str, b: &str) -> String {
        if matches!(ty, Ty::Int(_) | Ty::Bool | Ty::Float(_) | Ty::Dur | Ty::Str) {
            return self.gen_cmp(ty, a, b);
        }
        let h = self.helper(Helper::Cmp, ty);
        let t = self.tmp();
        self.inst(&format!("{t} = call i32 {h}(ptr {a}, ptr {b})"));
        t
    }

    // ---- hashing ----

    fn mix(&mut self, h: &str, x: &str) -> String {
        let t = self.tmp();
        self.inst(&format!("{t} = call i64 @ovt_hash_mix(i64 {h}, i64 {x})"));
        t
    }

    fn gen_hash(&mut self, ty: &Ty, p: &str) -> String {
        match ty {
            Ty::Int(k) => {
                let lt = self.lty(ty);
                let v = self.load(&lt, p);
                let x = if k.bits() < 64 {
                    let t = self.tmp();
                    let ins = if k.signed() { "sext" } else { "zext" };
                    self.inst(&format!("{t} = {ins} {} to i64", v.op()));
                    t
                } else {
                    v.repr
                };
                self.mix("0", &x)
            }
            Ty::Dur => {
                let v = self.load("i64", p);
                self.mix("0", &v.repr)
            }
            Ty::Bool => {
                let v = self.load("i1", p);
                let t = self.tmp();
                self.inst(&format!("{t} = zext i1 {} to i64", v.repr));
                self.mix("0", &t)
            }
            Ty::Float(k) => {
                let lt = self.lty(ty);
                let v = self.load(&lt, p);
                // -0.0 and 0.0 are equal, so they must hash the same.
                let z = self.tmp();
                self.inst(&format!("{z} = fadd {lt} {}, 0.0", v.repr));
                let bits = self.tmp();
                let x = match k {
                    FloatTy::F64 => {
                        self.inst(&format!("{bits} = bitcast double {z} to i64"));
                        bits
                    }
                    FloatTy::F32 => {
                        self.inst(&format!("{bits} = bitcast float {z} to i32"));
                        let t = self.tmp();
                        self.inst(&format!("{t} = zext i32 {bits} to i64"));
                        t
                    }
                };
                self.mix("0", &x)
            }
            Ty::Str => {
                let t = self.tmp();
                self.inst(&format!("{t} = call i64 @ovt_hash_bytes(ptr {p})"));
                t
            }
            Ty::Unit | Ty::Fn(_) => "0".into(),
            Ty::Array(e) => {
                let e = (**e).clone();
                let acc = self.alloca("i64");
                let a = self.load("%ovt.arr", p);
                let len = self.extract(&a, 2, "i64");
                self.inst(&format!("store i64 {}, ptr {acc}", len.repr));
                self.each_index(&len.repr, |g, i| {
                    let ep = g.elem_ptr(p, i, &e);
                    let h = g.hash_call(&e, &ep);
                    let cur = g.load("i64", &acc);
                    let m = g.mix(&cur.repr, &h);
                    g.inst(&format!("store i64 {m}, ptr {acc}"));
                });
                self.load("i64", &acc).repr
            }
            Ty::Opt(inner) => {
                let inner = (**inner).clone();
                let lt = self.lty(ty);
                let acc = self.alloca("i64");
                self.inst(&format!("store i64 0, ptr {acc}"));
                let tp = self.gep(&lt, p, &[0, 0]);
                let tag = self.load("i1", &tp);
                let ip = self.gep(&lt, p, &[0, 1]);
                self.when(&tag.repr, |g| {
                    let h = g.hash_call(&inner, &ip);
                    let m = g.mix("1", &h);
                    g.inst(&format!("store i64 {m}, ptr {acc}"));
                });
                self.load("i64", &acc).repr
            }
            Ty::Tuple(_) => {
                let acc = self.alloca("i64");
                self.inst(&format!("store i64 0, ptr {acc}"));
                self.hash_fields(ty, None, p, &acc);
                self.load("i64", &acc).repr
            }
            Ty::Adt(id, _) if self.is_payloadless_enum(*id) => {
                let v = self.load("i32", p);
                let t = self.tmp();
                self.inst(&format!("{t} = zext i32 {} to i64", v.repr));
                self.mix("0", &t)
            }
            Ty::Adt(id, _) if self.p.adts[*id].is_enum() => {
                let lt = self.lty(ty);
                let tp = self.gep(&lt, p, &[0, 0]);
                let tag = self.load("i32", &tp);
                let t = self.tmp();
                self.inst(&format!("{t} = zext i32 {} to i64", tag.repr));
                let acc = self.alloca("i64");
                let first = self.mix("0", &t);
                self.inst(&format!("store i64 {first}, ptr {acc}"));
                let tyc = ty.clone();
                let accc = acc.clone();
                self.per_variant(ty, p, |g, v, payload| g.hash_fields(&tyc, Some(v), payload, &accc));
                self.load("i64", &acc).repr
            }
            Ty::Adt(..) => {
                let acc = self.alloca("i64");
                self.inst(&format!("store i64 0, ptr {acc}"));
                self.hash_fields(ty, None, p, &acc);
                self.load("i64", &acc).repr
            }
            _ => "0".into(),
        }
    }

    fn hash_fields(&mut self, ty: &Ty, variant: Option<usize>, p: &str, acc: &str) {
        let (lt, fields) = self.components(ty, variant);
        for (i, (boxed, fty)) in fields.iter().enumerate() {
            let mut fp = self.gep(&lt, p, &[0, i]);
            if *boxed {
                fp = self.box_value_ptr(&fp);
            }
            let h = self.hash_call(fty, &fp);
            let cur = self.load("i64", acc);
            let m = self.mix(&cur.repr, &h);
            self.inst(&format!("store i64 {m}, ptr {acc}"));
        }
    }

    fn hash_call(&mut self, ty: &Ty, p: &str) -> String {
        if matches!(ty, Ty::Int(_) | Ty::Bool | Ty::Float(_) | Ty::Dur | Ty::Str) {
            return self.gen_hash(ty, p);
        }
        let h = self.helper(Helper::Hash, ty);
        let t = self.tmp();
        self.inst(&format!("{t} = call i64 {h}(ptr {p})"));
        t
    }

    // ---- showing ----

    pub fn lit(&mut self, sb: &str, s: &str) {
        let c = self.cstr(s.as_bytes());
        self.inst(&format!("call void @ovt_sb_cstr(ptr {sb}, ptr {c}, i64 {})", s.len()));
    }

    pub fn show_call(&mut self, ty: &Ty, p: &str, sb: &str) {
        let h = self.helper(Helper::Show, ty);
        self.inst(&format!("call void {h}(ptr {p}, ptr {sb})"));
    }

    pub fn gen_show(&mut self, ty: &Ty, p: &str, sb: &str) {
        match ty {
            Ty::Int(k) => {
                let lt = self.lty(ty);
                let v = self.load(&lt, p);
                let x = if k.bits() < 64 {
                    let t = self.tmp();
                    let ins = if k.signed() { "sext" } else { "zext" };
                    self.inst(&format!("{t} = {ins} {} to i64", v.op()));
                    t
                } else {
                    v.repr
                };
                let f = if k.signed() { "ovt_sb_int" } else { "ovt_sb_uint" };
                self.inst(&format!("call void @{f}(ptr {sb}, i64 {x})"));
            }
            Ty::Float(k) => {
                let lt = self.lty(ty);
                let v = self.load(&lt, p);
                let x = if *k == FloatTy::F32 {
                    let t = self.tmp();
                    self.inst(&format!("{t} = fpext float {} to double", v.repr));
                    t
                } else {
                    v.repr
                };
                self.inst(&format!("call void @ovt_sb_f64(ptr {sb}, double {x})"));
            }
            Ty::Dur => {
                let v = self.load("i64", p);
                self.inst(&format!("call void @ovt_sb_dur(ptr {sb}, i64 {})", v.repr));
            }
            Ty::Bool => {
                let v = self.load("i1", p);
                let t = self.cstr(b"true");
                let f = self.cstr(b"false");
                let s = self.tmp();
                self.inst(&format!("{s} = select i1 {}, ptr {t}, ptr {f}", v.repr));
                let n = self.tmp();
                self.inst(&format!("{n} = select i1 {}, i64 4, i64 5", v.repr));
                self.inst(&format!("call void @ovt_sb_cstr(ptr {sb}, ptr {s}, i64 {n})"));
            }
            Ty::Str => self.inst(&format!("call void @ovt_sb_quoted(ptr {sb}, ptr {p})")),
            Ty::Unit => self.lit(sb, "()"),
            Ty::Fn(_) => self.lit(sb, "<fn>"),
            Ty::Array(e) => {
                let e = (**e).clone();
                self.lit(sb, "[");
                let a = self.load("%ovt.arr", p);
                let len = self.extract(&a, 2, "i64");
                self.each_index(&len.repr, |g, i| {
                    let first = g.tmp();
                    g.inst(&format!("{first} = icmp eq i64 {i}, 0"));
                    let notfirst = g.tmp();
                    g.inst(&format!("{notfirst} = xor i1 {first}, true"));
                    g.when(&notfirst, |g| g.lit(sb, ", "));
                    let ep = g.elem_ptr(p, i, &e);
                    g.show_call(&e, &ep, sb);
                });
                self.lit(sb, "]");
            }
            Ty::Opt(inner) => {
                let inner = (**inner).clone();
                let lt = self.lty(ty);
                let tp = self.gep(&lt, p, &[0, 0]);
                let tag = self.load("i1", &tp);
                let yes = self.label("some");
                let no = self.label("none");
                let done = self.label("done");
                self.term(&format!("br i1 {}, label %{yes}, label %{no}", tag.repr));
                self.start(&yes);
                let ip = self.gep(&lt, p, &[0, 1]);
                self.show_call(&inner, &ip, sb);
                self.term(&format!("br label %{done}"));
                self.start(&no);
                self.lit(sb, "none");
                self.term(&format!("br label %{done}"));
                self.start(&done);
            }
            Ty::Tuple(ts) => {
                let ts = ts.clone();
                let lt = self.lty(ty);
                self.lit(sb, "(");
                for (i, t) in ts.iter().enumerate() {
                    if i > 0 {
                        self.lit(sb, ", ");
                    }
                    let fp = self.gep(&lt, p, &[0, i]);
                    self.show_call(t, &fp, sb);
                }
                self.lit(sb, ")");
            }
            Ty::Adt(id, targs) if *id == self.p.known.map || *id == self.p.known.set => {
                let is_set = *id == self.p.known.set;
                let (kty, vty, map_ty) = if is_set {
                    (targs[0].clone(), Ty::Bool, Ty::Adt(self.p.known.map, vec![targs[0].clone(), Ty::Bool]))
                } else {
                    (targs[0].clone(), targs[1].clone(), ty.clone())
                };
                let mp = if is_set {
                    let st = self.lty(ty);
                    self.gep(&st, p, &[0, 0])
                } else {
                    p.to_string()
                };
                let mlt = self.lty(&map_ty);
                let keys = self.gep(&mlt, &mp, &[0, 0]);
                let vals = self.gep(&mlt, &mp, &[0, 1]);
                let live = self.gep(&mlt, &mp, &[0, 2]);
                let ka = self.load("%ovt.arr", &keys);
                let len = self.extract(&ka, 2, "i64");
                let first = self.alloca("i1");
                self.inst(&format!("store i1 true, ptr {first}"));
                self.lit(sb, "{");
                self.each_index(&len.repr, |g, i| {
                    let lp = g.elem_ptr(&live, i, &Ty::Bool);
                    let alive = g.load("i1", &lp);
                    g.when(&alive.repr, |g| {
                        let f = g.load("i1", &first);
                        let nf = g.tmp();
                        g.inst(&format!("{nf} = xor i1 {}, true", f.repr));
                        g.when(&nf, |g| g.lit(sb, ", "));
                        g.inst(&format!("store i1 false, ptr {first}"));
                        let kp = g.elem_ptr(&keys, i, &kty);
                        g.show_call(&kty, &kp, sb);
                        if !is_set {
                            g.lit(sb, ": ");
                            let vp = g.elem_ptr(&vals, i, &vty);
                            g.show_call(&vty, &vp, sb);
                        }
                    });
                });
                self.lit(sb, "}");
            }
            Ty::Adt(id, _) if self.is_payloadless_enum(*id) => {
                let v = self.load("i32", p);
                let names: Vec<String> = self.p.adts[*id].variants().iter().map(|v| v.name.clone()).collect();
                let done = self.label("done");
                let labels: Vec<String> = names.iter().map(|_| self.label("variant")).collect();
                let cases: Vec<String> = labels.iter().enumerate().map(|(i, l)| format!("i32 {i}, label %{l}")).collect();
                self.term(&format!("switch i32 {}, label %{done} [ {} ]", v.repr, cases.join(" ")));
                for (l, n) in labels.iter().zip(&names) {
                    self.start(l);
                    self.lit(sb, n);
                    self.term(&format!("br label %{done}"));
                }
                self.start(&done);
            }
            Ty::Adt(id, _) if self.p.adts[*id].is_enum() => {
                let names: Vec<(String, Vec<String>)> =
                    self.p.adts[*id].variants().iter().map(|v| (v.name.clone(), v.fields.iter().map(|f| f.name.clone()).collect())).collect();
                let tyc = ty.clone();
                self.per_variant(ty, p, |g, v, payload| {
                    let (name, fields) = &names[v];
                    g.lit(sb, name);
                    if !fields.is_empty() {
                        g.lit(sb, "(");
                        g.show_fields(&tyc, Some(v), payload, sb, fields);
                        g.lit(sb, ")");
                    }
                });
            }
            Ty::Adt(id, _) => {
                let a = &self.p.adts[*id];
                let name = a.name.clone();
                let fields: Vec<String> = a.fields().iter().map(|f| f.name.clone()).collect();
                self.lit(sb, &name);
                self.lit(sb, "(");
                self.show_fields(ty, None, p, sb, &fields);
                self.lit(sb, ")");
            }
            _ => self.lit(sb, "?"),
        }
    }

    fn show_fields(&mut self, ty: &Ty, variant: Option<usize>, p: &str, sb: &str, names: &[String]) {
        let (lt, fields) = self.components(ty, variant);
        for (i, (boxed, fty)) in fields.iter().enumerate() {
            if i > 0 {
                self.lit(sb, ", ");
            }
            self.lit(sb, &format!("{}: ", names[i]));
            let mut fp = self.gep(&lt, p, &[0, i]);
            if *boxed {
                fp = self.box_value_ptr(&fp);
            }
            self.show_call(fty, &fp, sb);
        }
    }
}
