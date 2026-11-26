//! `pir js <file> [args...]` — the subprocess script runner (node-compat
//! roadmap item B).
//!
//! Runs a standalone JS/TS file on pir's embedded QuickJS runtime with the
//! full node-compat layer, as a spawned child process would see it: real
//! stdio (`console.log` writes the process stdout), real argv
//! (`[execPath, file, ...args]` with `execPath` pointing back at the pir
//! binary), real exit codes (0 / `process.exit(n)` / 1 on an uncaught
//! module error), and the fs roots a script expects (script dir + cwd +
//! `~/.pi` + temp).
//!
//! Wiring (see `main_cli/run_mode.rs`): `js` is intercepted BEFORE clap and
//! argv preprocessing — script arguments are verbatim `process.argv`
//! material, so an unknown `--flag=value` must reach the script rather than
//! the extension-flag collector (the same reason the hidden codemode worker
//! intercepts early). No settings load, no trust resolution, no migrations,
//! no tracing, and above all no extension discovery: a script run loads only
//! its own entry file (npm dependencies of that file still resolve through
//! the normal loader). That keeps `pir js` cheap and keeps it from touching
//! any agent-session surface.
//!
//! Three node-CLI-shaped entry forms exist, because this subcommand is also
//! the target of the node-executable shim (`pir-node`'s child_process table
//! routes `spawn("node", …)` / `exec("node …")` here):
//!
//! * `pir js <file> [args...]` — the file runner (item B; unchanged).
//! * `pir js -e <code> [args...]` / `--eval <code>` / `--eval=<code>` —
//!   Node's `--eval`: the code runs as a CJS script in the global scope with
//!   `require` bound to the cwd, and `process.argv` is `[execPath, ...args]`
//!   (Node's `-e` argv shape: the code itself never appears in argv).
//! * `pir js -` — Node's stdin script: the whole stdin is read, then run
//!   through the same eval path (Node treats stdin as CJS without
//!   `--input-type`).
//! * `pir js -v` / `--version` — not the `js` runner's business, but the shim
//!   translation target for `node --version`: it claims the node
//!   compatibility baseline (see [`NODE_COMPAT_VERSION`]) instead of pir's
//!   own version. Deliberate compatibility masquerade.
//! * `pir [node-flags…] <file> [args...]` — the same runner reached through
//!   Node's respawn shape (`spawn(process.execPath, [file, …])`), with the
//!   V8-only flags in [`IGNORED_NODE_BOOL_FLAGS`] / [`IGNORED_NODE_VALUE_FLAGS`]
//!   dropped. Only an existing JS/TS file can take this path, so a prompt is
//!   never reinterpreted (see [`node_style_invocation`]).

use std::io::Read as _;
use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionLimits, ExtensionRuntime};

/// The Node version the node-executable shim claims. `node --version`,
/// `node -v`, `execFile("node", ["--version"])` and `execSync("node -v")` are
/// routed to `pir js --version`, which prints exactly this. The string is the
/// baseline the vendored node test suites and the shims were verified
/// against (see `assert_shim.js.txt` / `path_shim.js.txt` headers), so an
/// extension that gates behavior on the node version sees a coherent value.
///
/// This is an intentional compatibility lie: pir is not Node. `pir --version`
/// keeps reporting the real pir version, and no attempt is made to hide the
/// difference anywhere else (`process.execPath` in a routed child is the pir
/// binary, and `process.versions.node` remains the host stub's value).
const NODE_COMPAT_VERSION: &str = "v24.19.0";

/// Script-mode boot overlay (see `extensions/api/js/script_boot.js.txt`):
/// argv/execPath placeholders are replaced with JSON literals before eval.
///
/// Read through the engine's accessor rather than `include_str!`: the asset
/// moved into `crates/pir-core` with the engine split, so a relative
/// `include_str!` from this crate no longer resolves.
fn script_boot() -> &'static str {
    pir::extensions::api::script_boot_source()
}

/// Stack for the script thread: the entry module transpile (swc) and the
/// QuickJS runtime both recurse on deep module graphs; match the generous
/// main-thread budget rather than the 2 MB spawn default. Shared with
/// `src/main.rs` through [`pir::env_daemon::stack_budget_bytes`], so the two
/// entry points cannot drift apart.
fn script_thread_stack() -> usize {
    pir::env_daemon::stack_budget_bytes(cfg!(debug_assertions))
}

