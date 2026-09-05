"""Gate HTTP access while replacing the database and its immutable files."""

import asyncio
from starlette.responses import JSONResponse


class Maintenance:
    def __init__(self):
        self.restoring = False
        self.failed = False
        self.active = 0

    async def drain(self):
        while self.active > 1:
            await asyncio.sleep(0.01)


maintenance = Maintenance()


class MaintenanceMiddleware:
    def __init__(self, app):
        self.app = app

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http" or not scope.get("path", "").startswith("/api/"):
            return await self.app(scope, receive, send)
        if maintenance.restoring:
            message = "资料恢复失败，服务已锁定，请联系管理员" if maintenance.failed else "资料库正在恢复，请稍后重试"
            response = JSONResponse({"detail": message}, status_code=503)
            return await response(scope, receive, send)
        maintenance.active += 1
        try:
            await self.app(scope, receive, send)
        finally:
            maintenance.active -= 1
