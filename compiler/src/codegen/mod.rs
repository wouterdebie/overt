//! Textual LLVM IR from the typed program.
//!
//! Every generic function is emitted once per set of concrete type arguments
//! (its instances), on demand from a work queue. Per-type helpers (dup, drop,
//! equality, comparison, hashing, showing) are generated the same way.
//!
//! Memory: strings and arrays are `{ptr buf, i64 off, i64 len}` views of a
//! reference-counted buffer (see runtime/rt.c). Copying a value increments the
//! counts it holds; dropping decrements them. A value is either owned (+1,
//! dropped by whoever holds it) or borrowed (valid while its owner lives).

mod expr;
mod func;
mod handles;
mod helpers;
mod intrin;
mod json;
mod par;
mod pat;

use crate::source::Source;
use crate::tir::*;
use crate::types::{AdtId, Eff, FloatTy, FnId, IntTy, Ty};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt::Write;

pub use func::Fx;

pub enum Entry {
    Main,
    Tests { filter: Option<String> },
}

/// An LLVM value: its type and how to write it as an operand.
#[derive(Clone, Debug)]
pub struct V {
    pub ty: String,
    pub repr: String,
}

impl V {
    pub fn new(ty: impl Into<String>, repr: impl Into<String>) -> V {
        V { ty: ty.into(), repr: repr.into() }
    }

    pub fn unit() -> V {
        V::new("{}", "zeroinitializer")
    }

    pub fn op(&self) -> String {
        format!("{} {}", self.ty, self.repr)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Helper {
    Dup,
    Drop,
    /// Marks every count a value holds as shared, before other tasks can reach it.
    Mark,
    Eq,
    Cmp,
    Hash,
    Show,
    /// Appends a value's JSON to a string being built (see `json.rs`).
    JsonEnc,
    /// Reads a value from JSON (see `json.rs`).
    JsonDec,
}

enum Work {
    Fn { f: FnId, targs: Vec<Ty>, eargs: Vec<Eff>, sym: String },
    Closure { id: ClosureId, targs: Vec<Ty>, eargs: Vec<Eff>, failable: bool, sym: String },
    Thunk { f: FnId, targs: Vec<Ty>, eargs: Vec<Eff>, failable: bool, sym: String },
    Helper { kind: Helper, ty: Ty, sym: String },
    ClosureDrop { id: ClosureId, targs: Vec<Ty>, eargs: Vec<Eff>, sym: String },
    ClosureMark { id: ClosureId, targs: Vec<Ty>, eargs: Vec<Eff>, sym: String },
    /// The body `task.map` runs for each index (see `intrin.rs`).
    MapBody { t: Ty, u: Ty, failable: bool, sym: String },
    /// The dispatch function of a `par` (see `par.rs`).
    ParBody { sigs: Vec<(Ty, bool)>, sym: String },
    /// The body `task.timeout` runs in its child task (see `handles.rs`).
    TimeoutBody { t: Ty, fails: bool, sym: String },
}

pub struct Gen<'p> {
    pub p: &'p Program,
    pub srcs: &'p [&'p Source],
    body: String,
    type_defs: Vec<String>,
    named: HashMap<Ty, String>,
    layouts: HashMap<Ty, (u64, u64)>,
    fn_insts: HashMap<(FnId, Vec<Ty>, bool), String>,
    closure_insts: HashMap<(ClosureId, Vec<Ty>, bool, bool), String>,
    closure_drops: HashMap<(ClosureId, Vec<Ty>), String>,
    closure_marks: HashMap<(ClosureId, Vec<Ty>), String>,
    map_bodies: HashMap<(Ty, Ty, bool), String>,
    par_bodies: HashMap<Vec<(Ty, bool)>, String>,
    timeout_bodies: HashMap<(Ty, bool), String>,
    thunks: HashMap<(FnId, Vec<Ty>, bool, bool), String>,
    helpers: HashMap<(Helper, Ty), String>,
    helper_syms: std::collections::HashSet<String>,
    queue: VecDeque<Work>,
    strs: HashMap<Vec<u8>, String>,
    /// Static arrays of number literals, by their initializer.
    arrs: HashMap<String, String>,
    str_defs: Vec<String>,
    cstrs: HashMap<Vec<u8>, String>,
    cstr_defs: Vec<String>,
    decls: BTreeSet<String>,
    pub f: Fx,
}

