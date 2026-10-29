//! Virtual module shims for the upstream package imports.

use super::embedded::Shim;

/// Canonicalize a bare specifier to a known virtual module name.
/// Returns `None` when the specifier is not a supported virtual module.
pub static PI_CODING_AGENT_SHIM: Shim = Shim::new(
    "pi_coding_agent_shim.js.txt",
    include_bytes!("../../../assets/shims/pi_coding_agent_shim.js.txt.deflate"),
);
pub static PI_AGENT_CORE_SHIM: Shim = Shim::new(
    "pi_agent_core_shim.js.txt",
    include_bytes!("../../../assets/shims/pi_agent_core_shim.js.txt.deflate"),
);
pub static PI_AI_SHIM: Shim = Shim::new(
    "pi_ai_shim.js.txt",
    include_bytes!("../../../assets/shims/pi_ai_shim.js.txt.deflate"),
);
pub static PI_AI_OAUTH_SHIM: Shim = Shim::new(
    "pi_ai_oauth_shim.js.txt",
    include_bytes!("../../../assets/shims/pi_ai_oauth_shim.js.txt.deflate"),
);
pub static PI_AI_PROVIDERS_ALL_SHIM: Shim = Shim::new(
    "pi_ai_providers_all_shim.js.txt",
    include_bytes!("../../../assets/shims/pi_ai_providers_all_shim.js.txt.deflate"),
);
pub static PI_TUI_SHIM: Shim = Shim::new(
    "pi_tui_shim.js.txt",
    include_bytes!("../../../assets/shims/pi_tui_shim.js.txt.deflate"),
);
/// `@earendil-works/pi-codemode`: the pure-JS surfaces (source parsing,
/// identifier conversion, declaration rendering) are faithful ports — up to
/// upstream `7d75a0e3e`, including the constraint tags of `8a9c051f8` and the
/// MCP decoupling of `24382bedd`/`c661bc574`. The engine-backed faces route
/// to the host codemode executor (`src/plugins/codemode.rs`, the same engine
/// the builtin `codemode` tool uses) through the async host bridge
/// (`__pir_host.codemode*`): `CodemodeSandboxBase`/`CodemodeSandbox`/
/// `InlineCodemodeSandbox` `execute()`/`start()` are real (DRIFT §2.3 item 2 /
/// D28), while `RemoteCodemodeSandbox`/`serveCodemodeRemote` (no second
/// isolate) and `loadQuickJSWasm`/`cloudflareQuickJSWasm` (no wasm engine —
/// pir hosts the native QuickJS) keep throwing. One module serves every
/// published subpath (`/declarations`, `/source`, `/worker`, `/portable`,
/// `/cloudflare`).
pub static PI_CODEMODE_SHIM: Shim = Shim::new(
    "pi_codemode_shim.js.txt",
    include_bytes!("../../../assets/shims/pi_codemode_shim.js.txt.deflate"),
);
/// `better-sqlite3` virtual package: the native NAPI addon re-created over
/// the host SQLite bridge (see `js/better_sqlite3_shim.js.txt`). Registered as
/// a bare-package alias (not a `node:`/`pi-*` name) because npm packages
/// import it bare from both ESM and CJS.
pub static BETTER_SQLITE3_SHIM: Shim = Shim::new(
    "better_sqlite3_shim.js.txt",
    include_bytes!("../../../assets/shims/better_sqlite3_shim.js.txt.deflate"),
);
/// `sqlite3` virtual package: the other native SQLite NAPI addon
/// (`node_sqlite3.node`), re-created over the same host SQLite bridge — the
/// callback-style Database/Statement surface (`new Database(file, cb)` +
/// `db.run/get/all/each/exec/prepare`) plus the OPEN_*/result-code/LIMIT_*
/// constants (see `js/sqlite3_shim.js.txt`).
pub static SQLITE3_SHIM: Shim = Shim::new(
    "sqlite3_shim.js.txt",
    include_bytes!("../../../assets/shims/sqlite3_shim.js.txt.deflate"),
);
/// `bcrypt` virtual package: the NAPI native addon's JS face over a host
/// EksBlowfish implementation (`host_functions/bcrypt_fns.rs`), so
/// `hashSync`/`compareSync`/`hash`/`compare`/`genSalt(Sync)`/`getRounds` run
/// without the native binding (see `js/bcrypt_shim.js.txt`).
pub static BCRYPT_SHIM: Shim = Shim::new(
    "bcrypt_shim.js.txt",
    include_bytes!("../../../assets/shims/bcrypt_shim.js.txt.deflate"),
);

