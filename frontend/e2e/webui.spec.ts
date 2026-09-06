import { test, expect, type Page } from "@playwright/test";

const documents = [
  { doc_uid: "practice", doc_hash: "a".repeat(64), title: "关于开展暑期三下乡社会实践活动的通知", department: "校团委", domains: { 共青团: [] }, effective_date: "2026-06-01" },
  { doc_uid: "housing", doc_hash: "b".repeat(64), title: "学生宿舍日常管理规定", department: "学生工作部", domains: { 校园生活: [] }, effective_date: "2026-05-01" },
  { doc_uid: "awards", doc_hash: "c".repeat(64), title: "本科生奖学金评定办法", department: "学生工作部", domains: { 评奖评优: [] }, effective_date: "2026-04-01" },
].map(doc => ({ ...doc, doc_type: "docx", line_count: 80, audience: ["全校"], audience_scope: { confirmed: true }, deactivated_kind: "", notes: "" }));

const answer = "## 结论\n请在 **6 月 22 日前** 完成报名。[[EV1]]\n\n### 适用条件\n- 面向全体在校学生\n- 以团队为单位提交申请\n\n| 事项 | 时间 | 提交部门 |\n| --- | --- | --- |\n| 团队报名 | 6 月 22 日 | 学院团委 |\n| 材料审核 | 6 月 25 日 | 校团委 |\n\n> 具体安排以发布部门的现行通知为准。\n\n```text\n报名 → 材料审核 → 出发前培训\n```";

async function setup(page: Page, responseText = answer) {
  await page.addInitScript(() => localStorage.setItem("cpa.token", "webui-test"));
  await page.route("**/api/**", async route => {
    const path = new URL(route.request().url()).pathname;
    if (path === "/api/catalog") return route.fulfill({ json: { documents } });
    if (path.endsWith("/text")) return route.fulfill({ json: { ...documents[0], lines: ["L2: 请于6月22日前提交报名材料。"], line_start: 1, line_end: 5 } });
    if (path === "/api/chat") {
      const events = [
        { event: "started", request_id: "webui-request" },
        ...["analyzing", "embedding", "searching", "composing"].map(stage => ({ event: "stage", stage })),
        { event: "delta", text: responseText },
        { event: "stage", stage: "verifying" },
        { event: "citations", citations: [{ evidence_id: "EV1", ...documents[0], line_start: 2, line_end: 4, page: null, section: "报名安排", quote: ["请于6月22日前提交报名材料。"] }] },
        { event: "done", text: responseText, interrupted: false },
      ];
      return route.fulfill({ contentType: "text/event-stream", body: events.map(event => `data: ${JSON.stringify(event)}\n\n`).join("") });
    }
    return route.fulfill({ json: { ok: true } });
  });
}

async function layoutCheck(page: Page) {
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  const brokenImages = await page.locator("img").evaluateAll(images => images.some(image => !(image as HTMLImageElement).complete || !(image as HTMLImageElement).naturalWidth));
  expect(brokenImages).toBe(false);
}

test("bright welcome, catalog filters, settings and mobile navigation", async ({ page }, info) => {
  await setup(page);
  const errors: string[] = [];
  page.on("pageerror", error => errors.push(error.message));
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "校园里的事，问个明白。" })).toBeVisible();
  await expect(page.locator(".library-banner img")).toBeVisible();
  await layoutCheck(page);
  await page.screenshot({ path: info.outputPath("welcome.png"), fullPage: true });
  await page.getByRole("button", { name: "浏览资料库" }).click();
  await expect(page.locator(".document-card")).toHaveCount(3);
  await page.getByRole("textbox", { name: "搜索资料" }).fill("宿舍");
  await expect(page.locator(".document-card")).toHaveCount(1);
  await page.getByRole("button", { name: "基于此文提问" }).click();
  await expect(page.locator(".composer-scope")).toContainText("已选 1 份文件");
  await page.getByRole("button", { name: "打开个人资料" }).click();
  await page.getByPlaceholder("如：信息工程学院").fill("信息工程学院");
  await page.getByPlaceholder("如：2024").fill("2024");
  await layoutCheck(page);
  await page.screenshot({ path: info.outputPath("settings.png"), fullPage: true });
  await page.keyboard.press("Escape");
  await expect(page.getByRole("dialog")).toHaveCount(0);
  if (info.project.name === "mobile") {
    await page.getByRole("button", { name: "打开导航" }).click();
    await expect(page.locator(".chat-sidebar")).toBeVisible();
    await page.screenshot({ path: info.outputPath("navigation.png"), fullPage: true });
    await page.locator(".chat-sidebar").getByRole("button", { name: "政策资料库" }).click();
  } else await page.getByRole("navigation", { name: "主要导航" }).getByRole("button", { name: "资料库" }).click();
  await expect(page.locator(".document-card")).toHaveCount(3);
  await layoutCheck(page);
  await page.screenshot({ path: info.outputPath("catalog.png"), fullPage: true });
  expect(errors).toEqual([]);
});

