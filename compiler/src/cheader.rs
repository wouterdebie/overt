//! `extern "lib" header "x.h"`: declarations generated from a C header.
//!
//! clang reads the header twice: `-E -dD` gives its macros, and
//! `-ast-dump=json` its declarations. The result is Overt source, an
//! `extern` block of the header's functions and the C types they use,
//! followed by its integer constants (from `enum`s and `#define`s). It's
//! written to the build directory and cached by a hash of the preprocessed
//! header, so an unchanged header isn't read again.
//!
//! Skipped, with a note in the generated file: variadic functions, functions
//! that pass structs by value or use types Overt has no match for, and
//! macros that aren't integer constants. Structs become opaque C types,
//! used behind pointers.

use crate::cjson::{self, Json};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Bumped when the generated code changes, so old caches aren't used.
const VERSION: &str = "1";

pub struct Imported {
    pub path: PathBuf,
    pub text: String,
}

pub struct Request<'a> {
    pub header: &'a str,
    pub lib: &'a str,
    pub blocking: bool,
    /// Names the `extern` block declares itself, which the header's don't replace.
    pub skip: &'a HashSet<String>,
    pub include: &'a Path,
    pub cache: &'a Path,
}

pub fn import(r: &Request) -> Result<Imported, String> {
    std::fs::create_dir_all(r.cache).map_err(|e| format!("can't create {}: {e}", r.cache.display()))?;
    let stem: String = r.header.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    let c_file = r.cache.join(format!("{stem}.c"));
    std::fs::write(&c_file, format!("#include \"{}\"\n", r.header)).map_err(|e| format!("can't write {}: {e}", c_file.display()))?;
    let clang = std::env::var("OVT_CLANG").unwrap_or_else(|_| "clang".into());
    let inc = format!("-I{}", r.include.display());
    let pre = Command::new(&clang).args(["-x", "c", "-E", "-dD", &inc]).arg(&c_file).output().map_err(|e| format!("can't run {clang}: {e}"))?;
    if !pre.status.success() {
        let err = String::from_utf8_lossy(&pre.stderr);
        let first = err.lines().find(|l| l.contains("error")).unwrap_or("clang failed").to_string();
        return Err(first.split("error: ").last().unwrap_or(&first).to_string());
    }
    let pre = String::from_utf8_lossy(&pre.stdout).into_owned();
    let mut skip: Vec<&String> = r.skip.iter().collect();
    skip.sort();
    let key = fnv(&format!("{VERSION}\0{}\0{}\0{}\0{:?}\0{pre}", r.header, r.lib, r.blocking, skip));
    let out = r.cache.join(format!("{stem}-{key:016x}.ovt"));
    if let Ok(text) = std::fs::read_to_string(&out) {
        return Ok(Imported { path: out, text });
    }
    let ast = Command::new(&clang).args(["-x", "c", "-fsyntax-only", "-Xclang", "-ast-dump=json", &inc]).arg(&c_file).output().map_err(|e| format!("can't run {clang}: {e}"))?;
    if !ast.status.success() {
        return Err("clang couldn't read the header".into());
    }
    let json = cjson::parse(&String::from_utf8_lossy(&ast.stdout))?;
    let c_name = c_file.to_string_lossy().into_owned();
    let text = generate(r, &json, &pre, &c_name);
    std::fs::write(&out, &text).map_err(|e| format!("can't write {}: {e}", out.display()))?;
    Ok(Imported { path: out, text })
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// A C type as Overt sees it.
#[derive(Clone, PartialEq)]
enum CTy {
    Void,
    Num(&'static str),
    Opaque(String),
    Ptr(Box<CTy>),
}

impl CTy {
    fn text(&self) -> String {
        match self {
            CTy::Void => "u8".into(),
            CTy::Num(n) => n.to_string(),
            CTy::Opaque(n) => n.clone(),
            CTy::Ptr(t) => format!("*{}", t.text()),
        }
    }

    fn opaque(&self) -> Option<&str> {
        match self {
            CTy::Opaque(n) => Some(n),
            CTy::Ptr(t) => t.opaque(),
            _ => None,
        }
    }
}

struct Types<'j> {
    typedefs: HashMap<String, &'j str>,
    /// Structs and unions with a definition, which can't pass by value.
    complete: HashSet<String>,
}

const KEYWORDS: &[&str] = &[
    "fn", "type", "enum", "const", "let", "var", "if", "else", "match", "for", "in", "while", "break", "continue", "return", "inout", "sink", "self", "par", "lock", "as", "extern", "unsafe", "none", "true", "false", "simd",
];

impl Types<'_> {
    /// Maps a C type spelled like clang's `qualType`.
    fn map(&self, q: &str, depth: usize) -> Result<CTy, String> {
        if depth > 20 {
            return Err(format!("`{q}` is nested too deeply"));
        }
        let mut s: String = q
            .split_whitespace()
            .filter(|w| !matches!(*w, "const" | "volatile" | "restrict" | "__restrict" | "_Nonnull" | "_Nullable" | "_Null_unspecified" | "__unsafe_unretained"))
            .collect::<Vec<_>>()
            .join(" ");
        if s.contains('(') {
            // A pointer to a function: opaque to Overt.
            return if s.contains("(*") || s.contains("(^") { Ok(CTy::Ptr(Box::new(CTy::Void))) } else { Err(format!("`{q}` is a function type")) };
        }
        let mut stars = 0;
        loop {
            let t = s.trim_end();
            if let Some(rest) = t.strip_suffix('*') {
                stars += 1;
                s = rest.to_string();
            } else if t.ends_with(']') {
                // An array parameter is a pointer.
                let open = t.rfind('[').unwrap();
                stars += 1;
                s = t[..open].to_string();
            } else {
                s = t.to_string();
                break;
            }
        }
        let base = s.trim();
        let mut ty = match base {
            "void" => CTy::Void,
            "char" | "unsigned char" => CTy::Num("u8"),
            "signed char" => CTy::Num("i8"),
            "short" | "short int" | "signed short" => CTy::Num("ffi.short"),
            "unsigned short" | "unsigned short int" => CTy::Num("ffi.ushort"),
            "int" | "signed" | "signed int" => CTy::Num("ffi.int"),
            "unsigned" | "unsigned int" => CTy::Num("ffi.uint"),
            "long" | "long int" | "signed long" => CTy::Num("ffi.long"),
            "unsigned long" | "unsigned long int" => CTy::Num("ffi.ulong"),
            "long long" | "long long int" | "signed long long" => CTy::Num("i64"),
            "unsigned long long" | "unsigned long long int" => CTy::Num("u64"),
            "float" => CTy::Num("f32"),
            "double" => CTy::Num("f64"),
            "_Bool" | "bool" => CTy::Num("bool"),
            "size_t" => CTy::Num("ffi.size"),
            "ssize_t" => CTy::Num("ffi.long"),
            "int8_t" => CTy::Num("i8"),
            "int16_t" => CTy::Num("i16"),
            "int32_t" => CTy::Num("i32"),
            "int64_t" => CTy::Num("i64"),
            "uint8_t" => CTy::Num("u8"),
            "uint16_t" => CTy::Num("u16"),
            "uint32_t" => CTy::Num("u32"),
            "uint64_t" => CTy::Num("u64"),
            "long double" | "__int128" | "unsigned __int128" | "__builtin_va_list" | "va_list" => return Err(format!("Overt has no `{base}`")),
            b if b.starts_with("enum ") => CTy::Num("ffi.int"),
            b if b.starts_with("struct ") || b.starts_with("union ") => {
                let tag = b.split_once(' ').unwrap().1;
                if tag.starts_with('(') {
                    return Err(format!("`{q}` has no name"));
                }
                if stars == 0 && self.complete.contains(b) {
                    return Err(format!("passes `{b}` by value"));
                }
                CTy::Opaque(tag.to_string())
            }
            name => match self.typedefs.get(name) {
                Some(target) => {
                    let t = target.trim();
                    if (t.starts_with("struct ") || t.starts_with("union ")) && !t.contains('(') && !t.contains('*') {
                        if stars == 0 && self.complete.contains(t) {
                            return Err(format!("passes `{name}` by value"));
                        }
                        CTy::Opaque(t.split_once(' ').unwrap().1.to_string())
                    } else if (t.starts_with("struct (") || t.starts_with("union (")) && !t.contains('*') {
                        if stars == 0 {
                            return Err(format!("passes `{name}` by value"));
                        }
                        CTy::Opaque(name.to_string())
                    } else {
                        self.map(t, depth + 1)?
                    }
                }
                None => return Err(format!("unknown type `{name}`")),
            },
        };
        for _ in 0..stars {
            ty = CTy::Ptr(Box::new(ty));
        }
        Ok(ty)
    }
}

