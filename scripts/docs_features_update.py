"""Docs: correct the T1 record + list the new feature families (index-spliced)."""

import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

NEW_BLOCK = """image-protocol = ["tui"]                            # TUI 半块图预览渲染器（image crate 本身是核心）
# 提供方线协议适配器（一家一特性；provider-google 带 Vertex 的 rsa JWT，
# provider-bedrock 带 SigV4 的 hmac；extensions 也需要这两个，故共享）
provider-openai = []            # 骨干：openai chat + responses/Codex + openrouter/compatible + pi_messages
provider-anthropic = []
provider-google = ["dep:rsa"]
provider-bedrock = ["dep:hmac"]
provider-mistral = []
# 宿主面与工具面
cli       = ["dep:clap"]                                 # src/coding_agent/cli.rs 的参数面
grep-find = ["dep:grep-searcher", "dep:grep-regex", "dep:ignore"]  # grep/find 工具 + ripgrep 引擎
"""

GATING_MAP = """## 6b. 特性落地面（新增族的门控位置）

| 特性 | 门控面 |
|---|---|
| `provider-*` | `src/ai/api/<family>` 适配器模块 + 构造/注册臂 + catalog/api-name 映射；关掉时构造该 provider 报出"缺哪个特性"的明确错误 |
| `cli` | `src/coding_agent/cli.rs`（clap 参数面）及其导出 |
| `grep-find` | `tools/{grep,find}.rs` + 工具注册/名单表 + `env_daemon/list.rs` 的 `ignore` 用点（`env` 关闭时给明确错误，不静默给空列表） |

"""


def main() -> int:
    # --- SDK-SCOPE.md -----------------------------------------------------
    path = os.path.join(ROOT, "SDK-SCOPE.md")
    with open(path, encoding="utf-8", newline="") as fh:
        t = fh.read()
    if "provider-openai = []" not in t:
        start = t.index('image-protocol = ["tui"]')
        fence = t.index("```", start)
        t = t[:start] + NEW_BLOCK + t[fence:]
    if "## 6b. 特性落地面" not in t:
        anchor = "## 6. 不在本次范围"
        t = t.replace(anchor, GATING_MAP + anchor, 1)
    t = t.replace(
        "T1 裁剪（已落地）：",
        "T1 裁剪（隔离副本实测 −25 包 / −115 s CPU，2026-10-16 落到树上）：",
        1,
    )
    with open(path, "w", encoding="utf-8", newline="") as fh:
        fh.write(t)
    print("SDK-SCOPE.md updated")

    # --- HANDOFF.md -------------------------------------------------------
    path = os.path.join(ROOT, "HANDOFF.md")
    with open(path, encoding="utf-8", newline="") as fh:
        h = fh.read()
    nl = "\r\n" if "\r\n" in h else "\n"
    old = "`tool-repair`, `tool-search`); `full` is the default and what the binary"
    new = (
        "`tool-repair`, `tool-search`, `provider-openai/-anthropic/-google/-bedrock/-mistral`," + nl
        + "  `cli`, `grep-find`); `full` is the default and what the binary"
    )
    if old in h:
        h = h.replace(old, new, 1)
    marker = "- Feature gating: never let a `#[cfg]` change the feature-ON path; keep the"
    add = (
        "- The `[features]` list is the SDK contract: declare a feature (and mark the deps" + nl
        + "  it owns `optional`) before gating code; `Cargo.toml` is single-writer — a lane" + nl
        + "  that needs a feature asks, it does not edit the manifest." + nl
    )
    if "single-writer — a lane" not in h and marker in h:
        h = h.replace(marker, add + marker, 1)
    with open(path, "w", encoding="utf-8", newline="") as fh:
        fh.write(h)
    print("HANDOFF.md updated")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
