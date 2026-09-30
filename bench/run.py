#!/usr/bin/env python3
"""Runs fresh agents on a task and records how it went.

Usage: bench/run.py <task> <langs> [--runs N] [--jobs N] [--model M] [--timeout MINUTES] [--no-perf]

  task    a directory under tasks/, like 01-wordfreq
  langs   overt, overt-spec, rust or go, or several separated by commas: overt,rust

Each run gets its own directory outside the repository, holding only the
task's README.md and tests/ (without the speed test), plus the `ovt` binary
for Overt. The agent runs headless in Claude Code's safe mode, so it sees no
CLAUDE.md, hooks, skills, plugins, MCP servers or memory, and it can't learn
about Overt from anything but its prompt and `ovt` itself.

Overt's docs are in the prompt, the way a CLAUDE.md would load them, since
the model already knows Rust and Go. `overt` gets SPEC.md and the outline of
every standard library module; `overt-spec` gets only SPEC.md and looks the
standard library up with `ovt outline`.

The prompt asks for a program that passes the tests, with no pressure on
speed. The harness then builds the program itself, runs the tests, reads the
transcript (including the numbers at the first full test pass), and, once all
agents are done, times each program one at a time with the task's
tests/perf.py. Each run appends one JSON line to bench/results/<task>.jsonl.
Transcripts, the agent's files and the built program are kept under
bench/runs/<run id>/.
"""
import argparse
import concurrent.futures
import datetime
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
# Bump when the prompt or the measurement changes, so results stay comparable.
PROTOCOL = "v4: spec and std outline in the prompt"

OVERT_BUILD = {
    "title": "Overt",
    "dir": "overt",
    "ovt": True,
    "layout": "Build with `ovt build {dir} -o bin/{name}`.",
    "build": lambda name: [["ovt", "build", "overt", "-o", f"bin/{name}"]],
}

LANGS = {
    "overt-spec": {
        **OVERT_BUILD,
        "docs": ["spec"],
        "notes": (
            "Overt is a new programming language, so you won't have seen it before. "
            "Its complete specification is below. The `ovt` command is on your PATH: "
            "`ovt help` lists its commands, and `ovt outline <module>` shows the functions of a standard library module."
        ),
    },
    "overt": {
        **OVERT_BUILD,
        "docs": ["spec", "std"],
        "notes": (
            "Overt is a new programming language, so you won't have seen it before. "
            "Its complete specification and its standard library, as `ovt outline` prints it, are below. "
            "The `ovt` command is on your PATH: `ovt help` lists its commands."
        ),
    },
    "rust": {
        "title": "Rust",
        "dir": "rust",
        "notes": "Use only the standard library: no crates.",
        "layout": (
            "`{dir}/` is a Cargo package whose binary is named `{name}`. Build with "
            "`cargo build --release --manifest-path {dir}/Cargo.toml` and copy `{dir}/target/release/{name}` to `bin/{name}`."
        ),
        "build": lambda name: [
            ["cargo", "build", "--release", "--quiet", "--manifest-path", "rust/Cargo.toml"],
            ["cp", f"rust/target/release/{name}", f"bin/{name}"],
        ],
    },
    "go": {
        "title": "Go",
        "dir": "go",
        "notes": "Use only the standard library: no third-party modules.",
        "layout": "`{dir}/` is a Go module with package main. Build with `go build -C {dir} -o ../bin/{name} .`.",
        "build": lambda name: [["go", "build", "-C", "go", "-o", f"../bin/{name}", "."]],
    },
}

PROMPT = """Write the program described in README.md, in {title}.

- Put the source in `{dir}/`. {layout}
- It must pass `python3 tests/run.py bin/{name}`.
- {notes}
- Work on your own until the tests pass; don't ask questions. When you're done, reply with one line: DONE.
{docs}"""

DOC_TAGS = {"spec": "spec", "std": "std-outline"}

BUILD_WORDS = re.compile(r"\b(ovt (build|run|test)|cargo (build|run|test|check)|go (build|run|test|vet))\b")
PASSED = re.compile(r"passed (\d+) of (\d+)")
# A tool call that only reads Overt's docs, for counting what learning Overt costs.
DOC_CMD = re.compile(r"\bovt (outline|help)\b|SPEC\.md")
NOT_DOC = re.compile(r"README|tests/|\bbin/|overt/|\.ovt\b")


