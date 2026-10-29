//! Tool selection: the shared validation and application of the
//! `settings.defaultTools` list plus the `--tools` / `--exclude-tools` /
//! `--no-tools` / `--no-builtin-tools` CLI flags (upstream
//! `settings-manager.ts` tool helpers and the coding-agent tool selection).

use crate::agent::tool_execution::{ToolExposure, ToolRegistry};
use crate::config::settings::{apply_tool_modifiers, get_tool_list_error, is_tool_modifier};

/// The resolved tool selection (upstream CLI `tools` + `excludeTools`
/// semantics).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolSelection {
    /// Enabled tool names (already matched against the registry).
    pub enabled: Vec<String>,
    /// The `+name`/`-name` entries of a pure-modifier `--tools` list, in list
    /// order (upstream `AgentSessionConfig.defaultToolModifiers`). A session
    /// that uses the default tools applies them to the loadout its transcript
    /// records on resume (upstream `_restoreToolsFromTranscript` with
    /// `modifiers`, agent-session.ts:1815-1825) — not to the base — so a
    /// resumed session keeps the tools it activated during the session, with
    /// the current CLI modifiers applied. Empty for every other form
    /// (allowlist, `--no-tools`, settings-only selection).
    pub modifiers: Vec<String>,
    /// Whether [`Self::enabled`] was derived from the DEFAULT tools — the
    /// settings `defaultTools` list or the
    /// [`crate::config::settings::DEFAULT_TOOL_NAMES`] base, with
    /// [`Self::modifiers`] applied — rather than given as an explicit
    /// selection (upstream `AgentSessionConfig.usesDefaultTools`). Only a
    /// derived selection may be replaced on resume by the loadout the
    /// transcript records; an explicit one (`--tools <names>` / `--no-tools` /
    /// `--no-builtin-tools`) keeps the base, exactly like upstream's restore
    /// gate (`_restoreToolsFromTranscript`).
    ///
    /// `Default` (so every literal that does not set it) means an explicit
    /// selection, the conservative reading.
    pub uses_default_tools: bool,
}

/// The global tool-selection inputs: the resolved settings `defaultTools`
/// layer plus the four CLI flags. Built once by the CLI host and handed to
/// every mode host, so RPC, the server and the TUI cannot drift.
///
/// A host resolves this against ITS OWN available-tools set (its run registry
/// or the facts it can enumerate before that registry exists), not against a
/// `ToolSelection` another host resolved: the RPC registry is core + builtin
/// plugins + on-disk JS tools, while the server's is core only and physically
/// drops the shell tools under `allow_remote_bash: false`
/// (`src/server/session.rs`). Resolving after the host's registry is known is
/// the D10 rule (an on-disk `--tools my_js_tool` must stay selectable).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolSelectionRequest {
    /// The resolved settings `defaultTools` list, exactly as the settings
    /// manager returns it.
    pub settings_default_tools: Option<Vec<String>>,
    /// `--tools` entries (allowlist or pure `+name`/`-name` modifiers).
    pub cli_tools: Vec<String>,
    /// `--exclude-tools` entries.
    pub cli_exclude: Vec<String>,
    /// `--no-tools`.
    pub no_tools: bool,
    /// `--no-builtin-tools`.
    pub no_builtin_tools: bool,
}

impl ToolSelectionRequest {
    /// Cheap gate: hosts must call this BEFORE enumerating tools or
    /// instantiating builtin plugins. `false` means no selection change.
    pub fn requested(&self) -> bool {
        tool_selection_requested(
            self.settings_default_tools.as_deref(),
            &self.cli_tools,
            &self.cli_exclude,
            self.no_tools,
            self.no_builtin_tools,
        )
    }

    /// Resolve against a host's `available_tools`, using the upstream
    /// `DEFAULT_TOOL_NAMES` base. This is the ONE entry point that emits the
    /// compiled-out-tool diagnostics (`compiled_out_selection_diagnostics`
    /// is already called inside `resolve_tool_selection_with_availability`).
    ///
    /// `Ok(None)` means the request does not change the default registry
    /// (no flags and no settings selection) — the host keeps every tool's
    /// exposure default.
    pub fn resolve(
        &self,
        available_tools: &[AvailableTool],
    ) -> Result<Option<ToolSelection>, String> {
        if !self.requested() {
            return Ok(None);
        }
        resolve_tool_selection_with_availability(
            self.settings_default_tools.as_deref(),
            &self.cli_tools,
            &self.cli_exclude,
            self.no_tools,
            self.no_builtin_tools,
            available_tools,
            &crate::config::settings::DEFAULT_TOOL_NAMES,
        )
    }
}

/// One entry of a host's `available_tools` set: the registered name plus what
/// the host knows about that tool's registration.
///
/// Names alone suffice to match a plain allowlist/exclude entry (upstream
/// matches the registered set by name), but the two DEFAULT bases are
/// registration rules: an exclude-only selection starts from the tools that
/// activate on registration, and `--no-builtin-tools` keeps the extension
/// tools that activate on registration (upstream `_isActivatedOnRegistration`,
/// agent-session.ts:3632-3634, reached through `includeAllExtensionTools`,
/// agent-session.ts:3608-3613). A host that resolves AFTER its run registry
/// exists (print, the session host) passes every tool's exposure and
/// `default_active` flag; the SDK builder feeds the name-only
/// [`resolve_tool_selection`] with its own `available`/`defaults` name sets; a
/// host that resolves after its extension RUNTIME loaded but before the
/// session/plugin host exists (the interactive TUI) gets the builtin plan's
/// facts from [`builtin_plan_available_tools`] plus the on-disk extensions'
/// facts from [`runtime_available_tools`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableTool {
    /// The registered tool name.
    pub name: String,
    /// The tool's registration facts. `None` is a name-only entry.
    pub registration: Option<ToolRegistration>,
}

