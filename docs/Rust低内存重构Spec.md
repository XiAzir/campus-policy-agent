# Rust 低内存后端重构 Spec

> 状态：待实施。本文件是交给后续编码模型的实施规范，不是实现完成记录。
> 编写日期：2026-09-06。
> 对照基线：`375274584985953051f44fe27778a82f873b4f6d`。
> 首要目标：在功能、安全和检索质量不退化的前提下，降低服务器常驻及峰值内存。
> 本次仅交付规范；不安装 Rust、不修改应用代码、不切换本机服务、不操作服务器。

## 1. 执行约定与决策

本文中的“必须”为验收条件，“建议”为可替换方案。下文文件路径均相对于仓库根目录，便于在其他 worktree 中交接。

1. 先阅读 `AGENTS.md`、`plan.md`、本 spec，再阅读实现及回归测试。遵守每完成一个可运行模块立即中文本地提交的约定；不 push。
2. 在 `backend-rust/` 新建 Rust 服务，逐模块替代整个常驻 Python 后端。不是给 Python 增加一个 Rust 检索子进程，也不是只翻译路由层。
3. 保留 `frontend/` 的 React、TypeScript、Markdown、安全过滤、原文依据折叠、引用弹窗、实际工作步骤及 IndexedDB 行为。正常情况下不改前端业务代码。
4. 保留 `skill/` 的本地 Python 文档提取、分块、向量生成和打包。最终服务器只运行 Rust 后端和现有 HTTPS 代理；不运行 Python、Node.js、LangGraph 或任何本地模型。
5. 保留 `backend/` 作为行为对照和回滚实现，至少持续到 Rust 全部验收通过。不要先删除旧代码或批量改写旧测试以“适配”新实现。
6. 采用单进程、单问答运行槽、最多 10 个排队；不通过增加进程、外部数据库、Redis 或付费服务转移内存占用。
7. 未经用户另行授权，只在本机隔离目录实施及验收，不部署、不读取私有资料内容、不调用付费 API。真实模型测试和服务器切换需单独确认。
8. 安全边界优先于旧实现偶然行为。本 spec 明确列出的有界化和协议修正可实施；其他行为差异必须先记录原因、影响、测试及回滚方式，再更新计划，不能静默偏离。

### 1.1 不属于本轮的工作

- 不重做 UI，不引入 SSR，不把 Markdown 移到服务器渲染。
- 不更换主模型、embedding 模型、维度、预处理版本、资料包格式或既有文档标识。
- 不引入 ANN/HNSW、向量量化、降维或改变召回范围；这些会引入独立的质量取舍。
- 不在服务器解析 PDF/DOCX、不做 OCR、不生成资料向量。查询向量仍由外部 API 生成。
- 不承诺减少上游模型延迟或 token 费用，不拿空壳 HTTP 压测作为项目性能结论。

## 2. 基线与必须保留的能力

现实现为 FastAPI、LangGraph、SQLite FTS5、jieba、NumPy mmap。模型与查询向量分别通过 `backend/app/llm.py` 中的外部 HTTP 客户端调用。前端构建结果由 `backend/app/main.py` 提供。

| 范围 | 必读文件 |
| --- | --- |
| HTTP 与字段 | `backend/app/api.py`、`frontend/src/api.ts`、`frontend/src/types.ts` |
| 排队与编排 | `backend/app/chat.py`、`backend/app/agent.py`、`backend/app/llm.py`、`backend/app/metrics.py` |
| 检索与适用性 | `backend/app/retrieval.py`、`backend/app/vectors.py`、`backend/app/textutil.py`、`backend/app/audience.py` |
| 数据与资料包 | `backend/app/db.py`、`backend/app/pkgfmt.py`、`backend/app/ingest.py`、`docs/资料包格式说明.md` |
| 故障与安全 | `backend/app/backup.py`、`backend/app/maintenance.py`、`backend/app/storage.py`、`backend/app/security.py`、`backend/app/body_limit.py`、`backend/app/logging_safe.py` |
| 验收与部署 | `backend/tests/`、`frontend/tests/`、`frontend/e2e/`、`docs/审查修复记录.md`、`docs/人工测试清单.md`、`deploy/` |

基线历史记录为后端 81 项、前端组件 26 项、模拟 API 的桌面/手机浏览器 12 项通过。它们不是 Rust 的测试结果，也不是生产验收。实现方必须重新运行并登记实际数量，不得复制历史 PASS。

此前 Windows 的 5 份资料、7 个分块观测不能作为 Ubuntu 的内存基线；本 spec 不以其推算节省比例。

## 3. 内存目标与测量协议

### 3.1 指标定义

全部内存单位使用 MiB，即 `bytes / 1024 / 1024`，原始结果同时保存 bytes。

- 进程：RSS、峰值 RSS、可获取时的 PSS、匿名内存、线程数、文件描述符数。
- 服务 cgroup：`memory.current`、`memory.peak`、`memory.stat` 中的 `anon`、`file`、`kernel`、`sock`，以及 `memory.events` 和 swap 用量。按键解析，不能固定字段顺序。[R1]
- 宿主机：`MemAvailable`、swap、Nginx 及其他服务的独立用量。采样器与模型模拟器不得放进被测后端的 cgroup。
- 必须区分匿名分配和可回收文件页缓存。mmap 不等于“零内存”，解除映射也不保证页缓存立刻消失。不能只报 RSS，或把 `VSZ/VmSize` 当成物理内存。
- `/api/admin/metrics` 保留既有字段；新增指标只能加字段。独立系统采样为准，恢复期间 API 返回 503 时继续采样，不能漏掉峰值。

### 3.2 验收数据集

| 编号 | 数据 | 用途 |
| --- | --- | --- |
| D0 | 合成小库，覆盖 PDF/DOCX/MD/TXT、草稿、现行、替代停用、手动停用、版本链和章节标签 | 功能、故障注入和跨语言契约 |
| D1 | 固定种子的 100 份文档、合计 10,000 分块、1024 维 float32；真实长度分布的合成中文原文；按资料包限制拆分 | 必做、可重复的内存与延迟标准集 |
| D2 | 用户授权提供的正式规模资料，覆盖拟上线完整集合；记录实际文档数、分块数、维度和字节数 | 上线容量认证；未提供则明确待验收 |
| D3 | 100,000 分块、1024 维、多个合法包，含窄范围和全库查询 | 压力上限探索，不自动承诺 1GB 支持此容量 |