test("markdown, collapsed sources and real-event step history survive refresh", async ({ page }, info) => {
  await setup(page);
  await page.goto("/");
  await page.getByRole("textbox", { name: "问题" }).fill("三下乡什么时候报名？");
  await page.getByRole("button", { name: "发送", exact: true }).click();
  await expect(page.getByText("处理完成", { exact: true })).toBeVisible();
  await expect(page.getByRole("heading", { name: "结论", exact: true })).toBeVisible();
  await expect(page.getByRole("table")).toBeVisible();
  const card = page.locator(".source-card");
  await expect(card).not.toHaveAttribute("open");
  await expect(page.getByRole("button", { name: "查看完整原文" })).not.toBeVisible();
  await layoutCheck(page);
  await page.screenshot({ path: info.outputPath("answer.png"), fullPage: true });
  await card.locator("summary").click();
  await expect(page.getByRole("button", { name: "查看完整原文" })).toBeVisible();
  await page.getByRole("button", { name: "查看完整原文" }).click();
  await expect(page.getByRole("dialog", { name: "原文追溯" })).toBeVisible();
  await page.keyboard.press("Escape");
  await page.reload();
  await expect(page.locator(".source-card")).not.toHaveAttribute("open");
  await page.locator(".work-steps summary").click();
  await expect(page.locator(".work-steps li")).toHaveCount(5);
  await expect(page.getByText("生成查询向量", { exact: true })).toBeVisible();
  await expect(page.getByText("读取原文上下文", { exact: true })).not.toBeVisible();
  await layoutCheck(page);
  await page.screenshot({ path: info.outputPath("steps.png"), fullPage: true });
});

test("login artwork and validation fit the viewport", async ({ page }, info) => {
  await page.goto("/");
  await expect(page.getByRole("button", { name: "进入", exact: true })).toBeDisabled();
  await expect(page.getByLabel("班级访问码")).toBeVisible();
  await layoutCheck(page);
  await page.screenshot({ path: info.outputPath("login.png"), fullPage: true });
});

test("answer evidence folds independently of source cards and preserves citation access", async ({ page }, info) => {
  await setup(page, "## 结论\n报名截止时间为6月22日。\n\n## 适用条件\n面向全体学生。\n\n---\n\n## 原文依据\n1. 各团队须在系统中完成报名。[[EV1]]\n2. 在截止时间前提交汇总表。\n\n---\n\n## 补充说明\n具体要求以现行通知为准。");
  await page.goto("/");
  await page.getByRole("textbox", { name: "问题" }).fill("三下乡什么时候报名？");
  await page.getByRole("button", { name: "发送", exact: true }).click();
  const fold = page.locator(".evidence-section");
  const sources = page.locator(".source-card");
  await expect(fold.locator("summary")).toBeVisible();
  await expect(fold).not.toHaveAttribute("open");
  await expect(fold.locator("ol")).not.toBeVisible();
  await expect(page.getByText("面向全体学生。", { exact: true })).toBeVisible();
  await expect(page.getByText("具体要求以现行通知为准。", { exact: true })).toBeVisible();
  await layoutCheck(page);
  await page.screenshot({ path: info.outputPath("evidence-collapsed.png"), fullPage: true });
  await fold.locator("summary").focus();
  await page.keyboard.press("Enter");
  await expect(fold.locator("ol")).toBeVisible();
  await expect(sources).not.toHaveAttribute("open");
  await fold.getByRole("button", { name: "原文 1", exact: true }).click();
  await expect(page.getByRole("dialog", { name: "原文追溯" })).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(fold).toHaveAttribute("open");
  await layoutCheck(page);
  await page.screenshot({ path: info.outputPath("evidence-expanded.png"), fullPage: true });
  await fold.locator("summary").click();
  await expect(fold.locator("ol")).not.toBeVisible();
  await sources.locator("summary").click();
  await expect(sources).toHaveAttribute("open");
  await expect(fold).not.toHaveAttribute("open");
  await page.reload();
  await expect(fold).not.toHaveAttribute("open");
  await expect(sources).not.toHaveAttribute("open");
});
