//! Auto-discovered `.agents/skills` directories (upstream
//! `addAutoDiscoveredResources` + `collectAncestorAgentsSkillDirs`): the
//! user-global `~/.agents/skills` and the trust-gated `.agents/skills`
//! directories of `cwd` and its ancestors, up to and including the enclosing
//! git repository root. Locks the discovery order against the other skill
//! roots, first-wins dedupe, the trust gate, and the silent handling of
//! missing directories.
//!
//! Standalone test binary (the `tests/suites/` and `g_*` harnesses belong to
//! another work stream). The fixtures always place a `.git` marker at the top
//! of the tree so the ancestor walk stops inside the fixture: on Windows the
//! temp directory sits under the real user profile, whose `.agents/skills`
//! would otherwise be discovered.

use std::fs;
use std::path::{Path, PathBuf};

use pir::coding_agent::resources::{
    collect_ancestor_agents_skill_dirs, DiscoveredResources, ResourceDiscoveryOptions,
};

fn unique_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pir-agents-skills-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// One skill directory with a `SKILL.md` whose frontmatter name is the
/// directory name.
fn write_skill(parent: &Path, name: &str) {
    let dir = parent.join(name);
    fs::create_dir_all(&dir).expect("skill dir");
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {name}\n---\nBody\n"),
    )
    .expect("write skill");
}

struct Layout {
    root: PathBuf,
    home: PathBuf,
    agent_dir: PathBuf,
    cwd: PathBuf,
}

/// `<root>/.git` (a directory) marks the git repository root so the ancestor
/// walk stops at `<root>`; the cwd sits two levels below it
/// (`<root>/repo/project`), giving three `.agents/skills` ancestor levels.
fn layout(tag: &str) -> Layout {
    let root = unique_dir(tag);
    fs::create_dir_all(root.join(".git")).expect("git marker");
    let home = root.join("home");
    let agent_dir = root.join("agent");
    let cwd = root.join("repo").join("project");
    fs::create_dir_all(&home).expect("home dir");
    fs::create_dir_all(&agent_dir).expect("agent dir");
    fs::create_dir_all(&cwd).expect("cwd");
    Layout {
        root,
        home,
        agent_dir,
        cwd,
    }
}

/// Discover with the layout's explicit home directory: never reads the
/// ambient `$HOME`.
fn discover(layout: &Layout, trusted: bool) -> DiscoveredResources {
    DiscoveredResources::discover_with_home(
        &ResourceDiscoveryOptions {
            cwd: &layout.cwd,
            agent_dir: &layout.agent_dir,
            project_trusted: trusted,
            cli_skill_paths: &[],
            cli_prompt_paths: &[],
            no_skills: false,
            no_prompt_templates: false,
        },
        Some(layout.home.as_path()),
    )
}

fn skill_names(resources: &DiscoveredResources) -> Vec<String> {
    resources
        .skills
        .iter()
        .map(|skill| skill.name.clone())
        .collect()
}

/// The skill tree behind most tests: the project `.pi/skills` dir, the three
/// ancestor `.agents/skills` levels (cwd, `repo`, git root), the user
/// `<agentDir>/skills` dir, and the user-global `~/.agents/skills` dir.
fn fill_tree(layout: &Layout) {
    write_skill(&layout.cwd.join(".pi").join("skills"), "project-pi");
    write_skill(&layout.cwd.join(".agents").join("skills"), "ancestor-cwd");
    write_skill(
        &layout.root.join("repo").join(".agents").join("skills"),
        "ancestor-repo",
    );
    write_skill(
        &layout.root.join(".agents").join("skills"),
        "ancestor-gitroot",
    );
    write_skill(&layout.agent_dir.join("skills"), "user-agent");
    write_skill(&layout.home.join(".agents").join("skills"), "user-agents");
}

