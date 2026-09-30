//! The `ovt` command line.

use crate::check::{self, SourceFile, TestSel};
use crate::diag::Diag;
use crate::lexer::{self, Comment};
use crate::parser::{self, ParseMode};
use crate::source::Source;
use crate::{ast, cheader, codegen, fmt, stdlib};
use std::path::{Path, PathBuf};
use std::process::Command;

const USAGE: &str = "\
usage: ovt <command> [args]

  build [path] [-o out]   compile the package (or one .ovt file)
  run [path] [-- args]    compile and run
  test [path] [filter]    run `ex` lines and `test` blocks; prints only failures
  outline <name>          signatures, docs, `pre` and `ex` lines of a module, file or
                          std module (str, array, Map, int, os, fs, math, prelude)
  fmt [--check] [paths]   format files in place; `ovt fmt -` formats stdin
";

pub fn main(args: &[String]) -> i32 {
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };
    if rest.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return 0;
    }
    match args.first().map(String::as_str) {
        Some("build") => build_cmd(rest, Action::Build),
        Some("run") => build_cmd(rest, Action::Run),
        Some("test") => build_cmd(rest, Action::Test),
        Some("fmt") => fmt_cmd(rest),
        Some("outline") => outline_cmd(rest),
        Some("help" | "--help" | "-h") | None => {
            print!("{USAGE}");
            0
        }
        Some(other) => {
            eprintln!("error: unknown command `{other}`; run `ovt help` for the list");
            2
        }
    }
}

pub struct Parsed {
    pub src: Source,
    pub file: ast::File,
    pub comments: Vec<Comment>,
    pub diags: Vec<Diag>,
}

pub fn parse_source(src: Source, mode: ParseMode) -> Parsed {
    let lexed = lexer::lex(&src.text);
    let mut diags = lexed.diags;
    let (file, pdiags) = parser::parse(&src, lexed.tokens, mode);
    diags.extend(pdiags);
    Parsed { src, file, comments: lexed.comments, diags }
}

fn display_path(p: &Path) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    p.strip_prefix(&cwd).unwrap_or(p).display().to_string()
}

fn read_source(path: &Path) -> Result<Source, String> {
    std::fs::read_to_string(path)
        .map(|text| Source::new(display_path(path), text))
        .map_err(|e| format!("error: can't read {}: {e}", display_path(path)))
}

fn report(src: &Source, diags: &[Diag]) {
    for d in diags {
        eprintln!("{}", d.render(src));
    }
}

/// The nearest directory, from `start` up, containing `src/main.ovt`.
fn find_package(start: &Path) -> Option<PathBuf> {
    let start = if start.is_file() { start.parent()? } else { start };
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join("src").join("main.ovt").is_file() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

fn ovt_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if p.is_dir() {
            if !name.starts_with('.') && name != "target" {
                ovt_files(&p, out);
            }
        } else if name.ends_with(".ovt") {
            out.push(p);
        }
    }
}

/// The standard library's files, parsed. Their diagnostics are compiler bugs.
fn std_files() -> Vec<SourceFile> {
    stdlib::FILES
        .iter()
        .map(|f| {
            let parsed = parse_source(Source::new(format!("std/{}.ovt", f.name), f.text), ParseMode::Std);
            if !parsed.diags.is_empty() {
                report(&parsed.src, &parsed.diags);
                panic!("the standard library doesn't parse");
            }
            SourceFile { src: parsed.src, ast: parsed.file, module: f.module.unwrap_or(f.name).to_string(), std: true, open: f.module.is_none() }
        })
        .collect()
}

/// Loads a program: the package's `src/**/*.ovt` (or one file), plus the standard library.
fn load_program(main_file: &Path, package_src: Option<&Path>) -> Result<Vec<SourceFile>, ()> {
    let mut files = std_files();
    let mut paths = Vec::new();
    match package_src {
        Some(src) => ovt_files(src, &mut paths),
        None => paths.push(main_file.to_path_buf()),
    }
    let mut ok = true;
    for path in paths {
        let src = match read_source(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{e}");
                ok = false;
                continue;
            }
        };
        let module = match package_src {
            Some(root) => {
                let rel = path.strip_prefix(root).unwrap_or(&path).with_extension("");
                rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join(".")
            }
            None => "main".to_string(),
        };
        let parsed = parse_source(src, ParseMode::File);
        if !parsed.diags.is_empty() {
            report(&parsed.src, &parsed.diags);
            ok = false;
            continue;
        }
        let first = module.split('.').next().unwrap_or("");
        if stdlib::MODULE_NAMES.contains(&first) || stdlib::FILES.iter().any(|f| f.name == module) {
            eprintln!("{}:1:1: error: `{module}` is the name of a standard module; rename the file", parsed.src.path);
            ok = false;
            continue;
        }
        if !module.split('.').all(check::is_snake) {
            eprintln!("{}:1:1: error: module names are snake_case: rename the file", parsed.src.path);
            ok = false;
        }
        files.push(SourceFile { src: parsed.src, ast: parsed.file, module, std: false, open: false });
    }
    if ok { Ok(files) } else { Err(()) }
}

