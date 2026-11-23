#!/usr/bin/env python3
"""Run Node.js's own test files against pir's Node surface.

This is deliberately *not* a cargo test.  It is a standalone harness that

  1. copies a Node source checkout's ``test/{common,fixtures,parallel}`` into a
     ``test``-shaped work root,
  2. applies the vendored ``common/index.js`` patch (``process.config``) and
     ensures the copied ``test`` tree has a package boundary,
  3. runs every requested test in its own ``pir js`` process with a per-test
     timeout, and
  4. writes ``results.jsonl``, one log per test, and ``summary.json``.

Every test gets its own process (Node's ``common.mustCall`` accounting runs in
an ``exit`` handler; sharing a process would turn missing calls into false
PASSes), its own ``TEST_SERIAL_ID``/``TEST_THREAD_ID``, its own
``NODE_TEST_DIR`` and its own ``PIR_AGENT_DIR``.

Usage
-----
    python tools/node-suite/runner.py \
        --pir C:/wp/empty/_pir_node_wt/target/debug/pir.exe \
        --pir-root C:/wp/empty/_pir_node_wt \
        --corpus C:/wp/empty/_scratch/node-suite-probe/node \
        --work  C:/wp/empty/_scratch/ns-agent/work \
        --out   C:/wp/empty/_scratch/ns-agent/runs/current \
        --list  tools/node-suite/baseline/tier-b-api.txt \
        --timeout 8 --jobs 5

    python tools/node-suite/runner.py ... --prefix test-fs- --prefix test-zlib-

See README.md for the environment model and the known deviations.
"""

from __future__ import annotations

import argparse
import hashlib
import itertools
import json
import os
import queue
import re
import shutil
import subprocess
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime, timezone
from pathlib import Path
from typing import NamedTuple

HERE = Path(__file__).resolve().parent
DEFAULT_PATCH = HERE / "overlay" / "common-index-process-config.patch"
# Tests whose wall time is driven by data size / event-loop turns rather than
# an API hang. Node's own harness default is 120 s; the global --timeout stays
# low so real hangs are cheap to detect. Measurements:
# _scratch/agent-reports/iter2-timeouts.md.
TIMEOUT_OVERRIDES = {
    # 3 x 1000 serialized setImmediate turns (~10 ms/hop on this host).
    "test-stream-pipe-manual-resume.js": 30.0,
    # depth-Infinity inspect render of ~1e6 nodes; no assertion at the end.
    "test-util-inspect-long-running.js": 60.0,
}
SHIM_ENV = {"PIR_EXT_BYTECODE": "0", "PIR_EXT_SHIM_SRC": "1"}
DISABLED_SWITCH_VALUES = ("0", "false", "off")

# Test-name prefix -> module family used in the per-module summary.
MODULE_PREFIXES = (
    ("test-string-decoder-", "string_decoder"),
    ("test-querystring-", "querystring"),
    ("test-buffer-", "buffer"),
    ("test-stream-", "stream"),
    ("test-assert-", "assert"),
    ("test-events-", "events"),
    ("test-path-", "path"),
    ("test-zlib-", "zlib"),
    ("test-util-", "util"),
    ("test-url-", "url"),
    ("test-fs-", "fs"),
)

TAP_POINT_RE = re.compile(r"^[ \t]*(ok|not ok)[ \t]+[0-9]+(?=[ \t]|$)(.*)$")
TAP_SKIP_PLAN_RE = re.compile(r"^1\.\.0[ \t]+(.*)$")
TAP_DIRECTIVE_RE = re.compile(
    r"(?:^|[ \t])#[ \t]*(skip(?:ped)?|todo)(?=[ \t:]|$):?[ \t]*(.*)$", re.I
)
UNSUPPORTED_RE = re.compile(
    r"\bERR_NOT_SUPPORTED\b|not supported by the pir extension runtime|no test-runner host",
    re.I,
)
ASSERT_RE = re.compile(r"\b(?:AssertionError|ERR_ASSERTION)\b")
ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")

RESULT_ORDER = ("PASS", "ASSERT", "ERROR", "UNSUPPORTED", "TIMEOUT", "SKIP")


def eprint(*args: object) -> None:
    print(*args, file=sys.stderr, flush=True)


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def iso_mtime(path: Path) -> str:
    return datetime.fromtimestamp(path.stat().st_mtime).astimezone().isoformat()