def sh(cmd, cwd, env=None, timeout=None):
    return subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout)


def ovt_binary():
    r = sh(["cargo", "build", "--release", "--quiet", "--manifest-path", str(REPO / "compiler/Cargo.toml")], REPO)
    if r.returncode != 0:
        sys.exit(f"building ovt failed:\n{r.stderr}")
    return REPO / "compiler/target/release/ovt"


def std_outline(ovt):
    """`ovt outline` of every std module, prelude first."""
    names = ["prelude"] + sorted(p.stem for p in (REPO / "std").glob("*.ovt") if p.stem != "prelude")
    parts = []
    for n in names:
        r = sh([str(ovt), "outline", n], REPO)
        if r.returncode != 0:
            sys.exit(f"ovt outline {n} failed:\n{r.stderr}")
        parts.append(r.stdout.strip())
    return "\n\n".join(parts)


def doc_texts(langs, ovt):
    """The docs the chosen languages put in their prompts, by name."""
    wanted = {d for lang in langs for d in LANGS[lang].get("docs", [])}
    texts = {}
    if "spec" in wanted:
        texts["spec"] = (REPO / "SPEC.md").read_text().strip()
    if "std" in wanted:
        texts["std"] = std_outline(ovt)
    return texts


def is_doc_lookup(block):
    """Whether a tool call only reads Overt's docs: SPEC.md, `ovt help` or `ovt outline`."""
    inp = block.get("input", {})
    if block.get("name") in ("Read", "Grep"):
        return str(inp.get("file_path") or inp.get("path") or "").endswith("SPEC.md")
    cmd = inp.get("command", "") if block.get("name") == "Bash" else ""
    return bool(DOC_CMD.search(cmd)) and not NOT_DOC.search(cmd)


def setup(task, lang, run_id, ovt):
    root = Path(tempfile.gettempdir()) / "overt-bench" / run_id
    work = root / "work"
    tools = root / "tools" / "bin"
    work.mkdir(parents=True)
    tools.mkdir(parents=True)
    task_dir = REPO / "tasks" / task
    shutil.copy(task_dir / "README.md", work / "README.md")
    # The speed test stays with the harness, so agents aren't pushed to optimize.
    shutil.copytree(task_dir / "tests", work / "tests", ignore=shutil.ignore_patterns("data", "__pycache__", "perf.py"))
    (work / "bin").mkdir()
    if LANGS[lang].get("ovt"):
        shutil.copy(ovt, tools / "ovt")
    return root, work, tools


def result_text(block):
    text = block.get("content")
    if isinstance(text, list):
        text = "\n".join(t.get("text", "") for t in text if isinstance(t, dict))
    return str(text)


