//! Integration tests for the standalone native commands: `pir grep`,
//! `pir sed`, `pir python` (native paths, exit codes, and the
//! `--no-fallback` / `PIR_NATIVE_FALLBACK` switch).
//!
//! The commands are spawned as real processes (`CARGO_BIN_EXE_pir`) because
//! their contract is process-level: argv in, stdio and exit code out.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The binary under test. `option_env!` keeps this compiling for a library-only
/// build (where cargo sets no `CARGO_BIN_EXE_pir`); every test then skips.
fn pir_bin() -> Option<&'static str> {
    option_env!("CARGO_BIN_EXE_pir")
}

fn run(args: &[&str]) -> Output {
    run_with_env(args, &[])
}

fn run_with_env(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(pir_bin().expect("pir binary"));
    command.args(args);
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().expect("pir binary should start")
}

/// Like [`run`], but feeds raw bytes through a pipe (no text-mode handling).
fn run_with_stdin(args: &[&str], input: &[u8]) -> Output {
    let mut command = Command::new(pir_bin().expect("pir binary"));
    command
        .args(args)
        .env_remove("PIR_NATIVE_FALLBACK")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("pir binary should start");
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(input).expect("write child stdin");
    }
    child.wait_with_output().expect("wait for pir binary")
}

/// The first candidate that can be spawned, mirroring the native fallback's
/// candidate order. `None` means the test's system tool is unavailable.
fn system_program(candidates: &[&'static str]) -> Option<&'static str> {
    candidates.iter().copied().find(|program| {
        Command::new(program)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    })
}

fn run_program_with_stdin(program: &str, args: &[&str], input: &[u8]) -> Option<Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    {
        let mut stdin = child.stdin.take()?;
        stdin.write_all(input).ok()?;
    }
    child.wait_with_output().ok()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n")
}

/// A unique temp directory removed on drop.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "pir-native-cli-{label}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("temp dir");
        Self { path }
    }

    fn join(&self, relative: &str) -> PathBuf {
        self.path.join(relative)
    }

    fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("temp subdir");
        }
        std::fs::write(&path, contents).expect("write fixture");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A path as the native commands print it (forward slashes).