pub fn emit(p: &Program, srcs: &[&Source], entry: Entry) -> String {
    let mut g = Gen {
        p,
        srcs,
        body: String::new(),
        type_defs: Vec::new(),
        named: HashMap::new(),
        layouts: HashMap::new(),
        fn_insts: HashMap::new(),
        closure_insts: HashMap::new(),
        closure_drops: HashMap::new(),
        closure_marks: HashMap::new(),
        map_bodies: HashMap::new(),
        par_bodies: HashMap::new(),
        timeout_bodies: HashMap::new(),
        thunks: HashMap::new(),
        helpers: HashMap::new(),
        helper_syms: std::collections::HashSet::new(),
        queue: VecDeque::new(),
        strs: HashMap::new(),
        arrs: HashMap::new(),
        str_defs: Vec::new(),
        cstrs: HashMap::new(),
        cstr_defs: Vec::new(),
        decls: BTreeSet::new(),
        f: Fx::default(),
    };
    let main_body = match entry {
        Entry::Main => g.main_entry(),
        Entry::Tests { filter } => g.test_entry(filter.as_deref()),
    };
    while let Some(w) = g.queue.pop_front() {
        let text = match w {
            Work::Fn { f, targs, eargs, sym } => g.emit_fn(f, &targs, &eargs, &sym),
            Work::Closure { id, targs, eargs, failable, sym } => g.emit_closure(id, &targs, &eargs, failable, &sym),
            Work::Thunk { f, targs, eargs, failable, sym } => g.emit_thunk(f, &targs, &eargs, failable, &sym),
            Work::Helper { kind, ty, sym } => g.emit_helper(kind, &ty, &sym),
            Work::ClosureDrop { id, targs, eargs, sym } => g.emit_closure_drop(id, &targs, &eargs, &sym),
            Work::ClosureMark { id, targs, eargs, sym } => g.emit_closure_mark(id, &targs, &eargs, &sym),
            Work::MapBody { t, u, failable, sym } => g.emit_map_body(&t, &u, failable, &sym),
            Work::ParBody { sigs, sym } => g.emit_par_body(&sigs, &sym),
            Work::TimeoutBody { t, fails, sym } => g.emit_timeout_body(&t, fails, &sym),
        };
        g.body.push_str(&text);
        g.body.push('\n');
    }
    let mut out = String::new();
    out.push_str("; generated by ovt\n\n%ovt.arr = type { ptr, i64, i64 }\n%ovt.fn = type { ptr, ptr }\n");
    for d in &g.type_defs {
        out.push_str(d);
        out.push('\n');
    }
    out.push('\n');
    for d in &g.str_defs {
        out.push_str(d);
        out.push('\n');
    }
    for d in &g.cstr_defs {
        out.push_str(d);
        out.push('\n');
    }
    out.push('\n');
    out.push_str(RUNTIME_DECLS);
    for d in &g.decls {
        out.push_str(d);
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&g.body);
    out.push_str(&main_body);
    out
}

