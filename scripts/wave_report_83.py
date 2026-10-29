"""WAVE-REPORT: archive the TUI-lane root cause + fixes (2026-10-16 late)."""

import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REL = "WAVE-REPORT.md"

BLOCK = """### 8.3 "内存极大 / 超级慢 / 思考不显示"的真根因（2026-10-16，实测定位）

用户交互模式报告四症状。逐项实测（release 二进制，同一条 10 000 delta 流：8 000 `reasoning_content` + 2 000 `content`，
ConPTY 120x40，RSS 每 250 ms 采样）：

| 配置 | 最后一个文本 delta 上屏 | 峰值工作集 | CPU |
|---|---|---|---|
| 无扩展 handler | 1.1 s | 38–40 MB | 1.0 s |
| **一个空 handler**：`pi.on("message_update", () => {})` | **23.6 s** | **442 MB** | 25.4 s |
| 空 handler + `JSON.stringify(event.message)` | 30.7 s | 440 MB | 32.5 s |
| 用户真实配置（7 包）**去掉 pi-phone** | 1.2 s | ~40 MB | — |
| 用户真实配置 **仅 pi-phone** | 20.2 s | 445 MB | 21.9 s |

无头对照 `pir print`（无 TUI，同一空扩展）：3.03 s → **25.55 s** ⇒ **与 TUI 无关**。机制：
`agent_loop.rs::message_update_payload` 每个 delta 做 `serde_json::to_value(event)` **加** `.get("partial").cloned()`
（整条累计消息两次全量拷贝，O(reply)/token），回复通道再叠 ~3 次消息级拷贝（`dispatch.rs` `to_string` →
QuickJS `JSON.parse/stringify` → Rust parse → clone）；`MessageUpdateDispatcher` 只界定了 backlog(16)，
稳态速率被 JS 往返钳在 ~380 delta/s。RSS 是心跳式高水位（流结束回落到 ~50 MB）。

**已修（TUI 侧，真 bug）**：交互会话起始 thinking level 硬编码 `ThinkingLevel::Medium`，而不是
`settings.defaultThinkingLevel` —— 用户模型（`ar/gpt-5`，`thinkingLevelMap` 把 `medium` 显式映成 `null`）
因此一直收到 `reasoning_effort:"medium"`，端点没有理由开推理流，"思考过程也不展示"。现改为
`initial_interactive_thinking()`（settings → 共享 `DEFAULT_THINKING_LEVEL`；`off` 视为未设）。
同一车道还发现并修复：**位置参数启动 prompt**（`pir "…"`）没有同步点亮 `Working` 标签，导致首次运行前的整个
窗口跑在 100 ms 空闲轮询档（"很久才出字"的其中一段）。

**未修（在别的车道/后续）**：per-delta `message_update` 载荷（上表第 2/3 行，已派专门车道）；
随 transcript 增长的整体帧比对成本（375 → 214 delta/s over 24 turns，~2 ms/帧 @3000 行，主屏 diff 固有）；
print 侧 thinking 解析缺口（已派车道）。

"""


def main() -> int:
    path = os.path.join(ROOT, REL)
    with open(path, encoding="utf-8", newline="") as fh:
        text = fh.read()
    if "8.3 " in text and "内存极大" in text:
        print("already present")
        return 0
    nl = "\r\n" if "\r\n" in text else "\n"
    anchor = "## 3. 红点台账"
    if nl == "\r\n":
        anchor = anchor
    if anchor not in text:
        print("anchor missing", file=sys.stderr)
        return 1
    block = BLOCK if nl == "\n" else BLOCK.replace("\n", "\r\n")
    with open(path, "w", encoding="utf-8", newline="") as fh:
        fh.write(text.replace(anchor, block + anchor, 1))
    print("WAVE-REPORT §8.3 archived")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
