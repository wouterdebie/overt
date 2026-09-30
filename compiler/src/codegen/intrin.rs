//! Bodies of standard library functions implemented by the compiler
//! (functions declared in std/*.ovt without a body).

use super::*;

impl<'p> Gen<'p> {
    /// The name an intrinsic is known by: `str.find`, `[T].push`, `os.args`.
    fn intrinsic_key(&self, f: FnId) -> String {
        let def = &self.p.fns[f];
        if def.name.contains('.') { def.name.clone() } else { format!("{}.{}", def.module, def.name) }
    }

    fn some(&mut self, ty: &Ty, v: &V) -> V {
        let lt = self.lty(&Ty::opt(ty.clone()));
        let o = V::new(lt, "undef");
        let o = self.insert(&o, &V::new("i1", "true"), 0);
        self.insert(&o, v, 1)
    }

    fn none_of(&mut self, ty: &Ty) -> V {
        let lt = self.lty(&Ty::opt(ty.clone()));
        V::new(lt, "zeroinitializer")
    }

    /// Fails with `kind` (an ErrKind index, maybe dynamic) and an owned message.
    pub fn fail_with(&mut self, kind: &str, msg: V) {
        let et = self.err_lty();
        let e = V::new(et, "undef");
        let e = self.insert(&e, &V::new("i32", kind), 0);
        let e = self.insert(&e, &msg, 1);
        self.emit_fail(e);
    }

    /// A message: `prefix` followed by the string at `s`, quoted.
    fn quoted_msg(&mut self, prefix: &str, s: &str) -> V {
        let sb = self.alloca("%ovt.arr");
        self.inst(&format!("store %ovt.arr zeroinitializer, ptr {sb}"));
        let c = self.cstr(prefix.as_bytes());
        self.inst(&format!("call void @ovt_sb_cstr(ptr {sb}, ptr {c}, i64 {})", prefix.len()));
        self.inst(&format!("call void @ovt_sb_quoted(ptr {sb}, ptr {s})"));
        self.load("%ovt.arr", &sb)
    }

    /// Calls a runtime function returning 0 for success or an ErrKind + 1,
    /// with the message in `err`; fails on error.
    pub fn fail_on_code(&mut self, code: &str, err: &str) {
        let bad = self.tmp();
        self.inst(&format!("{bad} = icmp ne i32 {code}, 0"));
        let fail = self.label("failed");
        let ok = self.label("ok");
        self.term(&format!("br i1 {bad}, label %{fail}, label %{ok}"));
        self.start(&fail);
        let kind = self.tmp();
        self.inst(&format!("{kind} = sub i32 {code}, 1"));
        let msg = self.load("%ovt.arr", err);
        self.fail_with(&kind, msg);
        self.start(&ok);
    }