/// The registration facts upstream `_isActivatedOnRegistration` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolRegistration {
    /// The tool's exposure class.
    pub exposure: ToolExposure,
    /// Upstream `definition.defaultActive !== false`.
    pub default_active: bool,
}

impl ToolRegistration {
    /// Whether registering the tool activates it (upstream
    /// `_isActivatedOnRegistration`: a declarable exposure AND
    /// `defaultActive !== false`).
    pub fn activates_on_registration(self) -> bool {
        self.exposure.active_on_registration(self.default_active)
    }
}

impl AvailableTool {
    /// A name the host knows is registered, with its registration facts.
    pub fn registered(
        name: impl Into<String>,
        exposure: ToolExposure,
        default_active: bool,
    ) -> Self {
        Self {
            name: name.into(),
            registration: Some(ToolRegistration {
                exposure,
                default_active,
            }),
        }
    }

    /// A name the host knows only by name (its runtime has not loaded yet).
    pub fn name_only(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            registration: None,
        }
    }

    /// Every tool of `registry`, with its registration facts.
    pub fn from_registry(registry: &ToolRegistry) -> Vec<Self> {
        registry
            .iter()
            .map(|tool| Self::registered(tool.name(), tool.exposure(), tool.default_active()))
            .collect()
    }

    /// Upstream `_isActivatedOnRegistration` for this entry, or `None` when
    /// the host only knows the name.
    pub fn activates_on_registration(&self) -> Option<bool> {
        self.registration
            .map(ToolRegistration::activates_on_registration)
    }
}

/// Validate and resolve the tool selection from settings + CLI flags, given
/// only the registered NAMES.
///
/// This is the name-only compatibility entry point: core names get the core
/// registration's exposure facts from the real core registration pass, while a
/// non-core name keeps the name-only rules ([`AvailableTool::name_only`]).
/// A host that can describe its tools should call
/// [`resolve_tool_selection_with_availability`] instead, so the
/// `--exclude-tools` and `--no-builtin-tools` bases follow upstream
/// `_isActivatedOnRegistration` for plugin tools too (the interactive TUI does
/// this with [`builtin_plan_available_tools`] plus, once the on-disk
/// extensions have loaded, [`runtime_available_tools`]).
///
/// Semantics (upstream `--tools` / `--exclude-tools` help text):
/// - plain `--tools` entries are an allowlist of names or `*` patterns; a
///   pure `+name`/`-name` modifier list adds to / removes from the defaults;
///   the two forms must not be mixed;
/// - MCP tools (`mcp__*`) are only in the returned active set when the base
///   or an allowlist entry names or matches them; an unnamed one stays
///   REGISTERED (upstream `_isAllowedTool` vs `_isActivatable`,
///   agent-session.ts:1542-1558) and reachable through `codemode`/
///   `tool_search`, never declared;
/// - `--exclude-tools` applies to everything, MCP tools included;
/// - `--no-tools` disables everything; `--no-builtin-tools` keeps the
///   extension/custom tools that activate on registration.
///
/// `builtin_tool_names` is the base the enabled list starts from for the
/// settings/modifier paths (upstream `DEFAULT_TOOL_NAMES`, sdk.ts:282-286 —
/// CLI hosts pass exactly that 4-name default). Two paths deliberately use a
/// different base:
/// - `--exclude-tools` without `--tools`/settings starts from the tools that
///   activate on registration and are actually present, plus any default names
///   it lacks — a denylist filters the default set, it never adds to it
///   (upstream `_isActivatedOnRegistration`, agent-session.ts:3632-3634);
/// - `--no-builtin-tools` keeps the non-core tools that activate on
///   registration (upstream `noTools: "builtin"` empties only the default
///   base and the refresh pass activates the extension tools with
///   `_isActivatedOnRegistration`, agent-session.ts:3608-3613).
///
/// `available_tools` must be every name the host's run registry can hold —
/// the core tools AND the plugin/extension tools — because plain allowlist and
/// exclude entries are matched against it (upstream filters the registered
/// set, so a name that exists must be selectable). Hosts that resolve after
/// loading extensions pass their merged registry's names/facts; the
/// interactive TUI resolves the instant its on-disk extension runtime exists
/// (upstream loads extensions in `_buildRuntime`, before
/// `_refreshToolRegistry` applies the allowlist) and passes the core
/// registry's facts plus [`builtin_plan_available_tools`] and
/// [`runtime_available_tools`].
///
/// Returns `Ok(None)` when nothing changes the default registry contents.
pub fn resolve_tool_selection(
    settings_default_tools: Option<&[String]>,
    cli_tools: &[String],
    cli_exclude: &[String],
    no_tools: bool,
    no_builtin_tools: bool,
    available_tools: &[String],
    builtin_tool_names: &[&str],
) -> Result<Option<ToolSelection>, String> {
    let core = core_registrations();
    let available: Vec<AvailableTool> = available_tools
        .iter()
        .map(|name| match core.get(name) {
            Some(registration) => AvailableTool::registered(
                name.clone(),
                registration.exposure,
                registration.default_active,
            ),
            None => AvailableTool::name_only(name.clone()),
        })
        .collect();
    resolve_tool_selection_with_availability(
        settings_default_tools,
        cli_tools,
        cli_exclude,
        no_tools,
        no_builtin_tools,
        &available,
        builtin_tool_names,
    )
}

