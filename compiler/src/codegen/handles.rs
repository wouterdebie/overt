//! Intrinsics of the shared handles and of network and time IO: `Chan`,
//! `Shared`, `net`, `time`, `task.group` and `task.timeout`.
//!
//! A handle is a std struct holding one closure-like value `{fn: null, env}`,
//! whose environment is a runtime object that starts with the environment
//! header `{count, drop, mark}` (see runtime/rt.c). So copies share the
//! object, counting and marking work as for closures, and the object is freed
//! with the last copy.

use super::*;
use crate::source::Span;

/// `ErrKind.Timeout`, in declaration order in std/prelude.ovt.
const KIND_TIMEOUT: &str = "4";

impl<'p> Gen<'p> {
    /// The environment (the runtime object) of the handle value `h`.
    fn handle_env(&mut self, h: &V) -> String {
        let cell = self.extract(h, 0, "%ovt.fn");
        self.extract(&cell, 1, "ptr").repr
    }

    /// A value of handle type `ty` around `env`.
    fn make_handle(&mut self, ty: &Ty, env: &str) -> V {
        let lt = self.lty(ty);
        let cell = self.insert(&V::new("%ovt.fn", "{ ptr null, ptr null }"), &V::new("ptr", env), 1);
        self.insert(&V::new(lt, "undef"), &cell, 0)
    }

    /// An optional of the function's return type from an `i32` flag and a value.
    fn optional_from(&mut self, got: &str, v: &V) -> V {
        let bit = self.tmp();
        self.inst(&format!("{bit} = icmp ne i32 {got}, 0"));
        let ret = self.f.ret.clone();
        let lt = self.lty(&ret);
        let o = self.insert(&V::new(lt, "undef"), &V::new("i1", bit), 0);
        self.insert(&o, v, 1)
    }

    /// Calls the mark helper of `ty` on the value at `ptr`, if it holds anything counted.
    fn mark_ptr(&mut self, ptr: &str, ty: &Ty) {
        let m = self.mark_fn(ty);
        if m != "null" {
            self.inst(&format!("call void {m}(ptr {ptr})"));
        }
    }