fn displayed(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn skip_without_binary() -> bool {
    if pir_bin().is_none() {
        eprintln!("skipping: CARGO_BIN_EXE_pir is unavailable in this build");
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------

#[test]
fn grep_native_recursive_search_reports_matches_and_zero_exit() {
    if skip_without_binary() {
        return;
    }
    let dir = TempDir::new("grep-native");
    dir.write("a.txt", "alpha\nneedle\nomega\n");
    dir.write("sub/b.txt", "x\nneedle\n");

    let output = run(&[
        "grep",
        "-n",
        "needle",
        dir.path.to_str().expect("utf-8 temp path"),
        "--no-fallback",
    ]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let mut lines: Vec<String> = stdout(&output).lines().map(str::to_owned).collect();
    lines.sort();
    assert_eq!(
        lines,
        vec![
            format!("{}/a.txt:2:needle", displayed(&dir.path)),
            format!("{}/sub/b.txt:2:needle", displayed(&dir.path)),
        ]
    );

    // No match: exit 1, no stdout.
    let none = run(&[
        "grep",
        "-n",
        "does-not-exist",
        dir.path.to_str().expect("utf-8 temp path"),
        "--no-fallback",
    ]);
    assert_eq!(none.status.code(), Some(1), "stderr: {}", stderr(&none));
    assert_eq!(stdout(&none), "");
}

#[test]
fn grep_single_file_line_numbers_and_count() {
    if skip_without_binary() {
        return;
    }
    let dir = TempDir::new("grep-single");
    let file = dir.write("one.txt", "a\nfoo\nb\nfoo\n");
    let path = file.to_str().expect("utf-8 temp path");

    let numbered = run(&["grep", "-n", "foo", path, "--no-fallback"]);
    assert_eq!(numbered.status.code(), Some(0));
    assert_eq!(stdout(&numbered), "2:foo\n4:foo\n");

    let counted = run(&["grep", "-c", "foo", path, "--no-fallback"]);
    assert_eq!(counted.status.code(), Some(0));
    assert_eq!(stdout(&counted), "2\n");
}

#[test]
fn grep_no_fallback_rejects_unsupported_option() {
    if skip_without_binary() {
        return;
    }
    let dir = TempDir::new("grep-nofallback");
    let file = dir.write("one.txt", "foo\n");

    let output = run(&[
        "grep",
        "-v",
        "foo",
        file.to_str().expect("utf-8 temp path"),
        "--no-fallback",
    ]);
    assert_eq!(output.status.code(), Some(2), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "");
}

#[test]
fn grep_environment_switch_disables_fallback() {
    if skip_without_binary() {
        return;
    }
    let dir = TempDir::new("grep-env");
    let file = dir.write("one.txt", "foo\n");
    let path = file.to_str().expect("utf-8 temp path");

    // `-v` is outside the native subset: with the switch off it falls back to
    // the system grep, with it on it is rejected with grep's error code.
    let disabled = run_with_env(
        &["grep", "-v", "foo", path],
        &[("PIR_NATIVE_FALLBACK", "0")],
    );
    assert_eq!(
        disabled.status.code(),
        Some(2),
        "stderr: {}",
        stderr(&disabled)
    );
    assert_eq!(stdout(&disabled), "");
}

#[test]
fn global_flags_before_grep_keep_include_and_exclude() {
    if skip_without_binary() {
        return;
    }
    let dir = TempDir::new("grep-global-flags");
    dir.write("match.rs", "needle\n");
    dir.write("match.txt", "needle\n");
    let path = dir.path.to_str().expect("utf-8 temp path");

    // `--include` is stripped by `preprocess_args` before clap, so the native
    // command must recover it from the raw argv even though the global flag
    // makes the subcommand no longer the first token.
    let included = run(&[
        "--verbose",
        "grep",
        "--include=*.rs",
        "needle",
        path,
        "--no-fallback",
    ]);
    assert_eq!(
        included.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&included)
    );
    assert_eq!(
        stdout(&included),
        format!("{}/match.rs:needle\n", displayed(&dir.path))
    );

    let excluded = run(&[
        "--verbose",
        "grep",
        "--exclude=*.rs",
        "needle",
        path,
        "--no-fallback",
    ]);
    assert_eq!(
        excluded.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&excluded)
    );
    assert_eq!(
        stdout(&excluded),
        format!("{}/match.txt:needle\n", displayed(&dir.path))
    );

    // A global flag's value that happens to spell the subcommand must not be
    // taken for the subcommand itself (`--model grep grep …`).
    let model_value = run(&[
        "--model",
        "grep",
        "grep",
        "--include=*.rs",
        "needle",
        path,
        "--no-fallback",
    ]);
    assert_eq!(
        model_value.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&model_value)
    );
    assert_eq!(
        stdout(&model_value),
        format!("{}/match.rs:needle\n", displayed(&dir.path))
    );
}

#[test]
fn extension_flag_value_that_spells_grep_is_not_taken_for_the_subcommand() {
    if skip_without_binary() {
        return;
    }
    let dir = TempDir::new("grep-ext-flag");
    dir.write("match.rs", "needle\n");
    let path = dir.path.to_str().expect("utf-8 temp path");

    // `--ext-flag` is an extension-registered (unknown to clap) long flag;
    // `preprocess_args` strips it together with the following `grep` as its
    // value. The old raw-argv scan took that *value* for the subcommand and
    // ran grep with pattern `grep`, silently searching the wrong target. The
    // native dispatch must fall back to the clap argv here, which still has
    // the real subcommand's pattern (`needle`).
    let output = run(&[
        "--ext-flag",
        "grep",
        "grep",
        "--include=*.rs",
        "needle",
        path,
        "--no-fallback",
    ]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!("{}/match.rs:needle\n", displayed(&dir.path))
    );
}

// ---------------------------------------------------------------------------
// sed
// ---------------------------------------------------------------------------

#[test]
fn global_flags_before_sed_keep_original_argv() {
    if skip_without_binary() {
        return;
    }
    let dir = TempDir::new("sed-global-flags");
    let file = dir.write("one.txt", "a\nb\n");

    // `-n` is normalized to `--name` for clap; the raw argv still carries the
    // sed spelling and must win.
    let output = run(&[
        "--verbose",
        "sed",
        "-n",
        "s/a/A/p",
        file.to_str().expect("utf-8 temp path"),
        "--no-fallback",
    ]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "A\n");
}

#[test]
fn sed_native_quiet_substitute_prints_only_replaced_lines() {
    if skip_without_binary() {
        return;
    }
    let dir = TempDir::new("sed-substitute");
    let file = dir.write("one.txt", "a\nb\nc\n");

    let output = run(&[
        "sed",
        "-n",
        "s/a/A/p",
        file.to_str().expect("utf-8 temp path"),
        "--no-fallback",
    ]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "A\n");
}

#[test]
fn sed_native_in_place_rewrites_the_file() {
    if skip_without_binary() {
        return;
    }
    let dir = TempDir::new("sed-in-place");
    let file = dir.write("one.txt", "foo\nbar\n");
    let path = file.to_str().expect("utf-8 temp path");

    let output = run(&["sed", "-i", "s/foo/baz/", path, "--no-fallback"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert_eq!(
        std::fs::read_to_string(&file).expect("read rewritten file"),
        "baz\nbar\n"
    );
}

#[test]
fn sed_missing_file_exits_two_but_processes_the_rest() {
    if skip_without_binary() {
        return;
    }
    let dir = TempDir::new("sed-missing");
    let file = dir.write("one.txt", "foo\n");
    let missing = dir.join("missing.txt");

    let output = run(&[
        "sed",
        "s/foo/bar/",
        missing.to_str().expect("utf-8 temp path"),
        file.to_str().expect("utf-8 temp path"),
        "--no-fallback",
    ]);
    assert_eq!(output.status.code(), Some(2), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "bar\n");
    assert!(stderr(&output).contains("can't read"));
}

#[test]
fn sed_non_utf8_stdin_preserves_bytes_through_fallback() {
    if skip_without_binary() {
        return;
    }
    let Some(sed) = system_program(&["sed"]) else {
        eprintln!("skipping: no system sed found");
        return;
    };
    // The native subset reads stdin as UTF-8; these bytes are not, so the
    // system sed must receive them verbatim through a pipe instead of
    // inheriting the drained stdin (which silently produced empty output).
    let input: &[u8] = b"\xff\xfea\n";
    let expected =
        run_program_with_stdin(sed, &["s/a/b/"], input).expect("system sed should start");

    let output = run_with_stdin(&["sed", "s/a/b/"], input);
    assert_eq!(
        output.status.code(),
        expected.status.code(),
        "stderr: {}",
        stderr(&output)
    );
    assert_eq!(
        output.stdout, expected.stdout,
        "fallback must preserve the raw stdin bytes"
    );
    assert_eq!(output.stdout, b"\xff\xfeb\n".as_slice());
}

// ---------------------------------------------------------------------------
// python
// ---------------------------------------------------------------------------

#[test]
fn python_native_command_outputs_result() {
    if skip_without_binary() {
        return;
    }
    let output = run(&["python", "-c", "print(1+1)", "--no-fallback"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "2\n");
}

#[test]
fn python_native_script_sets_argv_and_exit_code() {
    if skip_without_binary() {
        return;
    }
    let dir = TempDir::new("python-script");
    let script = dir.write(
        "script.py",
        "import sys\nprint(sys.argv[0])\nprint(sys.argv[1])\n",
    );
    let script_arg = script.to_str().expect("utf-8 temp path");

    let output = run(&["python", script_arg, "hello", "--no-fallback"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines, vec![script_arg, "hello"]);

    // An uncaught exception exits 1 with the traceback on stderr.
    let failing = run(&["python", "-c", "raise ValueError('boom')", "--no-fallback"]);
    assert_eq!(failing.status.code(), Some(1));
    assert!(stderr(&failing).contains("ValueError"));
}

#[test]
fn python_future_import_without_docstring_compiles() {
    if skip_without_binary() {
        return;
    }
    let dir = TempDir::new("python-future-plain");
    let script = dir.write(
        "script.py",
        "from __future__ import annotations\nvalues: list[int] = [1, 2]\nprint(len(values))\n",
    );

    let output = run(&[
        "python",
        script.to_str().expect("utf-8 temp path"),
        "--no-fallback",
    ]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "2\n");
}

#[test]
fn python_future_import_after_docstring_compiles() {
    if skip_without_binary() {
        return;
    }
    let dir = TempDir::new("python-future-doc");
    let script = dir.write(
        "script.py",
        "\"\"\"Module docstring.\"\"\"\nfrom __future__ import annotations\nprint(__doc__)\n",
    );

    let output = run(&[
        "python",
        script.to_str().expect("utf-8 temp path"),
        "--no-fallback",
    ]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    // The preamble must follow the docstring, not displace it.
    assert_eq!(stdout(&output), "Module docstring.\n");
}

#[test]
fn python_multiple_future_imports_compile() {
    if skip_without_binary() {
        return;
    }
    let dir = TempDir::new("python-future-multi");
    let script = dir.write(
        "script.py",
        "from __future__ import annotations\nfrom __future__ import division\nprint(7 / 2)\n",
    );

    let output = run(&[
        "python",
        script.to_str().expect("utf-8 temp path"),
        "--no-fallback",
    ]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "3.5\n");
}

#[test]
fn python_plain_script_without_future_is_unchanged() {
    if skip_without_binary() {
        return;
    }
    let dir = TempDir::new("python-no-future");
    let script = dir.write("script.py", "print('plain')\n");

    let output = run(&[
        "python",
        script.to_str().expect("utf-8 temp path"),
        "--no-fallback",
    ]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "plain\n");
}

#[test]
fn python_non_utf8_stdin_does_not_run_empty_script() {
    if skip_without_binary() {
        return;
    }
    let Some(python) = system_program(&["python3", "python"]) else {
        eprintln!("skipping: no system python3/python found");
        return;
    };
    // Invalid UTF-8 source: the old fallback inherited an already-drained
    // stdin and ran an empty program, exiting 0 with no output.
    let input: &[u8] = b"\xff\xfe\n";
    let expected = run_program_with_stdin(python, &[], input).expect("system python should start");

    let output = run_with_stdin(&["python"], input);
    assert_eq!(
        output.status.code(),
        expected.status.code(),
        "stderr: {}",
        stderr(&output)
    );
    assert!(
        !(output.status.code() == Some(0) && output.stdout.is_empty()),
        "non-UTF-8 stdin must not be silently treated as an empty program"
    );
}