/// [`resolve_tool_selection`] over [`AvailableTool`] entries: the resolver
/// every host that has already built its run registry uses (print, the
/// session host). The SDK builder resolves through the name-only
/// [`resolve_tool_selection`] instead, feeding it its own `available` and
/// `defaults` name sets.
///
/// The name-only rules of [`resolve_tool_selection`] remain for entries the
/// host cannot describe ([`AvailableTool::name_only`]): such a name is kept by
/// `--no-builtin-tools` (the historical name-only rule) and stays out of the
/// exclude-only base.
///
/// `available_tools` must be every tool the host's run registry can hold —
/// the core tools AND the plugin/extension tools — because plain allowlist and
/// exclude entries are matched against it (upstream filters the registered
/// set, so a name that exists must be selectable).
///
/// Returns `Ok(None)` when nothing changes the default registry contents.
pub fn resolve_tool_selection_with_availability(
    settings_default_tools: Option<&[String]>,
    cli_tools: &[String],
    cli_exclude: &[String],
    no_tools: bool,
    no_builtin_tools: bool,
    available_tools: &[AvailableTool],
    builtin_tool_names: &[&str],
) -> Result<Option<ToolSelection>, String> {
    if let Some(error) = get_tool_list_error(cli_tools) {
        return Err(format!("--tools: {error}"));
    }

    // Unchanged selection: everything stays enabled (including MCP tools).
    if !tool_selection_requested(
        settings_default_tools,
        cli_tools,
        cli_exclude,
        no_tools,
        no_builtin_tools,
    ) {
        return Ok(None);
    }

    // A pure-modifier `--tools` list changes the default selection instead of
    // replacing it (upstream sdk.ts:283-286 `toolModifiers`); the list also
    // rides along on the selection (upstream `defaultToolModifiers`) so a
    // resumed session can apply it to the loadout its transcript records.
    let tool_modifiers =
        !cli_tools.is_empty() && cli_tools.iter().all(|entry| is_tool_modifier(entry));

    // An explicit request for a name whose surface this build compiled out is
    // reported instead of silently resolving to an empty selection; a
    // genuinely unknown name stays silent (upstream drops it). The selection
    // itself is unchanged: the name simply matches nothing.
    for message in compiled_out_selection_diagnostics(
        settings_default_tools,
        cli_tools,
        available_tools,
        no_tools,
        no_builtin_tools,
    ) {
        eprintln!("pir: {message}");
    }

    // Start from the settings `defaultTools` selection (already resolved by
    // the settings manager) or the built-in defaults. `--no-builtin-tools`
    // keeps only the non-core (extension/custom) tools that activate on
    // registration: upstream `noTools: "builtin"` empties the default base and
    // the refresh pass activates extension tools with
    // `_isActivatedOnRegistration` (`includeAllExtensionTools`,
    // agent-session.ts:3608-3613, 3632-3634), so a `defaultActive: false` or
    // non-declarable plugin tool stays callable but UNDECLARED.
    let mut enabled: Vec<String> = if no_tools {
        Vec::new()
    } else if no_builtin_tools {
        available_tools
            .iter()
            .filter(|tool| {
                !crate::coding_agent::tools::CORE_TOOL_NAMES.contains(&tool.name.as_str())
            })
            .filter(|tool| tool.activates_on_registration().unwrap_or(true))
            .map(|tool| tool.name.clone())
            .collect()
    } else {
        match settings_default_tools {
            Some(tools) => tools.to_vec(),
            None => builtin_tool_names
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
        }
    };
    // upstream `usesDefaultTools` (sdk.ts:472): the active set came from the
    // DEFAULT tools — the settings `defaultTools` list or the
    // `DEFAULT_TOOL_NAMES` base, with `+name`/`-name` entries applied — so a
    // resumed session may replace it with the loadout its transcript records.
    // `--tools <plain names>`, `--no-tools` and `--no-builtin-tools` select
    // explicitly and keep the base.
    let uses_default_tools =
        !no_tools && !no_builtin_tools && (cli_tools.is_empty() || tool_modifiers);
    // CLI allowlist: a pure-modifier list appends to the current selection;
    // plain names/patterns replace it. An explicit `--tools` list applies with
    // or without `--no-tools`/`--no-builtin-tools`: those flags suppress the
    // DEFAULT BASE only (upstream sdk.ts:282 `defaultToolNames = options.noTools
    // ? [] : ...`), while the explicit list still selects (sdk.ts:283-291
    // `selectedToolNames = options.tools`, `allowedToolNames = options.tools`).
    // The SDK builder's `.no_tools().tools([...])` path already declares the
    // named tools (sdk/builder.rs:1469-1509), so the CLI hosts must too — the
    // old `!no_tools` guard made `--no-tools --tools read` declare nothing.
    if !cli_tools.is_empty() {
        if tool_modifiers {
            enabled = apply_tool_modifiers(&enabled, cli_tools);
        } else {
            enabled = match_allowlist(cli_tools, available_tools);
        }
    } else if !no_tools && !cli_exclude.is_empty() && settings_default_tools.is_none() {
        // Exclude-only with no settings selection: the base is every tool that
        // activates on registration AND is actually present — the core tools
        // plus the extension tools upstream's `includeAllExtensionTools` pass
        // activates (agent-session.ts:3608-3613) — plus any default names it
        // lacks. A denylist must never ACTIVATE a tool the default run leaves
        // off (`powershell` is registered with `default_active: false`) nor
        // un-declare an unrelated default-active plugin tool (`ctx_execute`,
        // `todo`, ...). Name-only entries (the interactive TUI resolves before
        // its runtime exists) stay out of this base unless their name is a
        // core registration: the host cannot know their exposure yet.
        enabled = available_tools
            .iter()
            .filter(|tool| tool.activates_on_registration().unwrap_or(false))
            .map(|tool| tool.name.clone())
            .collect();
        for name in builtin_tool_names {
            if !enabled.iter().any(|tool| tool == name) {
                enabled.push((*name).to_string());
            }
        }
    }
    // ------------------------------------------------------------------
    // MCP tools: registration is not activation (upstream `_isAllowedTool`
    // vs `_isActivatable`, agent-session.ts:1542-1558)
    // ------------------------------------------------------------------
    //
    // Upstream keeps an MCP tool the allowlist does not name or match
    // REGISTERED (`_isAllowedTool`) but never adds it to the loadout:
    // `_buildRuntime`'s allowlist branch pushes only `allowedTools(toolName)`
    // matches (agent-session.ts:3600-3607). `_isActivatable` (1552-1558) is the
    // gate for the paths that later re-activate a kept-registered name (a
    // recorded transcript loadout, an extension `setActiveTools`); it admits a
    // non-`direct` exposure there, and only with `tool_search` registered so
    // discovery can load it. It never feeds the initial active set.
    //
    // `enabled` IS the active/declared set here — every host hands it to
    // `ToolRegistry::active_names` as the override and declares exactly it
    // (`print.rs::print_tool_loadout`, `agent_session::tools::
    // build_tool_loadout`) — so an MCP name in this list is DECLARED. The old
    // unconditional `mcp__*` push therefore did not "keep MCP tools
    // registered" (registration is unconditional in pir, the F12 rule: a
    // selection never filters the registry); it declared every registered MCP
    // tool, `direct` exposure included, which upstream forbids ("An MCP tool
    // that no entry names or matches is never declared directly, whatever its
    // exposure"), and it removed `codemode`/`deferred` tools from
    // `tool_search`'s candidate set (it searches the NOT-yet-active tools).
    //
    // So no MCP name is added here. The tool stays registered (callable from
    // `codemode`, discoverable by `tool_search`, loaded later into the
    // session's active override), and only a name the selection names or
    // matches — an `mcp__*` allowlist entry/pattern, a settings `defaultTools`
    // entry, or the `--no-builtin-tools` base — is activated. That keeps the
    // `mcp_named` behaviour: a named entry is selected by `match_allowlist`,
    // and an unnamed MCP tool is never added either way. `--no-tools`
    // (upstream's zero-length allowlist) already added nothing, and the
    // exclude-only base now treats MCP tools like every other non-core tool:
    // a denylist removes names, it never activates them.

    // Denylist applies to everything, MCP tools included.
    if !cli_exclude.is_empty() {
        let excluded = match_allowlist(cli_exclude, available_tools);
        enabled.retain(|name| !excluded.contains(name));
    }

    // Dedupe while preserving order.
    let mut seen = std::collections::BTreeSet::new();
    enabled.retain(|name| seen.insert(name.clone()));

    // Resume restore inputs (upstream `defaultToolModifiers` +
    // `_restoreToolsFromTranscript`): a default-derived selection is REPLACED
    // by the loadout the transcript records, with these entries applied. The
    // exclusions must ride along as `-name` entries, or resuming a session
    // re-declares a tool the user excluded: upstream cannot do that (its
    // `_isAllowedTool` keeps the excluded tool out of the registry, so the
    // recorded name has nothing to resolve against), while pir's registry
    // stays complete by design (the F12 rule). The SDK builder re-expresses
    // its denylist the same way (sdk/builder.rs:1569-1587); doing it here
    // gives every default-derived host the same resume behaviour.
    let mut modifiers: Vec<String> = if tool_modifiers {
        cli_tools.to_vec()
    } else {
        Vec::new()
    };
    if uses_default_tools && !cli_exclude.is_empty() {
        // Exact names: the plain entries, registered or not (a tool that
        // registers later — MCP discovery, a late JS extension — must stay
        // excluded too).
        let mut deny: Vec<String> = match_allowlist(cli_exclude, available_tools);
        for entry in cli_exclude {
            // Only `*` makes an entry a pattern (upstream
            // `createToolNameMatcher`, mcp-servers.ts:12-33); `?` is a literal
            // name character, so it rides along as an exact `-name` entry.
            if !entry.contains('*') {
                deny.push(entry.clone());
            }
        }
        let mut push_modifier = |entry: String| {
            if entry.len() > 1 && !modifiers.contains(&entry) {
                modifiers.push(entry);
            }
        };
        for name in deny {
            push_modifier(format!("-{name}"));
        }
        for entry in cli_exclude {
            if entry.contains('*') {
                // A pattern must survive as a MATCHER, not as the names it
                // matches today: a bridge discovered later (MCP discovery, a
                // late JS extension) is excluded at registration time
                // upstream (`_excludedTools`). The `mcp` builtin and the
                // direct-bridge union read `-<pattern>` entries through
                // [`tool_excluded_by`], so the entry is kept verbatim here.
                push_modifier(format!("-{entry}"));
            }
        }
    }
    Ok(Some(ToolSelection {
        enabled,
        modifiers,
        uses_default_tools,
    }))
}

