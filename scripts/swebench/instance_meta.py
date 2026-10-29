#!/usr/bin/env python3
"""Export one SWE-bench instance from a local parquet dataset to JSON.

Default source is SWE-bench Verified (500 instances,
logs/swebench/data/verified.parquet). Pass --dataset to use another parquet,
e.g. the SWE-bench Lite split logs/swebench/data/test.parquet.

The dataset stores FAIL_TO_PASS / PASS_TO_PASS as JSON-encoded strings; this
script decodes them into real lists.

Example:
    python scripts/swebench/instance_meta.py \
        --instance psf__requests-2317 \
        --out logs/swebench/meta/psf__requests-2317.json

Only pyarrow is required (the host Python 3.12 already has it). Importing the
`swebench` package itself is avoided on Windows because swebench 3.0.17 imports
the Unix-only `resource` module at import time.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

FIELDS = (
    "instance_id",
    "repo",
    "base_commit",
    "version",
    "difficulty",
    "problem_statement",
    "test_patch",
    "FAIL_TO_PASS",
    "PASS_TO_PASS",
    "patch",
)


def default_dataset() -> Path:
    # scripts/swebench/instance_meta.py -> <repo>/logs/swebench/data/verified.parquet
    return Path(__file__).resolve().parents[2] / "logs" / "swebench" / "data" / "verified.parquet"


def parse_test_list(value):
    if value is None:
        return []
    if isinstance(value, list):
        return value
    text = str(value).strip()
    if not text:
        return []
    parsed = json.loads(text)
    if not isinstance(parsed, list):
        raise ValueError(f"expected a JSON list, got {type(parsed).__name__}")
    return parsed


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--instance", required=True, help="SWE-bench instance_id, e.g. psf__requests-2317")
    parser.add_argument("--dataset", default=None, help="parquet file (default: logs/swebench/data/verified.parquet; pass test.parquet for Lite)")
    parser.add_argument("--out", default=None, help="output JSON file; omit to print to stdout")
    args = parser.parse_args()

    try:
        import pyarrow.parquet as pq
    except ImportError:
        print("error: pyarrow is required (pip install pyarrow)", file=sys.stderr)
        return 2

    dataset = Path(args.dataset) if args.dataset else default_dataset()
    if not dataset.is_file():
        print(f"error: dataset not found: {dataset}", file=sys.stderr)
        return 2

    table = pq.read_table(dataset)
    if "instance_id" not in table.column_names:
        print(f"error: {dataset} has no instance_id column", file=sys.stderr)
        return 2

    rows = [row for row in table.to_pylist() if row.get("instance_id") == args.instance]
    if not rows:
        print(
            f"error: instance {args.instance!r} not found in {dataset} "
            f"({table.num_rows} rows)",
            file=sys.stderr,
        )
        return 3

    row = rows[0]
    meta = {field: row.get(field) for field in FIELDS}
    meta["FAIL_TO_PASS"] = parse_test_list(row.get("FAIL_TO_PASS"))
    meta["PASS_TO_PASS"] = parse_test_list(row.get("PASS_TO_PASS"))

    payload = json.dumps(meta, ensure_ascii=False, indent=2) + "\n"
    if args.out:
        out_path = Path(args.out)
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(payload, encoding="utf-8", newline="\n")
        print(f"wrote {out_path} ({len(meta['FAIL_TO_PASS'])} F2P, {len(meta['PASS_TO_PASS'])} P2P)")
    else:
        sys.stdout.reconfigure(encoding="utf-8")
        sys.stdout.write(payload)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
