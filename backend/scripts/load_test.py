"""并发压测脚本（plan 第五节第 4 条）：记录排队时间、总耗时与引用数，采样服务器状态。

在部署后的服务器上运行：
  .venv/bin/python backend/scripts/load_test.py --port 8000 --code <访问码> --concurrency 10 --admin-token <管理令牌>

行为：N 个会话同时发起问答（线程并发），逐请求记录提交→启动（排队）与提交→完成；
并发 1 + 队列 10 时，第 12 个请求应被 429 拒绝。输出 CSV 与摘要。密钥不打印。
"""

from __future__ import annotations

import argparse
import csv
import json
import time
from concurrent.futures import ThreadPoolExecutor, as_completed

import httpx


def run_one(base: str, token: str, question: str, idx: int) -> dict:
    row = {"req": idx, "queued_s": "", "total_s": "", "rejected": False, "done": False, "citations": 0, "note": ""}
    t_submit = time.monotonic()
    try:
        with httpx.Client(timeout=600) as c:
            r = c.post(
                f"{base}/api/chat",
                headers={"Authorization": f"Bearer {token}"},
                json={"question": question, "messages": [], "scope": {"mode": "auto"}, "profile": {}},
            )
        if r.status_code == 429:
            row["rejected"] = True
            row["note"] = r.json().get("detail", "")[:80]
            return row
        if r.status_code != 200:
            row["note"] = f"HTTP {r.status_code}"
            return row
        events = []
        for line in r.text.splitlines():
            if line.startswith("data: "):
                try:
                    events.append(json.loads(line[6:]))
                except json.JSONDecodeError:
                    pass
        started = next((e for e in events if e["event"] == "started"), None)
        if started:
            row["queued_s"] = round(time.monotonic() - t_submit, 2)
        cit = next((e for e in events if e["event"] == "citations"), None)
        if cit:
            row["citations"] = len(cit["citations"])
        row["done"] = any(e["event"] == "done" for e in events)
        row["total_s"] = round(time.monotonic() - t_submit, 2)
    except Exception as exc:  # noqa: BLE001
        row["note"] = f"{type(exc).__name__}: {exc}"[:120]
    return row


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, default=8000)
    p.add_argument("--code", required=True, help="访问码")
    p.add_argument("--count", type=int, default=11, help="同时到达的请求数（并发1+队列10 时取 11）")
    p.add_argument("--question", default="旷课或迟到会受到什么处分？")
    p.add_argument("--admin-token", default="", help="可选：采样 /api/admin/status")
    args = p.parse_args()

    base = f"http://127.0.0.1:{args.port}"
    tokens = []
    with httpx.Client(timeout=60) as c:
        for _ in range(args.count):
            r = c.post(f"{base}/api/auth/login", json={"code": args.code})
            if r.status_code != 200:
                print("登录失败：", r.text[:200])
                return 1
            tokens.append(r.json()["token"])

    rows = []
    with ThreadPoolExecutor(max_workers=args.count) as pool:
        futures = [pool.submit(run_one, base, tok, args.question, i) for i, tok in enumerate(tokens)]
        for fut in as_completed(futures):
            row = fut.result()
            rows.append(row)
            tag = "429 拒绝" if row["rejected"] else f"排队 {row['queued_s']}s 总 {row['total_s']}s 引用 {row['citations']}"
            print(f"[{row['req']}] {tag} {row['note']}")

    rows.sort(key=lambda r: r["req"])
    rejected = sum(1 for r in rows if r["rejected"])
    done = sum(1 for r in rows if r["done"])
    totals = [r["total_s"] for r in rows if isinstance(r["total_s"], float)]
    print(
        f"\n摘要：{args.count} 并发 → 完成 {done}，拒绝 {rejected}，"
        f"总耗时 max={max(totals) if totals else 0:.1f}s min={min(totals) if totals else 0:.1f}s"
    )

    if args.admin_token:
        try:
            with httpx.Client(timeout=30) as c:
                st = c.get(
                    f"{base}/api/admin/status", headers={"Authorization": f"Bearer {args.admin_token}"}
                ).json()
            print("服务器状态：", json.dumps(st, ensure_ascii=False))
        except Exception as exc:  # noqa: BLE001
            print("状态采样失败：", exc)

    out = "load_test_result.csv"
    with open(out, "w", newline="", encoding="utf-8") as f:
        w = csv.DictWriter(f, fieldnames=["req", "queued_s", "total_s", "rejected", "done", "citations", "note"])
        w.writeheader()
        w.writerows(rows)
    print(f"明细已写入 {out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
