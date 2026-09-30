//! Functions, closures and statements.
//!
//! Cleanup: owned values that need dropping (locals and statement temporaries)
//! are registered on a stack of scopes. Leaving a scope normally drops its
//! entries; `return`, failure, `break` and `continue` drop every scope they
//! leave. Locals live in `alloca` slots, which clang turns into registers.

use super::*;
use crate::ast::Mode;
use crate::source::Span;

#[derive(Default)]
pub struct Fx {
    pub allocas: String,
    pub code: String,
    pub tmp: usize,
    pub label: usize,
    pub terminated: bool,
    pub cur: String,
    pub targs: Vec<Ty>,
    pub eargs: Vec<Eff>,
    /// The slot of each local; for `Ref` locals the slot holds a pointer.
    pub slots: Vec<String>,
    pub local_tys: Vec<Ty>,
    pub local_kinds: Vec<LocalKind>,
    pub scopes: Vec<Scope>,
    pub loops: Vec<Loop>,
    pub ret: Ty,
    pub failable: bool,
    pub file: FileId,
}

pub struct Scope {
    pub drops: Vec<(String, Ty)>,
    /// A block's scope (holds locals), as opposed to a statement's temporaries.
    pub block: bool,
}

pub struct Loop {
    pub cont: String,
    pub brk: String,
    /// Scopes at and above these depths are dropped by `continue` and `break`.
    pub cont_depth: usize,
    pub brk_depth: usize,
}

impl<'p> Gen<'p> {
    // ---- emitting instructions ----

    pub fn tmp(&mut self) -> String {
        self.f.tmp += 1;
        format!("%t{}", self.f.tmp)
    }

    pub fn label(&mut self, hint: &str) -> String {
        self.f.label += 1;
        format!("{hint}.{}", self.f.label)
    }

    /// Emits an instruction, first opening an unreachable block if the current one has ended.
    pub fn inst(&mut self, s: &str) {
        if self.f.terminated {
            let l = self.label("dead");
            self.f.code.push_str(&format!("{l}:\n"));
            self.f.cur = l;
            self.f.terminated = false;
        }
        self.f.code.push_str("  ");
        self.f.code.push_str(s);
        self.f.code.push('\n');
    }

    pub fn term(&mut self, s: &str) {
        self.inst(s);
        self.f.terminated = true;
    }

    /// Starts block `l`, falling through into it from the current block.
    pub fn start(&mut self, l: &str) {
        if !self.f.terminated {
            self.f.code.push_str(&format!("  br label %{l}\n"));
        }
        self.f.code.push_str(&format!("{l}:\n"));
        self.f.cur = l.to_string();
        self.f.terminated = false;
    }

    pub fn alloca(&mut self, ty: &str) -> String {
        self.f.tmp += 1;
        let name = format!("%s{}", self.f.tmp);
        let _ = writeln!(self.f.allocas, "  {name} = alloca {ty}, align 8");
        name
    }

    pub fn spill(&mut self, v: &V) -> String {
        let p = self.alloca(&v.ty);
        self.inst(&format!("store {}, ptr {p}", v.op()));
        p
    }

    pub fn load(&mut self, ty: &str, ptr: &str) -> V {
        let t = self.tmp();
        self.inst(&format!("{t} = load {ty}, ptr {ptr}"));
        V::new(ty, t)
    }

    /// Loads the count at the start of a buffer, box or environment. It's an
    /// atomic load, since other tasks may be changing a shared count.
    pub fn load_rc(&mut self, ptr: &str) -> V {
        let t = self.tmp();
        self.inst(&format!("{t} = load atomic i64, ptr {ptr} monotonic, align 8"));
        V::new("i64", t)
    }

    pub fn extract(&mut self, v: &V, i: usize, ty: &str) -> V {
        let t = self.tmp();
        self.inst(&format!("{t} = extractvalue {} {}, {i}", v.ty, v.repr));
        V::new(ty, t)
    }

    pub fn insert(&mut self, agg: &V, field: &V, i: usize) -> V {
        let t = self.tmp();
        self.inst(&format!("{t} = insertvalue {} {}, {}, {i}", agg.ty, agg.repr, field.op()));
        V::new(agg.ty.clone(), t)
    }

    pub fn gep(&mut self, ty: &str, ptr: &str, idx: &[usize]) -> String {
        let t = self.tmp();
        let path: Vec<String> = idx.iter().map(|i| format!("i32 {i}")).collect();
        self.inst(&format!("{t} = getelementptr inbounds {ty}, ptr {ptr}, {}", path.join(", ")));
        t
    }

    pub fn sub(&self, t: &Ty) -> Ty {
        t.subst(&self.f.targs, &self.f.eargs)
    }

