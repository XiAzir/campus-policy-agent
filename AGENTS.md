# AGENTS.md — 仓库工作约定

本文件对本仓库内所有编码 Agent（Codex、Claude Code、ZCode 等）生效。

## Git 提交纪律（强制）

- **每完成一节代码（一个功能模块、一次可运行的改动），立即 `git add` 并 `git commit`，保存到本地 git 仓库。**
- 不等待、不攒批：一节完成 = 一次 commit，保证任意时点可回滚到"节"粒度。
- commit message 用一行中文说明本节内容，例如：`资料包校验：路径穿越与解压规模检查`。
- 只 commit 到本地仓库，不执行 push，除非明确要求。
- `deploy-config.md`、`.env` 含明文密钥，已列入 `.gitignore`，严禁以任何形式 commit（包括改名副本、粘贴进其他文件）。

## 其他约定

- 实施依据是 `plan.md`；实现与计划冲突时，先修订计划或先说明理由，不留静默偏差。
- 对 `plan.md`、`AGENTS.md` 本身的修订，同样按上述纪律 commit。
