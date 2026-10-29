//! Integration tests for system-prompt assembly, skills, and settings.json.
//!
//! Covers: section order + XML wrapping, context-file discovery order, settings
//! two-scope merge precedence, skills frontmatter validation + XML, SYSTEM.md
//! discovery, section diffing, and git-worktree shadowing.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use pir::coding_agent::resources::{DiscoveredResources, ResourceDiscoveryOptions};
use pir::config::settings::{
    discover_context_files, discover_system_md, Settings, SettingsManager,
};
use pir::skills::{
    available_skills_xml, discover_skills, load_skill_from_file, validate_description,
    validate_name, SkillFileReadTool, SkillInfo,
};
use pir::system_prompt::{
    build_system_prompt, build_system_prompt_sections, diff_sections, SectionChanges,
    SystemPromptInput, SystemPromptSection, SystemPromptSections, ToolInfo,
};

/// Acquire the binary-wide environment lock (`tests/common/mod.rs`) from a
/// sync test. `PI_PACKAGE_DIR` is process-global, and the other suites in this
/// merged binary serialize their `PIR_AGENT_DIR`/`HOME` mutations on the same
/// tokio mutex; a module-local lock would not exclude them.
fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(crate::common::agent_dir_env_lock().lock())
}

/// Set (or remove) `PI_PACKAGE_DIR` for the lifetime of the guard, restoring
/// the previous value on drop — including a panic mid-test, so a failed test
/// cannot leak a package dir into its neighbours.
struct PackageDirVar(Option<std::ffi::OsString>);

impl PackageDirVar {
    fn set(path: &Path) -> Self {
        let previous = std::env::var_os("PI_PACKAGE_DIR");
        std::env::set_var("PI_PACKAGE_DIR", path.as_os_str());
        Self(previous)
    }

    fn removed() -> Self {
        let previous = std::env::var_os("PI_PACKAGE_DIR");
        std::env::remove_var("PI_PACKAGE_DIR");
        Self(previous)
    }
}

impl Drop for PackageDirVar {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => std::env::set_var("PI_PACKAGE_DIR", value),
            None => std::env::remove_var("PI_PACKAGE_DIR"),
        }
    }
}

// ---------------------------------------------------------------------------
// Temp-dir helper (no tempfile dependency)
// ---------------------------------------------------------------------------

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-sysprompt-{}-{}-{}",
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

    fn child(&self, relative: &str) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(&path).expect("create child dir");
        path
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

// ---------------------------------------------------------------------------
// Section order + XML wrapping
// ---------------------------------------------------------------------------

fn child_dir(parent: &Path, relative: &str) -> PathBuf {
    let path = parent.join(relative);
    fs::create_dir_all(&path).expect("create child dir");
    path
}

fn default_tools() -> Vec<ToolInfo> {
    vec![
        ToolInfo {
            name: "read".to_string(),
            description: "Read the contents of a file.".to_string(),
            snippet: Some("Read file contents".to_string()),
            guidelines: vec!["Use read to examine files instead of cat or sed.".to_string()],
        },
        ToolInfo {
            name: "bash".to_string(),
            description: "Execute bash commands".to_string(),
            snippet: Some("Execute bash commands (ls, grep, find, etc.)".to_string()),
            guidelines: vec![
                "You can inspect PI_* environment variables for current model and session details."
                    .to_string(),
            ],
        },
    ]
}

fn sample_input<'a>(
    tools: &'a [ToolInfo],
    project_files: &'a [(PathBuf, String)],
    skills: &'a [SkillInfo],
) -> SystemPromptInput<'a> {
    SystemPromptInput {
        preamble_override: None,
        append: vec!["Always answer in English.".to_string()],
        tools,
        hidden_tools: &[],
        project_files,
        skills,
        custom_sections: Vec::new(),
        cwd: Path::new("C:\\some\\dir"),
        package_dir: None,
    }
}

#[test]
fn sections_are_ordered_and_xml_wrapped() {
    // Containment against the docs tests' PI_PACKAGE_DIR mutation.
    let _guard = env_lock();
    let project_files = vec![(PathBuf::from("AGENTS.md"), "be nice".to_string())];
    let skills = vec![SkillInfo {
        name: "code-review".to_string(),
        description: "Review code".to_string(),
        path: PathBuf::from("skills/code-review/SKILL.md"),
        disable_model_invocation: false,
    }];
    let tools = default_tools();
    let input = sample_input(&tools, &project_files, &skills);
    let sections = build_system_prompt_sections(&input);

    let order: Vec<SystemPromptSection> = sections.iter().map(|(name, _)| name.clone()).collect();
    assert_eq!(
        order,
        vec![
            SystemPromptSection::Preamble,
            SystemPromptSection::Tools,
            SystemPromptSection::Rules,
            SystemPromptSection::Addendum,
            SystemPromptSection::ProjectContext,
            SystemPromptSection::Skills,
            SystemPromptSection::Cwd,
        ]
    );

    let preamble = sections.get(SystemPromptSection::Preamble).unwrap();
    assert!(!preamble.starts_with('<'), "preamble must be untagged");

    let tools = sections.get(SystemPromptSection::Tools).unwrap();
    assert!(tools.starts_with("<tools>\n") && tools.ends_with("</tools>"));
    // The tools section uses the tool's promptSnippet (not the description).
    assert!(tools.contains("- read: Read file contents"));
    assert!(
        !tools.contains("Read the contents of a file."),
        "snippet wins over the description: {tools}"
    );
    assert!(
        !tools.contains("Extra detail line"),
        "only first line of description"
    );
    assert!(tools.contains("In addition to the tools above"));

    let rules = sections.get(SystemPromptSection::Rules).unwrap();
    // Per-tool guidelines precede the fixed rules. The fixture declares only
    // read+bash (no grep/find/ls), so the conditional file-operations rule is
    // present (upstream system-prompt.ts: `(hasBash||hasPowerShell) &&
    // !hasGrep && !hasFind && !hasLs`).
    assert!(
        rules.contains("- Use bash for file operations like ls, rg, find"),
        "{rules}"
    );
    assert!(rules.contains("- Use read to examine files instead of cat or sed."));
    assert!(rules.contains(
        "- You can inspect PI_* environment variables for current model and session details."
    ));
    assert!(rules.contains("- Be concise in your responses"));
    assert!(rules.contains("- Show file paths clearly when working with files"));

    let addendum = sections.get(SystemPromptSection::Addendum).unwrap();
    assert_eq!(
        addendum,
        "<addendum>\nAlways answer in English.\n</addendum>"
    );

    let project_context = sections.get(SystemPromptSection::ProjectContext).unwrap();
    assert!(project_context
        .contains("<project_instructions path=\"AGENTS.md\">\nbe nice\n</project_instructions>"));

    let cwd = sections.get(SystemPromptSection::Cwd).unwrap();
    assert_eq!(cwd, "<cwd>\nC:/some/dir\n</cwd>", "backslashes normalized");

    let prompt = build_system_prompt(&input);
    let preamble_end = prompt.find("coding agent harness").unwrap();
    let tools_start = prompt.find("<tools>").unwrap();
    let rules_start = prompt.find("<rules>").unwrap();
    let addendum_start = prompt.find("<addendum>").unwrap();
    let project_start = prompt.find("<project_context>").unwrap();
    let skills_start = prompt.find("<skills>").unwrap();
    let cwd_start = prompt.find("<cwd>").unwrap();
    assert!(
        preamble_end < tools_start
            && tools_start < rules_start
            && rules_start < addendum_start
            && addendum_start < project_start
            && project_start < skills_start
            && skills_start < cwd_start
    );
    assert!(prompt.contains("</cwd>"));
}