/// Adds the declarations `extern "lib" header "x.h"` blocks generate, as a
/// file of the same module (see cheader.rs).
fn import_headers(files: &mut Vec<SourceFile>, include: &Path, cache: &Path) -> Result<(), ()> {
    let mut extra = Vec::new();
    let mut ok = true;
    for f in files.iter().filter(|f| !f.std) {
        for item in &f.ast.items {
            let crate::ast::ItemKind::Extern(e) = &item.kind else { continue };
            let Some(h) = &e.header else { continue };
            let text = |s: &crate::ast::StrLit| s.parts.iter().map(|p| if let crate::ast::StrPart::Text(t) = p { t.as_str() } else { "" }).collect::<String>();
            let (header, lib) = (text(h), text(&e.lib));
            let skip: std::collections::HashSet<String> = e
                .items
                .iter()
                .flatten()
                .filter_map(|it| match &it.kind {
                    crate::ast::ItemKind::Fn(d) => Some(d.name.name.clone()),
                    crate::ast::ItemKind::Type(t) => Some(t.name.name.clone()),
                    _ => None,
                })
                .collect();
            let req = cheader::Request { header: &header, lib: &lib, blocking: e.blocking, skip: &skip, include, cache };
            match cheader::import(&req) {
                Ok(imp) => {
                    let parsed = parse_source(Source::new(display_path(&imp.path), imp.text), ParseMode::File);
                    if !parsed.diags.is_empty() {
                        report(&parsed.src, &parsed.diags);
                        eprintln!("error: the declarations generated from \"{header}\" don't parse; this is a bug in the compiler");
                        ok = false;
                        continue;
                    }
                    extra.push(SourceFile { src: parsed.src, ast: parsed.file, module: f.module.clone(), std: false, open: false });
                }
                Err(msg) => {
                    let (line, col) = f.src.line_col(h.span.lo);
                    eprintln!("{}:{line}:{col}: error: can't import \"{header}\": {msg}", f.src.path);
                    ok = false;
                }
            }
        }
    }
    files.extend(extra);
    if ok { Ok(()) } else { Err(()) }
}

// ---- fmt ----

fn fmt_cmd(args: &[String]) -> i32 {
    let check_only = args.iter().any(|a| a == "--check");
    let paths: Vec<&String> = args.iter().filter(|a| *a != "--check").collect();
    if paths.len() == 1 && paths[0] == "-" {
        let mut text = String::new();
        if std::io::Read::read_to_string(&mut std::io::stdin(), &mut text).is_err() {
            eprintln!("error: can't read stdin");
            return 2;
        }
        let parsed = parse_source(Source::new("<stdin>", text), ParseMode::Snippet);
        if !parsed.diags.is_empty() {
            report(&parsed.src, &parsed.diags);
            return 1;
        }
        print!("{}", fmt::format(&parsed.src, &parsed.file, &parsed.comments));
        return 0;
    }
    let mut files = Vec::new();
    if paths.is_empty() {
        let cwd = std::env::current_dir().unwrap_or_default();
        let root = find_package(&cwd).map(|r| r.join("src")).unwrap_or(cwd);
        ovt_files(&root, &mut files);
    }
    for p in paths {
        let p = PathBuf::from(p);
        if p.is_dir() { ovt_files(&p, &mut files) } else { files.push(p) }
    }
    let mut status = 0;
    for path in files {
        let src = match read_source(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{e}");
                status = 1;
                continue;
            }
        };
        let std = path.components().any(|c| c.as_os_str() == "std");
        let parsed = parse_source(src, if std { ParseMode::Std } else { ParseMode::File });
        if !parsed.diags.is_empty() {
            report(&parsed.src, &parsed.diags);
            status = 1;
            continue;
        }
        let out = fmt::format(&parsed.src, &parsed.file, &parsed.comments);
        if out != parsed.src.text {
            if check_only {
                eprintln!("{}: not formatted; run `ovt fmt`", parsed.src.path);
                status = 1;
            } else if let Err(e) = std::fs::write(&path, out) {
                eprintln!("error: can't write {}: {e}", parsed.src.path);
                status = 1;
            }
        }
    }
    status
}

