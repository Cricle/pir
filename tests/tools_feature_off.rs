//! The per-tool feature boundary.
//!
//! Each core built-in is its own leaf feature (`tool-read`, `tool-bash`,
//! `tool-powershell`, `tool-edit`, `tool-write`, `tool-ls`,
//! `tool-background`) and the aggregate `tools` feature is exactly their
//! union. This standalone target compiles in every feature shape and asserts,
//! for whatever shape it was built with:
//!
//! * `CORE_TOOL_NAMES` lists exactly the enabled leaves (order pinned), so an
//!   off-path removes the name and an on-path adds it;
//! * `register_core_tools` registers exactly that set;
//! * `tool_definitions` has one model-facing definition per registered tool;
//! * with no leaf at all, the registry is empty, the prompt renders `(none)`
//!   and an implicit `DEFAULT_TOOL_NAMES` selection never diagnoses.
//!
//! `tools` on: it enables all seven leaves; the independent `grep-find` leaf
//! adds the `grep`/`find` pair, preserving the historical 10-name set. A
//! `grep-find`-only build now exposes just those two names (the feature no
//! longer implies `tools`).
//!
//! Run it in a leaf shape to pin that leaf's on/off path, e.g.
//! `cargo test --no-default-features --features tool-read --test tools_feature_off`.

#[cfg(feature = "tool-read")]
use pir::agent::tool_execution::ToolContext;
use pir::agent::tool_execution::ToolRegistry;
use pir::coding_agent::tool_selection::compiled_out_tool_feature;
#[cfg(not(any(
    feature = "tool-read",
    feature = "tool-bash",
    feature = "tool-powershell",
    feature = "tool-edit",
    feature = "tool-write",
    feature = "tool-ls",
    feature = "tool-background",
    feature = "grep-find"
)))]
use pir::coding_agent::tool_selection::resolve_tool_selection;
use pir::coding_agent::tools::{register_core_tools, tool_definitions, CORE_TOOL_NAMES};
#[cfg(not(any(
    feature = "tool-read",
    feature = "tool-bash",
    feature = "tool-powershell",
    feature = "tool-edit",
    feature = "tool-write",
    feature = "tool-ls",
    feature = "tool-background",
    feature = "grep-find"
)))]
use pir::config::settings::DEFAULT_TOOL_NAMES;
#[cfg(not(any(
    feature = "tool-read",
    feature = "tool-bash",
    feature = "tool-powershell",
    feature = "tool-edit",
    feature = "tool-write",
    feature = "tool-ls",
    feature = "tool-background",
    feature = "grep-find"
)))]
use pir::system_prompt::{build_system_prompt_sections, SystemPromptInput, SystemPromptSection};

/// The exact `CORE_TOOL_NAMES` order for this build, derived from `cfg!` so
/// one test pins every leaf's on AND off path in every feature shape.
fn expected_core_tool_names() -> Vec<&'static str> {
    let mut names = Vec::new();
    if cfg!(feature = "tool-read") {
        names.push("read");
    }
    if cfg!(feature = "tool-bash") {
        names.push("bash");
    }
    if cfg!(feature = "tool-powershell") {
        names.push("powershell");
    }
    if cfg!(feature = "tool-edit") {
        names.push("edit");
    }
    if cfg!(feature = "tool-write") {
        names.push("write");
    }
    // The grep/find pair follows the independent `grep-find` leaf; it no
    // longer implies `tools` or any `tool-*` leaf.
    if cfg!(feature = "grep-find") {
        names.push("grep");
        names.push("find");
    }
    if cfg!(feature = "tool-ls") {
        names.push("ls");
    }
    if cfg!(feature = "tool-background") {
        names.push("background_status");
        names.push("background_abort");
    }
    names
}

