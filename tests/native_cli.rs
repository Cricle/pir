//! Integration tests for the standalone native commands: `pir grep`,
//! `pir sed`, `pir python` (native paths, exit codes, and the
//! `--no-fallback` / `PIR_NATIVE_FALLBACK` switch).
//!
//! The commands are spawned as real processes (`CARGO_BIN_EXE_pir`) because
//! their contract is process-level: argv in, stdio and exit code out.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

// ---------------------------------------------------------------------------
// sed
// ---------------------------------------------------------------------------

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
