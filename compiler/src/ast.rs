//! Syntax tree. It keeps enough layout information (spans, literal source text)
//! for `ovt fmt` to reproduce canonical source.

use crate::source::Span;

#[derive(Clone, Debug)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

#[derive(Clone, Debug, Default)]
pub struct File {
    pub items: Vec<Item>,
    /// Set when the file was parsed as a statement snippet instead of declarations.
    pub stmts: Vec<Stmt>,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub kind: ItemKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum ItemKind {
    Const(ConstDecl),
    Type(TypeDecl),
    Enum(EnumDecl),
    Fn(FnDecl),
    Extern(ExternDecl),
    Drop(DropDecl),
    Test(TestDecl),
}

#[derive(Clone, Debug)]
pub struct ConstDecl {
    pub name: Ident,
    pub ty: Option<TypeExpr>,
    pub value: Expr,
}

#[derive(Clone, Debug)]
pub struct TypeDecl {
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub body: TypeBody,
}

#[derive(Clone, Debug)]
pub enum TypeBody {
    Struct { fields: Vec<Field>, one_line: bool },
    Alias(TypeExpr),
    /// `type name` inside an `extern` block.
    Opaque,
}

#[derive(Clone, Debug)]
pub struct Field {
    pub name: Ident,
    pub ty: TypeExpr,
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct EnumDecl {
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub variants: Vec<Variant>,
    pub one_line: bool,
}

#[derive(Clone, Debug)]
pub struct Variant {
    pub name: Ident,
    pub fields: Option<Vec<Field>>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum GenericParam {
    Type { name: Ident, bounds: Vec<Ident> },
    Effect { name: Ident },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Read,
    Inout,
    Sink,
}

#[derive(Clone, Debug)]
pub struct Param {
    pub mode: Mode,
    /// `self` for the receiver.
    pub name: Ident,
    /// `None` only for `self`.
    pub ty: Option<TypeExpr>,
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Example {
    pub expr: Expr,
    /// `Some(None)` for `fails`, `Some(Some(kind))` for `fails .Kind`.
    pub fails: Option<Option<Expr>>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct FnDecl {
    pub is_unsafe: bool,
    pub blocking: bool,
    /// `Type[G].` in `fn Type[G].name(...)`.
    pub recv: Option<(Ident, Vec<GenericParam>)>,
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub effects: Vec<Ident>,
    pub pre: Vec<Expr>,
    pub ex: Vec<Example>,
    /// `None` inside `extern` blocks.
    pub body: Option<Block>,
    /// From `fn` to the end of the effects.
    pub sig_span: Span,
}

#[derive(Clone, Debug)]
pub struct ExternDecl {
    pub blocking: bool,
    pub lib: StrLit,
    pub header: Option<StrLit>,
    pub items: Option<Vec<Item>>,
}

#[derive(Clone, Debug)]
pub struct DropDecl {
    pub ty: TypeExpr,
    pub body: Block,
}

#[derive(Clone, Debug)]
pub struct TestDecl {
    pub name: StrLit,
    pub effects: Vec<Ident>,
    pub body: Block,
}

#[derive(Clone, Debug)]
pub struct StrLit {
    pub triple: bool,
    pub parts: Vec<StrPart>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum StrPart {
    /// Source text, escapes not decoded.
    Text(String),
    Interp(Box<Expr>),
}

#[derive(Clone, Debug)]
pub struct TypeExpr {
    pub kind: TypeKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum TypeKind {
    Named { path: Vec<Ident>, args: Vec<TypeExpr> },
    Array(Box<TypeExpr>),
    Fixed(Box<TypeExpr>, Box<Expr>),
    Tuple(Vec<TypeExpr>),
    Optional(Box<TypeExpr>),
    Fn { params: Vec<TypeExpr>, ret: Option<Box<TypeExpr>>, effects: Vec<Ident> },
    Ptr(Box<TypeExpr>),
}

#[derive(Clone, Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
    Set,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

impl AssignOp {
    pub fn text(self) -> &'static str {
        match self {
            AssignOp::Set => "=",
            AssignOp::Add => "+=",
            AssignOp::Sub => "-=",
            AssignOp::Mul => "*=",
            AssignOp::Div => "/=",
            AssignOp::Rem => "%=",
        }
    }
}

#[derive(Clone, Debug)]
pub enum StmtKind {
    Let { mutable: bool, pat: Pattern, ty: Option<TypeExpr>, value: Expr },
    Assign { target: Expr, op: AssignOp, value: Expr },
    Expr(Expr),
    /// `pats` holds one pattern, or two for `for i, x in xs`.
    For { inout: bool, pats: Vec<Pattern>, iter: Expr, body: Block },
    While { cond: Cond, body: Block },
    Par(Block),
}

#[derive(Clone, Debug)]
pub enum Cond {
    Expr(Expr),
    /// `let v = opt` in `if let` and `while let`.
    Let { name: Ident, value: Expr },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    AddW,
    SubW,
    MulW,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

impl BinOp {
    pub fn text(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::AddW => "+%",
            BinOp::SubW => "-%",
            BinOp::MulW => "*%",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "&&",
            BinOp::Or => "||",
            BinOp::BitAnd => "&",
            BinOp::BitOr => "|",
            BinOp::BitXor => "^",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
        }
    }

    /// Binding power; higher binds tighter.
    pub fn prec(self) -> u8 {
        match self {
            BinOp::Or => 3,
            BinOp::And => 4,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 5,
            BinOp::BitOr => 6,
            BinOp::BitXor => 7,
            BinOp::BitAnd => 8,
            BinOp::Shl | BinOp::Shr => 9,
            BinOp::Add | BinOp::Sub | BinOp::AddW | BinOp::SubW => 10,
            BinOp::Mul | BinOp::Div | BinOp::Rem | BinOp::MulW => 11,
        }
    }

    pub fn is_comparison(self) -> bool {
        self.prec() == 5
    }
}

/// Precedence of `else` and `catch`, the loosest operators.
pub const PREC_ELSE: u8 = 1;
/// Precedence of `..` and `..=`.
pub const PREC_RANGE: u8 = 2;

#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    /// Literals keep their source text.
    Int(String),
    Float(String),
    Dur(String),
    Byte(String),
    Str(StrLit),
    Bool(bool),
    None,
    Ident(String),
    SelfRef,
    /// `_`: a typed hole in expressions, the discard target in `_ = f()`.
    Hole,
    /// `.Name`, a variant of a type known from context.
    Variant(Ident),
    Paren(Box<Expr>),
    Tuple(Vec<Expr>),
    Array { items: Vec<Expr>, multiline: bool },
    Map { entries: Vec<(Expr, Expr)>, multiline: bool },
    Set { items: Vec<Expr>, multiline: bool },
    /// `{}`: an empty map or set.
    EmptyBraces,
    /// Field access, method name or tuple index (`t.0`).
    Field(Box<Expr>, Ident),
    Call { callee: Box<Expr>, args: Vec<Arg>, multiline: bool },
    /// Indexing, or type arguments (`json.decode[User]`); resolved later.
    Index { base: Box<Expr>, args: Vec<Expr> },
    /// A type written where an expression was expected (`f[?T]`).
    Type(TypeExpr),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Range { lo: Option<Box<Expr>>, hi: Option<Box<Expr>>, inclusive: bool },
    Try(Box<Expr>),
    Else(Box<Expr>, Box<Expr>),
    Catch { expr: Box<Expr>, name: Ident, body: Block },
    If { cond: Box<Cond>, then: Block, els: Option<Box<Expr>> },
    Match { scrutinee: Box<Expr>, arms: Vec<Arm>, one_line: bool },
    Lock { target: Box<Expr>, name: Ident, body: Block },
    Unsafe(Block),
    /// A block where a value is expected: a `match` arm or the right side of `else`.
    Block(Block),
    Closure { params: Vec<ClosureParam>, body: Box<Expr> },
    Return(Option<Box<Expr>>),
    Break,
    Continue,
}

#[derive(Clone, Debug)]
pub struct Arg {
    pub name: Option<Ident>,
    pub inout: bool,
    pub value: Expr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct ClosureParam {
    pub name: Ident,
    pub ty: Option<TypeExpr>,
}

#[derive(Clone, Debug)]
pub struct Arm {
    pub pat: Pattern,
    pub guard: Option<Expr>,
    pub body: Expr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Pattern {
    pub kind: PatKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum PatKind {
    Wild,
    Bind(String),
    /// A literal: number, string, byte, bool or `none`.
    Lit(Box<Expr>),
    Range { lo: Box<Expr>, hi: Box<Expr>, inclusive: bool },
    Tuple(Vec<Pattern>),
    Array(Vec<Pattern>),
    /// `..` or `..name` inside an array pattern.
    Rest(Option<Ident>),
    /// `.Name(...)` or `Type.Name(...)`.
    Variant { ty: Vec<Ident>, name: Ident, fields: Option<Vec<FieldPat>> },
    Or(Vec<Pattern>),
}

#[derive(Clone, Debug)]
pub struct FieldPat {
    pub name: Ident,
    pub pat: Option<Pattern>,
}