#[test]
fn rules_include_the_conditional_bash_file_operations_rule() {
    // Containment against the docs tests' PI_PACKAGE_DIR mutation.
    let _guard = env_lock();
    // Only bash is active: the file-operations rule is added (upstream
    // buildRules fires when a shell is declared without grep/find/ls).
    let tools = vec![ToolInfo::new("bash", "Execute bash commands")];
    let input = SystemPromptInput {
        preamble_override: None,
        append: Vec::new(),
        tools: &tools,
        hidden_tools: &[],
        project_files: &[],
        skills: &[],
        custom_sections: Vec::new(),
        cwd: Path::new("/w"),
        package_dir: None,
    };
    let rules = build_system_prompt_sections(&input)
        .get(SystemPromptSection::Rules)
        .unwrap()
        .to_string();
    assert!(
        rules.contains("- Use bash for file operations like ls, rg, find"),
        "{rules}"
    );

    // With grep declared, the rule is unnecessary and omitted.
    let tools = vec![
        ToolInfo::new("bash", "Execute bash commands"),
        ToolInfo::new("grep", "Search file contents"),
    ];
    let input = SystemPromptInput {
        preamble_override: None,
        append: Vec::new(),
        tools: &tools,
        hidden_tools: &[],
        project_files: &[],
        skills: &[],
        custom_sections: Vec::new(),
        cwd: Path::new("/w"),
        package_dir: None,
    };
    let rules = build_system_prompt_sections(&input)
        .get(SystemPromptSection::Rules)
        .unwrap()
        .to_string();
    assert!(!rules.contains("Use bash for file operations"), "{rules}");
}

#[test]
fn tools_without_snippets_are_not_listed() {
    // Containment against the docs tests' PI_PACKAGE_DIR mutation.
    let _guard = env_lock();
    // Upstream builds the tools section from snippet-carrying tools only
    // (agent-session.ts:1701-1706); the description is never a fallback.
    let tools = vec![ToolInfo::new("read", "Read the contents of a file.\nMore.")];
    let input = SystemPromptInput {
        preamble_override: None,
        append: Vec::new(),
        tools: &tools,
        hidden_tools: &[],
        project_files: &[],
        skills: &[],
        custom_sections: Vec::new(),
        cwd: Path::new("/w"),
        package_dir: None,
    };
    let tools_section = build_system_prompt_sections(&input)
        .get(SystemPromptSection::Tools)
        .unwrap()
        .to_string();
    assert!(
        !tools_section.contains("- read"),
        "a tool without a promptSnippet is omitted: {tools_section}"
    );
    assert!(tools_section.contains("(none)"));
    assert!(tools_section.contains("In addition to the tools above"));
}

#[test]
fn custom_sections_are_appended_in_order_and_validated() {
    // Containment against the docs tests' PI_PACKAGE_DIR mutation.
    let _guard = env_lock();
    let tools: Vec<ToolInfo> = Vec::new();
    let input = SystemPromptInput {
        preamble_override: None,
        append: Vec::new(),
        tools: &tools,
        hidden_tools: &[],
        project_files: &[],
        skills: &[],
        custom_sections: vec![
            ("company_rules".to_string(), "Be excellent.".to_string()),
            ("bad Name".to_string(), "dropped".to_string()),
            ("preamble".to_string(), "dropped too".to_string()),
            ("second".to_string(), String::new()), // empty content is skipped
        ],
        cwd: Path::new("/w"),
        package_dir: None,
    };
    let sections = build_system_prompt_sections(&input);
    let order: Vec<SystemPromptSection> = sections.iter().map(|(name, _)| name.clone()).collect();
    assert_eq!(
        order,
        vec![
            SystemPromptSection::Preamble,
            SystemPromptSection::Tools,
            SystemPromptSection::Rules,
            SystemPromptSection::Cwd,
            SystemPromptSection::Custom("company_rules".to_string()),
        ]
    );
    let custom = sections
        .get(SystemPromptSection::Custom("company_rules".to_string()))
        .unwrap();
    assert_eq!(custom, "<company_rules>\nBe excellent.\n</company_rules>");
}

#[test]
fn docs_section_is_emitted_when_package_docs_exist() {
    let _guard = env_lock();
    let root = TempDir::new("docs-section");
    let package = root.child("package");
    fs::write(package.join("README.md"), "# pir").unwrap();
    fs::create_dir_all(package.join("docs")).unwrap();
    fs::create_dir_all(package.join("examples")).unwrap();

    // Pinned for the whole test (restored by the guard). Like the real
    // callers the test resolves the package dir up front and threads it into
    // the input.
    let _package = PackageDirVar::set(&package);
    let package_dir = pir::config::env::package_dir();
    let tools = default_tools();
    let mut input = sample_input(&tools, &[], &[]);
    input.package_dir = package_dir.as_deref();
    let sections = build_system_prompt_sections(&input);

    let order: Vec<SystemPromptSection> = sections.iter().map(|(name, _)| name.clone()).collect();
    let docs_index = order
        .iter()
        .position(|section| *section == SystemPromptSection::Docs)
        .expect("docs section present");
    let rules_index = order
        .iter()
        .position(|section| *section == SystemPromptSection::Rules)
        .unwrap();
    assert!(rules_index < docs_index, "docs follows rules: {order:?}");
    let docs = sections.get(SystemPromptSection::Docs).unwrap();
    assert!(docs.contains("- Main documentation: "));
    assert!(docs.contains("- Additional docs: "));
    assert!(docs.contains("- Examples: "));
}

#[test]
fn docs_section_is_absent_without_package_docs() {
    let _guard = env_lock();
    let _package = PackageDirVar::removed();
    let tools = default_tools();
    // `sample_input` threads no package dir: no Docs section regardless of the
    // process environment (the builder never reads it).
    let input = sample_input(&tools, &[], &[]);
    let sections = build_system_prompt_sections(&input);
    assert!(sections.get(SystemPromptSection::Docs).is_none());
}