/// The part of a function type before its parameter list: `int` in `int (char *)`.
fn return_part(q: &str) -> Option<&str> {
    let b = q.as_bytes();
    if *b.last()? != b')' {
        return None;
    }
    let mut depth = 0;
    for i in (0..b.len()).rev() {
        match b[i] {
            b')' => depth += 1,
            b'(' => {
                depth -= 1;
                if depth == 0 {
                    return Some(q[..i].trim());
                }
            }
            _ => {}
        }
    }
    None
}

fn loc_from<'j>(n: &'j Json) -> Option<&'j Json> {
    let loc = n.get("loc")?;
    Some(loc.get("expansionLoc").unwrap_or(loc))
}

fn generate(r: &Request, json: &Json, pre: &str, c_name: &str) -> String {
    let decls = json.get("inner").map(|i| i.arr()).unwrap_or(&[]);
    let mut types = Types { typedefs: HashMap::new(), complete: HashSet::new() };
    for d in decls {
        match d.path(&["kind"]) {
            Some("TypedefDecl") => {
                if let (Some(n), Some(t)) = (d.path(&["name"]), d.path(&["type", "qualType"])) {
                    types.typedefs.insert(n.to_string(), t);
                }
            }
            Some("RecordDecl") if d.get("completeDefinition").is_some() => {
                if let (Some(n), Some(k)) = (d.path(&["name"]), d.path(&["tagUsed"])) {
                    types.complete.insert(format!("{k} {n}"));
                }
            }
            _ => {}
        }
    }
    // Declarations written in the header itself: included from our file.
    let own = |d: &Json| loc_from(d).and_then(|l| l.path(&["includedFrom", "file"])).is_some_and(|f| f == c_name);
    let mut fns = String::new();
    let mut opaque: Vec<String> = Vec::new();
    let mut skipped = Vec::new();
    let mut consts: Vec<(String, i128)> = Vec::new();
    let mut seen = HashSet::new();
    for d in decls {
        if !own(d) {
            continue;
        }
        match d.path(&["kind"]) {
            Some("FunctionDecl") => {
                let Some(name) = d.path(&["name"]) else { continue };
                if r.skip.contains(name) || !seen.insert(name.to_string()) || name.starts_with('_') {
                    continue;
                }
                if d.get("variadic").is_some() {
                    skipped.push(format!("{name}: it's variadic"));
                    continue;
                }
                if d.path(&["storageClass"]) == Some("static") {
                    continue;
                }
                match function(&types, name, d) {
                    Ok((text, used)) => {
                        for u in used {
                            if !opaque.contains(&u) && !r.skip.contains(&u) {
                                opaque.push(u);
                            }
                        }
                        let b = if r.blocking { "blocking " } else { "" };
                        let _ = writeln!(fns, "  {b}fn {text}");
                    }
                    Err(why) => skipped.push(format!("{name}: {why}")),
                }
            }
            Some("EnumDecl") => {
                let mut next: i128 = 0;
                for c in d.get("inner").map(|i| i.arr()).unwrap_or(&[]) {
                    if c.path(&["kind"]) != Some("EnumConstantDecl") {
                        continue;
                    }
                    let Some(n) = c.path(&["name"]) else { continue };
                    let v = c.get("inner").and_then(|i| i.arr().iter().find_map(|x| x.path(&["value"]))).and_then(|v| v.parse::<i128>().ok()).unwrap_or(next);
                    next = v + 1;
                    consts.push((n.to_string(), v));
                }
            }
            _ => {}
        }
    }
    macros(pre, c_name, &mut consts);
    let mut out = String::new();
    let _ = writeln!(out, "// Generated by ovt from \"{}\". Don't edit; it's made again when the header changes.", r.header);
    let _ = writeln!(out, "extern \"{}\" {{", r.lib);
    for t in &opaque {
        let _ = writeln!(out, "  type {t}");
    }
    out.push_str(&fns);
    out.push_str("}\n");
    let mut names = HashSet::new();
    for (n, v) in consts {
        let upper = n.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') && n.starts_with(|c: char| c.is_ascii_uppercase());
        if !upper || r.skip.contains(&n) || !names.insert(n.clone()) {
            continue;
        }
        let ty = if i32::try_from(v).is_ok() { "ffi.int" } else if i64::try_from(v).is_ok() { "int" } else { continue };
        let _ = writeln!(out, "\nconst {n}: {ty} = {v}");
    }
    if !skipped.is_empty() {
        out.push_str("\n// Skipped:\n");
        for s in skipped {
            let _ = writeln!(out, "// - {s}");
        }
    }
    out
}