/// The discovery order is the upstream `resourcePrecedenceRank` order:
/// project `.pi/skills`, project ancestor `.agents/skills` (cwd outward),
/// `<agentDir>/skills`, then `~/.agents/skills`. Codex/Claude-style
/// `.agents/skills` dirs never shadow the `.pi` dir, and the user-global dir
/// never shadows the agent dir.
#[test]
fn agents_skills_are_discovered_in_the_upstream_precedence_order() {
    let layout = layout("order");
    fill_tree(&layout);

    let resources = discover(&layout, true);
    assert_eq!(
        skill_names(&resources),
        vec![
            "project-pi".to_string(),
            "ancestor-cwd".to_string(),
            "ancestor-repo".to_string(),
            "ancestor-gitroot".to_string(),
            "user-agent".to_string(),
            "user-agents".to_string(),
        ],
        "project <cwd>/.pi/skills, then ancestors cwd->up, then <agentDir>/skills, then ~/.agents/skills"
    );
    assert!(
        resources.diagnostics.is_empty(),
        "a fully present tree has no diagnostics: {:?}",
        resources.diagnostics
    );
    // `.agents/skills` never feeds the prompt-template scan (upstream only
    // auto-discovers skills there).
    assert!(
        resources.prompt_templates.is_empty(),
        "no prompt templates from .agents: {:?}",
        resources.prompt_templates
    );
}

/// The trust gate: an untrusted project contributes neither `<cwd>/.pi/skills`
/// nor the ancestor `.agents/skills` dirs, while the user scope (including
/// `~/.agents/skills`) stays discoverable — upstream never trust-gates the
/// user-global dir.
#[test]
fn trust_gates_project_ancestors_but_never_the_user_agents_dir() {
    let layout = layout("trust");
    fill_tree(&layout);

    let resources = discover(&layout, false);
    assert_eq!(
        skill_names(&resources),
        vec!["user-agent".to_string(), "user-agents".to_string()],
        "an untrusted project contributes nothing, the user scope still does"
    );
    assert!(
        resources.diagnostics.is_empty(),
        "nothing was dropped silently: {:?}",
        resources.diagnostics
    );

    // Same tree, trusted: everything loads (see the order test). The trust
    // flag is the only difference.
    let trusted = discover(&layout, true);
    assert_eq!(trusted.skills.len(), 6, "trusted discovers all six skills");
}

/// `collectAncestorAgentsSkillDirs` semantics: cwd first, then outward, up to
/// and including the git repository root (a `.git` directory or file), and the
/// filesystem root when no `.git` exists anywhere above.
#[test]
fn ancestor_walk_stops_at_the_git_repo_root() {
    let layout = layout("walk");
    let expected = vec![
        layout.cwd.join(".agents").join("skills"),
        layout.root.join("repo").join(".agents").join("skills"),
        layout.root.join(".agents").join("skills"),
    ];
    assert_eq!(
        collect_ancestor_agents_skill_dirs(&layout.cwd),
        expected,
        "cwd first, then outward, git root inclusive"
    );

    // A `.git` FILE (a linked worktree / submodule) stops the walk too.
    let root = unique_dir("walk-file");
    let worktree = root.join("worktree");
    let cwd = worktree.join("sub");
    fs::create_dir_all(&cwd).expect("cwd");
    fs::write(worktree.join(".git"), "gitdir: /elsewhere\n").expect("git file");
    assert_eq!(
        collect_ancestor_agents_skill_dirs(&cwd),
        vec![
            cwd.join(".agents").join("skills"),
            worktree.join(".agents").join("skills"),
        ],
        "a .git file is a repo root as well"
    );

    // Without any `.git`, the walk runs to the filesystem root (inclusive).
    let root = unique_dir("walk-none");
    let cwd = root.join("a").join("b");
    fs::create_dir_all(&cwd).expect("cwd");
    let dirs = collect_ancestor_agents_skill_dirs(&cwd);
    let mut fs_root = cwd.clone();
    while let Some(parent) = fs_root.parent() {
        if parent == fs_root {
            break;
        }
        fs_root = parent.to_path_buf();
    }
    assert_eq!(dirs.first(), Some(&cwd.join(".agents").join("skills")));
    assert_eq!(
        dirs.last(),
        Some(&fs_root.join(".agents").join("skills")),
        "the filesystem root closes the walk"
    );
    let expected_levels = cwd.ancestors().count();
    assert_eq!(
        dirs.len(),
        expected_levels,
        "every ancestor level up to the fs root is present"
    );
}

