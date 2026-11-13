//! W22: the prompt/request loadout contract (PIR-BIN-061/062/064).
//!
//! Three divergences the round-1 prompt A/B lane measured against upstream pi
//! (`system-prompt.ts` / `agent-session.ts`) are pinned here:
//!
//! 1. **PIR-BIN-064** — the provider `tools[]` ARRAY order must equal the
//!    `<tools>` section order (upstream `_applyToolLoadout` walks
//!    `getActiveToolNames()` and `system-prompt.ts` lists `selectedTools`;
//!    both preserve the caller's selection order). pir's declared set used to
//!    flow through a filtered registry's name-sorted `iter()`, so
//!    `--tools read,bash,edit,write` declared `bash,edit,read,write` while the
//!    prompt said `read,bash,edit,write`.
//! 2. **PIR-BIN-061** — the `docs` section names only paths the running
//!    install actually has: `README.md`, `docs/` and `examples/` must all be
//!    present in the resolved package directory. A release archive ships only
//!    `README.md` and `cargo install` ships no assets at all, so an installed
//!    pir sends no `docs` block instead of pointing the model at paths it does
//!    not have (the previous path-join-only section was worse than omitting
//!    it). A source checkout keeps the full section.
//! 3. **PIR-BIN-062** — the four pir-only `<rules>` bullets are a DOCUMENTED
//!    deviation (`docs/system-prompt-audit.md` §4, B19): they encode pir
//!    behaviours upstream lacks (`PIR_READ_BEFORE_EDIT` enforcement, the
//!    literal-path rule, the broad-search guard, the finish-check). Their
//!    exact list and gating are pinned so an edit cannot silently grow the
//!    prompt or drop an enforced rule.
//!
//! Everything here reads the rendered prompt/definitions the real print host
//! builds (`PrintToolLoadout` / `build_system_prompt_sections`); no fixture
//! special-cases the production path.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::coding_agent::print::print_tool_loadout_with_profile;
use pir::config::settings::PerformanceProfile;
use pir::system_prompt::{
    build_system_prompt_sections, SystemPromptInput, SystemPromptSection, ToolInfo,
};

/// A snippet-carrying probe tool: the `<tools>` section only lists tools with a
/// `promptSnippet`, so a probe without one could not pin the two orders.
struct SnippetProbe {
    name: &'static str,
    snippet: &'static str,
    guidelines: Vec<&'static str>,
}

impl SnippetProbe {
    fn new(name: &'static str, snippet: &'static str) -> Self {
        Self {
            name,
            snippet,
            guidelines: Vec::new(),
        }
    }

    fn with_guideline(mut self, guideline: &'static str) -> Self {
        self.guidelines.push(guideline);
        self
    }
}

impl AgentTool for SnippetProbe {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "probe tool"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({"type": "object", "properties": {}}))
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some(self.snippet.to_string())
    }

    fn prompt_guidelines(&self) -> Vec<String> {
        self.guidelines.iter().map(|s| (*s).to_string()).collect()
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async move { ToolResult::ok("ok") })
    }
}

fn probe_registry(names: &[&'static str]) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for name in names {
        let snippet: &'static str = match *name {
            "read" => "Read file contents",
            "bash" => "Execute bash commands",
            "edit" => "Make precise file edits",
            "write" => "Create or overwrite files",
            _ => "probe tool",
        };
        registry.register(Arc::new(SnippetProbe::new(name, snippet)));
    }
    registry
}

