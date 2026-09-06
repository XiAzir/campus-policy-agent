# Rust 重构契约与差异记录

> 阶段：M0（基线冻结）。对照基线 commit `3752745`，规范见 `docs/Rust低内存重构Spec.md`。
> 本文件记录 Rust 实现必须互认的契约、已知基线怪癖及处理决定；每个里程碑追加章节。

## 1. 契约夹具（M0 生成）

生成命令：`.venv/Scripts/python.exe scripts/rust/gen_fixtures.py`（产物 `backend-rust/tests/fixtures/`，84 个文件）。

| 目录 | 内容 | Rust 侧要求 |
| --- | --- | --- |
| `auth/token_vectors.json` | HMAC 令牌签发/验签向量（含键序/空格不同的等价 payload、篡改样例） | 逐值对齐；验签用原始 base64url 段 |
| `auth/password_vectors.json` | PBKDF2-HMAC-SHA256、200,000 次、16B salt、32B digest 向量与畸形样例 | 逐值对齐 |
| `tokenize/jieba_golden.json` | Python jieba（0.42.1，默认字典，cut_for_search）15 条语料 + 7 条查询 | 逐 token 对齐（顺序、重复 token） |
| `retrieval/fts_bm25_golden.json` | 同一 FTS5 库上的 bm25 分值与排序 | 排序一致、分值近似 |
| `retrieval/vector_queries.json` + `*.npy` | C/Fortran 布局 float32 点积、坏样本（float64/一维/NaN/零行/大端） | 得分一致（±1e-5）；坏样本必须拒绝 |
| `routes/main_*.json`（63 例） | 26 条路由的请求/响应快照（状态码、JSON 结构、错误 detail 格式） | 结构与语义对齐；动态字段另注 |
| `sse/*.json`（5 场景） | full_flow / expand_request / tool_failure / upstream_error / queued | 事件序列对齐（§3 已注明的基线怪癖除外） |
| `config/config_defaults.json` | 环境变量名与默认值 | 名称与默认值一致 |
| `packages/small_v1.zip` | 6 文档合法包（txt/md/pdf/docx、250 行长文、page_map） | 可导入发布 |

## 2. 已冻结的基线怪癖及处理决定

以下行为在夹具中如实捕获；按 spec 6.3 允许的差异处理，Rust 侧消除时须有测试证明"一次有效 started、一次语义终止"：

1. **queued 任务双重 `started`**：`chat.py` 的 `_advance` 先 put 一次 started 再进入 `_start` 再 put 一次（`sse/queued_second_client.json` second 客户端序列为 `queued, started, started, …`）。Rust 消除为单次。
2. **upstream_error 双重终止**：Agent 捕获 LLMError 先发语义 `error`（含"模型调用失败 HTTP 500"），ChatManager 包装层再发 `服务内部错误：LLMError`。Rust 保留第一条语义错误，不再追加内部错误。
3. **空文本签名 part 产生空 delta**：`full_flow` 中两个 delta 其一为空字符串（thoughtSignature 空 text part）。Rust 保持行为（不合并 part），空 delta 允许原样发送或跳过——前端对空串无操作，两种均视为兼容；推荐跳过并在差异记录登记。
4. **`argpartition` 同分不稳定**：`vectors.py` 每 4096 行块取局部 top 64，float32 同分时块间顺序未定义。Rust 按 spec 7.3 固定 tie-break（package_id/row_index），白名单差异以 chunk 语义等价记录。
5. **管理员密码改回短密码被 422 拒绝**：`new_password` min_length=8（`routes/main_admin_password_rotate_back.json` note）。初始密码 admin 仅空库初始化。
6. **`POST /api/chat` 空问题返回 422**（pydantic 校验），非 400。Axum 侧须转换为项目 JSON `{"detail": …}` 格式。

## 3. 路由与字段快照摘要

- 26 条路由全部有快照（`scripts/rust/gen_fixtures.py` 的 `drive_routes` 覆盖清单与 spec 6.1 一一对应）。
- 动态字段：`/admin/metrics` 的 `rss_bytes/cpu_s/disk_free_bytes`、`request_id`、`imported_at/published_at/at`（生成时刻固定为 `2026-09-06T12:00:00Z`）、`data_dir_mb`。Rust 测试比较结构与其余字段。
- 原文行前缀 `L{n}: `、`to` 闭区间且最多 `frm+200`（201 行）已在 `routes/main_source_text_201_lines.json` 冻结。

## 4. 分词与检索兼容策略（M2 实施前登记）

- Python jieba 版本 0.42.1、默认字典（dict.txt sha256 见 `tokenize/jieba_golden.json`）、cut_for_search 全模式。
- jieba-rs 的 `cut_for_search` 与 Python 版在 HMM/字典哈希上存在已知差异风险；M2 第一件事是跑 golden 对齐，不兼容时按 spec 7.1.4 提交差异与迁移方案，不得静默降级。

## 5. 差异登记表（随实施追加）

| 日期 | 模块 | 差异 | 决定 | 依据 |
| --- | --- | --- | --- | --- |
| 2026-09-06 | M0 | 初次冻结 | — | spec §1.8 |