// ---------------------------------------------------------------------------
// Compiled-out tool surfaces: the `pir:` diagnostics for an explicit request
// ---------------------------------------------------------------------------

/// The tool-name → cargo-feature table for surfaces a full build registers but
/// a trimmed build compiles out.
///
/// Mirrors [`crate::plugins::builtin::BUILTIN_FEATURES`] for the tool names the
/// builtin plugins register: `subagent`/`python`/`todo`/`codemode` are both the
/// builtin and the tool name, `tool-search` registers `tool_search`, and the
/// `context-mode` builtin registers the `ctx_*` family. Each core built-in tool
/// names its own leaf feature (`tool-read`, `tool-bash`, …), so a partial build
/// reports exactly the surface it lacks instead of the legacy aggregate
/// `tools` wording. The core `grep`/`find` pair stays on `grep-find` (which
/// implies `tools`, hence every leaf) until the deferred `tool-grep`/
/// `tool-find` split. The `mcp` builtin registers server-derived `mcp__*` names
/// that cannot be known statically (a user JS extension can register the same
/// shape), so it is not listed: such a name stays silent rather than guessing.
/// `tool-repair` and `phone` register no tools. Provider features have no tool
/// surface — a provider selects a wire adapter, never a tool name — so they
/// are deliberately absent.
///
/// The table is only consulted for a name the host's `available_tools` does
/// NOT contain, and only a row whose feature is actually off diagnoses (see
/// [`compiled_out_tool_feature`]), so a disabled builtin in a full build stays
/// silent, like an unknown name.
const TOOL_SURFACE_FEATURES: &[(&str, &str)] = &[
    ("read", "tool-read"),
    ("bash", "tool-bash"),
    ("powershell", "tool-powershell"),
    ("edit", "tool-edit"),
    ("write", "tool-write"),
    ("ls", "tool-ls"),
    ("background_status", "tool-background"),
    ("background_abort", "tool-background"),
    ("grep", "grep-find"),
    ("find", "grep-find"),
    ("subagent", "subagent"),
    ("tool_search", "tool-search"),
    ("todo", "todo"),
    ("python", "python"),
    ("codemode", "extensions"),
    ("ctx_execute", "context-mode"),
    ("ctx_execute_file", "context-mode"),
    ("ctx_batch_execute", "context-mode"),
    ("ctx_index", "context-mode"),
    ("ctx_search", "context-mode"),
    ("ctx_fetch_and_index", "context-mode"),
    ("ctx_stats", "context-mode"),
    ("ctx_purge", "context-mode"),
    ("ctx_doctor", "context-mode"),
];

