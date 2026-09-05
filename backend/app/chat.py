"""问答任务管理：排队、并发控制、取消与断线释放（plan 第四节）。

- 初始并发 1、最多排队 10，超队拒绝；同一浏览器（client_id）只允许 1 个未完成请求。
- 断线即任务终止并释放服务端资源；取消同理。
- 浏览器随请求提交聊天历史，服务端按轮数与字符上限自最旧截断（参数压测前用保守值）。
- 服务端不持久化问答正文；任务对象只在处理期间存在。
"""

from __future__ import annotations

import asyncio
import uuid
from dataclasses import dataclass, field
from typing import Callable

from .config import config


class ChatRejected(Exception):
    pass


@dataclass
class ChatJob:
    request_id: str
    client_id: str
    runner: Callable  # async (job) -> None，结束前持续向 job.queue 放事件
    queue: asyncio.Queue = field(default_factory=asyncio.Queue)
    task: asyncio.Task | None = None


class ChatManager:
    """单个运行任务 + 等待队列。事件以 dict 放入 job.queue，"__end__" 表示流终止。"""

    def __init__(self):
        self._lock = asyncio.Lock()
        self._running: ChatJob | None = None
        self._waiting: list[ChatJob] = []
        self._by_client: dict[str, ChatJob] = {}

    async def submit(self, client_id: str, runner: Callable) -> ChatJob:
        async with self._lock:
            if client_id in self._by_client:
                raise ChatRejected("同一浏览器已有进行中的问答，请等待完成或先取消")
            total = (1 if self._running else 0) + len(self._waiting)
            if total >= config.chat_concurrency + config.chat_queue_max:
                raise ChatRejected(
                    f"当前排队已满（运行 {config.chat_concurrency} + 队列 {config.chat_queue_max}），请稍后再试"
                )
            job = ChatJob(request_id=uuid.uuid4().hex, client_id=client_id, runner=runner)
            self._by_client[client_id] = job
            if self._running is None:
                self._start(job)
            else:
                self._waiting.append(job)
                job.queue.put_nowait(
                    {"event": "queued", "position": len(self._waiting), "request_id": job.request_id}
                )
            return job

    def _start(self, job: ChatJob) -> None:
        self._running = job

        async def wrap() -> None:
            try:
                async with asyncio.timeout(config.chat_request_timeout_s):
                    await job.runner(job)
            except asyncio.CancelledError:
                job.queue.put_nowait({"event": "error", "message": "已取消"})
            except TimeoutError:
                job.queue.put_nowait({"event": "error", "message": "请求超时，任务已终止"})
            except Exception as exc:  # noqa: BLE001
                job.queue.put_nowait({"event": "error", "message": f"服务内部错误：{type(exc).__name__}"})
            finally:
                job.queue.put_nowait({"event": "__end__"})

        job.task = asyncio.create_task(wrap())
        job.queue.put_nowait({"event": "started", "request_id": job.request_id})
        job.task.add_done_callback(lambda _t: asyncio.get_running_loop().call_soon(self._advance))

    def _advance(self) -> None:
        if self._running:
            if self._running.task and not self._running.task.done():
                return
            self._by_client.pop(self._running.client_id, None)
            self._running = None
        while self._waiting:
            job = self._waiting.pop(0)
            if self._by_client.get(job.client_id) is not job:
                continue  # 已取消/断开
            job.queue.put_nowait({"event": "started", "request_id": job.request_id})
            self._start(job)
            break

    async def cancel(self, request_id: str, client_id: str) -> bool:
        async with self._lock:
            job = self._by_client.get(client_id)
            if not job or job.request_id != request_id:
                return False
            self._by_client.pop(client_id, None)
            if job is self._running:
                if job.task and not job.task.done():
                    job.task.cancel()
            else:
                if job in self._waiting:
                    self._waiting.remove(job)
                job.queue.put_nowait({"event": "error", "message": "已取消"})
                job.queue.put_nowait({"event": "__end__"})
            return True

    def detach(self, client_id: str) -> None:
        """SSE 断开时调用：终止任务并释放资源。"""
        job = self._by_client.get(client_id)
        if not job:
            return
        self._by_client.pop(client_id, None)
        if job is self._running:
            if job.task and not job.task.done():
                job.task.cancel()
        elif job in self._waiting:
            self._waiting.remove(job)

    async def cancel_all(self):
        async with self._lock:
            jobs = list(self._by_client.values())
            self._waiting.clear()
            self._by_client.clear()
            for job in jobs:
                if job.task and not job.task.done():
                    job.task.cancel()
                elif job.task is None:
                    job.queue.put_nowait({"event": "error", "message": "资料库恢复，问答已中断"})
                    job.queue.put_nowait({"event": "__end__"})
            tasks = [job.task for job in jobs if job.task]
        await asyncio.gather(*tasks, return_exceptions=True)
        self._running = None


def trim_history(history: list[dict]) -> list[dict]:
    """保留最近 N 轮且总字符不超上限，自最旧截断。"""
    max_rounds = config.chat_history_max_rounds
    max_chars = config.chat_history_max_chars

    def chars(msgs: list[dict]) -> int:
        return sum(len(p.get("text", "")) for m in msgs for p in m.get("parts", []))

    trimmed = history[-(max_rounds * 2) :]
    while trimmed and chars(trimmed) > max_chars:
        trimmed = trimmed[1:]
    return trimmed
