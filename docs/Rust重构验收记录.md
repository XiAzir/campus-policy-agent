# Rust 重构验收记录

> 按里程碑登记实际执行的命令、结果与 commit。不混用旧 Python 勾选；未执行的不写。
> 环境说明：开发与测试在 WSL Ubuntu 26.04（与部署目标 Ubuntu 24.04 同为 Linux；1C1G 对照
> 测试须在目标内核环境另行执行，见 M6）。

## M0：基线冻结与契约夹具（2026-09-06）

| 项 | 命令 | 结果 |
| --- | --- | --- |
| Python 基线重跑 | `.venv/Scripts/python.exe -m pytest backend/tests -q` | **81 passed**（9.36s，Windows 本机） |
| 契约夹具生成 | `.venv/Scripts/python.exe scripts/rust/gen_fixtures.py` | 84 个文件 → `backend-rust/tests/fixtures/` |
| D1 数据集 | `.venv/Scripts/python.exe scripts/rust/gen_d1.py --dataset d1` | 5 包 / 100 文档 / 10,000 分块 / 39.1MiB 裸向量 → `.local-acceptance/datasets/d1/`（忽略目录） |
| Rust 工具链 | WSL Ubuntu 26.04 用户目录 rustup | cargo/rustc 1.98.1 stable（`/home/pengyuhao/.cargo`） |

夹具覆盖：26 条路由 63 个快照用例；SSE 5 场景（含排队与上游故障）；令牌/口令互认向量；
jieba 分词 golden 15 语料 + 7 查询；FTS bm25 golden；NPY C/Fortran 与 5 类坏样本；配置默认值；
6 文档合法包与路径穿越坏包。基线怪癖清单见 `docs/Rust重构契约与差异.md` §2。

待办（后续阶段）：D3 数据集生成与 cgroup 采样脚本在 Linux 侧实跑（脚本已交付
`scripts/rust/sample_memory.py`）；26 条路由之外的前端 26 组件/12 浏览器回归在 M3 直连时执行。

## M1：Cargo 骨架、配置、鉴权、SQLite 读取、静态前端、指标（2026-09-06）

| 项 | 命令 | 结果 |
| --- | --- | --- |
| 依赖与工具链 | `backend-rust/Cargo.toml`, `backend-rust/rust-toolchain.toml` | 锁定 1.98.1、axum 0.8、rusqlite 0.33 bundled、pbkdf2、sha2、hmac、subtle、reqwest rustls、jieba-rs 等 |
| 配置契约测试 | `cargo test config` (WSL) | 15 项配置字段默认值与夹具 `config_defaults.json` 100% 对齐，`ensure_secrets` 拦截成功 |
| 鉴权互认测试 | `cargo test auth` (WSL) | `password_vectors.json` 4 组用例 + 4 组畸形样本全部通过；`token_vectors.json` 4 组令牌 + 篡改样本全部通过；内存令牌桶限流通过 |
| 数据库只读兼容 | `cargo test db` (WSL) | 有界通道架构（1写2读连接，busy_timeout 5000，2MB cache）；成功读取 Python 生成的 legacy `campus.db`（6篇文档、版本链、状态统计、settings） |
| 受众规则过滤 | `cargo test audience` (WSL) | 全校、学院/年级抽取、confirmed 覆盖逻辑全部通过 |
| API集成测试 | `cargo test --test api_integration` (WSL) | `/api/auth/state`、`/api/catalog` 鉴权与列表、`/api/source/{uid}/text` 窗口与闭区间保护（201行限制）、`/api/source/{uid}/file` nosniff与流式下载、`/api/admin/metrics` 全部通过 |
| 代码质量校验 | `cargo clippy --all-targets --locked -- -D warnings` & `cargo fmt --check` (WSL) | **0 warnings, 0 errors**，代码格式与 lint 完全达标 |
| Release 编译 | `cargo build --release` (WSL) | 成功产出优化二进制 `campus_policy_backend` |

M1 出阶段门槛：token 双向互认、旧库目录/原文/版本可读、指标接口、编译零警告全部达成。

## M2：NPY 流式读取、分词、FTS、范围、Top-K/RRF 融合与 D1 基准初测（2026-09-06）

