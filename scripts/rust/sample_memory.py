#!/usr/bin/env python3
"""独立系统内存采样器（spec 3.1/3.4 节）——Linux + cgroup v2 专用。

采样器自身绝不放入被测服务的 cgroup；以 >=10Hz 读取：
- 服务 cgroup：memory.current / memory.peak / memory.stat(anon,file,kernel,sock) /
  memory.events / memory.swap.current，按键解析（不固定字段顺序）。
- 服务进程：/proc/<pid>/status 的 VmRSS/VmHWM/RssAnon/Threads/FDSize。
- 宿主机：/proc/meminfo 的 MemAvailable 与 SwapFree。

用法：
  python3 sample_memory.py --pid <服务pid> [--cgroup /sys/fs/cgroup/xxx.slice] \
      --out result.jsonl [--interval 0.1] [--duration 600]
不带 --cgroup 时只采进程与宿主机指标。缺失的指标不写 0，直接省略字段。
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import time
from pathlib import Path


def read_int(path: Path) -> int | None:
    try:
        return int(path.read_text().strip())
    except (OSError, ValueError):
        return None


def cgroup_stats(cg: Path) -> dict:
    out: dict[str, int] = {}
    cur = read_int(cg / "memory.current")
    if cur is not None:
        out["memory_current"] = cur
    peak = read_int(cg / "memory.peak")
    if peak is not None:
        out["memory_peak"] = peak
    swap = read_int(cg / "memory.swap.current")
    if swap is not None:
        out["swap_current"] = swap
    stat = cg / "memory.stat"
    if stat.exists():
        keep = ("anon", "file", "kernel", "sock", "slab", "pgmajfault", "pgfault")
        for line in stat.read_text().splitlines():
            parts = line.split()
            if len(parts) == 2 and parts[0] in keep:
                out[parts[0]] = int(parts[1])
    events = cg / "memory.events"
    if events.exists():
        for line in events.read_text().splitlines():
            parts = line.split()
            if len(parts) == 2 and parts[0] in ("oom", "oom_kill", "oom_group_kill"):
                out[parts[0]] = int(parts[1])
    return out


def proc_stats(pid: int) -> dict:
    out: dict[str, int] = {}
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            key, _, rest = line.partition(":")
            if key in ("VmRSS", "VmHWM", "RssAnon", "RssFile", "Threads"):
                out[key] = int(rest.strip().split()[0]) * (1 if key == "Threads" else 1024)
        n_fd = sum(1 for _ in os.scandir(f"/proc/{pid}/fd"))
        out["fds"] = n_fd
    except (OSError, ValueError, IndexError):
        pass
    return out


def host_stats() -> dict:
    out: dict[str, int] = {}
    try:
        for line in Path("/proc/meminfo").read_text().splitlines():
            key, _, rest = line.partition(":")
            if key in ("MemAvailable", "SwapFree", "Cached"):
                out[key] = int(rest.strip().split()[0]) * 1024
    except (OSError, ValueError):
        pass
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pid", type=int, required=True)
    ap.add_argument("--cgroup", type=Path, default=None)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--interval", type=float, default=0.1)
    ap.add_argument("--duration", type=float, default=600.0)
    ap.add_argument("--label", default="")
    args = ap.parse_args()
    if not 0 < args.interval <= 0.1 or args.duration <= 0:
        ap.error("interval 必须大于 0 且不超过 0.1 秒；duration 必须为正数")

    stop = False

    def on_term(*_):
        nonlocal stop
        stop = True

    signal.signal(signal.SIGINT, on_term)
    signal.signal(signal.SIGTERM, on_term)

    args.out.parent.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    n = 0
    with args.out.open("a", encoding="utf-8") as sink:
        while not stop and time.monotonic() - started < args.duration:
            rec = {"t": round(time.monotonic() - started, 3), "label": args.label}
            rec.update(proc_stats(args.pid))
            if args.cgroup:
                rec.update(cgroup_stats(args.cgroup))
            rec.update(host_stats())
            sink.write(json.dumps(rec, ensure_ascii=False) + "\n")
            n += 1
            time.sleep(args.interval)
        # 结束时补一次 cgroup 峰值（防尾段漏采）
        if args.cgroup:
            tail = {"t": round(time.monotonic() - started, 3), "label": args.label + ":final",
                    **cgroup_stats(args.cgroup)}
            sink.write(json.dumps(tail, ensure_ascii=False) + "\n")
    print(f"samples={n} -> {args.out}", file=__import__("sys").stderr)


if __name__ == "__main__":
    main()
