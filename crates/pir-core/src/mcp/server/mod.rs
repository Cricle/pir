//! The built-in MCP **servers** hosted by the `pir` executable itself.
//!
//! Single-binary policy ([UPDATING.md §6]: the repo keeps one crate and one
//! binary). pir therefore never ships a second executable to be an MCP
//! server: the servers it *is* (rather than merely speaks to) live behind
//! hidden internal subcommands, exactly like `pir env-daemon` and
//! `pir __codemode-worker`:
//!
//! - `pir mcp-sd [--root <dir>]` — [`sd`], a sed-style search & replace tool
//!   ([chmln/sd]-shaped) served to any MCP client over stdio, including pir's
//!   own [`crate::mcp::client`].
//!
//! The subcommand is intercepted on the **raw argv** in
//! `main_cli/run_mode.rs`, before clap (so it stays out of `--help`) and
//! before settings/trust/migration/tracing setup — the latter because a stdio
//! MCP server owns its stdout: the only bytes this process may write there are
//! the JSON-RPC frames [`rmcp`] produces. Diagnostics go to stderr; the
//! process exits cleanly (0) when stdin reaches EOF.
//!
//! The whole module is gated by the `mcp-sd` cargo feature (`Cargo.toml`;
//! included in `full`), so a build without it simply has no `mcp-sd`
//! subcommand — no honest-degradation path is needed, like the other gated
//! surfaces.
//!
//! [UPDATING.md §6]: ../UPDATING.md
//! [chmln/sd]: https://github.com/chmln/sd

pub mod sd;

use std::path::PathBuf;

/// The hidden subcommand that hosts the built-in `sd` MCP server.
pub const SUBCOMMAND: &str = "mcp-sd";

/// Usage line for a bad `pir mcp-sd` invocation.
const USAGE: &str = "usage: pir mcp-sd [--root <dir>]";

/// `pir mcp-sd [--root <dir>]` — the process entry for the built-in `sd` MCP
/// server. Returns the process exit code: 0 on a clean end (stdin EOF,
/// transport closed), 1 on an I/O/initialize failure, 2 on a usage error.
///
/// `run_mode.rs` turns this into `std::process::exit`, like the `js` and
/// `env-daemon` intercepts.
pub async fn run_main(args: &[String]) -> i32 {
    let root = match parse_root(args) {
        Ok(root) => root,
        Err(message) => {
            eprintln!("pir {SUBCOMMAND}: {message}\n{USAGE}");
            return 2;
        }
    };
    match sd::serve_stdio(root).await {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("pir {SUBCOMMAND}: {message}");
            1
        }
    }
}

/// Parse `[--root <dir> | --root=<dir>]` (default: the current directory) and
/// canonicalize it, so every request path can be checked against one resolved
/// root and `..`/symlink tricks in the argument are settled once.
fn parse_root(args: &[String]) -> Result<PathBuf, String> {
    let mut requested: Option<&str> = None;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--root" {
            let value = args
                .get(index + 1)
                .ok_or_else(|| "--root needs a directory".to_owned())?;
            requested = Some(value);
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--root=") {
            requested = Some(value);
            index += 1;
            continue;
        }
        return Err(format!("unknown argument {arg:?}"));
    }
    let requested = match requested {
        Some(value) => PathBuf::from(value),
        None => std::env::current_dir().map_err(|error| format!("cannot read cwd: {error}"))?,
    };
    let canonical = std::fs::canonicalize(&requested)
        .map_err(|error| format!("--root {}: {error}", requested.display()))?;
    if !canonical.is_dir() {
        return Err(format!("--root {} is not a directory", requested.display()));
    }
    Ok(canonical)
}
