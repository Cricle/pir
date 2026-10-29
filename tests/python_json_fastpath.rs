//! Embedded-RustPython JSON fast path: native `json.dumps`/`json.loads`/
//! `json.dump`/`json.load` backed by `serde_json`, with the stdlib functions
//! kept as a semantic fallback.
//!
//! Coverage:
//! - the override is installed in both session policies (`python` tool =
//!   persistent namespace, `ctx_execute` = fresh namespace per call);
//! - a representative 1k-record payload round-trips and produces the same
//!   text as the standard library for `sort_keys`, `indent`, `separators`,
//!   `ensure_ascii` and non-ASCII strings;
//! - unsupported objects/options delegate to the original stdlib functions
//!   (so `TypeError`/`ValueError`/`JSONDecodeError` behavior is preserved),
//!   including circular structures and dict subclasses;
//! - `json.loads` keeps arbitrary-precision Python ints (the serde fast path
//!   declines those) and still raises `JSONDecodeError` for malformed input;
//! - `json.dump`/`json.load` on file-like objects;
//! - the 1 MiB captured-output cap is unchanged for serialized output;
//! - the timeout error names the budget and suggests batching.
//!
//! `perf_json_10k_and_loop_100k` is `#[ignore]`d: it prints timings instead
//! of asserting, for before/after comparisons.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::json;

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, ToolContext};
use pir::plugins::builtin::{
    ContextModePlugin, PythonTool, CTX_EXECUTE_TOOL_NAME, MAX_OUTPUT_CHARS,
};
use pir::plugins::plugin::{AgentPlugin, PluginRegistrations, RegistrationContext, ToolBackend};

// ---------------------------------------------------------------------------
// Harnesses
// ---------------------------------------------------------------------------

fn python_tool() -> PythonTool {
    PythonTool::new()
}

fn tool_ctx() -> ToolContext {
    ToolContext::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

async fn run_python(tool: &PythonTool, code: &str) -> ToolResult {
    tool.run(&json!({ "code": code }), &tool_ctx()).await
}

/// Assert a Python script ran clean and printed the expected marker.
async fn run_python_ok(tool: &PythonTool, code: &str) {
    let result = run_python(tool, code).await;
    assert!(
        !result.is_error,
        "python script failed:\n{}",
        result.content
    );
    assert!(
        result.content.contains("OK"),
        "expected the OK marker, got:\n{}",
        result.content
    );
}

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-python-json-{tag}-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap_or_else(|error| panic!("temp dir: {error}"));
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A registered `ctx_execute` tool (the `Fresh` session policy), isolated in
/// its own temp directory.
struct ContextHarness {
    tool: Arc<dyn AgentTool>,
    _dir: TestDir,
}

impl ContextHarness {
    fn new(tag: &str) -> Self {
        let dir = TestDir::new(tag);
        let plugin = ContextModePlugin::with_db_path(dir.path().join("context-mode.db"));
        let mut registrations = PluginRegistrations::default();
        plugin
            .register(&mut RegistrationContext::new(&mut registrations))
            .unwrap_or_else(|error| panic!("register: {error}"));
        let tools: BTreeMap<String, Arc<dyn AgentTool>> = registrations
            .tools
            .into_iter()
            .filter_map(|(definition, backend)| match backend {
                ToolBackend::Rust(tool) => Some((definition.name, tool)),
                ToolBackend::Js { .. } => None,
            })
            .collect();
        let tool = tools
            .get(CTX_EXECUTE_TOOL_NAME)
            .unwrap_or_else(|| panic!("`{CTX_EXECUTE_TOOL_NAME}` was not registered"))
            .clone();
        Self { tool, _dir: dir }
    }

    async fn run(&self, code: &str) -> ToolResult {
        self.tool.run(&json!({ "code": code }), &tool_ctx()).await
    }
}

// ---------------------------------------------------------------------------
// Installation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn override_is_installed_in_persistent_session() {
    let tool = python_tool();
    run_python_ok(
        &tool,
        r#"
import json

assert hasattr(json, "_pir_stdlib_dumps"), "stdlib dumps backup missing"
assert hasattr(json, "_pir_stdlib_loads"), "stdlib loads backup missing"
assert hasattr(json, "_pir_stdlib_dump"), "stdlib dump backup missing"
assert hasattr(json, "_pir_stdlib_load"), "stdlib load backup missing"
assert json.dumps is not json._pir_stdlib_dumps, "dumps was not replaced"

# Native functions are `builtin_function_or_method`; the pure-Python stdlib
# functions are plain `function` objects.
assert type(json.dumps).__name__ == "builtin_function_or_method", type(json.dumps).__name__
assert type(json.loads).__name__ == "builtin_function_or_method", type(json.loads).__name__
assert type(json.dump).__name__ == "builtin_function_or_method", type(json.dump).__name__
assert type(json.load).__name__ == "builtin_function_or_method", type(json.load).__name__

# A fresh import in the same session still sees the override.
del json
import json
assert type(json.dumps).__name__ == "builtin_function_or_method"
print("OK")
"#,
    )
    .await
}

