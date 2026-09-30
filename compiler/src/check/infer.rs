//! Inference variables and unification, within one function body.

use crate::types::{Eff, FnTy, Ty};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VarKind {
    Any,
    /// An integer literal: becomes any number type, `int` by default.
    IntLit,
    /// A float literal: becomes a float type, `f64` by default.
    FloatLit,
}

#[derive(Clone, Debug)]
enum State {
    Unbound(VarKind),
    Bound(Ty),
}

#[derive(Default)]
pub struct Infer {
    vars: Vec<State>,
    effs: Vec<Option<Eff>>,
}

impl Infer {
    pub fn fresh(&mut self, kind: VarKind) -> Ty {
        self.vars.push(State::Unbound(kind));
        Ty::Var(self.vars.len() as u32 - 1)
    }

    pub fn fresh_eff(&mut self) -> Eff {
        self.effs.push(None);
        Eff { vars: vec![self.effs.len() as u32 - 1], ..Eff::default() }
    }

    pub fn kind(&self, v: u32) -> Option<VarKind> {
        match &self.vars[v as usize] {
            State::Unbound(k) => Some(*k),
            State::Bound(_) => None,
        }
    }

    /// Follows bound variables at the top of `t`.
    pub fn shallow(&self, t: &Ty) -> Ty {
        let mut t = t.clone();
        while let Ty::Var(v) = t {
            match &self.vars[v as usize] {
                State::Bound(b) => t = b.clone(),
                State::Unbound(_) => break,
            }
        }
        t
    }

