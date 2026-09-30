//! Resources: a type with a `drop` block, or one holding such a type, is a
//! resource. A resource can't be copied, so using it where a value is taken
//! (a `let`, a `sink` argument, a field of a new value, a `return`) moves it,
//! and the variable can't be used again. Reading it (a plain argument, a
//! method call, a field) doesn't move it.
//!
//! This pass runs over the finished bodies. Where it treats a use as a move
//! follows codegen: `owned` on a local moves a resource, and clears the
//! local's drop flag (see codegen/func.rs).
//!
//! Resources can't be elements of arrays, maps or sets, type arguments (except
//! to `Shared` and `Chan`), or captured by closures: all of those copy.

use super::*;
use crate::tir::*;
use std::collections::{HashMap, HashSet};

impl<'a> Checker<'a> {
    /// The types that are resources, as a fixed point.
    pub fn resource_set(&self) -> HashSet<AdtId> {
        let mut set: HashSet<AdtId> = self.adts.iter().enumerate().filter(|(_, a)| a.drop.is_some()).map(|(i, _)| i).collect();
        loop {
            let before = set.len();
            for (i, a) in self.adts.iter().enumerate() {
                if set.contains(&i) {
                    continue;
                }
                let tys = a.fields().iter().chain(a.variants().iter().flat_map(|v| v.fields.iter())).map(|f| &f.ty);
                if tys.into_iter().any(|t| holds_resource(t, &set)) {
                    set.insert(i);
                }
            }
            if set.len() == before {
                return set;
            }
        }
    }

    /// Checks every body for copies of resources and uses after a move.
    pub fn check_resources(&mut self) {
        let set = self.resource_set();
        if set.is_empty() {
            return;
        }
        // Signatures and bodies.
        for f in 0..self.fns.len() {
            let def = &self.fns[f];
            let (file, span) = (def.file, def.span);
            let sig: Vec<Ty> = def.params.iter().map(|p| p.ty.clone()).chain(std::iter::once(def.ret.clone())).collect();
            for t in sig {
                if let Some(why) = self.misused(&t, &set) {
                    self.err(file, span, format!("a parameter or the result of `{}` {why}", self.fns[f].name));
                }
            }
            if self.fns[f].body.is_some() {
                let body = self.fns[f].body.take().unwrap();
                let mut m = Moves::new(&set, &body, file);
                m.block(self, &body.block);
                self.fns[f].body = Some(body);
                for (span, msg) in m.errors {
                    self.err(file, span, msg);
                }
            }
        }
        for c in 0..self.closures.len() {
            let owner = self.closures[c].owner;
            let file = if owner < self.fns.len() { self.fns[owner].file } else { continue };
            let body = std::mem::replace(&mut self.closures[c].body, Body { locals: Vec::new(), params: Vec::new(), pre: Vec::new(), block: TBlock { stmts: Vec::new(), value: None } });
            let mut m = Moves::new(&set, &body, file);
            m.block(self, &body.block);
            self.closures[c].body = body;
            for (span, msg) in m.errors {
                self.err(file, span, msg);
            }
        }
    }