容量由分块数、维度及数据字节数决定，不能仅用“500 份文件”描述。D1 向量裸数据约 39.1 MiB，D3 约 390.6 MiB；格式头、原文、SQLite、缓存和运行时另计。

D1/D3 向量必须有限且归一化，使用固定种子、不同方向和可控制的相似度间距；不能用全零、全重复或极易压缩的数据蒙混测试。生成脚本运行在测试机，合成大文件放忽略目录，不提交仓库。

### 3.3 工程门槛，不是已实现的性能承诺

以下门槛用于指导实施；若做不到，提交测量和瓶颈报告，不得自行降低标准或宣称重构完成。

| 场景 | Rust 验收目标 |
| --- | --- |
| D1 完成分词初始化、TLS 模拟请求、一次全库查询后静置 60 秒 | 末 30 秒匿名内存中位数不超过 96 MiB，RSS 中位数不超过 160 MiB |
| D1 连续问答及 10 个同时到达请求，保持 1 运行 + 排队 | 匿名内存峰值不超过 128 MiB，RSS 峰值不超过 224 MiB |
| D1 上导入/发布接近 200 MiB 合法包，备份/恢复，及允许的问答重叠 | 匿名内存峰值不超过 192 MiB，RSS 峰值不超过 320 MiB |
| D1 全套资源流程 | `MemoryHigh=384M`、`MemoryMax=512M`、`MemorySwapMax=0` 下无 OOM、无任务遗漏、无功能失败；记录回收压力及延迟 |
| 同条件 Python/Rust 对照 | D1 预热空闲及问答场景，匿名内存对应指标至少减少 40%；问答峰值 RSS 至少减少 25% |
| 稳定性 | 预热后 1,000 次模拟问答及至少 2 小时浸泡；末段静置匿名内存较同样预热后的起点增长不超过 16 MiB；线程/FD/任务数量回落到配置边界 |

匿名指标优先使用独立 cgroup 的 `memory.stat anon`，不能在对照中一边用它、一边用 Windows 私有提交量。相对收益与绝对门槛必须同时满足，功能减配的结果无效。

### 3.4 公平对照与延迟保护

1. 同一 Ubuntu 24.04、同一架构、1 CPU 配额、1GB 测试环境上顺序运行 Python 与 Rust release。记录内核、SQLite、jieba 字典哈希、编译器、依赖锁、提交、配置和模拟负载版本。
2. 相对收益对照先统一为原 `MemoryHigh=600M`、`MemoryMax=700M`、禁用 swap；再对 Rust 单独执行 512M 资格测试。不能只把 Rust 内存限额调低便称为节省。
3. 每场景至少 3 次独立运行，交替先后顺序；报告各次原始峰值和中位数，所有有效运行满足绝对门槛。采样间隔不大于 100ms，同时读取内核峰值计数防漏采。接口以目标内核实际支持为准；缺失指标不写 0。
4. 冷/热缓存分开测；仅在一次性测试 VM 中统一控制缓存。新建 cgroup 不自动清除共享缓存，不得复用另一实例已承担的缓存造成偏差。不在用户正常服务器执行全局缓存清理。
5. 固定外部模拟 SSE、工具往返、向量响应、网络延迟和 token 元数据。模拟器不与后端共享 CPU 配额。真实模型兼容性单列，不用于计算确定性速度收益。
6. 新增 `local_retrieval_s`、`embedding_s` 等数值字段：旧 `retrieval_s` 含查询 embedding 等待，保留语义，不能当纯本地检索。
7. D1 热态本地混合检索 p95 不超过 3 秒，且 `p95_Rust <= p95_Python + max(0.2 * p95_Python, 0.05秒)`；冷读结果单列。至少 100 次，覆盖全库和过滤查询。全链路、首字延迟、排队时间也须报告。
8. 正常取消后 1 秒内回收问答槽；无业务事件时约 15 秒一次 SSE 心跳。磁盘故障引起的不可中断调用例外如实报告，不提前释放仍占用的工作槽。
9. D2 必须达到同样的功能和 512M 运行资格门槛；分项内存随规模报告，不把 D1 指标套成任意规模承诺。D2 不达标则阻止切换。D3 即使失败也报告上限，不影响已证明的较小容量结论。

同限额对照中，Rust 的预热空闲和问答 cgroup 总用量及峰值也必须报告，三次运行中位数不得高于 Python 对应值的 105%。512M 资格测试不能替代该对照：文件读取方式变化可能降低 RSS，却只是把占用转移到未映射页缓存。发生这种情况须解释 anon/file 分项，不宣称同等幅度的服务器总内存节省。[R1]

接近 200 MiB 的包需用合法原文件组成并记录各条目大小/压缩比，不要求在 8 MiB manifest 中塞入同等大小正文。每次导入/备份场景从同一 D1 副本开始，避免 Rust、Python 测了不同的累积库。

## 4. 目标结构与依赖原则

建议使用一个 Cargo package，包含库、服务器二进制和诊断/基准入口，避免拆成微服务。

```text
backend-rust/
  Cargo.toml / Cargo.lock / rust-toolchain.toml
  src/
    main.rs / lib.rs / config.rs
    api/                 # 请求/响应、路由、SSE
    auth.rs / db.rs / audience.rs / tokenizer.rs
    vectors.rs / retrieval.rs / llm.rs / agent.rs / chat.rs
    package.rs / ingest.rs / backup.rs / maintenance.rs
    limits.rs / metrics.rs
  tests/
    contracts/
    fixtures/            # 仅小型合成数据/生成器，无私有资料和密钥
```

默认技术选择为 Tokio + Axum、Serde/serde_json、reqwest + Rustls、rusqlite + SQLite FTS5/backup、jieba-rs。NPY 使用成熟头解析器如 npyz，ZIP 使用成熟 ZIP 库；加密使用成熟 PBKDF2/HMAC/SHA-256 实现，禁止手写密码学。[R2][R3][R4][R5][R6][R7]

实施时核对依赖维护状态、许可证、MSRV 和实际 feature 名，再锁定可重复版本；不要求追逐最新版本。提交 Cargo.lock/toolchain，记录依赖安全审计。不要启用无关数据库驱动、图片处理、模型 SDK、全功能 Agent 框架或第二套 TLS。