#[tokio::test]
async fn override_is_installed_in_fresh_session() {
    let harness = ContextHarness::new("fresh");
    let result = harness
        .run(
            r#"
import json
print(hasattr(json, "_pir_stdlib_dumps"))
print(type(json.dumps).__name__)
print(json.dumps({"a": [1, None, True]}))
"#,
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("True"),
        "stdlib backup visible, got: {}",
        result.content
    );
    assert!(
        result.content.contains("builtin_function_or_method"),
        "dumps is native, got: {}",
        result.content
    );
    assert!(
        result.content.contains(r#"{"a": [1, null, true]}"#),
        "fast path output present, got: {}",
        result.content
    );

    // A second call uses a fresh namespace on the same interpreter: the
    // module-level override is still there.
    let second = harness
        .run(
            r#"
import json
assert "leaked" not in globals()
print(type(json.loads).__name__)
print(json.loads('{"b": 2}')["b"])
"#,
        )
        .await;
    assert!(!second.is_error, "{}", second.content);
    assert!(
        second.content.contains("builtin_function_or_method"),
        "loads is native in the fresh namespace too, got: {}",
        second.content
    );
    assert!(
        second.content.contains('2'),
        "fast path loads works, got: {}",
        second.content
    );
}

// ---------------------------------------------------------------------------
// Correctness vs the standard library
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dumps_and_loads_match_stdlib_for_records() {
    let tool = python_tool();
    run_python_ok(
        &tool,
        r#"
import json

records = [
    {
        "id": i,
        "name": "user-%d-é你" % i,
        "score": i * 0.5,
        "active": i % 2 == 0,
        "tags": ["alpha", "beta", "ü"],
        "meta": {"x": i, "y": -i, "ok": True, "missing": None},
        "pair": [i, i + 1],
    }
    for i in range(200)
]

# Exact text equality with the original stdlib implementation (ASCII escapes,
# default separators, insertion order).
fast = json.dumps(records)
slow = json._pir_stdlib_dumps(records)
assert fast == slow, "default dumps differs at %d" % next(
    (i for i, (a, b) in enumerate(zip(fast, slow)) if a != b), -1
)

# Tuples encode as arrays (the round-trip necessarily yields a list).
assert json.dumps((1, 2, (3,))) == "[1, 2, [3]]"

# Option coverage where our formatter mirrors the stdlib byte for byte.
for kwargs in (
    dict(sort_keys=True),
    dict(ensure_ascii=False),
    dict(indent=2),
    dict(indent=0),
    dict(separators=(",", ":")),
    dict(separators=("|", "=")),
    dict(indent=2, separators=(",", ":"), sort_keys=True),
):
    a = json.dumps(records, **kwargs)
    b = json._pir_stdlib_dumps(records, **kwargs)
    assert a == b, "dumps kwargs %r differ" % (kwargs,)

# Round-trip through the fast loads path.
text = json.dumps(records, sort_keys=True)
assert json.loads(text) == records
assert json.loads(json._pir_stdlib_dumps(records, ensure_ascii=False)) == records
assert json.loads(b'{"a": [1, 2]}') == {"a": [1, 2]}

# Exponent-form floats are still valid JSON that parses back identically.
floats = [1e20, 1e-20, 1.25, -0.5, 0.0]
assert json.loads(json.dumps(floats)) == floats
print("OK")
"#,
    )
    .await;
}

#[tokio::test]
async fn unicode_escapes_match_stdlib() {
    let tool = python_tool();
    run_python_ok(
        &tool,
        r#"
import json

payload = {
    "accent": "é",
    "cjk": "你好",
    "emoji": "😀",
    "controls": "a\nb\tc\r\b\f",
    "quote": 'a"b\\c',
}
ascii_text = json.dumps(payload)
assert ascii_text == json._pir_stdlib_dumps(payload)
assert "\\u00e9" in ascii_text, ascii_text
assert "\\u4f60\\u597d" in ascii_text, ascii_text
assert "\\ud83d\\ude00" in ascii_text, ascii_text
assert "\\n" in ascii_text and "\\t" in ascii_text and "\\r" in ascii_text

raw_text = json.dumps(payload, ensure_ascii=False)
assert raw_text == json._pir_stdlib_dumps(payload, ensure_ascii=False)
assert "é" in raw_text and "😀" in raw_text
assert json.loads(raw_text) == payload
assert json.loads(ascii_text) == payload
print("OK")
"#,
    )
    .await;
}

#[tokio::test]
async fn unsupported_objects_fall_back_to_stdlib() {
    let tool = python_tool();
    run_python_ok(
        &tool,
        r#"
import json

class Thing:
    pass

# No default: the stdlib must raise its own TypeError.
try:
    json.dumps({"x": Thing()})
except TypeError as error:
    assert "not JSON serializable" in str(error), str(error)
else:
    raise AssertionError("expected TypeError")

# With a default, the stdlib handles the object.
assert json.dumps({"x": Thing()}, default=lambda o: "thing") == '{"x": "thing"}'

# Dict/list subclasses are delegated (exact-type fast path only).
class D(dict):
    pass

class L(list):
    pass

assert json.dumps(D(a=1)) == '{"a": 1}'
assert json.dumps(L([1, 2])) == "[1, 2]"

# Circular structures: the fast path's cycle guard delegates, and the stdlib
# raises ValueError.
cycle = []
cycle.append(cycle)
try:
    json.dumps(cycle)
except ValueError as error:
    assert "Circular" in str(error), str(error)
else:
    raise AssertionError("expected ValueError for a circular value")

# Non-finite floats: allow_nan default True requires the stdlib's JavaScript
# spellings, which serde_json cannot emit.
assert json.dumps([float("nan")]) == "[NaN]"
assert json.dumps([float("inf"), -float("inf")]) == "[Infinity, -Infinity]"
try:
    json.dumps([float("nan")], allow_nan=False)
except ValueError:
    pass
else:
    raise AssertionError("expected ValueError for allow_nan=False")

# `skipkeys=True` is not part of the fast path.
assert json.dumps({1: "a", "b": 2}, sort_keys=False) == '{"1": "a", "b": 2}'

# Integers beyond u64 and float dict keys are stdlib-only encodings.
big = 2 ** 80
assert json.dumps(big) == str(big)
assert json.dumps({1.5: "x"}) == '{"1.5": "x"}'
print("OK")
"#,
    )
    .await;
}

#[tokio::test]
async fn loads_errors_and_big_ints_keep_python_semantics() {
    let tool = python_tool();
    run_python_ok(
        &tool,
        r#"
import json

# Malformed input still raises JSONDecodeError (a ValueError subclass) from
# the stdlib fallback.
for bad in ('{bad', '{"a": }', '', 'tru', '{"a": 1,}'):
    try:
        json.loads(bad)
    except json.JSONDecodeError as error:
        assert isinstance(error, ValueError)
    else:
        raise AssertionError("expected JSONDecodeError for %r" % (bad,))

# The stdlib accepts NaN/Infinity constants; the fallback preserves that.
assert json.loads("NaN") != json.loads("NaN")
assert json.loads("Infinity") == float("inf")

# Integers beyond u64 must stay Python ints (serde_json would lose precision),
# including negative zero.
big = 123456789012345678901234567890
assert json.loads(str(big)) == big
assert type(json.loads(str(big))) is int
assert type(json.loads("-0")) is int
assert json.loads("-0") == 0

# Duplicate keys keep the last value; order keeps the first position.
obj = json.loads('{"b": 1, "a": 2, "b": 3}')
assert obj == {"b": 3, "a": 2}
assert list(obj) == ["b", "a"]

# Floats are correctly rounded exactly like Python's parser. serde_json's
# default parser is ~1 ulp off for long mantissas without its float_roundtrip
# feature; the native path corrects each literal from the source text.
for literal, expected in (
    ("90.28571428571429", 632 / 7.0),
    ("0.1", 0.1),
    ("1e-20", 1e-20),
    ("3.141592653589793", 3.141592653589793),
    ("-0.0", -0.0),
):
    parsed = json.loads(literal)
    assert parsed == expected, (literal, parsed, expected)
    assert type(parsed) is float
ratios = [i / 7.0 for i in range(2000)]
assert json.loads(json.dumps(ratios)) == ratios

# Bytes input is delegated to the stdlib decoder.
assert json.loads(b'{"a": 1}') == {"a": 1}
print("OK")
"#,
    )
    .await;
}

#[tokio::test]
async fn dump_and_load_file_objects() {
    let tool = python_tool();
    run_python_ok(
        &tool,
        r#"
import io
import json

data = {"a": [1, 2, 3], "b": "é", "c": None}
buffer = io.StringIO()
json.dump(data, buffer)
text = buffer.getvalue()
assert text == json._pir_stdlib_dumps(data)

assert json.load(io.StringIO(text)) == data
assert json.load(io.StringIO('{"x": Infinity}'))["x"] == float("inf")

# Bytes-backed readers flow through the stdlib loads fallback.
assert json.load(io.BytesIO(b'{"a": 1}')) == {"a": 1}
print("OK")
"#,
    )
    .await;
}

// ---------------------------------------------------------------------------
// Output cap (unchanged)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn json_output_is_still_capped() {
    let tool = python_tool();
    let result = run_python(
        &tool,
        "import json\nprint(json.dumps({'k': 'x' * 1_100_000}))",
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    let limit = MAX_OUTPUT_CHARS;
    assert!(
        result.content.len() > limit,
        "output was produced and capped, got {} chars",
        result.content.len()
    );
    assert!(
        result.content.len() < limit + 200,
        "cap + notice, got {} chars",
        result.content.len()
    );
    assert!(
        result.content.contains("truncated"),
        "truncation notice present, got: ...{}",
        &result.content[result.content.len() - 120..]
    );
}

// ---------------------------------------------------------------------------
// Timeout failure mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn timeout_reports_budget_and_suggests_batching() {
    let tool = python_tool();
    let result = tool
        .run(
            &json!({ "code": "while True:\n    pass", "timeout_ms": 500 }),
            &tool_ctx(),
        )
        .await;
    assert!(result.is_error, "expected a timeout error");
    assert!(
        result.content.contains("timed out after 0.5s"),
        "the error names the elapsed budget, got: {}",
        result.content
    );
    assert!(
        result.content.contains("batch"),
        "the error suggests batching, got: {}",
        result.content
    );
}

