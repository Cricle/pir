use super::*;

// ---------------------------------------------------------------------------
// Context files
// ---------------------------------------------------------------------------

/// Context-file candidates per directory, in upstream precedence order.
pub const CONTEXT_FILE_CANDIDATES: [&str; 5] = [
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

/// pir hardening: context files larger than this are skipped (upstream
/// `loadContextFileFromDir` reads without a cap).
pub const MAX_CONTEXT_FILE_BYTES: u64 = 1024 * 1024;

/// Load the first existing context file in `dir` (candidates in upstream order).
fn load_context_file_from_dir(dir: &Path) -> Option<(PathBuf, String)> {
    for filename in CONTEXT_FILE_CANDIDATES {
        let path = dir.join(filename);
        if !path.is_file() {
            continue;
        }
        // Skip oversized files before reading (pir hardening).
        if fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0) > MAX_CONTEXT_FILE_BYTES {
            continue;
        }
        if let Some(content) = read_text_file(&path) {
            return Some((path, content));
        }
    }
    None
}

/// Locate the git repository layout for `cwd` by walking up to the nearest `.git`.
struct GitPaths {
    /// Directory that owns the `.git` entry (worktree or main checkout root).
    repo_dir: PathBuf,
    /// Shared git dir (`.git` itself, or the worktree gitdir's `commondir` target).
    common_git_dir: PathBuf,
}

fn find_git_paths(cwd: &Path) -> Option<GitPaths> {
    let mut dir = cwd.to_path_buf();
    loop {
        let git_path = dir.join(".git");
        if git_path.exists() {
            if git_path.is_file() {
                if let Ok(content) = fs::read_to_string(&git_path) {
                    if let Some(target) = content.trim().strip_prefix("gitdir: ") {
                        let git_dir = dir.join(target.trim());
                        if !git_dir.join("HEAD").exists() {
                            return None;
                        }
                        let common_dir_path = git_dir.join("commondir");
                        let common_git_dir = if common_dir_path.exists() {
                            let rel = fs::read_to_string(&common_dir_path)
                                .ok()?
                                .trim()
                                .to_string();
                            git_dir.join(rel)
                        } else {
                            git_dir.clone()
                        };
                        return Some(GitPaths {
                            repo_dir: dir,
                            common_git_dir,
                        });
                    }
                }
            } else if git_path.is_dir() {
                if !git_path.join("HEAD").exists() {
                    return None;
                }
                return Some(GitPaths {
                    repo_dir: dir,
                    common_git_dir: git_path,
                });
            }
        }
        dir = dir.parent()?.to_path_buf();
    }
}

/// The main repo's context file that a nested linked worktree's own copy shadows:
/// both occupy the same logical repository scope, so loading both applies that
/// context twice. Returns the canonical path of the file to shadow, or `None`.
fn find_shadowed_context_file(cwd: &Path) -> Option<PathBuf> {
    let git_paths = find_git_paths(cwd)?;
    let common_git_dir = canonicalize_lossy(&git_paths.common_git_dir);
    let worktree_root = canonicalize_lossy(&git_paths.repo_dir);
    let main_repo_root = common_git_dir.parent()?.to_path_buf();
    // False for an ordinary repo (roots are equal). For a nested worktree the
    // worktree root must sit BELOW main_repo_root (upstream
    // resource-loader.ts:222 `worktreeRoot.startsWith(mainRepoRoot + sep)`).
    // `Path::starts_with` compares WHOLE COMPONENTS, which is exactly that
    // string-with-separator semantics: a sibling like `repo-main` does not
    // start_with `repo`, and the remainder of a genuine child never carries a
    // leading separator (strip_prefix is component-based, so checking the
    // rest for one can never succeed).
    let is_nested = worktree_root != main_repo_root && worktree_root.starts_with(&main_repo_root);
    if !is_nested {
        return None;
    }
    // The main worktree root must itself be checked out from the same repo (excludes
    // bare layouts like `proj/.bare` + `proj/main`).
    if canonicalize_lossy(&main_repo_root.join(".git")) != common_git_dir {
        return None;
    }
    let (worktree_context_path, _) = load_context_file_from_dir(&worktree_root)?;
    Some(main_repo_root.join(worktree_context_path.file_name()?))
}

/// Discover context files in upstream order: the global agent dir first, then every
/// ancestor of `cwd` from root-most down to `cwd` itself. Per directory the first
/// existing candidate (`AGENTS.override.md`, `AGENTS.md`, `AGENTS.MD`, `CLAUDE.md`,
/// `CLAUDE.MD`) wins. A linked worktree's copy shadows the main repo's file at the
/// same logical scope.
pub fn discover_context_files(cwd: &Path, global_dir: &Path) -> Vec<(PathBuf, String)> {
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();

    if let Some(global_context) = load_context_file_from_dir(global_dir) {
        seen.insert(canonicalize_lossy(&global_context.0));
        files.push(global_context);
    }

    let shadowed = find_shadowed_context_file(cwd).map(|path| canonicalize_lossy(&path));
    let mut ancestors: Vec<(PathBuf, String)> = Vec::new();
    let mut dir = cwd.to_path_buf();
    loop {
        if let Some(context_file) = load_context_file_from_dir(&dir) {
            // Compare the directory's real path + file name, not the file's:
            // a worktree AGENTS.md that symlinks to the main repo's file
            // canonicalizes to the shadowed path itself (upstream
            // resource-loader.ts `loadProjectContextFiles`).
            let dir_canonical = canonicalize_lossy(&dir);
            let is_shadowed = shadowed.as_ref().is_some_and(|shadow| {
                dir_canonical.join(context_file.0.file_name().unwrap_or_default()) == *shadow
            });
            let canonical = canonicalize_lossy(&context_file.0);
            if !is_shadowed && !seen.contains(&canonical) {
                seen.insert(canonical);
                ancestors.insert(0, context_file);
            }
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => break,
        }
    }
    files.extend(ancestors);
    files
}

// ---------------------------------------------------------------------------
// SYSTEM.md / APPEND_SYSTEM.md
// ---------------------------------------------------------------------------

/// Discover the system prompt override files. `SYSTEM.md` replaces the preamble;
/// `APPEND_SYSTEM.md` becomes the addendum section. The project file
/// (`<cwd>/.pi/<name>`) is preferred over the global one (`<global_dir>/<name>`).
///
/// The project file is trust-gated: when `project_trusted` is `false` it is
/// skipped, mirroring the upstream trust gate over
/// `.pi/{SYSTEM.md,APPEND_SYSTEM.md}`.
pub fn discover_system_md(
    cwd: &Path,
    global_dir: &Path,
    project_trusted: bool,
) -> (Option<String>, Option<String>) {
    let project_config_dir = cwd.join(".pi");
    let discover = |filename: &str| {
        let mut dirs: Vec<&Path> = Vec::new();
        if project_trusted {
            dirs.push(project_config_dir.as_path());
        }
        dirs.push(global_dir);
        for dir in dirs {
            let path = dir.join(filename);
            if path.is_file() {
                if let Some(content) = read_text_file(&path) {
                    return Some(content);
                }
            }
        }
        None
    };
    (discover("SYSTEM.md"), discover("APPEND_SYSTEM.md"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
