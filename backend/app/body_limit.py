"""Enforce actual body size before multipart parsing, including chunked uploads."""

from starlette.responses import JSONResponse
from .config import config


class BodyTooLarge(Exception):
    pass


class BodyLimitMiddleware:
    def __init__(self, app):
        self.app = app

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http":
            return await self.app(scope, receive, send)
        path = scope.get("path", "")
        limit = (config.max_upload_body_mb * 1024**2 if path == "/api/admin/packages" else
                 10 * 1024**3 if path == "/api/admin/restore" else 256 * 1024)
        headers = dict(scope.get("headers", []))
        try:
            length = int(headers.get(b"content-length", b"0"))
        except ValueError:
            length = limit + 1
        response = JSONResponse({"detail": "请求体超过上限"}, status_code=413)
        if length > limit:
            return await response(scope, receive, send)
        total = 0
        started = False

        async def bounded_receive():
            nonlocal total
            message = await receive()
            total += len(message.get("body", b""))
            if total > limit:
                raise BodyTooLarge()
            return message

        async def guarded_send(message):
            nonlocal started
            if total > limit:
                return
            if message["type"] == "http.response.start":
                started = True
            await send(message)

        try:
            await self.app(scope, bounded_receive, guarded_send)
        except BodyTooLarge:
            pass
        if total > limit and not started:
            await response(scope, receive, send)