    pub fn loc(&self, span: Span) -> (u32, u32) {
        self.srcs[self.f.file].line_col(span.lo)
    }

    /// `ptr @file, i32 line, i32 col` for runtime calls that may trap.
    pub fn loc_args(&mut self, span: Span) -> String {
        let (line, col) = self.loc(span);
        let file = self.file_cstr(self.f.file);
        format!("ptr {file}, i32 {line}, i32 {col}")
    }

    /// The size of a type in bytes, as an LLVM constant expression.
    pub fn size_const(&mut self, ty: &Ty) -> String {
        let (s, _) = self.size_align(ty);
        s.to_string()
    }

    // ---- traps ----

    pub fn trap(&mut self, msg: &str, span: Span) {
        let c = self.cstr(msg.as_bytes());
        let loc = self.loc_args(span);
        self.inst(&format!("call void @ovt_trap(ptr {c}, i64 {}, {loc})", msg.len()));
        self.term("unreachable");
    }

    /// Traps with `msg` if `bad` (an `i1`) is true.
    pub fn trap_if(&mut self, bad: &str, msg: &str, span: Span) {
        let ok = self.label("ok");
        let fail = self.label("trap");
        self.term(&format!("br i1 {bad}, label %{fail}, label %{ok}"));
        self.start(&fail);
        self.trap(msg, span);
        self.start(&ok);
    }

    // ---- cleanup ----

    pub fn push_scope(&mut self, block: bool) {
        self.f.scopes.push(Scope { drops: Vec::new(), block });
    }

    pub fn pop_scope(&mut self) {
        let s = self.f.scopes.pop().unwrap();
        if !self.f.terminated {
            for (p, ty) in s.drops.iter().rev() {
                self.drop_ptr(p, ty);
            }
        }
    }

    pub fn add_drop(&mut self, ptr: &str, ty: &Ty) {
        if self.needs_rc(ty) {
            self.f.scopes.last_mut().unwrap().drops.push((ptr.to_string(), ty.clone()));
        }
    }

    pub fn add_local_drop(&mut self, ptr: &str, ty: &Ty) {
        if self.needs_rc(ty) {
            let s = self.f.scopes.iter_mut().rev().find(|s| s.block).unwrap();
            s.drops.push((ptr.to_string(), ty.clone()));
        }
    }

    /// Drops everything in scopes `depth..`, innermost first, without leaving them.
    pub fn cleanup_from(&mut self, depth: usize) {
        let entries: Vec<(String, Ty)> = self.f.scopes[depth..].iter().rev().flat_map(|s| s.drops.iter().rev().cloned()).collect();
        for (p, ty) in entries {
            self.drop_ptr(&p, &ty);
        }
    }

    pub fn drop_ptr(&mut self, ptr: &str, ty: &Ty) {
        if self.needs_rc(ty) {
            let h = self.helper(Helper::Drop, ty);
            self.inst(&format!("call void {h}(ptr {ptr})"));
        }
    }

    pub fn dup_ptr(&mut self, ptr: &str, ty: &Ty) {
        if self.needs_rc(ty) {
            let h = self.helper(Helper::Dup, ty);
            self.inst(&format!("call void {h}(ptr {ptr})"));
        }
    }

    pub fn dup_value(&mut self, v: &V, ty: &Ty) {
        if self.needs_rc(ty) {
            let p = self.spill(v);
            self.dup_ptr(&p, ty);
        }
    }

    pub fn drop_value(&mut self, v: &V, ty: &Ty) {
        if self.needs_rc(ty) {
            let p = self.spill(v);
            self.drop_ptr(&p, ty);
        }
    }

    /// Keeps an owned value alive until the end of the current statement.
    pub fn keep(&mut self, v: &V, ty: &Ty) {
        if self.needs_rc(ty) {
            let p = self.spill(v);
            self.add_drop(&p, ty);
        }
    }

    // ---- returning ----

    pub fn emit_return(&mut self, v: Option<V>) {
        self.cleanup_from(0);
        let ret = self.f.ret.clone();
        if self.f.failable {
            let rt = self.ret_lty(&ret, true);
            let et = self.err_lty();
            let val = v.unwrap_or_else(|| {
                let t = self.lty(&ret);
                V::new(t, "zeroinitializer")
            });
            let r = V::new(rt.clone(), "undef");
            let r = self.insert(&r, &V::new("i1", "false"), 0);
            let r = self.insert(&r, &val, 1);
            let r = self.insert(&r, &V::new(et, "zeroinitializer"), 2);
            self.term(&format!("ret {}", r.op()));
        } else if ret == Ty::Unit || ret == Ty::Never {
            self.term("ret void");
        } else {
            let v = v.expect("a value to return");
            self.term(&format!("ret {}", v.op()));
        }
    }

