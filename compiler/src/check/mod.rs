//! Name resolution and type checking.
//!
//! `check` collects every declaration of the program and the standard
//! library, resolves signatures, then checks function bodies (in `body`),
//! producing the typed program.

mod body;
mod call;
mod infer;
mod pat;
mod zonk;

use crate::ast::{self, GenericParam, ItemKind, Mode, TypeBody, TypeKind};
use crate::diag::{closest, Diag};
use crate::source::{Source, Span};
use crate::stdlib;
use crate::tir::*;
use crate::types::{AdtId, Eff, FloatTy, FnId, FnTy, IntTy, Ty};
use std::collections::{HashMap, HashSet};

pub struct SourceFile {
    pub src: Source,
    pub ast: ast::File,
    pub module: String,
    pub std: bool,
    /// Declarations are visible everywhere without a module name (prelude-like std files).
    pub open: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TestSel {
    None,
    User,
    Std,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RecvKey {
    Adt(AdtId),
    Str,
    Array,
    Int(IntTy),
    Float(FloatTy),
    Bool,
}

impl RecvKey {
    pub fn of(t: &Ty) -> Option<RecvKey> {
        Some(match t {
            Ty::Adt(id, _) => RecvKey::Adt(*id),
            Ty::Str => RecvKey::Str,
            Ty::Array(_) => RecvKey::Array,
            Ty::Int(k) => RecvKey::Int(*k),
            Ty::Float(k) => RecvKey::Float(*k),
            Ty::Bool => RecvKey::Bool,
            _ => return None,
        })
    }
}

#[derive(Default)]
pub struct Scope {
    pub types: HashMap<String, TypeRef>,
    pub fns: HashMap<String, FnId>,
    pub consts: HashMap<String, usize>,
}

#[derive(Clone)]
pub enum TypeRef {
    Adt(AdtId),
    Alias(Ty),
}

pub struct ConstDef<'a> {
    pub name: String,
    pub file: FileId,
    pub expr: &'a ast::Expr,
    pub ty: Option<Ty>,
    pub value: Option<TExpr>,
    pub checking: bool,
    pub span: Span,
}

/// Where a declaration's source lives, for checking its body later.
struct PendingFn<'a> {
    id: FnId,
    ast: &'a ast::FnDecl,
    file: FileId,
}

pub struct Checker<'a> {
    pub files: &'a [SourceFile],
    pub diags: Vec<Diag>,
    pub adts: Vec<AdtDef>,
    pub fns: Vec<FnDef>,
    pub closures: Vec<ClosureDef>,
    pub consts: Vec<ConstDef<'a>>,
    /// Scopes of user modules and of std modules like `os`.
    pub modules: HashMap<String, Scope>,
    /// Declarations visible everywhere (prelude-like std files).
    pub global: Scope,
    pub methods: HashMap<(RecvKey, String), Vec<FnId>>,
    /// The receiver type of each method, for matching `[str].join` against `[T]`.
    pub fn_recv: HashMap<FnId, Ty>,
    pub known: Known,
    pub tests: Vec<TestDef>,
    /// Methods declared for an `Adt` must live in that type's module.
    adt_files: Vec<FileId>,
}

pub fn check(files: &[SourceFile], tests: TestSel) -> Result<Program, Vec<Diag>> {
    let mut c = Checker {
        files,
        diags: Vec::new(),
        adts: Vec::new(),
        fns: Vec::new(),
        closures: Vec::new(),
        consts: Vec::new(),
        modules: HashMap::new(),
        global: Scope::default(),
        methods: HashMap::new(),
        fn_recv: HashMap::new(),
        known: Known::default(),
        tests: Vec::new(),
        adt_files: Vec::new(),
    };
    for f in files {
        if !f.open {
            c.modules.entry(f.module.clone()).or_default();
        }
    }
    let adt_items = c.collect_types();
    c.find_known();
    c.resolve_types(&adt_items);
    let pending = c.collect_fns();
    c.find_known_fns();
    for i in 0..c.consts.len() {
        c.const_value(i);
    }
    for p in &pending {
        c.check_fn_body(p.id, p.ast, p.file);
    }
    c.collect_tests(&pending, tests);
    let main = c.find_main();
    if !c.diags.is_empty() {
        let mut diags = std::mem::take(&mut c.diags);
        diags.sort_by_key(|d| (d.file, d.span.lo));
        diags.dedup_by(|a, b| a.file == b.file && a.span == b.span && a.msg == b.msg);
        return Err(diags);
    }
    Ok(Program { adts: c.adts, fns: c.fns, closures: c.closures, main, tests: c.tests, known: c.known })
}