#[test]
fn core_tool_names_match_the_enabled_leaves() {
    let expected = expected_core_tool_names();
    assert_eq!(
        CORE_TOOL_NAMES,
        expected.as_slice(),
        "CORE_TOOL_NAMES must be exactly the enabled leaves in registration order"
    );

    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    let mut registered: Vec<String> = registry.iter().map(|tool| tool.name().to_owned()).collect();
    registered.sort();
    let mut expected_sorted: Vec<String> = expected.iter().map(|name| (*name).to_owned()).collect();
    expected_sorted.sort();
    assert_eq!(
        registered, expected_sorted,
        "register_core_tools must register exactly the enabled leaves"
    );

    let definitions = tool_definitions(&registry);
    assert_eq!(
        definitions.len(),
        CORE_TOOL_NAMES.len(),
        "every registered core tool has one model-facing definition"
    );
}

#[test]
fn diagnostics_name_the_leaf_feature_for_each_disabled_tool() {
    // `compiled_out_tool_feature` is `Some(leaf)` exactly when the leaf is
    // off; a full build has no compiled-out core surface.
    let cases: [(&str, &str, bool); 8] = [
        ("read", "tool-read", cfg!(feature = "tool-read")),
        ("bash", "tool-bash", cfg!(feature = "tool-bash")),
        (
            "powershell",
            "tool-powershell",
            cfg!(feature = "tool-powershell"),
        ),
        ("edit", "tool-edit", cfg!(feature = "tool-edit")),
        ("write", "tool-write", cfg!(feature = "tool-write")),
        ("ls", "tool-ls", cfg!(feature = "tool-ls")),
        (
            "background_status",
            "tool-background",
            cfg!(feature = "tool-background"),
        ),
        (
            "background_abort",
            "tool-background",
            cfg!(feature = "tool-background"),
        ),
    ];
    for (tool, leaf, enabled) in cases {
        if enabled {
            assert_eq!(
                compiled_out_tool_feature(tool),
                None,
                "`{tool}` is compiled in, so it must never be diagnosed"
            );
        } else {
            assert_eq!(
                compiled_out_tool_feature(tool),
                Some(leaf),
                "`{tool}` must name its leaf feature `{leaf}`"
            );
        }
    }
}