const RUNTIME_DECLS: &str = "\
declare void @ovt_rt_init(i32, ptr)
declare void @ovt_rt_exit()
declare void @ovt_flush()
declare ptr @ovt_alloc(i64)
declare void @ovt_free(ptr)
declare void @ovt_buf_release(ptr, i64, ptr)
declare void @ovt_box_release(ptr, ptr)
declare void @ovt_env_release(ptr)
declare void @ovt_rc_inc(ptr)
declare void @ovt_mark_buf(ptr, i64, ptr)
declare void @ovt_mark_box(ptr, ptr)
declare void @ovt_mark_env(ptr)
declare void @ovt_mark_none(ptr)
declare void @ovt_shared_lock(ptr, ptr, i32, i32)
declare void @ovt_shared_unlock(ptr)
declare ptr @ovt_shared_value(ptr)
declare ptr @ovt_shared_new(i64, ptr, ptr)
declare ptr @ovt_chan_new(i64, i64, ptr)
declare i32 @ovt_chan_send(ptr, ptr, ptr)
declare i32 @ovt_chan_recv(ptr, ptr)
declare void @ovt_chan_close(ptr)
declare i64 @ovt_chan_len(ptr)
declare i32 @ovt_net_listen(ptr, ptr, ptr)
declare i32 @ovt_net_connect(ptr, ptr, ptr)
declare i32 @ovt_net_accept(ptr, ptr, ptr)
declare i32 @ovt_net_read_line(ptr, ptr, i32, ptr, ptr)
declare i32 @ovt_net_read(ptr, ptr, ptr)
declare void @ovt_sb_json_str(ptr, ptr)
declare void @ovt_sb_json_f64(ptr, double)
declare ptr @ovt_jp_new(ptr)
declare i32 @ovt_jp_finish(ptr, i32, ptr)
declare i32 @ovt_jp_null(ptr)
declare i32 @ovt_jp_bool(ptr)
declare i32 @ovt_jp_int(ptr, ptr, i64, i64)
declare i32 @ovt_jp_uint(ptr, ptr)
declare i32 @ovt_jp_f64(ptr, ptr)
declare i32 @ovt_jp_str(ptr, ptr)
declare i32 @ovt_jp_arr(ptr)
declare i32 @ovt_jp_obj(ptr)
declare i32 @ovt_jp_next(ptr, i32)
declare i32 @ovt_jp_key(ptr, i32)
declare i32 @ovt_jp_name(ptr)
declare i32 @ovt_jp_key_is(ptr, ptr, i64)
declare void @ovt_jp_take_key(ptr, ptr)
declare void @ovt_jp_at_field(ptr, ptr, i64)
declare void @ovt_jp_at_index(ptr, i64)
declare void @ovt_jp_at_key(ptr, ptr)
declare void @ovt_jp_missing(ptr, ptr, i64)
declare void @ovt_jp_bad_name(ptr, ptr, i64)
declare void @ovt_jp_bad_len(ptr, i64)
declare void @ovt_jp_one_key(ptr)
declare void @ovt_jp_expected(ptr, ptr)
declare i32 @ovt_jp_peek(ptr)
declare i32 @ovt_jp_skip(ptr)
declare i32 @ovt_net_read_exactly(ptr, i64, ptr, ptr)
declare i32 @ovt_net_write(ptr, ptr, ptr)
declare void @ovt_net_set_timeout(ptr, i64)
declare void @ovt_net_close(ptr)
declare void @ovt_net_peer(ptr, ptr)
declare i64 @ovt_net_port(ptr)
declare void @ovt_time_sleep(i64)
declare i64 @ovt_time_monotonic()
declare ptr @ovt_group_new()
declare void @ovt_group_spawn(ptr, ptr, ptr, ptr, i32, i32)
declare void @ovt_group_wait(ptr, i32)
declare i32 @ovt_timeout(i64, ptr, ptr)
declare i32 @ovt_rt_run(ptr)
declare i64 @ovt_parallel(i64, ptr, ptr)
declare ptr @ovt_buf_new(i64, i64)
declare ptr @ovt_calloc(i64)
declare void @ovt_parallel_cleanup(ptr, i64, ptr, i64, ptr, ptr, i64)
declare i32 @ovt_fs_list(ptr, i32, ptr, ptr)
declare void @ovt_box_unique(ptr, i64, ptr, ptr)
declare void @ovt_arr_unique(ptr, i64, ptr, ptr)
declare ptr @ovt_arr_push(ptr, i64, ptr, ptr)
declare i32 @ovt_arr_pop(ptr, i64, ptr, ptr, ptr)
declare ptr @ovt_arr_insert(ptr, i64, i64, ptr, ptr, ptr, i32, i32)
declare void @ovt_arr_remove(ptr, i64, i64, ptr, ptr, ptr, ptr, i32, i32)
declare void @ovt_arr_clear(ptr, i64, ptr)
declare void @ovt_arr_reverse(ptr, i64, ptr, ptr)
declare void @ovt_arr_sort(ptr, i64, ptr, ptr, ptr)
declare void @ovt_arr_slice(ptr, ptr, i64, i64, ptr, i32, i32)
declare void @ovt_str_slice(ptr, ptr, i64, i64, ptr, i32, i32)
declare void @ovt_str_append_bytes(ptr, ptr, i64)
declare void @ovt_str_append(ptr, ptr)
declare void @ovt_str_concat(ptr, ptr, ptr)
declare i32 @ovt_str_eq(ptr, ptr)
declare i32 @ovt_str_cmp(ptr, ptr)
declare i64 @ovt_str_find(ptr, ptr)
declare void @ovt_str_case(ptr, ptr, i32)
declare i32 @ovt_str_from_bytes(ptr, ptr)
declare void @ovt_str_runes(ptr, ptr)
declare i64 @ovt_hash_bytes(ptr)
declare i64 @ovt_hash_mix(i64, i64)
declare void @ovt_sb_int(ptr, i64)
declare void @ovt_sb_uint(ptr, i64)
declare void @ovt_sb_f64(ptr, double)
declare void @ovt_sb_dur(ptr, i64)
declare void @ovt_sb_cstr(ptr, ptr, i64)
declare void @ovt_sb_quoted(ptr, ptr)
declare i32 @ovt_parse_int(ptr, ptr)
declare i32 @ovt_parse_f64(ptr, ptr)
declare void @ovt_print(ptr, i32)
declare void @ovt_dbg(ptr, ptr, i32, i32, ptr)
declare void @ovt_os_args(ptr)
declare i32 @ovt_os_env(ptr, ptr)
declare void @ovt_os_exit(i64) noreturn
declare i32 @ovt_fs_read(ptr, ptr, i32, ptr)
declare i32 @ovt_read_stdin(ptr, i32, ptr)
declare i32 @ovt_fs_write(ptr, ptr, ptr)
declare i32 @ovt_fs_exists(ptr)
declare void @ovt_main_failed(ptr) noreturn
declare void @ovt_trap(ptr, i64, ptr, i32, i32) noreturn cold
declare void @ovt_trap_str(ptr, ptr, i32, i32) noreturn cold
declare void @ovt_trap_index(i64, i64, ptr, i32, i32) noreturn cold
declare void @ovt_test_run(ptr, ptr)
declare i32 @ovt_test_finish()
declare void @ovt_expect_failed(ptr, ptr)
declare void @ovt_note(ptr)
declare void @ovt_note_str(ptr, ptr)
declare double @llvm.fabs.f64(double)
declare double @llvm.sqrt.f64(double)
declare double @llvm.floor.f64(double)
declare double @llvm.ceil.f64(double)
declare double @llvm.round.f64(double)
";