常驻对象仅包括配置、受限连接/线程、一个分词器、受限资料元数据、任务管理器和 HTTP 客户端。HTTP 客户端按上游复用，配置连接池和空闲超时，不每次查询新建。[R4]

### 4.1 并发与连接所有权

- 1 个 Tokio 调度线程作为初始配置，CPU/同步 SQLite/ZIP/PBKDF2 不在其上直接执行。
- 建议 DB 专用工作线程：1 个写连接 + 最多 2 个读连接，各自有界队列；备份短期最多增加 2 个受控连接。不能按请求创建连接池。
- 检索 CPU 工作者最多 1 个，存储任务最多 1 个，密码计算最多 1 个；可合并线程，但不能让长发布阻塞所有目录读。
- 明确所有线程、排队任务、连接及缓冲的上限和所有者，提交预算表。阻塞池显式限制，并在提交前获得应用许可，不靠池内部排队承载任意任务。[R2]
- SQLite 每连接页缓存建议初始 2 MiB，配置忙等待；SQLite mmap 默认关闭。页缓存设置是预算建议，不是总内存硬上限。[R8]
- 保留 WAL、外键、事务和独立读路径。不跨外部 HTTP await 持有 DB 快照，避免 WAL 无限增长。
- 锁顺序和恢复 drain 必须写明；不持有事务、同步互斥锁或无必要文件锁等待上游模型。

## 5. 内存有界化要求

### 5.1 输入、队列和模型状态

| 对象 | 默认约束 |
| --- | --- |
| 普通 HTTP 请求体 | 256 KiB，兼容当前 `body_limit.py` |
| 资料包请求体 / ZIP 文件 | 210 MiB / 200 MiB；实际接收字节计数 |
| 备份恢复请求体 | 10 GiB 硬上限，仍须根据磁盘容量提前拒绝 |
| 问答历史 | 最近 12 轮、最多 8,000 个 Unicode 字符；问题 1..4,000 字符 |
| 排队 | 10 个；入队前校验/截断历史，只保留一份必要数据 |
| SSE 出站队列 | 最多 64 个事件且序列化有效载荷总计不超过 1 MiB；两种限制同时实施 |
| 单轮回答正文 | 最多 256 KiB UTF-8；超限明确中断，不写成功 done |
| 上游单个 SSE 事件 | 最多 1 MiB；单次问答累计上游事件正文最多 8 MiB |
| 本轮保留的完整模型/工具上下文 | 序列化有效载荷最多 8 MiB；实际堆分配也要计入内存验收 |
| 工具调用 | 最多 3 轮，每轮最多 8 个工具；总证据最多 192 条 |
| 普通活动 HTTP 请求 | 初始最多 64；许可覆盖完整 body/响应流生命周期；超额 429 |
| 管理上传 | 初始同时 1 个；已有存储操作时可在读 body 前 409；不进入无界后台队列 |
| 限流桶 | 最多 10,000 个 key，有 TTL 和受控过期清理，饱和时保守拒绝，不清空活跃桶放行 |

新增上游/输出/连接限制是本轮明确允许的资源保护，不是原功能已有保证。集中配置、测试边界、记录超限错误；不得静默截断签名、答案、工具返回或范围列表。合法的大返回如完整目录须流式序列化，不能偷偷减少文档数。

Rust 字符串字节长度不等于 Python `len(str)`；历史、问题、学院 60 字符和入学年份 20 字符使用 Unicode 标量值计数。输入字节上限和网络预算才使用字节。保留前端 payload 字段及默认值。

有界 mpsc 容量只限制消息数量，不能替代字节预算。[R3] 正常拥塞等待背压；超过 15 秒未消费，取消上游并关闭流，不能丢 delta 后发成功。终止/取消路径不得等待已断开消费者腾出队列。

取消贯穿等待槽、HTTP、SSE、检索块和后续工具。已开始的 `spawn_blocking` 不能靠 abort 停止；工作者在块边界检查取消标记，许可与锁由实际工作者持有直到退出。[R2] 管理发布/恢复进入关键区后须完成事务或回滚，即使浏览器取消也不提前解锁。

不缓存问答正文或 embedding 结果，不将聊天/工具结果写临时文件、数据库、日志或崩溃转储；设置 `LimitCORE=0`。上下文超限只报错，不落盘“卸载内存”。

### 5.2 向量与候选行

首版默认采用固定缓冲的文件读取/按偏移读取，而不是长期缓存全部 NPY 映射。格式不变，先证明内存与延迟达标。可选只读窗口 mmap 优化，但须单独测量并说明底层文件不可同时修改的安全前提。[R9]

- 查询向量归一化一次，校验维度、finite、范数。仍为准确的归一化向量点积，不量化、不抽样。
- 工作缓冲最多 4 MiB，块行数为 `max(1, floor(4 * 1024 * 1024 / (dim * 4)))`：1024 维最多 1024 行，8192 维最多 128 行。
- 不读取完整矩阵到 `Vec<f32>`，不收集全库 vector_rows 或 chunk 文本。候选由 SQL 游标/键集分页产出，每批最多 1024 条，保留 `(package_id, row_index, chunk_id)`，不另建全库反向 HashMap。
- SQL 要有索引可用的访问路径；新增普通索引须为 Python 可继续使用的加法迁移，并测迁移磁盘/内存。避免无界内存排序。
- 按包/行顺序扫描，每批释放；句柄缓存最多 8 个，不为每包永久保留映射和句柄。
- 有界 Top-K 堆，不积累每块全部局部最优再全排序。默认向量分支 K=24、FTS 最多 24、融合后 8 条；同分规则见第 7 节。
- Fortran-order NPY 按实际布局读取或在私有暂存中有界转换，不把列主序当行主序。不覆盖已发布向量、ZIP 或哈希记录。
- 不把 madvise/fadvise 当释放保证，不在正常服务中写全局 drop_caches。页缓存计入 cgroup。

### 5.3 文件、ZIP 和数据库读取

