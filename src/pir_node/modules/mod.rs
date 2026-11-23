//! Node module implementations. Each module exposes pure `op` functions over
//! `serde_json::Value`; host-dependent parts go through `NodeContext`/`NodeHooks`.

pub mod bcrypt;
pub mod buffer;
pub mod child;
pub mod crypto;
pub mod dns;
pub mod fs;
pub mod fs_watch;
pub mod http;
pub mod npm;
pub mod path;
pub mod sqlite;
pub mod sys;
pub mod url;
pub mod worker;

use serde_json::{json, Value as JsonValue};

use crate::pir_node::error::{NodeError, NodeResult};

/// Dispatch for the `path` module. The four char-scanning kernels (`resolve`,
/// `join`, `normalize`, `relative`) run here for both namespaces; the JS shim
/// keeps Node's argument validation and the remaining (cheap) surface.
///
/// Payload: `{flavor: "win32"|"posix", args: [..], path, from, to}`. `flavor`
/// defaults to the host platform (Node's `path` module behavior).
pub fn path_op(
    op: &str,
    payload: &JsonValue,
    runtime: &crate::pir_node::NodeRuntime,
) -> NodeResult {
    use path::Flavor;

    let flavor = payload
        .get("flavor")
        .and_then(JsonValue::as_str)
        .and_then(Flavor::from_name)
        .unwrap_or(if cfg!(windows) {
            Flavor::Win32
        } else {
            Flavor::Posix
        });
    // `path::{join,resolve}` are generic over `AsRef<str>`, so the arguments
    // stay borrowed from the parsed payload instead of one `String` each.
    let parts: Vec<&str> = payload
        .get("args")
        .and_then(JsonValue::as_array)
        .map(|array| array.iter().filter_map(JsonValue::as_str).collect())
        .unwrap_or_default();
    let text = |key: &str| payload.get(key).and_then(JsonValue::as_str).unwrap_or("");

    let value = match op {
        "resolve" => {
            let cwd = runtime.context.cwd();
            json!(path::resolve(flavor, &parts, &cwd.to_string_lossy()))
        }
        "join" => json!(path::join(flavor, &parts)),
        "normalize" => json!(path::normalize(flavor, text("path"))),
        "relative" => {
            let cwd = runtime.context.cwd();
            json!(path::relative(
                flavor,
                text("from"),
                text("to"),
                &cwd.to_string_lossy()
            ))
        }
        other => return Err(NodeError::new("ENOSYS", format!("unknown path op `{other}`"))),
    };
    Ok(value)
}
