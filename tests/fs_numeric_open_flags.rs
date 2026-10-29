//! Host fs bridge: `open()` takes node's **numeric** `flags` — an int32
//! `fs.constants` `O_*` mask — not only flag strings.
//!
//! This is the gap that kept `NodeExecutionEnv.openBinaryReader`/`scanLines`
//! and `/tools`' read path unusable on the QuickJS runtime: the durable
//! environment opens with `constants.O_RDONLY` (a number), and the bridge used
//! to stringify it into `"0"`, which the flag parser rejected with
//! `ERR_INVALID_ARG_VALUE: The argument 'flags' is invalid. Received '0'`.
//!
//! The matrix below locks the mapping onto the host `OpenOptions` (access mode,
//! create/exclusive/truncate/append), node's argument validation
//! (`stringToFlags`: int32 range check, `null` → `O_RDONLY`, anything else is
//! an argument error) and the refusal shapes for the combinations the OS
//! rejects (access mode 3, read-only truncation → `EINVAL` with
//! `syscall`/`path`/`errno`, like node's libuv error).
//!
//! Fixtures are generated at runtime into per-test temp dirs (repository
//! hygiene gate; same pattern as `tests/child_process_stdio.rs`).

use std::path::PathBuf;

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// See tests/child_process_stdio.rs: fixtures must be written at runtime.
fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pir-fs-numeric-{}-{test_name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    for (name, content) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture subdir");
        }
        std::fs::write(&path, content).expect("write fixture");
    }
    dir
}