pub fn escape_bytes(bytes: &[u8]) -> String {
    let mut s = String::new();
    for &b in bytes {
        if (0x20..0x7f).contains(&b) && b != b'"' && b != b'\\' {
            s.push(b as char);
        } else {
            let _ = write!(s, "\\{b:02X}");
        }
    }
    s
}

fn quote_sym(s: &str) -> String {
    s.replace('"', "'").replace('\\', "/")
}

impl<'p> Gen<'p> {
    // ---- constants ----

    /// A static string: a buffer with count 0, never freed.
    pub fn str_const(&mut self, bytes: &[u8]) -> V {
        if bytes.is_empty() {
            return V::new("%ovt.arr", "zeroinitializer");
        }
        let name = match self.strs.get(bytes) {
            Some(n) => n.clone(),
            None => {
                let n = format!("@.str.{}", self.strs.len());
                self.str_defs.push(format!(
                    "{n} = private unnamed_addr constant {{ i64, i64, i64, [{l} x i8] }} {{ i64 0, i64 {l}, i64 {l}, [{l} x i8] c\"{}\" }}, align 8",
                    escape_bytes(bytes),
                    l = bytes.len()
                ));
                self.strs.insert(bytes.to_vec(), n.clone());
                n
            }
        };
        V::new("%ovt.arr", format!("{{ ptr {name}, i64 0, i64 {} }}", bytes.len()))
    }

    /// An array literal of number literals as a static buffer, like a string
    /// literal: its count is 0, so it's never freed, and changing it copies it.
    /// Returns `None` for any other array literal.
    pub fn static_array(&mut self, et: &Ty, items: &[TExpr]) -> Option<V> {
        if !matches!(et, Ty::Int(_) | Ty::Float(_)) {
            return None;
        }
        let elt = self.lty(et);
        let mut vals = Vec::new();
        for it in items {
            let repr = match (&it.kind, et) {
                (TK::Int(v), Ty::Float(_)) => Self::float_const(*v as f64, et),
                (TK::Int(v), _) if *v > i64::MAX as i128 => (*v as u64 as i64).to_string(),
                (TK::Int(v), _) => v.to_string(),
                (TK::Float(f), _) => Self::float_const(*f, et),
                _ => return None,
            };
            vals.push(format!("{elt} {repr}"));
        }
        let n = items.len();
        let init = format!("{{ i64, i64, i64, [{n} x {elt}] }} {{ i64 0, i64 {n}, i64 {n}, [{n} x {elt}] [{}] }}", vals.join(", "));
        let name = match self.arrs.get(&init) {
            Some(name) => name.clone(),
            None => {
                let name = format!("@.arr.{}", self.arrs.len());
                self.str_defs.push(format!("{name} = private unnamed_addr constant {init}, align 8"));
                self.arrs.insert(init, name.clone());
                name
            }
        };
        Some(V::new("%ovt.arr", format!("{{ ptr {name}, i64 0, i64 {n} }}")))
    }

    /// A NUL-terminated C string constant, for runtime messages.
    pub fn cstr(&mut self, bytes: &[u8]) -> String {
        if let Some(n) = self.cstrs.get(bytes) {
            return n.clone();
        }
        let n = format!("@.cstr.{}", self.cstrs.len());
        self.cstr_defs.push(format!("{n} = private unnamed_addr constant [{} x i8] c\"{}\\00\"", bytes.len() + 1, escape_bytes(bytes)));
        self.cstrs.insert(bytes.to_vec(), n.clone());
        n
    }

    pub fn file_cstr(&mut self, file: FileId) -> String {
        let path = self.srcs[file].path.clone();
        self.cstr(path.as_bytes())
    }

    // ---- types ----

    pub fn needs_rc(&self, t: &Ty) -> bool {
        match t {
            Ty::Str | Ty::Array(_) | Ty::Fn(_) => true,
            Ty::Opt(e) => self.needs_rc(e),
            Ty::Tuple(ts) => ts.iter().any(|t| self.needs_rc(t)),
            Ty::Adt(id, targs) => {
                let adt = &self.p.adts[*id];
                match &adt.kind {
                    AdtKind::Struct(fs) => fs.iter().any(|f| f.boxed || self.needs_rc(&f.ty.subst(targs, &[]))),
                    AdtKind::Enum(vs) => vs.iter().flat_map(|v| v.fields.iter()).any(|f| f.boxed || self.needs_rc(&f.ty.subst(targs, &[]))),
                }
            }
            _ => false,
        }
    }