/// The tools the loadout's prompt render advertises, in section order — the
/// SAME derivation `PrintPromptCache` performs (declared tools → `ToolInfo`
/// → `build_system_prompt_sections` → the `<tools>` bullets).
fn prompt_tools_section_order(loadout: &pir::coding_agent::print::PrintToolLoadout) -> Vec<String> {
    let tools: Vec<ToolInfo> = loadout
        .declared
        .iter()
        .filter_map(|name| loadout.registry.get(name))
        .map(|tool| ToolInfo::from_agent_tool(tool.as_ref()))
        .collect();
    let sections = build_system_prompt_sections(&SystemPromptInput {
        preamble_override: None,
        append: Vec::new(),
        tools: &tools,
        hidden_tools: &loadout.hidden,
        project_files: &[],
        skills: &[],
        custom_sections: Vec::new(),
        cwd: std::path::Path::new("/w"),
        package_dir: None,
    });
    sections
        .get(SystemPromptSection::Tools)
        .expect("<tools> section")
        .lines()
        .filter_map(|line| line.strip_prefix("- "))
        .filter_map(|line| line.split_once(':'))
        .map(|(name, _)| name.trim().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// PIR-BIN-064: the request array order is the active-set order
// ---------------------------------------------------------------------------

/// `--tools read,bash,edit,write`: the `tools[]` order equals the `<tools>`
/// section order, which is the selection order (upstream pi's request carries
/// `["read","bash","edit","write"]` in both places).
///
/// Before the fix the declaration path rebuilt a filtered registry and read
/// its name-sorted iteration, so the wire order was `bash,edit,read,write`.
#[test]
fn declared_tool_array_order_matches_the_tools_section_order() {
    let loadout = print_tool_loadout_with_profile(
        probe_registry(&["read", "bash", "edit", "write"]),
        None,
        &[
            "read".to_string(),
            "bash".to_string(),
            "edit".to_string(),
            "write".to_string(),
        ],
        &[],
        false,
        false,
        PerformanceProfile::Balanced,
    )
    .expect("the selection resolves");
    assert_eq!(loadout.active, vec!["read", "bash", "edit", "write"]);

    let declared: Vec<String> = loadout
        .definitions()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    let section = prompt_tools_section_order(&loadout);
    assert_eq!(
        declared, section,
        "the request array must follow the prompt's <tools> order"
    );
    assert_eq!(declared, vec!["read", "bash", "edit", "write"]);
}

/// The `+name` modifier form (a `tool_search` load) and the exclude-only form
/// keep the same rule: the declared array is the active-set order, not the
/// registry's name order.
#[test]
fn modifier_and_exclude_selections_keep_the_active_set_order() {
    // A pure-modifier list starts from the DEFAULT_TOOL_NAMES base and appends.
    let loadout = print_tool_loadout_with_profile(
        probe_registry(&["read", "bash", "edit", "write", "grep"]),
        None,
        &[
            "+edit".to_string(),
            "-bash".to_string(),
            "+grep".to_string(),
        ],
        &[],
        false,
        false,
        PerformanceProfile::Balanced,
    )
    .expect("the modifier list resolves");
    let declared: Vec<String> = loadout
        .definitions()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert_eq!(
        declared,
        prompt_tools_section_order(&loadout),
        "the modifier form must keep the two surfaces in one order"
    );

    // Exclude-only: the registration-active base is name-sorted upstream
    // (`Map` insertion), and BOTH surfaces read that same base order.
    let loadout = print_tool_loadout_with_profile(
        probe_registry(&["read", "bash", "edit", "write"]),
        None,
        &[],
        &["bash".to_string()],
        false,
        false,
        PerformanceProfile::Balanced,
    )
    .expect("the exclude-only selection resolves");
    let declared: Vec<String> = loadout
        .definitions()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert_eq!(declared, prompt_tools_section_order(&loadout));
}

// ---------------------------------------------------------------------------
// PIR-BIN-062: the four pir-only rules are documented and gated
// ---------------------------------------------------------------------------

/// The exact pir-only bullets, verbatim. These encode enforcement upstream does
/// not have (`docs/system-prompt-audit.md` §4, B19):
/// - the read-before-edit gate (`PIR_READ_BEFORE_EDIT`, default `enforce`),
/// - the literal-path check (`coding_agent/tools/util.rs`),
/// - the broad-search guard (`tools.broadSearchGuard`, default true),
/// - the finish/verification check.
const PIR_ONLY_RULES: [&str; 4] = [
    "Read an existing file completely (no offset/limit) before writing or editing it; after your own successful write/edit you do not need to re-read it unless the tool reports an external change",
    "Pass literal filesystem paths to read/edit/write; never use shell substitutions such as $(pwd) or backticks in path arguments",
    "Run shell commands from the current working directory; never scan /, /tmp, the home directory, or an entire drive to find project files, and do not run recursive searches outside the project",
    "Before finishing, re-read the task specification and verify the edge cases it names (empty input, trailing newlines, ranges, and error exits); run the relevant tests, not only the visible ones",
];

fn rules_of(tools: &[ToolInfo]) -> Vec<String> {
    let sections = build_system_prompt_sections(&SystemPromptInput {
        preamble_override: None,
        append: Vec::new(),
        tools,
        hidden_tools: &[],
        project_files: &[],
        skills: &[],
        custom_sections: Vec::new(),
        cwd: std::path::Path::new("/w"),
        package_dir: None,
    });
    sections
        .get(SystemPromptSection::Rules)
        .expect("<rules> section")
        .lines()
        .filter_map(|line| line.strip_prefix("- ").map(str::to_string))
        .collect()
}

fn tool(name: &'static str) -> ToolInfo {
    ToolInfo::new(name, "probe tool")
}

/// read + bash + edit + write: all four bulge in, in this order, after the
/// per-tool guidelines and before the two upstream fixed bullets.
#[test]
fn pir_only_rules_appear_exactly_once_for_the_core_four() {
    let rules = rules_of(&[tool("read"), tool("bash"), tool("edit"), tool("write")]);
    for rule in PIR_ONLY_RULES {
        assert_eq!(
            rules
                .iter()
                .filter(|candidate| candidate.as_str() == rule)
                .count(),
            1,
            "`{rule}` must appear exactly once: {rules:#?}"
        );
    }
    // The upstream tail stays last (upstream `buildRules` ends with these two).
    assert_eq!(rules[rules.len() - 2], "Be concise in your responses");
    assert_eq!(
        rules[rules.len() - 1],
        "Show file paths clearly when working with files"
    );
}

/// Tool-set gating: the read-gate pair needs a reader AND a mutation tool; the
/// cwd rule needs a shell; the finish rule needs a mutation tool or a shell.
/// A read-only selection must not pay for the mutation/shell rules (upstream
/// gates its conditional rule the same way).
#[test]
fn pir_only_rules_are_gated_on_the_tools_that_trigger_them() {
    // read only: none of the four (no mutation tool, no shell).
    let read_only = rules_of(&[tool("read")]);
    for rule in PIR_ONLY_RULES {
        assert!(
            !read_only.iter().any(|candidate| candidate == rule),
            "a read-only selection must not carry `{rule}`: {read_only:#?}"
        );
    }

    // bash only: the cwd rule and the finish rule.
    let bash_only = rules_of(&[tool("bash")]);
    assert!(bash_only.iter().any(|rule| rule == PIR_ONLY_RULES[2]));
    assert!(bash_only.iter().any(|rule| rule == PIR_ONLY_RULES[3]));
    assert!(!bash_only.iter().any(|rule| rule == PIR_ONLY_RULES[0]));
    assert!(!bash_only.iter().any(|rule| rule == PIR_ONLY_RULES[1]));

    // read + edit: the read-gate pair and the finish rule, no shell rule.
    let read_edit = rules_of(&[tool("read"), tool("edit")]);
    assert!(read_edit.iter().any(|rule| rule == PIR_ONLY_RULES[0]));
    assert!(read_edit.iter().any(|rule| rule == PIR_ONLY_RULES[1]));
    assert!(read_edit.iter().any(|rule| rule == PIR_ONLY_RULES[3]));
    assert!(!read_edit.iter().any(|rule| rule == PIR_ONLY_RULES[2]));
}

// ---------------------------------------------------------------------------
// PIR-BIN-061: the docs section only names paths that exist
// ---------------------------------------------------------------------------

static DOCS_FIXTURE_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A throwaway package root for the docs-section probe. Mirrors the two
/// layouts that matter: an installed release (`README.md` only) and a source
/// checkout (`README.md` + `docs/` + `examples/`).
struct DocsFixture {
    root: PathBuf,
}

impl DocsFixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "pir-docs-{name}-{}-{}",
            std::process::id(),
            DOCS_FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create docs fixture root");
        Self { root }
    }

    fn file(&self, relative: &str) -> PathBuf {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create docs fixture parent");
        }
        std::fs::write(&path, "# pir").expect("write docs fixture file");
        path
    }

    fn dir(&self, relative: &str) -> PathBuf {
        let path = self.root.join(relative);
        std::fs::create_dir_all(&path).expect("create docs fixture dir");
        path
    }
}