- 上传、复制、SHA-256、压缩解压、下载均使用不大于 1 MiB 的可复用 I/O 缓冲。避免大包 `.bytes()`、read_to_end/read_to_string、完整 Value 副本和整包内存 ZIP。
- manifest 上限 8 MiB、单原文 4 MiB、单包 20,000 chunks。允许一份有界 typed manifest + 一份原文，禁止 raw string + Value + 深拷贝 typed tree + 全部文档行数组同时存在。
- JSON 限递归/集合数量，NPY 头上限 10,000 字节。ZIP 中央目录解析前预检条目数、声明长度和元数据大小，不能等库已分配巨大目录后才限制。
- 原文只读请求行和上下文，行长仍受单文档上限保护。目录/预览只查输出字段，不加载 chunk 正文、向量或全量 ZIP。
- 测下载并发、连接超时、慢上传和断线暂存清理；缓冲不能随文件大小增长。
- 防止 WAL、临时 ZIP 和恢复副本积累；不能为省资源牺牲故障回滚数据。

## 6. HTTP、令牌和前端契约

### 6.1 路由清单

以下均带 `/api` 前缀。响应以基线及生成的契约夹具为准，保留空值、默认值、错误 detail 格式；不改另一套命名。

| 方法 | 路径 | 鉴权 |
| --- | --- | --- |
| POST | `/auth/login` | 公开，限流 |
| POST | `/auth/admin/login` | 公开，限流 |
| POST | `/auth/admin/password` | 管理员 |
| GET | `/auth/state` | 公开 |
| GET | `/catalog` | 用户 |
| GET | `/source/{doc_uid}/text` | 用户 |
| GET | `/source/{doc_uid}/file` | 用户 |
| GET | `/source/{doc_uid}/versions` | 用户 |
| POST | `/chat` | 用户 |
| POST | `/chat/cancel` | 用户 |
| GET | `/admin/packages` | 管理员 |
| GET | `/admin/packages/{package_id}` | 管理员 |
| POST | `/admin/packages` | 管理员，multipart file |
| PATCH | `/admin/packages/{package_id}/documents/{doc_hash}` | 管理员 |
| POST | `/admin/packages/{package_id}/publish` | 管理员 |
| DELETE | `/admin/packages/{package_id}` | 管理员 |
| GET | `/admin/documents` | 管理员 |
| GET | `/admin/documents/{doc_uid}/versions` | 管理员 |
| POST | `/admin/documents/{doc_uid}/deactivate` | 管理员 |
| POST | `/admin/documents/{doc_uid}/enable` | 管理员 |
| POST | `/admin/documents/{doc_uid}/unlink` | 管理员 |
| PUT | `/admin/access-code` | 管理员 |
| GET | `/admin/status` | 管理员 |
| GET | `/admin/metrics` | 管理员 |
| GET | `/admin/backup` | 管理员 |
| POST | `/admin/restore` | 管理员，multipart file |

兼容 400/401/404/409/413/422/429/503/507 和成功状态；保留前端可识别的语义、字段和安全错误，不要求复制 Python 异常类名。Axum extractor 默认错误需转换为项目 JSON，不能回传框架纯文本或上游正文。

原文参数为 frm、to，保留 `L1: ...`。区间为闭区间，当前 to 最多 frm+200，可能返回 201 行，不误改成 200；工具 read_source 最多 40 行是另一限制。

原文件/备份下载保留鉴权、内容类型、Content-Disposition 中文文件名、nosniff 和流式读取；验证 Range 和中断。禁止将 files/text/数据库/配置/备份挂为公开静态目录。

### 6.2 令牌与口令互认

- 不改 JWT。令牌为 `base64url(payload_without_padding) + '.' + signature`，payload 含 r/c/e；签名为对原始 base64url 段做 HMAC-SHA256 后取 hex 前 32 字符。key 为 settings.token_secret 的 hex 解码。
- 验签使用原始编码段，不重序列化 JSON；JSON 空格/键顺序不应影响旧 token。常量时间比签名。
- 口令为 `pbkdf2$<16字节salt的hex>$<digest的hex>`，PBKDF2-HMAC-SHA256、200,000 次、32 字节输出。保留旧 hash，不重置管理员/访问码，不因换语言强迫全部重新登录。
- 角色隔离、过期拒绝；client_id 来自验签 token，不信任 X-Client-Id；仅本人可取消 request_id。
- 换访问码只阻止旧码新登录，不失效旧 token。恢复使用备份签名密钥：该密钥签发令牌仍可验证，其他密钥令牌失效；签发/验证状态同时刷新。
- 初始密码仅空库初始化一次，保留 INITIAL_ADMIN_PASSWORD。正式切换要求私有管理员密码。
- 保留登录每分钟 2/burst10、聊天每分钟30/burst20、上传每分钟6/burst5；PBKDF2 前限流且并发原子更新。
- 仅信任明确配置的代理/回环地址。socket peer 不可信就忽略 Forwarded/X-Forwarded-For；多跳规则与部署一起测，不能直接取任意头第一个 IP。

### 6.3 SSE 与工作步骤

POST 流线格式保持 `data: <JSON>\n\n`，不能只使用 SSE event 字段替代 JSON 内 event。保留 text/event-stream、Cache-Control:no-cache、X-Accel-Buffering:no 和 `: keepalive\n\n`。

保留 queued/started/retrieving/generating/stage/delta/citations/metrics/expand_request/done/error，字段见 `frontend/src/types.ts`、`backend/app/metrics.py`。

阶段名固定为 analyzing/embedding/searching/reading/versions/composing/verifying，由实际操作触发。工具失败发相应 stage 的 `status: "failed"`；没检索就不显示检索，不输出内部思维或虚构进度。

允许消除旧实现重复 started 和重复终止错误，但须测试一次有效 started、一次语义终止。成功保留 citations 和校验后的 done.text；最终正文不保证等于所有中间 delta 拼接，保留前端最终替换逻辑。expand_request 后不发成功 done；取消/错误后不再 delta 或执行下一工具。

## 7. 检索、范围及 Agent 等价性

### 7.1 中文分词是迁移门槛

jieba-rs 提供搜索模式，但不能仅凭函数同名断言与 Python jieba 等价。[R6]

