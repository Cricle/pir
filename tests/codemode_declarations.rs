//! DRIFT D14 + D25: codemode tool declarations show schema constraints, and
//! MCP tool declarations spell out the `CallToolResult` scripts receive.
//!
//! Upstream evidence: pi `8a9c051f8` (`packages/codemode/src/declarations.ts`
//! constraint `@` tags + the untagged re-render before `unknown`),
//! `24382bedd` (MCP decoupling: `MCP_TYPESCRIPT_PREAMBLE` /
//! `mcpStructuredContentSchema()` removed; the MCP result schema describes
//! itself with the server's local `$ref`s relocated) and `c661bc574` (the
//! durable copy of the same decoupling), plus `67996ab75` (the D6 leftover:
//! without an output schema `structuredContent` is the empty schema, which
//! renders `unknown`).
//!
//! The JS-side renderer is the shim
//! (`src/extensions/api/js/pi_codemode_shim.js.txt`, owned by the extension
//! runtime); the Rust side owns the MCP result schema
//! ([`pir::plugins::codemode::mcp_result_schema`] +
//! [`pir::plugins::codemode::relocate_schema`]) that the MCP bridge declares.
//! These tests render the Rust-built schema through the REAL shim, so a drift
//! on either side shows up as a byte mismatch.
//!
//! Coverage split: the raw renderer vectors of the upstream
//! `declarations.test.ts` (the flat `schemaToType` calls for the `@`-tag
//! battery, the nullable-`anyOf` branch, draft-04 `exclusive*`, the >80-char
//! tag drop, U+2028/29 escaping, and the `maxChars` budget) are locked
//! verbatim in `tests/pi_shims.rs`; this file locks the same constraint
//! vectors on the D25 path — through the Rust-built MCP result schema, which
//! is where an MCP tool's declaration actually comes from.

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// Fixtures must be written at runtime (repo hygiene forbids tracked `.ts`).
fn write_temp_fixture(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-codemode-decl-{}-{test_name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    dir
}

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: "You are a test.".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// One extension that exposes the shim's declaration renderer to the host:
/// `renderToolSignature` for the tool-declaration string and `schemaToType`
/// for the output-type half (upstream `renderToolOutputType`).
const CODEMODE_DECL_TS: &str = r##"
import { renderToolSignature, schemaToType, renderToolOutputType } from "@earendil-works/pi-codemode";

export default (pi: any) => {
  pi.on("session_start", async () => {
    (globalThis as any).__renderToolSignature = renderToolSignature;
    (globalThis as any).__schemaToType = schemaToType;
    (globalThis as any).__renderToolOutputType = renderToolOutputType;
  });
};
"##;

async fn new_runtime_at(dir: &Path) -> ExtensionRuntime {
    let rt = ExtensionRuntime::new(test_config_at(dir))
        .await
        .expect("runtime construction");
    rt.allow_root(dir.to_path_buf());
    rt
}

async fn shim_runtime(test_name: &str) -> ExtensionRuntime {
    let dir = write_temp_fixture(test_name);
    let rt = new_runtime_at(&dir).await;
    rt.seed_module("pir-ext:codemode-decl.ts", CODEMODE_DECL_TS)
        .expect("seed codemode decl fixture");
    let loaded = rt.seeded_load("pir-ext:codemode-decl.ts").await;
    assert_eq!(loaded.expect("seeded load"), 1);
    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors in the codemode decl fixture: {:?}",
        outcome.errors
    );
    rt
}

/// Render an output schema through the shim exactly as the declaration
/// renderer does (`Promise<...>` half of `renderToolSignature`).
async fn render_output_type(rt: &ExtensionRuntime, schema: &Value) -> String {
    let expression = format!(
        "JSON.stringify(globalThis.__renderToolOutputType({}))",
        serde_json::to_string(schema).expect("serialize schema")
    );
    rt.eval_json(&expression)
        .await
        .expect("render output type")
        .as_str()
        .expect("output type is a string")
        .to_owned()
}

