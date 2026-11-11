# Generates the 60-file English corpus for the trigram evaluation (cycle77).
# Usage: python gen_corpus.py <out-dir>
# Realistic English technical docs: unique sentences per section, real infra
# vocabulary, interpolated CJK marker sentences (the substring query class).

import os
import random
import sys

random.seed(20261111)

TOPICS = [
    "cache", "deploy", "monitor", "partition", "scheduler", "storage",
    "network", "auth", "queue", "database", "gateway", "indexer",
]

TITLE_TEMPLATE = "# The {topic} subsystem\n\n"

EN_SENTENCES = [
    "The {topic} layer keeps a bounded window of recent entries and evicts cold data under memory pressure.",
    "Every mutation to the {topic} state machine is journaled before it becomes visible to readers.",
    "A connection pool with warm standby sockets keeps tail latency flat during traffic spikes.",
    "The deployment pipeline promotes a build through staging gates before it reaches production.",
    "Alert aggregation collapses flapping notifications into one incident with a stable severity.",
    "The rate limiter admits requests in fixed windows and sheds excess load with a retry hint.",
    "Background compaction rewrites small segments into larger ones during off-peak hours.",
    "The scheduler picks pending work by deadline proximity rather than submission order.",
    "Garbage collection runs concurrently with writes so pauses stay under the ten millisecond budget.",
    "Health probes distinguish a slow dependency from a hard outage and degrade gracefully.",
    "The configuration service versions every change and supports atomic rollback to the previous revision.",
    "Metrics are sampled at ten second intervals and aggregated into per region rollups.",
    "The authentication flow rotates refresh tokens on every use and records an audit event.",
    "Sharded tables use a consistent hash ring so resharding moves a bounded number of keys.",
    "The message queue keeps at least once delivery and de-duplicates by consumer checkpoint.",
    "Cold storage tiering moves rarely accessed objects after thirty days of inactivity.",
    "The gossip protocol spreads membership updates within one round trip per hop.",
    "A circuit breaker opens after five consecutive failures and half opens after a cooldown.",
    "The replication stream ships a compressed oplog with a five second target lag.",
    "Query planning estimates selectivity from histograms maintained by the vacuum worker.",
    "The import worker parses CSV batches in streaming mode without loading whole files.",
    "Load balancer pools are drained slowly so long lived connections finish their requests.",
    "The secret store envelopes each key with a data key that rotates every ninety days.",
    "Structured logs carry a trace identifier that stitches spans across service hops.",
    "The quota service refills token buckets lazily on first use after an idle period.",
    "Schema migrations run in expand and contract phases to keep both versions alive.",
    "The webhook dispatcher signs payloads with a timestamp to prevent replay attacks.",
    "An idempotency key on every write makes client retries safe by default.",
    "The search indexer maintains a trigram table for substring matching over titles.",
    "Blue green releases shift traffic in ten percent steps with automatic rollback on error budget burn.",
]

CJK_MARKERS = [
    "缓存数据库连接池在高峰期保持尾部延迟平稳。",
    "自动化部署流水线分阶段发布构建。",
    "监控告警聚合把抖动通知合并为一个事件。",
    "水平分区路由策略迁移有界数量的键。",
    "批处理任务调度器按截止时间临近度挑选工作。",
]

def make_doc(name: str, idx: int) -> str:
    topic = TOPICS[idx % len(TOPICS)]
    parts = [TITLE_TEMPLATE.format(topic=topic)]
    for section in range(6):
        heading = f"## {topic.capitalize()} section {section}\n\n"
        sentences = random.sample(EN_SENTENCES, 4)
        body = " ".join(s.format(topic=topic) for s in sentences)
        marker = CJK_MARKERS[(idx + section) % len(CJK_MARKERS)]
        parts.append(heading + body + " " + marker + "\n\n")
    return "".join(parts)

def main() -> None:
    out = sys.argv[1] if len(sys.argv) > 1 else "corpus"
    count = int(sys.argv[2]) if len(sys.argv) > 2 else 60
    os.makedirs(out, exist_ok=True)
    for idx in range(count):
        name = f"doc-{idx:03d}-{TOPICS[idx % len(TOPICS)]}.md"
        with open(os.path.join(out, name), "w", encoding="utf-8") as handle:
            handle.write(make_doc(name, idx))
    print(f"wrote {count} files to {out}")

if __name__ == "__main__":
    main()
