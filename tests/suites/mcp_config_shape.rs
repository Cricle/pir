//! mcp.json wrapper-shape interop (upstream extensions/mcp/config.ts:101-107,
//! 228-242): upstream pi writes AND requires the
//! `{ "mcpServers": { name: config } }` wrapper. pir's writers emit that
//! shape and its loader reads it (preferred), while pir's legacy flat map
//! `{ name: config }` keeps loading for backward compatibility. Cross-tool
//! configs must survive in both directions.

use std::path::{Path, PathBuf};

use pir::coding_agent::cli::{mcp_config_remove, mcp_config_upsert};
use pir::config::mcp::{load_mcp_configs, parse_mcp_config, McpServerConfig};

/// A temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-mcp-shape-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// pir's writer emits the upstream wrapper shape and its own loader reads it
/// back (write with pir -> read with the loader round trip).
#[test]
fn writer_emits_upstream_wrapper_shape_and_loader_reads_it() {
    let dir = TempDir::new("wrapper-roundtrip");
    let path = dir.path().join("mcp.json");

    let replaced = mcp_config_upsert(
        &path,
        "files",
        &serde_json::json!({
            "command": "npx",
            "args": ["-y", "@example/files-mcp"],
        }),
    )
    .expect("first add");
    assert!(!replaced);

    // On disk: the upstream wrapper, not the flat map.
    let written = std::fs::read_to_string(&path).expect("read mcp.json");
    let root: serde_json::Value = serde_json::from_str(&written).expect("valid JSON");
    assert!(
        root.get("mcpServers")
            .is_some_and(|servers| servers.is_object()),
        "the writer must emit the upstream mcpServers wrapper: {root}"
    );
    assert!(
        root.get("files").is_none(),
        "no flat top-level entries may remain: {root}"
    );

    // pir's loader reads it back.
    let (servers, errors) = parse_mcp_config(&written);
    assert!(errors.is_empty(), "no loader errors: {errors:?}");
    assert!(matches!(
        servers.get("files"),
        Some(McpServerConfig::Stdio(_))
    ));

    // remove keeps the wrapper shape.
    assert!(mcp_config_remove(&path, "files").expect("remove present"));
    let written = std::fs::read_to_string(&path).expect("read back");
    let root: serde_json::Value = serde_json::from_str(&written).expect("valid JSON");
    assert!(root.get("mcpServers").is_some());
    let (servers, errors) = parse_mcp_config(&written);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(servers.is_empty(), "the entry is gone");
}

/// A hand-written upstream-shaped file (the shape pi itself writes, including
/// the optional `autoEnableCodemode` flag) loads through pir's loader and
/// `load_mcp_configs`.
#[test]
fn upstream_shaped_file_loads() {
    let dir = TempDir::new("upstream-file");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(
        pir::config::mcp::global_mcp_path(&agent_dir),
        r#"{
            "autoEnableCodemode": true,
            "mcpServers": {
                "filesystem": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."] },
                "docs": { "url": "https://example.com/mcp", "headers": { "Authorization": "Bearer ${DOCS_TOKEN}" } }
            }
        }"#,
    )
    .expect("write upstream-shaped mcp.json");

    let (servers, errors) = parse_mcp_config(
        &std::fs::read_to_string(pir::config::mcp::global_mcp_path(&agent_dir)).expect("read"),
    );
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(servers.len(), 2);
    assert!(matches!(
        servers.get("filesystem"),
        Some(McpServerConfig::Stdio(_))
    ));
    assert!(matches!(
        servers.get("docs"),
        Some(McpServerConfig::Http(_))
    ));

    let loaded = load_mcp_configs(&agent_dir, dir.path(), false);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].servers.len(), 2);
    assert!(loaded[0].errors.is_empty(), "{:?}", loaded[0].errors);
}

