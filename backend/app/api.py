"""HTTP API：鉴权、资料目录、原文读取、聊天事件流、资料包管理、备份恢复。

日志纪律：不记录问答正文、工具结果与密钥；错误信息面向用户时不含内部路径。
"""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import tempfile
import uuid
from pathlib import Path

import anyio
import numpy as np
from fastapi import APIRouter, File, Header, HTTPException, Request, UploadFile
from fastapi.responses import FileResponse, StreamingResponse
from pydantic import BaseModel, Field
from starlette.background import BackgroundTask
from starlette.concurrency import run_in_threadpool

from . import backup as backup_mod
from . import ingest
from .agent import Agent, TurnScope, profile_block
from .chat import ChatManager, ChatRejected, trim_history
from .config import config
from .db import Database, utcnow
from .retrieval import allowed_doc_ids
from .maintenance import maintenance
from .storage import require_space
from .metrics import TurnMetrics, current_metrics
from .security import (
    RateLimiter,
    Tokens,
    client_ip,
    hash_password,
    init_admin,
    limiter,
    require_admin_token,
    require_user_token,
    verify_password,
)

router = APIRouter(prefix="/api")

# 由 main 注入
db: Database
tokens: Tokens
agent: Agent
chats: ChatManager
storage_busy = False


async def storage_operation(operation):
    global storage_busy
    if storage_busy:
        raise HTTPException(409, "已有资料处理任务，请稍后重试")
    storage_busy = True
    task = asyncio.create_task(operation())
    cancelled = False
    try:
        # A cancelled HTTP request must not release paths/locks still used by a worker.
        with anyio.CancelScope(shield=True):
            while True:
                try:
                    result = await asyncio.shield(task)
                    break
                except asyncio.CancelledError:
                    if task.done():
                        raise
                    cancelled = True
            if cancelled:
                raise asyncio.CancelledError
            return result
    finally:
        storage_busy = False


async def storage_call(fn, *args):
    return await storage_operation(lambda: run_in_threadpool(fn, *args))


# ---------- 鉴权 ----------

class LoginBody(BaseModel):
    code: str = Field(min_length=1, max_length=128)


class AdminLoginBody(BaseModel):
    password: str = Field(min_length=1, max_length=128)


class PasswordBody(BaseModel):
    old_password: str
    new_password: str = Field(min_length=8, max_length=128)


@router.post("/auth/login")
async def login(body: LoginBody, request: Request):
    limiter.hit(f"login:{client_ip(request)}", rate_per_min=2, burst=10)
    stored = db.setting_get("access_code_hash")
    if stored is None:
        raise HTTPException(400, "访问码尚未设置，请联系管理员在管理端设置")
    if not verify_password(body.code, stored):
        db.audit("system", "login_failed", f"ip={client_ip(request)}")
        raise HTTPException(401, "访问码不正确")
    client_id = uuid.uuid4().hex
    return {"token": tokens.issue("user", client_id), "client_id": client_id}


@router.post("/auth/admin/login")
async def admin_login(body: AdminLoginBody, request: Request):
    limiter.hit(f"adminlogin:{client_ip(request)}", rate_per_min=2, burst=10)
    stored = db.setting_get("admin_password_hash")
    if not verify_password(body.password, stored or ""):
        db.audit("system", "admin_login_failed", f"ip={client_ip(request)}")
        raise HTTPException(401, "管理员密码不正确")
    client_id = uuid.uuid4().hex
    return {"admin_token": tokens.issue("admin", client_id), "client_id": client_id}


@router.post("/auth/admin/password")
async def admin_reset_password(body: PasswordBody, authorization: str | None = Header(default=None)):
    require_admin_token(db, authorization)
    stored = db.setting_get("admin_password_hash")
    if not verify_password(body.old_password, stored or ""):
        raise HTTPException(401, "当前密码不正确")
    if body.old_password == body.new_password:
        raise HTTPException(400, "新密码不能与当前密码相同")
    db.setting_set("admin_password_hash", hash_password(body.new_password))
    db.audit("admin", "admin_password_reset")
    return {"ok": True}


@router.get("/auth/state")
async def auth_state():
    return {"access_code_set": db.setting_get("access_code_hash") is not None}


# ---------- 资料目录与原文 ----------