/// `@earendil-works/pi-env`: the client half of the env line over pir's own
/// protocol-v1 daemon (`pir env-daemon`, src/env_daemon/). The upstream
/// `packages/env/src` surface is ported statement by statement — `Connection`
/// (framing, id correlation, cancellation, ping/silence, sync line),
/// `RemoteExecutionEnv`, `RemoteWatcher`, `openTerminal`/`RemoteTerminal`,
/// `LocalExecutionEnv`, and the SSH deploy surface over the system `ssh`.
/// The durable `env` contract values (`ok`/`err`/`FileError`/`ExecutionError`/
/// `StreamDecoder`) are imported from the `@earendil-works/pi-durable/env`
/// virtual module (D4), so client and daemon-side code share one contract.
pub static PI_ENV_SHIM: Shim = Shim::new(
    "pi_env_shim.js.txt",
    include_bytes!("../../../assets/shims/pi_env_shim.js.txt.deflate"),
);

/// `@earendil-works/chord`: the chord 1.2.0 (`0104fef1e` chord2) object/host
/// runtime — the portable kernel (contracts, declarations, cells/derivations,
/// records/handles, hosts/epochs, memory/WebSocket links, the frame-level
/// connection + remote handles, reload, replicated state) and the standalone
/// transactional JSON tracker (`/delta`), ported from upstream `packages/chord`
/// at `3daa20280`; the wire modules were regenerated for the D33 protocol
/// generation (schema-derived frames, `ack`, flow control, >64 KiB payload
/// deflate). One module serves the root, `./context` and `./delta`
/// entries; `./node` is a separate throws-tier module (see
/// `PI_CHORD_NODE_SHIM`). The only unsupported upstream surface is `./node`;
/// typebox-dependent validation runs through this runtime's typebox shims;
/// `not`/`contains`/`patternProperties` are modelled as of 2026-10-14
/// (self-tests and fuzz pass; the corpus re-run status is per the foreman
/// gate record).
pub static PI_CHORD_SHIM: Shim = Shim::new(
    "pi_chord_shim.js.txt",
    include_bytes!("../../../assets/shims/pi_chord_shim.js.txt.deflate"),
);

/// `@earendil-works/chord/node`: throws tier. Child-host processes
/// (`forkDriver`/`createHostManager`/`childControl`/`processLock`) and the facet
/// module loader (`nodeLoader`: `node:module` registerHooks + `chord2-reload`
/// query) have no counterpart in this single-process extension host; every entry
/// point throws the standard `ERR_NOT_SUPPORTED` error at first use, so an
/// extension that imports the subpath still links.
pub static PI_CHORD_NODE_SHIM: Shim = Shim::new(
    "pi_chord_node_shim.js.txt",
    include_bytes!("../../../assets/shims/pi_chord_node_shim.js.txt.deflate"),
);

/// `@earendil-works/pi-durable`: the durable SDK thin surface (wave D4). The
/// storage contract and its three backends (`MemoryStorage`, `JsonlStorage`,
/// `SqliteStorage` — `node:sqlite` is real over rusqlite), the session kernel
/// (`createSession`: commit/publication/reopen), `ExecutionEnv` +
/// `NodeExecutionEnv`, `/tools`, `/tool-search` and `/codemode` (the
/// `CodemodeSandboxBase` replay contract comes from the pi-codemode shim, wave
/// D3), plus the upstream extension/tool/document/task definitions — all ported
/// statement by statement from `packages/durable` at 7d75a0e3e. `Harness`,
/// `createRegistry` and `watchEvents` throw the standard `ERR_NOT_SUPPORTED`
/// error (the scheduler engine is the D5 decision); `/images*`, `/testing` and
/// `/storage/sqlite/cloudflare` live in the separate throws module below. This
/// shim imports the npm package `diff` (a durable dependency), which the
/// importing extension must have installed — see the shim header.
pub static PI_DURABLE_SHIM: Shim = Shim::new(
    "pi_durable_shim.js.txt",
    include_bytes!("../../../assets/shims/pi_durable_shim.js.txt.deflate"),
);