    /// A readable, unique name for a concrete type, used in symbols.
    pub fn mangle(&self, t: &Ty) -> String {
        match t {
            Ty::Int(k) => k.name().into(),
            Ty::Float(k) => k.name().into(),
            Ty::Bool => "bool".into(),
            Ty::Str => "str".into(),
            Ty::Dur => "Dur".into(),
            Ty::Unit => "()".into(),
            Ty::Never => "never".into(),
            Ty::Error | Ty::Var(_) | Ty::Param(_) => "?".into(),
            Ty::Array(e) => format!("[{}]", self.mangle(e)),
            Ty::Opt(e) => format!("?{}", self.mangle(e)),
            Ty::Tuple(ts) => format!("({})", ts.iter().map(|t| self.mangle(t)).collect::<Vec<_>>().join(",")),
            Ty::Adt(id, ts) => {
                let a = &self.p.adts[*id];
                let base = format!("{}.{}", a.module, a.name);
                if ts.is_empty() { base } else { format!("{base}[{}]", ts.iter().map(|t| self.mangle(t)).collect::<Vec<_>>().join(",")) }
            }
            Ty::Fn(f) => format!(
                "fn({}){}{}",
                f.params.iter().map(|t| self.mangle(t)).collect::<Vec<_>>().join(","),
                self.mangle(&f.ret),
                if f.eff.fail { "!fail" } else { "" }
            ),
        }
    }

    pub fn is_payloadless_enum(&self, id: AdtId) -> bool {
        match &self.p.adts[id].kind {
            AdtKind::Enum(vs) => vs.iter().all(|v| v.fields.is_empty()),
            _ => false,
        }
    }

    /// The LLVM type of a concrete Overt type.
    pub fn lty(&mut self, t: &Ty) -> String {
        match t {
            Ty::Int(k) => format!("i{}", k.bits()),
            Ty::Float(FloatTy::F32) => "float".into(),
            Ty::Float(FloatTy::F64) => "double".into(),
            Ty::Bool => "i1".into(),
            Ty::Dur => "i64".into(),
            Ty::Str | Ty::Array(_) => "%ovt.arr".into(),
            Ty::Fn(_) => "%ovt.fn".into(),
            Ty::Unit | Ty::Never | Ty::Error | Ty::Var(_) | Ty::Param(_) => "{}".into(),
            Ty::Opt(e) => format!("{{ i1, {} }}", self.lty(e)),
            Ty::Tuple(ts) => {
                let parts: Vec<String> = ts.iter().map(|t| self.lty(t)).collect();
                format!("{{ {} }}", parts.join(", "))
            }
            Ty::Adt(id, _) => {
                if self.is_payloadless_enum(*id) {
                    return "i32".into();
                }
                if let Some(n) = self.named.get(t) {
                    return n.clone();
                }
                let name = format!("%\"T.{}\"", quote_sym(&self.mangle(t)));
                self.named.insert(t.clone(), name.clone());
                let def = self.adt_def(t);
                self.type_defs.push(format!("{name} = type {def}"));
                name
            }
        }
    }

    fn adt_def(&mut self, t: &Ty) -> String {
        let Ty::Adt(id, targs) = t else { unreachable!() };
        let adt = &self.p.adts[*id];
        match &adt.kind {
            AdtKind::Struct(fs) => {
                let fields: Vec<(bool, Ty)> = fs.iter().map(|f| (f.boxed, f.ty.subst(targs, &[]))).collect();
                let parts: Vec<String> = fields.iter().map(|(boxed, ty)| if *boxed { "ptr".into() } else { self.lty(ty) }).collect();
                if parts.is_empty() { "{}".into() } else { format!("{{ {} }}", parts.join(", ")) }
            }
            AdtKind::Enum(_) => {
                let words = self.payload_words(*id, targs);
                format!("{{ i32, [{words} x i64] }}")
            }
        }
    }

    /// The literal struct type holding a variant's fields.
    pub fn variant_lty(&mut self, id: AdtId, targs: &[Ty], v: usize) -> String {
        let fields: Vec<(bool, Ty)> = self.p.adts[id].variants()[v].fields.iter().map(|f| (f.boxed, f.ty.subst(targs, &[]))).collect();
        let parts: Vec<String> = fields.iter().map(|(boxed, ty)| if *boxed { "ptr".into() } else { self.lty(ty) }).collect();
        format!("{{ {} }}", parts.join(", "))
    }