@router.get("/catalog")
async def catalog(authorization: str | None = Header(default=None)):
    require_user_token(db, authorization)
    rows = db.q("SELECT * FROM documents WHERE deactivated_kind='' ORDER BY title")
    return {"documents": [db.document_public(r) for r in rows]}


def _doc_or_404(doc_uid: str):
    row = db.one("SELECT * FROM documents WHERE doc_uid=?", (doc_uid,))
    if row is None:
        raise HTTPException(404, "资料不存在")
    return row


@router.get("/source/{doc_uid}/text")
async def source_text(doc_uid: str, frm: int = 1, to: int = 80, authorization: str | None = Header(default=None)):
    """标准化原文行（含历史版本的旧资料；手动停用资料不可检索但其引用仍可打开）。"""
    require_user_token(db, authorization)
    row = _doc_or_404(doc_uid)
    frm = max(1, frm)
    to = min(min(max(frm, to), frm + 200), row["line_count"])
    from .retrieval import read_lines

    lines = read_lines(db, row, frm, to)
    import json as _json

    from .retrieval import page_for_line

    page = page_for_line(_json.loads(row["page_map"]) if row["page_map"] else [], frm)
    section = db.one(
        "SELECT title FROM sections WHERE doc_id=? AND start_line<=? AND ?<=end_line",
        (row["id"], frm, frm),
    )
    return {
        "doc_uid": doc_uid,
        "title": row["title"],
        "doc_type": row["doc_type"],
        "line_start": frm,
        "line_end": to,
        "line_count": row["line_count"],
        "page": page,
        "section": section["title"] if section else None,
        "deactivated_kind": row["deactivated_kind"],
        "lines": [f"L{n}: {t}" for n, t in enumerate(lines, start=frm)],
    }


@router.get("/source/{doc_uid}/file")
async def source_file(doc_uid: str, authorization: str | None = Header(default=None)):
    """原文件下载：所有原文件接口都要求鉴权。"""
    require_user_token(db, authorization)
    row = _doc_or_404(doc_uid)
    path = config.data_dir / "files" / f"{row['doc_hash']}{Path(row['original_filename']).suffix.lower()}"
    if not path.exists():
        raise HTTPException(404, "原文件缺失")
    return FileResponse(
        path,
        filename=row["original_filename"],
        media_type="application/octet-stream",
        headers={"X-Content-Type-Options": "nosniff"},
    )


@router.get("/source/{doc_uid}/versions")
async def source_versions(doc_uid: str, authorization: str | None = Header(default=None)):
    require_user_token(db, authorization)
    _doc_or_404(doc_uid)
    from .ingest import version_history

    return {"versions": version_history(db, doc_uid)}


# ---------- 聊天（SSE 事件流） ----------

class ChatMessage(BaseModel):
    role: str = Field(pattern="^(user|model)$")
    text: str


class ScopeSpec(BaseModel):
    mode: str = Field(default="auto", pattern="^(auto|domains|files)$")
    domains: list[str] = []
    doc_uids: list[str] = []
    year_mode: str = Field(default="current", pattern="^(current|past)$")
    expand_confirmed: bool = False


class ChatBody(BaseModel):
    messages: list[ChatMessage] = []
    question: str = Field(min_length=1, max_length=4000)
    scope: ScopeSpec = ScopeSpec()
    profile: dict = {}


def _turn_scope(scope: ScopeSpec) -> tuple[TurnScope, str]:
    note = ""
    if scope.mode == "files" and not scope.expand_confirmed:
        if not scope.doc_uids:
            raise HTTPException(400, "请至少选择一份文件")
        allowed = allowed_doc_ids(db, doc_uids=scope.doc_uids, year_mode=scope.year_mode)
        if not allowed:
            raise HTTPException(400, "指定的资料不存在或已停用")
        note = f"用户明确指定了 {len(allowed)} 份文件，严格限定在此范围内"
        return TurnScope(strict_files=True, strict_doc_uids=scope.doc_uids, year_mode=scope.year_mode), note
    if scope.mode == "domains" and scope.domains:
        allowed = allowed_doc_ids(db, year_mode=scope.year_mode, domains=scope.domains)
        note = f"用户选择领域：{'、'.join(scope.domains)}"
        if scope.expand_confirmed:
            allowed |= allowed_doc_ids(db, year_mode=scope.year_mode)
            note += "（用户已确认可扩展到全部资料）"
        return TurnScope(domains=[] if scope.expand_confirmed else scope.domains, year_mode=scope.year_mode), note
    if scope.expand_confirmed:
        note = "用户已确认可扩展到全部资料"
    return TurnScope(year_mode=scope.year_mode), note


