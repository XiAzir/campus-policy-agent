"""后端配置：一律从环境变量（仓库根 .env 或进程环境）读取，密钥不落库、不进日志。"""

from __future__ import annotations

import os
import secrets
from pathlib import Path

from dotenv import load_dotenv

REPO_ROOT = Path(__file__).resolve().parents[2]
load_dotenv(REPO_ROOT / ".env")


def _int_env(name: str, default: int) -> int:
    try:
        return int(os.environ.get(name, default))
    except ValueError:
        return default


class Config:
    # 主模型（Gemini 原生 /v1beta 反代）
    gemini_base_url: str = os.environ.get("GEMINI_BASE_URL", "").rstrip("/")
    gemini_api_key: str = os.environ.get("GEMINI_API_KEY", "")
    gemini_model: str = os.environ.get("GEMINI_MODEL", "")

    # Embedding（硅基流动，查询侧；资料包内已带向量）
    siliconflow_api_key: str = os.environ.get("SILICONFLOW_API_KEY", "")
    siliconflow_model: str = os.environ.get("SILICONFLOW_EMBED_MODEL", "")
    embed_dims: int = _int_env("EMBED_DIMS", 1024)
    preprocessing_version: str = os.environ.get("PREPROCESSING_VERSION", "v1")

    # 数据目录（资料、向量、SQLite）
    data_dir: Path = Path(os.environ.get("DATA_DIR", REPO_ROOT / "backend" / "data"))

    # 访问控制
    token_ttl_days: int = _int_env("TOKEN_TTL_DAYS", 30)
    initial_admin_password: str = os.environ.get("INITIAL_ADMIN_PASSWORD", "admin")

    # 问答并发与排队（plan 第四节：初始并发 1、排队 10、单浏览器 1 个未完成请求）
    chat_concurrency: int = _int_env("CHAT_CONCURRENCY", 1)
    chat_queue_max: int = _int_env("CHAT_QUEUE_MAX", 10)
    chat_history_max_rounds: int = _int_env("CHAT_HISTORY_MAX_ROUNDS", 12)
    chat_history_max_chars: int = _int_env("CHAT_HISTORY_MAX_CHARS", 8000)
    chat_request_timeout_s: int = _int_env("CHAT_REQUEST_TIMEOUT_S", 180)
    chat_max_tool_rounds: int = 3

    # 资料包与磁盘
    max_package_mb: int = _int_env("MAX_PACKAGE_MB", 200)
    disk_warn_gb: float = float(os.environ.get("DISK_WARN_GB", "8"))
    max_upload_body_mb: int = _int_env("MAX_UPLOAD_BODY_MB", 210)

    # 运行
    host: str = os.environ.get("HOST", "127.0.0.1")
    port: int = _int_env("PORT", 8000)


config = Config()


def ensure_secrets() -> None:
    """启动前自检：主模型配置缺失时明确报错，不静默退化为无依据聊天。"""
    missing = [n for n, v in (("GEMINI_BASE_URL", config.gemini_base_url), ("GEMINI_API_KEY", config.gemini_api_key), ("GEMINI_MODEL", config.gemini_model)) if not v]
    if missing:
        raise RuntimeError(f"缺少主模型配置：{', '.join(missing)}（应在 .env 中配置）")
    if not config.siliconflow_api_key:
        raise RuntimeError("缺少 SILICONFLOW_API_KEY（查询侧向量化需要）")


def new_secret() -> str:
    return secrets.token_hex(32)
