"""FastAPI 入口：初始化、路由挂载、前端静态文件、全局错误映射。"""

from __future__ import annotations

import logging
from contextlib import asynccontextmanager

from fastapi import FastAPI, HTTPException, Request
from fastapi.responses import JSONResponse
from fastapi.staticfiles import StaticFiles

from . import api
from .agent import Agent
from .chat import ChatManager
from .config import REPO_ROOT, config, ensure_secrets
from .db import Database
from .llm import LLMError
from .logging_safe import configure_logging
from .security import Tokens, init_admin
from .vectors import get_index

log = logging.getLogger("campus-policy")
configure_logging()


@asynccontextmanager
async def lifespan(app: FastAPI):
    ensure_secrets()
    db = Database(config.data_dir / "campus.db")
    init_admin(db)
    api.db = db
    api.tokens = Tokens(db)
    api.agent = Agent(db, get_index(config.data_dir / "vectors"))
    api.chats = ChatManager()
    log.info(
        "服务启动：模型=%s 数据目录=%s 并发=%d 队列=%d",
        config.gemini_model,
        config.data_dir,
        config.chat_concurrency,
        config.chat_queue_max,
    )
    yield
    await api.agent.close()
    db.close()


app = FastAPI(title="班级政策问答 Agent", lifespan=lifespan, docs_url=None, redoc_url=None)


@app.exception_handler(LLMError)
async def llm_error_handler(request: Request, exc: LLMError):
    return JSONResponse(status_code=502, content={"detail": f"模型服务异常：{exc}"})


@app.exception_handler(HTTPException)
async def http_error_handler(request: Request, exc: HTTPException):
    return JSONResponse(status_code=exc.status_code, content={"detail": str(exc.detail)})


app.include_router(api.router)

_dist = REPO_ROOT / "frontend" / "dist"
if _dist.exists():
    app.mount("/", StaticFiles(directory=_dist, html=True), name="frontend")
else:
    @app.get("/")
    async def root():
        return {"service": "campus-policy-agent", "frontend": "尚未构建（frontend/dist 不存在）"}


def main() -> None:
    import uvicorn

    uvicorn.run("app.main:app", host=config.host, port=config.port, workers=1)


if __name__ == "__main__":
    main()
