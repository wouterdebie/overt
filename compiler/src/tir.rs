//! The typed program: what the checker produces and codegen consumes.
//!
//! Types in function bodies may contain generic parameters; codegen
//! instantiates each function once per set of type arguments.

use crate::ast::{BinOp, Mode, UnOp};
use crate::source::Span;
use crate::types::{AdtId, Eff, FnId, Ty};

pub type LocalId = usize;
pub type ClosureId = usize;
pub type FileId = usize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bounds {
    pub eq: bool,
    pub ord: bool,
    pub hash: bool,
}

#[derive(Clone, Debug)]
pub enum GenericKind {
    Type(Bounds),
    Effect,
}

#[derive(Clone, Debug)]
pub struct GenericDef {
    pub name: String,
    pub kind: GenericKind,
    /// Position among the type parameters, or among the effect parameters.
    pub index: u32,
}

pub struct Program {
    pub adts: Vec<AdtDef>,
    pub fns: Vec<FnDef>,
    pub closures: Vec<ClosureDef>,
    pub main: Option<FnId>,
    pub tests: Vec<TestDef>,
    pub known: Known,
}

/// Standard library declarations the compiler relies on.
#[derive(Clone, Debug, Default)]
pub struct Known {
    pub err: AdtId,
    pub err_kind: AdtId,
    pub map: AdtId,
    pub set: AdtId,
    /// `Map.new`, `Map.set`, `Map.get`, `Set.new`, `Set.insert`.
    pub map_new: FnId,
    pub map_set: FnId,
    pub map_get: FnId,
    pub set_new: FnId,
    pub set_insert: FnId,
    /// `Map.equals` and `Set.equals`, which `==` on maps and sets calls.
    pub map_equals: FnId,
    pub set_equals: FnId,
    /// `Chan` and `Shared`, which `for v in ch` and `lock` need, and `Chan.recv`.
    pub chan: AdtId,
    pub shared: AdtId,
    pub chan_recv: FnId,
}

pub struct AdtDef {
    pub name: String,
    pub module: String,
    pub generics: Vec<GenericDef>,
    pub kind: AdtKind,
    pub file: FileId,
}

pub enum AdtKind {
    Struct(Vec<FieldDef>),
    Enum(Vec<VariantDef>),
}

pub struct FieldDef {
    pub name: String,
    pub ty: Ty,
    /// Stored behind a pointer, because the type contains itself.
    pub boxed: bool,
    pub default: Option<TExpr>,
}

pub struct VariantDef {
    pub name: String,
    pub fields: Vec<FieldDef>,
}

impl AdtDef {
    pub fn fields(&self) -> &[FieldDef] {
        match &self.kind {
            AdtKind::Struct(f) => f,
            AdtKind::Enum(_) => &[],
        }
    }

    pub fn variants(&self) -> &[VariantDef] {
        match &self.kind {
            AdtKind::Enum(v) => v,
            AdtKind::Struct(_) => &[],
        }
    }

    pub fn is_enum(&self) -> bool {
        matches!(self.kind, AdtKind::Enum(_))
    }

    pub fn type_params(&self) -> usize {
        self.generics.iter().filter(|g| matches!(g.kind, GenericKind::Type(_))).count()
    }
}

pub struct ParamDef {
    pub name: String,
    pub mode: Mode,
    pub ty: Ty,
    pub default: Option<TExpr>,
}