/// `PIR_JS_MEMORY_MB`: the explicit heap cap of a `pir js` run, in MiB
/// (`0` = no cap). This is a script-runner knob only: the extension host's
/// `PI_EXT_MEMORY_MB` / `extensionLimits.memoryMb` are deliberately NOT read
/// here (see [`script_limits`]).
const ENV_JS_MEMORY_MB: &str = "PIR_JS_MEMORY_MB";

/// QuickJS resource limits of a `pir js` runtime.
///
/// `pir js` is a standalone script process, not an extension: its workload
/// (large JSON/files, long-running node-style scripts) routinely needs a heap
/// far above the extension actor's low-resource safety net. Passing
/// `limits: None` would resolve the GLOBAL settings file plus the `PI_EXT_*`
/// environment through [`ExtensionLimits::resolve`], silently capping the
/// script at 128 MiB (or whatever an operator configured for extensions) and
/// failing it with the extension-host error text on exhaustion.
///
/// The injection seam is used instead — `Some(limits)` is adopted verbatim,
/// with no settings/env overlay — so the heap is uncapped by default and only
/// an explicit [`ENV_JS_MEMORY_MB`] opts into a cap. The stack and timeout
/// fields keep the extension defaults; script runs drive their own thread
/// stack ([`script_thread_stack`]) and are not dispatched through the
/// extension call-timeout machinery.
fn script_limits() -> ExtensionLimits {
    ExtensionLimits {
        memory_bytes: script_memory_bytes(),
        ..ExtensionLimits::default()
    }
}

/// [`ENV_JS_MEMORY_MB`] parsed into a byte cap; absent or `0` = no cap.
/// A malformed value is reported on stderr and treated as absent.
fn script_memory_bytes() -> Option<usize> {
    let raw = std::env::var(ENV_JS_MEMORY_MB).ok()?;
    match raw.trim().parse::<u64>() {
        Ok(0) => None,
        Ok(mb) => Some(usize::try_from(mb.saturating_mul(1024 * 1024)).unwrap_or(usize::MAX)),
        Err(_) => {
            eprintln!(
                "Error: {ENV_JS_MEMORY_MB}=\"{raw}\" ignored: expected MiB as a non-negative integer"
            );
            None
        }
    }
}

/// What one `pir js` invocation runs.
enum Entry {
    /// `pir js <file> [args...]`: import the file as the entry module.
    File { file: String, args: Vec<String> },
    /// `-e/--eval <code>` and the stdin script (`-`): evaluated as a CJS
    /// script in the global scope (Node's `-e` semantics).
    Eval { code: String, args: Vec<String> },
}

/// Entry point for `pir js <file|-> [args...]` (args exclude the `js`
/// subcommand itself). Returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    run_with_exec_argv(args, &[])
}