    /// Returns the error `err` (owned) from a failable function.
    pub fn emit_fail(&mut self, err: V) {
        self.cleanup_from(0);
        let ret = self.f.ret.clone();
        let rt = self.ret_lty(&ret, true);
        let t = self.lty(&ret);
        let r = V::new(rt, "undef");
        let r = self.insert(&r, &V::new("i1", "true"), 0);
        let r = self.insert(&r, &V::new(t, "zeroinitializer"), 1);
        let r = self.insert(&r, &err, 2);
        self.term(&format!("ret {}", r.op()));
    }

    // ---- functions ----

    fn setup_locals(&mut self, locals: &[LocalDef]) {
        for l in locals {
            let ty = self.sub(&l.ty);
            let lt = if l.kind == LocalKind::Ref { "ptr".to_string() } else { self.lty(&ty) };
            let slot = self.alloca(&lt);
            self.f.slots.push(slot);
            self.f.local_tys.push(ty);
            self.f.local_kinds.push(l.kind);
        }
    }

    pub fn emit_fn(&mut self, f: FnId, targs: &[Ty], eargs: &[Eff], sym: &str) -> String {
        let def = &self.p.fns[f];
        let failable = def.eff.subst(eargs).fail;
        let ret = def.ret.subst(targs, eargs);
        self.f = Fx { targs: targs.to_vec(), eargs: eargs.to_vec(), ret: ret.clone(), failable, file: def.file, cur: "entry".into(), ..Default::default() };
        let mut params = Vec::new();
        let mut pvals = Vec::new();
        for (i, p) in def.params.iter().enumerate() {
            let pty = p.ty.subst(targs, eargs);
            let lt = if p.mode == Mode::Inout { "ptr".to_string() } else { self.lty(&pty) };
            params.push(format!("{lt} %p{i}"));
            pvals.push(V::new(lt, format!("%p{i}")));
        }
        if def.intrinsic.is_some() {
            self.intrinsic_body(f, &pvals);
        } else {
            let body = def.body.as_ref().expect("a function body");
            self.setup_locals(&body.locals);
            self.push_scope(true);
            for (i, &id) in body.params.iter().enumerate() {
                let slot = self.f.slots[id].clone();
                self.inst(&format!("store {}, ptr {slot}", pvals[i].op()));
                if def.params[i].mode == Mode::Sink {
                    let ty = self.f.local_tys[id].clone();
                    self.add_local_drop(&slot, &ty);
                }
            }
            for (cond, text) in &body.pre {
                self.push_scope(false);
                let c = self.borrow(cond);
                let bad = self.tmp();
                self.inst(&format!("{bad} = xor i1 {}, true", c.repr));
                self.trap_if(&bad, &format!("precondition failed: {text}"), cond.span);
                self.pop_scope();
            }
            if body.block.value.is_some() {
                let v = self.block_value(&body.block);
                if !self.f.terminated {
                    self.emit_return(v);
                }
            } else {
                self.block_stmts(&body.block);
            }
            if !self.f.terminated {
                if ret == Ty::Unit {
                    self.emit_return(None);
                } else {
                    self.term("unreachable");
                }
            }
        }
        let rt = self.ret_lty(&ret, failable);
        let f = std::mem::take(&mut self.f);
        format!("define internal {rt} {sym}({}) {{\nentry:\n{}{}}}\n", params.join(", "), f.allocas, f.code)
    }

    /// The environment struct of a closure: count, drop function, mark
    /// function (see `Helper::Mark`), then the captures.
    pub fn env_lty(&mut self, id: ClosureId) -> String {
        let c = &self.p.closures[id];
        let tys: Vec<Ty> = c.captures.iter().map(|(_, inner)| self.sub(&c.body.locals[*inner].ty)).collect();
        let mut parts = vec!["i64".to_string(), "ptr".to_string(), "ptr".to_string()];
        for t in &tys {
            parts.push(self.lty(t));
        }
        format!("{{ {} }}", parts.join(", "))
    }