1. 固定当前 Python jieba 版本、默认字典、HMM 配置及哈希，在合成中文、专名、学院、年份、中英混排、标点、未知词和长文本上导出搜索分词序列及 FTS MATCH 表达式。
2. Rust 必须在 golden fixtures 上逐 token 对齐，包括顺序、重复 token 的处理。查询保留去重、最多 24 个安全 token、双引号包装和 OR 语义；索引写入保留空格连接。
3. 在同一已有 FTS5 数据库上验证 Rust 查询，再验证 Rust 新发布的 FTS body 可由 Python 查询。不能只测空库内“Rust 写、Rust 读”。
4. 差异优先通过同字典/同 HMM/兼容适配消除。不得偷偷改用按字切分、英文 tokenizer、删除分词或改掉旧 FTS。若仍不能对齐，此阶段失败；提交差异和另行审批的索引迁移方案，不继续宣称直接替换兼容。
5. 字典全局初始化一次，预热内存测试必须包含它。不要通过延迟到第一次真实问题才加载而美化空闲内存。

### 7.2 范围必须由服务端约束

- 默认只查 `deactivated_kind=''`；明确往年可加 `superseded`；`manual` 永不参与问答检索和问答工具原文读取。
- 历史引用的 HTTP 阅读和下载仍可访问已发布的停用版本；它与 Agent 的范围限制是不同路径，不能混淆。
- 文档级/章节级领域、明确文件列表、适用学院和入学年份都要同时约束 FTS、向量及工具原文。章节级只取完整落在授权章节中的 chunk。
- 指定领域必须先查该领域，扩展需要先前查询记录及非空 reason；明确文件只能在用户确认本轮扩展后放开。确认不永久改浏览器设置。
- 执行队列任务时及查询 embedding 返回后重新验证文档状态；扫描前形成短期一致快照，返回证据前复核并发停用。状态变化不能泄漏已禁用证据；必要时移除并补足有界候选或明确无结果。
- `audience_scope.confirmed` 的条件及未确认标签的保守识别保持 `audience.py` 语义；未知标签、缺学院/年份要澄清，不自行猜测。
- 不将领域选择器当权限系统。用户 HTTP 目录显示现行资料，管理员目录含全部；草稿不能进入用户端或问答。

### 7.3 排名与证据

保留 FTS5 bm25、向量点积和 RRF：两分支各取 `top_k * 3`，默认 24；融合 `score += 1 / (60 + rank + 1)`，rank 从 0 开始，最终 top 8。FTS 排序须遵守现有 bm25 分值方向。[R10]

用固定向量和固定上游工具调用对照，不比较随机模型生成文案。无近似同分时证据 ID 对应的 `doc_uid`、行区间、候选顺序必须相同。float32 点积允许绝对误差 `1e-5`；近似同分候选可重排但不能越权、丢失高于边界容差的结果。

为新实现制定稳定 tie-break：向量同分按 package_id/row_index，FTS 同分按 chunk_id，RRF 同分按 chunk_id；记录旧 `argpartition` 同分不稳定造成的白名单差异。不能把所有召回不一致归为浮点误差。向量分支的有界全局 Top-K 必须通过等价测试。

证据编号仍是本轮 `EV1`、`EV2` 等，正文标记为 `[[EVn]]`；长期追溯依靠 `doc_uid`、doc_hash、行区间和不可变原文，不把历史计划中的“哈希+行号”字样误实现成另一套前端标记。

仅注册本轮允许工具产生的证据；最终正文剔除未知编号，引用包含 `evidence_id/doc_uid/doc_hash/title/line_start/line_end/page/section/quote`。页码、章节和摘录从真实资料生成，不能采用模型自报位置。历史 IndexedDB 不迁移、不清空。

### 7.4 轻量编排与上游协议

用显式状态机替代 LangGraph：准备本轮范围与历史 -> 模型流 -> 受限工具调用 -> 模型流 -> 校验/澄清/扩展/结束。保持系统提示的政策、安全和回答结构语义；没有证据不得解释为“没有规定/处罚”。

- 工具仅 `policy_search`、`read_source`、`get_versions`，不得增加任意 HTTP、shell、文件路径读取或写入工具。工具参数必须校验。
- 最多 3 轮工具，第 4 次模型请求不提供工具；超出工具轮数不继续调用。每轮工具按受控顺序执行，不并发复制多份上下文。
- 继续使用配置的 Gemini 原生 `/v1beta` generateContent/streamGenerateContent 契约，不迁移到另一模型 API。密钥放 `x-goog-api-key` 请求头，不放 URL。
- 保留原始 parts 顺序、每一个 `thoughtSignature`、空文本签名 part、functionCall 的 id，并在 functionResponse 中回传一致 id。不能合并跨签名 part、剥离未知 part 字段或只保留 text/functionCall。[R11]
- 推荐 typed 外层 + 有界原始 part JSON，避免序列化 round-trip 丢失未知字段。原始 part 仅在本轮工具往返期间驻留，禁止发前端或日志；对外 delta 只来自非 thought 文本。
- SSE 解析支持 UTF-8 跨包、CRLF/LF、多行 data、空事件、事件边界跨包、最终 usage、带空文本签名的末尾事件；不能按 TCP chunk 当完整 JSON。
- 损坏 JSON、半截流、整个模型响应没有有效候选、非 2xx、身份不匹配、超时、超限返回脱敏错误。合法的仅 usage/finish 元数据事件不能因缺少正文而判错。不要复制旧实现“跳过坏 JSON 后继续成功”的宽松行为。
- embedding 保留模型、dimensions、预处理身份约束；校验有限数值、长度和范数。环境变量名不变；模拟测试通过依赖注入 URL/客户端，不开放用户自选上游地址。
- 保留模型 120 秒/连接 15 秒、embedding 60 秒、问答 180 秒默认边界；明确排队等待与运行超时的计时差别。不加无界重试，已输出 delta 后不自动重发。
- 保留 token 用量字段及 usage 是否实际报告的布尔值；累计每次模型调用最终用量，缺失不能伪填“已报告 0”。

## 8. 数据、导入、发布与恢复

### 8.1 零格式迁移优先

工作目录继续支持：

```text
DATA_DIR/
  campus.db
  campus.db-wal / campus.db-shm
  files/<doc_hash>.<ext>
  text/<doc_hash>.txt
  vectors/pkg-<package_id>.npy
  vectors/pkg-draft-<package_sha前16位>.npy
  packages/<package_sha>.zip
```

保留 `db.py` 全部业务表、FTS5 表及关联 ID，尤其是 settings、packages、documents、sections、doc_tags、chunks、vector_rows、audit_log。旧数据库默认直接可读写，不用 ORM 自动删表重建。

