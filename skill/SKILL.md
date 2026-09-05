---
name: policy-package-preprocess
description: 班级政策问答资料预处理。当管理员要把新的政策文件（PDF/Word/Markdown/TXT）发布到班级政策问答系统时使用：提取文字、生成标准化原文、辅助提取元数据（领域/部门/适用范围/生效时间/版本关系）、构建并校验资料包。不适用于上传或发布操作本身（那在管理端网页完成）。
---

# 政策资料预处理 Skill

目标：把管理员提供的原始文件变成一份能通过服务器校验的资料包（zip），全过程在本地完成，只向服务器上传成品。

## 铁律

1. **绝不改写原文**：标准化原文只做格式标记（页码/章节/表格标记）与空行折叠，不修正错别字、不调整措辞、不翻译。
2. **绝不做 OCR**：无文字层的扫描件在提取阶段即标记退回，如实报告管理员，由管理员决定取舍。
3. **绝不猜测元数据**：领域、发布部门、适用范围、生效时间、替代关系等字段，能从原文直接看出且无歧义的才填写；有疑点逐项询问管理员确认，不合并提问、不替管理员决定。
4. **密钥不落盘不入包**：embedding 密钥从仓库根目录 `.env` 读取，不写进任何输出文件，不打印到日志。

## 工作流程

以下命令均在仓库根目录执行，Python 使用 `.venv/Scripts/python`（Windows）或 `.venv/bin/python`（Linux）。

### 第 1 步：提取标准化原文

```bash
.venv/Scripts/python skill/scripts/extract_text.py -o work <原始文件...>
```

- 输出：`work/standardized/<doc_hash>.txt`（标准化原文）与 `work/extract_report.json`。
- 检查报告：`needs_ocr: true` 的文件必须停下，向管理员说明"该文件无文字层（扫描件），系统不做 OCR"，由管理员决定放弃或另找文字版；其他 warnings 也逐条报告。
- 提取成功后，通读每份标准化原文，为下一步元数据做准备。

### 第 2 步：与管理员确认元数据

为每份文件起草 `work/meta.json`（模板见下）。填写规则：

- `title`：用文件内标题；文件名与内标题不一致时向管理员报告并以管理员意见为准。
- `department`（发布部门）、`effective_date`（生效日期，YYYY-MM-DD 或 null）、`audience`（适用范围标签数组）：原文明确写了才填。
- `domains`：领域标签，从这些标签里选或向管理员提议新标签：`宿管`、`校纪`、`共青团`、`教务`、`资助`、`奖惩`、`安全`、`其他`。`section_ids: []` 表示全文适用；只覆盖部分章节时填写章节 id（章节从标准化原文的 `[[SECTION …]]` 标记与 extract_report 中确认）。
- `sections`（可选）：当正文用"一、二、三"等文字标题而 Word 没有使用标题样式时，自动检测不到章节；此时在 meta 中按 `{"section_id": "s1", "title": "一、申请条件", "start_line": 行号, "end_line": 行号}` 补充（行号以标准化原文为准，1 起），覆盖自动检测结果。
- `replaces`：若本文件是某份现行资料的修订版，填该现行资料的 doc_hash（在管理端资料目录可见），并在报告里明确"发布时需确认替代关系"；不确定就留 null。
- 每一个你不确定或原文未写明的字段，**逐项**询问管理员，得到答复后再写入。

`work/meta.json` 模板：

```json
{
  "preprocessing_version": "v1",
  "documents": [
    {
      "doc_hash": "<extract_report.json 中的 doc_hash>",
      "title": "……",
      "department": "……",
      "effective_date": "2025-09-01",
      "audience": ["全校"],
      "domains": [{"tag": "校纪", "section_ids": []}],
      "replaces": null,
      "notes": ""
    }
  ]
}
```

### 第 3 步：构建资料包

```bash
.venv/Scripts/python skill/scripts/build_package.py --work work --out 资料包-2026-09.zip
```

- 脚本自动完成：复制原文件、分块、调用硅基流动生成向量（1024 维）、写 manifest.json、打 zip。
- 网络失败会自动重试 3 次；仍失败则停止并报告，不要降低维度或换模型。

### 第 4 步：本地校验

```bash
.venv/Scripts/python skill/scripts/validate_package.py 资料包-2026-09.zip
```

必须输出"校验通过"。任何一项失败都要回到对应步骤修正，**不允许把已知不合格的包交给管理员上传**。

### 第 5 步：交付

告诉管理员：资料包路径、包含哪些文件、每份资料的元数据摘要（标题/部门/生效时间/领域/替代关系），提醒管理员在管理端网页上传并在预览页复核元数据后发布。

## 相关文档

- 包格式与校验规则：`docs/资料包格式说明.md`
- 服务器端会再次独立校验，本地通过不代表可以绕过服务器规则