/// `name(params) -> ret`, and the C types it uses.
fn function(types: &Types, name: &str, d: &Json) -> Result<(String, Vec<String>), String> {
    let q = d.path(&["type", "qualType"]).ok_or("no type")?;
    let ret = return_part(q).ok_or_else(|| format!("can't read its type `{q}`"))?;
    let ret = types.map(ret, 0)?;
    let mut used = Vec::new();
    let mut params = Vec::new();
    let mut names = HashSet::new();
    for (i, p) in d.get("inner").map(|i| i.arr()).unwrap_or(&[]).iter().filter(|p| p.path(&["kind"]) == Some("ParmVarDecl")).enumerate() {
        let pq = p.path(&["type", "qualType"]).ok_or("a parameter has no type")?;
        let t = types.map(pq, 0)?;
        if t == CTy::Void {
            return Err("a parameter is `void`".into());
        }
        if let Some(o) = t.opaque() {
            used.push(o.to_string());
        }
        let mut pname = p.path(&["name"]).unwrap_or("").to_string();
        if pname.is_empty() || !pname.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || pname.starts_with(|c: char| c.is_ascii_digit()) {
            pname = format!("a{i}");
        }
        if KEYWORDS.contains(&pname.as_str()) {
            pname.push('_');
        }
        while !names.insert(pname.clone()) {
            pname.push('_');
        }
        params.push(format!("{pname}: {}", t.text()));
    }
    if let Some(o) = ret.opaque() {
        used.push(o.to_string());
    }
    let ret = if ret == CTy::Void { String::new() } else { format!(" -> {}", ret.text()) };
    Ok((format!("{name}({}){ret}", params.join(", ")), used))
}