impl Drop for DocsFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn sections_with_package_dir(
    package_dir: Option<&Path>,
) -> pir::system_prompt::SystemPromptSections {
    let tools = vec![tool("read")];
    build_system_prompt_sections(&SystemPromptInput {
        preamble_override: None,
        append: Vec::new(),
        tools: &tools,
        hidden_tools: &[],
        project_files: &[],
        skills: &[],
        custom_sections: Vec::new(),
        cwd: std::path::Path::new("/w"),
        package_dir,
    })
}

/// The path a docs bullet points at, without the trailing parenthetical
/// description (`Examples: … (extensions, custom tools, SDK)`).
fn documented_path<'a>(docs: &'a str, label: &str) -> &'a str {
    let prefix = format!("- {label}: ");
    let line = docs
        .lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .unwrap_or_else(|| panic!("`{label}` bullet missing from: {docs}"));
    line.split(" (").next().unwrap_or(line)
}

/// The release archive ships only `README.md`; `cargo install` ships nothing
/// next to the binary. Neither layout may produce a `<docs>` section: every
/// line in it names a path under the package root, and the topic index is
/// relative to `docs/` and `examples/`. Before this gate the path-join-only
/// section told an installed pir to read `<install>/docs/tui.md`, which does
/// not exist — worse than omitting the section.
#[test]
fn docs_section_is_absent_when_the_package_tree_lacks_the_asset_set() {
    // A resolved-but-missing package root.
    let missing = PathBuf::from("/nonexistent/pir-package-root");
    assert!(!missing.exists(), "fixture must be a missing directory");
    assert!(sections_with_package_dir(Some(&missing))
        .get(SystemPromptSection::Docs)
        .is_none());

    // Release archive: binary + README.md, no docs/ or examples/.
    let archive = DocsFixture::new("archive");
    archive.file("README.md");
    assert!(
        sections_with_package_dir(Some(&archive.root))
            .get(SystemPromptSection::Docs)
            .is_none(),
        "a README-only install must not point at docs/ or examples/"
    );

    // Partial tree: docs/ is present but examples/ is not.
    let partial = DocsFixture::new("partial");
    partial.file("README.md");
    partial.dir("docs");
    assert!(
        sections_with_package_dir(Some(&partial.root))
            .get(SystemPromptSection::Docs)
            .is_none(),
        "a partial asset tree must not render dangling lines"
    );
}

