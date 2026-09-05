import { test, expect } from "@playwright/test";

const doc = { doc_uid: "doc-1", doc_hash: "a".repeat(64), title: "测试政策", doc_type: "txt", department: "测试部门", audience: ["全校"], audience_scope: {}, domains: { "共青团": [""] }, deactivated_kind: "", replaces_doc_uid: null, line_count: 3 };

test("file expansion, citation download and refresh persist", async ({ page }, info) => {
  await page.addInitScript(() => {
    localStorage.setItem("cpa.token", "test-user");
    localStorage.setItem("cpa.prefs", JSON.stringify({ college: "", entryYear: "", scopeMode: "files", domains: [], docUids: ["doc-1"], yearMode: "current" }));
  });
  const scopes: unknown[] = [];
  let downloadedAuth = "";
  await page.route("**/api/**", async route => {
    const path = new URL(route.request().url()).pathname;
    if (path === "/api/catalog") return route.fulfill({ json: { documents: [doc] } });
    if (path.endsWith("/text")) return route.fulfill({ json: { ...doc, lines: ["L1: 测试原文"], line_start: 1, line_end: 1 } });
    if (path.endsWith("/file")) {
      downloadedAuth = route.request().headers()["authorization"];
      return route.fulfill({ body: "original", headers: { "Content-Disposition": "attachment; filename=policy.txt", "Content-Type": "application/octet-stream" } });
    }
    if (path === "/api/chat") {
      scopes.push(route.request().postDataJSON().scope);
      const events = scopes.length === 1 ? [{ event: "expand_request", reason: "需要其他资料" }] : [
        { event: "started", request_id: "request-two" },
        { event: "delta", text: "测试回答 [[EV1]]" },
        { event: "citations", citations: [{ evidence_id: "EV1", doc_uid: "doc-1", doc_hash: doc.doc_hash, title: doc.title, line_start: 1, line_end: 1, page: null, section: null, quote: ["测试原文"] }] },
        { event: "done", text: "测试回答 [[EV1]]", interrupted: false },
      ];
      return route.fulfill({ body: events.map(e => `data: ${JSON.stringify(e)}\n\n`).join(""), contentType: "text/event-stream" });
    }
    return route.fulfill({ json: { ok: true } });
  });
  page.on("dialog", dialog => dialog.accept());
  await page.goto("/");
  await page.getByRole("textbox", { name: "问题" }).fill("测试问题");
  await page.getByRole("button", { name: "发送", exact: true }).click();
  await expect(page.getByRole("button", { name: "原文 1", exact: true })).toBeVisible();
  expect(scopes).toHaveLength(2);
  expect(scopes[1]).toMatchObject({ mode: "files", expand_confirmed: true });
  await page.reload();
  await page.getByRole("button", { name: "原文 1", exact: true }).click();
  const download = page.waitForEvent("download");
  await page.getByRole("button", { name: "下载原文件" }).click();
  expect((await download).suggestedFilename()).toBe("policy.txt");
  expect(downloadedAuth).toBe("Bearer test-user");
  await expect(page.getByText("测试原文").last()).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.screenshot({ path: info.outputPath("citation.png"), fullPage: true });
});

test("admin-only version workflow", async ({ page }, info) => {
  await page.addInitScript(() => localStorage.setItem("cpa.adminToken", "test-admin"));
  let versionAuth = "";
  let unlinked = false;
  await page.route("**/api/**", async route => {
    const path = new URL(route.request().url()).pathname;
    if (path.endsWith("/packages")) return route.fulfill({ json: { packages: [] } });
    if (path.endsWith("/documents")) return route.fulfill({ json: { documents: [{ ...doc, replaces_doc_uid: unlinked ? null : "old" }] } });
    if (path.endsWith("/versions")) {
      versionAuth = route.request().headers()["authorization"];
      return route.fulfill({ json: { versions: [{ ...doc, is_current: true }] } });
    }
    if (path.endsWith("/unlink")) unlinked = true;
    return route.fulfill({ json: { ok: true } });
  });
  page.on("dialog", dialog => dialog.accept());
  await page.goto("/#/admin");
  await page.getByText("资料与版本", { exact: true }).first().click();
  await page.getByRole("button", { name: "版本链" }).click();
  expect(versionAuth).toBe("Bearer test-admin");
  await page.getByRole("button", { name: "关闭", exact: true }).click();
  await page.getByRole("button", { name: "解除替代关系" }).click();
  await expect(page.getByRole("button", { name: "解除替代关系" })).toHaveCount(0);
  await page.screenshot({ path: info.outputPath("admin.png"), fullPage: true });
});