    fn payload_words(&mut self, id: AdtId, targs: &[Ty]) -> u64 {
        let n = self.p.adts[id].variants().len();
        let mut max = 0;
        for v in 0..n {
            let fields: Vec<(bool, Ty)> = self.p.adts[id].variants()[v].fields.iter().map(|f| (f.boxed, f.ty.subst(targs, &[]))).collect();
            let (mut size, mut align) = (0u64, 1u64);
            for (boxed, ty) in fields {
                let (s, a) = if boxed { (8, 8) } else { self.size_align(&ty) };
                size = size.div_ceil(a) * a + s;
                align = align.max(a);
            }
            max = max.max(size.div_ceil(align) * align);
        }
        max.div_ceil(8)
    }

    /// Size and alignment, following LLVM's default data layout.
    pub fn size_align(&mut self, t: &Ty) -> (u64, u64) {
        if let Some(l) = self.layouts.get(t) {
            return *l;
        }
        let l = match t {
            Ty::Int(k) => {
                let b = (k.bits() / 8) as u64;
                (b, b)
            }
            Ty::Float(FloatTy::F32) => (4, 4),
            Ty::Float(FloatTy::F64) | Ty::Dur => (8, 8),
            Ty::Bool => (1, 1),
            Ty::Str | Ty::Array(_) => (24, 8),
            Ty::Fn(_) => (16, 8),
            Ty::Unit | Ty::Never | Ty::Error | Ty::Var(_) | Ty::Param(_) => (0, 1),
            Ty::Opt(e) => self.struct_layout(&[(false, Ty::Bool), (false, (**e).clone())]),
            Ty::Tuple(ts) => {
                let fs: Vec<(bool, Ty)> = ts.iter().map(|t| (false, t.clone())).collect();
                self.struct_layout(&fs)
            }
            Ty::Adt(id, targs) => {
                if self.is_payloadless_enum(*id) {
                    (4, 4)
                } else if self.p.adts[*id].is_enum() {
                    let w = self.payload_words(*id, targs);
                    (8 + 8 * w, 8)
                } else {
                    let fs: Vec<(bool, Ty)> = self.p.adts[*id].fields().iter().map(|f| (f.boxed, f.ty.subst(targs, &[]))).collect();
                    self.struct_layout(&fs)
                }
            }
        };
        self.layouts.insert(t.clone(), l);
        l
    }

    fn struct_layout(&mut self, fields: &[(bool, Ty)]) -> (u64, u64) {
        let (mut size, mut align) = (0u64, 1u64);
        for (boxed, ty) in fields {
            let (s, a) = if *boxed { (8, 8) } else { self.size_align(ty) };
            size = size.div_ceil(a) * a + s;
            align = align.max(a);
        }
        (size.div_ceil(align) * align, align)
    }

    /// The LLVM type a function returns: failable functions return
    /// `{ i1 failed, T, Err }`.
    pub fn ret_lty(&mut self, ret: &Ty, failable: bool) -> String {
        if failable {
            let t = self.lty(ret);
            let e = self.err_lty();
            format!("{{ i1, {t}, {e} }}")
        } else if *ret == Ty::Unit || *ret == Ty::Never {
            "void".into()
        } else {
            self.lty(ret)
        }
    }

    pub fn err_ty(&self) -> Ty {
        Ty::Adt(self.p.known.err, Vec::new())
    }

    pub fn err_lty(&mut self) -> String {
        let t = self.err_ty();
        self.lty(&t)
    }

    // ---- instances ----

    /// The symbol of function `f` instantiated with `targs`, queued for emission.
    pub fn fn_inst(&mut self, f: FnId, targs: &[Ty], eargs: &[Eff]) -> String {
        let failable = self.p.fns[f].eff.subst(eargs).fail;
        let key = (f, targs.to_vec(), failable);
        if let Some(s) = self.fn_insts.get(&key) {
            return s.clone();
        }
        let def = &self.p.fns[f];
        let mut sym = def.symbol.clone();
        if !targs.is_empty() {
            sym.push_str(&format!("<{}>", targs.iter().map(|t| self.mangle(t)).collect::<Vec<_>>().join(",")));
        }
        if failable && !def.eff.fail {
            sym.push_str("!fail");
        }
        let sym = format!("@\"{}\"", quote_sym(&sym));
        self.fn_insts.insert(key, sym.clone());
        self.queue.push_back(Work::Fn { f, targs: targs.to_vec(), eargs: eargs.to_vec(), sym: sym.clone() });
        sym
    }

    pub fn fn_failable(&self, f: FnId, eargs: &[Eff]) -> bool {
        self.p.fns[f].eff.subst(eargs).fail
    }