pub fn is_snake(name: &str) -> bool {
    name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

// A leading `_` makes a name private, so the case rules apply to the rest.

pub fn is_pascal(name: &str) -> bool {
    let name = name.strip_prefix('_').unwrap_or(name);
    name.starts_with(|c: char| c.is_ascii_uppercase()) && name.chars().all(|c| c.is_ascii_alphanumeric())
}

pub fn is_upper(name: &str) -> bool {
    let name = name.strip_prefix('_').unwrap_or(name);
    name.starts_with(|c: char| c.is_ascii_uppercase()) && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

pub fn to_snake(name: &str) -> String {
    let mut out = String::new();
    for (i, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 && !out.ends_with('_') {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

pub fn to_pascal(name: &str) -> String {
    name.split('_').filter(|p| !p.is_empty()).map(|p| p[..1].to_uppercase() + &p[1..]).collect()
}

/// Functions from the spec that this compiler doesn't implement yet: (module, name, milestone).
pub const LATER_FNS: &[(&str, &str, &str)] = &[];

/// Types from the spec that this compiler doesn't implement yet.
pub const LATER_TYPES: &[(&str, &str)] =
    &[("never", "1")];

impl<'a> Checker<'a> {
    pub fn err(&mut self, file: FileId, span: Span, msg: impl Into<String>) {
        self.diags.push(Diag::new(span, msg).in_file(file));
    }

    pub fn src(&self, file: FileId) -> &Source {
        &self.files[file].src
    }

    /// The scope a file's own declarations go into.
    fn home_scope(&mut self, file: FileId) -> &mut Scope {
        if self.files[file].open {
            &mut self.global
        } else {
            let m = self.files[file].module.clone();
            self.modules.get_mut(&m).unwrap()
        }
    }

    // ---- types ----

    fn collect_types(&mut self) -> Vec<(AdtId, &'a ast::Item, FileId)> {
        let mut items = Vec::new();
        for (fi, f) in self.files.iter().enumerate() {
            for item in &f.ast.items {
                let (name, generics, is_enum) = match &item.kind {
                    ItemKind::Type(t) => {
                        if matches!(t.body, TypeBody::Alias(_)) {
                            continue;
                        }
                        (&t.name, &t.generics, false)
                    }
                    ItemKind::Enum(e) => (&e.name, &e.generics, true),
                    _ => continue,
                };
                if !is_pascal(&name.name) {
                    self.err(fi, name.span, format!("type names are PascalCase: rename `{}` to `{}`", name.name, to_pascal(&name.name)));
                }
                let id = self.adts.len();
                let generics = self.generic_defs(generics, fi, &[]);
                self.adts.push(AdtDef {
                    name: name.name.clone(),
                    module: f.module.clone(),
                    generics,
                    kind: if is_enum { AdtKind::Enum(Vec::new()) } else { AdtKind::Struct(Vec::new()) },
                    file: fi,
                });
                self.adt_files.push(fi);
                let scope = self.home_scope(fi);
                if scope.types.contains_key(&name.name) {
                    self.err(fi, name.span, format!("the type `{}` is declared twice", name.name));
                } else {
                    self.home_scope(fi).types.insert(name.name.clone(), TypeRef::Adt(id));
                }
                items.push((id, item, fi));
            }
        }
        // Aliases, in order; an alias may use types declared anywhere, and earlier aliases.
        for (fi, f) in self.files.iter().enumerate() {
            for item in &f.ast.items {
                if let ItemKind::Type(t) = &item.kind {
                    if let TypeBody::Alias(target) = &t.body {
                        let ty = self.resolve_type(target, fi, &[]);
                        self.home_scope(fi).types.insert(t.name.name.clone(), TypeRef::Alias(ty));
                    }
                }
            }
        }
        items
    }

    fn find_known(&mut self) {
        let get = |c: &Checker, n: &str| match c.global.types.get(n) {
            Some(TypeRef::Adt(id)) => *id,
            _ => panic!("the standard library must declare `{n}`"),
        };
        self.known.err = get(self, "Err");
        self.known.err_kind = get(self, "ErrKind");
        self.known.map = get(self, "Map");
        self.known.set = get(self, "Set");
        self.known.chan = get(self, "Chan");
        self.known.shared = get(self, "Shared");
    }

    fn find_known_fns(&mut self) {
        let m = |c: &Checker, adt: AdtId, n: &str| c.methods[&(RecvKey::Adt(adt), n.to_string())][0];
        self.known.map_new = m(self, self.known.map, "new");
        self.known.map_set = m(self, self.known.map, "set");
        self.known.map_get = m(self, self.known.map, "get");
        self.known.set_new = m(self, self.known.set, "new");
        self.known.set_insert = m(self, self.known.set, "insert");
        self.known.map_equals = m(self, self.known.map, "equals");
        self.known.set_equals = m(self, self.known.set, "equals");
        self.known.chan_recv = m(self, self.known.chan, "recv");
    }

    pub fn generic_defs(&mut self, gs: &[GenericParam], file: FileId, outer: &[GenericDef]) -> Vec<GenericDef> {
        let mut out: Vec<GenericDef> = outer.to_vec();
        let mut ntype = outer.iter().filter(|g| matches!(g.kind, GenericKind::Type(_))).count() as u32;
        let mut neff = outer.iter().filter(|g| matches!(g.kind, GenericKind::Effect)).count() as u32;
        for g in gs {
            match g {
                GenericParam::Type { name, bounds } => {
                    let mut b = Bounds::default();
                    for bound in bounds {
                        match bound.name.as_str() {
                            "Eq" => b.eq = true,
                            "Ord" => {
                                b.ord = true;
                                b.eq = true;
                            }
                            "Hash" => b.hash = true,
                            other => self.err(file, bound.span, format!("unknown constraint `{other}`; the constraints are `Eq`, `Ord` and `Hash`")),
                        }
                    }
                    if out.iter().any(|o| o.name == name.name) {
                        self.err(file, name.span, format!("the type parameter `{}` is declared twice", name.name));
                    }
                    out.push(GenericDef { name: name.name.clone(), kind: GenericKind::Type(b), index: ntype });
                    ntype += 1;
                }
                GenericParam::Effect { name } => {
                    out.push(GenericDef { name: name.name.clone(), kind: GenericKind::Effect, index: neff });
                    neff += 1;
                }
            }
        }
        out
    }

    fn resolve_types(&mut self, items: &[(AdtId, &'a ast::Item, FileId)]) {
        for &(id, item, fi) in items {
            let generics = self.adts[id].generics.clone();
            match &item.kind {
                ItemKind::Type(t) => {
                    let TypeBody::Struct { fields, .. } = &t.body else { continue };
                    let fs = self.field_defs(fields, fi, &generics);
                    self.adts[id].kind = AdtKind::Struct(fs);
                }
                ItemKind::Enum(e) => {
                    let mut vs = Vec::new();
                    let mut seen = HashSet::new();
                    for v in &e.variants {
                        if !is_pascal(&v.name.name) {
                            self.err(fi, v.name.span, format!("variant names are PascalCase: rename `{}` to `{}`", v.name.name, to_pascal(&v.name.name)));
                        }
                        if !seen.insert(v.name.name.clone()) {
                            self.err(fi, v.name.span, format!("the variant `{}` is declared twice", v.name.name));
                        }
                        let fields = v.fields.as_ref().map(|fs| self.field_defs(fs, fi, &generics)).unwrap_or_default();
                        vs.push(VariantDef { name: v.name.name.clone(), fields });
                    }
                    self.adts[id].kind = AdtKind::Enum(vs);
                }
                _ => {}
            }
        }
        self.mark_boxed();
        // Field defaults are checked like constants, once all types are known.
        for &(id, item, fi) in items {
            if let ItemKind::Type(t) = &item.kind {
                if let TypeBody::Struct { fields, .. } = &t.body {
                    for (i, f) in fields.iter().enumerate() {
                        if let Some(d) = &f.default {
                            let ty = self.adts[id].fields()[i].ty.clone();
                            if ty.has_params() {
                                self.err(fi, d.span, "a field whose type is a type parameter can't have a default");
                                continue;
                            }
                            let v = self.check_const_expr(d, fi, Some(&ty));
                            if let AdtKind::Struct(fs) = &mut self.adts[id].kind {
                                fs[i].default = Some(v);
                            }
                        }
                    }
                }
            }
        }
    }

    fn field_defs(&mut self, fields: &[ast::Field], file: FileId, generics: &[GenericDef]) -> Vec<FieldDef> {
        let mut out: Vec<FieldDef> = Vec::new();
        for f in fields {
            if !is_snake(&f.name.name) {
                self.err(file, f.name.span, format!("field names are snake_case: rename `{}` to `{}`", f.name.name, to_snake(&f.name.name)));
            }
            if out.iter().any(|o| o.name == f.name.name) {
                self.err(file, f.name.span, format!("the field `{}` is declared twice", f.name.name));
            }
            let ty = self.resolve_type(&f.ty, file, generics);
            out.push(FieldDef { name: f.name.name.clone(), ty, boxed: false, default: None });
        }
        out
    }

    /// A field is boxed when its type contains its own declaration without an
    /// array or other indirection in between.
    fn mark_boxed(&mut self) {
        for id in 0..self.adts.len() {
            let mut flags = Vec::new();
            match &self.adts[id].kind {
                AdtKind::Struct(fs) => flags.extend(fs.iter().map(|f| self.inline_contains(&f.ty, id, &mut HashSet::new()))),
                AdtKind::Enum(vs) => {
                    for v in vs {
                        flags.extend(v.fields.iter().map(|f| self.inline_contains(&f.ty, id, &mut HashSet::new())));
                    }
                }
            }
            let mut it = flags.into_iter();
            match &mut self.adts[id].kind {
                AdtKind::Struct(fs) => fs.iter_mut().for_each(|f| f.boxed = it.next().unwrap()),
                AdtKind::Enum(vs) => vs.iter_mut().flat_map(|v| v.fields.iter_mut()).for_each(|f| f.boxed = it.next().unwrap()),
            }
        }
    }

    fn inline_contains(&self, t: &Ty, target: AdtId, seen: &mut HashSet<AdtId>) -> bool {
        match t {
            Ty::Adt(id, _) => {
                if *id == target {
                    return true;
                }
                if !seen.insert(*id) {
                    return false;
                }
                let adt = &self.adts[*id];
                let inner: Vec<&Ty> = match &adt.kind {
                    AdtKind::Struct(fs) => fs.iter().filter(|f| !f.boxed).map(|f| &f.ty).collect(),
                    AdtKind::Enum(vs) => vs.iter().flat_map(|v| v.fields.iter()).filter(|f| !f.boxed).map(|f| &f.ty).collect(),
                };
                inner.into_iter().any(|t| self.inline_contains(t, target, seen))
            }
            Ty::Tuple(ts) => ts.iter().any(|t| self.inline_contains(t, target, seen)),
            Ty::Opt(t) => self.inline_contains(t, target, seen),
            _ => false,
        }
    }

    /// Resolves a written type. `generics` are the type parameters in scope.
    pub fn resolve_type(&mut self, t: &ast::TypeExpr, file: FileId, generics: &[GenericDef]) -> Ty {
        match &t.kind {
            TypeKind::Array(e) => Ty::array(self.resolve_type(e, file, generics)),
            TypeKind::Optional(e) => {
                let inner = self.resolve_type(e, file, generics);
                if matches!(inner, Ty::Opt(_)) {
                    self.err(file, t.span, "`??T` isn't allowed; an optional can't hold another optional");
                    return Ty::Error;
                }
                Ty::opt(inner)
            }
            TypeKind::Tuple(items) => {
                if items.is_empty() {
                    return Ty::Unit;
                }
                if items.len() == 1 {
                    return self.resolve_type(&items[0], file, generics);
                }
                Ty::Tuple(items.iter().map(|i| self.resolve_type(i, file, generics)).collect())
            }
            TypeKind::Fn { params, ret, effects } => {
                let params = params.iter().map(|p| self.resolve_type(p, file, generics)).collect();
                let ret = ret.as_ref().map(|r| self.resolve_type(r, file, generics)).unwrap_or(Ty::Unit);
                let eff = self.resolve_effects(effects, file, generics);
                Ty::Fn(Box::new(FnTy { params, ret, eff }))
            }
            TypeKind::Fixed(..) => {
                self.err(file, t.span, "fixed-size arrays `[T; N]` aren't supported by this compiler yet (planned for milestone 5); use an array `[T]`, like `[u32(0)].repeat(64)`");
                Ty::Error
            }
            TypeKind::Ptr(_) => {
                self.err(file, t.span, "raw pointers aren't supported by this compiler yet (planned for milestone 5)");
                Ty::Error
            }
            TypeKind::Named { path, args } => self.resolve_named(path, args, file, generics, t.span),
        }
    }

    pub fn resolve_effects(&mut self, effects: &[ast::Ident], file: FileId, generics: &[GenericDef]) -> Eff {
        let mut e = Eff::default();
        for eff in effects {
            match eff.name.as_str() {
                "io" => e.io = true,
                "fail" => e.fail = true,
                n => match generics.iter().find(|g| g.name == n && matches!(g.kind, GenericKind::Effect)) {
                    Some(g) => e.params.push(g.index),
                    None => self.err(file, eff.span, format!("unknown effect `{n}`; the effects are `io` and `fail`, or an effect parameter like `!E`")),
                },
            }
        }
        e
    }

    fn resolve_named(&mut self, path: &[ast::Ident], args: &[ast::TypeExpr], file: FileId, generics: &[GenericDef], span: Span) -> Ty {
        let targs: Vec<Ty> = args.iter().map(|a| self.resolve_type(a, file, generics)).collect();
        let name = &path.last().unwrap().name;
        let no_args = |c: &mut Checker, ty: Ty| {
            if !targs.is_empty() {
                c.err(file, span, format!("`{name}` takes no type arguments"));
            }
            ty
        };
        if path.len() == 1 {
            if let Some(g) = generics.iter().find(|g| &g.name == name) {
                return match g.kind {
                    GenericKind::Type(_) => no_args(self, Ty::Param(g.index)),
                    GenericKind::Effect => {
                        self.err(file, span, format!("`{name}` is an effect parameter, not a type"));
                        Ty::Error
                    }
                };
            }
            if let Some(k) = IntTy::from_name(name) {
                return no_args(self, Ty::Int(k));
            }
            match name.as_str() {
                "f64" => return no_args(self, Ty::Float(FloatTy::F64)),
                "f32" => return no_args(self, Ty::Float(FloatTy::F32)),
                "bool" => return no_args(self, Ty::Bool),
                "str" => return no_args(self, Ty::Str),
                "Dur" => return no_args(self, Ty::Dur),
                "never" if self.files[file].std => return Ty::Never,
                _ => {}
            }
            let found = self.lookup_type(name, file);
            match found {
                // The spec writes `Atomic[int]`, the only kind there is.
                Some(TypeRef::Adt(id)) if name == "Atomic" && self.files[self.adts[id].file].std && !targs.is_empty() => {
                    if targs.len() != 1 || (targs[0] != Ty::INT && targs[0] != Ty::Error) {
                        self.err(file, span, "only `Atomic[int]` is supported");
                        return Ty::Error;
                    }
                    return self.adt_type(id, Vec::new(), file, span);
                }
                Some(TypeRef::Adt(id)) => return self.adt_type(id, targs, file, span),
                Some(TypeRef::Alias(t)) => return no_args(self, t),
                None => {}
            }
            if let Some((_, m)) = LATER_TYPES.iter().find(|(n, _)| n == name) {
                self.err(file, span, format!("the type `{name}` isn't supported by this compiler yet (planned for milestone {m})"));
                return Ty::Error;
            }
            let hint = match name.as_str() {
                "String" | "string" | "Str" => "; the string type is `str`".to_string(),
                "Vec" | "List" | "Array" => "; arrays are written `[T]`".to_string(),
                "HashMap" | "Dict" | "dict" => "; the map type is `Map[K, V]`".to_string(),
                "HashSet" => "; the set type is `Set[T]`".to_string(),
                "Option" | "Optional" => "; optionals are written `?T`".to_string(),
                "Result" => "; a function that can fail declares `! fail` instead".to_string(),
                "usize" | "isize" | "i128" | "u128" => "; use `int`".to_string(),
                "float" | "double" => "; use `f64`".to_string(),
                "boolean" => "; use `bool`".to_string(),
                _ => {
                    let mut names: Vec<String> = vec!["int".into(), "str".into(), "bool".into(), "f64".into(), "u8".into()];
                    names.extend(self.visible_type_names(file));
                    names.extend(generics.iter().map(|g| g.name.clone()));
                    closest(name, names.iter().map(|s| s.as_str())).map(|s| format!("; did you mean `{s}`?")).unwrap_or_default()
                }
            };
            self.err(file, span, format!("unknown type `{name}`{hint}"));
            return Ty::Error;
        }
        // `module.Type`
        let module: Vec<&str> = path[..path.len() - 1].iter().map(|p| p.name.as_str()).collect();
        let module = module.join(".");
        let found = self.modules.get(&module).and_then(|s| s.types.get(name)).cloned();
        match found {
            Some(TypeRef::Adt(id)) => self.adt_type(id, targs, file, span),
            Some(TypeRef::Alias(t)) => t,
            None if self.modules.contains_key(&module) => {
                self.err(file, span, format!("module `{module}` has no type `{name}`"));
                Ty::Error
            }
            None if stdlib::MODULE_NAMES.contains(&module.as_str()) => {
                self.err(file, span, format!("the `{module}` module isn't available in this compiler yet (planned for milestone {})", stdlib::planned_milestone(&module)));
                Ty::Error
            }
            None => {
                self.err(file, span, format!("unknown module `{module}`"));
                Ty::Error
            }
        }
    }

    fn adt_type(&mut self, id: AdtId, targs: Vec<Ty>, file: FileId, span: Span) -> Ty {
        let want = self.adts[id].type_params();
        if targs.len() != want {
            let name = self.adts[id].name.clone();
            if want == 0 {
                self.err(file, span, format!("`{name}` takes no type arguments"));
            } else {
                let params: Vec<String> = self.adts[id].generics.iter().map(|g| g.name.clone()).collect();
                self.err(file, span, format!("`{name}` takes {want} type argument{}: `{name}[{}]`", if want == 1 { "" } else { "s" }, params.join(", ")));
            }
            return Ty::Error;
        }
        Ty::Adt(id, targs)
    }

    pub fn lookup_type(&self, name: &str, file: FileId) -> Option<TypeRef> {
        let f = &self.files[file];
        if !f.open {
            if let Some(t) = self.modules.get(&f.module).and_then(|s| s.types.get(name)) {
                return Some(t.clone());
            }
        }
        self.global.types.get(name).cloned()
    }

    fn visible_type_names(&self, file: FileId) -> Vec<String> {
        let f = &self.files[file];
        let mut out: Vec<String> = self.global.types.keys().cloned().collect();
        if let Some(s) = self.modules.get(&f.module) {
            out.extend(s.types.keys().cloned());
        }
        out
    }

    // ---- functions ----

    fn collect_fns(&mut self) -> Vec<PendingFn<'a>> {
        let mut pending = Vec::new();
        for (fi, f) in self.files.iter().enumerate() {
            for item in &f.ast.items {
                match &item.kind {
                    ItemKind::Fn(d) => {
                        if let Some(id) = self.declare_fn(d, fi) {
                            pending.push(PendingFn { id, ast: d, file: fi });
                        }
                    }
                    ItemKind::Const(k) => {
                        if !is_upper(&k.name.name) {
                            self.err(fi, k.name.span, format!("constant names are UPPER_CASE: rename `{}` to `{}`", k.name.name, to_snake(&k.name.name).to_uppercase()));
                        }
                        let ty = k.ty.as_ref().map(|t| self.resolve_type(t, fi, &[]));
                        let id = self.consts.len();
                        self.consts.push(ConstDef {
                            name: k.name.name.clone(),
                            file: fi,
                            expr: &k.value,
                            ty,
                            value: None,
                            checking: false,
                            span: k.name.span,
                        });
                        self.home_scope(fi).consts.insert(k.name.name.clone(), id);
                    }
                    ItemKind::Extern(_) => self.err(fi, item.span, "`extern` isn't supported by this compiler yet (planned for milestone 5)"),
                    ItemKind::Drop(_) => self.err(fi, item.span, "`drop` isn't supported by this compiler yet (planned for milestone 5)"),
                    _ => {}
                }
            }
        }
        pending
    }

    fn declare_fn(&mut self, d: &'a ast::FnDecl, fi: FileId) -> Option<FnId> {
        let file = &self.files[fi];
        if d.is_unsafe {
            self.err(fi, d.sig_span, "`unsafe fn` isn't supported by this compiler yet (planned for milestone 5)");
            return None;
        }
        if !is_snake(&d.name.name) {
            self.err(fi, d.name.span, format!("function names are snake_case: rename `{}` to `{}`", d.name.name, to_snake(&d.name.name)));
        }
        // The receiver: which type the method belongs to, and its generics.
        let mut recv_ty = None;
        let mut generics = Vec::new();
        let mut display = d.name.name.clone();
        if let Some((rname, rgens)) = &d.recv {
            let (key, ty, gens) = self.receiver(rname, rgens, fi)?;
            generics = gens;
            let shown = if rname.name == "[]" {
                format!("[{}]", rgens.iter().map(|g| match g { GenericParam::Type { name, .. } => name.name.clone(), GenericParam::Effect { name } => name.name.clone() }).collect::<Vec<_>>().join(", "))
            } else {
                rname.name.clone()
            };
            display = format!("{shown}.{}", d.name.name);
            recv_ty = Some((key, ty));
        }
        let generics = self.generic_defs(&d.generics, fi, &generics);
        let mut params = Vec::new();
        let mut has_self = false;
        for (i, p) in d.params.iter().enumerate() {
            let ty = match &p.ty {
                None => {
                    if i != 0 {
                        self.err(fi, p.span, "`self` must be the first parameter");
                    }
                    match &recv_ty {
                        Some((_, t)) => {
                            has_self = true;
                            t.clone()
                        }
                        None => {
                            self.err(fi, p.span, format!("only methods take `self`; declare it as `fn Type.{}(self, ...)`", d.name.name));
                            Ty::Error
                        }
                    }
                }
                Some(t) => {
                    if !is_snake(&p.name.name) {
                        self.err(fi, p.name.span, format!("parameter names are snake_case: rename `{}` to `{}`", p.name.name, to_snake(&p.name.name)));
                    }
                    self.resolve_type(t, fi, &generics)
                }
            };
            if params.iter().any(|q: &ParamDef| q.name == p.name.name) {
                self.err(fi, p.name.span, format!("the parameter `{}` is declared twice", p.name.name));
            }
            params.push(ParamDef { name: p.name.name.clone(), mode: p.mode, ty, default: None });
        }
        let ret = d.ret.as_ref().map(|r| self.resolve_type(r, fi, &generics)).unwrap_or(Ty::Unit);
        let eff = self.resolve_effects(&d.effects, fi, &generics);
        let intrinsic = if d.body.is_none() { Some(display.clone()) } else { None };
        if d.body.is_none() && !file.std {
            self.err(fi, d.sig_span, "a function needs a body");
        }
        let module = file.module.clone();
        let symbol = format!("{module}.{display}");
        let id = self.fns.len();
        self.fns.push(FnDef {
            name: display.clone(),
            symbol,
            module,
            file: fi,
            generics,
            params,
            ret,
            eff,
            has_self,
            intrinsic,
            body: None,
            span: d.name.span,
        });
        match recv_ty {
            Some((key, ty)) => {
                let entry = self.methods.entry((key, d.name.name.clone())).or_default();
                entry.push(id);
                self.fn_recv.insert(id, ty);
            }
            None => {
                let scope = self.home_scope(fi);
                if let Some(&prev) = scope.fns.get(&d.name.name) {
                    let (line, _) = self.src(self.fns[prev].file).line_col(self.fns[prev].span.lo);
                    self.err(fi, d.name.span, format!("`{}` is already declared on line {line}; Overt has no overloading, so give one a different name", d.name.name));
                } else {
                    scope.fns.insert(d.name.name.clone(), id);
                }
            }
        }
        Some(id)
    }

    /// The receiver of `fn Recv[G].name`: its key, type and generic parameters.
    fn receiver(&mut self, rname: &ast::Ident, rgens: &[GenericParam], fi: FileId) -> Option<(RecvKey, Ty, Vec<GenericDef>)> {
        if rname.name == "[]" {
            if !self.files[fi].std {
                self.err(fi, rname.span, "only the standard library declares methods on arrays; write a function that takes the array instead");
                return None;
            }
            if let GenericParam::Type { name, bounds } = &rgens[0] {
                let concrete = match name.name.as_str() {
                    "str" => Some(Ty::Str),
                    n => IntTy::from_name(n).map(Ty::Int),
                };
                if let Some(t) = concrete {
                    let _ = bounds;
                    return Some((RecvKey::Array, Ty::array(t), Vec::new()));
                }
            }
            let g = self.generic_defs(rgens, fi, &[]);
            return Some((RecvKey::Array, Ty::array(Ty::Param(0)), g));
        }
        let builtin = match rname.name.as_str() {
            "str" => Some(Ty::Str),
            "bool" => Some(Ty::Bool),
            "f64" => Some(Ty::Float(FloatTy::F64)),
            "f32" => Some(Ty::Float(FloatTy::F32)),
            n => IntTy::from_name(n).map(Ty::Int),
        };
        if let Some(t) = builtin {
            if !self.files[fi].std {
                self.err(fi, rname.span, format!("only the standard library declares methods on `{}`; write a function that takes it instead", rname.name));
                return None;
            }
            return Some((RecvKey::of(&t).unwrap(), t, Vec::new()));
        }
        let Some(TypeRef::Adt(id)) = self.lookup_type(&rname.name, fi) else {
            self.err(fi, rname.span, format!("unknown type `{}`; methods are declared in the module of their type", rname.name));
            return None;
        };
        if self.adt_files[id] != fi && self.files[self.adt_files[id]].module != self.files[fi].module {
            self.err(fi, rname.span, format!("methods of `{}` must be declared in the module that declares the type", rname.name));
            return None;
        }
        let want = self.adts[id].type_params();
        if rgens.len() != want {
            let names: Vec<String> = self.adts[id].generics.iter().map(|g| g.name.clone()).collect();
            let shown = if want == 0 { rname.name.clone() } else { format!("{}[{}]", rname.name, names.join(", ")) };
            self.err(fi, rname.span, format!("`{}` has {want} type parameter{}; write `fn {shown}.name`", rname.name, if want == 1 { "" } else { "s" }));
            return None;
        }
        // The receiver's parameters keep the type's bounds, plus any the method adds.
        let mut gens = self.generic_defs(rgens, fi, &[]);
        for (g, tg) in gens.iter_mut().zip(self.adts[id].generics.clone()) {
            if let (GenericKind::Type(b), GenericKind::Type(tb)) = (&mut g.kind, &tg.kind) {
                b.eq |= tb.eq;
                b.ord |= tb.ord;
                b.hash |= tb.hash;
            }
        }
        let args = (0..want as u32).map(Ty::Param).collect();
        Some((RecvKey::Adt(id), Ty::Adt(id, args), gens))
    }

    fn find_main(&mut self) -> Option<FnId> {
        let main_file = self.files.iter().position(|f| !f.std && f.module == "main")?;
        let id = self.modules.get("main").and_then(|s| s.fns.get("main")).copied();
        match id {
            None => {
                self.err(main_file, Span::default(), "no `fn main`; the program starts at `fn main() ! io, fail`");
                None
            }
            Some(id) => {
                let f = &self.fns[id];
                if !f.params.is_empty() || f.ret != Ty::Unit || !f.generics.is_empty() {
                    let (file, span) = (f.file, f.span);
                    self.err(file, span, "`main` takes no parameters and returns nothing; read arguments with `os.args()`");
                }
                Some(id)
            }
        }
    }

    // ---- constants ----

    /// The checked value of a constant, checking it on first use.
    pub fn const_value(&mut self, id: usize) -> Option<TExpr> {
        if let Some(v) = &self.consts[id].value {
            return Some(v.clone());
        }
        if self.consts[id].checking {
            let (file, span, name) = (self.consts[id].file, self.consts[id].span, self.consts[id].name.clone());
            self.err(file, span, format!("the constant `{name}` depends on itself"));
            return None;
        }
        self.consts[id].checking = true;
        let (file, ty) = (self.consts[id].file, self.consts[id].ty.clone());
        let expr = self.consts[id].expr;
        let v = self.check_const_expr(expr, file, ty.as_ref());
        self.consts[id].value = Some(v.clone());
        self.consts[id].checking = false;
        Some(v)
    }

    // ---- tests ----

    fn collect_tests(&mut self, pending: &[PendingFn<'a>], sel: TestSel) {
        if sel == TestSel::None {
            return;
        }
        for p in pending {
            let std = self.files[p.file].std;
            if (sel == TestSel::Std) != std {
                continue;
            }
            for ex in &p.ast.ex {
                self.check_example(p.id, ex, p.file);
            }
        }
        for (fi, f) in self.files.iter().enumerate() {
            if (sel == TestSel::Std) != f.std {
                continue;
            }
            for item in &f.ast.items {
                if let ItemKind::Test(t) = &item.kind {
                    self.check_test_block(t, fi);
                }
            }
        }
    }

    pub fn fn_sig_text(&self, id: FnId) -> String {
        let f = &self.fns[id];
        let names = type_param_names(&f.generics);
        let params: Vec<String> = f
            .params
            .iter()
            .map(|p| {
                if p.name == "self" {
                    match p.mode {
                        Mode::Inout => "inout self".into(),
                        Mode::Sink => "sink self".into(),
                        Mode::Read => "self".into(),
                    }
                } else {
                    let mode = match p.mode {
                        Mode::Inout => "inout ",
                        Mode::Sink => "sink ",
                        Mode::Read => "",
                    };
                    format!("{}: {mode}{}", p.name, self.ty_name_with(&p.ty, &names))
                }
            })
            .collect();
        let ret = if f.ret == Ty::Unit { String::new() } else { format!(" -> {}", self.ty_name_with(&f.ret, &names)) };
        format!("{}({}){ret}{}", f.name, params.join(", "), self.eff_text(&f.eff, &f.generics))
    }

    pub fn eff_text(&self, e: &Eff, generics: &[GenericDef]) -> String {
        let mut parts = Vec::new();
        if e.io {
            parts.push("io".to_string());
        }
        if e.fail {
            parts.push("fail".to_string());
        }
        for p in &e.params {
            let name = generics.iter().find(|g| matches!(g.kind, GenericKind::Effect) && g.index == *p).map(|g| g.name.clone());
            parts.push(name.unwrap_or_else(|| "E".into()));
        }
        if parts.is_empty() { String::new() } else { format!(" ! {}", parts.join(", ")) }
    }

    /// A type as written in Overt, for messages.
    pub fn ty_name(&self, t: &Ty) -> String {
        self.ty_name_with(t, &[])
    }

    pub fn ty_name_with(&self, t: &Ty, params: &[String]) -> String {
        match t {
            Ty::Int(k) => k.name().into(),
            Ty::Float(k) => k.name().into(),
            Ty::Bool => "bool".into(),
            Ty::Str => "str".into(),
            Ty::Dur => "Dur".into(),
            Ty::Unit => "nothing".into(),
            Ty::Never => "never".into(),
            Ty::Error | Ty::Var(_) => "_".into(),
            Ty::Array(e) => format!("[{}]", self.ty_name_with(e, params)),
            Ty::Opt(e) => format!("?{}", self.ty_name_with(e, params)),
            Ty::Tuple(ts) => format!("({})", ts.iter().map(|t| self.ty_name_with(t, params)).collect::<Vec<_>>().join(", ")),
            Ty::Adt(id, args) => {
                let a = &self.adts[*id];
                let name = if a.module == "main" || self.files[a.file].open { a.name.clone() } else { format!("{}.{}", a.module, a.name) };
                if args.is_empty() {
                    name
                } else {
                    format!("{name}[{}]", args.iter().map(|t| self.ty_name_with(t, params)).collect::<Vec<_>>().join(", "))
                }
            }
            Ty::Fn(f) => {
                let ps: Vec<String> = f.params.iter().map(|t| self.ty_name_with(t, params)).collect();
                let ret = if f.ret == Ty::Unit { String::new() } else { format!(" -> {}", self.ty_name_with(&f.ret, params)) };
                format!("fn({}){ret}{}", ps.join(", "), self.eff_text(&f.eff, &[]))
            }
            Ty::Param(i) => params.get(*i as usize).cloned().unwrap_or_else(|| format!("T{i}")),
        }
    }
}

/// Names of the type parameters, by index, for showing `Ty::Param`.
pub fn type_param_names(generics: &[GenericDef]) -> Vec<String> {
    let mut names: Vec<(u32, String)> =
        generics.iter().filter(|g| matches!(g.kind, GenericKind::Type(_))).map(|g| (g.index, g.name.clone())).collect();
    names.sort();
    names.into_iter().map(|(_, n)| n).collect()
}