/// A complete package root (a source checkout): the section is emitted and
/// EVERY path it names exists on disk. The old section was a pure path join,
/// so the same assertion over an installed layout caught `docs/`/`examples/`
/// pointing at nothing.
#[test]
fn docs_section_names_only_paths_that_exist() {
    let package = DocsFixture::new("complete");
    let readme = package.file("README.md");
    let docs = package.dir("docs");
    let examples = package.dir("examples");

    let sections = sections_with_package_dir(Some(&package.root));
    let rendered = sections
        .get(SystemPromptSection::Docs)
        .expect("a complete package root renders <docs>");

    let mut documented = Vec::new();
    for label in ["Main documentation", "Additional docs", "Examples"] {
        let path = documented_path(&rendered, label);
        assert!(
            Path::new(path).exists(),
            "`{label}` must name a path that exists in this install: `{path}`\n{rendered}"
        );
        documented.push(path.to_string());
    }
    assert_eq!(
        documented,
        vec![
            readme.to_string_lossy().replace('\\', "/"),
            docs.to_string_lossy().replace('\\', "/"),
            examples.to_string_lossy().replace('\\', "/"),
        ],
        "the section must name exactly the resolved asset paths"
    );
}

/// The section sits between `<rules>` and `<addendum>`, exactly where upstream
/// emits it (`system-prompt.ts` insertion order).
#[test]
fn docs_section_sits_between_rules_and_addendum() {
    let package = DocsFixture::new("order");
    package.file("README.md");
    package.dir("docs");
    package.dir("examples");
    let tools = vec![tool("read")];
    let sections = build_system_prompt_sections(&SystemPromptInput {
        preamble_override: None,
        append: vec!["APPENDED".to_string()],
        tools: &tools,
        hidden_tools: &[],
        project_files: &[],
        skills: &[],
        custom_sections: Vec::new(),
        cwd: std::path::Path::new("/w"),
        package_dir: Some(&package.root),
    });
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
}

/// `PI_PACKAGE_DIR` still wins when set; a bare build tree next to the binary
/// (no README.md/docs) resolves no package dir, so a `cargo test` run on a
/// developer machine does not silently grow a `<docs>` section.
#[test]
fn package_dir_prefers_the_override_and_needs_a_package_root_next_to_the_binary() {
    // The process-wide env guard (`tests/common/mod.rs`) is shared with the
    // system_prompt suite; both mutate PI_PACKAGE_DIR.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _guard = runtime.block_on(crate::common::agent_dir_env_lock().lock());

    let previous = std::env::var_os("PI_PACKAGE_DIR");

    // The override wins regardless of the binary's directory.
    let override_dir = std::path::PathBuf::from("/override/package");
    std::env::set_var("PI_PACKAGE_DIR", &override_dir);
    assert_eq!(
        pir::config::env::package_dir().as_deref(),
        Some(override_dir.as_path())
    );

    // An EMPTY override falls through to the executable probe.
    std::env::set_var("PI_PACKAGE_DIR", "");
    match std::env::current_exe() {
        Ok(exe) => {
            let dir = exe
                .parent()
                .expect("test binary has a parent")
                .to_path_buf();
            let is_root = dir.join("README.md").is_file() || dir.join("docs").is_dir();
            assert_eq!(
                pir::config::env::package_dir().as_deref(),
                is_root.then_some(dir.as_path()),
                "a bare target dir must not resolve as a package root"
            );
        }
        Err(_) => {
            assert!(pir::config::env::package_dir().is_none());
        }
    }

    match previous {
        Some(value) => std::env::set_var("PI_PACKAGE_DIR", value),
        None => std::env::remove_var("PI_PACKAGE_DIR"),
    }
}
