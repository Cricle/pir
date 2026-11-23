//! `pir-node` — the Node/npm runtime surface used by pir's embedded JS host.
//!
//! This crate owns every Node builtin implementation (fs/os/crypto/process/
//! path/...) in one structured place. It is deliberately independent of both
//! pir and rquickjs: ops speak `serde_json::Value` and return a single
//! [`NodeError`] type. The pir side converts live QuickJS values to/from those
//! JSON trees in its C-ABI glue (`native_bridge.rs`), so plugin authors never
//! see a JSON envelope and never change plugin source.

pub mod bridge;
pub mod error;
pub mod fd;
pub mod modules;
pub mod runtime;

pub use error::{NodeError, NodeResult};
pub use runtime::{
    ExecOutcome, NodeContext, NodeHooks, NodeRuntime, NpmLookup, NpmSource, NullContext, NullHooks,
    SessionConfig,
};

use serde_json::{json, Value as JsonValue};

impl NodeRuntime {
    /// Universal entry — mirrors `node_fns.rs::node_call`'s module dispatch.
    /// The JS funnel `__pirNodeCall(module, op, payload)` calls exactly this.
    pub fn invoke(&self, module: &str, op: &str, payload: &JsonValue) -> NodeResult {
        let started = std::time::Instant::now();
        let result = match module {
            "os" => modules::sys::os_op(op, payload, self),
            "process" => modules::sys::process_op(op, payload, self),
            "path" => modules::path_op(op, payload, self),
            "crypto" => modules::crypto::crypto_op(op, payload),
            "zlib" => {
                // child.rs owns the wrapped gzip/deflate family; crypto.rs owns
                // the raw deflateRaw/inflateRaw pair (routed here by the JS
                // helper's `__pirNodePrimaryOps` whitelist).
                match modules::crypto::zlib_raw_op(op, payload) {
                    Err(e) if e.code == "ENOSYS" => modules::child::zlib_op(op, payload),
                    other => other,
                }
            }
            "sqlite" => modules::sqlite::sqlite_op(self, op, payload),
            "bcrypt" => modules::bcrypt::bcrypt_op(op, payload),
            "child" => modules::child::child_op(op, payload, self),
            "fs_watch" | "fsWatch" => modules::fs_watch::fs_watch_op(op, payload, self),
            "fs" => modules::fs::fs_op(self, self.context.generation(), op, payload),
            "dns" => modules::dns::dns_op(op, payload),
            "npm" => modules::npm::npm_op(self, op, payload),
            "http" => modules::http::http_op(self, op, payload),
            "worker" => modules::worker::worker_op(self, op, payload),
            other => Err(NodeError::new(
                "ENOSYS",
                format!("unknown node host module `{other}`"),
            )),
        };
        self.context
            .record_host_op(label(module, op), started.elapsed());
        result
    }
}

/// Static timing label (mirrors `node_op_label`), never the payload/path.
/// `NodeRuntime::invoke` is the single recording point for every module; the
/// per-module op tables must not record a second sample.
fn label(module: &str, op: &str) -> &'static str {
    match (module, op) {
        ("fs", "readFile") => "fs.readFile",
        ("fs", "stat") => "fs.stat",
        ("fs", "lstat") => "fs.lstat",
        ("fs", "readdir") => "fs.readdir",
        ("fs", "realpath") => "fs.realpath",
        ("fs", "exists") => "fs.exists",
        ("fs", "readlink") => "fs.readlink",
        ("fs", "access") => "fs.access",
        ("fs", "writeFile") => "fs.writeFile",
        ("npm", "resolve") => "npm.resolve",
        ("npm", "readSource") => "npm.readSource",
        ("child", _) => "child_process",
        ("http", op) if op.starts_with("fetch") => "fetch",
        ("http", op) if op.starts_with("server") => "http.server",
        ("http", op) if op.starts_with("net") => "net",
        ("fs_watch" | "fsWatch", _) => "fs.watch",
        ("crypto", _) => "crypto",
        ("os", _) => "os",
        ("process", _) => "process",
        ("dns", _) => "dns",
        ("sqlite", _) => "sqlite",
        ("bcrypt", _) => "bcrypt",
        ("zlib", _) => "zlib",
        ("worker", _) => "worker",
        _ => "node.other",
    }
}

/// Success envelope helper used by the C-ABI glue.
pub fn ok_envelope(value: JsonValue) -> JsonValue {
    NodeError::ok(value)
}

/// The empty payload, used when a JS caller omits its argument.
pub fn empty_payload() -> JsonValue {
    json!({})
}