/// Whether `feature` is compiled into this build (a compile-time constant per
/// [`TOOL_SURFACE_FEATURES`] row).
fn tool_feature_compiled_in(feature: &str) -> bool {
    match feature {
        // The legacy aggregate row: kept for any table entry that has not (or
        // cannot) name a leaf. Every current core row names its leaf, so this
        // arm only matters to a future/third-party table addition.
        "tools" => cfg!(feature = "tools"),
        "tool-read" => cfg!(feature = "tool-read"),
        "tool-bash" => cfg!(feature = "tool-bash"),
        "tool-powershell" => cfg!(feature = "tool-powershell"),
        "tool-edit" => cfg!(feature = "tool-edit"),
        "tool-write" => cfg!(feature = "tool-write"),
        "tool-ls" => cfg!(feature = "tool-ls"),
        "tool-background" => cfg!(feature = "tool-background"),
        "grep-find" => cfg!(feature = "grep-find"),
        "subagent" => cfg!(feature = "subagent"),
        "tool-search" => cfg!(feature = "tool-search"),
        "todo" => cfg!(feature = "todo"),
        "python" => cfg!(feature = "python"),
        "extensions" => cfg!(feature = "extensions"),
        "context-mode" => cfg!(feature = "context-mode"),
        // A row naming a feature this match does not know is treated as
        // compiled in, so adding a table entry cannot claim "compiled out"
        // before its cfg arm exists.
        _ => true,
    }
}

/// The cargo feature gating the tool NAME `name`, but only when this build
/// compiled that surface out. `None` for an available/unknown name or an
/// enabled feature: those stay silent (an unknown name is upstream's silent
/// drop).
pub fn compiled_out_tool_feature(name: &str) -> Option<&'static str> {
    let (_, feature) = TOOL_SURFACE_FEATURES
        .iter()
        .find(|(tool, _)| *tool == name)?;
    (!tool_feature_compiled_in(feature)).then_some(*feature)
}

/// The diagnostic line (without the `pir:` prefix the caller emits) for a
/// tool surface compiled out of this build.
pub fn unavailable_tool_message(name: &str, feature: &str) -> String {
    format!("tool `{name}` is unavailable: this build was compiled without the `{feature}` feature")
}

/// [`unavailable_tool_message`] for a known-but-compiled-out `name`; `None`
/// when the name is unknown or its feature is compiled in.
pub fn compiled_out_tool_message(name: &str) -> Option<String> {
    compiled_out_tool_feature(name).map(|feature| unavailable_tool_message(name, feature))
}

