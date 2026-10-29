//! G11: worktree context-file shadowing with symlinked directories (upstream
//! f1b2e77f5 — `loadProjectContextFiles` compares the owning directory's
//! realpath + file name, not the file's own realpath).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use pir::config::settings::discover_context_files;

// ---------------------------------------------------------------------------
// Temp-dir helper (no tempfile dependency)
// ---------------------------------------------------------------------------

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-g11-shadow-{}-{}-{}",
            name,
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        fs::write(&path, contents).expect("write file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Create `link -> target`; returns `false` when the platform refuses (Windows
/// without developer mode / privileges) so the caller can skip-and-pass.
fn try_symlink(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        if target.is_dir() {
            std::os::windows::fs::symlink_dir(target, link).is_ok()
        } else {
            std::os::windows::fs::symlink_file(target, link).is_ok()
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        false
    }
}

/// Layout a nested linked worktree the way `git worktree add <main>/wt <name>`
/// does: the worktree's `.git` file points at `<main>/.git/worktrees/<name>`
/// (with `HEAD` + `commondir`), and `commondir` points back at `<main>/.git`.
/// `worktree_dir` must live INSIDE `main_dir` (pir shadows only nested
/// worktrees).
fn layout_worktree(main_dir: &Path, worktree_name: &str, worktree_dir: &Path) {
    let git_dir = main_dir.join(".git").join("worktrees").join(worktree_name);
    fs::create_dir_all(&git_dir).expect("create worktree git dir");
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/feat\n").expect("write HEAD");
    // Relative from the worktree git dir back to the common git dir.
    fs::write(git_dir.join("commondir"), "../..").expect("write commondir");

    fs::create_dir_all(worktree_dir).expect("create worktree checkout");
    let git_file = worktree_dir.join(".git");
    fs::write(
        &git_file,
        format!(
            "gitdir: {}",
            git_dir
                .canonicalize()
                .expect("canonicalize gitdir")
                .display()
        ),
    )
    .expect("write .git file");
}

fn empty_global_dir(name: &str) -> TempDir {
    TempDir::new(name)
}

// ---------------------------------------------------------------------------
// Shadowing
// ---------------------------------------------------------------------------

#[test]
fn nested_worktree_shadows_main_repo_copy() {
    let root = TempDir::new("nested");
    let main = root.path().join("main");
    fs::create_dir_all(main.join(".git")).expect("create .git");
    fs::write(main.join(".git").join("HEAD"), "ref: refs/heads/main\n").expect("write HEAD");
    root.write("main/AGENTS.md", "main repo context\n");

    let wt = main.join("wt-feat");
    layout_worktree(&main, "feat", &wt);
    root.write("main/wt-feat/AGENTS.md", "worktree context\n");

    let global = empty_global_dir("nested-global");
    let files = discover_context_files(&wt, global.path());
    // The worktree's own copy loads; the main repo's copy is shadowed.
    assert_eq!(files.len(), 1, "{files:?}");
    assert_eq!(files[0].0, wt.join("AGENTS.md"));
    assert_eq!(files[0].1, "worktree context\n");
}

#[test]
fn symlinked_worktree_context_file_still_shadows_main() {
    let root = TempDir::new("symlinked");
    let main = root.path().join("main");
    fs::create_dir_all(main.join(".git")).expect("create .git");
    fs::write(main.join(".git").join("HEAD"), "ref: refs/heads/main\n").expect("write HEAD");
    root.write("main/AGENTS.md", "main repo context\n");

    let wt = main.join("wt-feat");
    layout_worktree(&main, "feat", &wt);
    // The worktree's AGENTS.md symlinks to the main repo's file: its own
    // realpath IS the shadowed path, but the directory-based comparison still
    // loads the worktree copy and shadows the main repo's.
    let link = wt.join("AGENTS.md");
    if !try_symlink(&main.join("AGENTS.md"), &link) {
        eprintln!("skipping: symlink creation unavailable on this platform");
        return;
    }

    let global = empty_global_dir("symlinked-global");
    let files = discover_context_files(&wt, global.path());
    assert_eq!(files.len(), 1, "{files:?}");
    assert_eq!(files[0].0, link);
}

#[test]
fn sibling_worktree_is_not_shadowed() {
    let root = TempDir::new("sibling");
    let main = root.path().join("main");
    fs::create_dir_all(main.join(".git")).expect("create .git");
    fs::write(main.join(".git").join("HEAD"), "ref: refs/heads/main\n").expect("write HEAD");
    root.write("main/AGENTS.md", "main repo context\n");

    // A worktree OUTSIDE the main repo (upstream: main repo not an ancestor).
    // Shadowing does not apply (findShadowedContextFile requires the main repo
    // to be an ancestor of the worktree) — and the ancestor walk never visits
    // the main repo at all, so only the worktree's own file loads (upstream
    // loadProjectContextFiles walks cwd's ancestors only).
    let wt = root.path().join("wt-sibling");
    layout_worktree(&main, "sib", &wt);
    root.write("wt-sibling/AGENTS.md", "sibling worktree context\n");

    let global = empty_global_dir("sibling-global");
    let files = discover_context_files(&wt, global.path());
    assert_eq!(
        files,
        vec![(
            wt.join("AGENTS.md"),
            "sibling worktree context\n".to_string()
        )],
        "only the worktree's own copy loads; the shadowed main file is not an ancestor"
    );
}

#[test]
fn plain_repo_is_not_shadowed() {
    let root = TempDir::new("plain");
    let main = root.path().join("main");
    fs::create_dir_all(main.join(".git")).expect("create .git");
    fs::write(main.join(".git").join("HEAD"), "ref: refs/heads/main\n").expect("write HEAD");
    root.write("main/AGENTS.md", "main repo context\n");

    let global = empty_global_dir("plain-global");
    let files = discover_context_files(&main, global.path());
    assert_eq!(files.len(), 1, "{files:?}");
    assert_eq!(files[0].0, main.join("AGENTS.md"));
}
