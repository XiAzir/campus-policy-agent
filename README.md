# 班级政策问答 Agent

为 30～80 位同学提供手机、电脑均可使用的政策问答网站：领域筛选、跨领域查询、原文行号溯源、版本管理（现行/往年/手动停用）。管理员负责资料发布，学生不上传文件。实施依据见 [plan.md](plan.md)。

## 结构

| 目录 | 内容 |
| --- | --- |
| `backend/` | FastAPI + LangGraph 后端：资料包校验/导入/发布/版本停用（`app/pkgfmt.py`、`app/ingest.py`）、jieba+FTS5 与 NumPy 向量混合检索（`app/retrieval.py`、`app/vectors.py`）、问答 Agent 与 SSE 聊天管理（`app/agent.py`、`app/chat.py`）、备份恢复（`app/backup.py`）、API（`app/api.py`）；测试 `tests/`，脚本 `scripts/` |
| `frontend/` | React + TypeScript（Vite）：用户端（访问码登录、IndexedDB 历史/设置、引用弹窗、来源选择）+ 隐藏管理端（hash 路由 `#/admin`） |
| `skill/` | 本地预处理 Skill（`SKILL.md`）与确定性脚本：文字提取、分块、向量生成、打包、校验 |
| `docs/` | [资料包格式说明](docs/资料包格式说明.md)、[兼容性验证](docs/兼容性验证.md)、[人工测试清单](docs/人工测试清单.md) |
| `deploy/` | systemd 单进程单元、Nginx HTTPS 代理片段、服务器部署脚本 |

## 本地运行

```bash
# 0) 环境依赖：Python 3.12 + Node 20+；密钥在根目录 .env（不入 git）
python -m venv .venv
.venv/Scripts/python -m pip install -r backend/requirements.txt   # Linux: .venv/bin/python

# 1) 前端（开发模式二选一：vite dev 代理；或构建后由后端托管）
cd frontend && npm install && npm run build && cd ..

# 2) 启动后端（首次启动自动初始化管理员 admin）
.venv/Scripts/python -m uvicorn app.main:app --app-dir backend --port 8000

# 3) 访问
#    用户端 http://127.0.0.1:8000/   管理端 http://127.0.0.1:8000/#/admin
#    管理端先设置访问码，用户才能登录
```

## 资料发布流程

1. 把原始文件交给预处理 Skill（Codex/Claude Code 中引用 `skill/SKILL.md`）；
2. Skill 提取标准化原文并与管理员逐项确认元数据；
3. `skill/scripts/build_package.py` 本地生成向量（1024 维）并打包；
4. `skill/scripts/validate_package.py` 本地校验；
5. 管理端网页上传 → 预览修正 → 确认替代关系 → 发布。

## 测试与验证

```bash
.venv/Scripts/python -m pytest backend/tests -q          # 21 项自动化测试
.venv/Scripts/python backend/scripts/verify_compat.py    # 外部服务兼容性（需网络）
.venv/Scripts/python backend/scripts/e2e_real.py         # 真实模型端到端（需网络与密钥）
```

## 部署（Ubuntu 24.04，1C1G）

见 `deploy/deploy.sh` 与 `deploy/campus-policy-agent.service`；前端在本地构建，服务器只跑单后端进程并接入现有 HTTPS 代理（SSE 需关闭缓冲，见 nginx 片段）。

## 安全约定

- `deploy-config.md`、`.env`、`测试文档/`、资料包 zip 均不入 git；密钥不写日志、不入资料包、不入备份。
- 服务端不持久化问答正文与工具结果；日志只记动作与标识。
- 服务器对资料包独立实施全套安全校验（路径穿越/符号链接/解压炸弹/哈希与向量一致性/重复导入）。