/// Render one full tool declaration through the shim
/// (`renderToolSignature`: identifier + input type + output type).
async fn render_tool_signature(rt: &ExtensionRuntime, tool: &Value) -> String {
    let expression = format!(
        "JSON.stringify(globalThis.__renderToolSignature({}))",
        serde_json::to_string(tool).expect("serialize tool")
    );
    rt.eval_json(&expression)
        .await
        .expect("render tool signature")
        .as_str()
        .expect("signature is a string")
        .to_owned()
}

// ---------------------------------------------------------------------------
// D25 — the MCP result schema (Rust) renders as the upstream declaration
// ---------------------------------------------------------------------------

/// Upstream `test/mcp-extension.test.ts` (`67996ab75`): the declaration of
/// every MCP tool spells out the `content` blocks and `isError` the scripts
/// read, followed by the (relocated) `structuredContent`.
const MCP_RESULT_CONTENT: &str = "content: Array<{ data?: string; mimeType?: string; text?: string; type: \"text\" | \"image\" | \"audio\" | \"resource_link\" | \"resource\"; [key: string]: unknown; }>; isError?: boolean;";

/// The exact declaration the shim renders for a result schema whose
/// `structuredContent` type is `structured_content`:
/// `{ content: ...; isError?: boolean; structuredContent?: <structured_content>; [key: string]: unknown; }`.
fn expected_result_declaration(structured_content: &str) -> String {
    format!("{{ {MCP_RESULT_CONTENT} structuredContent?: {structured_content}; [key: string]: unknown; }}")
}

/// Upstream `test/mcp-extension.test.ts` (`24382bedd`): the declaration names
/// `content`/`structuredContent`/`isError` explicitly instead of referring to
/// a `CallToolResult<T>` the reader has never seen.
#[tokio::test(flavor = "current_thread")]
async fn d25_mcp_tool_declares_content_structured_content_and_is_error() {
    let rt = shim_runtime("mcp-result-schema").await;
    let schema = pir::plugins::codemode::mcp_result_schema(None);
    let rendered = render_output_type(&rt, &schema).await;
    // Upstream `67996ab75` expectation, byte for byte. The no-schema default is
    // the empty schema, which the renderer turns into `unknown` (the DRIFT D6
    // leftover) — NOT an object type.
    assert_eq!(
        rendered,
        expected_result_declaration("unknown"),
        "without a server output schema structuredContent renders `unknown`"
    );
    assert!(
        !rendered.contains("CallToolResult"),
        "no MCP type names survive in the declaration: {rendered}"
    );
}

/// The exact upstream expectation for a server-declared output schema whose
/// local references (`#`, `#/properties/hits`, `#/$defs/Hit~1v1`,
/// `#/definitions/Hit`) must resolve as before after relocation, with data that
/// looks like a reference left alone.
#[tokio::test(flavor = "current_thread")]
async fn d25_relocated_server_references_resolve_in_the_declaration() {
    let rt = shim_runtime("mcp-relocated-refs").await;
    let hit = json!({ "type": "object", "properties": { "id": { "type": "string" } }, "required": ["id"] });
    let server_schema = json!({
        "type": "object",
        "properties": {
            "hits": { "type": "array", "items": { "$ref": "#/$defs/Hit~1v1" } },
            "again": { "$ref": "#/properties/hits" },
            "children": { "type": "array", "items": { "$ref": "#" } },
            "default": { "$ref": "#/definitions/Hit" },
            "remote": { "$ref": "https://example.com/schema.json" },
        },
        "required": ["hits"],
        "$defs": { "Hit/v1": {
            "type": "object",
            "properties": { "id": { "type": "string" } },
            "required": ["id"],
            "default": { "$ref": "#/stays" },
        } },
        "definitions": { "Hit": hit },
    });
    let schema = pir::plugins::codemode::mcp_result_schema(Some(&server_schema));

    // Data inside `default` keeps its `$ref` text (upstream assertion).
    assert_eq!(
        schema["properties"]["structuredContent"]["$defs"]["Hit/v1"]["default"],
        json!({ "$ref": "#/stays" }),
        "a data keyword's `$ref` is not a reference: {schema}"
    );

    let expected = expected_result_declaration(
        "{ again?: Array<{ id: string; }>; children?: Array<{ again?: Array<{ id: string; }>; children?: Array<unknown>; default?: { id: string; }; hits: Array<{ id: string; }>; remote?: unknown; }>; default?: { id: string; }; hits: Array<{ id: string; }>; remote?: unknown; }",
    );
    let rendered = render_output_type(&rt, &schema).await;
    assert_eq!(
        rendered, expected,
        "upstream mcp-extension.test.ts expectation"
    );
}