    pub fn intrinsic_body(&mut self, f: FnId, params: &[V]) {
        let key = self.intrinsic_key(f);
        let fspan = self.p.fns[f].span;
        if self.handle_intrinsic(f, &key, params, fspan) || self.json_intrinsic(&key, params) {
            return;
        }
        let elem = self.f.targs.first().cloned().unwrap_or(Ty::Error);
        let span = self.p.fns[f].span;
        let arr_fns = |g: &mut Self| {
            let size = g.size_const(&elem);
            let dup = g.dup_fn(&elem);
            let drop = g.drop_fn(&elem);
            (size, dup, drop)
        };
        match key.as_str() {
            "str.len" | "[T].len" => {
                let l = self.extract(&params[0], 2, "i64");
                self.emit_return(Some(l));
            }
            "str.bytes" => {
                self.dup_value(&params[0], &Ty::Str);
                self.emit_return(Some(params[0].clone()));
            }
            "str.from_bytes" => {
                let bp = self.spill(&params[0]);
                let out = self.alloca("%ovt.arr");
                let ok = self.tmp();
                self.inst(&format!("{ok} = call i32 @ovt_str_from_bytes(ptr {out}, ptr {bp})"));
                let bad = self.tmp();
                self.inst(&format!("{bad} = icmp eq i32 {ok}, 0"));
                let fail = self.label("invalid");
                let good = self.label("valid");
                self.term(&format!("br i1 {bad}, label %{fail}, label %{good}"));
                self.start(&fail);
                let msg = self.str_const(b"the bytes aren't valid UTF-8");
                self.fail_with("0", msg);
                self.start(&good);
                let v = self.load("%ovt.arr", &out);
                self.emit_return(Some(v));
            }
            "str.find" => {
                let a = self.spill(&params[0]);
                let b = self.spill(&params[1]);
                let r = self.tmp();
                self.inst(&format!("{r} = call i64 @ovt_str_find(ptr {a}, ptr {b})"));
                let found = self.tmp();
                self.inst(&format!("{found} = icmp sge i64 {r}, 0"));
                let lt = self.lty(&Ty::opt(Ty::INT));
                let o = V::new(lt, "undef");
                let o = self.insert(&o, &V::new("i1", found), 0);
                let o = self.insert(&o, &V::new("i64", r), 1);
                self.emit_return(Some(o));
            }
            "str.to_lower" | "str.to_upper" => {
                let a = self.spill(&params[0]);
                let out = self.alloca("%ovt.arr");
                let up = if key == "str.to_upper" { 1 } else { 0 };
                self.inst(&format!("call void @ovt_str_case(ptr {out}, ptr {a}, i32 {up})"));
                let v = self.load("%ovt.arr", &out);
                self.emit_return(Some(v));
            }
            "str.runes" => {
                let a = self.spill(&params[0]);
                let out = self.alloca("%ovt.arr");
                self.inst(&format!("call void @ovt_str_runes(ptr {out}, ptr {a})"));
                let v = self.load("%ovt.arr", &out);
                self.emit_return(Some(v));
            }
            "[T].push" => {
                let (size, dup, drop) = arr_fns(self);
                let ap = params[0].repr.clone();
                // Fast path: the only view of the whole buffer, with room left.
                let fast = self.label("fast");
                let slow = self.label("slow");
                let (buf, used) = self.unique_check(&ap, &fast, &slow, true);
                self.start(&fast);
                let bytes = self.tmp();
                self.inst(&format!("{bytes} = mul i64 {used}, {size}"));
                let at = self.tmp();
                self.inst(&format!("{at} = add i64 {bytes}, 24"));
                let slot = self.tmp();
                self.inst(&format!("{slot} = getelementptr inbounds i8, ptr {buf}, i64 {at}"));
                self.inst(&format!("store {}, ptr {slot}", params[1].op()));
                let n = self.tmp();
                self.inst(&format!("{n} = add i64 {used}, 1"));
                let up = self.tmp();
                self.inst(&format!("{up} = getelementptr inbounds i8, ptr {buf}, i64 16"));
                self.inst(&format!("store i64 {n}, ptr {up}"));
                let lp = self.gep("%ovt.arr", &ap, &[0, 2]);
                self.inst(&format!("store i64 {n}, ptr {lp}"));
                self.emit_return(None);
                self.start(&slow);
                let slot = self.tmp();
                self.inst(&format!("{slot} = call ptr @ovt_arr_push(ptr {ap}, i64 {size}, ptr {dup}, ptr {drop})"));
                self.inst(&format!("store {}, ptr {slot}", params[1].op()));
                self.emit_return(None);
            }
            "[T].pop" => {
                let (size, dup, drop) = arr_fns(self);
                let lt = self.lty(&elem);
                let out = self.alloca(&lt);
                let ok = self.tmp();
                self.inst(&format!("{ok} = call i32 @ovt_arr_pop(ptr {}, i64 {size}, ptr {dup}, ptr {drop}, ptr {out})", params[0].repr));
                let got = self.tmp();
                self.inst(&format!("{got} = icmp ne i32 {ok}, 0"));
                let v = self.load(&lt, &out);
                let olt = self.lty(&Ty::opt(elem.clone()));
                let o = V::new(olt, "zeroinitializer");
                let o = self.insert(&o, &V::new("i1", got), 0);
                let o = self.insert(&o, &v, 1);
                self.emit_return(Some(o));
            }
            "[T].insert" => {
                let (size, dup, drop) = arr_fns(self);
                let loc = self.loc_args(span);
                let slot = self.tmp();
                self.inst(&format!("{slot} = call ptr @ovt_arr_insert(ptr {}, i64 {}, i64 {size}, ptr {dup}, ptr {drop}, {loc})", params[0].repr, params[1].repr));
                self.inst(&format!("store {}, ptr {slot}", params[2].op()));
                self.emit_return(None);
            }
            "[T].remove" => {
                let (size, dup, drop) = arr_fns(self);
                let lt = self.lty(&elem);
                let out = self.alloca(&lt);
                let loc = self.loc_args(span);
                self.inst(&format!("call void @ovt_arr_remove(ptr {}, i64 {}, i64 {size}, ptr {dup}, ptr {drop}, ptr {out}, {loc})", params[0].repr, params[1].repr));
                let v = self.load(&lt, &out);
                self.emit_return(Some(v));
            }
            "[T].clear" => {
                let (size, _, drop) = arr_fns(self);
                self.inst(&format!("call void @ovt_arr_clear(ptr {}, i64 {size}, ptr {drop})", params[0].repr));
                self.emit_return(None);
            }
            "[T].sort" => {
                let (size, dup, drop) = arr_fns(self);
                let cmp = self.helper(Helper::Cmp, &elem);
                self.inst(&format!("call void @ovt_arr_sort(ptr {}, i64 {size}, ptr {dup}, ptr {drop}, ptr {cmp})", params[0].repr));
                self.emit_return(None);
            }
            "[T].reverse" => {
                let (size, dup, drop) = arr_fns(self);
                self.inst(&format!("call void @ovt_arr_reverse(ptr {}, i64 {size}, ptr {dup}, ptr {drop})", params[0].repr));
                self.emit_return(None);
            }
            "int.parse" | "f64.parse" => {
                let s = self.spill(&params[0]);
                let (lt, rt, what) = if key == "int.parse" { ("i64", "@ovt_parse_int", "not an integer: ") } else { ("double", "@ovt_parse_f64", "not a number: ") };
                let out = self.alloca(lt);
                let ok = self.tmp();
                self.inst(&format!("{ok} = call i32 {rt}(ptr {s}, ptr {out})"));
                let bad = self.tmp();
                self.inst(&format!("{bad} = icmp eq i32 {ok}, 0"));
                let fail = self.label("invalid");
                let good = self.label("valid");
                self.term(&format!("br i1 {bad}, label %{fail}, label %{good}"));
                self.start(&fail);
                let msg = self.quoted_msg(what, &s);
                self.fail_with("0", msg);
                self.start(&good);
                let v = self.load(lt, &out);
                self.emit_return(Some(v));
            }
            "f64.abs" | "f64.sqrt" | "f64.floor" | "f64.ceil" | "f64.round" => {
                let name = match key.as_str() {
                    "f64.abs" => "fabs",
                    "f64.sqrt" => "sqrt",
                    "f64.floor" => "floor",
                    "f64.ceil" => "ceil",
                    _ => "round",
                };
                let t = self.tmp();
                self.inst(&format!("{t} = call double @llvm.{name}.f64(double {})", params[0].repr));
                self.emit_return(Some(V::new("double", t)));
            }
            "os.args" => {
                let out = self.alloca("%ovt.arr");
                self.inst(&format!("call void @ovt_os_args(ptr {out})"));
                let v = self.load("%ovt.arr", &out);
                self.emit_return(Some(v));
            }
            "os.env" => {
                let n = self.spill(&params[0]);
                let out = self.alloca("%ovt.arr");
                self.inst(&format!("store %ovt.arr zeroinitializer, ptr {out}"));
                let ok = self.tmp();
                self.inst(&format!("{ok} = call i32 @ovt_os_env(ptr {n}, ptr {out})"));
                let got = self.tmp();
                self.inst(&format!("{got} = icmp ne i32 {ok}, 0"));
                let v = self.load("%ovt.arr", &out);
                let olt = self.lty(&Ty::opt(Ty::Str));
                let o = V::new(olt, "zeroinitializer");
                let o = self.insert(&o, &V::new("i1", got), 0);
                let o = self.insert(&o, &v, 1);
                self.emit_return(Some(o));
            }
            "os.read_stdin" | "os.read_stdin_bytes" => {
                let out = self.alloca("%ovt.arr");
                let err = self.alloca("%ovt.arr");
                let text = if key == "os.read_stdin" { 1 } else { 0 };
                let code = self.tmp();
                self.inst(&format!("{code} = call i32 @ovt_read_stdin(ptr {out}, i32 {text}, ptr {err})"));
                self.fail_on_code(&code, &err);
                let v = self.load("%ovt.arr", &out);
                self.emit_return(Some(v));
            }
            "os.exit" => {
                self.inst(&format!("call void @ovt_os_exit(i64 {})", params[0].repr));
                self.term("unreachable");
            }
            "fs.read" | "fs.read_bytes" => {
                let p = self.spill(&params[0]);
                let out = self.alloca("%ovt.arr");
                let err = self.alloca("%ovt.arr");
                let text = if key == "fs.read" { 1 } else { 0 };
                let code = self.tmp();
                self.inst(&format!("{code} = call i32 @ovt_fs_read(ptr {p}, ptr {out}, i32 {text}, ptr {err})"));
                self.fail_on_code(&code, &err);
                let v = self.load("%ovt.arr", &out);
                self.emit_return(Some(v));
            }
            "fs.write" => {
                let p = self.spill(&params[0]);
                let d = self.spill(&params[1]);
                let err = self.alloca("%ovt.arr");
                let code = self.tmp();
                self.inst(&format!("{code} = call i32 @ovt_fs_write(ptr {p}, ptr {d}, ptr {err})"));
                self.fail_on_code(&code, &err);
                self.emit_return(None);
            }
            "fs.walk" | "fs.list" => {
                let p = self.spill(&params[0]);
                let out = self.alloca("%ovt.arr");
                let err = self.alloca("%ovt.arr");
                let deep = if key == "fs.walk" { 1 } else { 0 };
                let code = self.tmp();
                self.inst(&format!("{code} = call i32 @ovt_fs_list(ptr {p}, i32 {deep}, ptr {out}, ptr {err})"));
                self.fail_on_code(&code, &err);
                let v = self.load("%ovt.arr", &out);
                self.emit_return(Some(v));
            }
            "fs.exists" => {
                let p = self.spill(&params[0]);
                let r = self.tmp();
                self.inst(&format!("{r} = call i32 @ovt_fs_exists(ptr {p})"));
                let b = self.tmp();
                self.inst(&format!("{b} = icmp ne i32 {r}, 0"));
                self.emit_return(Some(V::new("i1", b)));
            }
            "task.map" => self.task_map(params),
            // An `Atomic` is a struct holding one closure-like value, whose
            // environment {count, drop, mark, value} holds the number.
            "Atomic.new" => {
                let env = self.tmp();
                self.inst(&format!("{env} = call ptr @ovt_alloc(i64 32)"));
                self.inst(&format!("store i64 1, ptr {env}"));
                for (off, f) in [(8, "@ovt_free"), (16, "@ovt_mark_none")] {
                    let p = self.tmp();
                    self.inst(&format!("{p} = getelementptr inbounds i8, ptr {env}, i64 {off}"));
                    self.inst(&format!("store ptr {f}, ptr {p}"));
                }
                let vp = self.tmp();
                self.inst(&format!("{vp} = getelementptr inbounds i8, ptr {env}, i64 24"));
                self.inst(&format!("store {}, ptr {vp}", params[0].op()));
                let ret = self.f.ret.clone();
                let lt = self.lty(&ret);
                let cell = self.insert(&V::new("%ovt.fn", "{ ptr null, ptr null }"), &V::new("ptr", env), 1);
                let a = self.insert(&V::new(lt, "undef"), &cell, 0);
                self.emit_return(Some(a));
            }
            "Atomic.load" | "Atomic.store" | "Atomic.add" => {
                let cell = self.extract(&params[0], 0, "%ovt.fn");
                let env = self.extract(&cell, 1, "ptr");
                let vp = self.tmp();
                self.inst(&format!("{vp} = getelementptr inbounds i8, ptr {}, i64 24", env.repr));
                match key.as_str() {
                    "Atomic.load" => {
                        let v = self.tmp();
                        self.inst(&format!("{v} = load atomic i64, ptr {vp} seq_cst, align 8"));
                        self.emit_return(Some(V::new("i64", v)));
                    }
                    "Atomic.store" => {
                        self.inst(&format!("store atomic {}, ptr {vp} seq_cst, align 8", params[1].op()));
                        self.emit_return(None);
                    }
                    _ => {
                        let old = self.tmp();
                        self.inst(&format!("{old} = atomicrmw add ptr {vp}, {} seq_cst", params[1].op()));
                        let new = self.tmp();
                        self.inst(&format!("{new} = add i64 {old}, {}", params[1].repr));
                        self.emit_return(Some(V::new("i64", new)));
                    }
                }
            }
            other => {
                // Builtins like `print` are handled by the checker and never called as functions.
                self.trap(&format!("internal error: no implementation for `{other}`"), span);
            }
        }
        let _ = (Self::some, Self::none_of);
    }