/// [`run`] with the Node options the process was started with (the
/// `pir [node-flags] <script>` respawn shape). They are not applied — this
/// runtime has no V8 flag set — but they are reported through
/// `process.execArgv`, which Node's own `test/common` uses to decide whether a
/// script still has to re-exec itself with its `// Flags:`.
pub fn run_with_exec_argv(args: &[String], exec_argv: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("--help")
        || args.first().map(String::as_str) == Some("-h")
    {
        println!("Usage: pir js <file|-> [args...]");
        println!("       pir js -e <code> [args...]");
        println!("       pir js --version");
        println!("       pir [node flags] <file> [args...]");
        println!();
        println!("Run a JavaScript/TypeScript file on pir's built-in QuickJS runtime with the");
        println!("node compatibility layer, as a spawned child process: real stdio, real argv,");
        println!("real exit codes. Arguments after <file> are passed to the script verbatim");
        println!("(process.argv[2..]).");
        println!();
        println!(
            "  <file>          entry module (`.js`/`.ts`; a relative path anchors at the cwd)"
        );
        println!("  -e, --eval      evaluate <code> as a CJS script (require bound to the cwd);");
        println!("                  process.argv is [execPath, ...args]");
        println!("  -               read the script from stdin (same CJS eval path)");
        println!(
            "  -v, --version   print the claimed node compatibility version ({NODE_COMPAT_VERSION})"
        );
        return 0;
    }
    // Node-version masquerade (see the module header): the shim routes
    // `node --version` / `node -v` here.
    if args.first().map(String::as_str) == Some("--version")
        || args.first().map(String::as_str) == Some("-v")
    {
        println!("{NODE_COMPAT_VERSION}");
        return 0;
    }

    let entry = match parse_entry(args) {
        Ok(entry) => entry,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };

    let argv = script_argv(&entry);

    // `process.execPath` points back at this binary, so
    // `child_process.spawn(process.execPath, ["js", other.js])` re-enters
    // the runner — the ecosystem pattern (mcp-cli, bridges) is built on it.
    // The routed node shim (the child_process table in `pir-node`) relies on
    // the same property: it spawns this binary with `js` prepended, never a
    // real node.
    let exec_path = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "pir".to_string());
    let mut full_argv: Vec<String> = Vec::with_capacity(argv.len() + 1);
    full_argv.push(exec_path.clone());
    full_argv.extend(argv);
    let exec_argv = exec_argv.to_vec();

    // rquickjs futures are !Send: run the whole script on a dedicated thread
    // with a current-thread tokio runtime + LocalSet (the extension actor
    // pattern, `extensions/handle/actor.rs`).
    let spawned = std::thread::Builder::new()
        .name("pir-js".to_owned())
        .stack_size(script_thread_stack())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("Error: failed to start the script executor: {error}");
                    return 1;
                }
            };
            let local = tokio::task::LocalSet::new();
            local.block_on(&runtime, async move {
                match entry {
                    Entry::File { file, .. } => {
                        let Some(script_path) = resolve_script_path(&file) else {
                            eprintln!("Error: Cannot find module '{file}'");
                            return 1;
                        };
                        run_script(script_path, full_argv, exec_path, exec_argv).await
                    }
                    Entry::Eval { code, .. } => {
                        run_eval(code, full_argv, exec_path, exec_argv).await
                    }
                }
            })
        });
    match spawned {
        Ok(handle) => match handle.join() {
            Ok(code) => code,
            Err(_) => {
                eprintln!("Error: script worker panicked");
                1
            }
        },
        Err(error) => {
            eprintln!("Error: failed to spawn the script worker: {error}");
            1
        }
    }
}

/// Parse the leading `pir js` options into an [`Entry`]. Only the leading
/// token(s) are inspected: everything after the entry is verbatim script
/// material (an unknown `--flag=value` after the file must reach the script,
/// and a leading one is treated as the file name, exactly as before).
fn parse_entry(args: &[String]) -> Result<Entry, String> {
    let Some(first) = args.first() else {
        return Err("Usage: pir js <file|-> [args...]".to_owned());
    };
    let rest = &args[1..];
    match first.as_str() {
        "-e" | "--eval" => {
            let Some(code) = rest.first() else {
                return Err(format!("Error: {first} requires a script argument"));
            };
            Ok(Entry::Eval {
                code: code.clone(),
                args: rest[1..].to_vec(),
            })
        }
        _ if first.starts_with("--eval=") => Ok(Entry::Eval {
            code: first["--eval=".len()..].to_owned(),
            args: rest.to_vec(),
        }),
        "-" => {
            let mut code = String::new();
            if let Err(error) = std::io::stdin().read_to_string(&mut code) {
                return Err(format!("Error: reading the stdin script failed: {error}"));
            }
            Ok(Entry::Eval {
                code,
                args: rest.to_vec(),
            })
        }
        // A leading `--` separates pir-level flags from script arguments
        // (Node convention); it never reaches process.argv.
        file => {
            let script_args: &[String] = if rest.first().map(String::as_str) == Some("--") {
                &rest[1..]
            } else {
                rest
            };
            Ok(Entry::File {
                file: file.to_owned(),
                args: script_args.to_vec(),
            })
        }
    }
}

/// Node CLI flags `pir` accepts and ignores when they precede a script.
///
/// They only steer V8/Node internals this runtime does not implement, yet
/// test harnesses and worker pools put them in front of the script whenever
/// they respawn `process.execPath`. An option not listed here is not claimed:
/// it falls back to the pi command line and keeps its existing error, so this
/// list can never silently swallow a real typo.
const IGNORED_NODE_BOOL_FLAGS: &[&str] = &[
    "--no-warnings",
    "--trace-warnings",
    "--throw-deprecation",
    "--trace-deprecation",
    "--no-deprecation",
    "--pending-deprecation",
    "--enable-source-maps",
    "--no-enable-source-maps",
    "--preserve-symlinks",
    "--preserve-symlinks-main",
    "--frozen-intrinsics",
    "--force-context-aware",
    "--zero-fill-buffers",
    "--abort-on-uncaught-exception",
    "--openssl-legacy-provider",
    "--use-openssl-ca",
    "--no-use-openssl-ca",
    "--interpreted-frames-native-stack",
];