@router.post("/chat")
async def chat(body: ChatBody, request: Request, x_client_id: str | None = Header(default=None), authorization: str | None = Header(default=None)):
    client_id = require_user_token(db, authorization)
    limiter.hit(f"chat:{client_id}", rate_per_min=30, burst=20)
    turn_scope, note = _turn_scope(body.scope)
    history = trim_history(
        [{"role": m.role, "parts": [{"text": m.text}]} for m in body.messages]
    )
    profile = {
        "college": str(body.profile.get("college", ""))[:60],
        "entry_year": str(body.profile.get("entry_year", ""))[:20],
        "scope_note": note,
    }

    async def runner(job) -> None:
        metrics = TurnMetrics()
        metric_token = current_metrics.set(metrics)
        try:
            result = await agent.run(history, body.question, turn_scope, profile, emit=lambda ev: job.queue.put(ev))
        finally:
            await job.queue.put({"event": "metrics", **metrics.public()})
            current_metrics.reset(metric_token)
        if result["expand_request"]:
            return
        text = result["text"]
        await job.queue.put({"event": "citations", "citations": result["citations"]})
        await job.queue.put({"event": "done", "text": text, "interrupted": False})

    try:
        job = await chats.submit(client_id, runner)
    except ChatRejected as exc:
        raise HTTPException(429, str(exc)) from exc

    async def event_stream():
        disconnected = False
        try:
            while True:
                try:
                    ev = await asyncio.wait_for(job.queue.get(), timeout=15)
                except asyncio.TimeoutError:
                    if await request.is_disconnected():
                        disconnected = True
                        break
                    yield ": keepalive\n\n"
                    continue
                if ev.get("event") == "__end__":
                    break
                yield f"data: {json.dumps(ev, ensure_ascii=False)}\n\n"
        except asyncio.CancelledError:
            disconnected = True
            raise
        finally:
            if disconnected:
                chats.detach(client_id)

    return StreamingResponse(
        event_stream(),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache", "X-Accel-Buffering": "no"},
    )


@router.post("/chat/cancel")
async def chat_cancel(request: Request, authorization: str | None = Header(default=None)):
    client_id = require_user_token(db, authorization)
    body = await request.json()
    ok = await chats.cancel(str(body.get("request_id", "")), client_id)
    return {"ok": ok}


# ---------- 管理端：资料包 ----------

@router.get("/admin/packages")
async def admin_packages(authorization: str | None = Header(default=None)):
    require_admin_token(db, authorization)
    rows = db.q("SELECT id, sha256, original_filename, size, imported_at, status, doc_count, chunk_count, embed_model, embed_dim FROM packages ORDER BY id DESC")
    return {"packages": [dict(r) for r in rows]}


@router.get("/admin/packages/{package_id}")
async def admin_package_preview(package_id: int, authorization: str | None = Header(default=None)):
    require_admin_token(db, authorization)
    try:
        preview = await storage_call(ingest.preview_package, db, package_id)
    except (ValueError, ingest.PackageError) as exc:
        raise HTTPException(400, str(exc)) from exc
    if preview is None:
        raise HTTPException(404, "资料包不存在")
    return preview


@router.post("/admin/packages")
async def admin_upload(request: Request, file: UploadFile = File(...), authorization: str | None = Header(default=None)):
    require_admin_token(db, authorization)
    limiter.hit("pkgupload", rate_per_min=6, burst=5)
    max_bytes = config.max_package_mb * 1024 * 1024
    fd, name = tempfile.mkstemp(prefix="cpb-upload-", suffix=".zip", dir=config.data_dir.parent)
    os.close(fd)
    tmp_path = Path(name)
    try:
        with tmp_path.open("wb") as tmp:
            total = 0
            while chunk := await file.read(1 << 20):
                total += len(chunk)
                if total > max_bytes:
                    raise HTTPException(413, f"资料包超过单包上限 {config.max_package_mb}MB，请分包导入")
                try:
                    require_space(config.data_dir.parent, len(chunk))
                    tmp.write(chunk)
                except (ValueError, OSError) as exc:
                    raise HTTPException(507, "磁盘空间不足") from exc
        package_id = await storage_call(ingest.import_package, db, tmp_path, file.filename or "package.zip")
    except ingest.IngestError as exc:
        raise HTTPException(400, str(exc)) from exc
    finally:
        tmp_path.unlink(missing_ok=True)
    return {"id": package_id}


