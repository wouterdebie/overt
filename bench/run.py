#!/usr/bin/env python3
"""Runs a fresh agent on a task and records how it went.

Usage: bench/run.py <task> <lang> [--runs N] [--model M] [--timeout MINUTES] [--no-perf]

  task   a directory under tasks/, like 01-wordfreq
  lang   overt, rust or go

Each run gets its own directory outside the repository, holding only the
task's README.md and tests/ (plus SPEC.md and the `ovt` binary for Overt).
The agent runs headless in Claude Code's safe mode, so it sees no CLAUDE.md,
hooks, skills, plugins, MCP servers or memory, and it can't learn about
Overt from anything but SPEC.md and `ovt` itself.

Afterwards the harness builds the program itself, runs the tests and the perf
check, reads the transcript, and appends one JSON line to
bench/results/<task>.jsonl. Transcripts and the agent's files are kept under
bench/runs/<run id>/ for reading later.
"""
import argparse
import datetime
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
LANGS = {
    "overt": {
        "title": "Overt",
        "notes": (
            "Overt is a new programming language, so you won't have seen it before. "
            "SPEC.md is its complete specification; read it first. The `ovt` command is on your PATH: "
            "`ovt help` lists its commands, and `ovt outline <module>` shows the functions of a standard library module."
        ),
        "layout": "Build with `ovt build {dir} -o bin/{name}`.",
        "build": lambda work, name: [["ovt", "build", "overt", "-o", f"bin/{name}"]],
    },
    "rust": {
        "title": "Rust",
        "notes": "Use only the standard library: no crates.",
        "layout": (
            "`{dir}/` is a Cargo package whose binary is named `{name}`. Build with "
            "`cargo build --release --manifest-path {dir}/Cargo.toml` and copy `{dir}/target/release/{name}` to `bin/{name}`."
        ),
        "build": lambda work, name: [
            ["cargo", "build", "--release", "--quiet", "--manifest-path", "rust/Cargo.toml"],
            ["cp", f"rust/target/release/{name}", f"bin/{name}"],
        ],
    },
    "go": {
        "title": "Go",
        "notes": "Use only the standard library: no third-party modules.",
        "layout": "`{dir}/` is a Go module with package main. Build with `go build -C {dir} -o ../bin/{name} .`.",
        "build": lambda work, name: [["go", "build", "-C", "go", "-o", f"../bin/{name}", "."]],
    },
}

PROMPT = """Write the program described in README.md, in {title}.

- Put the source in `{dir}/`. {layout}
- It must pass `python3 tests/run.py bin/{name}`. Speed matters too: it will be timed with `python3 tests/perf.py bin/{name}`.
- {notes}
- Work on your own until the tests pass; don't ask questions. When you're done, reply with one line: DONE.
"""

BUILD_WORDS = re.compile(r"\b(ovt (build|run|test)|cargo (build|run|test|check)|go (build|run|test|vet))\b")


def sh(cmd, cwd, env=None, timeout=None):
    return subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout)


def ovt_binary():
    r = sh(["cargo", "build", "--release", "--quiet", "--manifest-path", str(REPO / "compiler/Cargo.toml")], REPO)
    if r.returncode != 0:
        sys.exit(f"building ovt failed:\n{r.stderr}")
    return REPO / "compiler/target/release/ovt"


def setup(task, lang, run_id):
    root = Path(tempfile.gettempdir()) / "overt-bench" / run_id
    work = root / "work"
    tools = root / "tools" / "bin"
    work.mkdir(parents=True)
    tools.mkdir(parents=True)
    task_dir = REPO / "tasks" / task
    shutil.copy(task_dir / "README.md", work / "README.md")
    shutil.copytree(task_dir / "tests", work / "tests", ignore=shutil.ignore_patterns("data", "__pycache__"))
    (work / "bin").mkdir()
    if lang == "overt":
        shutil.copy(REPO / "SPEC.md", work / "SPEC.md")
        shutil.copy(ovt_binary(), tools / "ovt")
    return root, work, tools


def analyze(transcript):
    """Token use, turns, tool calls and builds from a stream-json transcript."""
    stats = {"model": None, "tool_calls": 0, "builds": 0, "failed_builds": 0, "outline_calls": 0, "build_errors": []}
    pending = {}
    result = {}
    for line in transcript.splitlines():
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") == "system" and ev.get("subtype") == "init":
            stats["model"] = ev.get("model")
        elif ev.get("type") == "assistant":
            for block in ev.get("message", {}).get("content", []):
                if block.get("type") == "tool_use":
                    stats["tool_calls"] += 1
                    cmd = block.get("input", {}).get("command", "") if block.get("name") == "Bash" else ""
                    if "ovt outline" in cmd:
                        stats["outline_calls"] += 1
                    if BUILD_WORDS.search(cmd):
                        stats["builds"] += 1
                        pending[block.get("id")] = cmd
        elif ev.get("type") == "user":
            content = ev.get("message", {}).get("content", [])
            for block in content if isinstance(content, list) else []:
                if block.get("type") == "tool_result" and block.get("tool_use_id") in pending:
                    cmd = pending.pop(block["tool_use_id"])
                    if block.get("is_error"):
                        stats["failed_builds"] += 1
                        text = block.get("content")
                        if isinstance(text, list):
                            text = "\n".join(t.get("text", "") for t in text if isinstance(t, dict))
                        stats["build_errors"].append({"command": cmd, "output": str(text)[:2000]})
        elif ev.get("type") == "result":
            result = ev
    usage = result.get("usage", {})
    stats.update(
        {
            "input_tokens": usage.get("input_tokens", 0),
            "output_tokens": usage.get("output_tokens", 0),
            "cache_read_tokens": usage.get("cache_read_input_tokens", 0),
            "cache_write_tokens": usage.get("cache_creation_input_tokens", 0),
            "cost_usd": result.get("total_cost_usd"),
            "turns": result.get("num_turns"),
            "agent_seconds": round(result.get("duration_ms", 0) / 1000, 1),
            "agent_error": result.get("is_error", True) or result.get("subtype") != "success",
            "final_message": (result.get("result") or "")[-500:],
        }
    )
    return stats


