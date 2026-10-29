"""WAVE-REPORT: record the delta-dispatch numbers + the acceptance target."""

import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REL = "WAVE-REPORT.md"

BLOCK = """**两次实测（同一工装，10 000 delta，无头）**：

| leg | 修复前（08:13 二进制） | 第一轮修复后（12:47） | 目标（delta-only wire + JS 活对象） |
|---|---|---|---|
| 无扩展 | 2.00 s / 31.6 MB | 2.41 s / 30.1 MB | 不变 |
| 一个空 handler | 33.54 s / 804.6 MB | 6.03 s / 709.1 MB | **≤3 s / ≤60 MB** |
| pi-phone 形态（读 `role`/`timestamp` + 转发 delta） | — | **5.01 s / 770.7 MB** | **≤3 s / ≤60 MB** |
| 读 `message.content` | — | 5.39 s / 768.9 MB | **≤3 s / ≤60 MB** |
| handler + `JSON.stringify(event.message)` | 43.05 s / 808.2 MB | 13.91 s / 788.7 MB | 慢是 handler 自己的选择（上游同样），但 pir 自身状态不得随之增长 |

第一轮修复（单次序列化 + 去掉回程 stringify/parse）把墙钟降了 5.6×，但内存几乎不动 —— 因为**每个 delta 仍要实体化一次累计快照**
（10 000 × 平均 ~70 KB ≈ 700 MB 瞬时分配），与 handler 是否读它无关。因此第二轮按上游语义改成：wire 只带 delta，
JS prelude 维护每个流的**活消息对象**（text/thinking 追加、toolCall 追加、usage/stopReason 终结），`event.message` 与
`assistantMessageEvent.partial` 都指向它，读取是普通属性访问（无 getter、无宿主调用、无 parse）；每 delta O(delta)，
流结束丢弃。正确性由"每个 delta 的 JS 累积对象与 Rust 参考构造 deep-equal"的脚本化测试钉住（覆盖文本/思考/工具调用/usage/终态，
不认识的 delta 形状必须显式报错而不是静默漂移）。

"""


def main() -> int:
    path = os.path.join(ROOT, REL)
    with open(path, encoding="utf-8", newline="") as fh:
        text = fh.read()
    if "第二轮按上游语义改成" in text:
        print("already present")
        return 0
    nl = "\r\n" if "\r\n" in text else "\n"
    anchor = "**未修（在别的车道/后续）**"
    if anchor not in text:
        print("anchor missing", file=sys.stderr)
        return 1
    block = BLOCK if nl == "\n" else BLOCK.replace("\n", "\r\n")
    with open(path, "w", encoding="utf-8", newline="") as fh:
        fh.write(text.replace(anchor, block + anchor, 1))
    print("WAVE-REPORT: delta numbers + target recorded")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