// ---------------------------------------------------------------------------
// Timings (ignored by default; run with `--ignored --nocapture`)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "perf probe: prints timings for before/after comparison"]
async fn perf_json_10k_and_loop_100k() {
    let tool = python_tool();
    // Warm the worker (interpreter startup is not part of the hotspot).
    let warm = run_python(&tool, "1 + 1").await;
    assert!(!warm.is_error, "{}", warm.content);

    // Each probe is its own call: the debug interpreter is slow enough that
    // the fast path, the stdlib baseline and the loop together exceed a single
    // 120s budget. Separate calls also keep a baseline timeout from discarding
    // an already-measured fast result.
    let records = r#"
def make_records(n):
    return [
        {
            "id": i,
            "name": "user-%d" % i,
            "score": i * 0.5,
            "ratio": i / 7.0,
            "active": i % 2 == 0,
            "tags": ["alpha", "beta", "gamma"],
            "meta": {"x": i, "y": -i, "ok": True},
            "note": None,
        }
        for i in range(n)
    ]
"#;

    async fn probe(tool: &PythonTool, code: String) -> ToolResult {
        tool.run(&json!({ "code": code, "timeout_ms": 120_000 }), &tool_ctx())
            .await
    }

    // Fast path (the code under measurement), at 10k records and at the 200
    // used for the loads baseline (the pure-Python decoder does not finish
    // 10k records within the 120s budget in an unoptimized build).
    let fast = probe(
        &tool,
        format!(
            r#"
import json
import time
{records}

payload = make_records(10000)
t0 = time.perf_counter()
text = json.dumps(payload)
t1 = time.perf_counter()
back = json.loads(text)
t2 = time.perf_counter()
assert back == payload
print("PERF fast dumps_10k_ms=%.1f loads_10k_ms=%.1f bytes=%d" % ((t1 - t0) * 1000.0, (t2 - t1) * 1000.0, len(text)))

small = payload[:200]
t0 = time.perf_counter()
small_text = json.dumps(small)
t1 = time.perf_counter()
small_back = json.loads(small_text)
t2 = time.perf_counter()
assert small_back == small
print("PERF fast dumps_200_ms=%.1f loads_200_ms=%.1f" % ((t1 - t0) * 1000.0, (t2 - t1) * 1000.0))
"#
        ),
    )
    .await;
    if fast.is_error {
        // The probe must never fail a debug run; report and stop.
        println!("PERF fast probe failed: {}", fast.content);
        return;
    }
    print!("{}", fast.content);

    // The loop probe: a timed 100k-iteration accumulator (the 1M-iteration
    // shape that hits the 30s deadline in production).
    let loop_probe = probe(
        &tool,
        r#"
import time
t0 = time.perf_counter()
total = 0
for i in range(100000):
    total += i
t1 = time.perf_counter()
assert total == 4999950000
print("PERF loop_100k_ms=%.1f" % ((t1 - t0) * 1000.0,))
"#
        .to_owned(),
    )
    .await;
    if loop_probe.is_error {
        println!("PERF loop probe failed: {}", loop_probe.content);
    } else {
        print!("{}", loop_probe.content);
    }

    // Stdlib baseline (the "before" implementation): the encoder at 10k, then
    // the decoder at 200 (see above). A baseline timeout prints a note instead
    // of failing the probe.
    let dumps = probe(
        &tool,
        format!(
            r#"
import json
import time
{records}

payload = make_records(10000)
t0 = time.perf_counter()
slow_text = json._pir_stdlib_dumps(payload)
t1 = time.perf_counter()
print("PERF stdlib dumps_10000_ms=%.1f bytes=%d" % ((t1 - t0) * 1000.0, len(slow_text)))
"#
        ),
    )
    .await;
    if dumps.is_error {
        println!("PERF stdlib dumps baseline failed: {}", dumps.content);
    } else {
        print!("{}", dumps.content);
    }

    let loads = probe(
        &tool,
        format!(
            r#"
import json
import time
{records}

payload = make_records(200)
text = json._pir_stdlib_dumps(payload)
t0 = time.perf_counter()
back = json._pir_stdlib_loads(text)
t1 = time.perf_counter()
assert back == payload
print("PERF stdlib loads_200_ms=%.1f" % ((t1 - t0) * 1000.0,))
"#
        ),
    )
    .await;
    if loads.is_error {
        println!("PERF stdlib loads baseline failed: {}", loads.content);
    } else {
        print!("{}", loads.content);
    }
}