/// Integer constants among the header's own `#define`s.
fn macros(pre: &str, c_name: &str, consts: &mut Vec<(String, i128)>) {
    let mut values: HashMap<String, i128> = consts.iter().cloned().collect();
    let mut header: Option<String> = None;
    let mut cur = String::new();
    for line in pre.lines() {
        if let Some(rest) = line.strip_prefix("# ") {
            // `# 12 "path" flags`: the lines that follow come from `path`.
            if let Some(start) = rest.find('"') {
                if let Some(end) = rest[start + 1..].find('"') {
                    cur = rest[start + 1..start + 1 + end].to_string();
                    if header.is_none() && cur != c_name && !cur.starts_with('<') {
                        header = Some(cur.clone());
                    }
                }
            }
            continue;
        }
        let Some(def) = line.strip_prefix("#define ") else { continue };
        if header.as_deref() != Some(cur.as_str()) {
            continue;
        }
        let name_end = def.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(def.len());
        let (name, body) = def.split_at(name_end);
        if body.starts_with('(') || name.is_empty() {
            continue;
        }
        if let Some(v) = eval(body.trim(), &values) {
            values.insert(name.to_string(), v);
            consts.push((name.to_string(), v));
        }
    }
}

/// An integer constant expression: literals, names of known constants,
/// parentheses, and `~ - + * / % << >> & ^ |`.
fn eval(s: &str, known: &HashMap<String, i128>) -> Option<i128> {
    let toks = tokens(s)?;
    if toks.is_empty() {
        return None;
    }
    let mut p = Eval { toks: &toks, i: 0, known };
    let v = p.binary(1)?;
    (p.i == toks.len()).then_some(v)
}

