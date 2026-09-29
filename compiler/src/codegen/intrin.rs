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
    fn fail_with(&mut self, kind: &str, msg: V) {
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
    fn fail_on_code(&mut self, code: &str, err: &str) {
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
            "fs.exists" => {
                let p = self.spill(&params[0]);
                let r = self.tmp();
                self.inst(&format!("{r} = call i32 @ovt_fs_exists(ptr {p})"));
                let b = self.tmp();
                self.inst(&format!("{b} = icmp ne i32 {r}, 0"));
                self.emit_return(Some(V::new("i1", b)));
            }
            other => {
                // Builtins like `print` are handled by the checker and never called as functions.
                self.trap(&format!("internal error: no implementation for `{other}`"), span);
            }
        }
        let _ = (Self::some, Self::none_of);
    }
}