    // ---- task.map ----
    //
    // `task.map(xs, f)` marks `xs` and `f` shared, then has the runtime run
    // the body for every index on several tasks. Each body call stores its
    // result, or its error, in a slot of its own, and sets a status byte:
    // 1 for a result, 2 for an error. The context the bodies get is
    // `{xs, f, results, errors, status}`.

    const MAP_CTX: &'static str = "{ %ovt.arr, %ovt.fn, ptr, ptr, ptr }";

    fn task_map(&mut self, params: &[V]) {
        let t = self.f.targs[0].clone();
        let u = self.f.targs[1].clone();
        let failable = self.f.failable;
        let xs = self.spill(&params[0]);
        let mark = self.mark_fn(&Ty::array(t.clone()));
        self.inst(&format!("call void {mark}(ptr {xs})"));
        let env = self.extract(&params[1], 1, "ptr");
        self.inst(&format!("call void @ovt_mark_env(ptr {})", env.repr));
        let n = self.extract(&params[0], 2, "i64");
        let usize = self.size_const(&u);
        let buf = self.tmp();
        self.inst(&format!("{buf} = call ptr @ovt_buf_new(i64 {}, i64 {usize})", n.repr));
        let data = self.tmp();
        self.inst(&format!("{data} = getelementptr inbounds i8, ptr {buf}, i64 24"));
        let status = self.tmp();
        self.inst(&format!("{status} = call ptr @ovt_calloc(i64 {})", n.repr));
        let et = self.err_lty();
        let errs = if failable {
            let esize = self.tmp();
            self.inst(&format!("{esize} = ptrtoint ptr getelementptr ({et}, ptr null, i32 1) to i64"));
            let bytes = self.tmp();
            self.inst(&format!("{bytes} = mul i64 {esize}, {}", n.repr));
            let e = self.tmp();
            self.inst(&format!("{e} = call ptr @ovt_alloc(i64 {bytes})"));
            e
        } else {
            "null".to_string()
        };
        let ctx = self.alloca(Self::MAP_CTX);
        for (i, (lt, v)) in [("%ovt.arr", params[0].repr.clone()), ("%ovt.fn", params[1].repr.clone()), ("ptr", data.clone()), ("ptr", errs.clone()), ("ptr", status.clone())].into_iter().enumerate() {
            let p = self.gep(Self::MAP_CTX, &ctx, &[0, i]);
            self.inst(&format!("store {lt} {v}, ptr {p}"));
        }
        let body = self.map_body(&t, &u, failable);
        let r = self.tmp();
        self.inst(&format!("{r} = call i64 @ovt_parallel(i64 {}, ptr {body}, ptr {ctx})", n.repr));
        let ok = self.label("mapped");
        if failable {
            let bad = self.tmp();
            self.inst(&format!("{bad} = icmp ne i64 {r}, -1"));
            let fail = self.label("map_failed");
            self.term(&format!("br i1 {bad}, label %{fail}, label %{ok}"));
            self.start(&fail);
            let drop = self.drop_fn(&u);
            self.inst(&format!("call void @ovt_parallel_cleanup(ptr {status}, i64 {}, ptr {data}, i64 {usize}, ptr {drop}, ptr {errs}, i64 {r})", n.repr));
            let errp = self.tmp();
            self.inst(&format!("{errp} = getelementptr inbounds {et}, ptr {errs}, i64 {r}"));
            let err = self.load(&et, &errp);
            self.inst(&format!("call void @ovt_free(ptr {status})"));
            self.inst(&format!("call void @ovt_free(ptr {errs})"));
            // Nothing is in the buffer yet, so this just frees it.
            self.inst(&format!("call void @ovt_buf_release(ptr {buf}, i64 {usize}, ptr null)"));
            self.emit_fail(err);
        } else {
            self.term(&format!("br label %{ok}"));
        }
        self.start(&ok);
        let usedp = self.tmp();
        self.inst(&format!("{usedp} = getelementptr inbounds i8, ptr {buf}, i64 16"));
        self.inst(&format!("store i64 {}, ptr {usedp}", n.repr));
        self.inst(&format!("call void @ovt_free(ptr {status})"));
        if failable {
            self.inst(&format!("call void @ovt_free(ptr {errs})"));
        }
        let a = V::new("%ovt.arr", "undef");
        let a = self.insert(&a, &V::new("ptr", buf), 0);
        let a = self.insert(&a, &V::new("i64", "0"), 1);
        let a = self.insert(&a, &n, 2);
        self.emit_return(Some(a));
    }

