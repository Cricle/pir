//! The Rust env client (`src/env_client/`) against a **real** daemon process
//! (`pir env-daemon`, the built-in upstream protocol-v1 daemon) — the same
//! end-to-end shape `tests/env_daemon_js_shim.rs` covers for the JS client, plus
//! the tool layer's local-vs-remote selection:
//!
//! * `the_client_drives_the_local_daemon_end_to_end`: lazy start, the sync line,
//!   the `hello` handshake, the whole file family, `listFiles`/`listDir`,
//!   `exec` with streamed output, `close()`.
//! * `file_info_size_is_bytes_not_characters_or_code_units`: the multibyte
//!   payload's reported `size` is its byte length, not its character or UTF-16
//!   code unit count.
//! * `exec_reports_timeouts_aborts_and_non_zero_exit_codes`.
//! * `watch_reports_changes_and_stops_on_close`.
//! * `the_tools_stay_local_without_a_configured_environment`: the regression
//!   lock — with nothing configured, `read`/`write`/`edit`/`bash` operate on the
//!   local file system exactly as before the indirection layer existed.
//! * `an_installed_environment_moves_the_tools_to_the_daemon`: the same four
//!   tools, with an environment explicitly installed, operate on the daemon's
//!   machine (the daemon's cwd), and `clear_override()` restores the local path.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pir::agent::tool_execution::ToolContext;
use pir::ai::types::AbortSignal;
use pir::env_client::{
    clear_override, configured_remote_env, install_override, Context, ExecCommand, ExecOptions,
    ExecutionErrorCode, ListFilesOptions, RemoteExecutionEnv, RemoteExecutionEnvOptions,
    WatchChange, WatchOptions, WatchTarget,
};
use serde_json::json;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn pir_binary() -> &'static str {
    env!("CARGO_BIN_EXE_pir")
}

/// The daemon argv these tests drive: `[<pir>, "env-daemon"]` — the shape
/// `local_daemon()` reports for the real binary, and the prefix the client
/// completes to `<pir> env-daemon serve --token <hex>`.
///
/// The image path comes from `CARGO_BIN_EXE_pir`, never from
/// `local_daemon()`/`current_exe()`: in an integration test `current_exe()` is
/// the **test harness** (`target/debug/deps/env_client_e2e-*.exe`), which does
/// not speak the protocol — the handshake would fail only after the 60 s start
/// timeout (see `local_daemon()`'s contract note). The `env-daemon` argument is
/// mandatory for the same reason it is in `local_daemon()`: pir has no `serve`
/// subcommand, so an argv without it starts an agent run instead of a daemon.
fn daemon_argv() -> Vec<String> {
    vec![pir_binary().to_string(), "env-daemon".to_string()]
}

/// A connection to the local daemon (`<pir> env-daemon`).
fn local_environment(cwd: Option<String>) -> RemoteExecutionEnv {
    local_environment_with_watch(cwd, WatchOptions::default())
}

fn local_environment_with_watch(cwd: Option<String>, watch: WatchOptions) -> RemoteExecutionEnv {
    let mut options = RemoteExecutionEnvOptions::new(
        "pi-env:e2e",
        pir::env_client::Connection::argv(daemon_argv()),
    );
    options.cwd = cwd;
    options.watch = watch;
    RemoteExecutionEnv::new(options)
}

/// A unique temporary directory for one test.
struct Tree(std::path::PathBuf);

impl Tree {
    fn new(name: &str) -> Tree {
        let path = std::env::temp_dir().join(format!(
            "pir-env-e2e-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create the test tree");
        Tree(path)
    }

    fn path(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }

    fn root(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }

    /// A path with forward slashes, for the shell commands the bash tool runs.
    fn slash_path(&self, name: &str) -> String {
        self.path(name).replace('\\', "/")
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.0.join(name)).unwrap_or_default()
    }