def analyze(transcript, total_tests):
    """Token use, turns, tool calls and builds from a stream-json transcript,
    for the whole run and at the first time every test passed.

    Also the context size at each API call, and an estimate of the tokens of
    docs the agent looked up: each call's growth in context is split over the
    text that came in, by characters. `doc_lookup_carried` counts each looked-up
    token once for every later call that carried it."""
    stats = {"model": None, "tool_calls": 0, "builds": 0, "failed_builds": 0, "outline_calls": 0, "build_errors": [], "first_pass": None}
    pending = {}
    result = {}
    seen_msgs = set()
    context = 0
    steps = []  # per API call: its context size, and the characters and doc characters added after it
    for line in transcript.splitlines():
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") == "system" and ev.get("subtype") == "init":
            stats["model"] = ev.get("model")
        elif ev.get("type") == "assistant":
            msg = ev.get("message", {})
            if msg.get("id") not in seen_msgs:
                seen_msgs.add(msg.get("id"))
                u = msg.get("usage", {})
                size = u.get("input_tokens", 0) + u.get("cache_read_input_tokens", 0) + u.get("cache_creation_input_tokens", 0)
                context += size
                steps.append({"context": size, "chars": 0, "docs": 0})
            for block in msg.get("content", []):
                if steps:
                    steps[-1]["chars"] += len(block.get("text") or block.get("thinking") or "") + len(json.dumps(block.get("input", "")))
                if block.get("type") == "tool_use":
                    stats["tool_calls"] += 1
                    cmd = block.get("input", {}).get("command", "") if block.get("name") == "Bash" else ""
                    if "ovt outline" in cmd:
                        stats["outline_calls"] += 1
                    is_build = bool(BUILD_WORDS.search(cmd))
                    if is_build:
                        stats["builds"] += 1
                    pending[block.get("id")] = (cmd, is_build, is_doc_lookup(block))
        elif ev.get("type") == "user":
            content = ev.get("message", {}).get("content", [])
            for block in content if isinstance(content, list) else []:
                if block.get("type") != "tool_result" or block.get("tool_use_id") not in pending:
                    continue
                cmd, is_build, is_doc = pending.pop(block["tool_use_id"])
                text = result_text(block)
                if steps:
                    steps[-1]["chars"] += len(text)
                    if is_doc:
                        steps[-1]["docs"] += len(text)
                if is_build and block.get("is_error"):
                    stats["failed_builds"] += 1
                    stats["build_errors"].append({"command": cmd, "output": text[:2000]})
                m = PASSED.search(text)
                if m and stats["first_pass"] is None and int(m.group(1)) == int(m.group(2)) == (total_tests or int(m.group(2))):
                    stats["first_pass"] = {
                        "tool_calls": stats["tool_calls"],
                        "input_tokens": context,
                        "builds": stats["builds"],
                        "failed_builds": stats["failed_builds"],
                    }
        elif ev.get("type") == "result":
            result = ev
    lookup = carried = 0.0
    for i, step in enumerate(steps[:-1]):
        if step["docs"] and step["chars"]:
            tokens = (steps[i + 1]["context"] - step["context"]) * step["docs"] / step["chars"]
            lookup += tokens
            carried += tokens * (len(steps) - 1 - i)
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
            "contexts": [s["context"] for s in steps],
            "doc_lookup_tokens": round(lookup),
            "doc_lookup_carried": round(carried),
        }
    )
    return stats


def build_and_test(lang, name, work, env):
    out = {"built": False, "tests_passed": 0, "tests_total": None}
    for cmd in LANGS[lang]["build"](name):
        r = sh(cmd, work, env=env, timeout=900)
        if r.returncode != 0:
            out["build_output"] = (r.stdout + r.stderr)[-2000:]
            return out
    out["built"] = True
    r = sh(["python3", "tests/run.py", str(work / "bin" / name)], work, env=env, timeout=1800)
    m = PASSED.search(r.stdout)
    if m:
        out["tests_passed"], out["tests_total"] = int(m.group(1)), int(m.group(2))
    out["test_failures"] = [l for l in r.stdout.splitlines() if l.startswith("FAIL")][:20]
    return out