/// Regression for the `system_prompt` load-flake: the builder must take the
/// package dir through `SystemPromptInput::package_dir` (like `cwd`), not read
/// `PI_PACKAGE_DIR` deep inside — so the caller's resolved value, not ambient
/// process state, decides the `Docs` section and the order stays deterministic.
#[test]
fn docs_section_follows_the_threaded_package_dir_not_the_process_env() {
    let _guard = env_lock();
    let root = TempDir::new("docs-regression");
    let package = root.child("package");
    fs::write(package.join("README.md"), "# pir").unwrap();
    fs::create_dir_all(package.join("docs")).unwrap();
    fs::create_dir_all(package.join("examples")).unwrap();

    // Hostile ambient state: the env var names a real package with docs
    // (restored by the guard when the test ends).
    let _package = PackageDirVar::set(&package);

    // A caller that resolves the value up front and threads it in gets Docs
    // between Rules and Addendum, deterministically.
    let resolved = pir::config::env::package_dir();
    assert_eq!(resolved.as_deref(), Some(package.as_path()));
    let tools = default_tools();
    let mut input = sample_input(&tools, &[], &[]);
    input.package_dir = resolved.as_deref();
    let sections = build_system_prompt_sections(&input);
    let order: Vec<SystemPromptSection> = sections.iter().map(|(name, _)| name.clone()).collect();
    assert_eq!(
        order,
        vec![
            SystemPromptSection::Preamble,
            SystemPromptSection::Tools,
            SystemPromptSection::Rules,
            SystemPromptSection::Docs,
            SystemPromptSection::Addendum,
            SystemPromptSection::Cwd,
        ]
    );
    let docs = sections.get(SystemPromptSection::Docs).unwrap();
    assert!(docs.contains("- Main documentation: "));
    assert!(docs.contains("- Additional docs: "));
    assert!(docs.contains("- Examples: "));

    // The very same ambient env with `package_dir: None` yields no Docs
    // section: the builder never consults the process environment.
    let input = sample_input(&tools, &[], &[]);
    assert!(input.package_dir.is_none());
    let order: Vec<SystemPromptSection> = build_system_prompt_sections(&input)
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    assert_eq!(
        order,
        vec![
            SystemPromptSection::Preamble,
            SystemPromptSection::Tools,
            SystemPromptSection::Rules,
            SystemPromptSection::Addendum,
            SystemPromptSection::Cwd,
        ]
    );
}

#[test]
fn empty_tools_render_none_fallback_and_no_optional_sections_when_empty() {
    // Containment against the docs tests' PI_PACKAGE_DIR mutation.
    let _guard = env_lock();
    let input = SystemPromptInput {
        preamble_override: None,
        append: Vec::new(),
        tools: &[],
        hidden_tools: &[],
        project_files: &[],
        skills: &[],
        custom_sections: Vec::new(),
        cwd: Path::new("/tmp/work"),
        package_dir: None,
    };
    let sections = build_system_prompt_sections(&input);
    let order: Vec<SystemPromptSection> = sections.iter().map(|(name, _)| name.clone()).collect();
    assert_eq!(
        order,
        vec![
            SystemPromptSection::Preamble,
            SystemPromptSection::Tools,
            SystemPromptSection::Rules,
            SystemPromptSection::Cwd,
        ]
    );
    assert!(sections
        .get(SystemPromptSection::Tools)
        .unwrap()
        .contains("(none)"));
}

#[test]
fn preamble_override_replaces_tools_and_rules() {
    // Containment against the docs tests' PI_PACKAGE_DIR mutation.
    let _guard = env_lock();
    let input = SystemPromptInput {
        preamble_override: Some("You are a specialist.".to_string()),
        append: vec!["tail".to_string()],
        tools: &[ToolInfo::new("read", "Read")],
        hidden_tools: &[],
        project_files: &[],
        skills: &[],
        custom_sections: Vec::new(),
        cwd: Path::new("/w"),
        package_dir: None,
    };
    let sections = build_system_prompt_sections(&input);
    assert_eq!(
        sections.get(SystemPromptSection::Preamble),
        Some("You are a specialist.")
    );
    assert!(sections.get(SystemPromptSection::Tools).is_none());
    assert!(sections.get(SystemPromptSection::Rules).is_none());
    assert!(sections.get(SystemPromptSection::Addendum).is_some());
}

// ---------------------------------------------------------------------------
// Section diffing
// ---------------------------------------------------------------------------

#[test]
fn diff_sections_reports_changed_and_removed() {
    let mut previous = SystemPromptSections::default();
    previous.insert(SystemPromptSection::Preamble, "p".to_string());
    previous.insert(
        SystemPromptSection::Tools,
        "<tools>\nold\n</tools>".to_string(),
    );
    previous.insert(
        SystemPromptSection::Rules,
        "<rules>\nold\n</rules>".to_string(),
    );

    let mut current = SystemPromptSections::default();
    current.insert(SystemPromptSection::Preamble, "p".to_string());
    current.insert(
        SystemPromptSection::Tools,
        "<tools>\nnew\n</tools>".to_string(),
    );
    current.insert(SystemPromptSection::Cwd, "<cwd>\n/w\n</cwd>".to_string());

    let changes: SectionChanges = diff_sections(&previous, &current);
    assert_eq!(
        changes.changed,
        vec![
            (
                SystemPromptSection::Tools,
                "<tools>\nnew\n</tools>".to_string()
            ),
            (SystemPromptSection::Cwd, "<cwd>\n/w\n</cwd>".to_string()),
        ]
    );
    assert_eq!(changes.removed, vec![SystemPromptSection::Rules]);

    assert!(diff_sections(&previous, &previous).is_empty());
}

// ---------------------------------------------------------------------------
// Context-file discovery
// ---------------------------------------------------------------------------

#[test]
fn context_files_discovered_global_first_then_root_most_ancestor_first() {
    let root = TempDir::new("ctx-order");
    let global = root.child("global");
    let a = child_dir(root.path(), "repo");
    let b = child_dir(&a, "sub");
    let c = child_dir(&b, "deep");

    root.write("global/AGENTS.md", "global rules");
    // Note: writes below go through TempDir::write on root's path.
    fs::write(a.join("AGENTS.md"), "repo rules").unwrap();
    fs::write(b.join("CLAUDE.md"), "sub rules").unwrap();
    fs::write(c.join("AGENTS.override.md"), "deep override").unwrap();

    let files = discover_context_files(&c, &global);
    let paths: Vec<String> = files
        .iter()
        .map(|(path, _)| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        paths,
        vec!["AGENTS.md", "AGENTS.md", "CLAUDE.md", "AGENTS.override.md"]
    );

    let contents: Vec<&str> = files.iter().map(|(_, content)| content.as_str()).collect();
    assert_eq!(
        contents,
        vec!["global rules", "repo rules", "sub rules", "deep override"]
    );
}

#[test]
fn candidate_precedence_within_one_directory() {
    let root = TempDir::new("ctx-candidates");
    let global = root.child("global");
    let project = root.child("project");

    fs::write(project.join("CLAUDE.md"), "claude lowercase").unwrap();
    fs::write(project.join("AGENTS.md"), "agents lowercase").unwrap();
    fs::write(project.join("AGENTS.override.md"), "override wins").unwrap();

    let files = discover_context_files(&project, &global);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].0.file_name().unwrap(), "AGENTS.override.md");
    assert_eq!(files[0].1, "override wins");

    // Without the override, AGENTS.md beats CLAUDE.md.
    fs::remove_file(project.join("AGENTS.override.md")).unwrap();
    let files = discover_context_files(&project, &global);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].0.file_name().unwrap(), "AGENTS.md");
}