/// `relocate_schema` unit surface: pointers, the `#` root reference, map
/// keywords, data keywords, arrays, and non-string `$ref` values.
#[test]
fn d25_relocate_schema_rewrites_only_references() {
    let relocate = pir::plugins::codemode::relocate_schema;
    let pointer = "#/properties/structuredContent";

    // `#` and `#/...` become pointer-prefixed; remote and non-string stay.
    assert_eq!(
        relocate(&json!({ "$ref": "#" }), pointer),
        json!({ "$ref": pointer })
    );
    assert_eq!(
        relocate(&json!({ "$ref": "#/$defs/A" }), pointer),
        json!({ "$ref": "#/properties/structuredContent/$defs/A" })
    );
    assert_eq!(
        relocate(&json!({ "$ref": "https://example.com/s.json" }), pointer),
        json!({ "$ref": "https://example.com/s.json" })
    );
    assert_eq!(
        relocate(&json!({ "$ref": 7 }), pointer),
        json!({ "$ref": 7 })
    );
    // A non-string `$ref` still recurses (upstream's else branch): a nested
    // local reference inside the malformed value is relocated too.
    assert_eq!(
        relocate(&json!({ "$ref": { "$ref": "#/$defs/A" } }), pointer),
        json!({ "$ref": { "$ref": "#/properties/structuredContent/$defs/A" } })
    );
    assert_eq!(
        relocate(&json!({ "$ref": [{ "$ref": "#" }] }), pointer),
        json!({ "$ref": [{ "$ref": pointer }] })
    );

    // Data keywords keep their values verbatim, references inside them too.
    assert_eq!(
        relocate(&json!({ "default": { "$ref": "#/stays" } }), pointer),
        json!({ "default": { "$ref": "#/stays" } })
    );
    assert_eq!(
        relocate(&json!({ "enum": [{ "$ref": "#/stays" }] }), pointer),
        json!({ "enum": [{ "$ref": "#/stays" }] })
    );

    // Map keywords rewrite their VALUES, keeping the keys as names.
    assert_eq!(
        relocate(
            &json!({ "properties": { "a": { "$ref": "#/$defs/A" } } }),
            pointer
        ),
        json!({ "properties": { "a": { "$ref": "#/properties/structuredContent/$defs/A" } } })
    );
    assert_eq!(
        relocate(&json!({ "$defs": { "A": { "$ref": "#" } } }), pointer),
        json!({ "$defs": { "A": { "$ref": pointer } } })
    );

    // Arrays and scalars recurse / pass through.
    assert_eq!(
        relocate(&json!([{ "$ref": "#" }, "x", null]), pointer),
        json!([{ "$ref": pointer }, "x", null])
    );
    assert_eq!(relocate(&json!("x"), pointer), json!("x"));
}

/// The result schema is a plain `CallToolResult` shape: required `content`, an
/// open object, and the server schema relocated below `structuredContent`.
#[test]
fn d25_result_schema_shape() {
    let schema = pir::plugins::codemode::mcp_result_schema(None);
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["required"], json!(["content"]));
    assert_eq!(schema["additionalProperties"], true);
    assert_eq!(schema["properties"]["content"]["type"], "array");
    assert_eq!(
        schema["properties"]["isError"],
        json!({ "type": "boolean" })
    );
    // No server schema: the empty schema (renders `unknown`), not `{type:object}`.
    assert_eq!(schema["properties"]["structuredContent"], json!({}));
    // The content block type is the compact upstream one.
    assert_eq!(
        schema["properties"]["content"]["items"]["properties"]["type"]["enum"],
        json!(["text", "image", "audio", "resource_link", "resource"])
    );
    assert_eq!(
        schema["properties"]["content"]["items"]["required"],
        json!(["type"])
    );
    assert_eq!(
        schema["properties"]["content"]["items"]["additionalProperties"],
        true
    );

    // An explicit `null` schema (a JS `outputSchema: null`) behaves like one
    // that is absent: scripts may receive any JSON value.
    assert_eq!(
        pir::plugins::codemode::mcp_result_schema(Some(&Value::Null))["properties"]
            ["structuredContent"],
        json!({})
    );
}

