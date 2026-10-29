#!/usr/bin/env python3
"""Extract the SWE-bench Lite instance inventory from a local parquet file.

Outputs a JSON array of {instance_id, repo, version} preserving dataset order.
Only pyarrow is required. Used by scripts/docker/pull_lite_all.ps1 so the
PowerShell puller never needs a hand-maintained instance list.

Example:
    python scripts/docker/lite_instances.py \
        --dataset logs/swebench/data/test.parquet \
        --out logs/lane-docker-lite/lite-instances.json
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

FIELDS = ("instance_id", "repo", "version")


def main() -> int:
    here = Path(__file__).resolve()
    default_dataset = here.parents[2] / "logs" / "swebench" / "data" / "test.parquet"

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dataset", default=str(default_dataset), help="parquet dataset path")
    parser.add_argument("--out", default=None, help="output JSON path; omit to print to stdout")
    parser.add_argument("--repo", action="append", default=[], help="only this repo (repeatable)")
    parser.add_argument("--instance", action="append", default=[], help="only this instance_id (repeatable)")
    args = parser.parse_args()

    dataset = Path(args.dataset)
    if not dataset.is_file():
        print(f"error: dataset not found: {dataset}", file=sys.stderr)
        return 2

    try:
        import pyarrow.parquet as pq
    except ImportError:
        print("error: pyarrow is required (pip install pyarrow)", file=sys.stderr)
        return 2

    table = pq.read_table(dataset, columns=list(FIELDS))
    rows = []
    for row in table.to_pylist():
        if args.repo and row["repo"] not in args.repo:
            continue
        if args.instance and row["instance_id"] not in args.instance:
            continue
        rows.append({field: row.get(field) for field in FIELDS})

    payload = json.dumps(rows, ensure_ascii=False, indent=2) + "\n"
    if args.out:
        out = Path(args.out)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(payload, encoding="utf-8", newline="\n")
        print(f"wrote {out} ({len(rows)} instances)", file=sys.stderr)
    else:
        sys.stdout.reconfigure(encoding="utf-8")
        sys.stdout.write(payload)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