    /// Fields that hold resources where they can't be, reported at the field.
    pub fn check_resource_fields(&mut self, items: &[(AdtId, &'a ast::Item, FileId)]) {
        let set = self.resource_set();
        if set.is_empty() {
            return;
        }
        for &(id, item, fi) in items {
            let spans: Vec<Span> = match &item.kind {
                ItemKind::Type(ast::TypeDecl { body: TypeBody::Struct { fields, .. }, .. }) => fields.iter().map(|f| f.span).collect(),
                ItemKind::Enum(e) => e.variants.iter().flat_map(|v| v.fields.iter().flatten()).map(|f| f.span).collect(),
                _ => continue,
            };
            let a = &self.adts[id];
            let tys: Vec<Ty> = a.fields().iter().chain(a.variants().iter().flat_map(|v| v.fields.iter())).map(|f| f.ty.clone()).collect();
            for (t, span) in tys.iter().zip(spans) {
                if let Some(why) = self.misused(t, &set) {
                    self.err(fi, span, format!("this field {why}"));
                }
            }
        }
    }

    /// Why `t` holds a resource where one can't be, if it does.
    fn misused(&self, t: &Ty, set: &HashSet<AdtId>) -> Option<String> {
        match t {
            Ty::Array(e) if holds_resource(e, set) => Some(format!("is an array of `{}`, a resource; arrays copy their elements, so they can't hold resources", self.ty_name(e))),
            Ty::Adt(id, args) if *id == self.known.map || *id == self.known.set => {
                let e = args.iter().find(|a| holds_resource(a, set))?;
                Some(format!("holds `{}`, a resource, in a `{}`; collections copy their elements, so they can't hold resources", self.ty_name(e), self.adts[*id].name))
            }
            Ty::Adt(id, args) if *id != self.known.shared && *id != self.known.chan => {
                if let Some(e) = args.iter().find(|a| holds_resource(a, set)) {
                    return Some(format!("passes `{}`, a resource, as a type argument; only `Shared` and `Chan` can hold resources", self.ty_name(e)));
                }
                args.iter().find_map(|a| self.misused(a, set))
            }
            Ty::Array(e) | Ty::Opt(e) | Ty::Ptr(e) => self.misused(e, set),
            Ty::Tuple(ts) => ts.iter().find_map(|t| self.misused(t, set)),
            Ty::Fn(f) => f.params.iter().chain(std::iter::once(&f.ret)).find_map(|t| self.misused(t, set)),
            _ => None,
        }
    }
}

fn holds_resource(t: &Ty, set: &HashSet<AdtId>) -> bool {
    match t {
        Ty::Adt(id, _) => set.contains(id),
        Ty::Opt(e) => holds_resource(e, set),
        Ty::Tuple(ts) => ts.iter().any(|t| holds_resource(t, set)),
        _ => false,
    }
}

/// Whether a block ends by leaving: its moves don't reach the code after it.
fn diverges(b: &TBlock) -> bool {
    if let Some(v) = &b.value {
        return v.ty == Ty::Never;
    }
    matches!(b.stmts.last(), Some(TStmt::Expr(e)) if e.ty == Ty::Never)
}

struct Moves<'s> {
    set: &'s HashSet<AdtId>,
    kinds: Vec<LocalKind>,
    names: Vec<String>,
    tys: Vec<Ty>,
    params: HashSet<LocalId>,
    file: FileId,
    /// Resource locals moved away, and where.
    moved: HashMap<LocalId, Span>,
    /// How many loops enclose each local's declaration, and the code now.
    depth: HashMap<LocalId, usize>,
    loops: usize,
    in_return: bool,
    reported: HashSet<LocalId>,
    errors: Vec<(Span, String)>,
}

impl<'s> Moves<'s> {
    fn new(set: &'s HashSet<AdtId>, body: &Body, file: FileId) -> Self {
        Moves {
            set,
            kinds: body.locals.iter().map(|l| l.kind).collect(),
            names: body.locals.iter().map(|l| l.name.clone()).collect(),
            tys: body.locals.iter().map(|l| l.ty.clone()).collect(),
            params: body.params.iter().copied().collect(),
            file,
            moved: HashMap::new(),
            depth: HashMap::new(),
            loops: 0,
            in_return: false,
            reported: HashSet::new(),
            errors: Vec::new(),
        }
    }

    fn resource(&self, t: &Ty) -> bool {
        holds_resource(t, self.set)
    }

    fn line(&self, c: &Checker, span: Span) -> u32 {
        c.src(self.file).line_col(span.lo).0
    }

    fn declare(&mut self, id: LocalId) {
        self.depth.insert(id, self.loops);
        self.moved.remove(&id);
    }

    fn block(&mut self, c: &Checker, b: &TBlock) {
        for s in &b.stmts {
            self.stmt(c, s);
        }
        if let Some(v) = &b.value {
            self.expr(c, v, true);
        }
    }

    /// Runs `f` on a copy of the state for a branch; returns the moves it made
    /// unless the branch leaves.
    fn branch(&mut self, c: &Checker, f: impl FnOnce(&mut Self, &Checker) -> bool) -> Option<HashMap<LocalId, Span>> {
        let saved = self.moved.clone();
        let leaves = f(self, c);
        let after = std::mem::replace(&mut self.moved, saved);
        if leaves { None } else { Some(after) }
    }

    /// After branches: a local moved on any path that continues counts as moved.
    fn merge(&mut self, outcomes: Vec<Option<HashMap<LocalId, Span>>>) {
        for m in outcomes.into_iter().flatten() {
            for (k, v) in m {
                self.moved.entry(k).or_insert(v);
            }
        }
    }

    fn loop_body(&mut self, c: &Checker, b: &TBlock) {
        self.loops += 1;
        let outcome = self.branch(c, |m, c| {
            m.block(c, b);
            diverges(b)
        });
        self.loops -= 1;
        self.merge(vec![outcome]);
    }

