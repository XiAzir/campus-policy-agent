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

## M1：Cargo 骨架、配置、鉴权、SQLite 读取、静态前端、指标（待实施）