    fn exists(&self, name: &str) -> bool {
        self.0.join(name).exists()
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run one core tool by name (the registry `register_core_tools` builds).
async fn run_tool(
    name: &str,
    args: serde_json::Value,
    cwd: &str,
) -> pir::agent::tool_batch::ToolResult {
    let mut registry = pir::agent::tool_execution::ToolRegistry::new();
    pir::coding_agent::tools::register_core_tools(&mut registry);
    let tool = registry.get(name).expect("a registered core tool");
    let context = ToolContext::new(cwd);
    tool.run(&args, &context).await
}

// ---------------------------------------------------------------------------
// The client against a real daemon
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_client_drives_the_local_daemon_end_to_end() {
    // The daemon command shape first: a missing `env-daemon` argument would
    // turn every assertion below into a 60 s handshake timeout.
    assert_eq!(
        daemon_argv().last().map(String::as_str),
        Some("env-daemon"),
        "the daemon subcommand must be part of the argv"
    );
    assert!(
        std::path::Path::new(&daemon_argv()[0]).is_file(),
        "the test image exists: {}",
        daemon_argv()[0]
    );
    let tree = Tree::new("client");
    let environment = local_environment(Some(tree.root()));
    // The handshake happens on the first request (lazy start).
    let info = environment.info().await.expect("hello");
    assert_eq!(info.protocol, 1);
    let session = environment
        .connection
        .session()
        .await
        .expect("a session id");
    assert!(session > 0, "session ids count up from 1");

    // The file family, in the order the JS shim's probe uses it:
    // createTempDir → writeFile → readTextFile → fileInfo → listFiles → listDir
    // → rename → truncate → remove.
    let dir = environment
        .create_temp_dir("pir-env-e2e-", &Context::default())
        .await
        .expect("createTempDir");
    assert!(!dir.is_empty());
    let file = format!("{dir}/probe.txt");
    environment
        .write_file(
            &file,
            "hello from the client".as_bytes(),
            &Context::default(),
        )
        .await
        .expect("writeFile");
    let text = environment
        .read_text_file(&file, &Context::default())
        .await
        .expect("readTextFile");
    assert_eq!(text, "hello from the client");
    let bytes = environment
        .read_binary_file(&file, &Context::default())
        .await
        .expect("readBinaryFile");
    assert_eq!(bytes, b"hello from the client");
    let listing = environment
        .list_files(&dir, &ListFilesOptions::default(), &Context::default())
        .await
        .expect("listFiles");
    assert!(
        listing
            .entries
            .iter()
            .any(|entry| entry.path == "probe.txt" && entry.kind == "file"),
        "listFiles records: {:?}",
        listing.entries
    );
    assert!(!listing.truncated);
    let entries = environment
        .list_dir(&dir, &Context::default())
        .await
        .expect("listDir");
    assert!(
        entries
            .iter()
            .any(|entry| entry.name == "probe.txt"
                && entry.kind == pir::env_client::FileKind::File),
        "listDir entries: {entries:?}"
    );
    // FileInfo reports the size and a modification time.
    let file_info = environment
        .file_info(&file, &Context::default())
        .await
        .expect("fileInfo");
    assert_eq!(file_info.size, 21);
    assert!(file_info.mtime_ms > 0.0);
    assert!(environment
        .exists(&file, &Context::default())
        .await
        .expect("exists"));
    assert!(!environment
        .exists(&format!("{dir}/missing.txt"), &Context::default())
        .await
        .expect("exists"));
    // A missing file is `not_found`, with the path the daemon reported.
    let error = environment
        .read_text_file(&format!("{dir}/missing.txt"), &Context::default())
        .await
        .expect_err("a missing file");
    assert_eq!(error.code, pir::env_client::FileErrorCode::NotFound);
    // Rename, truncate, append and remove.
    let renamed = format!("{dir}/renamed.txt");
    environment
        .rename_file(&file, &renamed, &Context::default())
        .await
        .expect("renameFile");
    environment
        .append_file(&renamed, " +more".as_bytes(), &Context::default())
        .await
        .expect("appendFile");
    assert_eq!(
        environment
            .read_text_file(&renamed, &Context::default())
            .await
            .expect("readTextFile"),
        "hello from the client +more"
    );
    environment
        .truncate_file(&renamed, 5, &Context::default())
        .await
        .expect("truncateFile");
    assert_eq!(
        environment
            .read_text_file(&renamed, &Context::default())
            .await
            .expect("readTextFile"),
        "hello"
    );
    environment
        .flush_file(&renamed, &Context::default())
        .await
        .expect("flushFile");
    environment
        .remove(&dir, true, true, &Context::default())
        .await
        .expect("remove");
    assert!(!environment
        .exists(&dir, &Context::default())
        .await
        .expect("exists"));
    // A chunked write (over the 512 KiB request chunk) keeps its bytes in order.
    let big = Tree::new("chunks");
    let environment = local_environment(Some(big.root()));
    let mut content = Vec::with_capacity(1_200_000);
    for index in 0..1_200_000u32 {
        content.push((index % 251) as u8);
    }
    environment
        .write_file(&big.path("big.bin"), &content, &Context::default())
        .await
        .expect("a chunked writeFile");
    let read = environment
        .read_binary_file(&big.path("big.bin"), &Context::default())
        .await
        .expect("readBinaryFile");
    assert_eq!(read, content, "chunked writes and chunked reads round-trip");
    environment.connection.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_info_size_is_bytes_not_characters_or_code_units() {
    // The ASCII probe above cannot tell the three units apart (21 bytes, 21
    // chars and 21 UTF-16 code units all agree), so a `size` computed with
    // `str::chars()` or `encode_utf16()` would still pass it. This payload
    // keeps the counts distinct — 10 bytes, 6 chars, 7 code units — so the
    // assertion below is only satisfiable by a byte count.
    let tree = Tree::new("bytesize");
    let payload = "h\u{e8}llo\u{1f642}";
    assert_eq!(payload.len(), 10, "the payload's UTF-8 byte length");
    assert_eq!(payload.chars().count(), 6, "the payload's character count");
    assert_eq!(
        payload.encode_utf16().count(),
        7,
        "the payload's UTF-16 code unit count"
    );
    let environment = local_environment(Some(tree.root()));
    let file = tree.path("multibyte.txt");
    environment
        .write_file(&file, payload.as_bytes(), &Context::default())
        .await
        .expect("writeFile");
    let file_info = environment
        .file_info(&file, &Context::default())
        .await
        .expect("fileInfo");
    assert_eq!(
        file_info.size,
        payload.len() as u64,
        "the reported size is the byte length, not a count of characters or code units"
    );
    assert_eq!(
        environment
            .read_text_file(&file, &Context::default())
            .await
            .expect("readTextFile"),
        payload,
        "the text round-trips as written"
    );
    environment.connection.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_reports_timeouts_aborts_and_non_zero_exit_codes() {
    let tree = Tree::new("exec");
    let environment = local_environment(Some(tree.root()));
    // A shell string runs through the daemon's own shell resolution (Git Bash
    // or `where bash.exe` on Windows, `/bin/bash` on POSIX).
    let text = Arc::new(Mutex::new(String::new()));
    let captured = text.clone();
    let result = environment
        .exec(
            &ExecCommand::Shell("echo out && echo err 1>&2 && exit 3".to_string()),
            &ExecOptions {
                cwd: Some(tree.root()),
                on_output: Some(Arc::new(move |chunk: &str, _info| {
                    if let Ok(mut guard) = captured.lock() {
                        guard.push_str(chunk);
                    }
                })),
                ..Default::default()
            },
            &Context::default(),
        )
        .await
        .expect("exec");
    assert_eq!(result.exit_code, 3, "the shell's own exit code");
    let captured = text.lock().map(|guard| guard.clone()).unwrap_or_default();
    assert!(captured.contains("out"), "stdout streamed: {captured:?}");
    assert!(captured.contains("err"), "stderr streamed: {captured:?}");

    // A timeout is an `ExecutionError` with code `timeout`.
    let slow = if cfg!(windows) {
        "ping -n 30 127.0.0.1 > nul"
    } else {
        "sleep 30"
    };
    let error = environment
        .exec(
            &ExecCommand::Shell(slow.to_string()),
            &ExecOptions {
                cwd: Some(tree.root()),
                timeout_secs: Some(0.4),
                ..Default::default()
            },
            &Context::default(),
        )
        .await
        .expect_err("a timeout");
    assert_eq!(error.code, ExecutionErrorCode::Timeout);
    assert_eq!(error.message, "timeout:0.4", "upstream's message form");

    // An abort cancels the request; the daemon answers `aborted`.
    let signal = AbortSignal::new();
    let context = Context::new(Some(signal.clone()));
    let abort_after = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        signal.abort();
    });
    let error = environment
        .exec(
            &ExecCommand::Shell(slow.to_string()),
            &ExecOptions {
                cwd: Some(tree.root()),
                ..Default::default()
            },
            &context,
        )
        .await
        .expect_err("an abort");
    abort_after.await.expect("the abort task");
    assert_eq!(error.code, ExecutionErrorCode::Aborted);

    // An invalid timeout is refused before any request goes out.
    let error = environment
        .exec(
            &ExecCommand::Shell("echo x".to_string()),
            &ExecOptions {
                cwd: Some(tree.root()),
                timeout_secs: Some(0.0),
                ..Default::default()
            },
            &Context::default(),
        )
        .await
        .expect_err("a zero timeout");
    assert_eq!(error.code, ExecutionErrorCode::Timeout);
    assert_eq!(
        error.message,
        "Invalid timeout: must be a finite number of seconds"
    );
    // A missing program is a `spawn_error`.
    let error = environment
        .exec(
            &ExecCommand::Argv(vec!["definitely-not-a-program-xyz".to_string()]),
            &ExecOptions {
                cwd: Some(tree.root()),
                ..Default::default()
            },
            &Context::default(),
        )
        .await
        .expect_err("a missing program");
    assert_eq!(error.code, ExecutionErrorCode::SpawnError);
    environment.connection.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_reports_changes_and_stops_on_close() {
    let tree = Tree::new("watch");
    let environment = local_environment_with_watch(
        Some(tree.root()),
        WatchOptions {
            poll_interval_ms: Some(200),
            ..Default::default()
        },
    );
    let changes = Arc::new(Mutex::new(Vec::<WatchChange>::new()));
    let seen = changes.clone();
    let watcher = environment
        .watch(
            &[WatchTarget {
                path: tree.root(),
                recursive: true,
                exclude: None,
            }],
            Arc::new(move |change| {
                if let Ok(mut guard) = seen.lock() {
                    guard.push(change);
                }
            }),
            &Context::default(),
        )
        .await
        .expect("watch coverage");
    let mode = watcher.mode();
    assert!(mode == "native" || mode == "polling", "mode: {mode}");
    // Coverage is established, so a change after `open` must be reported.
    std::fs::write(tree.path("watched.txt"), "x").expect("write the watched file");
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut paths: Vec<String> = Vec::new();
    while std::time::Instant::now() < deadline {
        if let Ok(guard) = changes.lock() {
            for change in guard.iter() {
                if let WatchChange::Paths(found) = change {
                    paths.extend(found.iter().cloned());
                }
            }
        }
        if !paths.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !paths.is_empty(),
        "a change after coverage must be reported: {paths:?}"
    );
    // `close()` cancels the request and waits for the daemon to finish it.
    watcher.close().await;
    environment.connection.close();
}

// ---------------------------------------------------------------------------
// The tool layer: local by default, remote only when configured
// ---------------------------------------------------------------------------

/// One test covers both halves because the override is process-wide: the local
/// behavior is asserted first, then with an environment installed, then again
/// after `clear_override()`. Running them in one test keeps the global switch
/// from leaking into the other tests of this binary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tools_stay_local_by_default_and_move_when_configured() {
    let local = Tree::new("tool-local");
    let remote = Tree::new("tool-remote");
    std::fs::write(local.path("same.txt"), "local content\n").expect("write the local file");
    std::fs::write(remote.path("same.txt"), "remote content\n").expect("write the remote file");
    std::fs::write(local.path("edit-me.txt"), "local old text\n")
        .expect("write the local edit file");
    std::fs::write(remote.path("edit-me.txt"), "remote old text\n")
        .expect("write the remote edit file");

    // Nothing configured: the tools must not consult any environment at all.
    assert!(
        std::env::var("PI_ENV_SSH_HOST")
            .unwrap_or_default()
            .is_empty(),
        "the test process must not have PI_ENV_SSH_HOST set"
    );
    assert!(
        configured_remote_env().is_none(),
        "the default environment is local"
    );
    let result = run_tool("read", json!({ "path": "same.txt" }), &local.root()).await;
    assert!(!result.is_error, "read: {}", result.content);
    assert!(
        result.content.contains("local content"),
        "the read tool reads the local file: {:?}",
        result.content
    );
    let result = run_tool(
        "write",
        json!({ "path": "written.txt", "content": "written locally\n" }),
        &local.root(),
    )
    .await;
    assert!(!result.is_error, "write: {}", result.content);
    assert_eq!(local.read("written.txt"), "written locally\n");
    assert!(
        !remote.exists("written.txt"),
        "the remote tree is untouched"
    );
    let result = run_tool(
        "edit",
        json!({
            "path": "edit-me.txt",
            "edits": [{ "oldText": "local old text", "newText": "local new text" }],
        }),
        &local.root(),
    )
    .await;
    assert!(!result.is_error, "edit: {}", result.content);
    assert_eq!(local.read("edit-me.txt"), "local new text\n");
    assert_eq!(
        remote.read("edit-me.txt"),
        "remote old text\n",
        "the remote tree is untouched"
    );
    // The bash tool runs the command locally: it writes into the local tree.
    let result = run_tool(
        "bash",
        json!({
            "command": format!("echo shell-local > \"{}\"", local.slash_path("shell-local.txt")),
        }),
        &local.root(),
    )
    .await;
    assert!(!result.is_error, "bash: {}", result.content);
    assert_eq!(
        local.read("shell-local.txt"),
        "shell-local\n",
        "the bash tool ran on this machine"
    );
    assert!(!remote.exists("shell-local.txt"));

    // An explicitly installed environment moves all four tools to the daemon.
    let environment = Arc::new(local_environment(Some(remote.root())));
    environment.info().await.expect("the daemon answers hello");
    install_override(environment.clone());
    assert_eq!(
        pir::env_client::tool_env_description().as_deref(),
        Some("remote:pi-env:e2e")
    );
    let result = run_tool("read", json!({ "path": "same.txt" }), &local.root()).await;
    assert!(!result.is_error, "remote read: {}", result.content);
    assert!(
        result.content.contains("remote content"),
        "the read tool now reads through the daemon: {:?}",
        result.content
    );
    let result = run_tool(
        "write",
        json!({ "path": "written.txt", "content": "written remotely\n" }),
        &local.root(),
    )
    .await;
    assert!(!result.is_error, "remote write: {}", result.content);
    assert_eq!(remote.read("written.txt"), "written remotely\n");
    assert_eq!(
        local.read("written.txt"),
        "written locally\n",
        "the local file keeps what the local write put there"
    );
    let result = run_tool(
        "edit",
        json!({
            "path": "edit-me.txt",
            "edits": [{ "oldText": "remote old text", "newText": "remote new text" }],
        }),
        &local.root(),
    )
    .await;
    assert!(!result.is_error, "remote edit: {}", result.content);
    assert_eq!(remote.read("edit-me.txt"), "remote new text\n");
    assert_eq!(
        local.read("edit-me.txt"),
        "local new text\n",
        "the local tree keeps what the local edit wrote"
    );
    let result = run_tool(
        "bash",
        json!({ "command": "echo shell-remote > shell.txt; pwd" }),
        &local.root(),
    )
    .await;
    assert!(!result.is_error, "remote bash: {}", result.content);
    assert_eq!(
        local.read("shell-local.txt"),
        "shell-local\n",
        "the local shell file keeps what the local run wrote"
    );
    assert!(
        remote.exists("shell.txt"),
        "the command ran in the daemon's own cwd: {}",
        result.content
    );
    assert!(
        !local.exists("shell.txt"),
        "the remote run did not write into the local tree"
    );
    assert!(
        result.content.contains("shell-remote") || !result.content.is_empty(),
        "the remote shell's output came back: {:?}",
        result.content
    );
    // A path outside the daemon's cwd is refused by the tool layer's
    // confinement rule (the remote form of `workspace_path`).
    let result = run_tool(
        "read",
        json!({ "path": local.path("same.txt") }),
        &local.root(),
    )
    .await;
    assert!(
        result.is_error && result.content.contains("escapes the workspace root"),
        "an absolute path outside the remote root is refused: {:?}",
        result.content
    );
    let result = run_tool("read", json!({ "path": "../same.txt" }), &local.root()).await;
    assert!(
        result.is_error && result.content.contains("escapes the workspace root"),
        "a parent-directory component is refused: {:?}",
        result.content
    );

    // Clearing the override restores the local path exactly.
    clear_override();
    assert!(configured_remote_env().is_none());
    let result = run_tool("read", json!({ "path": "same.txt" }), &local.root()).await;
    assert!(
        result.content.contains("local content"),
        "the local path is back: {:?}",
        result.content
    );
    let result = run_tool(
        "write",
        json!({ "path": "after.txt", "content": "local again\n" }),
        &local.root(),
    )
    .await;
    assert!(!result.is_error, "write after clearing: {}", result.content);
    assert_eq!(local.read("after.txt"), "local again\n");
    assert!(!remote.exists("after.txt"));
    // The environment is the caller's to close (its daemon exits with it).
    environment.connection.close();
}

// ---------------------------------------------------------------------------
// The opened-handle readers against the real daemon
// (`openBinaryReader` / `openTextLineReader` / `openDirReader`)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_line_reader_reassembles_a_character_split_across_reads() {
    // `readLine` reads the file in 64 KiB positional reads (`LINE_CHUNK`), so a
    // multi-byte character can straddle two of them: the streaming decoder must
    // hold the leading byte back until its continuation bytes arrive, and the
    // line still has to come back whole. 65 535 ASCII bytes put the boundary
    // inside the 3-byte snowman (the first read returns exactly 65 536 bytes,
    // ending after its first byte).
    let tree = Tree::new("linechunk");
    let text = format!("{}\u{2603}\nsecond", "a".repeat(65_535));
    std::fs::write(tree.path("lines.txt"), text.as_bytes()).expect("write lines.txt");
    let environment = local_environment(Some(tree.root()));
    let context = Context::default();
    let mut reader = environment
        .open_text_line_reader("lines.txt", &context)
        .await
        .expect("openTextLineReader");

    let line = reader
        .read_line(&context)
        .await
        .expect("readLine")
        .expect("the first line");
    assert_eq!(
        line.text.chars().count(),
        65_536,
        "the character split across the chunk boundary is reassembled"
    );
    assert!(line.text.ends_with('\u{2603}'), "the snowman is whole");
    assert!(line.terminated, "the line ends with a newline");
    let line = reader
        .read_line(&context)
        .await
        .expect("readLine")
        .expect("the second line");
    assert_eq!(line.text, "second");
    assert!(
        !line.terminated,
        "a last line without a newline is reported unterminated"
    );
    assert!(reader
        .read_line(&context)
        .await
        .expect("readLine at the end")
        .is_none());
    reader.close().await;
    environment.connection.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_a_reader_is_idempotent_and_later_calls_report_it() {
    // Upstream's readers are closed by their `close()` and every call after
    // that is refused rather than sent to the daemon; a second `close()` is not
    // an error (and asks the daemon nothing again).
    let tree = Tree::new("readerclose");
    std::fs::write(tree.path("f.txt"), "one\ntwo\n").expect("write f.txt");
    std::fs::create_dir_all(tree.path("sub")).expect("mkdir sub");
    let environment = local_environment(Some(tree.root()));
    let context = Context::default();

    let path = environment.resolve("f.txt").await.expect("resolve f.txt");
    let mut reader = environment
        .open_text_line_reader("f.txt", &context)
        .await
        .expect("openTextLineReader");
    let line = reader
        .read_line(&context)
        .await
        .expect("readLine")
        .expect("the first line");
    assert_eq!(line.text, "one");
    reader.close().await;
    reader.close().await;
    let error = reader
        .read_line(&context)
        .await
        .expect_err("a closed line reader refuses readLine");
    assert_eq!(error.code, pir::env_client::FileErrorCode::Invalid);
    assert_eq!(error.message, "Text line reader is closed");
    assert_eq!(error.path.as_deref(), Some(path.as_str()));

    let mut reader = environment
        .open_binary_reader("f.txt", false, &context)
        .await
        .expect("openBinaryReader");
    reader.close().await;
    reader.close().await;
    for error in [
        reader
            .read(0, 1, &context)
            .await
            .expect_err("a closed binary reader refuses read"),
        reader
            .info(&context)
            .await
            .expect_err("a closed binary reader refuses fstat"),
    ] {
        assert_eq!(error.code, pir::env_client::FileErrorCode::Invalid);
        assert_eq!(error.message, "Binary reader is closed");
        assert_eq!(error.path.as_deref(), Some(path.as_str()));
    }

    let sub = environment.resolve("sub").await.expect("resolve sub");
    let mut reader = environment
        .open_dir_reader("sub", &context)
        .await
        .expect("openDirReader");
    reader.close().await;
    reader.close().await;
    let error = reader
        .next(1, &context)
        .await
        .expect_err("a closed directory reader refuses next");
    assert_eq!(error.code, pir::env_client::FileErrorCode::Invalid);
    assert_eq!(error.message, "Directory reader is closed");
    assert_eq!(error.path.as_deref(), Some(sub.as_str()));
    environment.connection.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_line_reader_drops_a_leading_bom_and_stops_when_aborted() {
    // The streaming decoder drops a byte-order mark at the start of the file
    // only (upstream decodes the whole file with `TextDecoder`), and the abort
    // signal is checked before every request, so an aborted reader reports
    // `aborted` instead of asking the daemon for a line.
    let tree = Tree::new("bomabort");
    std::fs::write(tree.path("bom.txt"), "\u{feff}x\ny\n").expect("write bom.txt");
    std::fs::write(tree.path("mid.txt"), "a\u{feff}b\n").expect("write mid.txt");
    let environment = local_environment(Some(tree.root()));
    let context = Context::default();

    for (name, expected) in [("bom.txt", "x"), ("mid.txt", "a\u{feff}b")] {
        let mut reader = environment
            .open_text_line_reader(name, &context)
            .await
            .expect("openTextLineReader");
        let line = reader
            .read_line(&context)
            .await
            .expect("readLine")
            .expect("the first line");
        assert_eq!(line.text, expected, "{name}");
        reader.close().await;
    }

    let signal = AbortSignal::new();
    let mut reader = environment
        .open_text_line_reader("bom.txt", &Context::new(Some(signal.clone())))
        .await
        .expect("a live signal opens");
    signal.abort();
    let error = reader
        .read_line(&Context::new(Some(signal.clone())))
        .await
        .expect_err("an aborted signal refuses readLine");
    assert_eq!(error.code, pir::env_client::FileErrorCode::Aborted);
    assert_eq!(error.message, "aborted");
    reader.close().await;

    // An already-aborted signal refuses the open itself, before the daemon is
    // asked for a handle.
    let aborted = Context::new(Some(signal.clone()));
    let Err(error) = environment.open_text_line_reader("bom.txt", &aborted).await else {
        panic!("an aborted signal refuses openTextLineReader");
    };
    assert_eq!(error.code, pir::env_client::FileErrorCode::Aborted);
    assert_eq!(
        error.path.as_deref(),
        Some(
            environment
                .resolve("bom.txt")
                .await
                .expect("resolve")
                .as_str()
        )
    );
    environment.connection.close();
}

// ---------------------------------------------------------------------------
// F1: connection liveness and teardown races
//
// A mock daemon (this test binary re-invoked through a wrapper that turns the
// client-generated `serve --token <token>` suffix into environment variables)
// answers `hello`, records every `ping` it receives, and can go silent on
// purpose. It speaks the shared `pir::env_daemon::frame` codec, so the client
// under test runs its real reader/writer/pending paths.
// ---------------------------------------------------------------------------

fn mock_env_append(dir: &std::path::Path, name: &str, bytes: &[u8]) {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(name))
        .expect("open the mock capture file");
    file.write_all(bytes).expect("capture bytes");
}

/// The mock daemon loop: enabled by `PIR_MOCK_ENV_DAEMON=1`; its token,
/// capture directory and mode come from the wrapper script's environment.
/// `reply` answers every request, `stall` answers the handshake and then
/// swallows everything (a daemon that went silent).
#[test]
fn mock_env_daemon_child() {
    if std::env::var("PIR_MOCK_ENV_DAEMON").as_deref() != Ok("1") {
        return;
    }
    use pir::env_daemon::frame::{self, Frame, PING, REQUEST, RESULT};
    use std::io::Write as _;

    let token = std::env::var("PIR_MOCK_ENV_TOKEN").unwrap_or_default();
    let dir = std::path::PathBuf::from(std::env::var("PIR_MOCK_ENV_DIR").unwrap_or_default());
    let mode = std::env::var("PIR_MOCK_ENV_MODE").unwrap_or_default();
    let stdout = std::io::stdout();
    {
        let mut out = stdout.lock();
        writeln!(out, "PI-ENV {token}").expect("sync line");
        out.flush().expect("sync line flush");
    }
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut answered_hello = false;
    while let Ok(Some(request)) = frame::read_frame(&mut input) {
        match request.kind {
            PING => mock_env_append(&dir, "pings.log", b"ping\n"),
            REQUEST => {
                let op = request
                    .json
                    .get("op")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if op == "hello" && !answered_hello {
                    answered_hello = true;
                    if mode == "slow-hello" {
                        // Hold the handshake answer so a start is observably in
                        // flight (see the pending-handshake test).
                        std::thread::sleep(Duration::from_millis(1500));
                    }
                    let mut out = stdout.lock();
                    frame::write_frame(
                        &mut out,
                        &Frame::new(
                            RESULT,
                            request.id,
                            json!({
                                "protocol": 1,
                                "version": "mock-env-daemon",
                                "os": std::env::consts::OS,
                                "arch": std::env::consts::ARCH,
                                "home": dir.to_string_lossy(),
                                "separator": std::path::MAIN_SEPARATOR.to_string(),
                                "tmpdir": dir.to_string_lossy(),
                                "cwd": dir.to_string_lossy(),
                                "pid": 99,
                                "features": [],
                            }),
                        ),
                    )
                    .expect("hello reply");
                } else if mode != "stall" {
                    mock_env_append(&dir, "requests.log", format!("{op}\n").as_bytes());
                    let mut out = stdout.lock();
                    frame::write_frame(&mut out, &Frame::new(RESULT, request.id, json!({})))
                        .expect("result reply");
                }
            }
            _ => {}
        }
    }
}

/// A wrapper that turns the client-generated `serve --token <token>` suffix
/// into `PIR_MOCK_ENV_TOKEN` for the mock daemon child.
fn write_mock_env_wrapper(dir: &std::path::Path, mode: &str) -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("current test executable");
    #[cfg(windows)]
    {
        let wrapper = dir.join("mock-env-daemon.bat");
        let script = format!(
            "@echo off\r\nset \"PIR_MOCK_ENV_TOKEN=%~3\"\r\nset \"PIR_MOCK_ENV_DIR={}\"\r\nset \"PIR_MOCK_ENV_MODE={mode}\"\r\nset \"PIR_MOCK_ENV_DAEMON=1\"\r\n\"{}\" mock_env_daemon_child --exact --nocapture\r\n",
            dir.display(),
            exe.display()
        );
        std::fs::write(&wrapper, script).expect("write the daemon wrapper");
        wrapper
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let wrapper = dir.join("mock-env-daemon.sh");
        let script = format!(
            "#!/bin/sh\nPIR_MOCK_ENV_TOKEN=\"$3\"\nPIR_MOCK_ENV_DIR=\"{}\"\nPIR_MOCK_ENV_MODE=\"{mode}\"\nPIR_MOCK_ENV_DAEMON=1\nexport PIR_MOCK_ENV_TOKEN PIR_MOCK_ENV_DIR PIR_MOCK_ENV_MODE PIR_MOCK_ENV_DAEMON\nexec \"{}\" mock_env_daemon_child --exact --nocapture\n",
            dir.display(),
            exe.display()
        );
        std::fs::write(&wrapper, script).expect("write the daemon wrapper");
        let mut permissions = std::fs::metadata(&wrapper)
            .expect("wrapper metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&wrapper, permissions).expect("wrapper mode");
        wrapper
    }
}

/// A connection to the mock daemon in `dir`, with test-sized liveness timings.
fn mock_env_connection(
    dir: &std::path::Path,
    mode: &str,
    ping_interval: Duration,
    silence_limit: Duration,
) -> pir::env_client::Connection {
    let wrapper = write_mock_env_wrapper(dir, mode);
    // WSL/overlayfs can briefly report ETXTBSY when exec races the close of
    // the just-written wrapper; let the write settle before spawning it.
    std::thread::sleep(Duration::from_millis(20));
    #[cfg(windows)]
    let argv = vec![
        "cmd".to_owned(),
        "/c".to_owned(),
        wrapper.to_string_lossy().into_owned(),
    ];
    #[cfg(unix)]
    let argv = vec![wrapper.to_string_lossy().into_owned()];
    pir::env_client::Connection::new(
        pir::env_client::ConnectionOptions::argv(argv).with_liveness(ping_interval, silence_limit),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_connection_still_pings_and_declares_a_stalled_daemon_lost() {
    // 1. Idle pings: after the handshake nothing is sent for a while, and the
    //    writer must still ping the daemon (it exits after 30 s of client
    //    silence).
    let tree = Tree::new("idle-pings");
    let connection = mock_env_connection(
        &tree.0,
        "reply",
        Duration::from_millis(100),
        Duration::from_secs(30),
    );
    connection.info().await.expect("the mock handshake");
    tokio::time::sleep(Duration::from_millis(700)).await;
    let pings = std::fs::read_to_string(tree.0.join("pings.log")).unwrap_or_default();
    assert!(
        pings.lines().count() >= 2,
        "an idle connection must keep pinging; the daemon saw {}",
        pings.lines().count()
    );
    connection.close();

    // 2. A silent daemon: no ping and no reply. The writer's silence limit
    //    (test-sized here) must tear the session down and fail the request
    //    instead of leaving it parked forever.
    let stalled = Tree::new("silent-daemon");
    let connection = mock_env_connection(
        &stalled.0,
        "stall",
        Duration::from_millis(100),
        Duration::from_millis(300),
    );
    connection.info().await.expect("the mock handshake");
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        connection.request(
            "lstat",
            json!({ "path": "." }),
            pir::env_client::RequestOptions::default(),
        ),
    )
    .await
    .expect("a stalled daemon must not hang the request")
    .expect_err("a stalled daemon fails the request");
    assert!(error.lost, "the error is a lost connection: {error:?}");
    connection.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_is_not_handed_out_before_the_handshake_answers() {
    // A session published before its `hello` reply is processed could be
    // handed to a concurrent `session()` (or request) that then skips the
    // protocol check; `close()` would miss it in that window too. The mock
    // holds its hello reply, so the second `session()` must wait for the start
    // gate (it used to return the published-but-unready session immediately).
    let tree = Tree::new("pending-handshake");
    let connection = mock_env_connection(
        &tree.0,
        "slow-hello",
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    let first = {
        let connection = connection.clone();
        tokio::spawn(async move { connection.session().await })
    };
    // Let the client spawn the daemon and send hello; the mock holds it 1.5 s.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let early = tokio::time::timeout(Duration::from_millis(500), connection.session()).await;
    assert!(
        early.is_err(),
        "session() returned before the handshake answered: {early:?}"
    );
    let first_id = first
        .await
        .expect("the first session task joins")
        .expect("the handshake answers");
    let second_id = connection
        .session()
        .await
        .expect("the ready session is reused");
    assert_eq!(
        first_id, second_id,
        "the session handed out after the handshake is the one that started"
    );
    connection.close();
}

#[test]
fn a_request_registered_during_teardown_fails_instead_of_hanging() {
    // The decision seam behind `Connection::send`: a registration that lands
    // after teardown's drain must be taken back, so the caller observes
    // `lost` instead of awaiting a reply that can never arrive.
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;

    use pir::env_client::connection::register_pending;

    let pending: Mutex<HashMap<u32, &'static str>> = Mutex::new(HashMap::new());
    let live = AtomicBool::new(true);
    assert_eq!(register_pending(&pending, &live, 7, "reply"), None);
    assert_eq!(pending.lock().expect("pending").get(&7), Some(&"reply"));

    // Teardown already swapped `live` off (and drained an empty map): the
    // registration is handed straight back and leaves no sender behind.
    let live = AtomicBool::new(false);
    assert_eq!(
        register_pending(&pending, &live, 8, "late-reply"),
        Some("late-reply")
    );
    assert!(
        !pending.lock().expect("pending").contains_key(&8),
        "a request registered during teardown must not stay in the pending map"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversized_request_tears_the_session_down() {
    let tree = Tree::new("oversized");
    let connection = mock_env_connection(
        &tree.0,
        "reply",
        Duration::from_millis(100),
        Duration::from_secs(5),
    );
    connection.info().await.expect("the mock handshake");
    // A payload at the frame cap makes the encoded frame larger than
    // `MAX_FRAME`, so `write_frame` refuses it. That must tear the session
    // down: the pending request fails `lost` instead of hanging forever on a
    // frame the daemon will never see.
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        connection.request(
            "write",
            json!({ "path": "unreachable.bin" }),
            pir::env_client::RequestOptions {
                payload: vec![0u8; pir::env_client::MAX_FRAME],
                ..Default::default()
            },
        ),
    )
    .await
    .expect("an unencodable request must not hang")
    .expect_err("the oversized request fails");
    assert!(error.lost, "the error is a lost connection: {error:?}");
    assert!(
        error.message.contains("frame limit"),
        "the encode failure tears the session down (not a silence timeout): {error:?}"
    );
    // The dead session is not reused: a later request starts a fresh one.
    let info = tokio::time::timeout(Duration::from_secs(10), connection.info())
        .await
        .expect("a new session starts")
        .expect("the fresh handshake");
    assert_eq!(info.protocol, 1);
    connection.close();
}
