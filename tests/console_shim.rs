//! `node:console` in the extension host: the node v24 Console surface and the
//! host-routing rule for the global console's default streams.
//!
//! The vendored nodejs/node suite (scripts/node_tests, test-console-*.js)
//! covers the same behavior in `pir js` script mode; this test pins the parts
//! that only an extension host can regress silently:
//!   * `require("console") === globalThis.console` and the per-instance bound
//!     methods (names, non-constructibility, custom `instanceof`);
//!   * the constructor's ERR_CONSOLE_WRITABLE_STREAM / ERR_INVALID_ARG_TYPE /
//!     ERR_OUT_OF_RANGE validation codes;
//!   * counters / group indentation on an explicit-stream Console;
//!   * a `process.stdout.write` replaced after boot receives every default
//!     console write (the extension host must not swallow it into notify),
//!     while an untouched default stream still routes to the prelude console.
//!
//! Fixtures are seeded from source strings at runtime (the repo hygiene gate
//! forbids tracked `.ts` files), mirroring tests/node_shims.rs.

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime, Notification};
use serde_json::{json, Value};

fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-console-shim-{}-{test_name}",
        std::process::id()
    ));
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

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: "You are a test.".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

async fn run_fixture(name: &str, ts: &str, global_name: &str) -> (Value, Vec<Notification>) {
    let dir = write_temp_fixture(name, &[]);
    let rt = ExtensionRuntime::new(test_config_at(&dir))
        .await
        .expect("runtime construction");
    rt.allow_root(dir);
    let module = format!("pir-ext:{name}.ts");
    rt.seed_module(&module, ts).expect("seed fixture");
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
    let value = rt
        .eval_json(&format!("JSON.stringify(globalThis.{global_name})"))
        .await
        .expect("read fixture results");
    (value, rt.drain_notifications())
}

const CONSOLE_SURFACE_TS: &str = r#"
import consoleModule from "node:console";

export default (pi: any) => {
  pi.on("session_start", () => {
    const out: any = {};
    const { Console } = consoleModule;

    out.identity = consoleModule === globalThis.console;
    out.instanceofConsole = globalThis.console instanceof Console;
    out.plainNotConsole = ({} as any) instanceof Console;
    out.consoleOnGlobal = typeof consoleModule.Console === "function";

    // Every method's `.name` is its own key on an instance; the global console
    // inherits the aliases' names (dirxml -> log, groupCollapsed -> group).
    const instance: any = new Console({ stdout: { write() { return true; } } });
    const names: any = {};
    for (const method of ["log", "info", "debug", "warn", "error", "dir", "count",
                          "group", "groupEnd", "table", "dirxml", "groupCollapsed"]) {
      names[method] = instance[method].name;
    }
    out.instanceNames = names;
    out.globalNames = {
      log: consoleModule.log.name,
      dirxml: consoleModule.dirxml.name,
      groupCollapsed: consoleModule.groupCollapsed.name,
    };

    // Methods are callable but not constructible (node binds them onto the
    // instance; a bound non-constructor throws under `new`).
    const notConstructed = (fn: any) => {
      try { new fn(); return false; } catch { return true; }
    };
    out.notConstructors =
      notConstructed(instance.log) && notConstructed(instance.dirxml) &&
      notConstructed(instance.group) && notConstructed(consoleModule.count);

    // Constructor validation: node's codes and messages.
    const tryCatch = (fn: any) => {
      try { fn(); return null; } catch (error: any) {
        return { name: error.constructor.name, code: error.code, message: error.message };
      }
    };
    out.noStdout = tryCatch(() => new Console());
    out.badStderr = tryCatch(() => new Console(
      { write() { return true; } }, { write: undefined }));
    out.badGroupIndent = tryCatch(() => new Console({
      stdout: { write() { return true; } }, groupIndentation: "3" }));
    out.outOfRangeIndent = tryCatch(() => new Console({
      stdout: { write() { return true; } }, groupIndentation: 1.5 }));
    out.badInspectOptions = tryCatch(() => new Console({
      stdout: { write() { return true; } }, inspectOptions: null }));

    // Counters / group indentation on an explicit-stream instance.
    const writes: string[] = [];
    const sink = { write(chunk: any) { writes.push(String(chunk)); return true; } };
    const c: any = new Console({ stdout: sink, stderr: sink, colorMode: false, groupIndentation: 3 });
    c.count();
    c.count("a");
    c.count();
    c.countReset("a");
    c.count("a");
    c.group("L");
    c.log("x\ny");
    c.groupEnd();
    c.log("done");
    const symbolCountThrew = (() => {
      try { c.count(Symbol("s")); return false; } catch (error: any) {
        return error.constructor.name === "TypeError";
      }
    })();
    out.writes = writes;
    out.symbolCountThrew = symbolCountThrew;

    // Routing: a write replaced after boot is what the default console uses.
    const previousOut = (process.stdout as any).write;
    const previousErr = (process.stderr as any).write;
    const captured: string[] = [];
    const capturedErr: string[] = [];
    (process.stdout as any).write = (chunk: any) => { captured.push(String(chunk)); return true; };
    (process.stderr as any).write = (chunk: any) => { capturedErr.push(String(chunk)); return true; };
    consoleModule.log("routed", { a: 1 });
    consoleModule.warn("warned");
    (process.stdout as any).write = previousOut;
    (process.stderr as any).write = previousErr;
    out.routed = captured.length === 1 && captured[0] === "routed { a: 1 }\n";
    out.routedWarn = capturedErr.length === 1 && capturedErr[0] === "warned\n";

    // With the boot streams untouched (restored above) the default console
    // must not go to the raw host stdio objects: the extension host drains the
    // prelude notify path instead, and painting raw text would corrupt the TUI.
    consoleModule.log("to-notify", 1);
    consoleModule.error("to-notify-error");

    globalThis.__consoleSurface = out;
  });
};
"#;

