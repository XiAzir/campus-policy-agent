import { expect, test, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import MessageBody from "../src/user/MessageBody";
import CitationCards from "../src/user/CitationCards";
import WorkSteps from "../src/user/WorkSteps";
import type { Citation } from "../src/types";

const citation: Citation = { evidence_id: "EV1", doc_uid: "doc-1", doc_hash: "hash", title: "社会实践通知",
  line_start: 2, line_end: 3, page: null, section: null, quote: ["报名截止原文"] };

test("renders GFM structure and verified markers without turning code into citations", async () => {
  const onOpen = vi.fn();
  const { container } = render(<MessageBody citations={[citation]} onOpen={onOpen} text={
    "## 结论\n**明确要求** [[EV1]]\n\n- 一项\n- 二项\n\n| 条件 | 内容 |\n| --- | --- |\n| 年级 | 全体 |\n\n> 说明\n\n`[[EV1]]`\n\n```js\nconst x = 1;\n```\n\n[[EV99]]"
  } />);
  expect(screen.getByRole("heading", { name: "结论" })).toBeTruthy();
  expect(container.querySelector("strong")?.textContent).toBe("明确要求");
  expect(screen.getByRole("table")).toBeTruthy();
  expect(container.querySelector("blockquote")).toBeTruthy();
  expect(container.querySelector("pre code")?.textContent).toContain("const x");
  expect(screen.getAllByRole("button", { name: "原文 1" })).toHaveLength(1);
  await userEvent.click(screen.getByRole("button", { name: "原文 1" }));
  expect(onOpen).toHaveBeenCalledWith(citation);
  expect(screen.getByText("[[EV99]]")).toBeTruthy();
});

test("blocks raw HTML, image requests and unsafe links", () => {
  const { container } = render(<MessageBody citations={[citation]} onOpen={vi.fn()} text={
    '<script>alert(1)</script>\n\n<img src="https://tracker.invalid/pixel">\n\n![图](https://tracker.invalid/pixel)\n\n[恶意](javascript:alert%281%29)\n\n[伪造](#source-EV1)\n\n[正常](https://example.org)'
  } />);
  expect(container.querySelector("script, img")).toBeNull();
  expect(screen.queryByRole("button")).toBeNull();
  expect(screen.getAllByRole("link")).toHaveLength(1);
  expect(screen.getByRole("link").getAttribute("rel")).toBe("noopener noreferrer");
});

test("source cards start collapsed and expand to the real source action", async () => {
  const onOpen = vi.fn();
  const { container } = render(<CitationCards citations={[citation]} onOpen={onOpen} />);
  expect(container.querySelector("details")?.open).toBe(false);
  await userEvent.click(screen.getByText("社会实践通知"));
  expect(container.querySelector("details")?.open).toBe(true);
  await userEvent.click(screen.getByRole("button", { name: "查看完整原文" }));
  expect(onOpen).toHaveBeenCalledWith(citation);
});

test("work steps only show received stages and preserve interruption", () => {
  const { rerender, container } = render(<WorkSteps steps={[{ stage: "analyzing" }, { stage: "embedding" }]} pending />);
  expect(screen.getByRole("status").textContent).toBe("正在生成查询向量");
  expect(screen.queryByText("检索关键词与向量索引")).toBeNull();
  rerender(<WorkSteps steps={[{ stage: "analyzing" }, { stage: "embedding" }]} interrupted />);
  expect(screen.getByText("处理已中断")).toBeTruthy();
  expect(container.querySelector("details")?.open).toBe(false);
});

test("failed embedding is not marked complete when the model continues", async () => {
  render(<WorkSteps steps={[{ stage: "embedding", status: "failed" }, { stage: "composing" }]} />);
  expect(screen.getByText("处理结束，部分步骤未完成").parentElement?.className).toBe("interrupted");
  await userEvent.click(screen.getByText("处理结束，部分步骤未完成"));
  expect(screen.getByText("生成查询向量（未完成）").className).toBe("error");
});

test.each(["## 原文依据", "### **原文依据**", "**原文依据**", "**三、原文依据：**"])("folds the answer evidence section headed by %s", async title => {
  const onOpen = vi.fn();
  const { container } = render(<MessageBody citations={[citation]} onOpen={onOpen} text={
    `## 结论\n报名需要提交材料。\n\n${title}\n\n1. 这是折叠的原文条款。[[EV1]]\n2. 提交电子版。\n\n## 后续说明\n后续说明不折叠。`
  } />);
  const section = container.querySelector("details")!;
  expect(section.open).toBe(false);
  expect(section.querySelector("ol")).toBeTruthy();
  expect(section.textContent).not.toContain("后续说明");
  expect(section.textContent).not.toContain("报名需要提交材料。");
  await userEvent.click(section.querySelector("summary")!);
  expect(section.open).toBe(true);
  await userEvent.click(screen.getByRole("button", { name: "原文 1" }));
  expect(onOpen).toHaveBeenCalledWith(citation);
  await userEvent.click(section.querySelector("summary")!);
  expect(section.open).toBe(false);
});

test("keeps subsection markdown inside the fold but leaves the next peer section outside", () => {
  const { container } = render(<MessageBody citations={[]} onOpen={vi.fn()} text={
    "## 原文依据\n### 报名条款\n\n| 内容 | 日期 |\n| --- | --- |\n| 报名 | 六月 |\n\n> 引用文本\n\n```text\n原文代码\n```\n\n## 注意事项\n不折叠的提醒。"
  } />);
  const section = container.querySelector("details")!;
  expect(section.querySelector("h3")?.textContent).toBe("报名条款");
  expect(section.querySelector("table")).toBeTruthy();
  expect(section.querySelector("blockquote")).toBeTruthy();
  expect(section.querySelector("pre code")?.textContent).toContain("原文代码");
  expect(section.textContent).not.toContain("不折叠的提醒");
});

test("stops a bold evidence section at a separator or the next bold title", () => {
  const { container } = render(<MessageBody citations={[]} onOpen={vi.fn()} text={
    "**原文依据**\n\n第一处条款\n\n**补充说明**\n\n说明在外面\n\n**原文依据**\n\n第二处条款\n\n---\n\n分隔线后的说明"
  } />);
  const sections = container.querySelectorAll("details");
  expect(sections).toHaveLength(2);
  expect(sections[0].textContent).toContain("第一处条款");
  expect(sections[0].textContent).not.toContain("说明在外面");
  expect(sections[1].textContent).toContain("第二处条款");
  expect(sections[1].textContent).not.toContain("分隔线后的说明");
});

test("does not fold evidence words in regular prose, code or quoted documents", () => {
  const { container } = render(<MessageBody citations={[]} onOpen={vi.fn()} text={
    "请查看**原文依据**，再提交申请。\n\n```md\n## 原文依据\n不要转换代码\n```\n\n> ## 原文依据\n> 这是被引用的文档标题。"
  } />);
  expect(container.querySelector("details")).toBeNull();
});

test("keeps the same open fold across streaming updates and late citations", async () => {
  const text = "## 结论\n结论保持可见。\n\n## 原文依据\n1. 条款正在到达";
  const { container, rerender } = render(<MessageBody citations={[]} onOpen={vi.fn()} text={text} />);
  const section = container.querySelector("details")!;
  expect(section.open).toBe(false);
  await userEvent.click(section.querySelector("summary")!);
  rerender(<MessageBody citations={[citation]} onOpen={vi.fn()} text={`${text}。[[EV1]]\n2. 后续条款。`} />);
  expect(container.querySelector("details")).toBe(section);
  expect(section.open).toBe(true);
  expect(section.textContent).toContain("后续条款");
  expect(section.querySelector(".cite-chip")).toBeTruthy();
});