    pub fn closure_inst(&mut self, id: ClosureId, targs: &[Ty], eargs: &[Eff], failable: bool) -> String {
        let key = (id, targs.to_vec(), failable, eargs.iter().any(|e| e.fail));
        if let Some(s) = self.closure_insts.get(&key) {
            return s.clone();
        }
        let sym = format!("@\"closure.{id}<{}>{}{}\"", targs.iter().map(|t| quote_sym(&self.mangle(t))).collect::<Vec<_>>().join(","), if failable { "!fail" } else { "" }, if key.3 { "!E" } else { "" });
        self.closure_insts.insert(key, sym.clone());
        self.queue.push_back(Work::Closure { id, targs: targs.to_vec(), eargs: eargs.to_vec(), failable, sym: sym.clone() });
        sym
    }

    pub fn closure_drop(&mut self, id: ClosureId, targs: &[Ty], eargs: &[Eff]) -> String {
        let key = (id, targs.to_vec());
        if let Some(s) = self.closure_drops.get(&key) {
            return s.clone();
        }
        let sym = format!("@\"closure.{id}<{}>.drop\"", targs.iter().map(|t| quote_sym(&self.mangle(t))).collect::<Vec<_>>().join(","));
        self.closure_drops.insert(key, sym.clone());
        self.queue.push_back(Work::ClosureDrop { id, targs: targs.to_vec(), eargs: eargs.to_vec(), sym: sym.clone() });
        sym
    }

    pub fn closure_mark(&mut self, id: ClosureId, targs: &[Ty], eargs: &[Eff]) -> String {
        let key = (id, targs.to_vec());
        if let Some(s) = self.closure_marks.get(&key) {
            return s.clone();
        }
        let sym = format!("@\"closure.{id}<{}>.mark\"", targs.iter().map(|t| quote_sym(&self.mangle(t))).collect::<Vec<_>>().join(","));
        self.closure_marks.insert(key, sym.clone());
        self.queue.push_back(Work::ClosureMark { id, targs: targs.to_vec(), eargs: eargs.to_vec(), sym: sym.clone() });
        sym
    }

    pub fn timeout_body(&mut self, t: &Ty, fails: bool) -> String {
        let key = (t.clone(), fails);
        if let Some(s) = self.timeout_bodies.get(&key) {
            return s.clone();
        }
        let sym = format!("@\"task.timeout.body<{}>{}\"", quote_sym(&self.mangle(t)), if fails { ".fails" } else { "" });
        self.timeout_bodies.insert(key, sym.clone());
        self.queue.push_back(Work::TimeoutBody { t: t.clone(), fails, sym: sym.clone() });
        sym
    }

    pub fn par_body(&mut self, sigs: &[(Ty, bool)]) -> String {
        if let Some(s) = self.par_bodies.get(sigs) {
            return s.clone();
        }
        let sym = format!("@\"par.body.{}\"", self.par_bodies.len());
        self.par_bodies.insert(sigs.to_vec(), sym.clone());
        self.queue.push_back(Work::ParBody { sigs: sigs.to_vec(), sym: sym.clone() });
        sym
    }

    pub fn map_body(&mut self, t: &Ty, u: &Ty, failable: bool) -> String {
        let key = (t.clone(), u.clone(), failable);
        if let Some(s) = self.map_bodies.get(&key) {
            return s.clone();
        }
        let sym = format!("@\"task.map.body<{},{}>{}\"", quote_sym(&self.mangle(t)), quote_sym(&self.mangle(u)), if failable { ".fails" } else { "" });
        self.map_bodies.insert(key, sym.clone());
        self.queue.push_back(Work::MapBody { t: t.clone(), u: u.clone(), failable, sym: sym.clone() });
        sym
    }

    /// A function taking an environment pointer first, calling named function `f`.
    pub fn thunk(&mut self, f: FnId, targs: &[Ty], eargs: &[Eff], failable: bool) -> String {
        let inner = self.fn_failable(f, eargs);
        let key = (f, targs.to_vec(), failable, inner);
        if let Some(s) = self.thunks.get(&key) {
            return s.clone();
        }
        let target = self.fn_inst(f, targs, eargs);
        let sym = format!("@\"thunk.{}{}\"", &target[2..target.len() - 1], if failable { "!fail" } else { "" });
        self.thunks.insert(key, sym.clone());
        self.queue.push_back(Work::Thunk { f, targs: targs.to_vec(), eargs: eargs.to_vec(), failable, sym: sym.clone() });
        sym
    }

