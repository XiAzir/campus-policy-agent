"""Live SSE load test. Records numeric telemetry only; never response text."""

from __future__ import annotations

import argparse
import csv
import getpass
import json
import os
import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

import httpx

FIELDS = ["req", "queued_s", "total_s", "rejected", "done", "citations", "note", "retrieval_s",
    "model_calls", "embedding_calls", "prompt_tokens", "output_tokens", "thought_tokens", "embedding_tokens",
    "model_usage_reported", "embedding_usage_reported"]


def prepare_tokens(client, base, code, count, sleep=time.sleep):
    tokens = []
    for _ in range(count):
        for attempt in range(6):
            response = client.post(f"{base}/api/auth/login", json={"code": code})
            if response.status_code == 200:
                tokens.append(response.json()["token"])
                break
            if response.status_code != 429:
                raise RuntimeError(f"准备会话失败 HTTP {response.status_code}")
            if attempt == 5:
                raise RuntimeError("准备会话持续被限流，测试尚未开始")
            sleep(31)
    return tokens


def run_one(base, token, question, idx, barrier=None, client_factory=httpx.Client):
    row = {key: "" for key in FIELDS}
    row.update(req=idx, rejected=False, done=False, citations=0, note="")
    with client_factory(timeout=2400) as client:
        if barrier:
            barrier.wait(timeout=60)
        started_at = time.monotonic()
        try:
            with client.stream("POST", f"{base}/api/chat", headers={"Authorization": f"Bearer {token}"},
                    json={"question": question, "messages": [], "scope": {"mode": "auto"}, "profile": {}}) as response:
                if response.status_code != 200:
                    row["rejected"] = response.status_code == 429
                    row["note"] = f"HTTP {response.status_code}"
                else:
                    for line in response.iter_lines():
                        if not line.startswith("data:"):
                            continue
                        event = json.loads(line[5:].strip())
                        kind = event.get("event")
                        if kind == "started" and row["queued_s"] == "":
                            row["queued_s"] = round(time.monotonic() - started_at, 4)
                        elif kind == "citations":
                            row["citations"] = len(event["citations"])
                        elif kind == "metrics":
                            for key in FIELDS[7:]:
                                value = event.get(key)
                                if isinstance(value, (int, float, bool)):
                                    row[key] = value
                        elif kind == "done":
                            row["done"] = True
                        elif kind in ("error", "expand_request"):
                            row["note"] = kind
        except (httpx.HTTPError, ValueError) as exc:
            row["note"] = type(exc).__name__
        row["total_s"] = round(time.monotonic() - started_at, 4)
    return row


def sample_resources(base, token, stop, rows, errors, interval=0.5):
    with httpx.Client(timeout=5) as client:
        while not stop.is_set():
            try:
                response = client.get(f"{base}/api/admin/metrics", headers={"Authorization": f"Bearer {token}"})
                response.raise_for_status()
                rows.append(response.json())
            except (httpx.HTTPError, ValueError) as exc:
                errors.append(type(exc).__name__)
            stop.wait(interval)


def write_csv(path, fields, rows):
    with Path(path).open("w", newline="", encoding="utf-8") as output:
        writer = csv.DictWriter(output, fieldnames=fields, extrasaction="ignore")
        writer.writeheader()
        writer.writerows(rows)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=8000)
    parser.add_argument("--code", default=os.environ.get("LOAD_TEST_ACCESS_CODE", ""))
    parser.add_argument("--admin-token", default=os.environ.get("LOAD_TEST_ADMIN_TOKEN", ""))
    parser.add_argument("--count", "--concurrency", type=int, default=11)
    parser.add_argument("--expected-rejections", type=int, default=0)
    parser.add_argument("--question", default="旷课或迟到会受到什么处分？")
    parser.add_argument("--output", default="load_test_result.csv")
    args = parser.parse_args()
    if not 1 <= args.count <= 80:
        parser.error("count 必须在 1..80")
    code = args.code or getpass.getpass("访问码：")
    admin_token = args.admin_token or getpass.getpass("管理员令牌（资源采样）：")
    if not admin_token:
        parser.error("容量验收必须提供管理员令牌进行资源采样")
    base = f"http://127.0.0.1:{args.port}"
    try:
        with httpx.Client(timeout=60) as client:
            if client.get(f"{base}/api/admin/metrics", headers={"Authorization": f"Bearer {admin_token}"}).status_code != 200:
                raise RuntimeError("资源采样鉴权失败，测试尚未开始")
            tokens = prepare_tokens(client, base, code, args.count)
    except (httpx.HTTPError, RuntimeError) as exc:
        print(type(exc).__name__, "会话准备失败，未执行压测")
        return 1
    rows, samples, errors = [], [], []
    stop, barrier = threading.Event(), threading.Barrier(args.count)
    sampler = threading.Thread(target=sample_resources, args=(base, admin_token, stop, samples, errors), daemon=True)
    sampler.start()
    try:
        with ThreadPoolExecutor(max_workers=args.count) as pool:
            futures = [pool.submit(run_one, base, token, args.question, i, barrier) for i, token in enumerate(tokens)]
            for future in as_completed(futures):
                row = future.result()
                rows.append(row)
                print(f"[{row['req']}] 排队={row['queued_s']}s 总耗时={row['total_s']}s 完成={row['done']} 拒绝={row['rejected']}")
    finally:
        stop.set()
        sampler.join(timeout=10)
    rows.sort(key=lambda row: row["req"])
    output = Path(args.output)
    write_csv(output, FIELDS, rows)
    write_csv(output.with_suffix(".resources.csv"), ["at", "rss_bytes", "cpu_s", "disk_free_bytes", "running", "waiting"], samples)
    rejected = sum(row["rejected"] for row in rows)
    completed = sum(row["done"] for row in rows)
    peak_mb = max((row["rss_bytes"] / 1024**2 for row in samples), default=0)
    usage_missing = any(not row["model_usage_reported"] or not row["embedding_usage_reported"] for row in rows if row["done"])
    summary = {"count": args.count, "completed": completed, "rejected": rejected, "peak_rss_mb": peak_mb,
        "samples": len(samples), "sampling_errors": len(errors), "usage_missing": usage_missing,
        "note": "token 数来自上游返回；缺失时不推算。费用须按实际账单核对。"}
    output.with_suffix(".summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(summary, ensure_ascii=False))
    return int(not samples or bool(errors) or usage_missing or peak_mb >= 700 or rejected != args.expected_rejections or completed + rejected != args.count)


if __name__ == "__main__":
    raise SystemExit(main())
