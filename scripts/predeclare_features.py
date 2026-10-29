"""Pre-declare the provider/CLI/grep-find features (Cargo.toml, single writer)."""

import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REL = "Cargo.toml"


def main() -> int:
    path = os.path.join(ROOT, REL)
    with open(path, encoding="utf-8", newline="") as fh:
        text = fh.read()
    if "provider-bedrock" in text:
        print("already present")
        return 0

    def rep(old: str, new: str) -> None:
        nonlocal text
        if old not in text:
            print(f"MISSING anchor:\n{old[:200]}", file=sys.stderr)
            sys.exit(1)
        text = text.replace(old, new, 1)

    rep('clap = { version = "4", features = ["derive"] }',
        'clap = { version = "4", features = ["derive"], optional = true }')
    rep('hmac = "0.12"', 'hmac = { version = "0.12", optional = true }')
    rep('rsa = { version = "0.9", features = ["sha2"] }',
        'rsa = { version = "0.9", features = ["sha2"], optional = true }')
    rep('grep-searcher = "0.1"', 'grep-searcher = { version = "0.1", optional = true }')
    rep('grep-regex = "0.1"', 'grep-regex = { version = "0.1", optional = true }')
    rep('ignore = "0.4"', 'ignore = { version = "0.4", optional = true }')

    rep('''full = [
    "tui", "extensions", "python", "mcp", "env", "server", "subagent", "tool-repair", "tool-search",
]''',
        '''full = [
    "tui",
    "extensions",
    "python",
    "mcp",
    "env",
    "server",
    "subagent",
    "tool-repair",
    "tool-search",
    "provider-openai",
    "provider-anthropic",
    "provider-google",
    "provider-bedrock",
    "provider-mistral",
    "cli",
    "grep-find",
]''')

    rep('''# Terminal UI: crossterm loop, editor, renderers, clipboard (arboard), grapheme''',
        '''# AI wire adapters: each feature gates one provider family's adapter module and
# its registry/construction arm. `provider-google` carries the Vertex
# service-account JWT (`rsa`), `provider-bedrock` the SigV4 signing (`hmac`);
# `extensions` needs both too (node:crypto), so they are shared, not owned.
provider-openai = []
provider-anthropic = []
provider-google = ["dep:rsa"]
provider-bedrock = ["dep:hmac"]
provider-mistral = []
# The clap-based CLI argument surface (`src/coding_agent/cli.rs`); the binary
# requires it, an SDK embed does not.
cli = ["dep:clap"]
# `grep`/`find` tools and the ripgrep engine crates they run in-process
# (`env_daemon`'s directory listing shares `ignore`, and carries `env` itself).
grep-find = ["dep:grep-searcher", "dep:grep-regex", "dep:ignore"]

# Terminal UI: crossterm loop, editor, renderers, clipboard (arboard), grapheme''')

    rep('''    "dep:md-5",
    "dep:sha1",
    "dep:pbkdf2",''',
        '''    "dep:md-5",
    "dep:sha1",
    "dep:pbkdf2",
    # node:crypto's RSA sign/verify and HMAC backends.
    "dep:rsa",
    "dep:hmac",''')
    rep('env = ["dep:notify"]', 'env = ["dep:notify", "dep:ignore"]')

    with open(path, "w", encoding="utf-8", newline="") as fh:
        fh.write(text)
    print("features pre-declared: providers x5, cli, grep-find")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