    pub fn emit_closure(&mut self, id: ClosureId, targs: &[Ty], eargs: &[Eff], failable: bool, sym: &str) -> String {
        let c = &self.p.closures[id];
        let owner_file = if c.owner == usize::MAX { 0 } else { self.p.fns[c.owner].file };
        let ret = c.ret.subst(targs, eargs);
        self.f = Fx { targs: targs.to_vec(), eargs: eargs.to_vec(), ret: ret.clone(), failable, file: owner_file, cur: "entry".into(), ..Default::default() };
        self.setup_locals(&c.body.locals);
        let mut params = vec!["ptr %env".to_string()];
        let pids = c.params.clone();
        for (i, &pid) in pids.iter().enumerate() {
            let ty = self.f.local_tys[pid].clone();
            let lt = self.lty(&ty);
            params.push(format!("{lt} %p{i}"));
            let slot = self.f.slots[pid].clone();
            self.inst(&format!("store {lt} %p{i}, ptr {slot}"));
        }
        let env = self.env_lty(id);
        let caps = c.captures.clone();
        for (k, (_, inner)) in caps.iter().enumerate() {
            let ty = self.f.local_tys[*inner].clone();
            let lt = self.lty(&ty);
            let p = self.gep(&env, "%env", &[0, k + 3]);
            let v = self.load(&lt, &p);
            let slot = self.f.slots[*inner].clone();
            self.inst(&format!("store {}, ptr {slot}", v.op()));
        }
        self.push_scope(true);
        let block = &self.p.closures[id].body.block;
        if block.value.is_some() {
            let v = self.block_value(block);
            if !self.f.terminated {
                self.emit_return(v);
            }
        } else {
            self.block_stmts(block);
        }
        if !self.f.terminated {
            if ret == Ty::Unit {
                self.emit_return(None);
            } else {
                self.term("unreachable");
            }
        }
        let rt = self.ret_lty(&ret, failable);
        let f = std::mem::take(&mut self.f);
        format!("define internal {rt} {sym}({}) {{\nentry:\n{}{}}}\n", params.join(", "), f.allocas, f.code)
    }

    pub fn emit_closure_drop(&mut self, id: ClosureId, targs: &[Ty], eargs: &[Eff], sym: &str) -> String {
        self.f = Fx { targs: targs.to_vec(), eargs: eargs.to_vec(), cur: "entry".into(), ..Default::default() };
        let env = self.env_lty(id);
        let c = &self.p.closures[id];
        let tys: Vec<Ty> = c.captures.iter().map(|(_, inner)| self.sub(&c.body.locals[*inner].ty)).collect();
        for (k, ty) in tys.iter().enumerate() {
            let p = self.gep(&env, "%env", &[0, k + 3]);
            self.drop_ptr(&p, ty);
        }
        self.inst("call void @ovt_free(ptr %env)");
        self.term("ret void");
        let f = std::mem::take(&mut self.f);
        format!("define internal void {sym}(ptr %env) {{\nentry:\n{}{}}}\n", f.allocas, f.code)
    }

    /// Marks what a closure captured as shared, once its environment is.
    pub fn emit_closure_mark(&mut self, id: ClosureId, targs: &[Ty], eargs: &[Eff], sym: &str) -> String {
        self.f = Fx { targs: targs.to_vec(), eargs: eargs.to_vec(), cur: "entry".into(), ..Default::default() };
        let env = self.env_lty(id);
        let c = &self.p.closures[id];
        let tys: Vec<Ty> = c.captures.iter().map(|(_, inner)| self.sub(&c.body.locals[*inner].ty)).collect();
        for (k, ty) in tys.iter().enumerate() {
            if self.needs_rc(ty) {
                let p = self.gep(&env, "%env", &[0, k + 3]);
                let h = self.helper(Helper::Mark, ty);
                self.inst(&format!("call void {h}(ptr {p})"));
            }
        }
        self.term("ret void");
        let f = std::mem::take(&mut self.f);
        format!("define internal void {sym}(ptr %env) {{\nentry:\n{}{}}}\n", f.allocas, f.code)
    }

    /// Calls named function `f` through the closure calling convention.
    pub fn emit_thunk(&mut self, f: FnId, targs: &[Ty], eargs: &[Eff], failable: bool, sym: &str) -> String {
        let def = &self.p.fns[f];
        let ret = def.ret.subst(targs, eargs);
        let inner_fails = def.eff.subst(eargs).fail;
        self.f = Fx { targs: targs.to_vec(), eargs: eargs.to_vec(), ret: ret.clone(), failable, file: def.file, cur: "entry".into(), ..Default::default() };
        let target = self.fn_inst(f, targs, eargs);
        let mut params = vec!["ptr %env".to_string()];
        let mut args = Vec::new();
        let modes: Vec<(Mode, Ty)> = def.params.iter().map(|p| (p.mode, p.ty.subst(targs, eargs))).collect();
        for (i, (mode, ty)) in modes.iter().enumerate() {
            let lt = self.lty(ty);
            params.push(format!("{lt} %p{i}"));
            let v = V::new(lt, format!("%p{i}"));
            if *mode == Mode::Sink {
                // Function values borrow their arguments; a `sink` parameter needs its own copy.
                self.dup_value(&v, ty);
            }
            args.push(v.op());
        }
        let rt = self.ret_lty(&ret, inner_fails);
        let outer = self.ret_lty(&ret, failable);
        if rt == "void" {
            self.inst(&format!("call void {target}({})", args.join(", ")));
            if failable {
                self.emit_return(None);
            } else {
                self.term("ret void");
            }
        } else {
            let r = self.tmp();
            self.inst(&format!("{r} = call {rt} {target}({})", args.join(", ")));
            if inner_fails == failable {
                self.term(&format!("ret {rt} {r}"));
            } else {
                // A function that can't fail, used where one that can is expected.
                self.emit_return(Some(V::new(rt, r)));
            }
        }
        let _ = outer;
        let fx = std::mem::take(&mut self.f);
        let outer = self.ret_lty(&ret, failable);
        format!("define internal {outer} {sym}({}) {{\nentry:\n{}{}}}\n", params.join(", "), fx.allocas, fx.code)
    }