def evaluate(task, lang, work, env, perf):
    name = task.split("-", 1)[1]
    out = {"built": False, "tests_passed": 0, "tests_total": None, "perf_seconds": None, "perf_correct": None}
    for cmd in LANGS[lang]["build"](work, name):
        r = sh(cmd, work, env=env, timeout=900)
        if r.returncode != 0:
            out["build_output"] = (r.stdout + r.stderr)[-2000:]
            return out
    out["built"] = True
    binary = str(work / "bin" / name)
    r = sh(["python3", "tests/run.py", binary], work, env=env, timeout=1800)
    m = re.search(r"passed (\d+) of (\d+)", r.stdout)
    if m:
        out["tests_passed"], out["tests_total"] = int(m.group(1)), int(m.group(2))
    out["test_failures"] = [l for l in r.stdout.splitlines() if l.startswith("FAIL")][:20]
    perf_script = REPO / "tasks" / task / "tests" / "perf.py"
    if perf and perf_script.exists():
        r = sh(["python3", str(perf_script), binary], work, env=env, timeout=3600)
        try:
            p = json.loads(r.stdout.strip().splitlines()[-1])
            out["perf_seconds"], out["perf_correct"] = p["seconds"], p["correct"]
        except (IndexError, ValueError, KeyError):
            out["perf_correct"] = False
    return out


def run_once(task, lang, model, timeout_min, perf, index):
    stamp = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    run_id = f"{task}-{lang}-{stamp}-{index}"
    root, work, tools = setup(task, lang, run_id)
    name = task.split("-", 1)[1]
    spec = LANGS[lang]
    prompt = PROMPT.format(
        title=spec["title"],
        dir=lang,
        name=name,
        layout=spec["layout"].format(dir=lang, name=name),
        notes=spec["notes"],
    )
    env = dict(os.environ, PATH=f"{tools}{os.pathsep}{os.environ['PATH']}")
    cmd = ["claude", "-p", prompt, "--safe-mode", "--dangerously-skip-permissions", "--output-format", "stream-json", "--verbose"]
    if model:
        cmd += ["--model", model]
    print(f"[{run_id}] agent running in {work}", flush=True)
    start = time.time()
    try:
        r = subprocess.run(cmd, cwd=work, env=env, capture_output=True, text=True, timeout=timeout_min * 60)
        transcript, timed_out = r.stdout, False
    except subprocess.TimeoutExpired as e:
        transcript, timed_out = (e.stdout or b"").decode() if isinstance(e.stdout, bytes) else (e.stdout or ""), True
    wall = round(time.time() - start, 1)
    keep = REPO / "bench" / "runs" / run_id
    keep.mkdir(parents=True)
    (keep / "transcript.jsonl").write_text(transcript)
    (keep / "prompt.txt").write_text(prompt)
    stats = analyze(transcript)
    print(f"[{run_id}] agent finished in {wall}s; building and testing", flush=True)
    results = evaluate(task, lang, work, env, perf)
    shutil.copytree(work, keep / "work", ignore=shutil.ignore_patterns("target", ".ovt", "bin", "data", "__pycache__"))
    commit = sh(["git", "rev-parse", "--short", "HEAD"], REPO).stdout.strip()
    record = {
        "run_id": run_id,
        "task": task,
        "lang": lang,
        "date": datetime.datetime.now().isoformat(timespec="seconds"),
        "repo_commit": commit,
        "timed_out": timed_out,
        "wall_seconds": wall,
        **stats,
        **results,
    }
    results_dir = REPO / "bench" / "results"
    results_dir.mkdir(exist_ok=True)
    with open(results_dir / f"{task}.jsonl", "a") as f:
        f.write(json.dumps(record) + "\n")
    shutil.rmtree(root, ignore_errors=True)
    print(
        f"[{run_id}] tests {results['tests_passed']}/{results['tests_total']}, "
        f"builds {stats['builds']} ({stats['failed_builds']} failed), "
        f"tokens in/out {stats['input_tokens'] + stats['cache_read_tokens'] + stats['cache_write_tokens']}/{stats['output_tokens']}, "
        f"perf {results['perf_seconds']}s",
        flush=True,
    )
    return record


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("task")
    ap.add_argument("lang", choices=sorted(LANGS))
    ap.add_argument("--runs", type=int, default=1)
    ap.add_argument("--model")
    ap.add_argument("--timeout", type=int, default=90, help="minutes per agent run")
    ap.add_argument("--no-perf", action="store_true")
    a = ap.parse_args()
    if not (REPO / "tasks" / a.task).is_dir():
        sys.exit(f"no task tasks/{a.task}")
    for i in range(a.runs):
        run_once(a.task, a.lang, a.model, a.timeout, not a.no_perf, i + 1)


if __name__ == "__main__":
    main()
