//! Node `dns` module: `dns.lookup` over the host resolver.
//!
//! Ported from `node_fns.rs::dns_op`. Uses the OS resolver through
//! `ToSocketAddrs`; the failure shape matches Node's `getaddrinfo ENOTFOUND`
//! (libuv `UV__EAI_NONAME`, errno `-3008`).

use std::net::ToSocketAddrs;

use serde_json::Value as JsonValue;

use crate::pir_node::bridge::payload_text;
use crate::pir_node::error::{NodeError, NodeResult};

pub fn dns_op(op: &str, payload: &JsonValue) -> NodeResult {
    match op {
        "lookup" => {
            let host = payload_text(payload, "host")?;
            let query = format!("{host}:0");
            let addresses: Vec<JsonValue> = match query.to_socket_addrs() {
                Ok(iter) => iter
                    .map(|addr| {
                        serde_json::json!({
                            "address": addr.ip().to_string(),
                            "family": if addr.is_ipv4() { 4 } else { 6 },
                        })
                    })
                    .collect(),
                Err(_) => Vec::new(),
            };
            if addresses.is_empty() {
                return Err(NodeError {
                    code: "ENOTFOUND".to_owned(),
                    message: format!("getaddrinfo ENOTFOUND {host}"),
                    syscall: Some("getaddrinfo".to_owned()),
                    path: None,
                    // libuv `UV__EAI_NONAME`; the EAI block is identical on
                    // Windows and POSIX (Node reports the same -3008).
                    errno: Some(-3008),
                });
            }
            Ok(serde_json::json!({ "addresses": addresses }))
        }
        other => Err(NodeError::new(
            "ENOSYS",
            format!("unknown dns op `{other}`"),
        )),
    }
}