/// The user-global `~/.agents/skills` is filtered out of the project ancestor
/// list (upstream `resolve(dir) !== resolve(userAgentsSkillsDir)`), so a cwd
/// inside `$HOME` consumes it exactly once, at the user position: the earlier
/// `<agentDir>/skills` root wins a same-name collision, and the dir is not
/// loaded a second time as a project ancestor.
#[test]
fn user_agents_dir_is_not_double_counted_when_cwd_is_under_home() {
    let root = unique_dir("dedupe");
    fs::create_dir_all(root.join(".git")).expect("git marker");
    let home = root.join("home");
    let agent_dir = home.join("agent");
    let cwd = home.join("work").join("project");
    fs::create_dir_all(&cwd).expect("cwd");
    fs::create_dir_all(&agent_dir).expect("agent dir");

    write_skill(&home.join(".agents").join("skills"), "shared");
    write_skill(&home.join(".agents").join("skills"), "global-only");
    write_skill(&agent_dir.join("skills"), "shared");
    write_skill(&cwd.join(".pi").join("skills"), "project-only");

    // The raw collector still lists the user dir (the filter is applied by
    // the discovery pass) ...
    assert!(
        collect_ancestor_agents_skill_dirs(&cwd).contains(&home.join(".agents").join("skills")),
        "the ancestor walk itself does not filter"
    );

    let layout = Layout {
        root: root.clone(),
        home: home.clone(),
        agent_dir: agent_dir.clone(),
        cwd: cwd.clone(),
    };
    let resources = discover(&layout, true);
    assert_eq!(
        skill_names(&resources),
        vec![
            "project-only".to_string(),
            "shared".to_string(),
            "global-only".to_string(),
        ],
        "the user-global dir loads once, at the user position (after <agentDir>/skills)"
    );
    let shared = resources
        .skills
        .iter()
        .find(|skill| skill.name == "shared")
        .expect("shared skill");
    assert!(
        shared.path.starts_with(agent_dir.join("skills")),
        "first-wins keeps the higher-precedence <agentDir>/skills copy: {}",
        shared.path.display()
    );
    assert_eq!(
        resources.diagnostics.len(),
        1,
        "the same-name collision is reported once: {:?}",
        resources.diagnostics
    );
    assert!(
        resources.diagnostics[0].contains("shared"),
        "the diagnostic names the colliding skill: {:?}",
        resources.diagnostics
    );
}

/// Missing `.agents/skills` directories are silent (no diagnostics, no
/// errors), and without a home directory the user-global root is skipped
/// instead of consulting the ambient `$HOME`.
#[test]
fn missing_agents_dirs_are_silent_and_no_home_skips_the_user_root() {
    // A tree with no `.agents` dirs at all.
    let tree = layout("missing");
    let resources = discover(&tree, true);
    assert!(
        resources.skills.is_empty(),
        "no skills anywhere: {:?}",
        skill_names(&resources)
    );
    assert!(
        resources.diagnostics.is_empty(),
        "missing dirs are silent: {:?}",
        resources.diagnostics
    );

    // `None` home: the ancestors still load, `~/.agents/skills` does not.
    let tree = layout("no-home");
    fill_tree(&tree);
    let resources = DiscoveredResources::discover_with_home(
        &ResourceDiscoveryOptions {
            cwd: &tree.cwd,
            agent_dir: &tree.agent_dir,
            project_trusted: true,
            cli_skill_paths: &[],
            cli_prompt_paths: &[],
            no_skills: false,
            no_prompt_templates: true,
        },
        None,
    );
    assert_eq!(
        skill_names(&resources),
        vec![
            "project-pi".to_string(),
            "ancestor-cwd".to_string(),
            "ancestor-repo".to_string(),
            "ancestor-gitroot".to_string(),
            "user-agent".to_string(),
        ],
        "no home directory means no ~/.agents/skills root"
    );
}
