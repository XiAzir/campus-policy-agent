# Rust 内存与性能基准报告 (M6 本机实测)

> 依照 `docs/Rust低内存重构Spec.md` §3 及 §11 进行测试与记录。
> 测试机环境：Windows 11 + WSL2 (Ubuntu 26.04 Linux 内核 5.15.167.4-microsoft-standard-WSL2)，AMD CPU / 64位架构。
> 工具链：rustc/cargo 1.98.1 release profile，node v20.18.0，python 3.14 / 3.12。

---

## 1. D1 合成数据集规格与基准说明

- **文档数**：100 份标准合成中文高校政策文件
- **分块数**：10,000 个连续文本分块（chunks）
- **向量维度**：1024 维 float32 归一化向量
- **裸向量体积**：10,000 × 1024 × 4 字节 = 39.1 MiB
- **测试程序**：`backend-rust/src/bin/bench_d1.rs`（编译为 release 独立二进制运行）

---

## 2. 测量结果与规范门槛对照表

| 评估指标 | Spec 规定门槛 (验收标准) | Rust Release 实测值 (D1) | 达标判定 | 收益与备注 |
| :--- | :--- | :--- | :---: | :--- |
| **混合检索吞吐速度** | 100 次检索耗时需在合理范围 | **1.60 秒** (平均每次 16.0 ms) | **PASS** | 极速全库扫描 |
| **本地检索 p95 延迟** | $\le 3.0$ 秒 | **0.020 秒 (20 ms)** | **PASS** | 远优于门槛 150 倍 |
| **本地检索 p50 延迟** | 报告中位数指标 | **0.016 秒 (16 ms)** | **PASS** | 极低延迟响应 |
| **本地检索 p99 延迟** | 报告尾部延迟指标 | **0.026 秒 (26 ms)** | **PASS** | 99% 请求在 30ms 内完成 |
| **全库加载后常驻内存 (RSS)** | $\le 160.0$ MiB | **74.57 MiB** | **PASS** | 优于门槛 85 MiB 以上 |
| **100 次混合检索后内存 (RSS)**| $\le 224.0$ MiB | **87.92 MiB** | **PASS** | 优于门槛 136 MiB 以上 |
| **检索阶段内存增量 (RSS Delta)**| 有界保护 | **13.35 MiB** | **PASS** | 无内存泄漏，工作缓存自动受控 |

---

## 3. 前端与跨端集成验证

1. **前端 26 项单元/组件测试**：
   - 运行 `npm.cmd --prefix frontend test`：**26 passed (100% 通过)**。
2. **前端生产打包构建**：
   - 运行 `npm.cmd --prefix frontend run build`：成功产出 `frontend/dist/`（371.72 kB JS, 34.46 kB CSS）。
3. **Rust 后端挂载分发**：
   - Rust 后端通过 `FRONTEND_DIST` 环境变量挂载静态服务，未命中 `/api` 路由时安全 fallback 提供 SPA 前端。

---

## 4. 部署与启动交付物清单

- **本机一键启动脚本**：`scripts/start-local-rust.ps1`（支持独立端口与隔离数据目录，默认端口 8012）。
- **生产 systemd 单元模板**：`deploy/campus-policy-rust.service`（具备 `LimitCORE=0`, `MemoryHigh=384M`, `MemoryMax=512M`, `MemorySwapMax=0`, `NoNewPrivileges=true`）。
- **反向代理 Nginx 示例**：`deploy/nginx-site.conf.example`（追加 `/api/admin/restore` 专用 10GB 上传及缓冲关闭配置）。

---

## 5. 待正式生产环境（1C1G 真实服务器）验收项说明

按照重构规范原则，本机虽已通过全套功能测试、回归测试和 D1 基准测试，以下生产环境指标需在最终上线部署到物理 1C1G 云服务器后进行最后确认：
- [ ] 目标 Ubuntu 24.04 裸机或云主机单核 1GB 内存 cgroup v2 采样（使用 `scripts/rust/sample_memory.py`）。
- [ ] 若有真实未脱敏的 D2 规模私有业务资料，在获得正式授权后于生产环境执行容量测试。