新资料 doc_uid 使用与基线相同的生成规则或经契约证明等效且 Python 可读的唯一格式；已有 uid/id/hash/行号不能改变。数据库加法变更必须先测 Python 回读，禁止引入会破坏旧版本回滚的必需字段。

NPY 头含形状、dtype、字节序和存储顺序。[R7] 必须识别目标 NPY 头、仅接受项目要求的二维 native float32、验证长度与溢出；拒绝 object/pickle、错误维度、截断和非有限向量。针对目标小端架构验证大端输入的兼容/拒绝，不盲目指针 cast 或执行头中的 Python 表达式。

资料包 `format_version=1` 不变。以校验代码和格式文档共同生成夹具；章节 ID 唯一域、`replaces` 的完整 uid/hash 匹配、范数 `allclose` 的 rtol/atol 都由基线 fixture 固定，发现文档与实现差异先记录，不盲目放宽。

### 8.2 导入和发布安全

- ZIP 包体 200 MiB、解压总量 2 GiB、总压缩比不超过 200、最多 10,002 条目；manifest 8 MiB、单原文 4 MiB、chunks 20,000。请求体上限在 multipart 解析前生效，不能只相信 Content-Length。[R5]
- 拒绝绝对路径、驱动器路径、反斜杠、冒号、空/点/父级组件、重复路径、symlink、非预期特殊文件、白名单外条目、加密或不支持的压缩方式；检查声明大小与实际解压字节。
- 不将未经验证条目直接 extract 到目标路径；目标规范化后必须位于独立暂存目录，防 symlink/TOCTOU。Windows 还拒绝保留设备名和大小写碰撞。
- 校验原文件 SHA-256、文本 SHA-256、标准化行数、每个 chunk 与原文逐字符一致、章节/标签边界、日期、向量连续行和模型身份；验证按块完成。
- 导入仅入草稿；包哈希重复拒绝。元数据修正仅限 `title/department/effective_date/audience/audience_scope/notes/replaces`，不改变分块、原文和向量。
- 发布事务成功前旧版仍可查询；新原文及向量准备完成后才切换现行状态。同 doc_hash 不同 text_sha256 必须拒绝，禁止历史行号漂移。
- 替代目标必须唯一合法，不能分叉或成环；手动停用优先于替代停用。重新启用被替代旧版前须解除关系，解除关系不会自动启用旧版。
- 文件和 DB 双层失败回滚，发布失败保留草稿向量可重试；不能“移动草稿成功、事务失败”后破坏下一次重试。
- 变更路径共享存储许可，HTTP 取消不释放实际工作者持有的许可。已提交任务最终完成或回滚，冲突操作返回 409。
- 目录读取能在长发布期间读取此前已提交状态。恢复等待所有活动读者及检索工作者退出，不能先关数据库再通知工作者。
- 磁盘预检保留至少 128 MiB 余量，并计算上传、解压、最终副本、索引/WAL、备份/回滚共存空间；8GB 告警和单位保持现有 API。逐块复检低空间，清理本次暂存，不删既有资料。

### 8.3 v2 备份双向兼容与恢复

备份继续为 `format=campus-policy-backup`、`version=2`；包含 `backup.json`、快照 `db.sqlite`、files/text/vectors/packages。保留 created_at、counts 及每文件 size/sha256。备份含口令 hash 和 token_secret，必须按敏感管理文件保护；“无 API 密钥”不等于无敏感信息。

备份基于 SQLite backup API 创建一致快照，不能直接复制打开中的 `campus.db`。[R12] 在受控文件锁下固定快照引用的不可变文件；压缩时不长时间阻塞目录读。临时快照/下载文件在成功、断线和失败后清理。

恢复校验至少保留：最多 100,000 条目、解压 10 GiB、backup.json 16 MiB、压缩比 200、全量文件哈希清单、数据库 integrity/foreign_key_check、必需表/字段、包 ZIP、原文行数、FTS/chunk/vector 关联、向量身份和范数、签名密钥。缺包 v1 备份保持拒绝，提示原实例升级后重新导出。

恢复状态机必须明确：

1. 受限上传落盘，在同文件系统私有暂存目录解压与完整验证；验证失败不得切换。
2. 获得维护所有权，拒绝新 `/api` 请求为 503，取消旧问答，最多等待活动请求/真实工作者 30 秒；超时 409 且不切换。
3. 写入并持久化 `<DATA_DIR名称>.restore-in-progress` 标记；关闭 DB 连接、statement、文件缓存和 mmap。
4. 将旧数据目录移到同级唯一 rollback 目录，把完整新目录安装到原位置，重开数据库并检查基本查询；关键文件按平台能力同步。
5. 刷新 token 签发/验证状态，读写成功后清除标记并退出维护。旧目录仅在切换确认成功后清理。
6. 任一步异常尝试还原完整旧目录并重开；回滚失败保留旧/新/失败目录及标记，服务保持失败锁定。重启见标记必须拒绝初始化，不能建空库伪装成功。

验证 Python 导出 -> Rust 恢复，以及 Rust 导出 -> Python 恢复；包含已发布包、草稿、override、版本关系、密码/令牌和恢复后继续发布。ZIP 字节不要求相同，内部有效内容和逻辑必须相容。

## 9. 配置、运行与回滚