/// A non-boolean `autoEnableCodemode` in the wrapper shape is reported
/// (upstream config.ts:105-106), and a non-object `mcpServers` is the
/// upstream loader error.
#[test]
fn wrapper_shape_type_errors_are_reported() {
    let (servers, errors) = parse_mcp_config(
        r#"{ "mcpServers": { "fs": { "command": "x" } }, "autoEnableCodemode": "yes" }"#,
    );
    assert_eq!(servers.len(), 1, "the server itself still loads");
    assert!(
        errors
            .iter()
            .any(|error| error.contains("autoEnableCodemode")),
        "{errors:?}"
    );

    let (servers, errors) = parse_mcp_config(r#"{ "mcpServers": "nope" }"#);
    assert!(servers.is_empty());
    assert!(
        errors
            .iter()
            .any(|error| error.contains("expected an object with an \"mcpServers\" object")),
        "{errors:?}"
    );
}

/// pir's legacy flat map keeps loading (backward compatibility with files
/// pir wrote before the wrapper shape).
#[test]
fn legacy_flat_file_still_loads() {
    let dir = TempDir::new("legacy-flat");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(
        pir::config::mcp::global_mcp_path(&agent_dir),
        r#"{ "legacy": { "command": "node", "args": ["server.js"] } }"#,
    )
    .expect("write legacy flat mcp.json");

    let loaded = load_mcp_configs(&agent_dir, dir.path(), false);
    assert_eq!(loaded.len(), 1);
    assert!(loaded[0].errors.is_empty(), "{:?}", loaded[0].errors);
    assert!(
        loaded[0].servers.contains_key("legacy"),
        "the flat file still loads: {:?}",
        loaded[0].servers
    );
}

/// Editing a legacy flat file migrates it to the wrapper shape, keeping the
/// pre-existing entries (pir's old files upgrade losslessly).
#[test]
fn editing_a_legacy_flat_file_migrates_it_to_the_wrapper() {
    let dir = TempDir::new("legacy-migrate");
    let path = dir.path().join("mcp.json");
    std::fs::write(
        &path,
        r#"{ "old-server": { "command": "node", "args": ["old.js"] } }"#,
    )
    .expect("write legacy flat mcp.json");

    let replaced = mcp_config_upsert(
        &path,
        "new-server",
        &serde_json::json!({ "url": "https://mcp.example.com/mcp" }),
    )
    .expect("upsert");
    assert!(!replaced);

    let written = std::fs::read_to_string(&path).expect("read back");
    let root: serde_json::Value = serde_json::from_str(&written).expect("valid JSON");
    let servers = root
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
        .expect("migrated to the wrapper shape");
    assert!(
        servers.contains_key("old-server"),
        "old entries kept: {root}"
    );
    assert!(servers.contains_key("new-server"));

    // Both entries load after the migration.
    let (servers, errors) = parse_mcp_config(&written);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(servers.len(), 2);

    // And remove works on the migrated file.
    assert!(mcp_config_remove(&path, "old-server").expect("remove migrated entry"));
    let (servers, errors) = parse_mcp_config(&std::fs::read_to_string(&path).expect("read back"));
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(servers.len(), 1);
    assert!(servers.contains_key("new-server"));
}

/// A non-object root (and a non-object `mcpServers`) errors on the writers
/// with the upstream text instead of corrupting the file.
#[test]
fn writers_reject_non_wrapper_roots() {
    let dir = TempDir::new("writer-reject");
    let path = dir.path().join("mcp.json");
    std::fs::write(&path, r#"[1, 2, 3]"#).expect("write array root");
    let error = mcp_config_upsert(&path, "x", &serde_json::json!({ "command": "true" }))
        .expect_err("an array root is not an object");
    assert!(
        error
            .to_string()
            .contains("expected an object with an \"mcpServers\" object"),
        "{error}"
    );

    std::fs::write(&path, r#"{ "mcpServers": "nope" }"#).expect("write bad wrapper");
    let error = mcp_config_upsert(&path, "x", &serde_json::json!({ "command": "true" }))
        .expect_err("a non-object mcpServers must error");
    assert!(
        error
            .to_string()
            .contains("expected an object with an \"mcpServers\" object"),
        "{error}"
    );
    let error = mcp_config_remove(&path, "x").expect_err("remove hits the same guard");
    assert!(
        error
            .to_string()
            .contains("expected an object with an \"mcpServers\" object"),
        "{error}"
    );
}