| 项 | 命令 | 结果 |
| --- | --- | --- |
| NPY 流式读取与头校验 | `cargo test vectors` (WSL) | 严格解析 NPY 1.0/2.0 头，工作缓冲上限 4MB；验证 C 与 Fortran 两种矩阵布局计算点积与 golden 偏差 `< 1e-5`；拦截大端、float64、一维、NaN 异常样本 |
| jieba 分词与查询构造 | `cargo test tokenizer` (WSL) | 15 项真实长句与标点语料 `jieba_golden.json` 逐 token 顺序与空格连接 100% 对齐；7 组特殊查询（含中英混排、标点、长字符）MATCH 表达式 100% 对齐 |
| FTS5 bm25 分值与排序 | `cargo test test_fts_bm25_golden_alignment` (WSL) | 5 组多词检索命中 rowid 序列与 bm25 升序分值与 SQLite FTS5 golden 偏差 `< 1e-5` 完全对齐 |
| RRF 排名融合与范围过滤 | `cargo test test_hybrid_search_with_legacy_fixtures` (WSL) | 现行/往年过滤、学院/年级受众过滤、领域标签约束生效；FTS 与向量 RRF (k=60) 融合排序，详细返回 SearchHit（含 PDF 页码映射与原文切片） |
| D1 合成数据集基准实测 | `cargo run --release --bin bench_d1` (WSL) | **100 份文档、10,000 分块、1024 维 (39.1 MiB 裸向量)** 全库扫描：<br>• 100 次混合检索耗时 2.03s（平均 20.3ms）<br>• **p95 延迟: 0.025s**（远低于规范要求 3.0s）<br>• **进程常驻 RSS: 88.23 MiB**（远低于规范门槛 160.0 MiB，内存增量仅 81.75 MiB） |
| 代码质量校验 | `cargo clippy --all-targets --locked -- -D warnings` & `cargo fmt --check` (WSL) | **0 warnings, 0 errors**，所有单元测试与集成测试（共 20 项）全部通过 |

M2 出阶段门槛：分词 golden 对齐、FTS bm25 对齐、C/Fortran NPY 对齐、D1 内存 (88.23MB < 160MB) 与 p95 延迟 (0.025s < 3.0s) 全部达标。

## M3：原生上游协议、轻量 Agent、SSE 事件流、排队/背压/取消、引用组装（2026-09-06）

| 项 | 命令 | 结果 |
| --- | --- | --- |
| 上游协议客户端与 embedding 校验 | `src/llm.rs` 单元验证 | Gemini 原生 `/v1beta` 流式协议；保留原始 parts 与 thought 属性隔离；SiliconFlow embedding 规范化与零向量/异常值拦截 |
| 有界任务管理器与取消 | `cargo test --test agent_integration test_chat_manager` (WSL) | 1 并发工作槽 + 10 排队容量；同浏览器单任务限制；超时保护；正在执行取消与排队取消即时释放全部通过 |
| 历史截断算法 | `cargo test --test agent_integration test_trim_history` (WSL) | 12 轮且 8000 字符限制自最旧轮次平滑截断，保留完整 user/model 结构通过 |
| Agent 完整端到端 SSE 流 | `cargo test --test agent_integration test_mock_upstream_sse_chat_flow` (WSL) | 模拟 Gemini + SiliconFlow 端到端：`started -> generating -> analyzing -> retrieving -> embedding -> searching -> composing -> delta -> verifying -> metrics -> citations -> done` 全流程通过；合法 EV1 保留，伪造 EV99 准确剔除 |
| 领域与范围扩展交互 | `cargo test --test agent_integration test_mock_upstream_expand_request_flow` (WSL) | 超出限定领域输出 `[[EXPAND_REQUEST:原因]]` 标记；服务拦截并发出 `expand_request` 事件，不发送 `done` 成功事件 |
| 错误处理语义 | `cargo test --test agent_integration test_mock_upstream_error_flow` (WSL) | 上游 500 错误直接中断并向客户端广播规范错误，不产生双重内部错误包装且不发 `done` |
| 代码质量校验与全套测试 | `cargo clippy --all-targets --locked -- -D warnings` & `cargo fmt --check` & `cargo test --locked` (WSL) | **0 warnings, 0 errors**，全套 26 项测试（15 单元 + 5 API 集成 + 6 Agent 模拟集成）全部通过 |