// ---- outline ----

fn outline_cmd(args: &[String]) -> i32 {
    let Some(target) = args.first() else {
        eprintln!("error: `ovt outline` needs a module name or a file, like `ovt outline main` or `ovt outline str`");
        return 2;
    };
    let std_file = stdlib::outline_file(target);
    if std_file.is_none() && stdlib::MODULE_NAMES.contains(&target.as_str()) {
        let m = stdlib::planned_milestone(target);
        eprintln!("error: the `{target}` module isn't available in this compiler yet (planned for milestone {m})");
        return 1;
    }
    let mut module = None;
    let (src, mode) = if let Some(f) = std_file.filter(|_| !target.ends_with(".ovt")) {
        module = f.module.map(|m| m.to_string());
        (Source::new(format!("std/{}.ovt", f.name), f.text), ParseMode::Std)
    } else {
        if !target.ends_with(".ovt") && target != "main" {
            module = Some(target.clone());
        }
        let path = if target.ends_with(".ovt") || Path::new(target).is_file() {
            PathBuf::from(target)
        } else {
            let cwd = std::env::current_dir().unwrap_or_default();
            let Some(root) = find_package(&cwd) else {
                eprintln!("error: no module `{target}`: there's no package here (no src/main.ovt), and it isn't a standard module");
                return 1;
            };
            let rel: PathBuf = target.split('.').collect();
            root.join("src").join(rel).with_extension("ovt")
        };
        match read_source(&path) {
            Ok(s) => (s, ParseMode::File),
            Err(_) => {
                if stdlib::MODULE_NAMES.contains(&target.as_str()) {
                    eprintln!("error: the `{target}` module isn't available in this compiler yet");
                } else {
                    eprintln!("error: no module `{target}` (looked for {})", display_path(&path));
                }
                return 1;
            }
        }
    };
    let parsed = parse_source(src, mode);
    if !parsed.diags.is_empty() {
        report(&parsed.src, &parsed.diags);
        return 1;
    }
    print!("{}", fmt::outline(&parsed.src, &parsed.file, &parsed.comments, module.as_deref()));
    0
}

// ---- build, run and test ----

const RUNTIME_C: &str = include_str!("../../runtime/rt.c");

fn clang() -> String {
    std::env::var("OVT_CLANG").unwrap_or_else(|_| "clang".into())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Build,
    Run,
    Test,
}