#[test]
fn nested_worktree_shadows_main_repo_context_file() {
    let root = TempDir::new("ctx-worktree");
    let global = root.child("global");
    let main = root.child("main");
    fs::create_dir_all(main.join(".git")).unwrap();
    fs::write(main.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(main.join("AGENTS.md"), "main repo rules").unwrap();

    // Simulated linked worktree layout (no git binary needed):
    // main/wt/.git -> file pointing at main/.git/worktrees/wt, whose
    // commondir points back at main/.git.
    let worktree = child_dir(&main, "wt");
    let git_dir = main.join(".git").join("worktrees").join("wt");
    fs::create_dir_all(&git_dir).unwrap();
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
    fs::write(git_dir.join("commondir"), "../..\n").unwrap();
    fs::write(
        worktree.join(".git"),
        format!("gitdir: {}", git_dir.display()),
    )
    .unwrap();
    fs::write(worktree.join("AGENTS.md"), "worktree rules").unwrap();

    let files = discover_context_files(&worktree, &global);
    let contents: Vec<&str> = files.iter().map(|(_, content)| content.as_str()).collect();
    // The main repo's AGENTS.md is shadowed; only the worktree's copy loads.
    assert_eq!(contents, vec!["worktree rules"]);

    // An ordinary repo (worktree root == main repo root) loads its own file.
    let files = discover_context_files(&main, &global);
    let contents: Vec<&str> = files.iter().map(|(_, content)| content.as_str()).collect();
    assert_eq!(contents, vec!["main repo rules"]);
}

// ---------------------------------------------------------------------------
// Settings: two-scope merge
// ---------------------------------------------------------------------------

#[test]
fn settings_project_scope_deep_merges_over_global() {
    let root = TempDir::new("settings-merge");
    let global_dir = root.child("agent");
    let project_dir = root.child("pi");
    let cwd = root.child("work");

    fs::write(
        global_dir.join("settings.json"),
        r#"{
            "defaultProvider": "anthropic",
            "defaultModel": "claude-global",
            "compaction": { "reserveTokens": 4096 },
            "retry": { "maxRetries": 5 }
        }"#,
    )
    .unwrap();
    fs::write(
        project_dir.join("settings.json"),
        r#"{
            "defaultModel": "claude-project",
            "compaction": { "keepRecentTokens": 1000 },
            "theme": "dark"
        }"#,
    )
    .unwrap();

    let manager = SettingsManager::load(&global_dir, &project_dir, &cwd, true);
    let settings = &manager.settings;
    // Scalars: project wins.
    assert_eq!(settings.default_provider.as_deref(), Some("anthropic"));
    assert_eq!(settings.default_model.as_deref(), Some("claude-project"));
    assert_eq!(settings.theme.as_deref(), Some("dark"));
    // Nested: merged per-field across scopes.
    let compaction = settings.compaction.as_ref().unwrap();
    // Unset nested fields stay None on the struct; accessors apply defaults.
    assert_eq!(compaction.enabled, None);
    assert!(manager.compaction_enabled(), "default-on field");
    assert_eq!(compaction.reserve_tokens, Some(4096));
    assert_eq!(compaction.keep_recent_tokens, Some(1000));
    let retry = settings.retry.as_ref().unwrap();
    assert_eq!(retry.max_retries, Some(5));
    assert_eq!(retry.enabled, None);
    assert_eq!(retry.base_delay_ms, None);
    assert_eq!(retry.max_agent_delay_ms, None);
    assert_eq!(manager.retry_settings().max_agent_delay_ms, Some(60000));
}

#[test]
fn settings_missing_files_fall_back_to_defaults() {
    let root = TempDir::new("settings-empty");
    let global_dir = root.child("agent");
    let project_dir = root.child("pi");
    let cwd = root.child("work");

    let manager = SettingsManager::load(&global_dir, &project_dir, &cwd, true);
    assert_eq!(manager.settings, Settings::default());
    assert_eq!(manager.settings.default_model, None);

    // Partial nested object: unset nested fields use upstream defaults.
    fs::write(
        global_dir.join("settings.json"),
        r#"{ "compaction": { "reserveTokens": 8192 } }"#,
    )
    .unwrap();
    let manager = SettingsManager::load(&global_dir, &project_dir, &cwd, true);
    let compaction = manager.settings.compaction.clone().unwrap();
    assert_eq!(compaction.enabled, None);
    assert_eq!(compaction.reserve_tokens, Some(8192));
    assert_eq!(compaction.keep_recent_tokens, None);
    // Accessors apply the upstream defaults on top.
    assert!(manager.compaction_enabled());
    assert_eq!(manager.compaction_keep_recent_tokens(None), 20000);
}

#[test]
fn settings_wire_names_match_upstream_camel_case() {
    let root = TempDir::new("settings-wire");
    let global_dir = root.child("agent");
    fs::write(
        global_dir.join("settings.json"),
        r#"{
            "defaultThinkingLevel": "high",
            "steeringMode": "one-at-a-time",
            "followUpMode": "all",
            "defaultTools": ["read", "bash"],
            "sessionDir": "C:/sessions",
            "enabledModels": ["claude-*"]
        }"#,
    )
    .unwrap();

    let manager = SettingsManager::load(&global_dir, &root.child("pi"), &root.child("work"), true);
    let settings = &manager.settings;
    assert!(matches!(
        settings.default_thinking_level,
        Some(pir::ai::types::ThinkingLevel::High)
    ));
    assert_eq!(
        settings.steering_mode,
        Some(pir::config::settings::SteeringMode::OneAtATime)
    );
    assert_eq!(
        settings.follow_up_mode,
        Some(pir::config::settings::FollowUpMode::All)
    );
    assert_eq!(
        settings.default_tools.as_deref().unwrap(),
        ["read".to_string(), "bash".to_string()]
    );
    assert_eq!(settings.session_dir.as_deref(), Some("C:/sessions"));
    assert_eq!(
        settings.enabled_models.as_deref().unwrap(),
        ["claude-*".to_string()]
    );
}

// ---------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------

#[test]
fn skill_frontmatter_validation_matches_agent_skills_spec() {
    assert!(validate_name("code-review").is_empty());
    assert!(!validate_name("Code Review").is_empty());
    assert!(!validate_name("has--dashes").is_empty());
    assert!(!validate_name("-leading").is_empty());
    assert!(!validate_name("trailing-").is_empty());
    assert!(!validate_name(&"a".repeat(65)).is_empty());
    assert!(validate_name(&"a".repeat(64)).is_empty());

    assert!(validate_description("review code").is_empty());
    assert!(!validate_description("   ").is_empty());
    assert!(!validate_description(&"d".repeat(1025)).is_empty());
    assert!(validate_description(&"d".repeat(1024)).is_empty());
}