// ---------------------------------------------------------------------------
// D25 — the tool-failure wording ("declares an output schema -> resolves to
// its value" instead of "MCP tools resolve to their CallToolResult")
// ---------------------------------------------------------------------------

/// The `codemode` tool description explains resolution without naming MCP (the
/// upstream `tool.ts` doc comment change): a tool that declares an output
/// schema resolves to its structured output, including on failure.
#[test]
fn d25_codemode_description_has_no_mcp_specific_resolution_rule() {
    let description = {
        use pir::agent::tool_execution::AgentTool;
        pir::plugins::codemode::CodemodeTool::default()
            .description()
            .to_owned()
    };
    assert!(
        !description.contains("MCP"),
        "the codemode tool description must not name MCP: {description}"
    );
    assert!(
        description.contains(
            "resolves to a tool's text output (or its structured output when it declares a schema)"
        ),
        "resolution rule stays the schema-based one: {description}"
    );
}

// ---------------------------------------------------------------------------
// D14 — constraint tags, on the D25 path: the upstream `declarations.test.ts`
// vectors rendered through the Rust-built MCP result schema (the declaration an
// MCP tool actually shows the model)
// ---------------------------------------------------------------------------

/// The upstream `declarations.test.ts` `constraints the type cannot express`
/// vector, plus its nullable-`anyOf`, both-branches and draft-04
/// `exclusive*` vectors: every tag survives `relocate_schema` and the nesting
/// under `structuredContent` unchanged.
#[tokio::test(flavor = "current_thread")]
async fn d14_constraint_tags_survive_the_mcp_result_schema_path() {
    let rt = shim_runtime("constraint-tags").await;
    // Same schemas as the upstream `schemaToType` vectors, in one object.
    let server_schema = json!({
        "type": "object",
        "properties": {
            "timeoutMs": { "type": "integer", "minimum": 0, "maximum": 30000, "default": 0, "description": "Wait in milliseconds." },
            "id": { "type": "string", "pattern": "^[a-z]+$", "maxLength": 8 },
            "tags": { "type": "array", "items": { "type": "string" }, "maxItems": 5, "uniqueItems": true, "deprecated": true },
            "name": { "type": "string" },
            // Pydantic's `limit: int | None = Field(None, ge=1, le=100)`.
            "limit": { "anyOf": [{ "type": "integer", "minimum": 1, "maximum": 100 }, { "type": "null" }], "default": null, "title": "Limit" },
            // Constraints of the property and the non-null branch both apply.
            "both": { "anyOf": [{ "type": "integer", "minimum": 10, "maximum": 50, "format": "int32" }, { "type": "null" }], "minimum": 0, "maximum": 20, "format": "port" },
            // Draft-04 boolean exclusive bounds.
            "n": { "type": "integer", "minimum": 0, "exclusiveMinimum": true, "maximum": 10, "exclusiveMaximum": false },
            "m": { "anyOf": [{ "type": "number", "maximum": 5, "exclusiveMaximum": true }, { "type": "null" }], "maximum": 8 },
        },
        "required": ["id"],
    });
    let schema = pir::plugins::codemode::mcp_result_schema(Some(&server_schema));
    let rendered = render_output_type(&rt, &schema).await;
    let structured_content = [
        "{",
        "  // @integer @minimum 10 @maximum 20 @format port @format int32",
        "  both?: number | null;",
        "  // @maxLength 8 @pattern ^[a-z]+$",
        "  id: string;",
        "  // @integer @minimum 1 @maximum 100",
        "  limit?: number | null;",
        "  // @maximum 8 @exclusiveMaximum 5",
        "  m?: number | null;",
        "  // @integer @exclusiveMinimum 0 @maximum 10",
        "  n?: number;",
        "  name?: string;",
        "  // @maxItems 5 @uniqueItems @deprecated",
        "  tags?: Array<string>;",
        "  // Wait in milliseconds.",
        "  // @integer @minimum 0 @maximum 30000 @default 0",
        "  timeoutMs?: number;",
        "}",
    ]
    .join("\n");
    assert_eq!(
        rendered,
        expected_result_declaration(&structured_content),
        "upstream declarations.test.ts @-tag vectors"
    );
}

