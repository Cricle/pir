#!/usr/bin/env python3
"""Render logs/lane-docker-lite/REPORT.md from summary.json + manifest-cache.json.

The PowerShell puller writes its own per-group REPORT.md. This renderer is for
a delivery snapshot when the long-running pull is paused or still in flight: it
reads the current machine-readable state and adds the measured network /
capacity notes taken during this lane.
"""

from __future__ import annotations

import json
from collections import OrderedDict
from datetime import datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
LOG = ROOT / "logs" / "lane-docker-lite"
REPORT = LOG / "REPORT.md"


def fmt_bytes(value: float | None, binary: bool = True) -> str:
    if value is None:
        return "-"
    value = float(value)
    units = [("TiB", 1 << 40), ("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)] if binary else [
        ("TB", 1e12), ("GB", 1e9), ("MB", 1e6), ("kB", 1e3)
    ]
    for name, size in units:
        if value >= size:
            return f"{value / size:.2f} {name}"
    return f"{int(value)} B"


def main() -> int:
    summary = json.loads((LOG / "summary.json").read_text(encoding="utf-8"))
    cache = {r["instance"]: r for r in json.loads((LOG / "manifest-cache.json").read_text(encoding="utf-8"))}
    instances = summary["instances"]
    progress = summary["progress"]
    disk = summary["disk"]
    estimate = summary["estimate"]
    eta = summary["eta"]

    by_repo: "OrderedDict[str, list[dict]]" = OrderedDict()
    for rec in instances:
        by_repo.setdefault(rec["repo"], []).append(rec)

    def counts(recs):
        return {
            "total": len(recs),
            "pulled": sum(1 for r in recs if r["status"] == "pulled"),
            "skipped": sum(1 for r in recs if r["status"] == "skipped"),
            "failed": sum(1 for r in recs if r["status"] == "failed"),
            "pending": sum(1 for r in recs if r["status"] == "pending"),
        }

    def unique_layers(recs):
        seen = {}
        for r in recs:
            mr = cache.get(r["instance"]) or {}
            for layer in mr.get("layers") or []:
                seen[layer["digest"]] = layer["size"]
        return sum(seen.values())

    remaining_unique = unique_layers([r for r in instances if r["status"] in ("pending", "failed")])

    lines: list[str] = []
    lines += ["# Lane Docker Lite — SWE-bench Lite 300 镜像全量拉取", ""]
    lines += [f"生成时间：{datetime.now().strftime('%Y-%m-%d %H:%M:%S')}（Asia/Shanghai）", ""]
    lines += [f"状态：**{summary.get('status', 'unknown')}**（phase: `{summary.get('phase', '-')}`）", ""]
    lines += ["## TL;DR", ""]
    lines += ["| 项 | 结果 |", "|---|---|"]
    lines += [f"| 数据集 | SWE-bench Lite `test.parquet`（300 实例 / 12 repo） |"]
    lines += [f"| 实例总数 | {progress['total']} |"]
    lines += [f"| 已拉取 (pulled) | **{progress['pulled']}** |"]
    lines += [f"| 已存在跳过 (skipped) | **{progress['skipped']}** |"]
    lines += [f"| 失败 (failed) | {progress['failed']} |"]
    lines += [f"| 待拉 (pending) | **{progress['pending']}** |"]
    lines += [f"| digest 不匹配/未验证 | {progress['mismatch']} |"]
    lines += [f"| 唯一压缩层（300 实例合计） | {fmt_bytes(estimate['unique_compressed_bytes_total'])} |"]
    lines += [f"| 剩余唯一压缩层 | {fmt_bytes(remaining_unique)} |"]
    lines += [f"| 估算 Docker 总占用（唯一压缩 × {estimate['docker_estimate_factor']}） | {fmt_bytes(estimate['estimated_docker_total_bytes'], False)} |"]
    lines += [f"| 估算剩余增量 | {fmt_bytes(remaining_unique * estimate['docker_estimate_factor'], False)} |"]
    lines += [f"| C: 可用（开始 → 现在） | {fmt_bytes(disk['c_free_before_bytes'])} → {fmt_bytes(disk['c_free_after_bytes'])} |"]
    lines += [f"| Docker 总占用（开始 → 现在） | {fmt_bytes(disk['docker_total_before_bytes'], False)} → {fmt_bytes(disk['docker_total_bytes'], False)} |"]
    lines += [f"| 脚本 ETA（按本进程完成样本） | {eta.get('eta_hours', '待实测')} 小时（样本少，仅作参考） |"]
    lines += [f"| 实测吞吐（压缩字节/耗时，上界） | {eta.get('effective_compressed_MBps', '尚无')} MB/s |"]
    lines += ["", "## 分 repo 进度", "", "| repo | 总数 | pulled | skipped | failed | pending | 压缩层合计 | 唯一压缩层 |", "|---|---:|---:|---:|---:|---:|---:|---:|"]
    for repo, recs in by_repo.items():
        c = counts(recs)
        compressed = sum((r.get("compressed_bytes") or 0) for r in recs)
        lines += [f"| {repo} | {c['total']} | {c['pulled']} | {c['skipped']} | {c['failed']} | {c['pending']} | {fmt_bytes(compressed)} | {fmt_bytes(unique_layers(recs))} |"]

    lines += ["", "## 待拉清单", ""]
    pending = [r for r in instances if r["status"] in ("pending", "failed")]
    if not pending:
        lines += ["（无）"]
    else:
        for repo, recs in by_repo.items():
            rp = [r for r in recs if r["status"] in ("pending", "failed")]
            if not rp:
                continue
            lines += [f"### {repo}（{len(rp)}）", ""]
            lines += [f"- `{r['instance']}`" for r in rp]
            lines += [""]

    lines += ["## 磁盘水位与保护", ""]
    lines += [f"* 开始：C: 可用 {fmt_bytes(disk['c_free_before_bytes'])}；Docker 总占用 {fmt_bytes(disk['docker_total_before_bytes'], False)}（images {fmt_bytes(disk['docker_images_bytes'], False)}）"]
    lines += [f"* 现在：C: 可用 {fmt_bytes(disk['c_free_after_bytes'])}；Docker 总占用 {fmt_bytes(disk['docker_total_bytes'], False)}（images {fmt_bytes(disk['docker_images_bytes'], False)}）；docker_data.vhdx {fmt_bytes(disk['docker_vhdx_bytes'])}"]
    lines += [f"* 停止阈值：C: 可用 < {fmt_bytes(disk['min_free_bytes'])} 或 Docker 占用 > {fmt_bytes(disk['max_docker_bytes'], False)}"]
    lines += ["* 本 lane **未执行 prune，未删除任何镜像/容器/卷**（仅清理了本 lane 自己的临时 registry 测试容器）。"]
    lines += ["", "## 网络实测（本轮证据）", ""]
    lines += ["| 路径 | 实测 | 结论 |", "|---|---|---|"]
    lines += ["| 宿主机 curl → 1panel blob（Range 10 MB） | **1.58 MB/s** | 外网/镜像本身可用 |"]
    lines += ["| Docker daemon 拉 `library/registry`（10 MB） | 125 s ≈ **80 KB/s** | Docker Desktop 代理 `http.docker.internal:3128` 是瓶颈 |"]
    lines += ["| Docker daemon 拉 `alpine:3.20`（~3 MB，1ms 源） | 218 s ≈ **14 KB/s** | 1ms 更慢，仅作兜底 |"]
    lines += ["| django-10914（1.07 GiB 压缩） | 跨多次重试后成功，单次常见 30 s HEAD 超时 | 镜像层下载经常卡住，重试可续传 |"]
    lines += ["| 宿主机 HTTP → Docker save tar → `docker load`（alpine 测试） | 8.1 MB tar，`docker load` **0.67 s** 成功 | 证明可绕过 daemon 代理，但批量导出层需额外开发 |"]
    lines += ["", "结论：当前 Docker Desktop 的 registry 代理路径比宿主机直连慢 20–100 倍。按宿主机 1.58 MB/s 的理论下界，75.4 GiB 唯一压缩层约需 **13–14 小时**；按 daemon 实测 14–80 KB/s 则需 **11–60 天**。因此**全量 300 单批在当前 daemon 代理下不可行**，需要换拉取路径或分批。", ""]
    lines += ["## 分批建议", ""]
    lines += ["1. **优先按 repo/version 小批**：`django 3.0`（15 个，已 1 个）→ `django 3.1`（21）→ `django 3.2`（20）→ ...；每个 version 组先拉 1 个镜像验证 base 层，再补本组其余镜像（共享层，后续只下差异层）。"]
    lines += ["2. **matplotlib 单独排期**：其 23 个实例唯一压缩层 35.95 GiB（占全部 75.42 GiB 的 48%），是单 repo 最大成本，建议单独一夜批次。"]
    lines += ["3. **换传输路径**：在宿主机用 `crane`/`skopeo` 直连 1panel 拉 OCI（实测 1.58 MB/s），再 `docker load`；或修复 Docker Desktop 代理/改用专用 runner。否则 daemon 代理会把 75 GiB 放大成数周。"]
    lines += ["4. **磁盘**：当前 C: 可用约 620 GiB，估算全量 Docker 占用 324 GB（唯一压缩 ×4，来自 requests 实测比例），磁盘不会先触发；若走 tar + docker load，需额外预留约 1 个最大版本的临时空间（matplotlib 单版本约 10–15 GiB）。"]
    lines += ["5. **断点续拉**：脚本已支持 `manifest-cache.json` + `summary.json` + Docker 层缓存；中断后直接重跑同一命令即可，已验证。"]
    lines += ["", "## 复现命令", "", "```powershell", "powershell -NoProfile -ExecutionPolicy Bypass -File scripts\\docker\\pull_lite_all.ps1 `", "  -MaxParallel 2 -MaxRetries 8 -AttemptTimeoutMin 60 -RetryDelaySec 30", "```", ""]
    lines += ["## 数据文件", "", "| 文件 | 说明 |", "|---|---|"]
    lines += ["| `summary.json` / `summary.csv` | 全部 300 实例状态、digest、大小、耗时、磁盘水位 |"]
    lines += ["| `manifest-cache.json` | 300 实例的 index/amd64 digest 与层列表 |"]
    lines += ["| `lite-instances.json` | 从 parquet 提取的完整清单 |"]
    lines += ["| `pending.txt` / `pending.csv` | 剩余待拉清单 |"]
    lines += ["| `progress.log` | 每个 repo 完成时的进度行 |"]
    lines += ["| `pull-<instance>.log` | 逐实例/逐尝试日志 |"]
    lines += ["", "## 备注", "", "* 本轮只做拉取，不运行 SWE-bench 评测；评测由 foreman 使用 `scripts/swebench/run_eval.ps1` 驱动。", "* 镜像名：`swebench/sweb.eval.x86_64.<instance_id __→_1776_>:latest`；经 1panel/1ms 传输，Docker 按 digest 内容寻址校验每层。", ""]

    REPORT.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"wrote {REPORT}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