    /// The body of one of these intrinsics; `false` if `key` isn't one.
    pub fn handle_intrinsic(&mut self, f: FnId, key: &str, params: &[V], span: Span) -> bool {
        match key {
            "Chan.new" => {
                let t = self.f.targs[0].clone();
                let size = self.size_const(&t);
                let drop = self.drop_fn(&t);
                let env = self.tmp();
                self.inst(&format!("{env} = call ptr @ovt_chan_new({}, i64 {size}, ptr {drop})", params[0].op()));
                let ret = self.f.ret.clone();
                let h = self.make_handle(&ret, &env);
                self.emit_return(Some(h));
            }
            "Chan.send" => {
                let t = self.f.targs[0].clone();
                let env = self.handle_env(&params[0]);
                // The value moves to whichever task receives it.
                let vp = self.spill(&params[1]);
                self.mark_ptr(&vp, &t);
                let err = self.alloca("%ovt.arr");
                let code = self.tmp();
                self.inst(&format!("{code} = call i32 @ovt_chan_send(ptr {env}, ptr {vp}, ptr {err})"));
                self.fail_on_code(&code, &err);
                self.emit_return(None);
            }
            "Chan.recv" => {
                let t = self.f.targs[0].clone();
                let env = self.handle_env(&params[0]);
                let tl = self.lty(&t);
                let out = self.alloca(&tl);
                let got = self.tmp();
                self.inst(&format!("{got} = call i32 @ovt_chan_recv(ptr {env}, ptr {out})"));
                let v = self.load(&tl, &out);
                let o = self.optional_from(&got, &v);
                self.emit_return(Some(o));
            }
            "Chan.close" => {
                let env = self.handle_env(&params[0]);
                self.inst(&format!("call void @ovt_chan_close(ptr {env})"));
                self.emit_return(None);
            }
            "Chan.len" => {
                let env = self.handle_env(&params[0]);
                let n = self.tmp();
                self.inst(&format!("{n} = call i64 @ovt_chan_len(ptr {env})"));
                self.emit_return(Some(V::new("i64", n)));
            }
            "Shared.new" => {
                let t = self.f.targs[0].clone();
                let size = self.size_const(&t);
                let drop = self.drop_fn(&t);
                let mark = self.mark_fn(&t);
                let env = self.tmp();
                self.inst(&format!("{env} = call ptr @ovt_shared_new(i64 {size}, ptr {drop}, ptr {mark})"));
                let vp = self.tmp();
                self.inst(&format!("{vp} = call ptr @ovt_shared_value(ptr {env})"));
                self.inst(&format!("store {}, ptr {vp}", params[0].op()));
                self.mark_ptr(&vp, &t);
                let ret = self.f.ret.clone();
                let h = self.make_handle(&ret, &env);
                self.emit_return(Some(h));
            }
            "net.listen" | "net.connect" | "Listener.accept" => {
                let out = self.alloca("ptr");
                let err = self.alloca("%ovt.arr");
                let code = self.tmp();
                if key == "Listener.accept" {
                    let env = self.handle_env(&params[0]);
                    self.inst(&format!("{code} = call i32 @ovt_net_accept(ptr {env}, ptr {out}, ptr {err})"));
                } else {
                    let a = self.spill(&params[0]);
                    let func = if key == "net.listen" { "ovt_net_listen" } else { "ovt_net_connect" };
                    self.inst(&format!("{code} = call i32 @{func}(ptr {a}, ptr {out}, ptr {err})"));
                }
                self.fail_on_code(&code, &err);
                let env = self.load("ptr", &out);
                let ret = self.f.ret.clone();
                let h = self.make_handle(&ret, &env.repr);
                self.emit_return(Some(h));
            }
            "Listener.port" => {
                let env = self.handle_env(&params[0]);
                let n = self.tmp();
                self.inst(&format!("{n} = call i64 @ovt_net_port(ptr {env})"));
                self.emit_return(Some(V::new("i64", n)));
            }
            "Listener.close" | "Conn.close" => {
                let env = self.handle_env(&params[0]);
                self.inst(&format!("call void @ovt_net_close(ptr {env})"));
                self.emit_return(None);
            }
            "Conn.read_line" | "Conn.read_line_bytes" => {
                let env = self.handle_env(&params[0]);
                let out = self.alloca("%ovt.arr");
                self.inst(&format!("store %ovt.arr zeroinitializer, ptr {out}"));
                let got = self.alloca("i32");
                let err = self.alloca("%ovt.arr");
                let text = if key == "Conn.read_line" { 1 } else { 0 };
                let code = self.tmp();
                self.inst(&format!("{code} = call i32 @ovt_net_read_line(ptr {env}, ptr {out}, i32 {text}, ptr {got}, ptr {err})"));
                self.fail_on_code(&code, &err);
                let g = self.load("i32", &got);
                let v = self.load("%ovt.arr", &out);
                let o = self.optional_from(&g.repr, &v);
                self.emit_return(Some(o));
            }
            "Conn.read" => {
                let env = self.handle_env(&params[0]);
                let out = self.alloca("%ovt.arr");
                let err = self.alloca("%ovt.arr");
                let code = self.tmp();
                self.inst(&format!("{code} = call i32 @ovt_net_read(ptr {env}, ptr {out}, ptr {err})"));
                self.fail_on_code(&code, &err);
                let v = self.load("%ovt.arr", &out);
                self.emit_return(Some(v));
            }
            "Conn.write" | "Conn.write_bytes" => {
                let env = self.handle_env(&params[0]);
                let d = self.spill(&params[1]);
                let err = self.alloca("%ovt.arr");
                let code = self.tmp();
                self.inst(&format!("{code} = call i32 @ovt_net_write(ptr {env}, ptr {d}, ptr {err})"));
                self.fail_on_code(&code, &err);
                self.emit_return(None);
            }
            "Conn.set_timeout" => {
                let env = self.handle_env(&params[0]);
                self.inst(&format!("call void @ovt_net_set_timeout(ptr {env}, {})", params[1].op()));
                self.emit_return(None);
            }
            "Conn.peer" => {
                let env = self.handle_env(&params[0]);
                let out = self.alloca("%ovt.arr");
                self.inst(&format!("store %ovt.arr zeroinitializer, ptr {out}"));
                self.inst(&format!("call void @ovt_net_peer(ptr {env}, ptr {out})"));
                let v = self.load("%ovt.arr", &out);
                self.emit_return(Some(v));
            }
            "time.sleep" => {
                self.inst(&format!("call void @ovt_time_sleep({})", params[0].op()));
                self.emit_return(None);
            }
            "time.monotonic" => {
                let n = self.tmp();
                self.inst(&format!("{n} = call i64 @ovt_time_monotonic()"));
                self.emit_return(Some(V::new("i64", n)));
            }
            "task.group" => self.task_group(f, params),
            "Group.spawn" => {
                let genv = self.handle_env(&params[0]);
                let func = self.extract(&params[1], 0, "ptr");
                let env = self.extract(&params[1], 1, "ptr");
                // The new task runs the closure: it's shared from now on, and
                // the task holds a reference of its own.
                self.inst(&format!("call void @ovt_mark_env(ptr {})", env.repr));
                self.inst(&format!("call void @ovt_rc_inc(ptr {})", env.repr));
                let loc = self.loc_args(span);
                self.inst(&format!("call void @ovt_group_spawn(ptr {genv}, ptr {}, ptr {}, {loc})", func.repr, env.repr));
                self.emit_return(None);
            }
            "task.timeout" => self.task_timeout(params),
            _ => return false,
        }
        true
    }

