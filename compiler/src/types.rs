//! Types, and effects as part of function types.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IntTy {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
}

impl IntTy {
    pub fn bits(self) -> u32 {
        match self {
            IntTy::I8 | IntTy::U8 => 8,
            IntTy::I16 | IntTy::U16 => 16,
            IntTy::I32 | IntTy::U32 => 32,
            IntTy::I64 | IntTy::U64 => 64,
        }
    }

    pub fn signed(self) -> bool {
        matches!(self, IntTy::I8 | IntTy::I16 | IntTy::I32 | IntTy::I64)
    }

    /// The type's name in Overt source; `int` is `i64`.
    pub fn name(self) -> &'static str {
        match self {
            IntTy::I8 => "i8",
            IntTy::I16 => "i16",
            IntTy::I32 => "i32",
            IntTy::I64 => "int",
            IntTy::U8 => "u8",
            IntTy::U16 => "u16",
            IntTy::U32 => "u32",
            IntTy::U64 => "u64",
        }
    }

    pub fn min_value(self) -> i128 {
        if self.signed() { -(1i128 << (self.bits() - 1)) } else { 0 }
    }

    pub fn max_value(self) -> i128 {
        if self.signed() { (1i128 << (self.bits() - 1)) - 1 } else { (1i128 << self.bits()) - 1 }
    }

    pub fn from_name(n: &str) -> Option<IntTy> {
        Some(match n {
            "int" | "i64" => IntTy::I64,
            "i32" => IntTy::I32,
            "i16" => IntTy::I16,
            "i8" => IntTy::I8,
            "u64" => IntTy::U64,
            "u32" => IntTy::U32,
            "u16" => IntTy::U16,
            "u8" => IntTy::U8,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FloatTy {
    F32,
    F64,
}

impl FloatTy {
    pub fn name(self) -> &'static str {
        match self {
            FloatTy::F32 => "f32",
            FloatTy::F64 => "f64",
        }
    }
}

pub type AdtId = usize;
pub type FnId = usize;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Ty {
    Int(IntTy),
    Float(FloatTy),
    Bool,
    Str,
    Dur,
    #[default]
    Unit,
    /// The type of expressions that don't finish: `return`, `trap(...)`.
    Never,
    /// An expression that already produced an error. Accepted everywhere, so
    /// one mistake doesn't cause a cascade of messages.
    Error,
    Array(Box<Ty>),
    Tuple(Vec<Ty>),
    Opt(Box<Ty>),
    /// A struct or enum with its type arguments.
    Adt(AdtId, Vec<Ty>),
    Fn(Box<FnTy>),
    /// A raw pointer, `*T`, for C interop.
    Ptr(Box<Ty>),
    /// A generic parameter of the enclosing declaration, by position.
    Param(u32),
    /// An inference variable, only inside the checker.
    Var(u32),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FnTy {
    pub params: Vec<Ty>,
    pub ret: Ty,
    pub eff: Eff,
}

/// A set of effects: `io`, `fail`, and effect parameters or variables that
/// stand for effects supplied by a caller.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Eff {
    pub io: bool,
    pub fail: bool,
    /// Effect parameters (`!E`) of the enclosing declaration, by position.
    pub params: Vec<u32>,
    /// Effect inference variables, only inside the checker.
    pub vars: Vec<u32>,
}

impl Eff {
    pub fn pure() -> Eff {
        Eff::default()
    }

    pub fn is_pure(&self) -> bool {
        !self.io && !self.fail && self.params.is_empty() && self.vars.is_empty()
    }

    pub fn union(&self, other: &Eff) -> Eff {
        let mut e = self.clone();
        e.io |= other.io;
        e.fail |= other.fail;
        for p in &other.params {
            if !e.params.contains(p) {
                e.params.push(*p);
            }
        }
        for v in &other.vars {
            if !e.vars.contains(v) {
                e.vars.push(*v);
            }
        }
        e.params.sort();
        e.vars.sort();
        e
    }
}

impl Ty {
    pub const INT: Ty = Ty::Int(IntTy::I64);
    pub const U8: Ty = Ty::Int(IntTy::U8);
    pub const F64: Ty = Ty::Float(FloatTy::F64);

    pub fn is_int(&self) -> bool {
        matches!(self, Ty::Int(_))
    }

    pub fn is_numeric(&self) -> bool {
        matches!(self, Ty::Int(_) | Ty::Float(_))
    }

    pub fn is_bad(&self) -> bool {
        matches!(self, Ty::Error | Ty::Never)
    }

    pub fn array(elem: Ty) -> Ty {
        Ty::Array(Box::new(elem))
    }

    pub fn opt(inner: Ty) -> Ty {
        Ty::Opt(Box::new(inner))
    }

    pub fn has_vars(&self) -> bool {
        let mut found = false;
        self.walk(&mut |t| found |= matches!(t, Ty::Var(_)));
        found
    }

    pub fn has_params(&self) -> bool {
        let mut found = false;
        self.walk(&mut |t| found |= matches!(t, Ty::Param(_)));
        found
    }

    pub fn walk(&self, f: &mut impl FnMut(&Ty)) {
        f(self);
        match self {
            Ty::Array(t) | Ty::Opt(t) | Ty::Ptr(t) => t.walk(f),
            Ty::Tuple(ts) | Ty::Adt(_, ts) => ts.iter().for_each(|t| t.walk(f)),
            Ty::Fn(ft) => {
                ft.params.iter().for_each(|t| t.walk(f));
                ft.ret.walk(f);
            }
            _ => {}
        }
    }

    /// Replaces generic parameters with the given types and effects.
    pub fn subst(&self, tys: &[Ty], effs: &[Eff]) -> Ty {
        match self {
            Ty::Param(i) => tys.get(*i as usize).cloned().unwrap_or(Ty::Error),
            Ty::Array(t) => Ty::Array(Box::new(t.subst(tys, effs))),
            Ty::Opt(t) => Ty::Opt(Box::new(t.subst(tys, effs))),
            Ty::Ptr(t) => Ty::Ptr(Box::new(t.subst(tys, effs))),
            Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| t.subst(tys, effs)).collect()),
            Ty::Adt(id, ts) => Ty::Adt(*id, ts.iter().map(|t| t.subst(tys, effs)).collect()),
            Ty::Fn(ft) => Ty::Fn(Box::new(FnTy {
                params: ft.params.iter().map(|t| t.subst(tys, effs)).collect(),
                ret: ft.ret.subst(tys, effs),
                eff: ft.eff.subst(effs),
            })),
            other => other.clone(),
        }
    }
}

impl Eff {
    /// Replaces effect parameters with the given effects.
    pub fn subst(&self, effs: &[Eff]) -> Eff {
        let mut out = Eff { io: self.io, fail: self.fail, params: Vec::new(), vars: self.vars.clone() };
        for p in &self.params {
            match effs.get(*p as usize) {
                Some(e) => out = out.union(e),
                None => out.params.push(*p),
            }
        }
        out
    }
}