class MetaPatch(BaseModel):
    fields: dict


@router.patch("/admin/packages/{package_id}/documents/{doc_hash}")
async def admin_patch_meta(package_id: int, doc_hash: str, body: MetaPatch, authorization: str | None = Header(default=None)):
    require_admin_token(db, authorization)
    try:
        await storage_call(ingest.apply_override, db, package_id, doc_hash, body.fields)
    except ingest.IngestError as exc:
        raise HTTPException(400, str(exc)) from exc
    return {"ok": True}


class PublishBody(BaseModel):
    replacements: dict[str, str | None] = {}


@router.post("/admin/packages/{package_id}/publish")
async def admin_publish(package_id: int, body: PublishBody, authorization: str | None = Header(default=None)):
    require_admin_token(db, authorization)
    try:
        await storage_call(ingest.publish_package, db, package_id, body.replacements)
    except ingest.IngestError as exc:
        raise HTTPException(400, str(exc)) from exc
    return {"ok": True}


@router.delete("/admin/packages/{package_id}")
async def admin_discard(package_id: int, authorization: str | None = Header(default=None)):
    require_admin_token(db, authorization)
    try:
        await storage_call(ingest.discard_package, db, package_id)
    except ingest.IngestError as exc:
        raise HTTPException(400, str(exc)) from exc
    return {"ok": True}


# ---------- 管理端：资料与版本 ----------

@router.get("/admin/documents")
async def admin_documents(authorization: str | None = Header(default=None)):
    require_admin_token(db, authorization)
    rows = db.q("SELECT * FROM documents ORDER BY published_at DESC")
    return {"documents": [db.document_public(r) for r in rows]}


@router.get("/admin/documents/{doc_uid}/versions")
async def admin_versions(doc_uid: str, authorization: str | None = Header(default=None)):
    require_admin_token(db, authorization)
    _doc_or_404(doc_uid)
    return {"versions": ingest.version_history(db, doc_uid)}


@router.post("/admin/documents/{doc_uid}/deactivate")
async def admin_deactivate(doc_uid: str, authorization: str | None = Header(default=None)):
    require_admin_token(db, authorization)
    try:
        await storage_call(ingest.deactivate_document, db, doc_uid)
    except ingest.IngestError as exc:
        raise HTTPException(400, str(exc)) from exc
    return {"ok": True}


@router.post("/admin/documents/{doc_uid}/enable")
async def admin_enable(doc_uid: str, authorization: str | None = Header(default=None)):
    require_admin_token(db, authorization)
    try:
        await storage_call(ingest.enable_document, db, doc_uid)
    except ingest.IngestError as exc:
        raise HTTPException(400, str(exc)) from exc
    return {"ok": True}


@router.post("/admin/documents/{doc_uid}/unlink")
async def admin_unlink(doc_uid: str, authorization: str | None = Header(default=None)):
    require_admin_token(db, authorization)
    try:
        await storage_call(ingest.unlink_replacement, db, doc_uid)
    except ingest.IngestError as exc:
        raise HTTPException(400, str(exc)) from exc
    return {"ok": True}


# ---------- 管理端：设置与状态 ----------

class AccessCodeBody(BaseModel):
    code: str = Field(min_length=4, max_length=64)


@router.put("/admin/access-code")
async def admin_set_access_code(body: AccessCodeBody, authorization: str | None = Header(default=None)):
    require_admin_token(db, authorization)
    db.setting_set("access_code_hash", hash_password(body.code))
    db.audit("admin", "access_code_changed", "旧访问码立即失效；已登录用户不受影响")
    return {"ok": True}