fn build_cmd(args: &[String], action: Action) -> i32 {
    let (own, prog_args) = match args.iter().position(|a| a == "--") {
        Some(i) => (&args[..i], &args[i + 1..]),
        None => (args, &[][..]),
    };
    let mut output: Option<PathBuf> = None;
    let mut positional = Vec::new();
    let mut std_tests = false;
    let mut it = own.iter();
    while let Some(a) = it.next() {
        if a == "-o" {
            match it.next() {
                Some(p) => output = Some(PathBuf::from(p)),
                None => {
                    eprintln!("error: `-o` needs a path, like `ovt build -o bin/app`");
                    return 2;
                }
            }
        } else if a == "--std" && action == Action::Test {
            std_tests = true;
        } else {
            positional.push(a.clone());
        }
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    // A path argument, if it names a file or directory; for `test`, anything else filters.
    let (path_arg, filter) = match positional.first() {
        Some(p) if p.ends_with(".ovt") || Path::new(p).exists() => (Some(PathBuf::from(p)), positional.get(1).cloned()),
        Some(p) if action == Action::Test => (None, Some(p.clone())),
        Some(p) => {
            eprintln!("error: no such file or directory: {p}");
            return 2;
        }
        None => (None, None),
    };
    let files;
    let out_dir;
    let name;
    if std_tests {
        files = std_files();
        out_dir = std::env::temp_dir().join("ovt-std-tests");
        name = "std".to_string();
    } else {
        let (main_file, src_dir, dir, n) = match &path_arg {
            Some(p) if p.extension().is_some_and(|e| e == "ovt") => {
                let dir = p.parent().map(Path::to_path_buf).filter(|d| !d.as_os_str().is_empty()).unwrap_or_else(|| cwd.clone());
                let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("main").to_string();
                (p.clone(), None, dir.join(".ovt"), stem)
            }
            other => {
                let start = other.clone().unwrap_or_else(|| cwd.clone());
                let Some(root) = find_package(&start) else {
                    eprintln!("error: no package found (no src/main.ovt in {} or above); pass a .ovt file to use just that file", display_path(&start));
                    return 2;
                };
                let n = root.file_name().and_then(|s| s.to_str()).unwrap_or("main").to_string();
                (root.join("src").join("main.ovt"), Some(root.join("src")), root.join(".ovt"), n)
            }
        };
        let mut loaded = match load_program(&main_file, src_dir.as_deref()) {
            Ok(f) => f,
            Err(()) => return 1,
        };
        let include = src_dir.as_deref().and_then(Path::parent).map(Path::to_path_buf).unwrap_or_else(|| dir.parent().map(Path::to_path_buf).unwrap_or_default());
        if import_headers(&mut loaded, &include, &dir.join("build").join("headers")).is_err() {
            return 1;
        }
        files = loaded;
        out_dir = dir;
        name = n;
    }
    let sel = match action {
        Action::Test if std_tests => TestSel::Std,
        Action::Test => TestSel::User,
        _ => TestSel::None,
    };
    let program = match check::check(&files, sel) {
        Ok(p) => p,
        Err(diags) => {
            let limit = 30;
            for d in diags.iter().take(limit) {
                eprintln!("{}", d.render(&files[d.file].src));
            }
            if diags.len() > limit {
                eprintln!("... and {} more errors", diags.len() - limit);
            }
            return 1;
        }
    };
    if action == Action::Test && program.tests.is_empty() {
        println!("no tests (add `ex` lines to functions, or `test \"name\" {{ ... }}` blocks)");
        return 0;
    }
    let sources: Vec<&Source> = files.iter().map(|f| &f.src).collect();
    let mode = match action {
        Action::Test => codegen::Entry::Tests { filter: filter.clone() },
        _ => codegen::Entry::Main,
    };
    let ir = codegen::emit(&program, &sources, mode);
    let build_dir = out_dir.join("build");
    if let Err(e) = std::fs::create_dir_all(&build_dir) {
        eprintln!("error: can't create {}: {e}", display_path(&build_dir));
        return 1;
    }
    let suffix = if action == Action::Test { "-test" } else { "" };
    let ll = build_dir.join(format!("{name}{suffix}.ll"));
    let rt = build_dir.join("rt.c");
    let bin = match (&output, action) {
        (Some(p), Action::Build | Action::Run) => {
            if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
                if let Err(e) = std::fs::create_dir_all(dir) {
                    eprintln!("error: can't create {}: {e}", display_path(dir));
                    return 1;
                }
            }
            p.clone()
        }
        _ => {
            let bin_dir = out_dir.join("bin");
            if let Err(e) = std::fs::create_dir_all(&bin_dir) {
                eprintln!("error: can't create {}: {e}", display_path(&bin_dir));
                return 1;
            }
            bin_dir.join(format!("{name}{suffix}"))
        }
    };
    if let Err(e) = std::fs::write(&ll, ir).and_then(|_| std::fs::write(&rt, RUNTIME_C)) {
        eprintln!("error: can't write build files: {e}");
        return 1;
    }
    let opt = std::env::var("OVT_OPT").unwrap_or_else(|_| "-O2".into());
    // Extra clang flags, like `-fsanitize=address` when testing the compiler.
    let extra = std::env::var("OVT_CFLAGS").unwrap_or_default();
    let libs: Vec<String> = program.libs.iter().map(|l| format!("-l{l}")).collect();
    let out = Command::new(clang()).arg(&opt).args(extra.split_whitespace()).args(["-w", "-o"]).arg(&bin).arg(&ll).arg(&rt).args(&libs).output();
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            eprint!("{}", String::from_utf8_lossy(&o.stderr));
            eprintln!("error: clang failed on {}; this is a bug in the compiler, not in your program", display_path(&ll));
            return 1;
        }
        Err(e) => {
            eprintln!("error: can't run `{}`: {e}; set OVT_CLANG to a clang binary", clang());
            return 1;
        }
    }
    if action == Action::Build {
        return 0;
    }
    match Command::new(&bin).args(prog_args).status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("error: can't run {}: {e}", display_path(&bin));
            1
        }
    }
}