    fn stmt(&mut self, c: &Checker, s: &TStmt) {
        match s {
            TStmt::Let { local, value } => {
                self.expr(c, value, true);
                self.declare(*local);
            }
            TStmt::LetTuple { locals, value } => {
                self.expr(c, value, true);
                for l in locals.iter().flatten() {
                    self.declare(*l);
                }
            }
            TStmt::Assign { place, op, value, .. } => {
                self.expr(c, value, op.is_none());
                match place {
                    // Assigning a whole local gives it a value again.
                    TPlace::Local(id) if op.is_none() => {
                        self.moved.remove(id);
                    }
                    p => self.place(c, p),
                }
            }
            TStmt::Expr(e) => self.expr(c, e, false),
            TStmt::While { cond, body } => {
                self.loops += 1;
                self.expr(c, cond, false);
                self.loops -= 1;
                self.loop_body(c, body);
            }
            TStmt::WhileLet { local, value, body } => {
                self.loops += 1;
                self.expr(c, value, true);
                self.declare(*local);
                self.loops -= 1;
                self.loop_body(c, body);
            }
            TStmt::ForRange { local, lo, hi, body, .. } => {
                self.expr(c, lo, false);
                self.expr(c, hi, false);
                self.declare(*local);
                self.loop_body(c, body);
            }
            TStmt::ForArray { elem, index, array, place, body } => {
                self.expr(c, array, false);
                if let Some(p) = place {
                    self.place(c, p);
                }
                self.declare(*elem);
                if let Some(i) = index {
                    self.declare(*i);
                }
                self.loop_body(c, body);
            }
            TStmt::ForMap { key, value, map, body, .. } => {
                self.expr(c, map, false);
                self.declare(*key);
                if let Some(v) = value {
                    self.declare(*v);
                }
                self.loop_body(c, body);
            }
            TStmt::Par { branches } => {
                for b in branches {
                    self.expr(c, &b.closure, false);
                    for o in &b.outs {
                        self.declare(*o);
                    }
                }
            }
        }
    }

    fn place(&mut self, c: &Checker, p: &TPlace) {
        match p {
            TPlace::Local(id) => self.used(c, *id, Span::default(), false),
            TPlace::Field(b, _) => self.place(c, b),
            TPlace::Index(b, i) => {
                self.place(c, b);
                self.expr(c, i, false);
            }
        }
    }

    /// A use of local `id`; `take` if it's used as an owned value.
    fn used(&mut self, c: &Checker, id: LocalId, span: Span, take: bool) {
        if !self.resource(&self.tys[id].clone()) {
            return;
        }
        let name = self.names[id].clone();
        if let Some(at) = self.moved.get(&id).copied() {
            if self.reported.insert(id) {
                let line = self.line(c, at);
                self.errors.push((span, format!("`{name}` was moved away on line {line}, so it can't be used here; `{}` is a resource, which can't be copied", c.ty_name(&self.tys[id]))));
            }
            return;
        }
        if !take {
            return;
        }
        if self.kinds[id] != LocalKind::Owned {
            let why = if self.params.contains(&id) && self.kinds[id] == LocalKind::Borrowed {
                format!("`{name}` is a parameter the caller keeps; to take it, declare it `{name}: sink {}`", c.ty_name(&self.tys[id]))
            } else if self.kinds[id] == LocalKind::Ref {
                format!("`{name}` is an `inout` place; it can be changed or assigned, but not moved away")
            } else {
                format!("`{name}` is bound to part of another value, so it can't be moved away; move the whole value")
            };
            self.errors.push((span, format!("{why} (`{}` is a resource, which can't be copied)", c.ty_name(&self.tys[id]))));
            return;
        }
        if self.loops > self.depth.get(&id).copied().unwrap_or(0) && !self.in_return {
            self.errors.push((span, format!("`{name}` is moved inside a loop, so it would be gone the next time around; move it after the loop, or declare it inside")));
            return;
        }
        self.moved.insert(id, span);
    }

    fn args(&mut self, c: &Checker, args: &[TArg]) {
        for a in args {
            match a.mode {
                Mode::Sink => self.expr(c, &a.expr, true),
                Mode::Read if a.copy => self.expr(c, &a.expr, true),
                Mode::Read => self.expr(c, &a.expr, false),
                Mode::Inout => self.expr(c, &a.expr, false),
            }
        }
    }