    /// `i32 body(ptr ctx, i64 i)`: calls `f(xs[i])` and stores what it gives.
    pub fn emit_map_body(&mut self, t: &Ty, u: &Ty, failable: bool, sym: &str) -> String {
        self.f = Fx { cur: "entry".into(), ..Default::default() };
        let ctx = Self::MAP_CTX;
        let xsp = self.gep(ctx, "%ctx", &[0, 0]);
        let fp = self.gep(ctx, "%ctx", &[0, 1]);
        let f = self.load("%ovt.fn", &fp);
        let mut slots = Vec::new();
        for i in 2..5 {
            let p = self.gep(ctx, "%ctx", &[0, i]);
            slots.push(self.load("ptr", &p).repr);
        }
        let (data, errs, status) = (slots[0].clone(), slots[1].clone(), slots[2].clone());
        let ep = self.elem_ptr(&xsp, "%i", t);
        let tl = self.lty(t);
        let x = self.load(&tl, &ep);
        let func = self.extract(&f, 0, "ptr");
        let env = self.extract(&f, 1, "ptr");
        let stp = self.tmp();
        self.inst(&format!("{stp} = getelementptr inbounds i8, ptr {status}, i64 %i"));
        let rt = self.ret_lty(u, failable);
        let ul = self.lty(u);
        let value = if rt == "void" {
            self.inst(&format!("call void {}(ptr {}, {})", func.repr, env.repr, x.op()));
            None
        } else {
            let r = self.tmp();
            self.inst(&format!("{r} = call {rt} {}(ptr {}, {})", func.repr, env.repr, x.op()));
            let r = V::new(rt.clone(), r);
            if failable {
                let failed = self.extract(&r, 0, "i1");
                let bad = self.label("failed");
                let ok = self.label("ok");
                self.term(&format!("br i1 {}, label %{bad}, label %{ok}", failed.repr));
                self.start(&bad);
                let et = self.err_lty();
                let e = self.extract(&r, 2, &et);
                let errp = self.tmp();
                self.inst(&format!("{errp} = getelementptr inbounds {et}, ptr {errs}, i64 %i"));
                self.inst(&format!("store {}, ptr {errp}", e.op()));
                self.inst(&format!("store i8 2, ptr {stp}"));
                self.term("ret i32 1");
                self.start(&ok);
                Some(self.extract(&r, 1, &ul))
            } else {
                Some(r)
            }
        };
        if let Some(v) = value {
            if ul != "{}" {
                let up = self.tmp();
                self.inst(&format!("{up} = getelementptr inbounds {ul}, ptr {data}, i64 %i"));
                self.inst(&format!("store {}, ptr {up}", v.op()));
            }
        }
        self.inst(&format!("store i8 1, ptr {stp}"));
        self.term("ret i32 0");
        let f = std::mem::take(&mut self.f);
        format!("define internal i32 {sym}(ptr %ctx, i64 %i) {{\nentry:\n{}{}}}\n", f.allocas, f.code)
    }
}