    /// `task.group(body)`: runs `body(g)` here, then waits for the tasks it
    /// spawned, cancelling them first if `body` failed.
    fn task_group(&mut self, f: FnId, params: &[V]) {
        let Ty::Fn(bt) = &self.p.fns[f].params[0].ty else { unreachable!("checked: a function") };
        let gty = bt.params[0].clone();
        let g = self.tmp();
        self.inst(&format!("{g} = call ptr @ovt_group_new()"));
        let h = self.make_handle(&gty, &g);
        let func = self.extract(&params[0], 0, "ptr");
        let env = self.extract(&params[0], 1, "ptr");
        let failable = self.f.failable;
        let rt = self.ret_lty(&Ty::Unit, failable);
        let (r, failed) = if rt == "void" {
            self.inst(&format!("call void {}(ptr {}, {})", func.repr, env.repr, h.op()));
            (None, "0".to_string())
        } else {
            let r = self.tmp();
            self.inst(&format!("{r} = call {rt} {}(ptr {}, {})", func.repr, env.repr, h.op()));
            let r = V::new(rt.clone(), r);
            let b = self.extract(&r, 0, "i1");
            let w = self.tmp();
            self.inst(&format!("{w} = zext i1 {} to i32", b.repr));
            (Some((r, b)), w)
        };
        self.inst(&format!("call void @ovt_group_wait(ptr {g}, i32 {failed})"));
        self.inst(&format!("call void @ovt_env_release(ptr {g})"));
        match r {
            Some((r, b)) => {
                let bad = self.label("group_failed");
                let ok = self.label("group_ok");
                self.term(&format!("br i1 {}, label %{bad}, label %{ok}", b.repr));
                self.start(&bad);
                let et = self.err_lty();
                let e = self.extract(&r, 2, &et);
                self.emit_fail(e);
                self.start(&ok);
                self.emit_return(None);
            }
            None => self.emit_return(None),
        }
    }