/// Node flags above that consume the following argv token when no `=` value
/// is attached.
const IGNORED_NODE_VALUE_FLAGS: &[&str] = &[
    "--max-old-space-size",
    "--max-semi-space-size",
    "--stack-size",
    "--v8-pool-size",
    "--inspect-port",
    "--title",
    "--icu-data-dir",
    "--diagnostic-dir",
    "--redirect-warnings",
    "--env-file",
    "--conditions",
    "--disable-warning",
    "--heapsnapshot-signal",
    "--openssl-config",
    "--tls-cipher-list",
    "--unhandled-rejections",
    "--dns-result-order",
    "--report-directory",
    "--report-filename",
];

/// Flag families whose individual members are all no-ops here.
const IGNORED_NODE_FLAG_PREFIXES: &[&str] = &[
    "--inspect",
    "--experimental-",
    "--no-experimental-",
    "--trace-",
    "--report-",
];

/// Recognize Node's `spawn(process.execPath, [node-flags…, script, …])` shape
/// and rewrite it to the `pir js` argv shape (`[script, …args]`).
///
/// `None` unless every leading option is ignorable *and* the first positional
/// is an existing JavaScript/TypeScript file. A prompt, a pi subcommand, or
/// `--print`-style invocation therefore never turns into a script run.
pub fn node_style_invocation(args: &[String]) -> Option<(Vec<String>, Vec<String>)> {
    let mut index = 0;
    let mut exec_argv: Vec<String> = Vec::new();
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            // Node's end-of-options marker: the next token is the script.
            index += 1;
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            break;
        }
        let name = arg.split_once('=').map_or(arg, |(name, _)| name);
        if !ignored_node_flag(name) {
            return None;
        }
        exec_argv.push(args[index].clone());
        let inline_value = arg.len() > name.len();
        index += 1;
        if !inline_value && IGNORED_NODE_VALUE_FLAGS.contains(&name) {
            if let Some(value) = args.get(index) {
                exec_argv.push(value.clone());
            }
            index += 1;
        }
    }
    let script = args.get(index)?;
    if !is_script_path(script) {
        return None;
    }
    Some((exec_argv, args[index..].to_vec()))
}

