//! `par { ... }`: each statement is a closure (see `Checker::par_stmt`), and
//! the runtime runs them on several tasks with `ovt_parallel`. The context
//! the dispatch function gets is an array of pointers: for each statement,
//! its closure value and its result slot, then the status bytes (1 for a
//! result, 2 for an error) and the error slots.

use super::*;

impl<'p> Gen<'p> {
    pub fn par_stmt(&mut self, branches: &[TParBranch]) {
        let k = branches.len();
        if k == 0 || self.f.terminated {
            return;
        }
        let mut fns = Vec::new();
        let mut sigs = Vec::new();
        for b in branches {
            let fty = self.sub(&b.closure.ty);
            let Ty::Fn(ft) = &fty else { unreachable!("checked: a closure") };
            let sig = (ft.ret.clone(), ft.eff.fail);
            let v = self.owned(&b.closure);
            // What the statements can reach is shared from here on.
            let env = self.extract(&v, 1, "ptr");
            self.inst(&format!("call void @ovt_mark_env(ptr {})", env.repr));
            fns.push((self.spill(&v), fty.clone()));
            sigs.push(sig);
        }
        let results: Vec<String> = sigs.iter().map(|(r, _)| {
            let lt = self.lty(r);
            self.alloca(&lt)
        }).collect();
        let status = self.alloca(&format!("[{k} x i8]"));
        self.inst(&format!("store [{k} x i8] zeroinitializer, ptr {status}"));
        let et = self.err_lty();
        let errs = self.alloca(&format!("[{k} x {et}]"));
        let ctx_lt = format!("[{} x ptr]", 2 * k + 2);
        let ctx = self.alloca(&ctx_lt);
        let mut ptrs: Vec<String> = Vec::new();
        for i in 0..k {
            ptrs.push(fns[i].0.clone());
            ptrs.push(results[i].clone());
        }
        ptrs.push(status.clone());
        ptrs.push(errs.clone());
        for (i, p) in ptrs.iter().enumerate() {
            let at = self.gep(&ctx_lt, &ctx, &[0, i]);
            self.inst(&format!("store ptr {p}, ptr {at}"));
        }
        let body = self.par_body(&sigs);
        let r = self.tmp();
        self.inst(&format!("{r} = call i64 @ovt_parallel(i64 {k}, ptr {body}, ptr {ctx})"));
        for (p, ty) in &fns {
            self.drop_ptr(p, ty);
        }
        if sigs.iter().any(|(_, fails)| *fails) {
            let bad = self.tmp();
            self.inst(&format!("{bad} = icmp ne i64 {r}, -1"));
            let fail = self.label("par_failed");
            let ok = self.label("par_ok");
            self.term(&format!("br i1 {bad}, label %{fail}, label %{ok}"));
            self.start(&fail);
            // Drop what the other statements made, and their errors.
            for i in 0..k {
                let sp = self.gep(&format!("[{k} x i8]"), &status, &[0, i]);
                let st = self.load("i8", &sp);
                let made = self.tmp();
                self.inst(&format!("{made} = icmp eq i8 {}, 1", st.repr));
                let (rty, _) = sigs[i].clone();
                let res = results[i].clone();
                self.when_true(&made, |g| g.drop_ptr(&res, &rty));
                let failed = self.tmp();
                self.inst(&format!("{failed} = icmp eq i8 {}, 2", st.repr));
                let other = self.tmp();
                self.inst(&format!("{other} = icmp ne i64 {r}, {i}"));
                let both = self.tmp();
                self.inst(&format!("{both} = and i1 {failed}, {other}"));
                let ep = self.gep(&format!("[{k} x {et}]"), &errs, &[0, i]);
                let msgp = self.gep(&et, &ep, &[0, 1]);
                self.when_true(&both, |g| g.drop_ptr(&msgp, &Ty::Str));
            }
            let errp = self.tmp();
            self.inst(&format!("{errp} = getelementptr inbounds [{k} x {et}], ptr {errs}, i64 0, i64 {r}"));
            let err = self.load(&et, &errp);
            self.emit_fail(err);
            self.start(&ok);
        }
        // Bind the variables the statements declared.
        for (i, b) in branches.iter().enumerate() {
            let (rty, _) = sigs[i].clone();
            match b.outs.len() {
                0 => {}
                1 => {
                    let lt = self.lty(&rty);
                    let v = self.load(&lt, &results[i]);
                    self.bind_out(b.outs[0], &v);
                }
                _ => {
                    let lt = self.lty(&rty);
                    let v = self.load(&lt, &results[i]);
                    let Ty::Tuple(tys) = &rty else { unreachable!("several variables come back as a tuple") };
                    for (j, id) in b.outs.iter().enumerate() {
                        let flt = self.lty(&tys[j]);
                        let x = self.extract(&v, j, &flt);
                        self.bind_out(*id, &x);
                    }
                }
            }
        }
    }

