# Rust 内存与性能基准记录（尚未通过资格验收）

## 历史初测的适用范围

2026-09-07 更正：此前使用 `bench_d1` 独立检索程序的 RSS 判定完整 HTTP 服务达标，证据不足，撤回所有内存 PASS 和“无泄漏”结论。

历史环境为 Windows/WSL Ubuntu 26.04，不是目标 Ubuntu 24.04 的 1C1G。D1 含 100 文档、10,000 分块、1024 维 float32。原记录中的 74.57 MiB 初始 RSS、87.92 MiB 检索后 RSS、20 ms p95 仅属于独立 `bench_d1` 进程的单次初测；本轮没有重新测量这些数值。

该程序没有覆盖 HTTP/SSE、完整 Agent、多轮上下文、鉴权、上传、发布、备份下载和恢复，不能替代 Spec §3、§11 的完整服务验收。100 次检索前后 RSS 差值也不能证明不存在内存泄漏。

## 尚未执行的资格测试

- [ ] 在目标 Ubuntu 24.04、1CPU/1GB、cgroup v2 环境运行完整 release 服务。
- [ ] Python 与 Rust 使用同一数据、模型模拟响应、配置，各执行至少三次对照。
- [ ] D1/D3 冷启动、预热、空闲、问答、排队、导入、发布、备份下载、恢复的独立峰值。
- [ ] 至少 10Hz 外部采样 RSS/HWM、RssAnon/RssFile、cgroup anon/file/kernel/sock、memory.peak、OOM、swap、FD 和实际线程数。
- [ ] 两小时稳定性、1000 次问答、断连、上游故障和慢客户端测试。
- [ ] 经授权的实际 D2 数据容量验收，记录拒绝边界与剩余磁盘余量。
- [ ] 完整服务在 MemoryHigh=384M、MemoryMax=512M、MemorySwapMax=0 下的资格测试，无 OOM、无静默降级。

采样器 `scripts/rust/sample_memory.py` 必须运行在被测 cgroup 外。未能读取的指标不得伪造为零；线程数为计数，不乘 1024。

当前结论：代码回归测试与资源资格测试是两回事。生产部署模板暂不启用未经验证的 512MiB 硬限制，全部资格门槛通过前禁止上线。
