//! Golden tests.
//!
//! - `tests/run/x.ovt` runs and must print `x.out`. If `x.stderr` exists, the
//!   program must trap (exit status 101) with exactly that message; `x.status`
//!   overrides the expected exit status.
//! - `tests/programs/<name>` builds and must pass `tasks/*-<name>/tests/run.py`.
//! - `tests/errors/x.ovt` must fail to build with exactly the messages in `x.err`.
//! - Every test program, and every `ovt` code block in SPEC.md, must come back
//!   unchanged from `ovt fmt`, and the program at the top of SPEC.md must pass
//!   its tests.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn ovt() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ovt"))
}

fn tests_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join(name)
}

fn ovt_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "ovt"))
        .collect();
    files.sort();
    files
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

fn name(p: &Path) -> String {
    p.file_name().unwrap().to_string_lossy().into_owned()
}

/// Formats `text` with `ovt fmt -`; `None` if it doesn't parse.
fn format(text: &str) -> Option<String> {
    let mut child = ovt().args(["fmt", "-"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    out.status.success().then(|| String::from_utf8(out.stdout).unwrap())
}

#[test]
fn programs_print_expected_output() {
    let dir = tests_dir("run");
    let mut failures = Vec::new();
    for f in ovt_files(&dir) {
        let out = ovt().current_dir(&dir).arg("run").arg(name(&f)).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stderr_file = f.with_extension("stderr");
        let (want_err, mut want_code) = if stderr_file.exists() { (read(&stderr_file), 101) } else { (String::new(), 0) };
        if let Ok(code) = read(&f.with_extension("status")).trim().parse::<i32>() {
            want_code = code;
        }
        let want_out = read(&f.with_extension("out"));
        let code = out.status.code().unwrap_or(-1);
        if stdout != want_out || stderr != want_err || code != want_code {
            failures.push(format!(
                "{}: exit {code} (want {want_code})\n--- stdout\n{stdout}--- want\n{want_out}--- stderr\n{stderr}--- want\n{want_err}",
                name(&f)
            ));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn errors_match_expected_messages() {
    let dir = tests_dir("errors");
    let mut failures = Vec::new();
    for f in ovt_files(&dir) {
        // `ex` lines are only checked by `ovt test`.
        let cmd = if name(&f).starts_with("ex_") { "test" } else { "build" };
        let out = ovt().current_dir(&dir).arg(cmd).arg(name(&f)).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        let want = read(&f.with_extension("err"));
        if out.status.code() != Some(1) || stderr != want {
            failures.push(format!("{}: exit {:?}\n--- got\n{stderr}--- want\n{want}", name(&f), out.status.code()));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn outlines_match() {
    let dir = tests_dir("outline");
    let mut failures = Vec::new();
    for f in ovt_files(&dir) {
        let out = ovt().current_dir(&dir).arg("outline").arg(name(&f)).output().unwrap();
        let got = String::from_utf8_lossy(&out.stdout);
        let want = read(&f.with_extension("outline"));
        if !out.status.success() || got != want {
            failures.push(format!("{}:\n--- got\n{got}--- want\n{want}", name(&f)));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn test_programs_are_formatted() {
    let mut failures = Vec::new();
    for dir in ["run", "errors", "outline"] {
        for f in ovt_files(&tests_dir(dir)) {
            let text = read(&f);
            // Files written to show a parse error can't be formatted.
            if let Some(formatted) = format(&text) {
                if formatted != text {
                    failures.push(format!("{dir}/{}:\n--- formatted\n{formatted}", name(&f)));
                }
            }
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn spec_code_blocks_are_canonical() {
    let spec = read(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../SPEC.md"));
    let mut blocks = Vec::new();
    let mut rest = spec.as_str();
    while let Some(start) = rest.find("```ovt\n") {
        let body = &rest[start + 7..];
        let end = body.find("```").expect("unclosed code block in SPEC.md");
        blocks.push(&body[..end]);
        rest = &body[end + 3..];
    }
    assert!(blocks.len() >= 5, "expected the SPEC.md code blocks to be tagged `ovt`");
    let mut failures = Vec::new();
    for (i, block) in blocks.iter().enumerate() {
        match format(block) {
            None => failures.push(format!("block {i} doesn't parse:\n{block}")),
            Some(f) if f != *block => failures.push(format!("block {i} isn't canonical:\n--- formatted\n{f}")),
            Some(_) => {}
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

/// The complete program at the top of SPEC.md builds and passes its test.
#[test]
fn spec_program_passes_its_tests() {
    let spec = read(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../SPEC.md"));
    let start = spec.find("```ovt\n// src/main.ovt").expect("SPEC.md starts with a complete program") + 7;
    let end = start + spec[start..].find("```").unwrap();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("spec_program");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.ovt"), &spec[start..end]).unwrap();
    let out = ovt().current_dir(&dir).arg("test").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && stdout.contains("tests passed"), "\n{stdout}{}", String::from_utf8_lossy(&out.stderr));
}

/// Tokens by `o200k_base`, a proxy for Claude's tokenizer, through `uvx` and
/// tiktoken; without them, an estimate from the length that errs high.
fn tokens(text: &str) -> (usize, &'static str) {
    let script = "import sys, tiktoken; print(len(tiktoken.get_encoding('o200k_base').encode(sys.stdin.read())))";
    let child = Command::new("uvx").args(["--with", "tiktoken", "python", "-c", script]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
    if let Ok(mut child) = child {
        let _ = child.stdin.take().unwrap().write_all(text.as_bytes());
        if let Ok(out) = child.wait_with_output() {
            if let Ok(n) = String::from_utf8_lossy(&out.stdout).trim().parse() {
                return (n, "o200k_base");
            }
        }
    }
    (text.len() * 10 / 32, "an estimate (no uvx)")
}

/// Agents get SPEC.md and the outline of every std module in their prompt
/// (bench/run.py builds it the same way), so both have a budget.
#[test]
fn docs_fit_their_budgets() {
    const SPEC_MAX: usize = 6000;
    const DOCS_MAX: usize = 12_000;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let spec = read(&root.join("SPEC.md"));
    let mut names: Vec<String> = std::fs::read_dir(root.join("std"))
        .unwrap()
        .filter_map(|e| e.ok()?.path().file_stem().map(|s| s.to_string_lossy().to_string()))
        .filter(|n| n != "prelude")
        .collect();
    names.sort();
    names.insert(0, "prelude".into());
    let mut parts = Vec::new();
    for n in &names {
        let out = ovt().args(["outline", n]).output().unwrap();
        assert!(out.status.success(), "ovt outline {n} failed");
        parts.push(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    let outline = parts.join("\n\n");
    let (spec_n, how) = tokens(&spec);
    let (outline_n, _) = tokens(&outline);
    assert!(spec_n <= SPEC_MAX, "SPEC.md is {spec_n} tokens by {how}, over its budget of {SPEC_MAX}");
    assert!(spec_n + outline_n <= DOCS_MAX, "SPEC.md and the std outline are {} tokens by {how}, over their budget of {DOCS_MAX}", spec_n + outline_n);
}

#[test]
fn reference_programs_pass_task_tests() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut failures = Vec::new();
    for (name, task) in [("wordfreq", "01-wordfreq"), ("jsonfmt", "01-jsonfmt"), ("hashdir", "02-hashdir"), ("echo", "03-echo"), ("chat", "03-chat"), ("todo", "04-todo"), ("todo_sqlite", "05-todo-sqlite")] {
        let dir = root.join("tests/programs").join(name);
        let bin = dir.join("bin").join(name);
        let out = ovt().current_dir(&dir).args(["build", "-o"]).arg(&bin).output().unwrap();
        if !out.status.success() {
            failures.push(format!("{name} doesn't build:\n{}", String::from_utf8_lossy(&out.stderr)));
            continue;
        }
        let task_dir = root.join("..").join("tasks").join(task);
        let out = Command::new("python3").current_dir(&task_dir).arg("tests/run.py").arg(&bin).output().unwrap();
        if !out.status.success() {
            failures.push(format!("{name}:\n{}", String::from_utf8_lossy(&out.stdout)));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

/// Task 05 runs task 04's tests unchanged, from its own copy.
#[test]
fn todo_sqlite_uses_the_todo_tests() {
    let tasks = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tasks");
    let a = read(&tasks.join("04-todo/tests/run.py"));
    let b = read(&tasks.join("05-todo-sqlite/tests/api.py"));
    assert!(!a.is_empty() && a == b, "tasks/05-todo-sqlite/tests/api.py must be a copy of tasks/04-todo/tests/run.py");
}

#[test]
fn std_examples_pass() {
    let out = ovt().args(["test", "--std"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && stdout.contains("tests passed"), "\n{stdout}{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn std_and_reference_programs_are_formatted() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = ovt().arg("fmt").arg("--check").arg(root.join("../std")).arg(root.join("tests/programs")).output().unwrap();
    assert!(out.status.success(), "\n{}", String::from_utf8_lossy(&out.stderr));
}