    fn bind_out(&mut self, id: LocalId, v: &V) {
        let slot = self.f.slots[id].clone();
        self.inst(&format!("store {}, ptr {slot}", v.op()));
        let ty = self.f.local_tys[id].clone();
        self.add_local_drop(&slot, &ty);
    }

    /// Runs `body` only when the `i1` `cond` is true.
    fn when_true(&mut self, cond: &str, body: impl FnOnce(&mut Self)) {
        let yes = self.label("yes");
        let done = self.label("done");
        self.term(&format!("br i1 {cond}, label %{yes}, label %{done}"));
        self.start(&yes);
        body(self);
        if !self.f.terminated {
            self.term(&format!("br label %{done}"));
        }
        self.start(&done);
    }

    /// `i32 body(ptr ctx, i64 i)`: runs statement `i` and stores what it gives.
    pub fn emit_par_body(&mut self, sigs: &[(Ty, bool)], sym: &str) -> String {
        self.f = Fx { cur: "entry".into(), ..Default::default() };
        let k = sigs.len();
        let ctx_lt = format!("[{} x ptr]", 2 * k + 2);
        let labels: Vec<String> = (0..k).map(|_| self.label("stmt")).collect();
        let cases: Vec<String> = labels.iter().enumerate().map(|(i, l)| format!("i64 {i}, label %{l}")).collect();
        let end = self.label("end");
        self.term(&format!("switch i64 %i, label %{end} [ {} ]", cases.join(" ")));
        for (i, (ret, failable)) in sigs.iter().enumerate() {
            self.start(&labels[i]);
            let fpp = self.gep(&ctx_lt, "%ctx", &[0, 2 * i]);
            let fslot = self.load("ptr", &fpp);
            let f = self.load("%ovt.fn", &fslot.repr);
            let rpp = self.gep(&ctx_lt, "%ctx", &[0, 2 * i + 1]);
            let res = self.load("ptr", &rpp);
            let spp = self.gep(&ctx_lt, "%ctx", &[0, 2 * k]);
            let status = self.load("ptr", &spp);
            let epp = self.gep(&ctx_lt, "%ctx", &[0, 2 * k + 1]);
            let errs = self.load("ptr", &epp);
            let stp = self.tmp();
            self.inst(&format!("{stp} = getelementptr inbounds i8, ptr {}, i64 {i}", status.repr));
            let func = self.extract(&f, 0, "ptr");
            let env = self.extract(&f, 1, "ptr");
            let rt = self.ret_lty(ret, *failable);
            let rl = self.lty(ret);
            if rt == "void" {
                self.inst(&format!("call void {}(ptr {})", func.repr, env.repr));
            } else {
                let r = self.tmp();
                self.inst(&format!("{r} = call {rt} {}(ptr {})", func.repr, env.repr));
                let r = V::new(rt.clone(), r);
                let v = if *failable {
                    let failed = self.extract(&r, 0, "i1");
                    let bad = self.label("failed");
                    let ok = self.label("ok");
                    self.term(&format!("br i1 {}, label %{bad}, label %{ok}", failed.repr));
                    self.start(&bad);
                    let et = self.err_lty();
                    let e = self.extract(&r, 2, &et);
                    let errp = self.tmp();
                    self.inst(&format!("{errp} = getelementptr inbounds {et}, ptr {}, i64 {i}", errs.repr));
                    self.inst(&format!("store {}, ptr {errp}", e.op()));
                    self.inst(&format!("store i8 2, ptr {stp}"));
                    self.term("ret i32 1");
                    self.start(&ok);
                    self.extract(&r, 1, &rl)
                } else {
                    r
                };
                if rl != "{}" {
                    self.inst(&format!("store {}, ptr {}", v.op(), res.repr));
                }
            }
            self.inst(&format!("store i8 1, ptr {stp}"));
            self.term("ret i32 0");
        }
        self.start(&end);
        self.term("ret i32 0");
        let f = std::mem::take(&mut self.f);
        format!("define internal i32 {sym}(ptr %ctx, i64 %i) {{\nentry:\n{}{}}}\n", f.allocas, f.code)
    }
}
