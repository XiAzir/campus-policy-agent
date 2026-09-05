#!/usr/bin/env bash
# 在 Ubuntu 24.04 服务器上执行的一次性部署脚本（假设项目已上传到 /opt/campus-policy-agent）。
# 用法：sudo bash deploy/deploy.sh
set -euo pipefail

APP_DIR=/opt/campus-policy-agent
BACKEND=$APP_DIR/backend

echo "== 1. 系统依赖 =="
apt-get update -qq
apt-get install -y -qq python3-venv python3-pip nginx >/dev/null

echo "== 2. 虚拟环境与依赖（服务器不需要 pymupdf/python-docx，只装运行时依赖）=="
cd "$APP_DIR"
[ -d .venv ] || python3 -m venv .venv
.venv/bin/pip install -q --upgrade pip
.venv/bin/pip install -q fastapi 'uvicorn[standard]' httpx numpy jieba pydantic python-multipart langgraph langchain-core python-dotenv

echo "== 3. 环境文件（密钥）=="
if [ ! -f "$BACKEND/.env" ]; then
  cat > "$BACKEND/.env" <<'EOF'
# 在此填入密钥（与本地 deploy-config.md 一致）；本文件权限 600，不进 git、不进备份
GEMINI_BASE_URL=
GEMINI_API_KEY=
GEMINI_MODEL=
SILICONFLOW_API_KEY=
SILICONFLOW_EMBED_MODEL=
EMBED_DIMS=1024
EOF
  chmod 600 "$BACKEND/.env"
  echo "已生成 $BACKEND/.env 模板，请填写后重启服务"
fi

echo "== 4. 目录与权限 =="
mkdir -p "$BACKEND/data"
chown -R www-data:www-data "$APP_DIR"

echo "== 5. 前端静态文件 =="
# 前端在本地构建后随代码上传 frontend/dist；服务器不执行构建
[ -d "$APP_DIR/frontend/dist" ] || echo "警告：frontend/dist 不存在，请先本地 npm run build"

echo "== 6. systemd 服务 =="
cp "$APP_DIR/deploy/campus-policy-agent.service" /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now campus-policy-agent
systemctl --no-pager status campus-policy-agent | head -5 || true

echo "== 7. Nginx =="
echo "请将 deploy/nginx-site.conf.example 中的域名/证书替换为实际值后，放入 /etc/nginx/sites-available/ 并软链到 sites-enabled，然后：nginx -t && systemctl reload nginx"

echo "完成。健康检查：curl -s http://127.0.0.1:8000/api/auth/state"
