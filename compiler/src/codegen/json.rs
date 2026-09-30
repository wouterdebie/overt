//! `json.encode` and `json.decode`, generated for each type as helpers:
//! `jsonenc.T(ptr value, ptr builder)` appends the value's JSON to a string
//! being built, and `jsondec.T(ptr scanner, ptr out) -> i1` reads one value
//! with the runtime's JSON scanner (`ovt_jp_*` in runtime/rt.c), writing it
//! to `out` only when it succeeds. A decoder that fails has recorded the
//! error in the scanner and freed what it built.
//!
//! The JSON forms: numbers, `true`/`false` and strings as themselves; `none`
//! as `null`; arrays, sets and tuples as arrays; `Map[str, V]` and structs as
//! objects; a variant without fields as its name, and one with fields as
//! `{"Name": {fields}}`. Decoding ignores unknown keys, and a missing or
//! `null` field takes its default, or `none` for an optional.

use super::*;

/// `ErrKind.Invalid`, in declaration order in std/prelude.ovt.
const KIND_INVALID: &str = "0";

impl<'p> Gen<'p> {
    /// The bodies of `json.encode` and `json.decode`; `false` for other intrinsics.
    pub fn json_intrinsic(&mut self, key: &str, params: &[V]) -> bool {
        match key {
            "json.encode" => {
                let t = self.f.targs[0].clone();
                let p = self.spill(&params[0]);
                let sb = self.alloca("%ovt.arr");
                self.inst(&format!("store %ovt.arr zeroinitializer, ptr {sb}"));
                self.json_enc_call(&t, &p, &sb);
                let v = self.load("%ovt.arr", &sb);
                self.emit_return(Some(v));
            }
            "json.decode" => {
                let t = self.f.targs[0].clone();
                let s = self.spill(&params[0]);
                let jp = self.tmp();
                self.inst(&format!("{jp} = call ptr @ovt_jp_new(ptr {s})"));
                let lt = self.lty(&t);
                let out = self.alloca(&lt);
                let ok = self.json_dec_call(&t, &jp, &out);
                let ok32 = self.tmp();
                self.inst(&format!("{ok32} = zext i1 {ok} to i32"));
                let err = self.alloca("%ovt.arr");
                let bad = self.tmp();
                self.inst(&format!("{bad} = call i32 @ovt_jp_finish(ptr {jp}, i32 {ok32}, ptr {err})"));
                let failed = self.tmp();
                self.inst(&format!("{failed} = icmp ne i32 {bad}, 0"));
                let fail = self.label("invalid");
                let good = self.label("valid");
                self.term(&format!("br i1 {failed}, label %{fail}, label %{good}"));
                self.start(&fail);
                // Decoded, but something follows the value.
                self.when(&ok, |g| g.drop_ptr(&out, &t));
                let msg = self.load("%ovt.arr", &err);
                self.fail_with(KIND_INVALID, msg);
                self.start(&good);
                let v = self.load(&lt, &out);
                self.emit_return(Some(v));
            }
            _ => return false,
        }
        true
    }

    pub fn json_enc_call(&mut self, ty: &Ty, p: &str, sb: &str) {
        let h = self.helper(Helper::JsonEnc, ty);
        self.inst(&format!("call void {h}(ptr {p}, ptr {sb})"));
    }

    /// Decodes a `ty` into `out`; the `i1` success flag.
    pub fn json_dec_call(&mut self, ty: &Ty, jp: &str, out: &str) -> String {
        let h = self.helper(Helper::JsonDec, ty);
        let ok = self.tmp();
        self.inst(&format!("{ok} = call i1 {h}(ptr {jp}, ptr {out})"));
        ok
    }

    /// `, ` between elements: appends `,` unless `i` is 0.
    fn json_comma(&mut self, i: &str, sb: &str) {
        let later = self.tmp();
        self.inst(&format!("{later} = icmp ne i64 {i}, 0"));
        self.when(&later, |g| g.lit(sb, ","));
    }

    // ---- encoding ----