/// P1-J: read's image attachment/description/output-schema path is gated on
/// `image-tools` ALONE. `tui` also compiles the `image` crate (preview /
/// settings), but it must not turn read into an image-attaching tool: the
/// documented `tui`-without-`image-tools` shape reports the image type/size and
/// attaches nothing (docs/FEATURES.md §5).
#[cfg(feature = "tool-read")]
#[tokio::test]
async fn read_image_attachment_is_gated_on_image_tools_not_tui() {
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    let read = registry.get("read").expect("read is enabled by tool-read");

    if cfg!(feature = "image-tools") {
        assert!(
            read.description()
                .contains("Images are sent as attachments"),
            "image-tools build must advertise attachments: {}",
            read.description()
        );
        let schema = read.output_schema().expect("read has an output schema");
        assert!(
            schema.get("anyOf").is_some(),
            "image-tools build exposes the image output shape: {schema}"
        );
    } else {
        assert!(
            !read
                .description()
                .contains("Images are sent as attachments"),
            "a build without image-tools must not advertise attachments: {}",
            read.description()
        );
        assert!(
            read.description().contains("not attached"),
            "the off description names the compiled-out degradation: {}",
            read.description()
        );
        assert_eq!(
            read.output_schema().expect("read has an output schema"),
            serde_json::json!({ "type": "string" }),
            "without image-tools the output can only be text"
        );
    }

    // Behavioral half: a real 1x1 PNG read through the registered tool.
    let dir = std::env::temp_dir().join(format!(
        "pir-read-image-gate-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8,
        0xcf, 0xc0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0xc9, 0xfe, 0x92, 0xef, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    std::fs::write(dir.join("pixel.png"), PNG_1X1).expect("write png");
    let result = read
        .run(
            &serde_json::json!({ "path": "pixel.png" }),
            &ToolContext::new(&dir),
        )
        .await;
    let _ = std::fs::remove_dir_all(&dir);

    assert!(!result.is_error, "read failed: {}", result.content);
    if cfg!(feature = "image-tools") {
        assert_eq!(
            result.images.len(),
            1,
            "image-tools build attaches the image: {}",
            result.content
        );
    } else {
        assert!(
            result.images.is_empty(),
            "without image-tools read must not attach images (tui included): {}",
            result.content
        );
        assert!(
            result.content.contains("Image omitted"),
            "the off path reports the compiled-out degradation: {}",
            result.content
        );
    }
}

/// `tools` is the meta-feature over the seven leaves, not an independent gate.
/// The constant values are the assertion: this target only reaches the test in
/// a shape where `tools` (hence every leaf) is on, and a future manifest edit
/// making `tools` independent of a leaf must still fail here.
#[cfg(feature = "tools")]
#[allow(clippy::assertions_on_constants)]
#[test]
fn tools_meta_feature_enables_every_leaf() {
    // Every leaf is on as a consequence of `tools`.
    assert!(cfg!(feature = "tool-read"));
    assert!(cfg!(feature = "tool-bash"));
    assert!(cfg!(feature = "tool-powershell"));
    assert!(cfg!(feature = "tool-edit"));
    assert!(cfg!(feature = "tool-write"));
    assert!(cfg!(feature = "tool-ls"));
    assert!(cfg!(feature = "tool-background"));

    // The historical set is preserved: ten names with `grep-find`, eight
    // without it (no other core tool can be missing under the meta-feature).
    let expected_len = if cfg!(feature = "grep-find") { 10 } else { 8 };
    assert_eq!(CORE_TOOL_NAMES.len(), expected_len);
    let expected = expected_core_tool_names();
    assert_eq!(CORE_TOOL_NAMES, expected.as_slice());
}

#[cfg(not(any(
    feature = "tool-read",
    feature = "tool-bash",
    feature = "tool-powershell",
    feature = "tool-edit",
    feature = "tool-write",
    feature = "tool-ls",
    feature = "tool-background",
    feature = "grep-find"
)))]
#[test]
fn toolless_build_registers_no_core_tools() {
    assert!(
        CORE_TOOL_NAMES.is_empty(),
        "a toolless build must expose an empty core name set"
    );

    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    assert_eq!(
        registry.iter().count(),
        0,
        "register_core_tools must be a no-op without any leaf"
    );
    assert!(
        tool_definitions(&registry).is_empty(),
        "a toolless build declares no model-facing tool definitions"
    );
    assert!(
        registry.active_names(None).is_empty(),
        "a toolless registry activates nothing"
    );

    // The upstream default names still exist as a constant, but an implicit
    // default is not a selection request: it resolves to `None` and therefore
    // never diagnoses nor declares an absent tool.
    assert_eq!(DEFAULT_TOOL_NAMES, ["read", "bash", "edit", "write"]);
    let available: Vec<String> = registry.iter().map(|tool| tool.name().to_owned()).collect();
    let selection = resolve_tool_selection(
        None,
        &[],
        &[],
        false,
        false,
        &available,
        &DEFAULT_TOOL_NAMES,
    )
    .expect("the default selection is valid");
    assert!(
        selection.is_none(),
        "no flags means no selection change; the defaults never diagnose"
    );
}

#[cfg(not(any(
    feature = "tool-read",
    feature = "tool-bash",
    feature = "tool-powershell",
    feature = "tool-edit",
    feature = "tool-write",
    feature = "tool-ls",
    feature = "tool-background",
    feature = "grep-find"
)))]
#[test]
fn toolless_prompt_renders_an_empty_tools_section() {
    let cwd = std::env::temp_dir();
    let sections = build_system_prompt_sections(&SystemPromptInput {
        preamble_override: None,
        append: Vec::new(),
        tools: &[],
        hidden_tools: &[],
        project_files: &[],
        skills: &[],
        custom_sections: Vec::new(),
        cwd: &cwd,
        package_dir: None,
    });
    let tools = sections
        .get(SystemPromptSection::Tools)
        .expect("the default preamble renders a tools section");
    assert!(
        tools.contains("(none)"),
        "an empty declared set renders the documented `(none)` tools section; got:\n{tools}"
    );
    for absent in ["read", "bash", "edit", "write"] {
        assert!(
            !tools.contains(&format!("- {absent}:")),
            "the toolless prompt must not advertise `{absent}`; got:\n{tools}"
        );
    }
}