    /// The symbol of a per-type helper, queued for emission.
    pub fn helper(&mut self, kind: Helper, ty: &Ty) -> String {
        if let Some(s) = self.helpers.get(&(kind, ty.clone())) {
            return s.clone();
        }
        let k = match kind {
            Helper::Dup => "dup",
            Helper::Drop => "drop",
            Helper::Mark => "mark",
            Helper::Eq => "eq",
            Helper::Cmp => "cmp",
            Helper::Hash => "hash",
            Helper::Show => "show",
            Helper::JsonEnc => "jsonenc",
            Helper::JsonDec => "jsondec",
        };
        let sym = format!("@\"{k}.{}\"", quote_sym(&self.mangle(ty)));
        self.helpers.insert((kind, ty.clone()), sym.clone());
        // Types that differ only in effects (`fn() -> int` and `fn() -> int ! io`)
        // have the same name and layout, so they share a helper.
        if self.helper_syms.insert(sym.clone()) {
            self.queue.push_back(Work::Helper { kind, ty: ty.clone(), sym: sym.clone() });
        }
        sym
    }

    /// The drop helper as a function pointer for the runtime, or `null` when nothing needs dropping.
    pub fn drop_fn(&mut self, ty: &Ty) -> String {
        if self.needs_rc(ty) { self.helper(Helper::Drop, ty) } else { "null".into() }
    }

    pub fn dup_fn(&mut self, ty: &Ty) -> String {
        if self.needs_rc(ty) { self.helper(Helper::Dup, ty) } else { "null".into() }
    }

    pub fn mark_fn(&mut self, ty: &Ty) -> String {
        if self.needs_rc(ty) { self.helper(Helper::Mark, ty) } else { "null".into() }
    }

    // ---- entry points ----

    /// `main` runs as the first task: `@main` starts the runtime with
    /// `main.task`, which calls the program's `main`.
    fn main_entry(&mut self) -> String {
        let Some(main) = self.p.main else { return String::new() };
        let sym = self.fn_inst(main, &[], &[]);
        let failable = self.p.fns[main].eff.fail;
        let mut s = String::from("define i32 @main(i32 %argc, ptr %argv) {\nentry:\n  call void @ovt_rt_init(i32 %argc, ptr %argv)\n  %status = call i32 @ovt_rt_run(ptr @\"main.task\")\n  call void @ovt_rt_exit()\n  ret i32 0\n}\n\ndefine internal i32 @\"main.task\"() {\nentry:\n");
        if failable {
            let rt = self.ret_lty(&Ty::Unit, true);
            let et = self.err_lty();
            let _ = write!(
                s,
                "  %r = call {rt} {sym}()\n  %failed = extractvalue {rt} %r, 0\n  br i1 %failed, label %fail, label %ok\nfail:\n  %err = extractvalue {rt} %r, 2\n  %msg = extractvalue {et} %err, 1\n  %slot = alloca %ovt.arr\n  store %ovt.arr %msg, ptr %slot\n  call void @ovt_main_failed(ptr %slot)\n  unreachable\nok:\n"
            );
        } else {
            let _ = writeln!(s, "  call void {sym}()");
        }
        s.push_str("  ret i32 0\n}\n");
        s
    }

    fn test_entry(&mut self, filter: Option<&str>) -> String {
        let mut s = String::from("define i32 @main(i32 %argc, ptr %argv) {\nentry:\n  call void @ovt_rt_init(i32 %argc, ptr %argv)\n");
        let mut wrappers = String::new();
        for (i, t) in self.p.tests.iter().enumerate() {
            if let Some(f) = filter {
                if !t.name.contains(f) {
                    continue;
                }
            }
            let sym = self.fn_inst(t.func, &[], &[]);
            let failable = self.p.fns[t.func].eff.fail;
            let name = self.cstr(t.name.as_bytes());
            let w = format!("@\"test.run.{i}\"");
            if failable {
                let rt = self.ret_lty(&Ty::Unit, true);
                let et = self.err_lty();
                let _ = write!(
                    wrappers,
                    "define internal i32 {w}() {{\nentry:\n  %r = call {rt} {sym}()\n  %failed = extractvalue {rt} %r, 0\n  br i1 %failed, label %fail, label %ok\nfail:\n  %err = extractvalue {rt} %r, 2\n  %msg = extractvalue {et} %err, 1\n  %slot = alloca %ovt.arr\n  store %ovt.arr %msg, ptr %slot\n  call void @ovt_note_str(ptr {}, ptr %slot)\n  ret i32 1\nok:\n  ret i32 0\n}}\n",
                    self.cstr(b"failed: ")
                );
            } else {
                let _ = write!(wrappers, "define internal i32 {w}() {{\nentry:\n  call void {sym}()\n  ret i32 0\n}}\n");
            }
            let _ = writeln!(s, "  call void @ovt_test_run(ptr {name}, ptr {w})");
        }
        s.push_str("  %status = call i32 @ovt_test_finish()\n  ret i32 %status\n}\n");
        wrappers + &s
    }
}

pub fn int_ty(t: &Ty) -> Option<IntTy> {
    match t {
        Ty::Int(k) => Some(*k),
        _ => None,
    }
}
