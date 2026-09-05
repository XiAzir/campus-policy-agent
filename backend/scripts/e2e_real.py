"""真实 Gemini 端到端验证：服务器 + 真实资料包 + 真实模型 + 真实 embedding。

前置：已运行 skill/scripts/build_package.py 生成 测试资料包-2026-09.zip。
本脚本临时启用服务（uvicorn 子进程），走完整 HTTP 链路：
管理员登录 → 上传发布 → 设置访问码 → 用户登录 → 真实问答（SSE）→ 引用校验。
密钥不打印。
"""

from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import httpx

REPO = Path(__file__).resolve().parents[2]
PORT = 8901
BASE = f"http://127.0.0.1:{PORT}"
PKG = REPO / "测试资料包-2026-09.zip"

ok_all = True


def check(name: str, ok: bool, detail: str = "") -> None:
    global ok_all
    ok_all = ok_all and ok
    print(f"[{'PASS' if ok else 'FAIL'}] {name} {detail}")


def sse_events(resp: httpx.Response) -> list[dict]:
    events = []
    for line in resp.text.splitlines():
        if line.startswith("data: "):
            try:
                events.append(json.loads(line[6:]))
            except json.JSONDecodeError:
                pass
    return events


def main() -> int:
    with socket.socket() as probe:
        try:
            probe.bind(("127.0.0.1", PORT))
        except OSError:
            print("测试端口已占用，未启动测试或修改已有服务")
            return 1
    with tempfile.TemporaryDirectory(prefix="cpb-real-e2e-") as data_dir:
        return run_isolated(data_dir)


def run_isolated(data_dir) -> int:
    env = {**os.environ, "DATA_DIR": data_dir, "INITIAL_ADMIN_PASSWORD": "admin"}
    proc = subprocess.Popen(
        [
            sys.executable,
            "-m", "uvicorn", "app.main:app",
            "--app-dir", str(REPO / "backend"),
            "--host", "127.0.0.1",
            "--port", str(PORT),
            "--log-level", "warning",
        ],
        cwd=str(REPO / "backend"),
        env=env,
        creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        time.sleep(5)
        if proc.poll() is not None:
            print("测试子进程启动失败，未连接或修改已有服务")
            return 1
        with httpx.Client(timeout=300) as c:
            r = c.get(f"{BASE}/api/auth/state")
            check("服务可达", r.status_code == 200)

            r = c.post(f"{BASE}/api/auth/admin/login", json={"password": "admin"})
            check("管理员登录（初始密码 admin）", r.status_code == 200)
            ah = {"Authorization": f"Bearer {r.json()['admin_token']}"}

            # 若之前已发布过则跳过重复导入
            pkgs = c.get(f"{BASE}/api/admin/packages", headers=ah).json()["packages"]
            if not any(p["status"] == "published" for p in pkgs):
                with open(PKG, "rb") as f:
                    r = c.post(
                        f"{BASE}/api/admin/packages",
                        headers=ah,
                        files={"file": ("测试资料包-2026-09.zip", f, "application/zip")},
                    )
                check("资料包上传", r.status_code == 200, r.text[:120] if r.status_code != 200 else "")
                pid = r.json()["id"]
                r = c.post(f"{BASE}/api/admin/packages/{pid}/publish", headers=ah, json={"replacements": {}})
                check("资料包发布", r.status_code == 200, r.text[:120] if r.status_code != 200 else "")

            r = c.put(f"{BASE}/api/admin/access-code", headers=ah, json={"code": "test2026"})
            check("设置访问码", r.status_code == 200)

            r = c.post(f"{BASE}/api/auth/login", json={"code": "test2026"})
            check("访问码登录", r.status_code == 200)
            uh = {"Authorization": f"Bearer {r.json()['token']}"}

            cat = c.get(f"{BASE}/api/catalog", headers=uh).json()["documents"]
            check("资料目录", len(cat) == 5, f"{len(cat)} 份现行资料")
            by_uid = {d["doc_uid"]: d for d in cat}

            # 真实问答（真实 Gemini 工具调用 + 真实查询向量）
            body = {
                "question": "参加暑期三下乡社会实践，报名截止时间是什么时候？需要提交哪些材料？",
                "messages": [],
                "scope": {"mode": "auto"},
                "profile": {"college": "信息工程学院", "entry_year": "2024"},
            }
            with c.stream("POST", f"{BASE}/api/chat", headers=uh, json=body) as r:
                check("SSE 聊天连接", r.status_code == 200)
                r.read()
                events = sse_events(r)
            kinds = [e["event"] for e in events]
            check("事件流包含检索与生成", "retrieving" in kinds and "generating" in kinds, " → ".join(kinds))
            done = next((e for e in events if e["event"] == "done"), None)
            check("正常完成", done is not None)
            cit = next((e for e in events if e["event"] == "citations"), None)
            if done and cit and cit["citations"]:
                titles = {by_uid[c["doc_uid"]]["title"][:18] for c in cit["citations"] if c["doc_uid"] in by_uid}
                check("引用指向已发布资料", len(titles) > 0, f"引用 {len(cit['citations'])} 条 → {titles}")
                # 引用原文行必须真实存在于标准化原文
                quote_ok = True
                for cc in cit["citations"][:3]:
                    t = c.get(f"{BASE}/api/source/{cc['doc_uid']}/text", params={"frm": cc["line_start"], "to": cc["line_end"]}, headers=uh).json()
                    src_lines = [l.split(": ", 1)[1] for l in t["lines"]]
                    if src_lines and cc["quote"] and src_lines[0].strip() != cc["quote"][0].strip():
                        quote_ok = False
                check("引用行与原文一致", quote_ok)
            else:
                check("引用存在", False, f"kinds={kinds[-3:]}")

            # 往年/扩展与取消等场景由自动化测试覆盖；此处只验证真实链路
            r = c.get(f"{BASE}/api/admin/status", headers=ah).json()
            check(
                "磁盘状态接口",
                "disk" in r and r["counts"]["documents"] == 5,
                f"free={r['disk']['free_gb']}GB",
            )
    finally:
        if proc.poll() is None:
            proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
    print("\n结果：" + ("全部通过" if ok_all else "存在失败项"))
    return 0 if ok_all else 1


if __name__ == "__main__":
    sys.exit(main())