M3 出阶段门槛：Gemini 原生流式、只读三工具调度、单工作槽排队背压与取消、剔除伪造引用、SSE 契约完全达成。

## M4：流式包校验、草稿修正、事务发布、版本与并发读（2026-09-07）

| 项 | 命令 | 结果 |
| --- | --- | --- |
| ZIP 安全防护与解压炸弹拦截 | `cargo test --test ingest_integration test_ingest_path_traversal` (WSL) | 严格拦截 `../` 路径穿越、非法根条目、非法盘符、符号链接、条目数上限（10,002）与压缩比异常（>200） |
| 包结构与逐字一致性校验 | `cargo test --test ingest_integration test_ingest_duplicate` (WSL) | 校验原文件 SHA-256、文本 SHA-256、行数、chunk 逐字符一致性、章节/领域闭包约束与连续 vector_index；包哈希重复拒绝 |
| 草稿预览与元数据覆盖 | `cargo test --test ingest_integration test_ingest_preview` (WSL) | `title/department/effective_date/audience/audience_scope/notes/replaces` 7 项白名单可修正；拦截非法字段（如修改 chunks 报 400） |
| 事务性原子发布与替代链 | `cargo test --test ingest_integration test_ingest_publish` (WSL) | `draft -> published` 原子事务切换；旧版自动进入 `superseded`（已手动停用则保持 `manual`）；生成关联表 `sections`, `doc_tags`, `chunks`, `chunks_fts`, `vector_rows` 与审计日志 |
| 停用、启用与解除替代 | `cargo test --test ingest_integration test_ingest_publish_and_versions_lifecycle` (WSL) | `deactivate_document` 标记 manual；`enable_document` 启用时严格校验若已被替代须先调用 `unlink_replacement`，防止双现行版本并存 |
| 丢弃草稿包与文件清理 | `cargo test --test ingest_integration test_ingest_discard_draft` (WSL) | 丢弃草稿后清理 `pkg-draft-*.npy` 与暂存 zip，再次丢弃返回 400 |
| HTTP 管理端端点全链路 | `cargo test --test api_integration test_api_admin_packages_http_endpoints` (WSL) | `GET /admin/packages`、`POST /admin/packages` (multipart 上传与 413 保护)、`GET /admin/packages/{id}`、`PATCH /admin/packages/{id}/documents/{hash}`、`DELETE /admin/packages/{id}`、`POST /admin/documents/{uid}/deactivate`、`POST /admin/documents/{uid}/enable`、`POST /admin/documents/{uid}/unlink` 完整通过 |
| 代码质量校验与全套测试 | `cargo clippy --all-targets --locked -- -D warnings` & `cargo fmt --check` & `cargo test --locked` (WSL) | **0 warnings, 0 errors**，全套 33 项测试（15 单元 + 6 Agent 集成 + 6 API 集成 + 6 Ingest 集成）全部通过 |

M4 出阶段门槛：恶意包与路径穿越防御通过、元数据修正限制生效、事务发布与替代关系完整、管理端 HTTP 接口契约完全达成。

## M5：v2 备份双向兼容、维护门、故障标记与原子回滚恢复（2026-09-07）

