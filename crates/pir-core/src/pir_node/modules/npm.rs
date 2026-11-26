//! Node `npm` module: specifier resolution and module-source reads for the
//! CommonJS/ESM runtime.
//!
//! The resolver, the transpile stack and the virtual-shim registry live in the
//! pir host, so this module keeps the op table/response shape and calls the
//! [`crate::pir_node::NodeHooks`] callbacks for the two host-dependent steps. Ported from
//! `node_fns.rs::npm_op`.

use std::path::PathBuf;

use serde_json::{json, Value as JsonValue};

use crate::pir_node::bridge::{payload_bool, payload_text};
use crate::pir_node::error::{NodeError, NodeResult};
use crate::pir_node::runtime::NpmLookup;

pub fn npm_op(runtime: &crate::pir_node::NodeRuntime, op: &str, payload: &JsonValue) -> NodeResult {
    match op {
        "resolve" => {
            let dir = PathBuf::from(payload_text(payload, "dir")?);
            let spec = payload_text(payload, "spec")?;
            let require = payload.get("mode").and_then(JsonValue::as_str) == Some("require");
            Ok(match runtime.hooks.npm_lookup(spec, &dir, require) {
                NpmLookup::Builtin { name } => json!({ "kind": "builtin", "name": name }),
                NpmLookup::File { path, module_kind } => json!({
                    "kind": "file",
                    "path": path.to_string_lossy(),
                    "moduleKind": module_kind,
                }),
                // Not-found shapes are "missing" (MODULE_NOT_FOUND), everything
                // else is a coded "error" the JS layer raises verbatim.
                NpmLookup::NotFound { code, message } => {
                    json!({ "kind": "missing", "code": code, "message": message })
                }
                NpmLookup::Error { code, message } => {
                    json!({ "kind": "error", "code": code, "message": message })
                }
            })
        }
        "readSource" => {
            // Module-loading reads keep the stricter sandbox fence (the same
            // one fs writes use).
            let path = crate::pir_node::modules::fs::check_write(runtime, payload, "path", "open")?;
            let as_cjs = payload_bool(payload, "asCjs");
            let source = runtime.hooks.npm_source(&path, as_cjs)?;
            Ok(json!({ "kind": source.kind, "source": source.source }))
        }
        other => Err(NodeError::new(
            "ENOSYS",
            format!("unknown npm op `{other}`"),
        )),
    }
}