- 保留 `config.py` 现有环境变量名和默认业务配置。缺必需模型配置时明确拒绝启动；无效数值不造成无界分配。M0 导出配置契约，不把密钥值写进夹具。
- Rust 可写服务必须显式传入 DATA_DIR，不能默认落到既有正式目录。新增 FRONTEND_DIST 和显式配置文件参数，文档化相对路径基准和环境变量优先级；不依赖二进制位置猜数据目录。
- CHAT_CONCURRENCY 首版仅支持 1，其他值明确启动失败，不复制旧实现“配置更大但实际仍只跑一个”的歧义。提高并发是另行任务。
- 密钥来自环境或显式私有配置文件；不打进二进制、样例、日志、备份或报告。请求内的上游 URL/模型/密钥不得覆盖服务配置；HTTP 重定向不能把鉴权头转发给未授权主机。
- 新增 `scripts/start-local-rust.ps1`，默认回环地址、独立端口和 `.local-acceptance/rust-data`。端口冲突换端口，不结束用户已有进程；不改 Python 试用脚本默认行为。允许显式只读使用私有配置，但不复制/打印其内容。
- 发布物为 Linux release 二进制、依赖许可证/必要运行库清单、分词字典（如外置）、前端 dist 和校验和。构建在本机/CI 完成，不在 1C1G 服务器装编译工具、浏览器或前端开发服务。
- 新增 Rust systemd 模板，保留非特权用户、NoNewPrivileges、明确可写目录和 Restart；加入 LimitCORE=0、测量后的线程/FD 上限。512M 配置仅在资格测试后推荐部署，失败不靠提高 MemoryMax 掩盖。[R14]
- Nginx 继续同源代理，SSE 不缓冲、不缓存。核对 `proxy_request_buffering off` 的实际配置，不只看注释；资料包/restore 的 body 上限分 location 设置并测试。问答请求正文也不能因代理缓冲写入临时文件。[R13]
- 当前 Nginx 示例的 220m 全局限制会挡住更大备份，即使后端允许 10 GiB。新模板必须验证恢复专用上限及 multipart 余量；大恢复成功能力仍受磁盘预检约束，不能只加后端限制便称可用。

### 9.1 切换与回滚演练

1. 隔离目录验证两实现；禁止两个可写实例同时打开同一 DATA_DIR，不用生产请求双写。
2. 切换前停止写入并生成完整 v2 备份，保存 Python release、配置引用及目录状态；私有文件只在授权位置保存，不复制进 git。
3. 先停 Python，确认无遗留任务和可写连接，再用 Rust 打开同格式数据；验证旧 token、目录、旧引用、草稿、备份和授权范围内的问答。
4. 只改服务入口/代理目标，不变域名和浏览器存储键。回滚先停 Rust，再用 Python 读取当前兼容数据，保留 Rust 期间新发布资料。
5. 必须演练 Rust 新导入/发布/改密后切回 Python，不只测只读启动。不兼容写入则禁止上线；旧备份恢复会丢弃切换后变更，须先停写、保存新数据并由用户明确决定，不能自动覆盖。
6. 部署、停服和恢复正式数据是单独授权步骤，交付脚本不等于获准在服务器执行。

## 10. 实施阶段与交付物

所有阶段初始均为待实施。每阶段可有多个模块提交，但每个可运行模块完成即中文本地 commit，不等全部结束。

| 阶段 | 内容 | 出阶段门槛 |
| --- | --- | --- |
| M0 | 冻结基线、合成契约夹具、资源脚本和差异清单 | Python 自动化重跑；26 条路由和 SSE/令牌/文件/配置夹具完整；真实与模拟分离 |
| M1 | Cargo 骨架、配置、受限运行时、鉴权、SQLite 读取、静态前端和指标 | token 双向互认；旧库目录/原文/版本可读；预算初测 |
| M2 | NPY 流式读取、候选游标、分词、FTS、范围、Top-K/RRF | golden 分词/排名、动态停用/章节范围通过；D1 检索内存达标 |
| M3 | 原生上游协议、轻量 Agent、SSE、排队/背压/取消、引用 | 模拟签名/多工具/损坏流/慢客户端通过；旧前端直连 Rust 问答 |
| M4 | 流式包校验、草稿修正、事务发布、版本与并发读 | 恶意包/低磁盘/发布重试/取消不解锁通过；200 MiB 级测试 |
| M5 | v2 备份恢复、维护门、故障标记和回滚 | 双向恢复、新写入后回退 Python、故障注入全部通过 |
| M6 | 1C1G 对照、浸泡、D2/D3 容量、发布脚本和人工清单 | 功能/安全/内存/回滚四门通过；未授权的真实测试清楚标为待验收 |

M2、M4 尽早测内存，不等接口全完成才发现字典/导入超预算。没有达标数据时继续定位或提交具体阻塞证据，不用理论推断代替结果。

实现方交付：

- `backend-rust/` 完整实现、锁文件、工具链和 Windows/Ubuntu 可重复构建说明。
- `docs/Rust重构契约与差异.md`：路由/字段快照，token/NPY/分词/排序/错误差异及决定。
- `docs/Rust重构验收记录.md`：逐阶段命令、实际结果、commit、失败/待验收，不混用旧勾选。
- `docs/Rust内存基准报告.md`：匿名内存/RSS/cgroup/缓存/CPU/延迟/线程/FD/磁盘，每场景三次结果、相对收益算法和未达标项。
- 可参数化对照/资源脚本、合成数据生成器、Rust HTTP 浏览器集成及故障注入测试。
- 独立启动/部署/回滚脚本，README 和人工清单入口；旧 Python 路径继续可用。
- 原始 CSV/JSON/截图在 `.local-acceptance/` 或已有忽略输出目录。只提交无敏感信息摘要与小型合成夹具；新增忽略 backend-rust/target，不忽略 Cargo.lock。

## 11. 测试矩阵与最终完成定义

### 11.1 必做测试

| 测试组 | 关键断言 |
| --- | --- |
| 契约 | 26 路由；状态/空值/中文文件名；Unicode 字符边界；配置默认值；未知 part 字段保留 |
| 鉴权 | 双向令牌/口令，过期/错角色/签名，换码不踢旧会话，伪造代理头，原子限流/桶上限 |
| 检索 | 分词 golden；C/Fortran NPY；头溢出/截断/NaN/Inf/零向量；同分；版本/学院/年级/章节/并发停用 |
| 模型 | 原始 part/空签名/函数 id/多工具/末尾 usage；思维不外发；注入资料不变指令；无证据/冲突/确认扩展 |
| SSE | 10 同时到达，11 容量占满，12 受控拒绝；同 client；排队取消；断开；满队列取消；超时；不重复终止 |
| 解析/大小 | 缺/假 Content-Length、chunked 超限、JSON 嵌套、超长事件、慢上传下载、停止消费；有界拒绝而非崩溃 |
| 存储 | 非法 ZIP/重复/大小写碰撞/实际解压超限；哈希/行号；大包/低磁盘；发布回滚可重试；目录可读 |
| 恢复 | v2 双向；草稿继续发布；快照一致；验证前不切换；rename/reopen/rollback 故障；中止留标记；签名刷新 |
| UI | 原 26 组件/12 模拟浏览器回归；新增直连 Rust HTTP 的浏览器集成，只模拟外部模型，不拦截 `/api` |
| 资源 | D1 同限额对照和 512M 资格；D2 正式规模；D3 上限；1000 轮/2 小时；恢复期间独立采样；FD/线程/缓存有界 |
| 隐私 | 日志/报错/DB/暂存/备份无模型密钥或问答正文/工具结果；关闭 core；备份 hash/token_secret 受鉴权保护 |