#[test]
fn skill_loading_parses_frontmatter_and_falls_back_to_dir_name() {
    let root = TempDir::new("skills-load");
    let skill_dir = root.child("code-review");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: code-review\ndescription: Review code for bugs\ndisable-model-invocation: true\n---\n\nBody text.\n",
    )
    .unwrap();
    let skill = load_skill_from_file(&skill_dir.join("SKILL.md")).unwrap();
    assert_eq!(skill.name, "code-review");
    assert_eq!(skill.description, "Review code for bugs");
    assert!(skill.disable_model_invocation);

    // Name falls back to the parent directory name.
    let implicit = root.child("implicit-skill");
    fs::write(
        implicit.join("SKILL.md"),
        "---\ndescription: No explicit name\n---\nBody\n",
    )
    .unwrap();
    let skill = load_skill_from_file(&implicit.join("SKILL.md")).unwrap();
    assert_eq!(skill.name, "implicit-skill");
    assert!(!skill.disable_model_invocation);

    // Missing description is rejected.
    let broken = root.child("no-description");
    fs::write(broken.join("SKILL.md"), "---\nname: x\n---\nBody\n").unwrap();
    assert!(load_skill_from_file(&broken.join("SKILL.md")).is_err());

    // Invalid names are reported by validation.
    let invalid = root.child("bad-name");
    fs::write(
        invalid.join("SKILL.md"),
        "---\nname: Bad--Name\ndescription: ok\n---\nBody\n",
    )
    .unwrap();
    let skill = load_skill_from_file(&invalid.join("SKILL.md")).unwrap();
    assert!(!validate_name(&skill.name).is_empty());
}

#[test]
fn skill_discovery_walks_dirs_without_recursing_into_skill_roots() {
    let root = TempDir::new("skills-discover");
    let skills_dir = root.child("skills");

    // A skill root with SKILL.md: no recursion below it.
    root.write(
        "skills/reviewer/SKILL.md",
        "---\nname: reviewer\ndescription: Reviews things\n---\nBody\n",
    );
    fs::write(
        skills_dir.join("reviewer").join("nested.md"),
        "---\nname: nested-should-not-load\ndescription: hidden\n---\n",
    )
    .unwrap();

    // Root-level .md file is a skill; dotfiles and node_modules are skipped.
    fs::write(
        skills_dir.join("notes.md"),
        "---\nname: notes\ndescription: Root level skill\n---\n",
    )
    .unwrap();
    root.write("skills/.hidden/hidden.md", "not a skill\n");
    root.write(
        "skills/node_modules/pkg/SKILL.md",
        "---\nname: dep-skill\ndescription: Should be skipped\n---\n",
    );
    // Nested skill directory (below the scan root, not inside another skill root).
    root.write(
        "skills/group/testing/SKILL.md",
        "---\nname: testing\ndescription: Nested skill\n---\n",
    );

    let dirs = vec![skills_dir.clone()];
    let skills = discover_skills(&dirs);
    let mut names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["notes", "reviewer", "testing"]);

    // Name collisions: the first directory in the list wins.
    let other = root.child("other-skills");
    fs::write(
        other.join("reviewer.md"),
        "---\nname: reviewer\ndescription: Duplicate name\n---\n",
    )
    .unwrap();
    let dirs = vec![skills_dir, other];
    let skills = discover_skills(&dirs);
    let reviewer = skills.iter().find(|s| s.name == "reviewer").unwrap();
    assert_eq!(reviewer.description, "Reviews things");
}

#[test]
fn skill_discovery_respects_gitignore_and_fdignore_files() {
    let root = TempDir::new("skills-ignore");
    let skills_dir = root.child("skills");

    root.write(
        "skills/kept/SKILL.md",
        "---\nname: kept\ndescription: Kept skill\n---\nBody\n",
    );
    root.write(
        "skills/ignored-by-git/SKILL.md",
        "---\nname: ignored-by-git\ndescription: Filtered\n---\nBody\n",
    );
    root.write(
        "skills/group/fd-ignored/SKILL.md",
        "---\nname: fd-ignored\ndescription: Filtered subdir\n---\nBody\n",
    );
    root.write(
        "skills/group/kept-deep/SKILL.md",
        "---\nname: kept-deep\ndescription: Deep kept skill\n---\nBody\n",
    );

    // Ignore files: root .gitignore filters a top-level skill dir; a nested
    // .fdignore filters a skill below it.
    root.write("skills/.gitignore", "ignored-by-git/\n");
    root.write("skills/group/.fdignore", "fd-ignored/\n");

    let dirs = vec![skills_dir.clone()];
    let skills = discover_skills(&dirs);
    let mut names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["kept", "kept-deep"],
        "ignore files must filter the walk: {names:?}"
    );
}

#[test]
fn available_skills_xml_matches_upstream_shape_and_hides_command_only_skills() {
    let skills = vec![
        SkillInfo {
            name: "code&review".to_string(),
            description: "Review <code>".to_string(),
            path: PathBuf::from("C:\\skills\\code&review\\SKILL.md"),
            disable_model_invocation: false,
        },
        SkillInfo {
            name: "secret".to_string(),
            description: "Command only".to_string(),
            path: PathBuf::from("C:\\skills\\secret\\SKILL.md"),
            disable_model_invocation: true,
        },
    ];
    let xml = available_skills_xml(&skills, SkillFileReadTool::Read);
    assert!(xml.contains("<available_skills>"));
    assert!(xml.contains("<name>code&amp;review</name>"));
    assert!(xml.contains("<description>Review &lt;code&gt;</description>"));
    assert!(xml.contains("<location>C:\\skills\\code&amp;review\\SKILL.md</location>"));
    assert!(
        !xml.contains("secret"),
        "disable_model_invocation skills are excluded"
    );
    assert!(xml.trim_end().ends_with("</available_skills>"));
    assert!(xml.contains("Use the read tool to load a skill's file"));

    // The bash and indirect wording variants (upstream `fileReadTool`).
    let xml = available_skills_xml(&skills, SkillFileReadTool::Bash);
    assert!(xml.contains("Use bash to load a skill's file"));
    assert!(!xml.contains("Use the read tool"));
    let xml = available_skills_xml(&skills, SkillFileReadTool::Indirect);
    assert!(xml.contains("Load a skill's file when the task matches its description."));
    assert!(!xml.contains("Use the read tool"));
    assert!(!xml.contains("Use bash to load"));

    assert_eq!(available_skills_xml(&[], SkillFileReadTool::Read), "");
}

// ---------------------------------------------------------------------------
// Resource precedence (CLI paths first, upstream resourcePrecedenceRank)
// ---------------------------------------------------------------------------