#[tokio::test]
async fn console_surface_matches_node_semantics() {
    let (v, notifications) = run_fixture("surface", CONSOLE_SURFACE_TS, "__consoleSurface").await;

    assert_eq!(
        v["identity"], true,
        "require(\"console\") must be globalThis.console: {v}"
    );
    assert_eq!(
        v["instanceofConsole"], true,
        "global console instanceof Console: {v}"
    );
    assert_eq!(
        v["plainNotConsole"], false,
        "{{}} instanceof Console must be false: {v}"
    );
    assert_eq!(
        v["consoleOnGlobal"], true,
        "Console is exposed on the global console: {v}"
    );

    let names = &v["instanceNames"];
    for method in [
        "log",
        "info",
        "debug",
        "warn",
        "error",
        "dir",
        "count",
        "group",
        "groupEnd",
        "table",
        "dirxml",
        "groupCollapsed",
    ] {
        assert_eq!(
            names[method],
            json!(method),
            "instance method name mismatch for {method}: {v}"
        );
    }
    let global_names = &v["globalNames"];
    assert_eq!(global_names["log"], json!("log"), "global log name: {v}");
    assert_eq!(
        global_names["dirxml"],
        json!("log"),
        "dirxml is the log alias: {v}"
    );
    assert_eq!(
        global_names["groupCollapsed"],
        json!("group"),
        "groupCollapsed alias: {v}"
    );
    assert_eq!(
        v["notConstructors"], true,
        "console methods must not be constructors: {v}"
    );

    assert_eq!(
        v["noStdout"]["code"],
        json!("ERR_CONSOLE_WRITABLE_STREAM"),
        "new Console() code: {v}"
    );
    assert_eq!(
        v["noStdout"]["name"],
        json!("TypeError"),
        "new Console() type: {v}"
    );
    assert_eq!(
        v["noStdout"]["message"],
        json!("Console expects a writable stream instance for stdout"),
        "new Console() message: {v}"
    );
    assert_eq!(
        v["badStderr"]["message"],
        json!("Console expects a writable stream instance for stderr"),
        "stderr writability message: {v}"
    );
    assert_eq!(
        v["badGroupIndent"]["code"],
        json!("ERR_INVALID_ARG_TYPE"),
        "groupIndentation type code: {v}"
    );
    assert_eq!(
        v["outOfRangeIndent"]["code"],
        json!("ERR_OUT_OF_RANGE"),
        "groupIndentation range code: {v}"
    );
    assert!(
        v["outOfRangeIndent"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("an integer"),
        "groupIndentation range message: {v}"
    );
    assert_eq!(
        v["badInspectOptions"]["code"],
        json!("ERR_INVALID_ARG_TYPE"),
        "inspectOptions code: {v}"
    );
    assert_eq!(
        v["badInspectOptions"]["message"],
        json!("The \"options.inspectOptions\" property must be of type object. Received null"),
        "inspectOptions message: {v}"
    );

    assert_eq!(
        v["writes"],
        json!([
            "default: 1\n",
            "a: 1\n",
            "default: 2\n",
            "a: 1\n",
            "L\n",
            "   x\n   y\n",
            "done\n"
        ]),
        "count/group output mismatch: {v}"
    );
    assert_eq!(
        v["symbolCountThrew"], true,
        "count(Symbol()) must throw TypeError: {v}"
    );
    assert_eq!(
        v["routed"], true,
        "console.log must follow process.stdout.write: {v}"
    );
    assert_eq!(
        v["routedWarn"], true,
        "console.warn must follow process.stderr: {v}"
    );

    // The untouched default streams go to the prelude notify path, at the
    // levels the extension host has always used (`log` -> debug,
    // `error` -> error).
    let notices: Vec<(String, String)> = notifications
        .iter()
        .filter_map(|notification| match notification {
            Notification::Notify { message, level } => Some((message.clone(), level.clone())),
            _ => None,
        })
        .collect();
    assert!(
        notices
            .iter()
            .any(|(message, level)| message == "to-notify 1\n" && level == "debug"),
        "console.log must notify the host: {notices:?}"
    );
    assert!(
        notices
            .iter()
            .any(|(message, level)| message == "to-notify-error\n" && level == "error"),
        "console.error must notify the host: {notices:?}"
    );
}