pub struct FnDef {
    /// As shown to people: `parse`, `api.users.find`, `str.find`, `[T].push`.
    pub name: String,
    /// A unique symbol prefix; instances append their type arguments.
    pub symbol: String,
    pub module: String,
    pub file: FileId,
    pub generics: Vec<GenericDef>,
    pub params: Vec<ParamDef>,
    pub ret: Ty,
    pub eff: Eff,
    pub has_self: bool,
    /// Set for standard library functions without a body.
    pub intrinsic: Option<String>,
    pub body: Option<Body>,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalKind {
    /// Holds its own value, dropped when it goes out of scope.
    Owned,
    /// Holds the bits of a value owned elsewhere; never dropped.
    Borrowed,
    /// Holds a pointer to a place: `inout` parameters and `for inout` variables.
    Ref,
}

pub struct LocalDef {
    pub name: String,
    pub ty: Ty,
    pub kind: LocalKind,
}

pub struct Body {
    pub locals: Vec<LocalDef>,
    pub params: Vec<LocalId>,
    pub pre: Vec<(TExpr, String)>,
    pub block: TBlock,
}

pub struct ClosureDef {
    /// The function the closure appears in; its generic parameters are in scope.
    pub owner: FnId,
    pub params: Vec<LocalId>,
    /// Locals of the owner copied into the closure, and the closure locals they become.
    pub captures: Vec<(LocalId, LocalId)>,
    pub ret: Ty,
    pub eff: Eff,
    pub body: Body,
}

pub struct TestDef {
    pub name: String,
    /// The function that runs the test (a `test` block or an `ex` line, turned into a function).
    pub func: FnId,
}

#[derive(Clone)]
pub struct TExpr {
    pub kind: TK,
    pub ty: Ty,
    pub span: Span,
}

#[derive(Clone)]
pub struct TArg {
    pub mode: Mode,
    pub expr: TExpr,
    /// The argument reads a variable that another argument passes as `inout`,
    /// so it's copied before the call.
    pub copy: bool,
}

#[derive(Clone)]
pub enum TK {
    Int(i128),
    Float(f64),
    Bool(bool),
    Str(Vec<u8>),
    Unit,
    Local(LocalId),
    Call { f: FnId, targs: Vec<Ty>, eargs: Vec<Eff>, args: Vec<TArg> },
    /// Calling a function value (a closure or a named function).
    CallValue { callee: Box<TExpr>, args: Vec<TArg> },
    /// `lock s as v { body }`: `local` refers to the shared value while the
    /// lock is held; the block's value is the expression's.
    Lock { shared: Box<TExpr>, local: LocalId, body: TBlock },
    FnValue { f: FnId, targs: Vec<Ty>, eargs: Vec<Eff> },
    Closure { id: ClosureId },
    Struct { adt: AdtId, targs: Vec<Ty>, fields: Vec<TExpr> },
    Variant { adt: AdtId, targs: Vec<Ty>, variant: u32, fields: Vec<TExpr> },
    Tuple(Vec<TExpr>),
    Array(Vec<TExpr>),
    /// A struct field or tuple element.
    Field { base: Box<TExpr>, index: u32 },
    /// An array element or a byte of a string; traps when out of range.
    Index { base: Box<TExpr>, index: Box<TExpr> },
    Slice { base: Box<TExpr>, lo: Option<Box<TExpr>>, hi: Option<Box<TExpr>> },
    Some(Box<TExpr>),
    None,
    Unary(UnOp, Box<TExpr>),
    Binary(BinOp, Box<TExpr>, Box<TExpr>),
    /// A checked numeric conversion to `ty`.
    Convert(Box<TExpr>),
    /// String interpolation; `str` parts are appended as they are, others are shown.
    Interp(Vec<TExpr>),
    If { cond: Box<TExpr>, then: TBlock, els: Option<TBlock> },
    IfLet { local: LocalId, value: Box<TExpr>, then: TBlock, els: Option<TBlock> },
    Match { scrut: Box<TExpr>, arms: Vec<TArm> },
    Block(TBlock),
    Return(Option<Box<TExpr>>),
    Break,
    Continue,
    /// `f()?`
    Try(Box<TExpr>),
    /// `opt else alt`
    ElseOpt { value: Box<TExpr>, alt: Box<TExpr> },
    /// `f() else alt`
    ElseFail { value: Box<TExpr>, alt: Box<TExpr> },
    /// `f() catch e { ... }`
    Catch { value: Box<TExpr>, local: LocalId, body: TBlock },
    Fail { kind: Box<TExpr>, msg: Box<TExpr> },
    Trap(Box<TExpr>),
    Todo,
    Assert { cond: Box<TExpr>, msg: Option<Box<TExpr>>, text: String },
    Print { value: Box<TExpr>, stderr: bool },
    Dbg { value: Box<TExpr>, text: String },
    Hash(Box<TExpr>),
    /// `ex` lines: compares the two sides and reports both on a mismatch.
    ExpectEq { left: Box<TExpr>, right: Box<TExpr> },
    /// `ex f() fails [.Kind]`
    ExpectFail { value: Box<TExpr>, kind: Option<Box<TExpr>> },
    ExpectTrue { cond: Box<TExpr> },
}

#[derive(Clone)]
pub struct TBlock {
    pub stmts: Vec<TStmt>,
    /// The last line, when the block is used as a value.
    pub value: Option<Box<TExpr>>,
}

#[derive(Clone)]
pub enum TPlace {
    Local(LocalId),
    Field(Box<TPlace>, u32),
    Index(Box<TPlace>, Box<TExpr>),
}

#[derive(Clone)]
pub enum TStmt {
    Let { local: LocalId, value: TExpr },
    /// `let (a, b) = t`; `None` for `_`.
    LetTuple { locals: Vec<Option<LocalId>>, value: TExpr },
    Assign { place: TPlace, op: Option<BinOp>, value: TExpr, span: Span },
    Expr(TExpr),
    While { cond: TExpr, body: TBlock },
    WhileLet { local: LocalId, value: TExpr, body: TBlock },
    ForRange { local: LocalId, lo: TExpr, hi: TExpr, inclusive: bool, body: TBlock },
    /// `for x in xs`, `for i, x in xs`, `for inout x in xs`.
    ForArray { elem: LocalId, index: Option<LocalId>, array: TExpr, place: Option<TPlace>, body: TBlock },
    /// `for k, v in m` over a `Map`, or `for x in s` over a `Set` (`value` is `None`).
    ForMap { key: LocalId, value: Option<LocalId>, map: TExpr, is_set: bool, body: TBlock },
    /// `par { ... }`: each statement is a closure run at the same time as the
    /// others, returning the variables it declares; `outs` receive them.
    Par { branches: Vec<TParBranch> },
}

#[derive(Clone)]
pub struct TParBranch {
    pub closure: TExpr,
    pub outs: Vec<LocalId>,
}

#[derive(Clone)]
pub struct TArm {
    pub pat: TPat,
    pub guard: Option<TExpr>,
    pub body: TExpr,
}

#[derive(Clone)]
pub enum TPat {
    Wild,
    Bind(LocalId),
    Int(i128),
    Float(f64),
    Bool(bool),
    Str(Vec<u8>),
    None,
    Range { lo: i128, hi: i128, inclusive: bool },
    Tuple(Vec<TPat>),
    Array { before: Vec<TPat>, rest: Option<Option<LocalId>>, after: Vec<TPat> },
    Variant { variant: u32, fields: Vec<(u32, TPat)> },
    Or(Vec<TPat>),
}
