#!/usr/bin/env python3
"""Scan SWE-bench Lite container-registry manifests in parallel.

Reads the instance inventory produced by lite_instances.py, fetches each
image's OCI index + linux/amd64 manifest through Docker Hub mirrors, and writes
a manifest-cache.json compatible with scripts/docker/pull_lite_all.ps1.

Why this exists: the PowerShell sequential scan took 20-90 s per image when a
mirror stalled (Docker CLI/buildx front-end retries), while plain HTTPS GETs
are 1-3 s and can run concurrently. This helper uses requests + threads.

Records are merged into an existing cache and only missing/incomplete records
are queried, so it is safe to resume. Exit code 0 even when individual images
fail; failures are recorded in the cache (mirrors_failed) and listed on stderr.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime
from pathlib import Path

import requests

INDEX_ACCEPT = (
    "application/vnd.oci.image.index.v1+json, "
    "application/vnd.docker.distribution.manifest.list.v2+json, "
    "application/vnd.oci.image.manifest.v1+json, "
    "application/vnd.docker.distribution.manifest.v2+json"
)
MANIFEST_ACCEPT = (
    "application/vnd.oci.image.manifest.v1+json, "
    "application/vnd.docker.distribution.manifest.v2+json"
)

_print_lock = threading.Lock()


def log(msg: str) ->None:
    with _print_lock:
        print(msg, flush=True)


def image_ref(instance_id: str) -> str:
    return "swebench/sweb.eval.x86_64." + instance_id.replace("__", "_1776_") + ":latest"


def parse_image(image: str) -> tuple[str, str]:
    m = re.match(r"^(?P<repo>[^:]+)(?::(?P<tag>[^:]+))?$", image)
    if not m:
        raise ValueError(f"cannot parse image: {image}")
    return m.group("repo"), m.group("tag") or "latest"


def session() -> requests.Session:
    s = requests.Session()
    s.trust_env = False
    s.headers.update({"User-Agent": "pir-lite-manifest-scan/1.0"})
    return s


def get_with_auth(s: requests.Session, url: str, accept: str, timeout: float) -> requests.Response:
    r = s.get(url, headers={"Accept": accept}, timeout=timeout)
    if r.status_code == 401:
        challenge = r.headers.get("WWW-Authenticate", "")
        realm = re.search(r'realm="([^"]+)"', challenge)
        service = re.search(r'service="([^"]+)"', challenge)
        scope = re.search(r'scope="([^"]+)"', challenge)
        if not realm:
            r.raise_for_status()
        params = {}
        if service:
            params["service"] = service.group(1)
        if scope:
            params["scope"] = scope.group(1)
        token_resp = s.get(realm.group(1), params=params, timeout=timeout)
        token_resp.raise_for_status()
        token = token_resp.json().get("token") or token_resp.json().get("access_token")
        if not token:
            raise RuntimeError("registry token response has no token")
        r = s.get(url, headers={"Accept": accept, "Authorization": f"Bearer {token}"}, timeout=timeout)
    r.raise_for_status()
    return r


def fetch_manifest(host: str, image: str, timeout: float) -> dict:
    repo, tag = parse_image(image)
    s = session()
    base = f"https://{host}/v2/{repo}/manifests"
    r = get_with_auth(s, f"{base}/{tag}", INDEX_ACCEPT, timeout)
    body = r.content
    index_digest = r.headers.get("Docker-Content-Digest") or "sha256:" + hashlib.sha256(body).hexdigest()
    idx = json.loads(body)
    amd_digest = None
    layers = []
    if isinstance(idx, dict) and idx.get("manifests"):
        amd = next(
            (m for m in idx["manifests"]
             if (m.get("platform") or {}).get("os") == "linux"
             and (m.get("platform") or {}).get("architecture") == "amd64"),
            None,
        )
        if not amd:
            raise RuntimeError("no linux/amd64 manifest in index")
        amd_digest = amd["digest"]
    else:
        amd_digest = index_digest
        layers = idx.get("layers") or []

    if not layers:
        mr = get_with_auth(s, f"{base}/{amd_digest}", MANIFEST_ACCEPT, timeout)
        mj = json.loads(mr.content)
        layers = mj.get("layers") or []

    layer_list = [
        {"digest": str(l["digest"]), "size": int(l["size"])}
        for l in layers
        if l.get("digest")
    ]
    return {
        "index_digest": index_digest,
        "amd64_digest": amd_digest,
        "compressed_bytes": sum(l["size"] for l in layer_list),
        "layer_count": len(layer_list),
        "layers": layer_list,
        "mirror": host,
    }


def scan_one(item: dict, mirrors: list[str], timeout: float, attempts: int) -> dict:
    image = image_ref(item["instance_id"])
    errors = []
    for host in mirrors:
        for attempt in range(1, attempts + 1):
            try:
                info = fetch_manifest(host, image, timeout)
                return {
                    "instance": item["instance_id"],
                    "repo": item["repo"],
                    "version": item["version"],
                    "image": image,
                    "index_digest": info["index_digest"],
                    "amd64_digest": info["amd64_digest"],
                    "compressed_bytes": info["compressed_bytes"],
                    "layer_count": info["layer_count"],
                    "layers": info["layers"],
                    "mirrors_ok": host,
                    "mirrors_failed": "",
                    "digest_consistent": True,
                    "fetched_at": datetime.now().astimezone().isoformat(),
                }
            except Exception as exc:  # noqa: BLE001 - record every failure
                errors.append(f"{host}#{attempt}: {exc}")
                if attempt < attempts:
                    time.sleep(2 * attempt)
    return {
        "instance": item["instance_id"],
        "repo": item["repo"],
        "version": item["version"],
        "image": image,
        "index_digest": None,
        "amd64_digest": None,
        "compressed_bytes": None,
        "layer_count": 0,
        "layers": [],
        "mirrors_ok": "",
        "mirrors_failed": " | ".join(errors)[:2000],
        "digest_consistent": False,
        "fetched_at": datetime.now().astimezone().isoformat(),
    }


def save_cache(path: Path, cache: dict, lock: threading.Lock) -> None:
    with lock:
        records = [cache[k] for k in sorted(cache, key=lambda k: cache[k].get("instance", k))]
        tmp = path.with_suffix(path.suffix + ".tmp")
        tmp.write_text(json.dumps(records, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        os.replace(tmp, path)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inventory", required=True, help="lite-instances.json")
    parser.add_argument("--out", required=True, help="manifest-cache.json")
    parser.add_argument("--mirrors", default="docker.1panel.live,docker.1ms.run")
    parser.add_argument("--workers", type=int, default=4)
    parser.add_argument("--timeout", type=float, default=45.0)
    parser.add_argument("--attempts", type=int, default=2)
    parser.add_argument("--force", action="store_true")
    args = parser.parse_args()

    inventory = json.loads(Path(args.inventory).read_text(encoding="utf-8"))
    out = Path(args.out)
    cache = {}
    if out.is_file() and not args.force:
        for rec in json.loads(out.read_text(encoding="utf-8")):
            if rec.get("instance"):
                cache[rec["instance"]] = rec

    def complete(rec: dict) -> bool:
        return bool(rec and rec.get("index_digest") and rec.get("layers"))

    missing = [it for it in inventory if not complete(cache.get(it["instance_id"]))]
    log(f"[scan] {len(inventory)} instances, {len(cache)} cached, {len(missing)} to query")
    if not missing:
        return 0

    mirrors = [m.strip() for m in args.mirrors.split(",") if m.strip()]
    lock = threading.Lock()
    done = 0
    ok = 0
    failed = 0
    started = time.time()
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        futures = {pool.submit(scan_one, it, mirrors, args.timeout, args.attempts): it for it in missing}
        for fut in as_completed(futures):
            item = futures[fut]
            rec = fut.result()
            with lock:
                cache[item["instance_id"]] = rec
                done += 1
                if rec["index_digest"]:
                    ok += 1
                else:
                    failed += 1
            elapsed = time.time() - started
            rate = done / elapsed * 60 if elapsed else 0
            if rec["index_digest"]:
                log(f"[scan] {done}/{len(missing)} OK {item['instance_id']} "
                    f"layers={rec['layer_count']} compressed={rec['compressed_bytes'] / 1e9:.2f}GB "
                    f"({rate:.1f}/min)")
            else:
                log(f"[scan] {done}/{len(missing)} FAILED {item['instance_id']}: {rec['mirrors_failed'][:200]}")
            if done % 10 == 0:
                save_cache(out, cache, lock)

    save_cache(out, cache, lock)
    elapsed = time.time() - started
    log(f"[scan] done in {elapsed:.1f}s: ok={ok} failed={failed}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