    // ---- statements ----

    /// A block's statements in their own scope (its value, if any, is the caller's business).
    pub fn block_stmts(&mut self, b: &TBlock) {
        self.push_scope(true);
        for s in &b.stmts {
            if self.f.terminated {
                break;
            }
            self.stmt(s);
        }
        self.pop_scope();
    }

    /// A block used as a value: its statements, then its value (owned), then its locals dropped.
    pub fn block_value(&mut self, b: &TBlock) -> Option<V> {
        self.push_scope(true);
        for s in &b.stmts {
            if self.f.terminated {
                break;
            }
            self.stmt(s);
        }
        let v = match &b.value {
            Some(e) if !self.f.terminated => {
                self.push_scope(false);
                let v = self.owned(e);
                self.pop_scope();
                Some(v)
            }
            _ => None,
        };
        self.pop_scope();
        if self.f.terminated { None } else { v }
    }

    pub fn stmt(&mut self, s: &TStmt) {
        match s {
            TStmt::Let { local, value } => {
                self.push_scope(false);
                let v = self.owned(value);
                self.pop_scope();
                if self.f.terminated {
                    return;
                }
                let slot = self.f.slots[*local].clone();
                self.inst(&format!("store {}, ptr {slot}", v.op()));
                let ty = self.f.local_tys[*local].clone();
                self.add_local_drop(&slot, &ty);
            }
            TStmt::LetTuple { locals, value } => {
                self.push_scope(false);
                let v = self.owned(value);
                self.pop_scope();
                if self.f.terminated {
                    return;
                }
                let tty = self.sub(&value.ty);
                let Ty::Tuple(tys) = tty else { unreachable!("checked: a tuple") };
                for (i, (l, ty)) in locals.iter().zip(&tys).enumerate() {
                    let lt = self.lty(ty);
                    let part = self.extract(&v, i, &lt);
                    match l {
                        Some(id) => {
                            let slot = self.f.slots[*id].clone();
                            self.inst(&format!("store {}, ptr {slot}", part.op()));
                            self.add_local_drop(&slot, ty);
                        }
                        None => self.drop_value(&part, ty),
                    }
                }
            }
            TStmt::Assign { place, op, value, span } => self.assign(place, *op, value, *span),
            TStmt::Expr(e) => {
                self.push_scope(false);
                let (v, owned) = self.value(e);
                if owned && !self.f.terminated {
                    let ty = self.sub(&e.ty);
                    self.drop_value(&v, &ty);
                }
                self.pop_scope();
            }
            TStmt::While { cond, body } => {
                let head = self.label("while");
                let bl = self.label("body");
                let end = self.label("end");
                self.start(&head);
                self.push_scope(false);
                let c = self.borrow(cond);
                self.pop_scope();
                self.term(&format!("br i1 {}, label %{bl}, label %{end}", c.repr));
                self.start(&bl);
                let depth = self.f.scopes.len();
                self.f.loops.push(Loop { cont: head.clone(), brk: end.clone(), cont_depth: depth, brk_depth: depth });
                self.block_stmts(body);
                self.f.loops.pop();
                self.term(&format!("br label %{head}"));
                self.start(&end);
            }
            TStmt::WhileLet { local, value, body } => {
                let head = self.label("whilelet");
                let bl = self.label("body");
                let latch = self.label("latch");
                let done = self.label("done");
                let end = self.label("end");
                self.start(&head);
                let depth = self.f.scopes.len();
                self.push_scope(false);
                let opt_ty = self.sub(&value.ty);
                let v = self.owned(value);
                let slot = self.spill(&v);
                self.add_drop(&slot, &opt_ty);
                let is_some = self.extract(&v, 0, "i1");
                self.term(&format!("br i1 {}, label %{bl}, label %{done}", is_some.repr));
                self.start(&bl);
                let inner_ty = self.f.local_tys[*local].clone();
                let lt = self.lty(&inner_ty);
                let inner = self.extract(&v, 1, &lt);
                let lslot = self.f.slots[*local].clone();
                self.inst(&format!("store {}, ptr {lslot}", inner.op()));
                self.f.loops.push(Loop { cont: latch.clone(), brk: end.clone(), cont_depth: depth + 1, brk_depth: depth });
                self.block_stmts(body);
                self.f.loops.pop();
                self.start(&latch);
                self.cleanup_from(depth);
                self.term(&format!("br label %{head}"));
                self.start(&done);
                self.cleanup_from(depth);
                self.f.scopes.pop();
                self.start(&end);
            }
            TStmt::ForRange { local, lo, hi, inclusive, body } => self.for_range(*local, lo, hi, *inclusive, body),
            TStmt::ForArray { elem, index, array, place, body } => self.for_array(*elem, *index, array, place.as_ref(), body),
            TStmt::ForMap { key, value, map, is_set, body } => self.for_map(*key, *value, map, *is_set, body),
            TStmt::Par { branches } => self.par_stmt(branches),
        }
    }