/// The same result schema as a tool's `outputSchema`: the D14 input-side tag
/// (`@minLength`) and the D25 output side ride one declaration, named with the
/// identifier scripts call.
#[tokio::test(flavor = "current_thread")]
async fn d14_input_schema_tags_ride_the_mcp_tool_signature() {
    let rt = shim_runtime("constraint-signature").await;
    let server_schema = json!({
        "type": "object",
        "properties": {
            "timeoutMs": { "type": "integer", "minimum": 0, "maximum": 30000, "default": 0, "description": "Wait in milliseconds." },
            "id": { "type": "string", "pattern": "^[a-z]+$", "maxLength": 8 },
            "tags": { "type": "array", "items": { "type": "string" }, "maxItems": 5, "uniqueItems": true, "deprecated": true },
            "name": { "type": "string" },
        },
        "required": ["id"],
    });
    let schema = pir::plugins::codemode::mcp_result_schema(Some(&server_schema));
    let signature = render_tool_signature(
        &rt,
        &json!({
            "name": "mcp__sample__search",
            "inputSchema": { "type": "object", "properties": { "q": { "type": "string", "minLength": 1 } } },
            "outputSchema": schema,
        }),
    )
    .await;
    let structured_content = [
        "{",
        "  // @maxLength 8 @pattern ^[a-z]+$",
        "  id: string;",
        "  name?: string;",
        "  // @maxItems 5 @uniqueItems @deprecated",
        "  tags?: Array<string>;",
        "  // Wait in milliseconds.",
        "  // @integer @minimum 0 @maximum 30000 @default 0",
        "  timeoutMs?: number;",
        "}",
    ]
    .join("\n");
    assert_eq!(
        signature,
        format!(
            "mcp__sample__search(args: {{\n  // @minLength 1\n  q?: string;\n}}): Promise<{}>;",
            expected_result_declaration(&structured_content)
        ),
        "the MCP tool name is the identifier scripts call, input tags included"
    );
}

/// The upstream `declarations.test.ts` `leaves out long tags and escapes line
/// terminators` vector, again through the Rust-built result schema.
#[tokio::test(flavor = "current_thread")]
async fn d14_long_tags_and_line_terminators_survive_the_mcp_result_schema_path() {
    let rt = shim_runtime("long-tags").await;
    let server_schema = json!({
        "type": "object",
        "properties": {
            // The `@default` tag is over the 80-char cap: left out entirely, so
            // the property renders without a comment.
            "config": { "type": "object", "default": { "text": "x".repeat(100) } },
            // The description's U+2028 splits the comment; the pattern's CR/LF
            // and U+2028/29 and the `default`'s U+2028 escape inside the tag.
            "sep": { "type": "string", "pattern": "^[\r\n\u{2028}\u{2029}]$", "default": "\u{2028}", "description": "a\u{2028}b" },
        },
    });
    let schema = pir::plugins::codemode::mcp_result_schema(Some(&server_schema));
    let rendered = render_output_type(&rt, &schema).await;
    let structured_content = [
        "{",
        "  config?: { [key: string]: unknown; };",
        "  // a",
        "  // b",
        "  // @pattern ^[\\u000d\\u000a\\u2028\\u2029]$ @default \"\\u2028\"",
        "  sep?: string;",
        "}",
    ]
    .join("\n");
    assert_eq!(
        rendered,
        expected_result_declaration(&structured_content),
        "upstream declarations.test.ts long-tag and line-terminator vector"
    );
}