fn ignored_node_flag(name: &str) -> bool {
    IGNORED_NODE_BOOL_FLAGS.contains(&name)
        || IGNORED_NODE_VALUE_FLAGS.contains(&name)
        || IGNORED_NODE_FLAG_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// `pir <path>` is a script run only when `<path>` exists and carries a
/// JavaScript/TypeScript extension; every other positional stays a prompt.
fn is_script_path(path: &str) -> bool {
    const EXTENSIONS: &[&str] = &["js", "mjs", "cjs", "ts", "mts", "cts"];
    let candidate = Path::new(path);
    candidate.is_file()
        && candidate
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// The script's `process.argv` tail (argv[0], the pir executable, is added by
/// the caller).
///
/// * file: `[file, ...args]` — argv[1] is the entry path (Node file mode).
/// * eval / stdin: `[...args]` — Node's `-e` shape: the code is not in argv.
fn script_argv(entry: &Entry) -> Vec<String> {
    match entry {
        Entry::File { file, args } => {
            let mut argv = Vec::with_capacity(args.len() + 1);
            argv.push(file.clone());
            argv.extend(args.iter().cloned());
            argv
        }
        Entry::Eval { args, .. } => args.clone(),
    }
}

/// Resolve the entry file: relative paths anchor at the cwd (Node
/// semantics), with the loader's `.js -> .ts` fallback so `pir js foo`
/// (or `pir js foo.js`) can pick up a TS source.
fn resolve_script_path(file: &str) -> Option<PathBuf> {
    let given = Path::new(file);
    let absolute = if given.is_absolute() {
        given.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(given)
    };
    let mut candidates = vec![absolute.clone()];
    if absolute.extension().and_then(|ext| ext.to_str()) == Some("js") {
        candidates.push(absolute.with_extension("ts"));
    } else if absolute.extension().is_none() {
        let mut ts = absolute.into_os_string();
        ts.push(".ts");
        candidates.push(PathBuf::from(ts));
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}

/// Build the script-mode runtime, run the entry module, drain the event
/// loop, and return the exit code. The runtime's `Drop` (which emits the
/// Node final `exit` event) runs before this returns on the success path —
/// exit listeners observe the code the process will carry, like Node.
async fn run_script(
    script_path: PathBuf,
    argv: Vec<String>,
    exec_path: String,
    exec_argv: Vec<String>,
) -> i32 {
    let config = ExtensionConfig {
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        mode: "script".to_string(),
        // Explicit injection: the standalone script runner must not inherit
        // the extension host's heap policy (see [`script_limits`]).
        limits: Some(script_limits()),
        // The standalone runner owns the whole process, so `process.exit(n)`
        // in the script is the intended way to set its exit code (the boot
        // overlay calls `__pir_host.exitProcess`). An extension, context-mode
        // or worker runtime does NOT get that bridge — see
        // `extensions::runtime::host_functions`.
        allow_process_exit: true,
        ..Default::default()
    };
    let runtime = match ExtensionRuntime::new(config).await {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Error: failed to start the script runtime: {error}");
            return 1;
        }
    };

    // Keep the supplied script directory and its nearest package boundary as
    // explicit module roots. Check the real file before reading the entry;
    // the target directory of a symlinked script is not an implicit grant.
    if let Some(dir) = script_path.parent() {
        runtime.allow_root(dir.to_path_buf());
    }
    runtime.allow_root(pir::extensions::runtime::package_entry_root(&script_path));
    let Some(script_path) = runtime.allowed_entry_path(&script_path) else {
        eprintln!(
            "Error: script entry is outside the allowed extension roots or not a real file: {}",
            script_path.display()
        );
        return 1;
    };

    // Script-mode overlay: argv/execPath/execArgv + real-stdio console +
    // process.exit.
    let boot = script_boot()
        .replace(
            "__PIR_SCRIPT_ARGV__",
            &serde_json::to_string(&argv).unwrap_or_else(|_| "[]".to_string()),
        )
        .replace(
            "__PIR_SCRIPT_EXEC_PATH__",
            &serde_json::to_string(&exec_path).unwrap_or_else(|_| "\"pir\"".to_string()),
        )
        .replace(
            "__PIR_SCRIPT_EXEC_ARGV__",
            &serde_json::to_string(&exec_argv).unwrap_or_else(|_| "[]".to_string()),
        );
    if let Err(error) = runtime.eval_boot(&boot).await {
        eprintln!("Error: script runtime boot failed: {error}");
        return 1;
    }

    let module_name = pir::extensions::loader::module_name_for(&script_path);

    // Top-level-await downgrade for the entry only: QuickJS rejects `await`
    // at module top level, so the entry is pre-transpiled with the async-IIFE
    // wrapper and seeded into the module cache. Imported modules keep native
    // ESM semantics; CJS entries keep their own loader path.
    if let Some(entry_js) = pir::extensions::loader::script_entry_module_js(&script_path) {
        if let Err(error) = runtime.seed_module_js(&module_name, &entry_js) {
            eprintln!("Error: script entry transpile failed: {error}");
            return 1;
        }
    }

    // Entry import + top-level execution. An uncaught module error prints
    // its stack on stderr and exits 1 (Node's uncaught-exception contract).
    match runtime.run_script(&module_name).await {
        Ok(Ok(())) => {}
        Ok(Err(stack)) => {
            eprintln!("{stack}");
            return 1;
        }
        Err(error) => {
            eprintln!("Error: {error}");
            return 1;
        }
    }

    finish(&runtime).await
}

/// `-e/--eval <code>` and the stdin script (`-`): Node's eval semantics on
/// the same script-mode runtime.
///
/// The code is wrapped in a function body and evaluated in the global scope —
/// Node's `-e` is CJS, i.e. also a function body, so top-level `return`,
/// `var` scoping and `require` behave the way an eval script expects. `require`
/// is the CJS bridge bound to the cwd (`__pirMakeRequire`), with `module` /
/// `exports` / `__filename` ("[eval]") / `__dirname` (".") defined the way
/// Node's eval scope defines them. `process.argv` is the injected
/// `[execPath, ...args]`.
async fn run_eval(
    code: String,
    argv: Vec<String>,
    exec_path: String,
    exec_argv: Vec<String>,
) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config = ExtensionConfig {
        cwd: cwd.clone(),
        mode: "script".to_string(),
        // Explicit injection: the standalone script runner must not inherit
        // the extension host's heap policy (see [`script_limits`]).
        limits: Some(script_limits()),
        // Same reasoning as the entry-module runner above: this process exists
        // only to run the script, so `process.exit(n)` must reach the host.
        allow_process_exit: true,
        ..Default::default()
    };
    let runtime = match ExtensionRuntime::new(config).await {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Error: failed to start the script runtime: {error}");
            return 1;
        }
    };
    // Eval is explicitly cwd-scoped, unlike extension runtimes. Bare npm
    // imports use the loader's separate, package-scoped resolution policy.
    runtime.allow_root(cwd.clone());

    // Script-mode overlay: argv/execPath/execArgv + real-stdio console +
    // process.exit (the overlay's `__pirScriptRun` module runner stays unused
    // here).
    let boot = script_boot()
        .replace(
            "__PIR_SCRIPT_ARGV__",
            &serde_json::to_string(&argv).unwrap_or_else(|_| "[]".to_string()),
        )
        .replace(
            "__PIR_SCRIPT_EXEC_PATH__",
            &serde_json::to_string(&exec_path).unwrap_or_else(|_| "\"pir\"".to_string()),
        )
        .replace(
            "__PIR_SCRIPT_EXEC_ARGV__",
            &serde_json::to_string(&exec_argv).unwrap_or_else(|_| "[]".to_string()),
        );
    if let Err(error) = runtime.eval_boot(&boot).await {
        eprintln!("Error: script runtime boot failed: {error}");
        return 1;
    }

    // The CJS eval scope (Node's -e): a cwd-bound require plus the module
    // carrier globals.
    let scope = format!(
        "globalThis.require = globalThis.__pirMakeRequire({dir});\n\
         globalThis.module = {{ exports: {{}} }};\n\
         globalThis.exports = globalThis.module.exports;\n\
         globalThis.__filename = \"[eval]\";\n\
         globalThis.__dirname = \".\";\n",
        dir = serde_json::to_string(&cwd.display().to_string())
            .unwrap_or_else(|_| "\".\"".to_string()),
    );
    if let Err(error) = runtime.eval_boot(&scope).await {
        eprintln!("Error: script runtime boot failed: {error}");
        return 1;
    }

    // Sync throws are reported as Node's uncaught-exception block: the
    // recorded stack on stderr, exit 1, pending timers abandoned.
    let wrapped = format!(
        "(() => {{ try {{\n{code}\n; return JSON.stringify({{ ok: true }});\n\
         }} catch (error) {{ return JSON.stringify({{ ok: false, stack: error && error.stack ? String(error.stack) : String(error) }}); }} }})()"
    );
    match runtime.eval_json(&wrapped).await {
        Ok(outcome) => {
            if outcome.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
                let stack = outcome
                    .get("stack")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Uncaught error");
                eprintln!("{stack}");
                return 1;
            }
        }
        Err(error) => {
            eprintln!("Error: {error}");
            return 1;
        }
    }

    finish(&runtime).await
}