    fn for_range(&mut self, local: LocalId, lo: &TExpr, hi: &TExpr, inclusive: bool, body: &TBlock) {
        let ty = self.f.local_tys[local].clone();
        let lt = self.lty(&ty);
        let signed = int_ty(&ty).is_none_or(|k| k.signed());
        self.push_scope(false);
        let lo = self.borrow(lo);
        let hi = self.borrow(hi);
        self.pop_scope();
        let slot = self.f.slots[local].clone();
        let counter = self.alloca(&lt);
        self.inst(&format!("store {lt} {}, ptr {counter}", lo.repr));
        let head = self.label("for");
        let bl = self.label("body");
        let step = self.label("step");
        let end = self.label("end");
        self.start(&head);
        let i = self.load(&lt, &counter);
        let c = self.tmp();
        let cmp = match (inclusive, signed) {
            (true, true) => "sle",
            (true, false) => "ule",
            (false, true) => "slt",
            (false, false) => "ult",
        };
        self.inst(&format!("{c} = icmp {cmp} {lt} {}, {}", i.repr, hi.repr));
        self.term(&format!("br i1 {c}, label %{bl}, label %{end}"));
        self.start(&bl);
        self.inst(&format!("store {lt} {}, ptr {slot}", i.repr));
        let depth = self.f.scopes.len();
        self.f.loops.push(Loop { cont: step.clone(), brk: end.clone(), cont_depth: depth, brk_depth: depth });
        self.block_stmts(body);
        self.f.loops.pop();
        self.start(&step);
        let i2 = self.load(&lt, &counter);
        if inclusive {
            // Stop at the last value, so `..=` up to the largest value doesn't overflow.
            let last = self.tmp();
            let next = self.label("next");
            self.inst(&format!("{last} = icmp eq {lt} {}, {}", i2.repr, hi.repr));
            self.term(&format!("br i1 {last}, label %{end}, label %{next}"));
            self.start(&next);
        }
        let n = self.tmp();
        self.inst(&format!("{n} = add {lt} {}, 1", i2.repr));
        self.inst(&format!("store {lt} {n}, ptr {counter}"));
        self.term(&format!("br label %{head}"));
        self.start(&end);
    }

    /// Branches to `fast` when the array at `ap` is the only view of its whole
    /// buffer (and, with `room`, has capacity left), else to `slow`. Returns
    /// the buffer pointer and its `used` count, valid on the fast path.
    pub fn unique_check(&mut self, ap: &str, fast: &str, slow: &str, room: bool) -> (String, String) {
        let a = self.load("%ovt.arr", ap);
        let buf = self.extract(&a, 0, "ptr");
        let off = self.extract(&a, 1, "i64");
        let len = self.extract(&a, 2, "i64");
        let nn = self.tmp();
        self.inst(&format!("{nn} = icmp ne ptr {}, null", buf.repr));
        let check = self.label("check");
        self.term(&format!("br i1 {nn}, label %{check}, label %{slow}"));
        self.start(&check);
        let rc = self.load_rc(&buf.repr);
        let usedp = self.tmp();
        self.inst(&format!("{usedp} = getelementptr inbounds i8, ptr {}, i64 16", buf.repr));
        let used = self.load("i64", &usedp);
        let c1 = self.tmp();
        self.inst(&format!("{c1} = icmp eq i64 {}, 1", rc.repr));
        let c2 = self.tmp();
        self.inst(&format!("{c2} = icmp eq i64 {}, 0", off.repr));
        let c3 = self.tmp();
        self.inst(&format!("{c3} = icmp eq i64 {}, {}", len.repr, used.repr));
        let a1 = self.tmp();
        self.inst(&format!("{a1} = and i1 {c1}, {c2}"));
        let mut ok = self.tmp();
        self.inst(&format!("{ok} = and i1 {a1}, {c3}"));
        if room {
            let capp = self.tmp();
            self.inst(&format!("{capp} = getelementptr inbounds i8, ptr {}, i64 8", buf.repr));
            let cap = self.load("i64", &capp);
            let c4 = self.tmp();
            self.inst(&format!("{c4} = icmp slt i64 {}, {}", used.repr, cap.repr));
            let a2 = self.tmp();
            self.inst(&format!("{a2} = and i1 {ok}, {c4}"));
            ok = a2;
        }
        self.term(&format!("br i1 {ok}, label %{fast}, label %{slow}"));
        (buf.repr, used.repr)
    }