fn test_config() -> ExtensionConfig {
    ExtensionConfig {
        cwd: std::env::temp_dir(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: "You are a test.".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// Runtime + fixture in one call; `__DIR__` resolves to this test's temp dir.
async fn run_fixture(name: &str, body: &str) -> Value {
    let dir = write_temp_fixture(name, &[]);
    let ts = body.replace("__DIR__", &dir.to_string_lossy().replace('\\', "/"));
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");
    rt.allow_root(dir.clone());
    let module = format!("pir-ext:fs-numeric-{name}.ts");
    rt.seed_module(&module, &ts).expect("seed fixture");
    let loaded = rt.seeded_load(&module).await;
    assert_eq!(loaded.expect("seeded load"), 1, "fixture {name} must load");
    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors in {name}: {:?}",
        outcome.errors
    );
    rt.eval_json(&format!("JSON.stringify(globalThis.{name})"))
        .await
        .expect("read fixture results")
}

/// The probe every case shares: `code(run)` reports the value or the full
/// error shape (`name`/`code`/`message`/`syscall`/`errno`).
const HELPERS: &str = r#"
import fs from "node:fs";
import { join } from "node:path";

const DIR = "__DIR__";
const C = fs.constants;

function code(run) {
  try {
    const value = run();
    return { ok: true, value: String(value) };
  } catch (error) {
    return {
      ok: false,
      name: error.name,
      code: error.code,
      message: error.message,
      syscall: error.syscall,
      errno: error.errno,
      pathTail: error.path === undefined ? undefined : String(error.path).slice(-9),
    };
  }
}
"#;

/// The numeric-flag matrix over the whole fd family and the promises/readFile
/// wrappers that funnel through it.
#[tokio::test(flavor = "current_thread")]
async fn numeric_flags_open_read_write_append_and_create() {
    let ts = format!(
        r#"{HELPERS}
export default (pi) => {{
  pi.on("session_start", async () => {{
    const out = {{ isWin: process.platform === "win32" }};

    // O_RDONLY (0): plain read, positioned and sequential.
    const readFile = join(DIR, "read.txt");
    fs.writeFileSync(readFile, "hello world");
    out.rdonly = code(() => {{
      const fd = fs.openSync(readFile, C.O_RDONLY);
      const buffer = Buffer.alloc(5);
      const read = fs.readSync(fd, buffer, 0, 5, 0);
      fs.closeSync(fd);
      return read + ":" + buffer.toString("utf8");
    }});

    // flags omitted / null also mean O_RDONLY (node's stringToFlags).
    out.omitted = code(() => {{
      const fd = fs.openSync(readFile);
      const buffer = Buffer.alloc(5);
      const read = fs.readSync(fd, buffer, 0, 5, 0);
      fs.closeSync(fd);
      return read + ":" + buffer.toString("utf8");
    }});
    out.nullFlags = code(() => {{
      const fd = fs.openSync(readFile, null);
      fs.closeSync(fd);
      return typeof fd;
    }});

    // O_WRONLY (1): writes at the explicit position, reads are refused.
    const writeFile = join(DIR, "write.txt");
    fs.writeFileSync(writeFile, "hello world");
    out.wronly = code(() => {{
      const fd = fs.openSync(writeFile, C.O_WRONLY);
      fs.writeSync(fd, "HE", 0, "utf8");
      const readError = code(() => fs.readSync(fd, Buffer.alloc(1), 0, 1, 0));
      fs.closeSync(fd);
      return readError.code + ":" + fs.readFileSync(writeFile, "utf8").slice(0, 2);
    }});

    // O_RDWR (2): read and write on one descriptor.
    out.rdwr = code(() => {{
      const fd = fs.openSync(writeFile, C.O_RDWR);
      fs.writeSync(fd, "LL", 2, "utf8");
      const buffer = Buffer.alloc(4);
      const read = fs.readSync(fd, buffer, 0, 4, 0);
      fs.closeSync(fd);
      return read + ":" + buffer.toString("utf8");
    }});

    // O_APPEND (8): every write lands at EOF, explicit position or not.
    const appendFile = join(DIR, "append.txt");
    fs.writeFileSync(appendFile, "base");
    out.append = code(() => {{
      const fd = fs.openSync(appendFile, C.O_WRONLY | C.O_APPEND);
      fs.writeSync(fd, "-one");
      fs.writeSync(fd, "-two", 0);
      fs.closeSync(fd);
      return fs.readFileSync(appendFile, "utf8");
    }});
    // O_RDONLY|O_APPEND opens (a read-only descriptor cannot append anything).
    out.rdonlyAppend = code(() => {{
      const fd = fs.openSync(appendFile, C.O_RDONLY | C.O_APPEND);
      fs.closeSync(fd);
      return "opened";
    }});

    // O_CREAT|O_TRUNC|O_WRONLY creates and truncates.
    out.createTrunc = code(() => {{
      const created = join(DIR, "created-numeric.txt");
      const fd = fs.openSync(created, C.O_WRONLY | C.O_CREAT | C.O_TRUNC);
      fs.writeSync(fd, "made", 0, "utf8");
      fs.closeSync(fd);
      return fs.readFileSync(created, "utf8");
    }});
    // O_RDWR|O_TRUNC truncates an existing file to zero bytes.
    out.truncRdwr = code(() => {{
      const truncated = join(DIR, "trunc.txt");
      fs.writeFileSync(truncated, "0123456789");
      const fd = fs.openSync(truncated, C.O_RDWR | C.O_TRUNC);
      fs.closeSync(fd);
      return fs.statSync(truncated).size;
    }});
    // O_CREAT|O_EXCL on an existing path is EEXIST; the create bit alone
    // (no O_EXCL) succeeds on an existing file.
    out.createExcl = code(() => fs.openSync(readFile, C.O_WRONLY | C.O_CREAT | C.O_EXCL));
    out.createExisting = code(() => {{
      const fd = fs.openSync(readFile, C.O_WRONLY | C.O_CREAT);
      fs.closeSync(fd);
      return "opened";
    }});
    // O_EXCL without O_CREAT is ignored (the CRT contract node inherits).
    out.exclAlone = code(() => {{
      const fd = fs.openSync(readFile, C.O_WRONLY | C.O_EXCL);
      fs.closeSync(fd);
      return "opened";
    }});
    // A read-only access mode does not create (node win32: ENOENT on a
    // missing path, the file is not made).
    out.rdonlyCreateMissing = code(() => {{
      const missing = join(DIR, "rdonly-create.txt");
      const result = code(() => fs.openSync(missing, C.O_RDONLY | C.O_CREAT));
      return result.code + ":" + fs.existsSync(missing);
    }});

    // O_RDONLY on a missing path: ENOENT with the node error shape.
    out.missing = code(() => fs.openSync(join(DIR, "no-such-file.txt"), C.O_RDONLY));

    // promises.open and the readFile/writeFile flag options funnel through
    // the same numeric path.
    out.promisesOpen = "pending";
    try {{
      const handle = await fs.promises.open(readFile, C.O_RDONLY);
      const buffer = Buffer.alloc(5);
      await handle.read(buffer, 0, 5, 0);
      await handle.close();
      out.promisesOpen = buffer.toString("utf8");
    }} catch (error) {{
      out.promisesOpen = "ERR:" + error.code;
    }}
    out.readFileNumeric = code(() => {{
      const created = join(DIR, "readfile-numeric.txt");
      const text = fs.readFileSync(created, {{ encoding: "utf8", flag: C.O_RDWR | C.O_CREAT | C.O_TRUNC }});
      return JSON.stringify(text) + ":" + fs.existsSync(created);
    }});
    out.writeFileNumericAppend = code(() => {{
      const target = join(DIR, "writefile-numeric.txt");
      fs.writeFileSync(target, "a", {{ flag: C.O_WRONLY | C.O_CREAT | C.O_TRUNC }});
      fs.writeFileSync(target, "b", {{ flag: C.O_WRONLY | C.O_APPEND }});
      return fs.readFileSync(target, "utf8");
    }});
    // The callback form takes numeric flags too.
    out.callbackOpen = await new Promise((resolve) => {{
      fs.open(readFile, C.O_RDONLY, (error, fd) => {{
        if (error) {{ resolve("ERR:" + error.code); return; }}
        const buffer = Buffer.alloc(4);
        const read = fs.readSync(fd, buffer, 0, 4, 0);
        fs.closeSync(fd);
        resolve(read + ":" + buffer.toString("utf8"));
      }});
    }});

    globalThis.__fsNumericFlags = out;
  }});
}};
"#
    );
    let v = run_fixture("__fsNumericFlags", &ts).await;

    assert_eq!(
        v["rdonly"],
        json!({ "ok": true, "value": "5:hello" }),
        "O_RDONLY: {v}"
    );
    assert_eq!(
        v["omitted"],
        json!({ "ok": true, "value": "5:hello" }),
        "omitted flags default to O_RDONLY: {v}"
    );
    assert_eq!(
        v["nullFlags"]["value"], "number",
        "null flags default to O_RDONLY: {v}"
    );
    // A read on a write-only descriptor is refused. Node's libuv says EBADF;
    // the host's Windows handle denial surfaces as EPERM (pre-existing fd-path
    // divergence, orthogonal to the numeric flags).
    let read_denied = if cfg!(windows) {
        "EPERM:HE"
    } else {
        "EBADF:HE"
    };
    assert_eq!(
        v["wronly"],
        json!({ "ok": true, "value": read_denied }),
        "O_WRONLY writes and refuses reads: {v}"
    );
    assert_eq!(
        v["rdwr"],
        json!({ "ok": true, "value": "4:HELL" }),
        "O_RDWR reads back what it wrote: {v}"
    );
    assert_eq!(
        v["append"]["value"], "base-one-two",
        "O_APPEND lands every write at EOF: {v}"
    );
    assert_eq!(
        v["rdonlyAppend"]["value"], "opened",
        "O_RDONLY|O_APPEND opens read-only: {v}"
    );
    assert_eq!(
        v["createTrunc"]["value"], "made",
        "O_WRONLY|O_CREAT|O_TRUNC creates: {v}"
    );
    assert_eq!(
        v["truncRdwr"]["value"], "0",
        "O_RDWR|O_TRUNC truncates: {v}"
    );
    assert_eq!(
        v["createExcl"]["code"], "EEXIST",
        "O_CREAT|O_EXCL on an existing path is EEXIST: {v}"
    );
    assert_eq!(
        v["createExcl"]["syscall"], "open",
        "the EEXIST carries syscall open: {v}"
    );
    assert_eq!(
        v["createExisting"]["value"], "opened",
        "O_CREAT without O_EXCL opens an existing file: {v}"
    );
    assert_eq!(
        v["exclAlone"]["value"], "opened",
        "O_EXCL without O_CREAT is ignored: {v}"
    );
    assert_eq!(
        v["rdonlyCreateMissing"]["value"], "ENOENT:false",
        "a read-only create does not create (node win32 CRT semantics): {v}"
    );
    assert_eq!(
        v["missing"]["code"], "ENOENT",
        "O_RDONLY on a missing path: {v}"
    );
    assert_eq!(
        v["missing"]["syscall"], "open",
        "ENOENT carries syscall open: {v}"
    );
    assert_eq!(
        v["promisesOpen"], "hello",
        "fs.promises.open takes numeric flags: {v}"
    );
    assert_eq!(
        v["readFileNumeric"]["value"], "\"\":true",
        "readFileSync takes numeric flags: {v}"
    );
    assert_eq!(
        v["writeFileNumericAppend"]["value"], "ab",
        "writeFileSync honours a numeric O_APPEND: {v}"
    );
    assert_eq!(
        v["callbackOpen"], "4:hell",
        "fs.open's callback form takes numeric flags: {v}"
    );
}

/// Invalid combinations and argument validation: node's shapes, not the
/// bridge's old `ERR_INVALID_ARG_VALUE` for every numeric mask.
#[tokio::test(flavor = "current_thread")]
async fn numeric_flags_reject_invalid_shapes_like_node() {
    let ts = format!(
        r#"{HELPERS}
export default (pi) => {{
  pi.on("session_start", async () => {{
    const out = {{ isWin: process.platform === "win32" }};
    const file = join(DIR, "probe.txt");
    fs.writeFileSync(file, "probe");

    // Access mode 3 (O_WRONLY|O_RDWR) and a read-only truncation are refused
    // by the OS: EINVAL with syscall/path/errno, no argument-error wording.
    out.accessMode3 = code(() => fs.openSync(file, 3));
    out.rdonlyTrunc = code(() => fs.openSync(file, C.O_RDONLY | C.O_TRUNC));
    out.negative = code(() => fs.openSync(file, -1));

    // Argument validation happens in the shim, before the host call.
    out.fractional = code(() => fs.openSync(file, 1.5));
    out.nan = code(() => fs.openSync(file, NaN));
    out.overflow = code(() => fs.openSync(file, 2 ** 31));
    out.objectFlags = code(() => fs.openSync(file, {{}}));
    out.booleanFlags = code(() => fs.openSync(file, true));
    out.bigintFlags = code(() => fs.openSync(file, 1n));
    out.badString = code(() => fs.openSync(file, "rz"));

    // The refused opens must not have touched the file.
    out.stillIntact = fs.readFileSync(file, "utf8");

    globalThis.__fsNumericFlagErrors = out;
  }});
}};
"#
    );
    let v = run_fixture("__fsNumericFlagErrors", &ts).await;

    let expected_errno = if cfg!(windows) { -4071 } else { -22 };
    for key in ["accessMode3", "rdonlyTrunc", "negative"] {
        let seen = &v[key];
        assert_eq!(seen["ok"], false, "{key} must fail: {v}");
        assert_eq!(seen["code"], "EINVAL", "{key} is an OS-level EINVAL: {v}");
        assert_eq!(seen["syscall"], "open", "{key} carries syscall open: {v}");
        assert!(
            seen["message"]
                .as_str()
                .unwrap_or_default()
                .starts_with("EINVAL: invalid argument, open '"),
            "{key} keeps node's wording: {v}"
        );
        assert!(
            seen["message"]
                .as_str()
                .unwrap_or_default()
                .ends_with("probe.txt'"),
            "{key} names the path node would: {v}"
        );
        assert_eq!(
            seen["errno"], expected_errno,
            "{key} carries libuv's EINVAL errno: {v}"
        );
        assert_eq!(seen["pathTail"], "probe.txt", "{key} carries the path: {v}");
    }

    assert_eq!(
        v["fractional"],
        json!({
            "ok": false, "name": "RangeError", "code": "ERR_OUT_OF_RANGE",
            "message": "The value of \"flags\" is out of range. It must be an integer. Received 1.5",
        }),
        "a fractional flag is a JS range error: {v}"
    );
    for key in [
        "fractional",
        "nan",
        "overflow",
        "objectFlags",
        "booleanFlags",
        "bigintFlags",
    ] {
        assert!(
            v[key]["syscall"].is_null() && v[key]["errno"].is_null(),
            "{key} is a JS argument error with no OS errno: {v}"
        );
    }
    assert_eq!(v["nan"]["code"], "ERR_OUT_OF_RANGE", "NaN flags: {v}");
    assert_eq!(v["nan"]["name"], "RangeError", "NaN flags: {v}");
    assert_eq!(
        v["nan"]["message"],
        "The value of \"flags\" is out of range. It must be an integer. Received NaN",
        "NaN flags: {v}"
    );
    assert_eq!(
        v["overflow"]["code"], "ERR_OUT_OF_RANGE",
        "an out-of-int32 mask is a JS range error: {v}"
    );
    assert!(
        v["overflow"]["message"]
            .as_str()
            .unwrap_or_default()
            .starts_with("The value of \"flags\" is out of range. It must be >= -2147483648 && <= 2147483647. Received 2147483648"),
        "the int32 range text: {v}"
    );
    assert_eq!(
        v["objectFlags"],
        json!({
            "ok": false, "name": "TypeError", "code": "ERR_INVALID_ARG_VALUE",
            "message": "The argument 'flags' is invalid. Received {}",
        }),
        "an object flag is the node argument error: {v}"
    );
    assert_eq!(
        v["booleanFlags"]["message"], "The argument 'flags' is invalid. Received true",
        "a boolean flag: {v}"
    );
    assert_eq!(
        v["bigintFlags"]["message"], "The argument 'flags' is invalid. Received 1n",
        "a bigint flag: {v}"
    );
    assert_eq!(
        v["badString"],
        json!({
            "ok": false, "name": "Error", "code": "ERR_INVALID_ARG_VALUE",
            "message": "The argument 'flags' is invalid. Received 'rz'",
        }),
        "an unknown flag string keeps the host's validation: {v}"
    );
    assert!(
        v["badString"]["syscall"].is_null(),
        "a flag-string argument error carries no syscall: {v}"
    );
    assert_eq!(
        v["stillIntact"], "probe",
        "a refused open must not touch the file: {v}"
    );
}

/// Flag strings keep working (`r`/`rs`/`w`/`wx`/`a+`/`ax+`): the numeric path
/// is additive, and the string path's error shape is unchanged.
#[tokio::test(flavor = "current_thread")]
async fn string_flags_still_cover_the_node_set() {
    let ts = format!(
        r#"{HELPERS}
export default (pi) => {{
  pi.on("session_start", async () => {{
    const out = {{}};
    const file = join(DIR, "strings.txt");
    fs.writeFileSync(file, "alpha\nbeta\n");

    out.r = code(() => {{
      const fd = fs.openSync(file, "r");
      const buffer = Buffer.alloc(5);
      const read = fs.readSync(fd, buffer, 0, 5, 0);
      fs.closeSync(fd);
      return read + ":" + buffer.toString("utf8");
    }});
    out.rs = code(() => {{
      const fd = fs.openSync(file, "rs");
      fs.closeSync(fd);
      return "opened";
    }});
    out.rPlus = code(() => {{
      const fd = fs.openSync(file, "r+");
      fs.writeSync(fd, "A", 0, "utf8");
      fs.closeSync(fd);
      return fs.readFileSync(file, "utf8").slice(0, 1);
    }});
    out.w = code(() => {{
      const target = join(DIR, "w.txt");
      const fd = fs.openSync(target, "w");
      fs.writeSync(fd, "W", 0, "utf8");
      fs.closeSync(fd);
      return fs.readFileSync(target, "utf8");
    }});
    out.wx = code(() => fs.openSync(file, "wx"));
    out.wxNew = code(() => {{
      const fd = fs.openSync(join(DIR, "wx-new.txt"), "wx");
      fs.closeSync(fd);
      return "created";
    }});
    out.a = code(() => {{
      const target = join(DIR, "a.txt");
      fs.writeFileSync(target, "1");
      const fd = fs.openSync(target, "a");
      fs.writeSync(fd, "2");
      fs.closeSync(fd);
      return fs.readFileSync(target, "utf8");
    }});
    out.aPlus = code(() => {{
      const fd = fs.openSync(join(DIR, "aplus.txt"), "a+");
      fs.closeSync(fd);
      return "created";
    }});
    out.axPlus = code(() => {{
      const first = fs.openSync(join(DIR, "axplus.txt"), "ax+");
      fs.closeSync(first);
      return code(() => fs.openSync(join(DIR, "axplus.txt"), "ax+")).code;
    }});
    // The sync-hint twins of the append flags ("as"/"sa"/"as+"/"sa+") are part
    // of node's accepted set.
    out.as = code(() => {{
      const target = join(DIR, "as.txt");
      fs.writeFileSync(target, "1");
      const fd = fs.openSync(target, "as");
      fs.writeSync(fd, "2");
      fs.closeSync(fd);
      return fs.readFileSync(target, "utf8");
    }});
    out.sa = code(() => {{
      const fd = fs.openSync(join(DIR, "sa.txt"), "sa");
      fs.closeSync(fd);
      return "created";
    }});
    out.asPlus = code(() => {{
      const fd = fs.openSync(join(DIR, "asplus.txt"), "as+");
      fs.closeSync(fd);
      return "created";
    }});
    // Near-misses are refused, like node's stringToFlags switch.
    out.unknown = code(() => fs.openSync(file, "rw"));
    out.unknownMessage = code(() => fs.openSync(file, "rw")).message;
    out.nearMiss = code(() => fs.openSync(file, "rz")).code;
    out.upper = code(() => fs.openSync(file, "R")).code;
    out.empty = code(() => fs.openSync(file, "")).code;

    globalThis.__fsStringFlags = out;
  }});
}};
"#
    );
    let v = run_fixture("__fsStringFlags", &ts).await;

    assert_eq!(v["r"]["value"], "5:alpha", "'r' reads: {v}");
    assert_eq!(v["rs"]["value"], "opened", "'rs' opens: {v}");
    assert_eq!(v["rPlus"]["value"], "A", "'r+' writes: {v}");
    assert_eq!(v["w"]["value"], "W", "'w' creates and writes: {v}");
    assert_eq!(
        v["wx"]["code"], "EEXIST",
        "'wx' refuses an existing file: {v}"
    );
    assert_eq!(
        v["wxNew"]["value"], "created",
        "'wx' creates a new file: {v}"
    );
    assert_eq!(v["a"]["value"], "12", "'a' appends: {v}");
    assert_eq!(v["aPlus"]["value"], "created", "'a+' creates: {v}");
    assert_eq!(
        v["axPlus"]["value"], "EEXIST",
        "'ax+' refuses an existing file: {v}"
    );
    assert_eq!(v["as"]["value"], "12", "'as' appends like 'a': {v}");
    assert_eq!(v["sa"]["value"], "created", "'sa' creates: {v}");
    assert_eq!(v["asPlus"]["value"], "created", "'as+' creates: {v}");
    assert_eq!(
        v["nearMiss"], "ERR_INVALID_ARG_VALUE",
        "'rz' is refused, not read as 'r': {v}"
    );
    assert_eq!(
        v["upper"], "ERR_INVALID_ARG_VALUE",
        "an uppercase flag is refused: {v}"
    );
    assert_eq!(
        v["empty"], "ERR_INVALID_ARG_VALUE",
        "an empty flag string is refused: {v}"
    );
    assert_eq!(
        v["unknownMessage"], "The argument 'flags' is invalid. Received 'rw'",
        "an unknown flag string keeps node's message: {v}"
    );
    assert_eq!(
        v["unknown"]["name"], "Error",
        "the string-flag error shape: {v}"
    );
}
