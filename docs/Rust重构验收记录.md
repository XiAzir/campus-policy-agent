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