    /// An expression; `owned` if its value is taken rather than read.
    fn expr(&mut self, c: &Checker, e: &TExpr, owned: bool) {
        match &e.kind {
            TK::Local(id) => self.used(c, *id, e.span, owned),
            TK::Call { f, targs, args, .. } => {
                let def = &c.fns[*f];
                let handle = c.files[def.file].std && (def.name.starts_with("Shared.") || def.name.starts_with("Chan."));
                if !handle {
                    if let Some(t) = targs.iter().find(|t| self.resource(t)) {
                        self.errors.push((e.span, format!("`{}` can't take `{}`, a resource, as a type argument: generic code copies its values; only `Shared` and `Chan` can hold resources", def.name, c.ty_name(t))));
                    }
                }
                self.args(c, args);
            }
            TK::CallValue { callee, args } => {
                self.expr(c, callee, false);
                self.args(c, args);
            }
            TK::Lock { shared, body, .. } => {
                self.expr(c, shared, false);
                self.block(c, body);
            }
            TK::Closure { id } => {
                for &(outer, _) in &c.closures[*id].captures {
                    if self.resource(&self.tys[outer].clone()) {
                        let name = self.names[outer].clone();
                        self.errors.push((e.span, format!("a closure can't use `{name}`: closures copy what they use, and `{}` is a resource", c.ty_name(&self.tys[outer]))));
                    } else {
                        self.used(c, outer, e.span, false);
                    }
                }
            }
            TK::Struct { fields, .. } | TK::Variant { fields, .. } => fields.iter().for_each(|f| self.expr(c, f, true)),
            TK::Tuple(items) | TK::Array(items) => items.iter().for_each(|i| self.expr(c, i, true)),
            TK::Some(x) => self.expr(c, x, true),
            TK::Field { base, .. } => {
                self.expr(c, base, false);
                if owned && self.resource(&e.ty) {
                    self.errors.push((e.span, format!("a field that's a resource (`{}`) can't be moved out of its value; move the whole value", c.ty_name(&e.ty))));
                }
            }
            TK::Index { base, index } => {
                self.expr(c, base, false);
                self.expr(c, index, false);
            }
            TK::Slice { base, lo, hi } => {
                self.expr(c, base, false);
                for x in lo.iter().chain(hi.iter()) {
                    self.expr(c, x, false);
                }
            }
            TK::Unary(_, x) | TK::Convert(x) | TK::Trap(x) | TK::Hash(x) => self.expr(c, x, false),
            TK::Print { value, .. } | TK::Dbg { value, .. } => self.expr(c, value, false),
            TK::Binary(_, l, r) => {
                self.expr(c, l, false);
                self.expr(c, r, false);
            }
            TK::Interp(parts) => parts.iter().for_each(|p| self.expr(c, p, false)),
            TK::If { cond, then, els } => {
                self.expr(c, cond, false);
                let a = self.branch(c, |m, c| {
                    m.block(c, then);
                    diverges(then)
                });
                let b = self.branch(c, |m, c| {
                    if let Some(els) = els {
                        m.block(c, els);
                        diverges(els)
                    } else {
                        false
                    }
                });
                self.merge(vec![a, b]);
            }
            TK::IfLet { local, value, then, els } => {
                self.expr(c, value, true);
                self.declare(*local);
                let a = self.branch(c, |m, c| {
                    m.block(c, then);
                    diverges(then)
                });
                let b = self.branch(c, |m, c| {
                    if let Some(els) = els {
                        m.block(c, els);
                        diverges(els)
                    } else {
                        false
                    }
                });
                self.merge(vec![a, b]);
            }
            TK::Match { scrut, arms } => {
                self.expr(c, scrut, false);
                let mut outs = Vec::new();
                for arm in arms {
                    outs.push(self.branch(c, |m, c| {
                        if let Some(g) = &arm.guard {
                            m.expr(c, g, false);
                        }
                        m.expr(c, &arm.body, true);
                        arm.body.ty == Ty::Never
                    }));
                }
                self.merge(outs);
            }
            TK::Block(b) => self.block(c, b),
            TK::Return(v) => {
                if let Some(v) = v {
                    let saved = std::mem::replace(&mut self.in_return, true);
                    self.expr(c, v, true);
                    self.in_return = saved;
                }
            }
            TK::Try(x) => self.expr(c, x, true),
            TK::ElseOpt { value, alt } | TK::ElseFail { value, alt } => {
                self.expr(c, value, true);
                let a = self.branch(c, |m, c| {
                    m.expr(c, alt, true);
                    alt.ty == Ty::Never
                });
                self.merge(vec![a]);
            }
            TK::Catch { value, local, body } => {
                self.expr(c, value, true);
                self.declare(*local);
                let a = self.branch(c, |m, c| {
                    m.block(c, body);
                    diverges(body)
                });
                self.merge(vec![a]);
            }
            TK::Fail { kind, msg } => {
                self.expr(c, kind, false);
                self.expr(c, msg, true);
            }
            TK::Assert { cond, msg, .. } => {
                self.expr(c, cond, false);
                if let Some(m) = msg {
                    self.expr(c, m, false);
                }
            }
            TK::ExpectEq { left, right } => {
                self.expr(c, left, false);
                self.expr(c, right, false);
            }
            TK::ExpectFail { value, kind } => {
                self.expr(c, value, false);
                if let Some(k) = kind {
                    self.expr(c, k, false);
                }
            }
            TK::ExpectTrue { cond } => self.expr(c, cond, false),
            TK::Int(_) | TK::Float(_) | TK::Bool(_) | TK::Str(_) | TK::Unit | TK::None | TK::FnValue { .. } | TK::Break | TK::Continue | TK::Todo => {}
        }
    }
}