    /// Replaces every bound variable in `t`.
    pub fn resolve(&self, t: &Ty) -> Ty {
        match self.shallow(t) {
            Ty::Array(e) => Ty::Array(Box::new(self.resolve(&e))),
            Ty::Opt(e) => Ty::Opt(Box::new(self.resolve(&e))),
            Ty::Ptr(e) => Ty::Ptr(Box::new(self.resolve(&e))),
            Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| self.resolve(t)).collect()),
            Ty::Adt(id, ts) => Ty::Adt(id, ts.iter().map(|t| self.resolve(t)).collect()),
            Ty::Fn(f) => Ty::Fn(Box::new(FnTy {
                params: f.params.iter().map(|t| self.resolve(t)).collect(),
                ret: self.resolve(&f.ret),
                eff: self.resolve_eff(&f.eff),
            })),
            other => other,
        }
    }

    pub fn resolve_eff(&self, e: &Eff) -> Eff {
        let mut out = Eff { io: e.io, fail: e.fail, params: e.params.clone(), vars: Vec::new() };
        for v in &e.vars {
            match &self.effs[*v as usize] {
                Some(bound) => out = out.union(&self.resolve_eff(bound)),
                None => out.vars.push(*v),
            }
        }
        out
    }

    /// Makes an effect variable stand for `e` (added to anything it already stands for).
    pub fn bind_eff(&mut self, v: u32, e: &Eff) {
        let e = self.resolve_eff(e);
        if e.vars.contains(&v) {
            return;
        }
        let merged = match &self.effs[v as usize] {
            Some(prev) => prev.union(&e),
            None => e,
        };
        self.effs[v as usize] = Some(merged);
    }

    fn occurs(&self, v: u32, t: &Ty) -> bool {
        let mut found = false;
        self.resolve(t).walk(&mut |x| found |= *x == Ty::Var(v));
        found
    }

    fn bind(&mut self, v: u32, t: Ty) -> bool {
        if t == Ty::Var(v) {
            return true;
        }
        if self.occurs(v, &t) {
            return false;
        }
        self.vars[v as usize] = State::Bound(t);
        true
    }

    /// Makes `a` and `b` the same type. Returns false if they can't be.
    pub fn unify(&mut self, a: &Ty, b: &Ty) -> bool {
        let a = self.shallow(a);
        let b = self.shallow(b);
        match (&a, &b) {
            (Ty::Error, _) | (_, Ty::Error) | (Ty::Never, _) | (_, Ty::Never) => true,
            (Ty::Var(x), Ty::Var(y)) => {
                if x == y {
                    return true;
                }
                let (kx, ky) = (self.kind(*x).unwrap(), self.kind(*y).unwrap());
                // Keep the more specific kind.
                let (from, to) = match (kx, ky) {
                    (VarKind::Any, _) => (*x, *y),
                    (_, VarKind::Any) => (*y, *x),
                    (VarKind::IntLit, VarKind::FloatLit) => (*x, *y),
                    (VarKind::FloatLit, VarKind::IntLit) => (*y, *x),
                    _ => (*x, *y),
                };
                self.bind(from, Ty::Var(to))
            }
            (Ty::Var(x), t) | (t, Ty::Var(x)) => match self.kind(*x).unwrap() {
                VarKind::Any => self.bind(*x, t.clone()),
                VarKind::IntLit => t.is_numeric() && self.bind(*x, t.clone()),
                VarKind::FloatLit => matches!(t, Ty::Float(_)) && self.bind(*x, t.clone()),
            },
            (Ty::Array(x), Ty::Array(y)) | (Ty::Opt(x), Ty::Opt(y)) | (Ty::Ptr(x), Ty::Ptr(y)) => self.unify(x, y),
            (Ty::Tuple(xs), Ty::Tuple(ys)) => xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| self.unify(x, y)),
            (Ty::Adt(i, xs), Ty::Adt(j, ys)) => i == j && xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| self.unify(x, y)),
            (Ty::Fn(f), Ty::Fn(g)) => {
                f.params.len() == g.params.len()
                    && f.params.iter().zip(&g.params).all(|(x, y)| self.unify(x, y))
                    && self.unify(&f.ret, &g.ret)
                    && self.unify_eff(&f.eff, &g.eff)
            }
            _ => a == b,
        }
    }

    /// Effects of two function types that must match. An effect variable on
    /// either side takes on the other side's effects.
    pub fn unify_eff(&mut self, a: &Eff, b: &Eff) -> bool {
        let a = self.resolve_eff(a);
        let b = self.resolve_eff(b);
        if let Some(&v) = b.vars.first() {
            self.bind_eff(v, &Eff { vars: Vec::new(), ..a.clone() });
            return true;
        }
        if let Some(&v) = a.vars.first() {
            self.bind_eff(v, &Eff { vars: Vec::new(), ..b.clone() });
            return true;
        }
        a.io == b.io && a.fail == b.fail && a.params == b.params
    }

    /// Whether a value with effects `actual` may be used where `allowed` is
    /// expected; variables in `allowed` grow to cover `actual`.
    pub fn sub_eff(&mut self, actual: &Eff, allowed: &Eff) -> bool {
        let actual = self.resolve_eff(actual);
        let allowed = self.resolve_eff(allowed);
        if let Some(&v) = allowed.vars.first() {
            let extra = Eff {
                io: actual.io && !allowed.io,
                fail: actual.fail && !allowed.fail,
                params: actual.params.iter().filter(|p| !allowed.params.contains(p)).copied().collect(),
                vars: actual.vars.clone(),
            };
            self.bind_eff(v, &extra);
            return true;
        }
        (!actual.io || allowed.io) && (!actual.fail || allowed.fail) && actual.params.iter().all(|p| allowed.params.contains(p))
    }

    /// Unbound literal variables become `int` and `f64`.
    pub fn default_literals(&mut self) {
        for i in 0..self.vars.len() {
            if let State::Unbound(k) = self.vars[i] {
                match k {
                    VarKind::IntLit => self.vars[i] = State::Bound(Ty::INT),
                    VarKind::FloatLit => self.vars[i] = State::Bound(Ty::F64),
                    VarKind::Any => {}
                }
            }
        }
        for e in self.effs.iter_mut() {
            if e.is_none() {
                *e = Some(Eff::pure());
            }
        }
    }
}