/// `@earendil-works/pi-durable/env` + `/env/node`: the execution-environment
/// contract (`Result`/`FileError`/`ExecutionError`/`StreamDecoder`/
/// `LineScanner`) and its local Node implementation (`NodeExecutionEnv`), ported
/// verbatim from upstream `src/env/{index,decode,line-scan,node,node-watch}.ts`.
/// Its own module because `@earendil-works/pi-env` imports the contract values
/// from here and only needs `node:*` for that — the kernel module's npm
/// dependency (`diff`) stays out of this path.
pub static PI_DURABLE_ENV_SHIM: Shim = Shim::new(
    "pi_durable_env_shim.js.txt",
    include_bytes!("../../../assets/shims/pi_durable_env_shim.js.txt.deflate"),
);

/// `@earendil-works/pi-durable` throws tier: `/images`, `/images/node`,
/// `/images/cloudflare` (Photon wasm + worker thread), `/testing` (vitest-bound
/// conformance registrars) and `/storage/sqlite/cloudflare` (Durable Object SQL).
/// Every exported value throws the standard `ERR_NOT_SUPPORTED` error at first
/// use, so an extension that imports one of these entries still links.
pub static PI_DURABLE_THROWS_SHIM: Shim = Shim::new(
    "pi_durable_throws_shim.js.txt",
    include_bytes!("../../../assets/shims/pi_durable_throws_shim.js.txt.deflate"),
);