@router.get("/admin/status")
async def admin_status(authorization: str | None = Header(default=None)):
    require_admin_token(db, authorization)
    usage = shutil.disk_usage(config.data_dir)
    return {
        "disk": {
            "total_gb": round(usage.total / 1e9, 2),
            "used_gb": round(usage.used / 1e9, 2),
            "free_gb": round(usage.free / 1e9, 2),
            "warn_gb": config.disk_warn_gb,
        },
        "counts": {
            "documents": db.one("SELECT COUNT(*) c FROM documents")["c"],
            "current": db.one("SELECT COUNT(*) c FROM documents WHERE deactivated_kind=''")["c"],
            "chunks": db.one("SELECT COUNT(*) c FROM chunks")["c"],
            "packages": db.one("SELECT COUNT(*) c FROM packages")["c"],
        },
        "data_dir_mb": round(sum(f.stat().st_size for f in config.data_dir.rglob("*") if f.is_file()) / 1e6, 1),
    }


@router.get("/admin/metrics")
async def admin_metrics(authorization: str | None = Header(default=None)):
    require_admin_token(db, authorization)
    import psutil
    process = psutil.Process()
    memory = process.memory_info()
    cpu = process.cpu_times()
    disk = shutil.disk_usage(config.data_dir)
    return {"at": utcnow(), "rss_bytes": memory.rss, "cpu_s": cpu.user + cpu.system,
        "disk_free_bytes": disk.free, "running": int(chats._running is not None), "waiting": len(chats._waiting)}


# ---------- 备份与恢复 ----------

@router.get("/admin/backup")
async def admin_backup(authorization: str | None = Header(default=None)):
    require_admin_token(db, authorization)
    fd, name = tempfile.mkstemp(prefix="cpb-dl-", suffix=".zip", dir=config.data_dir.parent)
    os.close(fd)
    out = Path(name)
    try:
        await storage_call(backup_mod.create_backup, db, out)
    except BaseException as exc:
        out.unlink(missing_ok=True)
        if isinstance(exc, (ValueError, OSError)):
            raise HTTPException(400, "备份失败：资料不完整或磁盘空间不足") from exc
        raise
    db.audit("admin", "backup_download")

    def _unlink_retry():
        import time

        for _ in range(5):
            try:
                out.unlink()
                return
            except PermissionError:
                time.sleep(0.3)

    return FileResponse(
        out,
        filename=f"backup-{utcnow().replace(':', '')}.zip",
        media_type="application/zip",
        background=BackgroundTask(_unlink_retry),
    )


@router.post("/admin/restore")
async def admin_restore(file: UploadFile = File(...), authorization: str | None = Header(default=None)):
    require_admin_token(db, authorization)
    return await storage_operation(lambda: _restore_upload(file))


async def _restore_upload(file):
    global tokens
    fd, name = tempfile.mkstemp(prefix="cpb-restore-upload-", suffix=".zip", dir=config.data_dir.parent)
    os.close(fd)
    uploaded = Path(name)
    context = None
    entered = False
    owns_maintenance = False
    try:
        with uploaded.open("wb") as target:
            total = 0
            while chunk := await file.read(1024 * 1024):
                total += len(chunk)
                if total > backup_mod.MAX_EXPANDED:
                    raise ValueError("备份上传大小超限")
                require_space(config.data_dir.parent, len(chunk))
                target.write(chunk)
        context = backup_mod.validated_backup(uploaded)
        root, meta = await run_in_threadpool(context.__enter__)
        entered = True
        if maintenance.restoring:
            raise HTTPException(409, "已有恢复任务")
        maintenance.restoring = True
        owns_maintenance = True
        await chats.cancel_all()
        async with asyncio.timeout(30):
            await maintenance.drain()
        summary = await run_in_threadpool(backup_mod.install_backup, db, root, meta, agent.vectors.close_all)
        tokens = Tokens(db)
        db.audit("admin", "backup_restore", f"docs={summary.get('documents')}")
    except ValueError as exc:
        raise HTTPException(400, str(exc)) from exc
    except backup_mod.RestoreRollbackError as exc:
        maintenance.failed = True
        raise HTTPException(503, "恢复回滚失败，服务已锁定；请停止服务后按恢复故障步骤处理") from exc
    except TimeoutError as exc:
        raise HTTPException(409, "现有请求未及时结束，恢复未执行，请稍后重试") from exc
    except OSError as exc:
        raise HTTPException(400, "恢复失败，原资料库已保留；请检查磁盘空间和文件权限") from exc
    finally:
        if owns_maintenance and not maintenance.failed:
            maintenance.restoring = False
        if entered:
            await run_in_threadpool(context.__exit__, None, None, None)
        uploaded.unlink(missing_ok=True)
    return {"ok": True, "summary": summary}
