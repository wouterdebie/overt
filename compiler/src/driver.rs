//! The `ovt` command line.

use crate::diag::Diag;
use crate::lexer::{self, Comment};
use crate::source::Source;
use crate::{ast, check, codegen, fmt, parser};
use std::path::{Path, PathBuf};
use std::process::Command;

const USAGE: &str = "\
usage: ovt <command> [args]

  build [path]            compile the package (or one .ovt file)
  run [path] [-- args]    compile and run
  test [filter]           run `ex` lines and `test` blocks (not yet implemented)
  outline <module|file>   signatures, docs, `pre` and `ex` lines; no bodies
  fmt [--check] [paths]   format files in place; `ovt fmt -` formats stdin
";

pub fn main(args: &[String]) -> i32 {
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };
    match args.first().map(String::as_str) {
        Some("build") => build_cmd(rest, false),
        Some("run") => build_cmd(rest, true),
        Some("fmt") => fmt_cmd(rest),
        Some("outline") => outline_cmd(rest),
        Some("test") => {
            eprintln!("error: `ovt test` isn't implemented yet (it arrives with milestone 1)");
            2
        }
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

pub fn parse_source(src: Source, allow_snippet: bool) -> Parsed {
    let lexed = lexer::lex(&src.text);
    let mut diags = lexed.diags;
    let (file, pdiags) = parser::parse(&src, lexed.tokens, allow_snippet);
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

/// Finds the package root: the nearest directory, from `start` up, containing `src/main.ovt`.
fn find_package(start: &Path) -> Option<PathBuf> {
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
        let parsed = parse_source(Source::new("<stdin>", text), true);
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
        let parsed = parse_source(src, false);
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
        eprintln!("error: `ovt outline` needs a module name or a file, like `ovt outline main`");
        return 2;
    };
    let path = if target.ends_with(".ovt") || Path::new(target).is_file() {
        PathBuf::from(target)
    } else {
        let cwd = std::env::current_dir().unwrap_or_default();
        let Some(root) = find_package(&cwd) else {
            eprintln!("error: no package here (no src/main.ovt); pass a file instead");
            return 2;
        };
        let rel: PathBuf = target.split('.').collect();
        root.join("src").join(rel).with_extension("ovt")
    };
    let src = match read_source(&path) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("error: no module `{target}` (looked for {})", display_path(&path));
            return 1;
        }
    };
    let parsed = parse_source(src, false);
    if !parsed.diags.is_empty() {
        report(&parsed.src, &parsed.diags);
        return 1;
    }
    print!("{}", fmt::outline(&parsed.src, &parsed.file, &parsed.comments));
    0
}

// ---- build and run ----

const RUNTIME_C: &str = include_str!("../../runtime/rt.c");

fn clang() -> String {
    std::env::var("OVT_CLANG").unwrap_or_else(|_| "clang".into())
}

fn build_cmd(args: &[String], run: bool) -> i32 {
    let (own, prog_args) = match args.iter().position(|a| a == "--") {
        Some(i) => (&args[..i], &args[i + 1..]),
        None => (args, &[][..]),
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let (main_file, out_dir, name) = match own.first() {
        Some(p) if p.ends_with(".ovt") => {
            let file = PathBuf::from(p);
            let dir = file.parent().map(Path::to_path_buf).unwrap_or_else(|| cwd.clone());
            let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("main").to_string();
            (file, dir.join(".ovt"), stem)
        }
        other => {
            let start = other.map(PathBuf::from).unwrap_or_else(|| cwd.clone());
            let Some(root) = find_package(&start) else {
                eprintln!("error: no package found (no src/main.ovt in {} or above); pass a .ovt file to build just that file", display_path(&start));
                return 2;
            };
            let name = root.file_name().and_then(|s| s.to_str()).unwrap_or("main").to_string();
            (root.join("src").join("main.ovt"), root.join(".ovt"), name)
        }
    };
    let src = match read_source(&main_file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let parsed = parse_source(src, false);
    if !parsed.diags.is_empty() {
        report(&parsed.src, &parsed.diags);
        return 1;
    }
    let program = match check::check(&parsed.src, &parsed.file) {
        Ok(p) => p,
        Err(diags) => {
            report(&parsed.src, &diags);
            return 1;
        }
    };
    let ir = codegen::emit(&program, &parsed.src);
    let build_dir = out_dir.join("build");
    let bin_dir = out_dir.join("bin");
    for d in [&build_dir, &bin_dir] {
        if let Err(e) = std::fs::create_dir_all(d) {
            eprintln!("error: can't create {}: {e}", display_path(d));
            return 1;
        }
    }
    let ll = build_dir.join(format!("{name}.ll"));
    let rt = build_dir.join("rt.c");
    let bin = bin_dir.join(&name);
    if let Err(e) = std::fs::write(&ll, ir).and_then(|_| std::fs::write(&rt, RUNTIME_C)) {
        eprintln!("error: can't write build files: {e}");
        return 1;
    }
    let status = Command::new(clang())
        .args(["-O2", "-w", "-o"])
        .arg(&bin)
        .arg(&ll)
        .arg(&rt)
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(_) => {
            eprintln!("error: clang failed on {}; this is a compiler bug", display_path(&ll));
            return 1;
        }
        Err(e) => {
            eprintln!("error: can't run `{}`: {e}; set OVT_CLANG to a clang binary", clang());
            return 1;
        }
    }
    if !run {
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
