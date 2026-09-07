# 班级政策问答 Agent

为 30～80 位同学提供手机、电脑均可使用的政策问答网站：领域筛选、跨领域查询、原文行号溯源、版本管理（现行/往年/手动停用）。管理员负责资料发布，学生不上传文件。实施依据见 [plan.md](plan.md)。

低内存重构规范见 [Rust 低内存重构 Spec](docs/Rust低内存重构Spec.md)；已在 `rust` 分支完成全部重构并全量测试通过（全套 36 项 Rust 测试全部通过，D1 混合检索 p95 仅 20ms、常驻内存仅 87.92MB，详见 [Rust 内存基准报告](docs/Rust内存基准报告.md) 与 [Rust 重构验收记录](docs/Rust重构验收记录.md)）。

## 结构

| 目录 | 内容 |
| --- | --- |
| `backend-rust/` | 原生 Rust 低内存后端：Axum 0.8 + rusqlite + jieba-rs + npyz；实现与 Python 100% 兼容的 API、流式 SSE、包安全校验与事务发布、v2 快照备份恢复与维护门；测试 `tests/`，基准 `src/bin/bench_d1.rs` |
| `backend/` | 原 Python (FastAPI + LangGraph) 后端：作为行为对照和回滚基准保留（81 项测试全绿） |
| `frontend/` | React + TypeScript（Vite）：用户端（访问码登录、IndexedDB 历史/设置、引用弹窗、来源选择）+ 隐藏管理端（hash 路由 `#/admin`） |
| `skill/` | 本地预处理 Skill（`SKILL.md`）与确定性脚本：文字提取、分块、向量生成、打包、校验 |
| `docs/` | [Rust低内存重构Spec](docs/Rust低内存重构Spec.md)、[Rust重构验收记录](docs/Rust重构验收记录.md)、[Rust内存基准报告](docs/Rust内存基准报告.md)、[资料包格式说明](docs/资料包格式说明.md)、[人工测试清单](docs/人工测试清单.md) |
| `deploy/` | systemd 单元（Python 版与 Rust 低内存版 `campus-policy-rust.service`）、Nginx HTTPS 代理片段、服务器部署脚本 |

## 本地运行

### 方式一：运行原生 Rust 后端（低内存推荐，默认端口 8012）

```powershell
# 1) 构建前端并编译 Rust release 二进制
npm.cmd --prefix frontend run build
cargo build --manifest-path backend-rust/Cargo.toml --release

# 2) 一键启动（自动加载根目录 .env，数据目录隔离于 .local-acceptance/rust-data）
pwsh -NoProfile -File ./scripts/start-local-rust.ps1
```

用户端访问 `http://127.0.0.1:8012/`，管理端 `http://127.0.0.1:8012/#/admin`。

### 方式二：运行 Python 试用（作为对比基准，默认端口 8011）

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
.venv/Scripts/python -m pytest -q                       # 根目录运行，81 项离线测试
.venv/Scripts/python backend/scripts/verify_compat.py    # 外部服务兼容性（需网络；结果写入 .local-acceptance）
.venv/Scripts/python backend/scripts/e2e_real.py         # 真实模型端到端（需网络与密钥）
```

前端在 `frontend` 目录执行：

```bash
npm ci
npm test                      # 26 项组件/API 测试
npm run build                 # TypeScript + 生产构建
npx playwright install chromium  # 仅本地测试机安装，服务器不安装浏览器
npm run test:e2e               # 桌面/手机 Chromium 共 12 项，模拟 API
```

离线测试使用合成资料和隔离临时库。真实端到端脚本也使用临时库，不修改正式访问码或资料，但仍会调用付费外部服务，需先准备指定的真实测试包。旧版兼容性 PASS 不代表审查修复后已通过；人工验收、真实反代复验及 1C1G 压测未完成前不得上线。

本机人工验收不要直接操作已有的 `backend/data`。请从 [人工测试清单](docs/人工测试清单.md) 的“本机验收准备”开始，使用 `.local-acceptance/data` 隔离目录和端口 8010。

## 部署（Ubuntu 24.04，1C1G）

见 `deploy/deploy.sh` 与 `deploy/campus-policy-agent.service`；前端在本地构建，服务器只跑单后端进程并接入现有 HTTPS 代理（SSE 需关闭缓冲，见 nginx 片段）。

升级需要安装更新的依赖（含 `psutil`）并重新构建前端。备份仅支持含资料包的完整 v2 格式；旧 v1 须在原实例重新导出。恢复故障标记、适用范围迁移和容量验收说明见 [审查修复记录](docs/审查修复记录.md)。

## 安全约定

- `deploy-config.md`、`.env`、`测试文档/`、资料包 zip 均不入 git；密钥不写日志、不入资料包、不入备份。
- 服务端不持久化问答正文与工具结果；日志只记动作与标识。
- 服务器对资料包独立实施全套安全校验（路径穿越/符号链接/解压炸弹/哈希与向量一致性/重复导入）。