#[test]
fn resource_discovery_prefers_cli_paths_over_defaults_and_records_collisions() {
    let root = TempDir::new("res-precedence");
    let agent_dir = root.child("agent");
    let cwd = root.child("work");
    let cli = root.child("cli");
    let settings_dir = root.child("settings-skills");
    // A git marker at the fixture root: the ancestor `.agents/skills` walk
    // stops there, so the temp cwd (which lives under the real user profile)
    // can never pick up the machine's own `~/.agents/skills`.
    root.write(
        ".git/HEAD",
        "ref: refs/heads/main
",
    );

    // Default user skill that the CLI path shadows by name.
    root.write(
        "agent/skills/dup/SKILL.md",
        "---\nname: dup\ndescription: Default loses\n---\nBody\n",
    );
    // Default project skill (trust-gated `<cwd>/.pi/skills`).
    root.write(
        "work/.pi/skills/proj/SKILL.md",
        "---\nname: proj\ndescription: Project skill\n---\nBody\n",
    );
    // Settings-listed skill.
    root.write(
        "settings-skills/from-settings.md",
        "---\nname: from-settings\ndescription: Settings skill\n---\nBody\n",
    );
    fs::write(
        agent_dir.join("settings.json"),
        format!(
            r#"{{ "skills": ["{}"] }}"#,
            settings_dir.to_string_lossy().replace('\\', "\\\\")
        ),
    )
    .unwrap();
    // CLI skill with a colliding name plus a unique one.
    root.write(
        "cli/dup.md",
        "---\nname: dup\ndescription: CLI wins\n---\nBody\n",
    );
    root.write(
        "cli/only-cli.md",
        "---\nname: only-cli\ndescription: CLI skill\n---\nBody\n",
    );

    // Hermetic home: discovery would otherwise append the REAL
    // `~/.agents/skills` (the user's installed skills) to the expected list.
    let resources = DiscoveredResources::discover_with_home(
        &ResourceDiscoveryOptions {
            cwd: &cwd,
            agent_dir: &agent_dir,
            project_trusted: true,
            cli_skill_paths: &[cli],
            cli_prompt_paths: &[],
            no_skills: false,
            no_prompt_templates: true,
        },
        Some(&root.child("no-home")),
    );

    let names: Vec<&str> = resources
        .skills
        .iter()
        .map(|skill| skill.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["dup", "only-cli", "proj", "from-settings"],
        "CLI paths load first, then project settings/auto, then user \
         settings/auto: {names:?}"
    );
    let dup = resources
        .skills
        .iter()
        .find(|skill| skill.name == "dup")
        .unwrap();
    assert_eq!(dup.description, "CLI wins");
    assert!(
        resources
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("name \"dup\" collision")),
        "collision recorded: {:?}",
        resources.diagnostics
    );
}

#[test]
fn prompt_template_frontmatter_parse_failures_are_rejected_with_a_warning() {
    let root = TempDir::new("tmpl-warnings");
    let dir = root.child("prompts");
    fs::write(dir.join("good.md"), "plain body\n").unwrap();
    fs::write(
        dir.join("frontmatter.md"),
        "---\ndescription: Titled\nargument-hint: x\n---\nbody\n",
    )
    .unwrap();
    // Malformed YAML frontmatter rejects the file with a warning.
    fs::write(
        dir.join("broken.md"),
        "---\ndescription: [unclosed\n---\nbody\n",
    )
    .unwrap();

    let resources = DiscoveredResources::discover(&ResourceDiscoveryOptions {
        cwd: root.path(),
        agent_dir: &root.child("agent"),
        project_trusted: true,
        cli_skill_paths: &[],
        cli_prompt_paths: &[dir],
        no_skills: true,
        no_prompt_templates: false,
    });

    let names: Vec<&str> = resources
        .prompt_templates
        .iter()
        .map(|template| template.name.as_str())
        .collect();
    // The broken template is rejected; the good ones load (names are the file
    // stems — upstream prompt-templates.ts strips the `.md` suffix).
    assert_eq!(names, vec!["frontmatter", "good"]);
    assert!(
        resources
            .diagnostics
            .iter()
            .any(|warning| warning.contains("failed to parse prompt template file")),
        "parse warning surfaced: {:?}",
        resources.diagnostics
    );
}

// ---------------------------------------------------------------------------
// SYSTEM.md / APPEND_SYSTEM.md
// ---------------------------------------------------------------------------

#[test]
fn system_md_discovery_prefers_project_over_global() {
    let root = TempDir::new("system-md");
    let global_dir = root.child("agent");
    let cwd = root.child("work");

    fs::write(global_dir.join("SYSTEM.md"), "global preamble").unwrap();
    fs::write(global_dir.join("APPEND_SYSTEM.md"), "global append").unwrap();

    let (preamble, append) = discover_system_md(&cwd, &global_dir, true);
    assert_eq!(preamble.as_deref(), Some("global preamble"));
    assert_eq!(append.as_deref(), Some("global append"));

    root.write("work/.pi/SYSTEM.md", "project preamble");
    root.write("work/.pi/APPEND_SYSTEM.md", "project append");

    let (preamble, append) = discover_system_md(&cwd, &global_dir, true);
    assert_eq!(preamble.as_deref(), Some("project preamble"));
    assert_eq!(append.as_deref(), Some("project append"));

    // The project files are trust-gated: untrusted projects fall back to the
    // global copy.
    let (preamble, append) = discover_system_md(&cwd, &global_dir, false);
    assert_eq!(preamble.as_deref(), Some("global preamble"));
    assert_eq!(append.as_deref(), Some("global append"));

    // Missing files yield None.
    let empty_cwd = root.child("bare");
    let (preamble, append) = discover_system_md(&empty_cwd, &root.child("no-agent-dir"), true);
    assert_eq!(preamble, None);
    assert_eq!(append, None);
}

#[test]
fn settings_manager_helpers_round_trip() {
    let root = TempDir::new("manager-helpers");
    let global_dir = root.child("agent");
    let project_dir = root.child("pi");
    let cwd = root.child("work");

    fs::write(global_dir.join("AGENTS.md"), "global context").unwrap();
    fs::write(cwd.join("AGENTS.md"), "project context").unwrap();
    fs::write(project_dir.join("settings.json"), r#"{ "theme": "dark" }"#).unwrap();

    let manager = SettingsManager::load(&global_dir, &project_dir, &cwd, true);
    assert_eq!(manager.settings.theme.as_deref(), Some("dark"));
    let files = discover_context_files(&cwd, &global_dir);
    let contents: Vec<&str> = files.iter().map(|(_, c)| c.as_str()).collect();
    assert_eq!(contents, vec!["global context", "project context"]);

    fs::write(global_dir.join("SYSTEM.md"), "sys").unwrap();
    let (preamble, append) = discover_system_md(&cwd, &global_dir, true);
    assert_eq!(preamble.as_deref(), Some("sys"));
    assert_eq!(append, None);
}

// ---------------------------------------------------------------------------
// D6 follow-up: the SESSION build applies `prepareLoadout` hidden declarations
// to the system prompt (upstream `_rebuildSystemPrompt`'s
// `hiddenTools: [..._hiddenDeclarations]`, agent-session.ts:1727).
// ---------------------------------------------------------------------------

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{
    declared_tools, AgentTool, BoxFuture, ToolContext, ToolExposure, ToolRegistry,
};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::{Model, ModelCost, ModelCostRates, ThinkingLevel};
use pir::coding_agent::tools::register_core_tools;
use pir::coding_agent::ToolSelection;
use pir::config::settings::CodemodeMode;
use pir::plugins::{CodemodeConfig, CodemodePlugin};

/// A fixture tool with a prompt snippet and a guideline, so it joins both the
/// `<tools>` and `<rules>` sections exactly like a core tool does.
struct PromptProbeTool {
    name: &'static str,
    snippet: &'static str,
    guideline: &'static str,
    exposure: ToolExposure,
    default_active: bool,
}

impl AgentTool for PromptProbeTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &'static str {
        "system-prompt fixture tool"
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({ "type": "object" }))
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("ok") })
    }
    fn exposure(&self) -> ToolExposure {
        self.exposure
    }
    fn default_active(&self) -> bool {
        self.default_active
    }
    fn prompt_snippet(&self) -> Option<String> {
        Some(self.snippet.to_owned())
    }
    fn prompt_guidelines(&self) -> Vec<String> {
        vec![self.guideline.to_owned()]
    }
}