/// Shared script/eval tail: drain the event loop, deliver (or report) the
/// unhandled rejections, and read the final exit code.
async fn finish(runtime: &ExtensionRuntime) -> i32 {
    // Event-loop drain: pending jobs, timers, in-flight fetch/child work.
    runtime.wait_idle().await;

    // Node's unhandled-rejection contract: a promise rejected with no handler
    // prints `Unhandled promise rejection: <stack>` on stderr and exits 1.
    // When the script installed its own `process.on("unhandledRejection")`
    // listener, the record is delivered to it instead (minimal tier: the
    // reason arrives as an Error carrying the recorded stack text, not the
    // original object).
    let rejections = runtime.take_unhandled_rejections().await;
    if !rejections.is_empty() {
        let has_listener: bool = runtime
            .eval_json(
                "JSON.stringify(typeof process === \"object\" && typeof process.listenerCount === \"function\" ? { n: process.listenerCount(\"unhandledRejection\") } : { n: 0 })",
            )
            .await
            .ok()
            .and_then(|value| {
                value
                    .get("n")
                    .and_then(serde_json::Value::as_u64)
                    .map(|n| n > 0)
            })
            .unwrap_or(false);
        if has_listener {
            for reason in &rejections {
                let payload = serde_json::to_string(reason)
                    .unwrap_or_else(|_| "\"promise rejection\"".to_string());
                let snippet =
                    format!("process.emit(\"unhandledRejection\", new Error({payload}), process);");
                let _ = runtime.eval_boot(&snippet).await;
            }
        } else {
            for reason in &rejections {
                eprintln!("Unhandled promise rejection: {reason}");
            }
            return 1;
        }
    }

    runtime.read_exit_code().await.unwrap_or(0)
}
