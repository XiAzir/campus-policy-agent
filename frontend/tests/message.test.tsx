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