fn prompt_fake_model() -> Model {
    Model {
        id: "fake-model".into(),
        name: "Fake model".into(),
        api: "fake-api".into(),
        provider: "fake".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 8192,
        max_tokens: 512,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

/// A session whose prompt is context-derived (`system_prompt: None`) and whose
/// declared set is `active` (the host's `--tools` selection), with the real
/// `codemode` plugin registered in `mode`.
fn codemode_prompt_session(mode: CodemodeMode, active: Vec<String>, cwd: &Path) -> AgentSession {
    let mut tools = ToolRegistry::new();
    register_core_tools(&mut tools);
    tools.register(std::sync::Arc::new(PromptProbeTool {
        name: "direct_probe",
        snippet: "Probe a direct tool",
        guideline: "Use direct_probe for direct probing.",
        exposure: ToolExposure::Direct,
        default_active: true,
    }));
    // A non-direct declared tool: codemode mode `"only"` hides `direct`
    // declarations only, so this one stays in the prompt.
    tools.register(std::sync::Arc::new(PromptProbeTool {
        name: "model_probe",
        snippet: "Probe a model-only tool",
        guideline: "Use model_probe for model-only probing.",
        exposure: ToolExposure::ModelOnly,
        default_active: true,
    }));
    AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: std::sync::Arc::new(ProviderRegistry::default()),
            model: prompt_fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools,
            session: None,
            compaction: None,
            max_turns: 1,
            // The context builder owns the prompt (`context_derived_prompt`).
            system_prompt: None,
            cwd: Some(cwd.to_path_buf()),
            event_stream: None,
            extensions: None,
            tool_selection: Some(ToolSelection {
                enabled: active,
                modifiers: Vec::new(),
                uses_default_tools: false,
            }),
        },
        vec![std::sync::Arc::new(CodemodePlugin::with_config(
            CodemodeConfig {
                mode,
                inline_budget: pir::plugins::DEFAULT_INLINE_BUDGET,
                sandbox_memory_bytes: Some(pir::plugins::codemode::default_sandbox_memory_bytes()),
            },
        ))],
    )
}

fn codemode_active_set() -> Vec<String> {
    ["read", "bash", "direct_probe", "model_probe", "codemode"]
        .iter()
        .map(|name| (*name).to_string())
        .collect()
}

/// D6 follow-up: with `codemode.mode: "only"` the SESSION build-time system
/// prompt applies the plugin's `prepareLoadout` hidden declarations
/// (agent-session.ts:1589-1624, 1727), exactly as the per-request declarations
/// already do: the hidden direct tools leave the `<tools>`/`<rules>` sections
/// while staying active/callable, and the codemode surface stays declared.
///
/// The `hidden` tools leave the sections — the request's declarations and the
/// prompt must agree (print does the same after CL2).
#[tokio::test]
async fn session_prompt_applies_prepare_loadout_hidden_declarations() {
    let _env = crate::common::AgentDirEnvGuard::scratch("sysprompt-codemode", None).await;
    // A context file makes the context-derived prompt render.
    let cwd = TempDir::new("codemode-hidden");
    fs::write(cwd.path().join("AGENTS.md"), "project instructions").unwrap();

    let session = codemode_prompt_session(CodemodeMode::Only, codemode_active_set(), cwd.path());
    let prompt = session
        .system_prompt_snapshot()
        .expect("a context file produces the context-derived prompt");

    // The codemode surface remains: the plugin tool stays active and declared
    // (its own hook never hides itself), and a non-direct declared tool is
    // not hidden by mode `"only"`.
    let (registry, active) = session.active_tool_snapshot(session.plugin_host());
    let declared: Vec<String> = declared_tools(registry.as_ref(), &active)
        .iter()
        .map(|tool| tool.name().to_owned())
        .collect();
    assert!(
        declared.iter().any(|name| name == "codemode"),
        "the codemode tool remains declared: {declared:?}"
    );
    assert!(
        declared.iter().any(|name| name == "direct_probe"),
        "hidden declarations stay ACTIVE: {declared:?}"
    );
    assert!(
        prompt.contains("- model_probe: Probe a model-only tool"),
        "a non-direct tool is not hidden by mode only: {prompt}"
    );
    assert!(
        prompt.contains("Use model_probe for model-only probing."),
        "its guideline stays in the rules: {prompt}"
    );

    // The hidden direct tools leave both sections.
    for hidden in [
        "- read: Read file contents",
        "- bash: ",
        "- direct_probe: Probe a direct tool",
    ] {
        assert!(
            !prompt.contains(hidden),
            "codemode mode only hides `{hidden}` from the prompt: {prompt}"
        );
    }
    assert!(
        !prompt.contains("Use direct_probe for direct probing."),
        "a hidden tool's guideline leaves the rules section: {prompt}"
    );
    assert!(
        !prompt.contains("Use read to examine files"),
        "a hidden tool's guideline leaves the rules section: {prompt}"
    );
    assert!(
        !prompt.contains("Use bash for file operations like ls, rg, find"),
        "the conditional shell rule follows the hidden set: {prompt}"
    );

    // `/reload` rebuilds from the same live snapshot and keeps applying the
    // hidden declarations (it must not reintroduce them).
    session.set_prompt_resources_shared(Vec::new(), Vec::new());
    let reloaded = session
        .system_prompt_snapshot()
        .expect("the reload keeps the context-derived prompt");
    assert!(
        !reloaded.contains("- direct_probe: Probe a direct tool"),
        "a reload keeps the hidden declaration hidden: {reloaded}"
    );
    assert!(
        reloaded.contains("- model_probe: Probe a model-only tool"),
        "a reload keeps the declared non-direct tool: {reloaded}"
    );

    // Contrast: the default mode leaves the direct declarations in place.
    let session = codemode_prompt_session(CodemodeMode::On, codemode_active_set(), cwd.path());
    let prompt = session
        .system_prompt_snapshot()
        .expect("a context file produces the context-derived prompt");
    assert!(
        prompt.contains("- direct_probe: Probe a direct tool"),
        "the default mode does not hide direct declarations: {prompt}"
    );
    assert!(
        prompt.contains("- read: Read file contents"),
        "the default mode keeps the core direct declarations: {prompt}"
    );
}

// ---------------------------------------------------------------------------
// The `mcp_servers` prompt section (CL18 audit item 11)
// ---------------------------------------------------------------------------