pub fn canonical_virtual_name(name: &str) -> Option<&'static str> {
    const KNOWN: &[(&str, &str)] = &[
        ("typebox", "typebox"),
        ("typebox/compile", "typebox/compile"),
        ("typebox/value", "typebox/value"),
        ("@sinclair/typebox", "typebox"),
        ("@sinclair/typebox/compile", "typebox/compile"),
        ("@sinclair/typebox/value", "typebox/value"),
        (
            "@earendil-works/pi-coding-agent",
            "@earendil-works/pi-coding-agent",
        ),
        (
            "@mariozechner/pi-coding-agent",
            "@earendil-works/pi-coding-agent",
        ),
        (
            "@earendil-works/pi-agent-core",
            "@earendil-works/pi-agent-core",
        ),
        (
            "@mariozechner/pi-agent-core",
            "@earendil-works/pi-agent-core",
        ),
        ("@earendil-works/pi-ai", "@earendil-works/pi-ai"),
        ("@earendil-works/pi-ai/compat", "@earendil-works/pi-ai"),
        // Published subpaths of the pi-ai package (`package.json` exports):
        // their surfaces are subsets of the compat bundle served for the root
        // entry, so they resolve to the same virtual module.
        ("@earendil-works/pi-ai/models", "@earendil-works/pi-ai"),
        (
            "@earendil-works/pi-ai/bedrock-provider",
            "@earendil-works/pi-ai",
        ),
        ("@earendil-works/pi-ai/bun-oauth", "@earendil-works/pi-ai"),
        ("@mariozechner/pi-ai", "@earendil-works/pi-ai"),
        // Upstream virtual modules map the mariozechner compat entry to the
        // compat bundle too (virtual-modules.ts:26-37); leaving it unmapped
        // rejected the whole extension at dependency scan time.
        ("@mariozechner/pi-ai/compat", "@earendil-works/pi-ai"),
        ("@earendil-works/pi-ai/oauth", "@earendil-works/pi-ai/oauth"),
        ("@mariozechner/pi-ai/oauth", "@earendil-works/pi-ai/oauth"),
        (
            "@earendil-works/pi-ai/providers/all",
            "@earendil-works/pi-ai/providers/all",
        ),
        (
            "@mariozechner/pi-ai/providers/all",
            "@earendil-works/pi-ai/providers/all",
        ),
        ("@earendil-works/pi-tui", "@earendil-works/pi-tui"),
        ("@mariozechner/pi-tui", "@earendil-works/pi-tui"),
        ("@earendil-works/pi-codemode", "@earendil-works/pi-codemode"),
        // Every published pi-codemode subpath entry resolves to the one shim
        // module: `/declarations`, `/source` and `/worker` are its named
        // subsets, `/portable` (inline + remote sandboxes) and `/cloudflare`
        // (Wrangler CompiledWasm entry) add the runtime faces that throw.
        (
            "@earendil-works/pi-codemode/declarations",
            "@earendil-works/pi-codemode",
        ),
        (
            "@earendil-works/pi-codemode/source",
            "@earendil-works/pi-codemode",
        ),
        (
            "@earendil-works/pi-codemode/worker",
            "@earendil-works/pi-codemode",
        ),
        (
            "@earendil-works/pi-codemode/portable",
            "@earendil-works/pi-codemode",
        ),
        (
            "@earendil-works/pi-codemode/cloudflare",
            "@earendil-works/pi-codemode",
        ),
        // better-sqlite3: virtual package over the host SQLite bridge (the
        // real node_modules package is a native addon and can never load).
        ("better-sqlite3", "better-sqlite3"),
        // sqlite3: same situation, callback-style API (node_sqlite3.node).
        ("sqlite3", "sqlite3"),
        // bcrypt: NAPI native addon (bcrypt.node); the host carries the
        // EksBlowfish implementation.
        ("bcrypt", "bcrypt"),
        // @earendil-works/pi-env: one published entry ("."); everything the
        // client needs is in the single shim module.
        ("@earendil-works/pi-env", "@earendil-works/pi-env"),
        // @earendil-works/chord: the root and the two published subpaths whose
        // surfaces are subsets of the one shim module; `./node` is its own
        // throws-tier module and must not fold into the kernel.
        ("@earendil-works/chord", "@earendil-works/chord"),
        ("@earendil-works/chord/context", "@earendil-works/chord"),
        ("@earendil-works/chord/delta", "@earendil-works/chord"),
        ("@earendil-works/chord/node", "@earendil-works/chord/node"),
        // @earendil-works/pi-durable: the ten real entries fold into the one
        // kernel module (their surfaces are its published subsets); the five
        // throws-tier entries (`/images`, `/images/node`, `/images/cloudflare`,
        // `/testing`, `/storage/sqlite/cloudflare`) fold into the separate
        // throws module.
        ("@earendil-works/pi-durable", "@earendil-works/pi-durable"),
        (
            "@earendil-works/pi-durable/tools",
            "@earendil-works/pi-durable",
        ),
        (
            "@earendil-works/pi-durable/env",
            "@earendil-works/pi-durable/env",
        ),
        (
            "@earendil-works/pi-durable/env/node",
            "@earendil-works/pi-durable/env",
        ),
        (
            "@earendil-works/pi-durable/tool-search",
            "@earendil-works/pi-durable",
        ),
        (
            "@earendil-works/pi-durable/codemode",
            "@earendil-works/pi-durable",
        ),
        (
            "@earendil-works/pi-durable/storage/memory",
            "@earendil-works/pi-durable",
        ),
        (
            "@earendil-works/pi-durable/storage/jsonl",
            "@earendil-works/pi-durable",
        ),
        (
            "@earendil-works/pi-durable/storage/jsonl/node",
            "@earendil-works/pi-durable",
        ),
        (
            "@earendil-works/pi-durable/storage/sqlite",
            "@earendil-works/pi-durable",
        ),
        (
            "@earendil-works/pi-durable/storage/sqlite/node",
            "@earendil-works/pi-durable",
        ),
        (
            "@earendil-works/pi-durable/images",
            "@earendil-works/pi-durable/throws",
        ),
        (
            "@earendil-works/pi-durable/images/node",
            "@earendil-works/pi-durable/throws",
        ),
        (
            "@earendil-works/pi-durable/images/cloudflare",
            "@earendil-works/pi-durable/throws",
        ),
        (
            "@earendil-works/pi-durable/testing",
            "@earendil-works/pi-durable/throws",
        ),
        (
            "@earendil-works/pi-durable/storage/sqlite/cloudflare",
            "@earendil-works/pi-durable/throws",
        ),
        // The throws tier's own canonical name (the table maps the five
        // published subpaths onto it above; the self-entry is what lets the
        // loader find the module source when it is resolved by canonical name).
        (
            "@earendil-works/pi-durable/throws",
            "@earendil-works/pi-durable/throws",
        ),
        // pi-ai `utils/*` subpaths the durable port imports (`utils/estimate` is
        // deliberately absent: the pi-ai shim does not carry its two names, so
        // the durable shim vendors that module — see its header).
        (
            "@earendil-works/pi-ai/utils/overflow",
            "@earendil-works/pi-ai",
        ),
        ("@earendil-works/pi-ai/utils/retry", "@earendil-works/pi-ai"),
        (
            "@earendil-works/pi-ai/utils/transcript",
            "@earendil-works/pi-ai",
        ),
        ("@earendil-works/pi-ai/utils/uuid", "@earendil-works/pi-ai"),
        (
            "@earendil-works/pi-ai/utils/validation",
            "@earendil-works/pi-ai",
        ),
    ];
    KNOWN
        .iter()
        .find(|(from, _)| *from == name)
        .map(|(_, canonical)| *canonical)
        .or_else(|| super::node_shims::canonical_node_name(name))
}