    /// Makes the array at `ap` the only view of its buffer (copy-on-write),
    /// calling the runtime only when it isn't already.
    pub fn make_unique(&mut self, ap: &str, elem: &Ty) {
        let done = self.label("unique");
        let slow = self.label("copy");
        self.unique_check(ap, &done, &slow, false);
        self.start(&slow);
        let size = self.size_const(elem);
        let dup = self.dup_fn(elem);
        let drop = self.drop_fn(elem);
        self.inst(&format!("call void @ovt_arr_unique(ptr {ap}, i64 {size}, ptr {dup}, ptr {drop})"));
        self.term(&format!("br label %{done}"));
        self.start(&done);
    }

    /// A pointer to element `i` of the array value at `arr_ptr`.
    pub fn elem_ptr(&mut self, arr_ptr: &str, i: &str, elem: &Ty) -> String {
        let a = self.load("%ovt.arr", arr_ptr);
        let buf = self.extract(&a, 0, "ptr");
        let off = self.extract(&a, 1, "i64");
        let size = self.size_const(elem);
        let idx = self.tmp();
        self.inst(&format!("{idx} = add i64 {}, {i}", off.repr));
        let bytes = self.tmp();
        self.inst(&format!("{bytes} = mul i64 {idx}, {size}"));
        let at = self.tmp();
        self.inst(&format!("{at} = add i64 {bytes}, 24"));
        let p = self.tmp();
        self.inst(&format!("{p} = getelementptr inbounds i8, ptr {}, i64 {at}", buf.repr));
        p
    }

    fn for_array(&mut self, elem: LocalId, index: Option<LocalId>, array: &TExpr, place: Option<&TPlace>, body: &TBlock) {
        let arr_ty = self.sub(&array.ty);
        let Ty::Array(et) = arr_ty.clone() else { unreachable!("checked: an array") };
        let depth = self.f.scopes.len();
        self.push_scope(false);
        let arr_ptr = match place {
            Some(p) => {
                let ptr = self.place_ptr(p, true);
                self.make_unique(&ptr, &et);
                ptr
            }
            None => {
                // Iterate over a copy, so the body may change the original.
                let v = self.owned(array);
                let slot = self.spill(&v);
                self.add_drop(&slot, &arr_ty);
                slot
            }
        };
        let a = self.load("%ovt.arr", &arr_ptr);
        let len = self.extract(&a, 2, "i64");
        let counter = self.alloca("i64");
        self.inst(&format!("store i64 0, ptr {counter}"));
        let head = self.label("for");
        let bl = self.label("body");
        let step = self.label("step");
        let end = self.label("end");
        self.start(&head);
        let i = self.load("i64", &counter);
        let c = self.tmp();
        self.inst(&format!("{c} = icmp slt i64 {}, {}", i.repr, len.repr));
        self.term(&format!("br i1 {c}, label %{bl}, label %{end}"));
        self.start(&bl);
        let ep = self.elem_ptr(&arr_ptr, &i.repr, &et);
        let eslot = self.f.slots[elem].clone();
        if place.is_some() {
            self.inst(&format!("store ptr {ep}, ptr {eslot}"));
        } else {
            let lt = self.lty(&et);
            let ev = self.load(&lt, &ep);
            self.inst(&format!("store {}, ptr {eslot}", ev.op()));
        }
        if let Some(ix) = index {
            let islot = self.f.slots[ix].clone();
            self.inst(&format!("store i64 {}, ptr {islot}", i.repr));
        }
        let inner = self.f.scopes.len();
        self.f.loops.push(Loop { cont: step.clone(), brk: end.clone(), cont_depth: inner, brk_depth: inner });
        self.block_stmts(body);
        self.f.loops.pop();
        self.start(&step);
        let i2 = self.load("i64", &counter);
        let n = self.tmp();
        self.inst(&format!("{n} = add i64 {}, 1", i2.repr));
        self.inst(&format!("store i64 {n}, ptr {counter}"));
        self.term(&format!("br label %{head}"));
        self.start(&end);
        self.pop_scope();
        let _ = depth;
    }