fn tokens(s: &str) -> Option<Vec<String>> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c.is_ascii_alphanumeric() || c == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push(s[start..i].to_string());
        } else if (c == b'<' || c == b'>') && b.get(i + 1) == Some(&c) {
            out.push(s[i..i + 2].to_string());
            i += 2;
        } else if b"()~-+*/%&^|".contains(&c) {
            out.push((c as char).to_string());
            i += 1;
        } else {
            return None;
        }
    }
    Some(out)
}

struct Eval<'t> {
    toks: &'t [String],
    i: usize,
    known: &'t HashMap<String, i128>,
}

impl Eval<'_> {
    fn prec(op: &str) -> Option<u8> {
        Some(match op {
            "|" => 1,
            "^" => 2,
            "&" => 3,
            "<<" | ">>" => 4,
            "+" | "-" => 5,
            "*" | "/" | "%" => 6,
            _ => return None,
        })
    }

    /// Operators binding at least as tightly as `min`, left to right.
    fn binary(&mut self, min: u8) -> Option<i128> {
        let mut l = self.unary()?;
        while let Some(op) = self.toks.get(self.i) {
            let Some(p) = Self::prec(op) else { break };
            if p < min {
                break;
            }
            let op = op.clone();
            self.i += 1;
            let r = self.binary(p + 1)?;
            l = match op.as_str() {
                "|" => l | r,
                "^" => l ^ r,
                "&" => l & r,
                "<<" => l.checked_shl(u32::try_from(r).ok()?)?,
                ">>" => l.checked_shr(u32::try_from(r).ok()?)?,
                "+" => l.checked_add(r)?,
                "-" => l.checked_sub(r)?,
                "*" => l.checked_mul(r)?,
                "/" => l.checked_div(r)?,
                _ => l.checked_rem(r)?,
            };
        }
        Some(l)
    }

    fn unary(&mut self) -> Option<i128> {
        let t = self.toks.get(self.i)?.clone();
        self.i += 1;
        match t.as_str() {
            "-" => self.unary().map(|v| -v),
            "+" => self.unary(),
            "~" => self.unary().map(|v| !v),
            "(" => {
                let v = self.binary(1)?;
                if self.toks.get(self.i).map(|s| s.as_str()) != Some(")") {
                    return None;
                }
                self.i += 1;
                Some(v)
            }
            _ if t.starts_with(|c: char| c.is_ascii_digit()) => int_literal(&t),
            _ => self.known.get(&t).copied(),
        }
    }
}

fn int_literal(t: &str) -> Option<i128> {
    let t = t.trim_end_matches(['u', 'U', 'l', 'L']);
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        i128::from_str_radix(h, 16).ok()
    } else if t.len() > 1 && t.starts_with('0') {
        i128::from_str_radix(&t[1..], 8).ok()
    } else {
        t.parse().ok()
    }
}