/// Source of the virtual module for a canonical name.
pub fn source_for(canonical: &str) -> Option<&'static str> {
    match canonical {
        "typebox" => Some(TYPEBOX_SHIM.source()),
        "typebox/compile" => Some(TYPEBOX_COMPILE_SHIM.source()),
        "typebox/value" => Some(TYPEBOX_VALUE_SHIM.source()),
        "@earendil-works/pi-coding-agent" => Some(PI_CODING_AGENT_SHIM.source()),
        "@earendil-works/pi-agent-core" => Some(PI_AGENT_CORE_SHIM.source()),
        "@earendil-works/pi-ai" => Some(PI_AI_SHIM.source()),
        "@earendil-works/pi-ai/oauth" => Some(PI_AI_OAUTH_SHIM.source()),
        "@earendil-works/pi-ai/providers/all" => Some(PI_AI_PROVIDERS_ALL_SHIM.source()),
        "@earendil-works/pi-tui" => Some(PI_TUI_SHIM.source()),
        "@earendil-works/pi-codemode" => Some(PI_CODEMODE_SHIM.source()),
        "@earendil-works/pi-env" => Some(PI_ENV_SHIM.source()),
        "@earendil-works/chord" => Some(PI_CHORD_SHIM.source()),
        "@earendil-works/chord/node" => Some(PI_CHORD_NODE_SHIM.source()),
        "@earendil-works/pi-durable" => Some(PI_DURABLE_SHIM.source()),
        "@earendil-works/pi-durable/env" => Some(PI_DURABLE_ENV_SHIM.source()),
        "@earendil-works/pi-durable/throws" => Some(PI_DURABLE_THROWS_SHIM.source()),
        "better-sqlite3" => Some(BETTER_SQLITE3_SHIM.source()),
        "sqlite3" => Some(SQLITE3_SHIM.source()),
        "bcrypt" => Some(BCRYPT_SHIM.source()),
        _ => super::node_shims::source_for(canonical),
    }
}

/// Bare npm package names that resolve to virtual shims ahead of the on-disk
/// node_modules tree in the **CJS require path** (`npm_resolve_op` returns a
/// builtin-kind result keyed by the bare name; the bootstrap registry holds
/// the module). ESM resolution already consults `canonical_virtual_name` —
/// this function exists so the require path can pick exactly these three
/// native-addon packages (better-sqlite3, sqlite3, bcrypt) without routing
/// pi-* virtual packages (which do exist on disk) through the same override.
pub fn canonical_bare_package_alias(name: &str) -> Option<&'static str> {
    match name {
        "better-sqlite3" => Some("better-sqlite3"),
        "sqlite3" => Some("sqlite3"),
        "bcrypt" => Some("bcrypt"),
        _ => None,
    }
}