    pub fn gen_json_enc(&mut self, ty: &Ty, p: &str, sb: &str) {
        match ty {
            Ty::Int(_) | Ty::Bool => self.gen_show(ty, p, sb),
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
                self.inst(&format!("call void @ovt_sb_json_f64(ptr {sb}, double {x})"));
            }
            Ty::Str => self.inst(&format!("call void @ovt_sb_json_str(ptr {sb}, ptr {p})")),
            Ty::Array(e) => {
                let e = (**e).clone();
                self.lit(sb, "[");
                let a = self.load("%ovt.arr", p);
                let len = self.extract(&a, 2, "i64");
                self.each_index(&len.repr, |g, i| {
                    g.json_comma(i, sb);
                    let ep = g.elem_ptr(p, i, &e);
                    g.json_enc_call(&e, &ep, sb);
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
                self.json_enc_call(&inner, &ip, sb);
                self.term(&format!("br label %{done}"));
                self.start(&no);
                self.lit(sb, "null");
                self.term(&format!("br label %{done}"));
                self.start(&done);
            }
            Ty::Tuple(ts) => {
                let ts = ts.clone();
                let lt = self.lty(ty);
                self.lit(sb, "[");
                for (i, t) in ts.iter().enumerate() {
                    if i > 0 {
                        self.lit(sb, ",");
                    }
                    let fp = self.gep(&lt, p, &[0, i]);
                    self.json_enc_call(t, &fp, sb);
                }
                self.lit(sb, "]");
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
                self.lit(sb, if is_set { "[" } else { "{" });
                self.each_index(&len.repr, |g, i| {
                    let lp = g.elem_ptr(&live, i, &Ty::Bool);
                    let alive = g.load("i1", &lp);
                    g.when(&alive.repr, |g| {
                        let f = g.load("i1", &first);
                        let nf = g.tmp();
                        g.inst(&format!("{nf} = xor i1 {}, true", f.repr));
                        g.when(&nf, |g| g.lit(sb, ","));
                        g.inst(&format!("store i1 false, ptr {first}"));
                        let kp = g.elem_ptr(&keys, i, &kty);
                        g.json_enc_call(&kty, &kp, sb);
                        if !is_set {
                            g.lit(sb, ":");
                            let vp = g.elem_ptr(&vals, i, &vty);
                            g.json_enc_call(&vty, &vp, sb);
                        }
                    });
                });
                self.lit(sb, if is_set { "]" } else { "}" });
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
                    self.lit(sb, &format!("\"{n}\""));
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
                    if fields.is_empty() {
                        g.lit(sb, &format!("\"{name}\""));
                    } else {
                        g.lit(sb, &format!("{{\"{name}\":{{"));
                        g.json_enc_fields(&tyc, Some(v), payload, sb, fields);
                        g.lit(sb, "}}");
                    }
                });
            }
            Ty::Adt(id, _) => {
                let fields: Vec<String> = self.p.adts[*id].fields().iter().map(|f| f.name.clone()).collect();
                self.lit(sb, "{");
                self.json_enc_fields(ty, None, p, sb, &fields);
                self.lit(sb, "}");
            }
            // Types without a JSON form don't satisfy `Json`, so they can't get here.
            _ => self.lit(sb, "null"),
        }
    }

    fn json_enc_fields(&mut self, ty: &Ty, variant: Option<usize>, p: &str, sb: &str, names: &[String]) {
        let (lt, fields) = self.components(ty, variant);
        for (i, (boxed, fty)) in fields.iter().enumerate() {
            let sep = if i > 0 { "," } else { "" };
            self.lit(sb, &format!("{sep}\"{}\":", names[i]));
            let mut fp = self.gep(&lt, p, &[0, i]);
            if *boxed {
                fp = self.box_value_ptr(&fp);
            }
            self.json_enc_call(fty, &fp, sb);
        }
    }

    // ---- decoding ----

    /// Returns `false` from the decoder unless `ok` holds.
    fn or_fail(&mut self, ok: &str) {
        let bad = self.tmp();
        self.inst(&format!("{bad} = xor i1 {ok}, true"));
        self.when(&bad, |g| g.term("ret i1 false"));
    }

    /// `r` (an `i32` from `ovt_jp_*`) is nonzero.
    fn nonzero(&mut self, r: &str) -> String {
        let b = self.tmp();
        self.inst(&format!("{b} = icmp ne i32 {r}, 0"));
        b
    }

    /// Branches on `ovt_jp_next`/`ovt_jp_key`'s result: 1 to `more`, 0 to `end`, -1 to `bad`.
    fn json_step(&mut self, r: &str, more: &str, end: &str, bad: &str) {
        self.term(&format!("switch i32 {r}, label %{bad} [ i32 1, label %{more} i32 0, label %{end} ]"));
    }

    /// Decodes into `out` and returns the success flag, which is the decoder's result.
    pub fn gen_json_dec(&mut self, ty: &Ty, jp: &str, out: &str) -> String {
        match ty {
            Ty::Int(k) => {
                let slot = self.alloca("i64");
                let r = self.tmp();
                if *k == IntTy::U64 {
                    self.inst(&format!("{r} = call i32 @ovt_jp_uint(ptr {jp}, ptr {slot})"));
                } else {
                    let (lo, hi): (i64, i64) = match k {
                        IntTy::I8 => (i8::MIN as i64, i8::MAX as i64),
                        IntTy::I16 => (i16::MIN as i64, i16::MAX as i64),
                        IntTy::I32 => (i32::MIN as i64, i32::MAX as i64),
                        IntTy::U8 => (0, u8::MAX as i64),
                        IntTy::U16 => (0, u16::MAX as i64),
                        IntTy::U32 => (0, u32::MAX as i64),
                        _ => (i64::MIN, i64::MAX),
                    };
                    self.inst(&format!("{r} = call i32 @ovt_jp_int(ptr {jp}, ptr {slot}, i64 {lo}, i64 {hi})"));
                }
                let ok = self.nonzero(&r);
                self.or_fail(&ok);
                let v = self.load("i64", &slot);
                let lt = self.lty(ty);
                if lt == "i64" {
                    self.inst(&format!("store i64 {}, ptr {out}", v.repr));
                } else {
                    let t = self.tmp();
                    self.inst(&format!("{t} = trunc i64 {} to {lt}", v.repr));
                    self.inst(&format!("store {lt} {t}, ptr {out}"));
                }
                "true".into()
            }
            Ty::Float(k) => {
                let slot = self.alloca("double");
                let r = self.tmp();
                self.inst(&format!("{r} = call i32 @ovt_jp_f64(ptr {jp}, ptr {slot})"));
                let ok = self.nonzero(&r);
                self.or_fail(&ok);
                let v = self.load("double", &slot);
                if *k == FloatTy::F32 {
                    let t = self.tmp();
                    self.inst(&format!("{t} = fptrunc double {} to float", v.repr));
                    self.inst(&format!("store float {t}, ptr {out}"));
                } else {
                    self.inst(&format!("store double {}, ptr {out}", v.repr));
                }
                "true".into()
            }
            Ty::Bool => {
                let r = self.tmp();
                self.inst(&format!("{r} = call i32 @ovt_jp_bool(ptr {jp})"));
                let ok = self.tmp();
                self.inst(&format!("{ok} = icmp sge i32 {r}, 0"));
                self.or_fail(&ok);
                let b = self.tmp();
                self.inst(&format!("{b} = icmp eq i32 {r}, 1"));
                self.inst(&format!("store i1 {b}, ptr {out}"));
                "true".into()
            }
            Ty::Str => {
                let r = self.tmp();
                self.inst(&format!("{r} = call i32 @ovt_jp_str(ptr {jp}, ptr {out})"));
                self.nonzero(&r)
            }
            Ty::Opt(inner) => {
                let inner = (**inner).clone();
                let lt = self.lty(ty);
                let r = self.tmp();
                self.inst(&format!("{r} = call i32 @ovt_jp_null(ptr {jp})"));
                let is_null = self.nonzero(&r);
                let null = self.label("null");
                let value = self.label("value");
                self.term(&format!("br i1 {is_null}, label %{null}, label %{value}"));
                self.start(&null);
                self.inst(&format!("store {lt} zeroinitializer, ptr {out}"));
                self.term("ret i1 true");
                self.start(&value);
                let ip = self.gep(&lt, out, &[0, 1]);
                let ok = self.json_dec_call(&inner, jp, &ip);
                self.or_fail(&ok);
                let tp = self.gep(&lt, out, &[0, 0]);
                self.inst(&format!("store i1 true, ptr {tp}"));
                "true".into()
            }
            Ty::Array(e) => {
                let e = (**e).clone();
                self.json_dec_seq(jp, &e, out, |g, arr, item, _| {
                    let size = g.size_const(&e);
                    let dup = g.dup_fn(&e);
                    let drop = g.drop_fn(&e);
                    let slot = g.tmp();
                    g.inst(&format!("{slot} = call ptr @ovt_arr_push(ptr {arr}, i64 {size}, ptr {dup}, ptr {drop})"));
                    let elt = g.lty(&e);
                    let v = g.load(&elt, item);
                    g.inst(&format!("store {}, ptr {slot}", v.op()));
                });
                "true".into()
            }
            Ty::Adt(id, targs) if *id == self.p.known.set => {
                let e = targs[0].clone();
                let insert = self.fn_inst(self.p.known.set_insert, targs, &[]);
                let new = self.fn_inst(self.p.known.set_new, targs, &[]);
                let st = self.lty(ty);
                self.json_dec_seq_into(jp, &e, out, &st, &new, |g, set, item| {
                    let elt = g.lty(&e);
                    let v = g.load(&elt, item);
                    let t = g.tmp();
                    g.inst(&format!("{t} = call i1 {insert}(ptr {set}, {})", v.op()));
                });
                "true".into()
            }
            Ty::Tuple(ts) => {
                let ts = ts.clone();
                let lt = self.lty(ty);
                let r = self.tmp();
                self.inst(&format!("{r} = call i32 @ovt_jp_arr(ptr {jp})"));
                let ok = self.nonzero(&r);
                self.or_fail(&ok);
                let tmp = self.alloca(&lt);
                let failed = self.label("failed");
                let wrong_len = self.label("wrong_len");
                // How many elements are decoded, for the cleanup.
                let count = self.alloca("i64");
                self.inst(&format!("store i64 0, ptr {count}"));
                for (i, t) in ts.iter().enumerate() {
                    let r = self.tmp();
                    self.inst(&format!("{r} = call i32 @ovt_jp_next(ptr {jp}, i32 {})", if i == 0 { 1 } else { 0 }));
                    let more = self.label("elem");
                    self.json_step(&r, &more, &wrong_len, &failed);
                    self.start(&more);
                    let fp = self.gep(&lt, &tmp, &[0, i]);
                    let ok = self.json_dec_call(t, jp, &fp);
                    let next = self.label("next");
                    let bad = self.label("bad_elem");
                    self.term(&format!("br i1 {ok}, label %{next}, label %{bad}"));
                    self.start(&bad);
                    self.inst(&format!("call void @ovt_jp_at_index(ptr {jp}, i64 {i})"));
                    self.term(&format!("br label %{failed}"));
                    self.start(&next);
                    self.inst(&format!("store i64 {}, ptr {count}", i + 1));
                }
                let r = self.tmp();
                self.inst(&format!("{r} = call i32 @ovt_jp_next(ptr {jp}, i32 {})", if ts.is_empty() { 1 } else { 0 }));
                let end = self.label("end");
                self.json_step(&r, &wrong_len, &end, &failed);
                self.start(&wrong_len);
                self.inst(&format!("call void @ovt_jp_bad_len(ptr {jp}, i64 {})", ts.len()));
                self.term(&format!("br label %{failed}"));
                self.start(&failed);
                let n = self.load("i64", &count);
                for (i, t) in ts.iter().enumerate() {
                    let had = self.tmp();
                    self.inst(&format!("{had} = icmp ugt i64 {}, {i}", n.repr));
                    let fp = self.gep(&lt, &tmp, &[0, i]);
                    let tc = t.clone();
                    self.when(&had, |g| g.drop_ptr(&fp, &tc));
                }
                self.term("ret i1 false");
                self.start(&end);
                let v = self.load(&lt, &tmp);
                self.inst(&format!("store {}, ptr {out}", v.op()));
                "true".into()
            }
            Ty::Adt(id, targs) if *id == self.p.known.map => {
                let targs = targs.clone();
                let vty = targs[1].clone();
                let lt = self.lty(ty);
                let new = self.fn_inst(self.p.known.map_new, &targs, &[]);
                let set = self.fn_inst(self.p.known.map_set, &targs, &[]);
                let r = self.tmp();
                self.inst(&format!("{r} = call i32 @ovt_jp_obj(ptr {jp})"));
                let ok = self.nonzero(&r);
                self.or_fail(&ok);
                let m = self.alloca(&lt);
                let mv = self.tmp();
                self.inst(&format!("{mv} = call {lt} {new}()"));
                self.inst(&format!("store {lt} {mv}, ptr {m}"));
                let first = self.alloca("i32");
                self.inst(&format!("store i32 1, ptr {first}"));
                let head = self.label("entry");
                let more = self.label("more");
                let end = self.label("end");
                let failed = self.label("failed");
                self.term(&format!("br label %{head}"));
                self.start(&head);
                let f = self.load("i32", &first);
                self.inst(&format!("store i32 0, ptr {first}"));
                let r = self.tmp();
                self.inst(&format!("{r} = call i32 @ovt_jp_key(ptr {jp}, i32 {})", f.repr));
                self.json_step(&r, &more, &end, &failed);
                self.start(&more);
                let key = self.alloca("%ovt.arr");
                self.inst(&format!("call void @ovt_jp_take_key(ptr {jp}, ptr {key})"));
                let vlt = self.lty(&vty);
                let val = self.alloca(&vlt);
                let ok = self.json_dec_call(&vty, jp, &val);
                let good = self.label("good");
                let bad = self.label("bad_value");
                self.term(&format!("br i1 {ok}, label %{good}, label %{bad}"));
                self.start(&bad);
                self.inst(&format!("call void @ovt_jp_at_key(ptr {jp}, ptr {key})"));
                self.drop_ptr(&key, &Ty::Str);
                self.term(&format!("br label %{failed}"));
                self.start(&good);
                let k = self.load("%ovt.arr", &key);
                let v = self.load(&vlt, &val);
                self.inst(&format!("call void {set}(ptr {m}, {}, {})", k.op(), v.op()));
                self.term(&format!("br label %{head}"));
                self.start(&failed);
                self.drop_ptr(&m, ty);
                self.term("ret i1 false");
                self.start(&end);
                let v = self.load(&lt, &m);
                self.inst(&format!("store {}, ptr {out}", v.op()));
                "true".into()
            }
            Ty::Adt(id, _) if self.is_payloadless_enum(*id) => {
                let names: Vec<String> = self.p.adts[*id].variants().iter().map(|v| v.name.clone()).collect();
                let r = self.tmp();
                self.inst(&format!("{r} = call i32 @ovt_jp_name(ptr {jp})"));
                let ok = self.nonzero(&r);
                self.or_fail(&ok);
                for (i, n) in names.iter().enumerate() {
                    let is = self.key_is(jp, n);
                    self.when(&is, |g| {
                        g.inst(&format!("store i32 {i}, ptr {out}"));
                        g.term("ret i1 true");
                    });
                }
                self.bad_name(jp, &names);
                "false".into()
            }
            Ty::Adt(id, _) if self.p.adts[*id].is_enum() => {
                let variants: Vec<(String, bool)> = self.p.adts[*id].variants().iter().map(|v| (v.name.clone(), v.fields.is_empty())).collect();
                let lt = self.lty(ty);
                let c = self.tmp();
                self.inst(&format!("{c} = call i32 @ovt_jp_peek(ptr {jp})"));
                let name = self.label("name");
                let object = self.label("object");
                let other = self.label("other");
                self.term(&format!("switch i32 {c}, label %{other} [ i32 34, label %{name} i32 123, label %{object} ]"));
                self.start(&other);
                let want = self.cstr(b"a string or an object");
                self.inst(&format!("call void @ovt_jp_expected(ptr {jp}, ptr {want})"));
                self.term("ret i1 false");
                // A variant without fields, by name.
                self.start(&name);
                let r = self.tmp();
                self.inst(&format!("{r} = call i32 @ovt_jp_name(ptr {jp})"));
                let ok = self.nonzero(&r);
                self.or_fail(&ok);
                let units: Vec<String> = variants.iter().filter(|v| v.1).map(|v| v.0.clone()).collect();
                for (i, (n, unit)) in variants.iter().enumerate() {
                    if *unit {
                        let is = self.key_is(jp, n);
                        self.when(&is, |g| {
                            let tp = g.gep(&lt, out, &[0, 0]);
                            g.inst(&format!("store i32 {i}, ptr {tp}"));
                            g.term("ret i1 true");
                        });
                    }
                }
                self.bad_name(jp, &units);
                // A variant with fields: {"Name": {fields}}.
                self.start(&object);
                let r = self.tmp();
                self.inst(&format!("{r} = call i32 @ovt_jp_obj(ptr {jp})"));
                let ok = self.nonzero(&r);
                self.or_fail(&ok);
                let r = self.tmp();
                self.inst(&format!("{r} = call i32 @ovt_jp_key(ptr {jp}, i32 1)"));
                let keyed = self.label("keyed");
                let empty = self.label("empty");
                let broken = self.label("broken");
                self.json_step(&r, &keyed, &empty, &broken);
                self.start(&broken);
                self.term("ret i1 false");
                self.start(&empty);
                self.inst(&format!("call void @ovt_jp_one_key(ptr {jp})"));
                self.term("ret i1 false");
                self.start(&keyed);
                let tmp = self.alloca(&lt);
                let payload = self.gep(&lt, &tmp, &[0, 1]);
                let with_fields: Vec<String> = variants.iter().filter(|v| !v.1).map(|v| v.0.clone()).collect();
                let tyc = ty.clone();
                for (i, (n, unit)) in variants.iter().enumerate() {
                    if *unit {
                        continue;
                    }
                    let is = self.key_is(jp, n);
                    let nc = n.clone();
                    self.when(&is, |g| {
                        g.json_dec_fields(&tyc, Some(i), jp, &payload, Some(&nc));
                        let tp = g.gep(&lt, &tmp, &[0, 0]);
                        g.inst(&format!("store i32 {i}, ptr {tp}"));
                        let r = g.tmp();
                        g.inst(&format!("{r} = call i32 @ovt_jp_key(ptr {jp}, i32 0)"));
                        let done = g.label("done");
                        let extra = g.label("extra");
                        let broke = g.label("broke");
                        g.json_step(&r, &extra, &done, &broke);
                        g.start(&extra);
                        g.inst(&format!("call void @ovt_jp_one_key(ptr {jp})"));
                        g.term(&format!("br label %{broke}"));
                        g.start(&broke);
                        g.drop_ptr(&tmp, &tyc);
                        g.term("ret i1 false");
                        g.start(&done);
                        let v = g.load(&lt, &tmp);
                        g.inst(&format!("store {}, ptr {out}", v.op()));
                        g.term("ret i1 true");
                    });
                }
                self.bad_name(jp, &with_fields);
                "false".into()
            }
            Ty::Adt(..) => self.json_dec_fields(ty, None, jp, out, None),
            // Types without a JSON form don't satisfy `Json`, so they can't get here.
            _ => {
                let want = self.cstr(b"a value of a type with JSON");
                self.inst(&format!("call void @ovt_jp_expected(ptr {jp}, ptr {want})"));
                "false".into()
            }
        }
    }

    fn key_is(&mut self, jp: &str, name: &str) -> String {
        let c = self.cstr(name.as_bytes());
        let r = self.tmp();
        self.inst(&format!("{r} = call i32 @ovt_jp_key_is(ptr {jp}, ptr {c}, i64 {})", name.len()));
        self.nonzero(&r)
    }

    fn at_field(&mut self, jp: &str, name: &str) {
        let c = self.cstr(name.as_bytes());
        self.inst(&format!("call void @ovt_jp_at_field(ptr {jp}, ptr {c}, i64 {})", name.len()));
    }

    /// Fails because the name just read matched none of `names`.
    fn bad_name(&mut self, jp: &str, names: &[String]) {
        let choices = names.iter().map(|n| format!("\"{n}\"")).collect::<Vec<_>>().join(", ");
        let c = self.cstr(choices.as_bytes());
        self.inst(&format!("call void @ovt_jp_bad_name(ptr {jp}, ptr {c}, i64 {})", choices.len()));
        self.term("ret i1 false");
    }

    /// An array's elements, collected into a new array at `out`: `add(array
    /// pointer, item pointer, index)` takes each decoded item.
    fn json_dec_seq(&mut self, jp: &str, e: &Ty, out: &str, add: impl FnMut(&mut Self, &str, &str, &str)) {
        self.json_dec_seq_with(jp, e, out, "%ovt.arr", None, add);
    }

    /// Like `json_dec_seq`, into a collection of type `lt` made by calling `new`.
    fn json_dec_seq_into(&mut self, jp: &str, e: &Ty, out: &str, lt: &str, new: &str, mut add: impl FnMut(&mut Self, &str, &str)) {
        self.json_dec_seq_with(jp, e, out, lt, Some(new), |g, c, item, _| add(g, c, item));
    }

    fn json_dec_seq_with(&mut self, jp: &str, e: &Ty, out: &str, lt: &str, new: Option<&str>, mut add: impl FnMut(&mut Self, &str, &str, &str)) {
        let r = self.tmp();
        self.inst(&format!("{r} = call i32 @ovt_jp_arr(ptr {jp})"));
        let ok = self.nonzero(&r);
        self.or_fail(&ok);
        let coll = self.alloca(lt);
        match new {
            Some(f) => {
                let v = self.tmp();
                self.inst(&format!("{v} = call {lt} {f}()"));
                self.inst(&format!("store {lt} {v}, ptr {coll}"));
            }
            None => self.inst(&format!("store {lt} zeroinitializer, ptr {coll}")),
        }
        let ity = self.lty(e);
        let item = self.alloca(&ity);
        let index = self.alloca("i64");
        self.inst(&format!("store i64 0, ptr {index}"));
        let head = self.label("elem");
        let more = self.label("more");
        let end = self.label("end");
        let failed = self.label("failed");
        self.term(&format!("br label %{head}"));
        self.start(&head);
        let i = self.load("i64", &index);
        let first = self.tmp();
        self.inst(&format!("{first} = icmp eq i64 {}, 0", i.repr));
        let f32 = self.tmp();
        self.inst(&format!("{f32} = zext i1 {first} to i32"));
        let r = self.tmp();
        self.inst(&format!("{r} = call i32 @ovt_jp_next(ptr {jp}, i32 {f32})"));
        self.json_step(&r, &more, &end, &failed);
        self.start(&more);
        let ok = self.json_dec_call(e, jp, &item);
        let good = self.label("good");
        let bad = self.label("bad_elem");
        self.term(&format!("br i1 {ok}, label %{good}, label %{bad}"));
        self.start(&bad);
        self.inst(&format!("call void @ovt_jp_at_index(ptr {jp}, i64 {})", i.repr));
        self.term(&format!("br label %{failed}"));
        self.start(&good);
        add(self, &coll, &item, &i.repr);
        let n = self.tmp();
        self.inst(&format!("{n} = add i64 {}, 1", i.repr));
        self.inst(&format!("store i64 {n}, ptr {index}"));
        self.term(&format!("br label %{head}"));
        self.start(&failed);
        match new {
            // A set: its drop helper.
            Some(_) => {
                let set_ty = Ty::Adt(self.p.known.set, vec![e.clone()]);
                self.drop_ptr(&coll, &set_ty);
            }
            None => {
                let arr_ty = Ty::Array(Box::new(e.clone()));
                self.drop_ptr(&coll, &arr_ty);
            }
        }
        self.term("ret i1 false");
        self.start(&end);
        let v = self.load(lt, &coll);
        self.inst(&format!("store {}, ptr {out}", v.op()));
    }

    /// The fields of a struct or variant, from an object, into `out`. On
    /// failure it returns from the decoder, adding `path` (a variant's name) to
    /// the error's path.
    fn json_dec_fields(&mut self, ty: &Ty, variant: Option<usize>, jp: &str, out: &str, path: Option<&str>) -> String {
        let Ty::Adt(id, targs) = ty else { unreachable!() };
        let (id, targs) = (*id, targs.clone());
        let defs: Vec<(String, Option<TExpr>)> = match variant {
            None => self.p.adts[id].fields().iter().map(|f| (f.name.clone(), f.default.clone())).collect(),
            Some(v) => self.p.adts[id].variants()[v].fields.iter().map(|f| (f.name.clone(), f.default.clone())).collect(),
        };
        let (lt, fields) = self.components(ty, variant);
        let failed = self.label("failed");
        let r = self.tmp();
        self.inst(&format!("{r} = call i32 @ovt_jp_obj(ptr {jp})"));
        let ok = self.nonzero(&r);
        let object = self.label("object");
        let not_object = self.label("not_object");
        self.term(&format!("br i1 {ok}, label %{object}, label %{not_object}"));
        self.start(&not_object);
        if let Some(name) = path {
            self.at_field(jp, name);
        }
        self.term("ret i1 false");
        self.start(&object);
        let tmp = self.alloca(&lt);
        let seen: Vec<String> = fields.iter().map(|_| self.alloca("i1")).collect();
        for s in &seen {
            self.inst(&format!("store i1 false, ptr {s}"));
        }
        let first = self.alloca("i32");
        self.inst(&format!("store i32 1, ptr {first}"));
        let head = self.label("entry");
        let more = self.label("more");
        let end = self.label("end");
        self.term(&format!("br label %{head}"));
        self.start(&head);
        let f = self.load("i32", &first);
        self.inst(&format!("store i32 0, ptr {first}"));
        let r = self.tmp();
        self.inst(&format!("{r} = call i32 @ovt_jp_key(ptr {jp}, i32 {})", f.repr));
        self.json_step(&r, &more, &end, &failed);
        self.start(&more);
        for (i, (boxed, fty)) in fields.iter().enumerate() {
            let (name, default) = &defs[i];
            let is = self.key_is(jp, name);
            let this = self.label("field");
            let not = self.label("not_field");
            self.term(&format!("br i1 {is}, label %{this}, label %{not}"));
            self.start(&this);
            let fp = self.gep(&lt, &tmp, &[0, i]);
            // A repeated key: the last one wins.
            let had = self.load("i1", &seen[i]);
            self.when(&had.repr, |g| g.drop_field(&fp, *boxed, fty));
            self.inst(&format!("store i1 false, ptr {}", seen[i]));
            if default.is_some() && !matches!(fty, Ty::Opt(_)) {
                // `null` counts as missing, so it takes the default.
                let r = self.tmp();
                self.inst(&format!("{r} = call i32 @ovt_jp_null(ptr {jp})"));
                let is_null = self.nonzero(&r);
                self.when(&is_null, |g| g.term(&format!("br label %{head}")));
            }
            let ok = if *boxed {
                let flt = self.lty(fty);
                let val = self.alloca(&flt);
                let ok = self.json_dec_call(fty, jp, &val);
                self.when(&ok, |g| {
                    let v = g.load(&flt, &val);
                    let bx = g.make_box(&v, fty);
                    g.inst(&format!("store ptr {}, ptr {fp}", bx.repr));
                });
                ok
            } else {
                self.json_dec_call(fty, jp, &fp)
            };
            let good = self.label("good");
            let bad = self.label("bad_field");
            self.term(&format!("br i1 {ok}, label %{good}, label %{bad}"));
            self.start(&bad);
            self.at_field(jp, name);
            self.term(&format!("br label %{failed}"));
            self.start(&good);
            self.inst(&format!("store i1 true, ptr {}", seen[i]));
            self.term(&format!("br label %{head}"));
            self.start(&not);
        }
        let r = self.tmp();
        self.inst(&format!("{r} = call i32 @ovt_jp_skip(ptr {jp})"));
        let ok = self.nonzero(&r);
        self.term(&format!("br i1 {ok}, label %{head}, label %{failed}"));
        // Missing fields: `none`, the default, or an error.
        self.start(&end);
        let saved_targs = std::mem::replace(&mut self.f.targs, targs);
        for (i, (boxed, fty)) in fields.iter().enumerate() {
            let (name, default) = &defs[i];
            let had = self.load("i1", &seen[i]);
            let missing = self.tmp();
            self.inst(&format!("{missing} = xor i1 {}, true", had.repr));
            let fp = self.gep(&lt, &tmp, &[0, i]);
            self.when(&missing, |g| {
                let v = if let Some(d) = default {
                    g.push_scope(false);
                    let v = g.owned(d);
                    g.pop_scope();
                    v
                } else if let Ty::Opt(_) = fty {
                    let olt = g.lty(fty);
                    V::new(olt, "zeroinitializer")
                } else {
                    let c = g.cstr(name.as_bytes());
                    g.inst(&format!("call void @ovt_jp_missing(ptr {jp}, ptr {c}, i64 {})", name.len()));
                    g.term(&format!("br label %{failed}"));
                    return;
                };
                if *boxed {
                    let bx = g.make_box(&v, fty);
                    g.inst(&format!("store ptr {}, ptr {fp}", bx.repr));
                } else {
                    g.inst(&format!("store {}, ptr {fp}", v.op()));
                }
                g.inst(&format!("store i1 true, ptr {}", seen[i]));
            });
        }
        self.f.targs = saved_targs;
        let v = self.load(&lt, &tmp);
        self.inst(&format!("store {}, ptr {out}", v.op()));
        let done = self.label("done");
        self.term(&format!("br label %{done}"));
        // Frees the fields decoded so far.
        self.start(&failed);
        for (i, (boxed, fty)) in fields.iter().enumerate() {
            let had = self.load("i1", &seen[i]);
            let fp = self.gep(&lt, &tmp, &[0, i]);
            self.when(&had.repr, |g| g.drop_field(&fp, *boxed, fty));
        }
        if let Some(name) = path {
            self.at_field(jp, name);
        }
        self.term("ret i1 false");
        self.start(&done);
        "true".into()
    }

    fn drop_field(&mut self, fp: &str, boxed: bool, fty: &Ty) {
        if boxed {
            let bx = self.load("ptr", fp);
            let drop = self.drop_fn(fty);
            self.inst(&format!("call void @ovt_box_release(ptr {}, ptr {drop})", bx.repr));
        } else {
            self.drop_ptr(fp, fty);
        }
    }
}