| 项 | 命令 | 结果 |
| --- | --- | --- |
| SQLite Backup 快照与 v2 备份导出 | `cargo test --test backup_integration test_backup_create_and_validate` (WSL) | 基于 SQLite Backup API 生成一致快照 `db.sqlite`；输出符合 `format=campus-policy-backup, version=2` 的 `backup.json`，含不可变文件 SHA-256 与大小清单，流式打包不长期持有写锁 |
| 备份安全解压防护与深度校验 | `cargo test --test backup_integration test_restore_rejected_cases` (WSL) | 严格拦截 `../` 路径穿越、条目数上限（100,000）、解压上限（10 GiB）、压缩比炸弹（>200）以及清单与文件不一致的坏备份 |
| 数据库完整性与外键校验 | `check_unpacked_snapshot` 深入执行 | 校验 9 张核心数据表结构与 `audience_scope` 兼容字段；PRAGMA integrity_check 与 foreign_key_check；全量资料行数/哈希、FTS 关联、向量行连续性与归一化（模长容差 1e-3） |
| 维护门模式与旧任务优雅排空 | `cargo test --test backup_integration test_maintenance_gate_and_restore_cycle` (WSL) | 维护门开启期间所有新 `/api` 请求返回 503；自动触发 `cancel_all` 终止排队和运行中问答任务；最多等待活动请求 30 秒超时保护 |
| 原子安装、标记文件与回滚状态机 | `install_backup` 状态机执行 | 写入 `<DATA_DIR>.restore-in-progress` 标记；安全关闭当前读写连接与向量句柄；同级目录原子移动 `current -> rollback` 并安装新目录；重开数据库验证查询成功后清理 rollback 目录与标记；失败保留现场锁死 |
| 令牌签名密钥热刷新 | HTTP POST `/api/admin/restore` 触发验证 | 恢复成功后从新库 `settings.token_secret` 动态重载 `TokenService`，旧令牌安全失效，备份中的管理员与用户令牌无缝互认 |
| HTTP 管理端端点全链路 | `GET /api/admin/backup` & `POST /api/admin/restore` (WSL) | 备份下载流式分发、带时间戳中文附件名；恢复接口 multipart 上传与 10 GiB 上限拦截；恢复后 catalog 列表秒级就绪 |
| 代码质量校验与全套测试 | `cargo clippy --all-targets --locked -- -D warnings` & `cargo fmt --check` & `cargo test --locked` (WSL) | **0 warnings, 0 errors**，全套 36 项测试（15 单元 + 6 Agent 集成 + 6 API 集成 + 6 Ingest 集成 + 3 Backup 集成）全部通过 |

M5 出阶段门槛：v2 双向备份结构完全符合、坏备份/路径穿越拦截有效、维护门 503 拦截生效、原子替换与回滚保护可靠、恢复后继续读写通过。

## M6：1C1G 资源基准初测、发布启动脚本与交付（2026-09-07）

| 项 | 命令 | 结果 |
| --- | --- | --- |
| 本地启动脚本交付 | `scripts/start-local-rust.ps1` | 支持指定独立端口（默认 8012）、自动加载 `.env` 模型配置、挂载独立数据目录 `.local-acceptance/rust-data` 与静态前端 `frontend/dist`，绝不干扰既有 Python 试用实例 |
| 生产 systemd 单元模板 | `deploy/campus-policy-rust.service` | 遵循非特权用户运行、`NoNewPrivileges=true`、只读系统根、`LimitCORE=0` 防密钥泄露、`LimitNOFILE=1024`、`MemoryHigh=384M`、`MemoryMax=512M`、`MemorySwapMax=0` |
| Nginx 反代配置更新 | `deploy/nginx-site.conf.example` | 配置 SSE 禁用缓冲 `proxy_buffering off`、针对 `/api/admin/restore` 放宽 `client_max_body_size 10240m` 并关闭请求体缓冲 `proxy_request_buffering off` |
| 前端回归测试与构建 | `npm.cmd --prefix frontend test && npm.cmd --prefix frontend run build` | **26 passed (100% 通过)**；生产构建产出 `frontend/dist/` 正常分发 |
| Rust Release 编译与二进制产出 | `cargo build --release --locked` (WSL) | 成功产出优化原生可执行二进制 `backend-rust/target/release/campus_policy_backend` |
| D1 合成数据集基准实测 | `cargo run --release --bin bench_d1` (WSL) | **100 份文档、10,000 分块、1024 维 (39.1 MiB 裸向量)**：<br>• 100 次混合检索耗时 1.60s（平均 16.0ms）<br>• **p95 延迟: 0.020s**（远低于规范要求 3.0s）<br>• **初始全库常驻 RSS: 74.57 MiB**（远低于规范门槛 160.0 MiB）<br>• **百次检索后常驻 RSS: 87.92 MiB**（远低于规范门槛 224.0 MiB，增量仅 13.35 MiB） |
| 代码质量校验与全套测试 | `cargo clippy --all-targets --locked -- -D warnings` & `cargo fmt --check` & `cargo test --locked` (WSL) | **0 warnings, 0 errors**，全套 36 项测试全部通过 |
| 基准报告交付 | `docs/Rust内存基准报告.md` | 完整记录 D1 性能、指标门槛、交付清单及生产环境待验收事项 |

M6 本机门槛：发布脚本完备、Release 编译通过、前端回归通过、D1 延迟与内存达标、待验收清单明确。生产服务器 1C1G 真实 cgroup 采样与 D2 规模容量待正式上线时执行。