/// The `pir:` diagnostics (without their prefix) for EXPLICIT tool-selection
/// entries whose surface is compiled out of this build, in request order and
/// deduplicated. Empty when every requested name is available or genuinely
/// unknown.
///
/// Explicit requests are a plain `--tools` allowlist, the settings
/// `defaultTools` base (only when a plain CLI allowlist does not replace it),
/// and `+name` modifiers. `-name` exclusions and `*` patterns stay silent: an
/// exclusion of an absent tool is a no-op by design, and a pattern is not a
/// name (upstream `createToolNameMatcher`; only `*` is a wildcard).
pub fn compiled_out_selection_diagnostics(
    settings_default_tools: Option<&[String]>,
    cli_tools: &[String],
    available_tools: &[AvailableTool],
    no_tools: bool,
    no_builtin_tools: bool,
) -> Vec<String> {
    let tool_modifiers =
        !cli_tools.is_empty() && cli_tools.iter().all(|entry| is_tool_modifier(entry));
    let mut reported: Vec<String> = Vec::new();
    let mut messages: Vec<String> = Vec::new();
    let mut report = |name: &str| {
        if available_tools.iter().any(|tool| tool.name == name) {
            return;
        }
        let Some(message) = compiled_out_tool_message(name) else {
            return;
        };
        if !reported.iter().any(|existing| existing == name) {
            reported.push(name.to_owned());
            messages.push(message);
        }
    };
    // The settings `defaultTools` list is the default BASE only when the CLI
    // replacement paths (`--no-tools`/`--no-builtin-tools`) or a plain `--tools`
    // allowlist do not replace it, exactly like the selection resolution below.
    if !no_tools && !no_builtin_tools && (cli_tools.is_empty() || tool_modifiers) {
        if let Some(tools) = settings_default_tools {
            for entry in tools {
                if !entry.contains('*') {
                    report(entry);
                }
            }
        }
    }
    if tool_modifiers {
        for entry in cli_tools {
            if let Some(name) = entry.strip_prefix('+') {
                if !name.is_empty() {
                    report(name);
                }
            }
        }
    } else {
        for entry in cli_tools {
            if !entry.contains('*') {
                report(entry);
            }
        }
    }
    messages
}

// ---------------------------------------------------------------------------
// Plugin/builtin tool names: the `available_tools` set of a host that resolves
// its selection BEFORE its extension runtime exists
// ---------------------------------------------------------------------------

/// Whether a selection was requested at all: any of the `--tools` /
/// `--exclude-tools` / `--no-tools` / `--no-builtin-tools` flags, or a
/// settings `defaultTools` list. `false` means the run keeps every tool's
/// exposure default and [`resolve_tool_selection`] returns `Ok(None)` without
/// looking at the available set.
///
/// Hosts that build their available set from something expensive (the
/// interactive host instantiates the builtin plugins for it) gate on this
/// first.
pub fn tool_selection_requested(
    settings_default_tools: Option<&[String]>,
    cli_tools: &[String],
    cli_exclude: &[String],
    no_tools: bool,
    no_builtin_tools: bool,
) -> bool {
    !cli_tools.is_empty()
        || !cli_exclude.is_empty()
        || no_tools
        || no_builtin_tools
        || settings_default_tools.is_some()
}

/// The tools the compiled builtin plugin factories register for
/// `builtin_names` (`builtin:<name>` entries, without the prefix), WITH their
/// registration facts (`exposure`, `default_active`).
///
/// The names and the facts come from the factories' OWN registration pass —
/// the one [`crate::plugins::PluginHost::build`] runs — so a builtin that
/// adds, renames, re-exposes or re-activates a tool cannot drift from what
/// the run registry holds. An unknown or compiled-out `builtin:<name>`
/// contributes nothing, like the host's diagnostic path; a builtin whose
/// `register()` fails contributes nothing either, because the host keeps such
/// a plugin out of its registry.
///
/// The facts are exactly what the merged registry exposes to the shared
/// resolver: `parse(&definition.exposure)` with the `direct` fallback the
/// session/print registry uses for a JS-backed tool
/// ([`crate::agent::tool_execution::ExtensionTool`]), and the
/// `defaultActive` flag. A host that resolves before its runtime exists (the
/// interactive TUI) passes these entries to
/// [`resolve_tool_selection_with_availability`], so `--no-builtin-tools`
/// keeps only the registration-active plugin tools (not `codemode`,
/// `tool_search`, the codemode model bridges or a deferred MCP resource
/// tool), and `--exclude-tools` keeps the default-active ones
/// (`ctx_execute`, `todo`, ...).
///
/// Only the registration pass runs: no [`crate::plugins::PluginHost`], no JS
/// engine/actor, no event dispatch and no plugin lifecycle. The throwaway
/// plugin instances are dropped with their registrations.
pub fn plugin_available_tools<'a>(
    builtin_names: impl IntoIterator<Item = &'a str>,
) -> Vec<AvailableTool> {
    let mut tools: Vec<AvailableTool> = Vec::new();
    for name in builtin_names {
        let Some(plugin) = crate::plugins::instantiate_builtin(name) else {
            continue;
        };
        let mut registrations = crate::plugins::PluginRegistrations::default();
        let registered = {
            let mut context = crate::plugins::RegistrationContext::new(&mut registrations);
            plugin.register(&mut context)
        };
        if registered.is_err() {
            continue;
        }
        for (definition, _) in std::mem::take(&mut registrations.tools) {
            if !tools.iter().any(|tool| tool.name == definition.name) {
                tools.push(AvailableTool::registered(
                    definition.name,
                    // The same parse + fallback `ExtensionTool::new` applies to
                    // a JS-backed `ToolDef` (`ToolExposure::parse(..)
                    // .unwrap_or(Direct)`), so the pre-runtime facts equal the
                    // merged registry's.
                    ToolExposure::parse(&definition.exposure).unwrap_or(ToolExposure::Direct),
                    definition.default_active,
                ));
            }
        }
    }
    tools
}

/// Tool names the compiled builtin plugin factories register for
/// `builtin_names` — the name-only view of [`plugin_available_tools`].
pub fn plugin_tool_names<'a>(builtin_names: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    plugin_available_tools(builtin_names)
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

