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
//! 2. **PIR-BIN-061** — the `docs` section is emitted whenever a package
//!    directory resolves, with NO filesystem probe at render time (upstream's
//!    `getReadmePath`/`getDocsPath`/`getExamplesPath` are pure path joins).
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
// PIR-BIN-061: the docs section needs no filesystem probe
// ---------------------------------------------------------------------------

/// A package dir that does not exist still yields the `<docs>` section: the
/// section is a set of PATHS (upstream never probes them), and a
/// mis-installed/missing asset tree is exactly what the model has to be told
/// about. Before the fix a `README.md exists` gate suppressed the section, so
/// every real invocation (where `PI_PACKAGE_DIR` is unset) sent no docs at all.
#[test]
fn docs_section_renders_for_a_package_dir_that_does_not_exist() {
    let tools = vec![tool("read")];
    let missing = std::path::PathBuf::from("/nonexistent/pir-package-root");
    assert!(!missing.exists(), "fixture must be a missing directory");
    let sections = build_system_prompt_sections(&SystemPromptInput {
        preamble_override: None,
        append: Vec::new(),
        tools: &tools,
        hidden_tools: &[],
        project_files: &[],
        skills: &[],
        custom_sections: Vec::new(),
        cwd: std::path::Path::new("/w"),
        package_dir: Some(&missing),
    });
    let docs = sections
        .get(SystemPromptSection::Docs)
        .expect("a resolved package dir always renders <docs>");
    assert!(docs.contains("- Main documentation: "), "{docs}");
    assert!(
        docs.contains("/nonexistent/pir-package-root/README.md"),
        "{docs}"
    );
    assert!(docs.contains("- Additional docs: "), "{docs}");
    assert!(docs.contains("- Examples: "), "{docs}");
}

/// The section sits between `<rules>` and `<addendum>`, exactly where upstream
/// emits it (`system-prompt.ts` insertion order).
#[test]
fn docs_section_sits_between_rules_and_addendum() {
    let tools = vec![tool("read")];
    let package = std::path::PathBuf::from("/pkg");
    let sections = build_system_prompt_sections(&SystemPromptInput {
        preamble_override: None,
        append: vec!["APPENDED".to_string()],
        tools: &tools,
        hidden_tools: &[],
        project_files: &[],
        skills: &[],
        custom_sections: Vec::new(),
        cwd: std::path::Path::new("/w"),
        package_dir: Some(&package),
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