    fn for_map(&mut self, key: LocalId, value: Option<LocalId>, map: &TExpr, is_set: bool, body: &TBlock) {
        let mty = self.sub(&map.ty);
        self.push_scope(false);
        let v = self.owned(map);
        let slot = self.spill(&v);
        self.add_drop(&slot, &mty);
        let (kty, vty, map_ty) = match &mty {
            Ty::Adt(_, args) if is_set => (args[0].clone(), Ty::Bool, Ty::Adt(self.p.known.map, vec![args[0].clone(), Ty::Bool])),
            Ty::Adt(_, args) => (args[0].clone(), args[1].clone(), mty.clone()),
            _ => unreachable!("checked: a map or set"),
        };
        let map_lt = self.lty(&map_ty);
        let map_ptr = if is_set {
            let st = self.lty(&mty);
            self.gep(&st, &slot, &[0, 0])
        } else {
            slot.clone()
        };
        // Map fields: _keys, _vals, _live, _slots, _count.
        let keys = self.gep(&map_lt, &map_ptr, &[0, 0]);
        let vals = self.gep(&map_lt, &map_ptr, &[0, 1]);
        let live = self.gep(&map_lt, &map_ptr, &[0, 2]);
        let ka = self.load("%ovt.arr", &keys);
        let len = self.extract(&ka, 2, "i64");
        let counter = self.alloca("i64");
        self.inst(&format!("store i64 0, ptr {counter}"));
        let head = self.label("for");
        let check = self.label("live");
        let bl = self.label("body");
        let step = self.label("step");
        let end = self.label("end");
        self.start(&head);
        let i = self.load("i64", &counter);
        let c = self.tmp();
        self.inst(&format!("{c} = icmp slt i64 {}, {}", i.repr, len.repr));
        self.term(&format!("br i1 {c}, label %{check}, label %{end}"));
        self.start(&check);
        let lp = self.elem_ptr(&live, &i.repr, &Ty::Bool);
        let is_live = self.load("i1", &lp);
        self.term(&format!("br i1 {}, label %{bl}, label %{step}", is_live.repr));
        self.start(&bl);
        let kp = self.elem_ptr(&keys, &i.repr, &kty);
        let klt = self.lty(&kty);
        let kv = self.load(&klt, &kp);
        let kslot = self.f.slots[key].clone();
        self.inst(&format!("store {}, ptr {kslot}", kv.op()));
        if let Some(vl) = value {
            let vp = self.elem_ptr(&vals, &i.repr, &vty);
            let vlt = self.lty(&vty);
            let vv = self.load(&vlt, &vp);
            let vslot = self.f.slots[vl].clone();
            self.inst(&format!("store {}, ptr {vslot}", vv.op()));
        }
        let inner = self.f.scopes.len();
        self.f.loops.push(Loop { cont: step.clone(), brk: end.clone(), cont_depth: inner, brk_depth: inner });
        self.block_stmts(body);
        self.f.loops.pop();
        self.start(&step);
        let i2 = self.load("i64", &counter);
        let n = self.tmp();
        self.inst(&format!("{n} = add i64 {}, 1", i2.repr));
        self.inst(&format!("store i64 {n}, ptr {counter}"));
        self.term(&format!("br label %{head}"));
        self.start(&end);
        self.pop_scope();
    }

    fn assign(&mut self, place: &TPlace, op: Option<crate::ast::BinOp>, value: &TExpr, span: Span) {
        use crate::ast::BinOp;
        let ty = self.sub(&value.ty);
        self.push_scope(false);
        match op {
            None => {
                let v = self.owned(value);
                if self.f.terminated {
                    self.pop_scope();
                    return;
                }
                let p = self.place_ptr(place, true);
                let lt = self.lty(&ty);
                if self.needs_rc(&ty) {
                    let old = self.load(&lt, &p);
                    self.inst(&format!("store {}, ptr {p}", v.op()));
                    self.drop_value(&old, &ty);
                } else {
                    self.inst(&format!("store {}, ptr {p}", v.op()));
                }
            }
            Some(BinOp::Add) if ty == Ty::Str => {
                // `s += t` appends in place when `s` holds the only copy.
                let t = self.borrow(value);
                let tp = self.spill(&t);
                let p = self.place_ptr(place, true);
                self.inst(&format!("call void @ovt_str_append(ptr {p}, ptr {tp})"));
            }
            Some(op) => {
                let rhs = self.borrow(value);
                let p = self.place_ptr(place, true);
                let lt = self.lty(&ty);
                let old = self.load(&lt, &p);
                let new = self.arith(op, &ty, &old, &rhs, span);
                self.inst(&format!("store {}, ptr {p}", new.op()));
            }
        }
        self.pop_scope();
    }
}