/// The tools the builtin plugin host registers for a CLI-derived extension
/// plan, WITH their registration facts, resolved WITHOUT an extension runtime.
///
/// The interactive host must resolve `--tools`/`defaultTools` before the
/// runtime exists (the TUI worker loads it later, together with the
/// `AgentSession`), so its `available_tools` set is built here instead of from
/// a merged registry. The plan is derived with the loader functions the real
/// host uses — [`crate::plugins::order_extension_paths`] (default-enabled
/// builtins join the CLI `-e` entries; `no_extensions` keeps only explicit
/// ones), [`crate::plugins::filter_disabled_builtins`] and
/// [`crate::plugins::partition_paths`] — so those flag semantics are shared
/// with the run plan, not re-implemented.
///
/// `disabled_builtins` is the SAME set the plan builder passes to
/// `filter_disabled_builtins` — the CLI composes it with
/// `SettingsManager::disabled_builtins_with_flags` (`disabledBuiltinExtensions`,
/// `pythonTool: false` and `--no-mcp` in one place), so a settings key cannot
/// disable a builtin in the run plan while leaving its tools selectable here.
///
/// On-disk extension paths contribute nothing HERE: their tool names exist
/// only once the QuickJS runtime has evaluated them. That is why the
/// interactive host calls [`runtime_available_tools`] with the runtime the
/// CLI's strict extension validation already loaded, and why the print host
/// and the SDK builder resolve against their fully merged registry: all three
/// resolve after the on-disk tools are registered (upstream
/// `_refreshToolRegistry`, agent-session.ts:3525-3606).
///
/// The plan here is the CLI `-e` entries plus the default-enabled builtins,
/// which is every builtin a tool name can come from: the only non-default
/// builtin is `phone`, and it registers commands, not tools.
pub fn builtin_plan_available_tools(
    cli_paths: &[std::path::PathBuf],
    no_extensions: bool,
    disabled_builtins: &std::collections::BTreeSet<String>,
) -> Vec<AvailableTool> {
    let no_default_paths: Vec<std::path::PathBuf> = Vec::new();
    let plan = crate::plugins::order_extension_paths(cli_paths, &no_default_paths, no_extensions);
    let plan = crate::plugins::filter_disabled_builtins(&plan, disabled_builtins);
    let (builtins, _files) = crate::plugins::partition_paths(&plan);
    plugin_available_tools(builtins.iter().map(String::as_str))
}