    /// `task.timeout(d, f)`: the runtime runs the body in a child task, in a
    /// scope cancelled at the deadline. The context is `{f, result, error, status}`.
    fn task_timeout(&mut self, params: &[V]) {
        let t = self.f.targs[0].clone();
        let fails = self.f.eargs.first().is_some_and(|e| e.fail);
        let tl = self.lty(&t);
        let et = self.err_lty();
        let ctx_lt = format!("{{ %ovt.fn, {tl}, {et}, i8 }}");
        let ctx = self.alloca(&ctx_lt);
        let fp = self.gep(&ctx_lt, &ctx, &[0, 0]);
        self.inst(&format!("store {}, ptr {fp}", params[1].op()));
        let sp = self.gep(&ctx_lt, &ctx, &[0, 3]);
        self.inst(&format!("store i8 0, ptr {sp}"));
        let env = self.extract(&params[1], 1, "ptr");
        self.inst(&format!("call void @ovt_mark_env(ptr {})", env.repr));
        let body = self.timeout_body(&t, fails);
        let r = self.tmp();
        self.inst(&format!("{r} = call i32 @ovt_timeout({}, ptr {body}, ptr {ctx})", params[0].op()));
        let ok = self.label("in_time");
        let failed = self.label("failed");
        let late = self.label("timed_out");
        self.term(&format!("switch i32 {r}, label %{late} [ i32 0, label %{ok} i32 1, label %{failed} ]"));
        self.start(&ok);
        let vp = self.gep(&ctx_lt, &ctx, &[0, 1]);
        let v = self.load(&tl, &vp);
        self.emit_return(Some(v));
        self.start(&failed);
        let ep = self.gep(&ctx_lt, &ctx, &[0, 2]);
        let e = self.load(&et, &ep);
        self.emit_fail(e);
        self.start(&late);
        // The deadline passed: whatever the child made after being cancelled
        // gives way to the timeout.
        let st = self.load("i8", &sp);
        let made = self.tmp();
        self.inst(&format!("{made} = icmp eq i8 {}, 1", st.repr));
        let drop_value = self.label("drop_value");
        let drop_err = self.label("drop_error");
        let dropped = self.label("dropped");
        self.term(&format!("br i1 {made}, label %{drop_value}, label %{drop_err}"));
        self.start(&drop_value);
        let vp = self.gep(&ctx_lt, &ctx, &[0, 1]);
        self.drop_ptr(&vp, &t);
        self.term(&format!("br label %{dropped}"));
        self.start(&drop_err);
        let ep = self.gep(&ctx_lt, &ctx, &[0, 2]);
        let msgp = self.gep(&et, &ep, &[0, 1]);
        self.drop_ptr(&msgp, &Ty::Str);
        self.term(&format!("br label %{dropped}"));
        self.start(&dropped);
        let sb = self.alloca("%ovt.arr");
        self.inst(&format!("store %ovt.arr zeroinitializer, ptr {sb}"));
        let text = b"timed out after ";
        let c = self.cstr(text);
        self.inst(&format!("call void @ovt_sb_cstr(ptr {sb}, ptr {c}, i64 {})", text.len()));
        self.inst(&format!("call void @ovt_sb_dur(ptr {sb}, {})", params[0].op()));
        let msg = self.load("%ovt.arr", &sb);
        self.fail_with(KIND_TIMEOUT, msg);
    }

    /// `i32 body(ptr ctx)` for `task.timeout`: calls the closure and stores its
    /// result (status 1) or its error (status 2).
    pub fn emit_timeout_body(&mut self, t: &Ty, fails: bool, sym: &str) -> String {
        self.f = Fx { cur: "entry".into(), ..Default::default() };
        let tl = self.lty(t);
        let et = self.err_lty();
        let ctx_lt = format!("{{ %ovt.fn, {tl}, {et}, i8 }}");
        let fp = self.gep(&ctx_lt, "%ctx", &[0, 0]);
        let fv = self.load("%ovt.fn", &fp);
        let func = self.extract(&fv, 0, "ptr");
        let env = self.extract(&fv, 1, "ptr");
        let sp = self.gep(&ctx_lt, "%ctx", &[0, 3]);
        let rt = self.ret_lty(t, fails);
        if rt == "void" {
            self.inst(&format!("call void {}(ptr {})", func.repr, env.repr));
        } else {
            let r = self.tmp();
            self.inst(&format!("{r} = call {rt} {}(ptr {})", func.repr, env.repr));
            let r = V::new(rt.clone(), r);
            let v = if fails {
                let failed = self.extract(&r, 0, "i1");
                let bad = self.label("failed");
                let ok = self.label("ok");
                self.term(&format!("br i1 {}, label %{bad}, label %{ok}", failed.repr));
                self.start(&bad);
                let e = self.extract(&r, 2, &et);
                let ep = self.gep(&ctx_lt, "%ctx", &[0, 2]);
                self.inst(&format!("store {}, ptr {ep}", e.op()));
                self.inst(&format!("store i8 2, ptr {sp}"));
                self.term("ret i32 1");
                self.start(&ok);
                self.extract(&r, 1, &tl)
            } else {
                r
            };
            if tl != "{}" {
                let vp = self.gep(&ctx_lt, "%ctx", &[0, 1]);
                self.inst(&format!("store {}, ptr {vp}", v.op()));
            }
        }
        self.inst(&format!("store i8 1, ptr {sp}"));
        self.term("ret i32 0");
        let f = std::mem::take(&mut self.f);
        format!("define internal i32 {sym}(ptr %ctx) {{\nentry:\n{}{}}}\n", f.allocas, f.code)
    }
}
