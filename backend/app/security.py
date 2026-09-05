"""鉴权与限流：访问码/管理员令牌（HMAC 签名）、口令哈希、内存令牌桶。

访问码更换后旧码立即失效（仅保存当前码哈希）；已签发令牌不与访问码绑定，
换码不影响已登录用户。管理员初始密码 admin，可重置。
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import secrets
import time

from fastapi import Header, HTTPException, Request

from .config import config
from .db import Database, utcnow


def hash_password(password: str, salt: bytes | None = None) -> str:
    salt = salt or secrets.token_bytes(16)
    digest = hashlib.pbkdf2_hmac("sha256", password.encode(), salt, 200_000)
    return f"pbkdf2${salt.hex()}${digest.hex()}"


def verify_password(password: str, stored: str) -> bool:
    try:
        _, salt_hex, digest_hex = stored.split("$")
        digest = hashlib.pbkdf2_hmac("sha256", password.encode(), bytes.fromhex(salt_hex), 200_000)
        return hmac.compare_digest(digest.hex(), digest_hex)
    except (ValueError, AttributeError):
        return False


class Tokens:
    """HMAC 签名令牌：payload.role / exp / cid，服务端密钥存 settings。"""

    def __init__(self, db: Database):
        self.db = db
        secret = db.setting_get("token_secret")
        if secret is None:
            from .config import new_secret

            secret = new_secret()
            db.setting_set("token_secret", secret)
        self._key = bytes.fromhex(secret)

    def issue(self, role: str, client_id: str) -> str:
        payload = {"r": role, "c": client_id, "e": int(time.time()) + config.token_ttl_days * 86400}
        raw = base64.urlsafe_b64encode(json.dumps(payload).encode()).decode().rstrip("=")
        sig = hmac.new(self._key, raw.encode(), hashlib.sha256).hexdigest()[:32]
        return f"{raw}.{sig}"

    def verify(self, token: str, role: str) -> str | None:
        """返回 client_id；不合法或过期返回 None。"""
        try:
            raw, sig = token.rsplit(".", 1)
            expect = hmac.new(self._key, raw.encode(), hashlib.sha256).hexdigest()[:32]
            if not hmac.compare_digest(sig, expect):
                return None
            pad = "=" * (-len(raw) % 4)
            payload = json.loads(base64.urlsafe_b64decode(raw + pad))
            if payload.get("r") != role or payload.get("e", 0) < time.time():
                return None
            return str(payload.get("c", ""))
        except Exception:  # noqa: BLE001
            return None


class RateLimiter:
    """内存令牌桶：key → (tokens, last)。拒绝时抛 429。"""

    def __init__(self):
        self._buckets: dict[str, tuple[float, float]] = {}

    def hit(self, key: str, rate_per_min: float, burst: int, cost: float = 1.0) -> None:
        now = time.monotonic()
        tokens, last = self._buckets.get(key, (float(burst), now))
        tokens = min(float(burst), tokens + (now - last) * rate_per_min / 60)
        if tokens < cost:
            self._buckets[key] = (tokens, now)
            raise HTTPException(status_code=429, detail="请求过于频繁，请稍后再试")
        self._buckets[key] = (tokens - cost, now)

    def cleanup(self, max_keys: int = 10000) -> None:
        if len(self._buckets) > max_keys:
            self._buckets.clear()


limiter = RateLimiter()


def client_ip(request: Request) -> str:
    # Uvicorn applies proxy headers only for explicitly trusted peers.
    return request.client.host if request.client else "?"


def _client_id(x_client_id: str | None) -> str:
    return (x_client_id or "")[:64] or "anon"


def require_user_token(db: Database, authorization: str | None) -> str:
    if not authorization or not authorization.startswith("Bearer "):
        raise HTTPException(status_code=401, detail="未登录")
    cid = Tokens(db).verify(authorization[7:], "user")
    if cid is None:
        raise HTTPException(status_code=401, detail="登录已失效")
    return cid


def require_admin_token(db: Database, authorization: str | None) -> str:
    if not authorization or not authorization.startswith("Bearer "):
        raise HTTPException(status_code=401, detail="未以管理员身份登录")
    cid = Tokens(db).verify(authorization[7:], "admin")
    if cid is None:
        raise HTTPException(status_code=401, detail="管理员登录已失效")
    return cid


async def auth_header(authorization: str | None = Header(default=None)) -> str | None:
    return authorization


def init_admin(db: Database) -> None:
    if db.setting_get("admin_password_hash") is None:
        db.setting_set("admin_password_hash", hash_password(config.initial_admin_password))
        db.audit("system", "init_admin_password", "初始管理员密码已写入（admin）")


def init_access_code_state(db: Database) -> bool:
    return db.setting_get("access_code_hash") is not None


def now() -> str:
    return utcnow()