def git_output(corpus: Path, *args: str) -> str | None:
    try:
        out = subprocess.run(
            ["git", "-C", str(corpus), *args],
            check=True,
            capture_output=True,
            text=True,
            timeout=20,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return out.stdout.strip() or None


def suite_metadata(corpus: Path) -> dict:
    test_dir = corpus / "test"
    if not test_dir.is_dir():
        raise SystemExit(f"corpus has no test/ directory: {corpus}")
    tag = git_output(corpus, "describe", "--tags", "--always")
    commit = git_output(corpus, "rev-parse", "HEAD")
    return {
        "corpus": str(corpus.resolve()),
        "tag": tag,
        "commit": commit,
    }


def shim_mode(env: dict[str, str]) -> tuple[str, bool]:
    """Describe the configured lookup order, not which assets pir loaded."""
    source = env["PIR_EXT_SHIM_SRC"] not in DISABLED_SWITCH_VALUES
    bytecode = env["PIR_EXT_BYTECODE"] not in DISABLED_SWITCH_VALUES
    if bytecode:
        return ("embedded bytecode with source override on fallback" if source else "embedded bytecode", source)
    return ("source (PIR_EXT_SHIM_SRC)" if source else "embedded source", source)


def shim_tree_metadata(pir_root: Path) -> dict | None:
    """Sample the live shim source tree from pir's compile-time checkout."""
    shim_dir = pir_root / "src" / "extensions" / "api" / "js"
    digest = hashlib.sha256()
    newest = 0.0
    count = 0
    try:
        for path in sorted(shim_dir.glob("*.txt")):
            digest.update(path.name.encode("utf-8"))
            digest.update(path.read_bytes())
            newest = max(newest, path.stat().st_mtime)
            count += 1
    except OSError:
        return None
    if not count:
        return None
    return {
        "dir": str(shim_dir),
        "sha256": digest.hexdigest(),
        "files": count,
        "mtime": datetime.fromtimestamp(newest).astimezone().isoformat(),
    }


def module_of(name: str) -> str:
    for prefix, family in MODULE_PREFIXES:
        if name.startswith(prefix):
            return family
    return "other"


def normalize_test_name(raw: str) -> str:
    name = raw.strip().replace("\\", "/")
    if not name or name.startswith("#"):
        return ""
    for prefix in ("test/parallel/", "parallel/", "./parallel/"):
        if name.startswith(prefix):
            name = name[len(prefix):]
    return name


def collect_tests(corpus: Path, list_file: Path | None, prefixes: list[str]) -> list[str]:
    parallel = corpus / "test" / "parallel"
    names: list[str] = []
    if list_file is not None:
        for raw in list_file.read_text(encoding="utf-8").splitlines():
            name = normalize_test_name(raw)
            if name:
                names.append(name)
    if prefixes:
        available = sorted(p.name for p in parallel.glob("*.js"))
        for prefix in prefixes:
            names.extend(n for n in available if n.startswith(prefix))
    if not names:
        raise SystemExit("no tests selected: pass --list and/or --prefix")
    # De-duplicate while preserving deterministic (sorted) order.
    names = sorted(set(names))
    missing = [n for n in names if not (parallel / n).is_file()]
    if missing:
        preview = ", ".join(missing[:10])
        raise SystemExit(f"{len(missing)} selected test(s) not found in {parallel}: {preview}")
    return names


def copy_tree(src: Path, dst: Path) -> None:
    if not src.is_dir():
        raise SystemExit(f"missing corpus directory: {src}")
    shutil.copytree(src, dst, dirs_exist_ok=True, copy_function=shutil.copy2)


def prepare_work_root(corpus: Path, work: Path, patch: Path, *, skip_copy: bool) -> None:
    """Copy test/{common,fixtures,parallel} into <work>/test/ and patch common.

    Keeping the repo shape (``<work>/test/parallel``) matters: a few tests
    require ``../../test/common/...`` or the literal ``test/fixtures`` path.
    """
    test_dir = corpus / "test"
    target_test = work / "test"
    if target_test.resolve() == test_dir.resolve():
        raise SystemExit("--work must be separate from the Node corpus")
    if not skip_copy:
        for sub in ("common", "fixtures", "parallel"):
            eprint(f"preparing test/{sub}/ ...")
            copy_tree(test_dir / sub, target_test / sub)
    for sub in ("common", "fixtures", "parallel"):
        if not (target_test / sub).is_dir():
            raise SystemExit(f"work root is missing test/{sub}/: {work}")
    # The CLI grants local imports from the entry's package root, not cwd.
    # A boundary here lets parallel/*.mjs import ../common without widening
    # the grant beyond the copied test tree. Preserve any existing manifest.
    manifest = target_test / "package.json"
    if not manifest.exists():
        upstream_manifest = test_dir / "package.json"
        if upstream_manifest.is_file():
            shutil.copy2(upstream_manifest, manifest)
        else:
            manifest.write_text('{"type":"commonjs"}\n', encoding="utf-8")
    apply_common_patch(target_test / "common" / "index.js", patch)


def apply_common_patch(target: Path, patch: Path) -> None:
    """Insert the patch's added lines after common/index.js's process line.

    The diff is the source of truth; we parse its ``+`` lines rather than
    keeping a second copy of the snippet.  Re-applying is a no-op.
    """
    text = target.read_text(encoding="utf-8")
    marker = "pir compatibility shim: Node always exposes process.config.variables."
    if marker in text:
        return
    if not patch.is_file():
        raise SystemExit(f"missing overlay patch: {patch}")
    patch_text = patch.read_text(encoding="utf-8")
    added = [
        line[1:]
        for line in patch_text.splitlines()
        if line.startswith("+") and not line.startswith("+++")
    ]
    if not added:
        raise SystemExit(f"overlay patch has no added lines: {patch}")
    anchor = "const process = globalThis.process;"
    anchor_at = text.find(anchor)
    if anchor_at < 0:
        raise SystemExit(
            f"cannot apply common/index.js patch: anchor not found in {target}"
        )
    line_end = text.find("\n", anchor_at)
    if line_end < 0:
        line_end = len(text)
    insertion = "\n".join(added) + "\n"
    target.write_text(text[: line_end + 1] + insertion + text[line_end + 1:], encoding="utf-8")


class TapSignals(NamedTuple):
    failed: bool
    all_skipped: bool
    skip_reason: str
    failure_text: str


def tap_signals(output: str) -> TapSignals:
    """Read Node/pir's numbered TAP points, not comments or YAML diagnostics."""
    failed = skipped = False
    non_skipped = 0
    reason = ""
    failure_lines = []
    diagnostic_indent = None
    point_indent = None
    point_failed = point_passed = False
    for line in ANSI_RE.sub("", output).splitlines():
        stripped = line.lstrip(" \t")
        indent = len(line) - len(stripped)
        # TAP result lines can be indented for subtests, but YAML scalars can
        # also contain text that looks like a result. Keep the latter out of
        # verdicts, and use their error hints only for actual failed points.
        if diagnostic_indent is not None:
            if indent == diagnostic_indent and stripped.rstrip() == "...":
                diagnostic_indent = None
                continue
            if not stripped or indent >= diagnostic_indent:
                # A successful describe/suite wrapper is not an executed test.
                # Its child points determine whether anything ran.
                if point_passed and indent == diagnostic_indent and stripped.rstrip() in (
                    "type: 'suite'", 'type: "suite"', "type: suite",
                ):
                    non_skipped -= 1
                    point_passed = False
                if point_failed:
                    failure_lines.append(line)
                continue
            diagnostic_indent = None
        if point_indent is not None and indent > point_indent and stripped.rstrip() == "---":
            diagnostic_indent = indent
            point_indent = None
            continue
        point_indent = None
        point = TAP_POINT_RE.fullmatch(line)
        if point:
            status, tail = point.groups()
            directive = TAP_DIRECTIVE_RE.search(tail)
            kind = directive.group(1).lower() if directive else ""
            is_skip = kind in ("skip", "skipped")
            point_failed = status == "not ok" and not is_skip and kind != "todo"
            point_indent = indent
            point_passed = status == "ok" and not is_skip and kind != "todo"
            failed |= point_failed
            non_skipped += not is_skip
        else:
            plan = TAP_SKIP_PLAN_RE.fullmatch(line)
            directive = TAP_DIRECTIVE_RE.match(plan.group(1)) if plan else None
            is_skip = bool(directive and directive.group(1).lower() in ("skip", "skipped"))
        if is_skip:
            skipped = True
            reason = reason or directive.group(2).strip()
        elif (point is None and not stripped.startswith("#")) or (point and point_failed):
            failure_lines.append(line)
    return TapSignals(failed, skipped and not non_skipped, reason[:300], "\n".join(failure_lines))


def classify(output: str, exit_code: int | None, timed_out: bool) -> str:
    if timed_out:
        return "TIMEOUT"
    tap = tap_signals(output)
    if exit_code == 0 and not tap.failed:
        return "SKIP" if tap.all_skipped else "PASS"
    if UNSUPPORTED_RE.search(tap.failure_text):
        return "UNSUPPORTED"
    if ASSERT_RE.search(tap.failure_text):
        return "ASSERT"
    return "ERROR"


TMP_PATH_RE = re.compile(r"<out>[\\/]testtmp[\\/][^\s'\"()]*")
LONG_NUMBER_RE = re.compile(r"\d{4,}")
ERROR_HINT_RE = re.compile(
    r"AssertionError|\b(?:Type|Range|Reference|Syntax|URI|Eval)?Error\b"
    r"|not a function|Missing expected exception|Expected values"
    r"|Mismatched|EPERM|ERR_NOT_SUPPORTED|ERR_ASSERTION|not supported by the pir"
    r"|Unhandled promise rejection"
)


def first_line(output: str, work: Path, out: Path, exit_code: int | None) -> str:
    text = tap_signals(output).failure_text.strip("\n") or ANSI_RE.sub("", output)
    # Strip run-specific absolute paths so the baseline is reproducible.
    for variant in (str(out), str(out).replace("\\", "/"), out.as_posix()):
        text = text.replace(variant + "/", "<out>/").replace(variant + "\\", "<out>\\")
    text = TMP_PATH_RE.sub("<testtmp>", text)
    # Strip the work root, leaving test/parallel/<name> style paths.
    for variant in (str(work), str(work).replace("\\", "/"), work.as_posix()):
        text = text.replace(variant + "/", "").replace(variant + "\\", "")
    lines = [LONG_NUMBER_RE.sub("<n>", line.strip()) for line in text.splitlines()]
    lines = [line for line in lines if line]
    # Prefer the first line that actually carries an error signal; some tests
    # print progress ("Validating readFile ...", "first cb") before failing.
    for line in lines[:40]:
        if ERROR_HINT_RE.search(line):
            return line[:500]
    for line in lines:
        if line.startswith("not ok ") and TAP_POINT_RE.fullmatch(line):
            return line[:500]
    if lines:
        return lines[0][:500]
    if exit_code is None:
        return "killed before producing output"
    return f"exit code {exit_code} with no output"


def skip_reason(output: str) -> str:
    return tap_signals(output).skip_reason


def kill_tree(proc: subprocess.Popen) -> None:
    """Kill the whole process tree; pir tests can spawn children."""
    if os.name == "nt":
        subprocess.run(
            ["taskkill", "/F", "/T", "/PID", str(proc.pid)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
    else:
        try:
            os.killpg(os.getpgid(proc.pid), 9)
        except OSError:
            proc.kill()


def child_env(overrides: dict[str, str]) -> dict[str, str]:
    return {
        **{
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("PIR_", "TEST_"))
            and key not in ("NODE_TEST_DIR", "NODE_TEST_KNOWN_GLOBALS", "NODE_COMMON_PORT", "NODE_OPTIONS")
        },
        **overrides,
    }


def run_one(
    pir: Path,
    work: Path,
    out: Path,
    name: str,
    *,
    timeout: float,
    slot: int,
    test_id: int,
    test_tmp: Path,
    test_agent: Path,
    run_env: dict[str, str],
) -> dict:
    env = {
        "PIR_AGENT_DIR": str(test_agent),
        "NODE_TEST_KNOWN_GLOBALS": "0",
        "TEST_SERIAL_ID": str(test_id),
        "TEST_THREAD_ID": str(test_id),
        "NODE_TEST_DIR": str(test_tmp),
        "NODE_COMMON_PORT": str(12346 + slot),
        **run_env,
    }

    cmd = [str(pir), "js", f"test/parallel/{name}"]
    started = time.monotonic()
    timed_out = False
    try:
        proc = subprocess.Popen(
            cmd,
            cwd=str(work),
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            start_new_session=(os.name != "nt"),
        )
    except OSError as exc:
        return {
            "test": name,
            "result": "ERROR",
            "exit_code": None,
            "duration_ms": int((time.monotonic() - started) * 1000),
            "first_error": f"could not start pir: {exc}",
            "skip_reason": "",
            "output": "",
        }

    try:
        raw, _ = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        kill_tree(proc)
        try:
            raw, _ = proc.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            raw = b""
    duration_ms = int((time.monotonic() - started) * 1000)
    output = raw.decode("utf-8", errors="replace")
    exit_code = proc.returncode
    result = classify(output, exit_code, timed_out)
    return {
        "test": name,
        "result": result,
        "exit_code": exit_code,
        "duration_ms": duration_ms,
        "first_error": "" if result == "PASS" else first_line(output, work, out, exit_code),
        "skip_reason": skip_reason(output) if result == "SKIP" else "",
        "output": output,
    }


def write_json(path: Path, obj: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(obj, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    os.replace(tmp, path)


def percentile(sorted_values: list[int], q: float) -> int:
    if not sorted_values:
        return 0
    index = min(len(sorted_values) - 1, max(0, round((len(sorted_values) - 1) * q)))
    return sorted_values[index]


def build_summary(
    records: list[dict],
    *,
    pir: Path,
    pir_version: str | None,
    suite: dict,
    selection: dict,
    timeout: float,
    jobs: int,
    env_overrides: dict[str, str],
    configured_shim_mode: str,
    elapsed_s: float,
    shims: dict | None,
) -> dict:
    counts = {key: 0 for key in RESULT_ORDER}
    by_module: dict[str, dict[str, int]] = {}
    for record in records:
        counts[record["result"]] += 1
        module = record["module"]
        bucket = by_module.setdefault(
            module, {"total": 0, **{key: 0 for key in RESULT_ORDER}}
        )
        bucket["total"] += 1
        bucket[record["result"]] += 1
    total = len(records)
    durations = sorted(record["duration_ms"] for record in records)
    meaningful = total - counts["SKIP"] - counts["TIMEOUT"]
    top_failures = []
    for record in sorted(
        (r for r in records if r["result"] not in ("PASS", "SKIP")),
        key=lambda r: (r["result"] != "TIMEOUT", r["test"]),
    )[:15]:
        top_failures.append(
            {
                "test": record["test"],
                "module": record["module"],
                "result": record["result"],
                "first_error": record["first_error"][:300],
            }
        )
    return {
        "schema": 1,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "elapsed_s": round(elapsed_s, 3),
        "pir": {
            "path": str(pir.resolve()),
            "sha256": sha256_file(pir),
            "size": pir.stat().st_size,
            "mtime": iso_mtime(pir),
            "reported_version": pir_version,
        },
        "shims": shims,
        "suite": suite,
        "selection": selection,
        "harness": {
            "timeout_s": timeout,
            "jobs": jobs,
            "cwd": "repo-shaped work root (contains test/)",
            "shim_mode": configured_shim_mode,
            "env": {
                "PIR_AGENT_DIR": "<agent-root>/agent-<run>-<per-test id>",
                "NODE_TEST_KNOWN_GLOBALS": "0",
                "TEST_SERIAL_ID": "<per-test id>",
                "TEST_THREAD_ID": "<per-test id>",
                "NODE_TEST_DIR": "<out>/testtmp/<per-test id>",
                "NODE_COMMON_PORT": "12346+<slot>",
                **env_overrides,
            },
        },
        "counts": counts,
        "shares": {
            key: (round(value / total, 4) if total else 0)
            for key, value in counts.items()
        },
        "total": total,
        "meaningful_verdict": {
            "count": meaningful,
            "share": round(meaningful / total, 4) if total else 0,
            "definition": "PASS + ASSERT + ERROR + UNSUPPORTED (excludes SKIP and TIMEOUT)",
        },
        "durations_ms": {
            "min": durations[0] if durations else 0,
            "p50": percentile(durations, 0.50),
            "p90": percentile(durations, 0.90),
            "p99": percentile(durations, 0.99),
            "max": durations[-1] if durations else 0,
            "sum": sum(durations),
        },
        "by_module": dict(sorted(by_module.items())),
        "top_failures": top_failures,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--pir", required=True, type=Path, help="path to pir.exe")
    parser.add_argument(
        "--pir-root", type=Path,
        help="checkout used to build pir (required when PIR_EXT_SHIM_SRC is enabled)",
    )
    parser.add_argument("--corpus", required=True, type=Path, help="Node source checkout (contains test/)")
    parser.add_argument("--list", type=Path, help="file with test basenames, one per line")
    parser.add_argument("--prefix", action="append", default=[], help="select test/parallel/*.js by prefix (repeatable)")
    parser.add_argument("--out", required=True, type=Path, help="output directory for results and logs")
    parser.add_argument("--work", type=Path, help="test-shaped work root (default: <out>/work)")
    parser.add_argument("--timeout", type=float, default=8.0, help="per-test timeout in seconds (default: 8)")
    parser.add_argument("--jobs", type=int, default=5, help="parallel test processes (default: 5)")
    parser.add_argument("--patch", type=Path, default=DEFAULT_PATCH, help="common/index.js overlay patch")
    parser.add_argument("--skip-copy", action="store_true", help="reuse an already prepared work root")
    parser.add_argument(
        "--agent-root",
        type=Path,
        help="directory for per-test agent dirs (default: common parent of --work and --out)",
    )
    parser.add_argument("--env", action="append", default=[], metavar="KEY=VALUE", help="extra test env (repeatable)")
    parser.add_argument("--write-baseline", type=Path, help="also write known-failures.json to this path")
    parser.add_argument("--quiet", action="store_true", help="do not print per-test progress")
    args = parser.parse_args(argv)

    pir = args.pir.resolve()
    if not pir.is_file():
        raise SystemExit(f"pir executable not found: {pir}")
    corpus = args.corpus.resolve()
    work = (args.work or (args.out / "work")).resolve()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)

    env_overrides = SHIM_ENV.copy()
    for item in args.env:
        if "=" not in item:
            parser.error(f"--env expects KEY=VALUE, got: {item}")
        key, value = item.split("=", 1)
        env_overrides[key.upper() if os.name == "nt" else key] = value
    configured_shim_mode, source_enabled = shim_mode(env_overrides)
    pir_root = args.pir_root.resolve() if args.pir_root else None
    if source_enabled and pir_root is None:
        parser.error("--pir-root is required when PIR_EXT_SHIM_SRC is enabled")
    run_env = child_env(env_overrides)

    names = collect_tests(corpus, args.list, args.prefix)
    suite = suite_metadata(corpus)

    eprint(f"pir:    {pir}")
    eprint(f"sha256: {sha256_file(pir)}")
    eprint(f"mtime:  {iso_mtime(pir)}")
    eprint(f"suite:  {suite['tag']} ({suite['commit']})")
    eprint(f"tests:  {len(names)}  timeout: {args.timeout}s  jobs: {args.jobs}")
    eprint(f"work:   {work}")

    prepare_work_root(corpus, work, args.patch.resolve(), skip_copy=args.skip_copy)

    # pir confines filesystem writes to parent(PIR_AGENT_DIR).  Every agent
    # dir therefore has to be a direct child of a root that also contains the
    # work root and the output tree; nesting it deeper silently makes tmpdir
    # creation fail with EACCES.
    if args.agent_root:
        agent_root = args.agent_root.resolve()
    else:
        try:
            agent_root = Path(os.path.commonpath([str(work), str(out)]))
        except ValueError:
            agent_root = out.parent
            eprint(
                "warning: --work and --out are on different roots; "
                f"using {agent_root} as the agent root"
            )
    agent_root.mkdir(parents=True, exist_ok=True)
    run_token = re.sub(r"[^A-Za-z0-9_.-]+", "-", out.name)[:24] or "run"
    version_agent = agent_root / f"agent-{run_token}-version"
    version_agent.mkdir(parents=True, exist_ok=True)
    eprint(f"agents: {agent_root} (parent is pir's writable root)")

    # Every test gets a fresh tmp root and a globally unique serial id, so a
    # read-only file left behind by one test cannot poison the next one.
    test_tmp_root = out / "testtmp"
    test_tmp_root.mkdir(parents=True, exist_ok=True)

    # A missing tree can make pir silently fall back to embedded sources.
    # Never record that as a stable source run.
    shim_before = shim_tree_metadata(pir_root) if source_enabled and pir_root else None
    if source_enabled and shim_before is None:
        raise SystemExit(f"cannot fingerprint source shims under --pir-root: {pir_root}")

    # Ask pir what it claims to be (recorded, never enforced).
    try:
        ver = subprocess.run(
            [str(pir), "js", "--version"],
            capture_output=True,
            text=True,
            timeout=20,
            env={"PIR_AGENT_DIR": str(version_agent), **run_env},
        )
        pir_version = (ver.stdout or ver.stderr).strip().splitlines()[0] if (ver.stdout or ver.stderr).strip() else None
    except (OSError, subprocess.SubprocessError):
        pir_version = None

    log_dir = out / "logs"
    log_dir.mkdir(parents=True, exist_ok=True)

    free_slots: queue.Queue[int] = queue.Queue()
    for slot in range(args.jobs):
        free_slots.put(slot)

    records: list[dict] = []
    started = time.monotonic()
    id_counter = itertools.count()
    id_lock = threading.Lock()

    def next_test_id() -> int:
        with id_lock:
            return next(id_counter)

    def task(name: str) -> dict:
        slot = free_slots.get()
        test_id = next_test_id()
        test_tmp = test_tmp_root / f"{test_id:04d}"
        test_agent = agent_root / f"agent-{run_token}-{test_id:04d}"
        test_tmp.mkdir(parents=True, exist_ok=True)
        test_agent.mkdir(parents=True, exist_ok=True)
        try:
            record = run_one(
                pir,
                work,
                out,
                name,
                timeout=TIMEOUT_OVERRIDES.get(name, args.timeout),
                slot=slot,
                test_id=test_id,
                test_tmp=test_tmp,
                test_agent=test_agent,
                run_env=run_env,
            )
        finally:
            free_slots.put(slot)
        (log_dir / f"{name}.log").write_text(record.pop("output"), encoding="utf-8")
        return record

    try:
        with ThreadPoolExecutor(max_workers=args.jobs) as pool:
            futures = {pool.submit(task, name): name for name in names}
            done = 0
            for future in as_completed(futures):
                record = future.result()
                record["module"] = module_of(record["test"])
                records.append(record)
                done += 1
                if not args.quiet:
                    eprint(
                        f"[{done:>4}/{len(names)}] {record['test']:<58} "
                        f"{record['result']:<11} {record['duration_ms']:>6} ms"
                    )
    except KeyboardInterrupt:
        eprint("interrupted; writing partial results")
        raise SystemExit(130)

    elapsed = time.monotonic() - started
    shims = None
    if source_enabled and pir_root is not None:
        shim_after = shim_tree_metadata(pir_root)
        shims = {
            "checkout_root": str(pir_root),
            "before": shim_before,
            "after": shim_after,
            "stable": shim_after is not None and shim_before == shim_after,
        }
        if not shims["stable"]:
            eprint("warning: source shims changed or could not be read; results are not a single snapshot")
    records.sort(key=lambda r: r["test"])

    results_path = out / "results.jsonl"
    with results_path.open("w", encoding="utf-8") as fh:
        for record in records:
            fh.write(json.dumps(record, ensure_ascii=False) + "\n")

    selection = {
        "list": str(args.list.resolve()) if args.list else None,
        "prefixes": args.prefix,
        "count": len(names),
    }
    summary = build_summary(
        records,
        pir=pir,
        pir_version=pir_version,
        suite=suite,
        selection=selection,
        timeout=args.timeout,
        jobs=args.jobs,
        env_overrides=env_overrides,
        configured_shim_mode=configured_shim_mode,
        elapsed_s=elapsed,
        shims=shims,
    )
    write_json(out / "summary.json", summary)

    baseline = {
        "schema": 1,
        "generated_at": summary["generated_at"],
        "pir": summary["pir"],
        "shims": summary["shims"],
        "suite": summary["suite"],
        "selection": selection,
        "harness": summary["harness"],
        "results": {
            record["test"]: {
                "result": record["result"],
                "exit_code": record["exit_code"],
                "duration_ms": record["duration_ms"],
                "module": record["module"],
                "first_error": record["first_error"],
                "skip_reason": record["skip_reason"],
            }
            for record in records
        },
    }
    write_json(out / "known-failures.json", baseline)
    if args.write_baseline:
        write_json(args.write_baseline, baseline)
        eprint(f"baseline written: {args.write_baseline}")

    eprint("")
    eprint(f"wrote {results_path}")
    eprint(f"wrote {out / 'summary.json'}")
    eprint(f"wrote {out / 'known-failures.json'}")
    eprint("counts: " + ", ".join(f"{k}={summary['counts'][k]}" for k in RESULT_ORDER))
    eprint(
        f"median: {summary['durations_ms']['p50']} ms  "
        f"p90: {summary['durations_ms']['p90']} ms  "
        f"total: {elapsed:.1f}s"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