def agent_run(task, lang, model, timeout_min, ovt, docs):
    """Runs one agent and tests its program. Returns the record (without speed)."""
    stamp = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    run_id = f"{task}-{lang}-{stamp}-{uuid.uuid4().hex[:6]}"
    root, work, tools = setup(task, lang, run_id, ovt)
    name = task.split("-", 1)[1]
    spec = LANGS[lang]
    doc_part = "".join(f"\n<{DOC_TAGS[d]}>\n{docs[d]}\n</{DOC_TAGS[d]}>\n" for d in spec.get("docs", []))
    prompt = PROMPT.format(
        title=spec["title"], dir=spec["dir"], name=name, layout=spec["layout"].format(dir=spec["dir"], name=name), notes=spec["notes"], docs=doc_part
    )
    env = dict(os.environ, PATH=f"{tools}{os.pathsep}{os.environ['PATH']}")
    cmd = ["claude", "-p", prompt, "--safe-mode", "--dangerously-skip-permissions", "--output-format", "stream-json", "--verbose"]
    if model:
        cmd += ["--model", model]
    print(f"[{run_id}] agent started", flush=True)
    start = time.time()
    try:
        r = subprocess.run(cmd, cwd=work, env=env, capture_output=True, text=True, timeout=timeout_min * 60)
        transcript, timed_out = r.stdout, False
    except subprocess.TimeoutExpired as e:
        out = e.stdout or ""
        transcript, timed_out = (out.decode() if isinstance(out, bytes) else out), True
    wall = round(time.time() - start, 1)
    keep = REPO / "bench" / "runs" / run_id
    keep.mkdir(parents=True)
    (keep / "transcript.jsonl").write_text(transcript)
    (keep / "prompt.txt").write_text(prompt)
    results = build_and_test(lang, name, work, env)
    stats = analyze(transcript, results.get("tests_total"))
    shutil.copytree(work, keep / "work", ignore=shutil.ignore_patterns("target", ".ovt", "bin", "data", "__pycache__"))
    binary = None
    if results["built"]:
        (keep / "bin").mkdir()
        binary = keep / "bin" / name
        shutil.copy(work / "bin" / name, binary)
    shutil.rmtree(root, ignore_errors=True)
    record = {
        "run_id": run_id,
        "task": task,
        "lang": lang,
        "protocol": PROTOCOL,
        "docs_in_prompt": spec.get("docs", []),
        "date": datetime.datetime.now().isoformat(timespec="seconds"),
        "repo_commit": sh(["git", "rev-parse", "--short", "HEAD"], REPO).stdout.strip(),
        # Uncommitted changes to the compiler, runtime, std or docs.
        "repo_dirty": bool(sh(["git", "status", "--porcelain", "--", "compiler", "runtime", "std", "SPEC.md"], REPO).stdout.strip()),
        "timed_out": timed_out,
        "wall_seconds": wall,
        **stats,
        **results,
        "perf_seconds": None,
        "perf_cpu_seconds": None,
        "perf_correct": None,
    }
    fp = stats["first_pass"] or {}
    print(
        f"[{run_id}] done in {wall}s: tests {results['tests_passed']}/{results['tests_total']}, "
        f"builds {stats['builds']} ({stats['failed_builds']} failed), turns {stats['turns']}, "
        f"${stats['cost_usd']}, first pass at tool call {fp.get('tool_calls')}",
        flush=True,
    )
    return record, binary


def time_program(task, binary):
    """(wall seconds, CPU seconds if the speed test reports them, correct)."""
    perf = REPO / "tasks" / task / "tests" / "perf.py"
    if not binary or not perf.exists():
        return None, None, None
    r = sh(["python3", str(perf), str(binary)], REPO / "tasks" / task, timeout=3600)
    try:
        p = json.loads(r.stdout.strip().splitlines()[-1])
        return p["seconds"], p.get("cpu_seconds"), p["correct"]
    except (IndexError, ValueError, KeyError):
        return None, None, False


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("task")
    ap.add_argument("langs")
    ap.add_argument("--runs", type=int, default=1, help="runs per language")
    ap.add_argument("--jobs", type=int, default=1, help="agents running at the same time")
    ap.add_argument("--model")
    ap.add_argument("--timeout", type=int, default=60, help="minutes per agent run")
    ap.add_argument("--no-perf", action="store_true")
    a = ap.parse_args()
    if not (REPO / "tasks" / a.task).is_dir():
        sys.exit(f"no task tasks/{a.task}")
    langs = a.langs.split(",")
    for lang in langs:
        if lang not in LANGS:
            sys.exit(f"unknown language {lang}; use {', '.join(LANGS)}")
    ovt = ovt_binary() if any(LANGS[lang].get("ovt") for lang in langs) else None
    docs = doc_texts(langs, ovt)
    jobs = [lang for lang in langs for _ in range(a.runs)]
    with concurrent.futures.ThreadPoolExecutor(max_workers=a.jobs) as pool:
        done = list(pool.map(lambda lang: agent_run(a.task, lang, a.model, a.timeout, ovt, docs), jobs))
    # Timing runs one program at a time, after the agents, so nothing else competes for the CPU.
    results_dir = REPO / "bench" / "results"
    results_dir.mkdir(exist_ok=True)
    for record, binary in done:
        if not a.no_perf and record.get("tests_passed") and record["tests_passed"] == record.get("tests_total"):
            record["perf_seconds"], record["perf_cpu_seconds"], record["perf_correct"] = time_program(a.task, binary)
            print(f"[{record['run_id']}] perf {record['perf_seconds']}s (correct: {record['perf_correct']})", flush=True)
        with open(results_dir / f"{a.task}.jsonl", "a") as f:
            f.write(json.dumps(record) + "\n")


if __name__ == "__main__":
    main()