/// Tool names the builtin plugin host registers for a CLI-derived extension
/// plan — the name-only view of [`builtin_plan_available_tools`], kept for
/// callers that only need to match or enumerate names.
pub fn builtin_plan_tool_names(
    cli_paths: &[std::path::PathBuf],
    no_extensions: bool,
    disabled_builtins: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    builtin_plan_available_tools(cli_paths, no_extensions, disabled_builtins)
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

/// Builtin-plugin facts for an ALREADY-RESOLVED extension plan (the final
/// `Vec<PathBuf>` the CLI host passes to the RPC host, after ordering and
/// `filter_disabled_builtins`). Same registration pass as
/// [`builtin_plan_available_tools`], without re-running discovery.
///
/// RPC resolves its selection after loading the extension runtime, and the
/// runtime's recorded path plan is the exact plan its session's plugin host
/// will instantiate — including settings/package paths the CLI-input helper
/// above cannot see. The two helpers agree for an equivalent plan; the
/// `extension_plan_available_tools_matches_builtin_plan_available_tools` unit
/// test pins that.
pub fn extension_plan_available_tools(plan: &[std::path::PathBuf]) -> Vec<AvailableTool> {
    let (builtins, _files) = crate::plugins::partition_paths(plan);
    plugin_available_tools(builtins.iter().map(String::as_str))
}

/// Core facts + builtin-plan facts + the loaded runtime's on-disk tools, first
/// name wins.
///
/// This is exactly the merge the interactive TUI performs inline
/// (`src/main_cli/print_mode.rs::resolve_interactive_tool_selection`); RPC
/// needs the same composition so an on-disk `--tools my_js_tool` is selectable
/// there too. The sort is stable and core entries are appended first, then the
/// builtin plan, then the runtime's JS tools, so a plugin tool that shadows a
/// core name keeps the core facts and a JS tool that shadows a builtin keeps
/// the builtin facts — the same rule `merge_plugin_tools` applies.
pub fn host_available_tools(
    core: &ToolRegistry,
    builtin_tools: Vec<AvailableTool>,
    runtime: Option<&crate::extensions::ExtensionHandle>,
) -> Vec<AvailableTool> {
    let mut available = AvailableTool::from_registry(core);
    available.extend(builtin_tools);
    if let Some(runtime) = runtime {
        available.extend(runtime_available_tools(runtime));
    }
    available.sort_by(|left, right| left.name.cmp(&right.name));
    available.dedup_by(|left, right| left.name == right.name);
    available
}

/// The tools a LOADED extension runtime has registered, WITH their
/// registration facts (`exposure`, `defaultActive`).
///
/// The interactive host resolves its `--tools`/`--exclude-tools`/
/// `--no-builtin-tools` selection after the CLI's strict extension validation
/// has evaluated the on-disk extension files. Upstream does the same: its
/// extensions are loaded in `_buildRuntime` and the resulting registry is
/// filtered by `_refreshToolRegistry` (agent-session.ts:3525-3606,
/// 3636-3688), so a tool an on-disk `pi.registerTool` declared must be
/// selectable. The names only exist in
/// [`crate::extensions::ExtensionHandle::tool_defs`] after the runtime loaded
/// them; a static/builtin-only enumeration cannot know them. The builtin plan
/// facts come from [`builtin_plan_available_tools`].
///
/// These are the same [`crate::extensions::ToolDef`]s
/// [`crate::agent::tool_execution::merge_plugin_tools`] later wraps when the
/// session's plugin host merges the JS adapter, so the facts equal the merged
/// registry's: `ToolExposure::parse` with the `direct` fallback
/// (`ExtensionTool::new` applies it to a JS-backed definition), and its
/// `defaultActive` flag. A repeated name keeps the first registration,
/// matching the host's first-name-wins rule.
pub fn runtime_available_tools(runtime: &crate::extensions::ExtensionHandle) -> Vec<AvailableTool> {
    let mut tools: Vec<AvailableTool> = Vec::new();
    for definition in runtime.tool_defs() {
        if tools.iter().any(|tool| tool.name == definition.name) {
            continue;
        }
        tools.push(AvailableTool::registered(
            definition.name,
            ToolExposure::parse(&definition.exposure).unwrap_or(ToolExposure::Direct),
            definition.default_active,
        ));
    }
    tools
}

/// The registration facts of every core tool, keyed by name: what the real
/// registration pass ([`crate::coding_agent::tools::register_core_tools`])
/// produced, not a hand-maintained table, so a core tool that changes its
/// `default_active` (or a feature-gated core tool appearing/disappearing)
/// cannot drift from what the hosts declare.
///
/// The name-only [`resolve_tool_selection`] uses this to give core names the
/// registration facts it knows; non-core names it cannot describe stay
/// name-only.
fn core_registrations() -> std::collections::BTreeMap<String, ToolRegistration> {
    let mut registry = ToolRegistry::new();
    crate::coding_agent::tools::register_core_tools(&mut registry);
    registry
        .iter()
        .map(|tool| {
            (
                tool.name().to_owned(),
                ToolRegistration {
                    exposure: tool.exposure(),
                    default_active: tool.default_active(),
                },
            )
        })
        .collect()
}

/// Match an allowlist of names / `*` patterns against the available tools.
///
/// Pattern semantics are upstream `createToolNameMatcher` /
/// `toolPatternRegExp` (mcp-servers.ts:12-33): only `*` is a wildcard, every
/// other character — `?` included — is literal, and matching is
/// case-sensitive.
fn match_allowlist(entries: &[String], available: &[AvailableTool]) -> Vec<String> {
    let mut matched: Vec<String> = Vec::new();
    for entry in entries {
        if entry.contains('*') {
            for tool in available {
                if tool_pattern_matches(entry, &tool.name) && !matched.contains(&tool.name) {
                    matched.push(tool.name.clone());
                }
            }
        } else if available.iter().any(|tool| tool.name == *entry) && !matched.contains(entry) {
            matched.push(entry.clone());
        }
    }
    matched
}

/// Upstream `toolPatternRegExp` semantics (mcp-servers.ts:12-24): `*` matches
/// any run of characters (including none); every other character is literal;
/// matching is case-sensitive. `?` is NOT a wildcard.
/// Case-sensitive tool-name pattern match for a selection entry (upstream
/// `createToolNameMatcher`): only `*` is a wildcard, `?` is literal. The
/// pattern must match the whole name.
pub(crate) fn tool_pattern_matches(pattern: &str, text: &str) -> bool {
    fn matches(pattern: &[u8], text: &[u8]) -> bool {
        match (pattern.first(), text.first()) {
            (None, None) => true,
            (Some(b'*'), _) => (0..=text.len()).any(|skip| matches(&pattern[1..], &text[skip..])),
            (Some(expected), Some(actual)) if expected == actual => {
                matches(&pattern[1..], &text[1..])
            }
            _ => false,
        }
    }
    matches(pattern.as_bytes(), text.as_bytes())
}

/// Whether `name` is excluded by one of `exclusions`, each an exact tool name
/// or a `*` pattern (upstream `_isAllowedTool`'s `_excludedTools(name)`,
/// `createToolNameMatcher`).
///
/// A default-tools selection carries its `--exclude-tools` entries as
/// `-<entry>` modifiers (`-name` or `-pattern`); the `mcp` builtin and the
/// session's direct-bridge union consult this helper so a bridge that
/// registers AFTER the selection was resolved is excluded exactly like one
/// that was already registered. Only `*` is a wildcard; `?` is literal and
/// matching is case-sensitive ([`tool_pattern_matches`]).
pub(crate) fn tool_excluded_by(exclusions: &[String], name: &str) -> bool {
    exclusions.iter().any(|entry| {
        if entry.contains('*') {
            tool_pattern_matches(entry, name)
        } else {
            entry == name
        }
    })
}

/// Case-insensitive glob with `*` (any run) and `?` (one character).
///
/// This is the broader helper used by the model-scope matchers; the
/// TOOL-selection patterns (the shared resolver's `match_allowlist` and the
/// SDK builder's own `tool_name_pattern_matches`) use the upstream-faithful
/// [`tool_pattern_matches`] above, where `?` is literal and matching is
/// case-sensitive.
pub fn glob_matches(pattern: &str, text: &str) -> bool {
    glob_match_inner(
        pattern.to_lowercase().as_bytes(),
        text.to_lowercase().as_bytes(),
    )
}

fn glob_match_inner(pattern: &[u8], text: &[u8]) -> bool {
    match (pattern.first(), text.first()) {
        (None, None) => true,
        (Some(b'*'), _) => {
            let rest = &pattern[1..];
            (0..=text.len()).any(|skip| glob_match_inner(rest, &text[skip..]))
        }
        (Some(b'?'), Some(_)) => glob_match_inner(&pattern[1..], &text[1..]),
        (Some(ch), Some(other)) if ch.eq_ignore_ascii_case(other) => {
            glob_match_inner(&pattern[1..], &text[1..])
        }
        _ => false,
    }
}
