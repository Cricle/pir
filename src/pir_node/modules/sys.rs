//! Pure Node `os` / `process` ops — ported from `node_fns.rs`'s `os_op` and
//! `process_op`. These are the only ops the corresponding JS shims call.

use serde_json::{json, Map, Value as JsonValue};
use std::path::{Path, PathBuf};

use crate::pir_node::error::{NodeError, NodeResult};
use crate::pir_node::runtime::NodeRuntime;

fn env_snapshot() -> JsonValue {
    let mut map = Map::new();
    for (key, value) in std::env::vars_os() {
        map.insert(
            key.to_string_lossy().into_owned(),
            JsonValue::String(value.to_string_lossy().into_owned()),
        );
    }
    JsonValue::Object(map)
}

fn homedir() -> String {
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE")
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| {
                let drive = std::env::var("HOMEDRIVE").ok()?;
                let path = std::env::var("HOMEPATH").ok()?;
                Some(format!("{drive}{path}"))
            })
            .unwrap_or_default()
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOME").unwrap_or_default()
    }
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "localhost".to_owned())
}

pub fn os_op(op: &str, _payload: &JsonValue, _runtime: &NodeRuntime) -> NodeResult {
    let value = match op {
        "env" => env_snapshot(),
        "homedir" => JsonValue::String(homedir()),
        "tmpdir" => JsonValue::String(
            std::env::temp_dir()
                .to_string_lossy()
                .trim_end_matches(['\\', '/'])
                .to_owned(),
        ),
        "hostname" => JsonValue::String(hostname()),
        "cpus_count" => json!(std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)),
        other => return Err(NodeError::new("ENOSYS", format!("unknown os op `{other}`"))),
    };
    Ok(value)
}

pub fn process_op(op: &str, payload: &JsonValue, runtime: &NodeRuntime) -> NodeResult {
    let value = match op {
        "env" => env_snapshot(),
        "cwd" => JsonValue::String(runtime.cwd().to_string_lossy().into_owned()),
        // `process.chdir`: validate against the *current* cwd, canonicalize,
        // and keep the override on the runtime (never the shared session
        // config, and never the host process's own cwd). Relative fs paths in
        // every later op resolve through `NodeRuntime::cwd`.
        "chdir" => {
            let raw = payload
                .get("path")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| {
                    NodeError::new(
                        "ERR_INVALID_ARG_TYPE",
                        "The \"directory\" argument must be of type string",
                    )
                })?;
            if raw.is_empty() {
                return Err(NodeError::new(
                    "ENOENT",
                    "ENOENT: no such file or directory, chdir ''",
                ));
            }
            let current = runtime.cwd();
            let target = if Path::new(raw).is_absolute() {
                PathBuf::from(raw)
            } else {
                current.join(raw)
            };
            let resolved = std::fs::canonicalize(&target).map_err(|error| {
                let code = match error.kind() {
                    std::io::ErrorKind::NotFound => "ENOENT",
                    std::io::ErrorKind::PermissionDenied => "EPERM",
                    _ => "EINVAL",
                };
                NodeError::new(
                    code,
                    format!(
                        "{}: {}, chdir '{}'",
                        code,
                        match code {
                            "ENOENT" => "no such file or directory",
                            "EPERM" => "operation not permitted",
                            _ => "invalid argument",
                        },
                        target.to_string_lossy()
                    ),
                )
            })?;
            if !resolved.is_dir() {
                return Err(NodeError::new(
                    "ENOTDIR",
                    format!(
                        "ENOTDIR: not a directory, chdir '{}'",
                        target.to_string_lossy()
                    ),
                ));
            }
            let text = resolved.to_string_lossy();
            let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
            if let Ok(mut guard) = runtime.cwd_override.lock() {
                *guard = Some(PathBuf::from(text));
            }
            JsonValue::Null
        }
        "pid" => json!(std::process::id()),
        "cpu_usage" => json!({ "user": 0u64, "system": 0u64 }),
        other => {
            return Err(NodeError::new(
                "ENOSYS",
                format!("unknown process op `{other}`"),
            ))
        }
    };
    Ok(value)
}