/// The section renders upstream's shape as closely as the builder allows:
/// one `- mcp__<name> (<reach>): <state>` line per server, under the
/// `mcp_servers` tag (upstream `renderServersSection`,
/// `extensions/mcp/index.ts:195-223`), with the bounded state as the summary.
#[test]
fn mcp_servers_section_lists_each_state() {
    use pir::system_prompt::{
        render_mcp_servers_section, McpServerListing, McpServerState, MCP_SERVERS_SECTION,
    };
    assert_eq!(MCP_SERVERS_SECTION, "mcp_servers");

    let servers = vec![
        McpServerListing {
            name: "docs".to_owned(),
            reach: "codemode",
            state: McpServerState::Ready { tool_count: 3 },
        },
        McpServerListing {
            name: "broken".to_owned(),
            reach: "direct",
            state: McpServerState::Failed {
                reason: "MCP server \"broken\" failed to connect: spawn failed".to_owned(),
            },
        },
        McpServerListing {
            name: "slow".to_owned(),
            reach: "deferred",
            state: McpServerState::Pending,
        },
    ];
    let section = render_mcp_servers_section(&servers).expect("three servers render a section");
    let lines: Vec<&str> = section.lines().collect();
    assert!(
        lines[1].starts_with("- mcp__docs (codemode): connected · 3 tools"),
        "{lines:?}"
    );
    assert!(
        lines[2]
            .starts_with("- mcp__broken (direct): failed: MCP server \"broken\" failed to connect"),
        "{lines:?}"
    );
    assert!(
        lines[3].starts_with("- mcp__slow (deferred): connecting"),
        "{lines:?}"
    );
    assert!(
        !section.contains('\n') || !section.contains("stderr"),
        "no config/env/stderr dump: {section}"
    );

    // Empty: no section at all.
    assert!(render_mcp_servers_section(&[]).is_none());
}

/// A failure reason is bounded to one line (CL24's `bounded_reason` rule):
/// the first line survives, later lines and an overlong tail do not.
#[test]
fn mcp_servers_section_bounds_failure_reasons() {
    use pir::system_prompt::{
        bounded_mcp_reason, render_mcp_servers_section, McpServerListing, McpServerState,
        MAX_MCP_SERVER_REASON_CHARS,
    };
    assert_eq!(
        bounded_mcp_reason("cause\nstderr: SECRET_TOKEN=abc"),
        "cause"
    );
    let long = "x".repeat(MAX_MCP_SERVER_REASON_CHARS + 50);
    let bounded = bounded_mcp_reason(&long);
    assert_eq!(bounded.chars().count(), MAX_MCP_SERVER_REASON_CHARS + 1);
    assert!(bounded.ends_with('…'));

    let section = render_mcp_servers_section(&[McpServerListing {
        name: "broken".to_owned(),
        reach: "direct",
        state: McpServerState::Failed {
            reason: "first line\nSECRET=leak".to_owned(),
        },
    }])
    .expect("one server renders");
    assert!(section.contains("failed: first line"), "{section}");
    assert!(!section.contains("SECRET"), "no stderr dump: {section}");
}

/// The whole section is capped (upstream `MAX_SERVERS_SECTION_CHARS`): the
/// last servers are omitted and counted, never cut mid-line.
#[test]
fn mcp_servers_section_caps_its_size() {
    use pir::system_prompt::{
        render_mcp_servers_section, McpServerListing, McpServerState, MAX_MCP_SERVERS_SECTION_CHARS,
    };
    let servers: Vec<McpServerListing> = (0..400)
        .map(|index| McpServerListing {
            name: format!("server-with-a-long-name-{index}"),
            reach: "codemode",
            state: McpServerState::Pending,
        })
        .collect();
    let section = render_mcp_servers_section(&servers).expect("a section renders");
    assert!(
        section.chars().count() <= MAX_MCP_SERVERS_SECTION_CHARS,
        "section is bounded: {} chars",
        section.chars().count()
    );
    let last = section.lines().last().expect("a closing line");
    assert!(
        last.starts_with("- … ") && last.ends_with(" more servers"),
        "{last:?}"
    );
}

/// The MCP section enters the prompt as a `custom_sections` entry: it lands
/// AFTER the built-ins (upstream appends extension sections last, before the
/// wrapper pass) and is wrapped in its own `mcp_servers` tag.
#[test]
fn mcp_section_is_appended_after_the_builtins() {
    use pir::coding_agent::context::mcp_custom_sections;
    let tools = default_tools();
    let section = "MCP servers configured for this session; their tools are named mcp__<server>__<tool>.\n- mcp__docs (codemode): connected · 1 tool";
    let input = SystemPromptInput {
        preamble_override: None,
        append: Vec::new(),
        tools: &tools,
        hidden_tools: &[],
        project_files: &[],
        skills: &[],
        custom_sections: mcp_custom_sections(Some(section)),
        cwd: Path::new("/work"),
        package_dir: None,
    };
    let sections = build_system_prompt_sections(&input);
    let order: Vec<SystemPromptSection> = sections.iter().map(|(name, _)| name.clone()).collect();
    assert_eq!(
        order,
        vec![
            SystemPromptSection::Preamble,
            SystemPromptSection::Tools,
            SystemPromptSection::Rules,
            SystemPromptSection::Cwd,
            SystemPromptSection::Custom("mcp_servers".to_owned()),
        ]
    );
    let rendered = sections
        .get(SystemPromptSection::Custom("mcp_servers".to_owned()))
        .expect("the section is present");
    assert!(
        rendered.starts_with("<mcp_servers>\n") && rendered.ends_with("\n</mcp_servers>"),
        "{rendered}"
    );

    // Empty custom sections: the prompt is byte-identical to the MCP-less build.
    let plain = || SystemPromptInput {
        preamble_override: None,
        append: Vec::new(),
        tools: &tools,
        hidden_tools: &[],
        project_files: &[],
        skills: &[],
        custom_sections: Vec::new(),
        cwd: Path::new("/work"),
        package_dir: None,
    };
    let without = build_system_prompt(&plain());
    let with_empty = build_system_prompt(&SystemPromptInput {
        custom_sections: mcp_custom_sections(None),
        ..plain()
    });
    assert_eq!(without, with_empty);
    assert!(!with_empty.contains("mcp_servers"));
}

/// CLI `--system-prompt` / `--append-system-prompt`: the preamble is replaced
/// while the structured sections (project context, cwd) survive, and the
/// append entries render as the `addendum` section. The overrides must also
/// survive an active-set rebuild (`apply_prompt_overrides` records them on the
/// prompt cache).
#[tokio::test]
async fn cli_prompt_overrides_keep_structure_and_rebuild() {
    let _env = crate::common::AgentDirEnvGuard::scratch("sysprompt-overrides", None).await;
    let cwd = TempDir::new("prompt-overrides");
    fs::write(cwd.path().join("AGENTS.md"), "project instructions").unwrap();

    let mut tools = ToolRegistry::new();
    register_core_tools(&mut tools);
    let session = AgentSession::new(AgentSessionConfig {
        provider_registry: std::sync::Arc::new(ProviderRegistry::default()),
        model: prompt_fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools,
        session: None,
        compaction: None,
        max_turns: 1,
        system_prompt: None,
        cwd: Some(cwd.path().to_path_buf()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    });
    let before = session
        .system_prompt_snapshot()
        .expect("a context file produces the context-derived prompt");
    assert!(before.contains("project instructions"), "{before}");

    session.apply_prompt_overrides(
        Some("CUSTOM PREAMBLE".to_string()),
        vec!["EXTRA RULE".to_string()],
    );
    let after = session
        .system_prompt_snapshot()
        .expect("the override keeps the derived prompt");
    assert!(after.starts_with("CUSTOM PREAMBLE"), "{after}");
    assert!(
        after.contains("<addendum>\nEXTRA RULE\n</addendum>"),
        "{after}"
    );
    assert!(after.contains("project instructions"), "{after}");
    assert!(after.contains("<cwd>"), "{after}");
    assert_ne!(before, after);
}