/// Minimal TypeBox shim: chainable schema builders producing JSON-Schema
/// objects. Covers the surface used by the upstream example extensions:
/// `Object/String/Number/Integer/Boolean/Array/Optional/Union/Literal/
/// Enum/Unknown/Any/Unsafe`. `Compile`/`Value.Check`/`Value.Errors` are backed
/// by the real JSON-Schema subset validator implemented in the host prelude
/// (`globalThis.__pir_schemaValidate`).
///
/// This is the **root** entry (`typebox` / `@sinclair/typebox`) only. The
/// subpath entries are separate upstream modules with their own top-level
/// exports (`typebox/value` re-exports `Check`/`Errors`/`Parse`/...), so each
/// gets its own shim below.
pub static TYPEBOX_SHIM: Shim = Shim::new(
    "typebox_shim.js.txt",
    include_bytes!("../../../assets/shims/typebox_shim.js.txt.deflate"),
);

/// `typebox/value` + `@sinclair/typebox/value`: the subpath entry as its own
/// module — `Check`/`Errors`/`Parse`/`Clone`/`Assert`/`Convert`/`Clean`/
/// `Default`/`Decode`/`Encode` as bare top-level exports, plus the `Value`
/// namespace and `AssertError`/`ParseError`. Serving the root shim here made
/// `import { Check } from "typebox/value"` fail at link time, which rejected
/// the importing extension outright (pi-mcp-adapter, @tintinweb/pi-subagents).
pub static TYPEBOX_VALUE_SHIM: Shim = Shim::new(
    "typebox_value_shim.js.txt",
    include_bytes!("../../../assets/shims/typebox_value_shim.js.txt.deflate"),
);

/// `typebox/compile` + `@sinclair/typebox/compile`: subpath entry as its own
/// module — top-level `Compile` (also the default export) with `Type`/`Value`
/// carried along. Re-exports the root shim, so the compiled-validator shape
/// stays single-sourced.
pub static TYPEBOX_COMPILE_SHIM: Shim = Shim::new(
    "typebox_compile_shim.js.txt",
    include_bytes!("../../../assets/shims/typebox_compile_shim.js.txt.deflate"),
);

/// Every file-backed shim in this module, for the byte-identity test.
pub(crate) const SHIMS: &[(&str, &Shim)] = &[
    ("pi_coding_agent_shim.js.txt", &PI_CODING_AGENT_SHIM),
    ("pi_agent_core_shim.js.txt", &PI_AGENT_CORE_SHIM),
    ("pi_ai_shim.js.txt", &PI_AI_SHIM),
    ("pi_ai_oauth_shim.js.txt", &PI_AI_OAUTH_SHIM),
    ("pi_ai_providers_all_shim.js.txt", &PI_AI_PROVIDERS_ALL_SHIM),
    ("pi_tui_shim.js.txt", &PI_TUI_SHIM),
    ("pi_codemode_shim.js.txt", &PI_CODEMODE_SHIM),
    ("better_sqlite3_shim.js.txt", &BETTER_SQLITE3_SHIM),
    ("sqlite3_shim.js.txt", &SQLITE3_SHIM),
    ("bcrypt_shim.js.txt", &BCRYPT_SHIM),
    ("pi_env_shim.js.txt", &PI_ENV_SHIM),
    ("pi_chord_shim.js.txt", &PI_CHORD_SHIM),
    ("pi_chord_node_shim.js.txt", &PI_CHORD_NODE_SHIM),
    ("pi_durable_shim.js.txt", &PI_DURABLE_SHIM),
    ("pi_durable_env_shim.js.txt", &PI_DURABLE_ENV_SHIM),
    ("pi_durable_throws_shim.js.txt", &PI_DURABLE_THROWS_SHIM),
    ("typebox_shim.js.txt", &TYPEBOX_SHIM),
    ("typebox_value_shim.js.txt", &TYPEBOX_VALUE_SHIM),
    ("typebox_compile_shim.js.txt", &TYPEBOX_COMPILE_SHIM),
];