队列测试使用屏障和足够慢的模拟响应占住首任务，避免完成过快导致第 12 个不稳定拒绝。token 按限流规则预先准备；X-Client-Id 不能制造新会话。

旧 pytest 模块内单测不会自动测试 Rust。为 `backend/tests/test_*` 建映射表，用 Rust 单测或跨语言黑盒对应每个风险断言；禁止只跑旧 pytest 就写“Rust 81 项通过”。模型 API 随机回答不要求逐字相同，契约和证据范围必须通过。

构建检查入口（实施后确认与实际 package 一致）：

```powershell
cargo fmt --manifest-path backend-rust/Cargo.toml --check
cargo clippy --manifest-path backend-rust/Cargo.toml --all-targets --locked -- -D warnings
cargo test --manifest-path backend-rust/Cargo.toml --locked
cargo build --manifest-path backend-rust/Cargo.toml --release --locked
npm.cmd --prefix frontend test
npm.cmd --prefix frontend run build
npm.cmd --prefix frontend run test:e2e
```

当前尚无 backend-rust，以上不是已执行记录。最终还须运行交付的 HTTP 集成、跨语言对照和 Linux 内存脚本。没有 Linux 环境时完成本机开发与合成测试，明确列出待执行命令和证据缺口，不伪造容量结果。

### 11.2 完成条件

- [ ] 常驻后端能力全部 Rust 实现，服务器无需 Python/Node 常驻；本地 Skill 保留。
- [ ] HTTP、SSE、鉴权、旧数据/引用、包/备份、分词/召回和 UI 契约通过。
- [ ] 有界化限制有测试、明确错误和取消清理，不以成功响应掩盖截断。
- [ ] D1 绝对/相对内存、延迟保护、无 OOM 和稳定性通过，有原始数据。
- [ ] D2 正式规模完成；未授权/无资料/无 Linux 仅标本机完成、部署待验收，不能称生产可用。
- [ ] Rust 新写入后回退 Python、双向恢复通过，无不可逆隐式迁移。
- [ ] 安全/故障/隐私通过；真实上游在授权后复验，不伪造费用和 token。
- [ ] README、plan、Rust 验收记录和部署回滚一致；每模块本地 commit，未 push，无密钥/私有资料提交。
- [ ] 人工清单由用户确认后才允许上线，实施模型不能替用户签署。

## 12. 官方参考

核对日期：2026-09-06。以下资料确认 API 行为边界，不是本项目实测；实施时核对所锁版本，不因文档新增 API 改变兼容范围。方括号 R 编号对应正文技术参考；资源预算属于本项目拟定的验收要求。

- [R1] Linux cgroup v2，内存统计/缓存/峰值/事件：`https://www.kernel.org/doc/html/latest/admin-guide/cgroup-v2.html`。
- [R2] Tokio 阻塞任务线程/排队/取消：`https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html`。
- [R3] Tokio bounded mpsc：`https://docs.rs/tokio/latest/tokio/sync/mpsc/index.html`。
- [R4] reqwest Client 复用/连接池：`https://docs.rs/reqwest/latest/reqwest/struct.Client.html`。
- [R5] Axum body 限制及 Multipart：`https://docs.rs/axum/latest/axum/extract/struct.DefaultBodyLimit.html`；`https://docs.rs/axum/latest/axum/extract/struct.Multipart.html`。直接消费 Body 时不能只依赖 extractor 默认限制。
- [R6] jieba-rs 搜索模式/字典：`https://docs.rs/jieba-rs/latest/jieba_rs/struct.Jieba.html`。
- [R7] NPY 格式及流式读取库：`https://numpy.org/doc/stable/reference/generated/numpy.lib.format.html`；`https://docs.rs/npyz/latest/npyz/`。
- [R8] SQLite 页缓存/WAL/mmap 配置：`https://www.sqlite.org/pragma.html`。
- [R9] memmap2 文件映射安全前提：`https://docs.rs/memmap2/latest/memmap2/struct.MmapOptions.html`。
- [R10] SQLite FTS5/bm25：`https://www.sqlite.org/fts5.html`。
- [R11] Gemini 原生 Generate Content thought signatures：`https://ai.google.dev/gemini-api/docs/generate-content/thought-signatures`。本项目按现有协议兼容，不自动迁移别的 API。
- [R12] rusqlite backup：`https://docs.rs/rusqlite/latest/rusqlite/backup/index.html`。
- [R13] Nginx 请求/响应缓冲：`https://nginx.org/en/docs/http/ngx_http_proxy_module.html`。
- [R14] systemd 资源控制官方文档源码：`https://raw.githubusercontent.com/systemd/systemd/main/man/systemd.resource-control.xml`；实施时以 Ubuntu 安装版本为准。

## 13. 给实施模型的任务说明

```text
请在独立 worktree 中按 AGENTS.md、plan.md 第八节和
docs/Rust低内存重构Spec.md 实施 Rust 后端重构。

主要目标是降低服务器常驻与峰值内存，不是只换路由框架。
保留 React UI、本地 Python 预处理、旧 backend 回滚实现及现有数据/API/SSE 契约。
从 M0 基线与合成契约测试开始，每个可运行模块立即中文本地提交，不 push。
尽早验证 M2 检索内存，再补齐问答、资料管理、备份恢复。

只操作本机独立 .local-acceptance/rust-* 数据目录，不改变当前试用实例、
主仓库数据、私有配置或服务器；不擅自调用付费模型。
不得删减安全校验、检索范围、签名字段或回归断言来降低内存。
不能把旧 Python PASS、模拟 API 浏览器 PASS 或理论估计写成 Rust 生产验收。
分词/数据格式不兼容、内存未达标或需破坏性迁移时，先给证据和影响，
保留现状并更新差异记录，不静默降低门槛。
交付本地提交、运行入口、测试映射、内存报告、回滚步骤和待人工验收项。
```
