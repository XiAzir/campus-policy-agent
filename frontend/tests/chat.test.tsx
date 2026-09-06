import { beforeEach, expect, test, vi } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import ChatPage from "../src/user/ChatPage";
import { idb, newChat, modelMessage } from "../src/idb";
import { streamChat } from "../src/api";
import type { ChatEvent } from "../src/types";

vi.mock("../src/api", async importOriginal => ({
  ...await importOriginal<typeof import("../src/api")>(),
  api: { catalog: vi.fn(async () => ({ documents: [] })) },
  streamChat: vi.fn(), cancelChat: vi.fn(),
}));

beforeEach(async () => {
  await idb.clearChats();
  await idb.putPrefs({ college: "", entryYear: "", scopeMode: "files", docUids: ["doc-1"], domains: [], yearMode: "current" });
  vi.mocked(streamChat).mockReset();
});

async function start() {
  render(<ChatPage />);
  await new Promise(resolve => setTimeout(resolve, 20));
  await userEvent.type(screen.getByRole("textbox", { name: "问题" }), "问题");
  await userEvent.click(screen.getByRole("button", { name: "发送" }));
}

test("confirmed file expansion uses explicit flag without changing preferences", async () => {
  const bodies: Record<string, unknown>[] = [];
  vi.spyOn(window, "confirm").mockReturnValue(true);
  vi.mocked(streamChat).mockImplementation(async (body, event) => {
    bodies.push(body);
    if (bodies.length === 1) await event({ event: "expand_request", reason: "资料不足" });
    else await event({ event: "done", text: "扩展后的回答", interrupted: false });
  });
  await start();
  await screen.findByText("扩展后的回答");
  expect(bodies).toHaveLength(2);
  expect(bodies[0].scope).toMatchObject({ mode: "files", expand_confirmed: false });
  expect(bodies[1].scope).toMatchObject({ mode: "files", expand_confirmed: true });
  expect((await idb.getPrefs())?.scopeMode).toBe("files");
  expect((await idb.listChats())[0].messages.filter(m => m.role === "user")).toHaveLength(1);
});

test("refusing expansion does not send second request", async () => {
  vi.spyOn(window, "confirm").mockReturnValue(false);
  vi.mocked(streamChat).mockImplementation(async (_, event) => { await event({ event: "expand_request", reason: "资料不足" }); });
  await start();
  await screen.findByText("未扩展资料范围");
  expect(streamChat).toHaveBeenCalledTimes(1);
});

test.each(["network", "eof"])("partial answer survives %s", async kind => {
  vi.mocked(streamChat).mockImplementation(async (_, event) => {
    await event({ event: "delta", text: "已经显示的内容" });
    expect((await idb.listChats())[0].messages.at(-1)?.text).toBe("已经显示的内容");
    if (kind === "network") throw new TypeError("网络断开");
  });
  await start();
  await waitFor(async () => {
    const reply = (await idb.listChats())[0].messages.at(-1);
    expect(reply).toMatchObject({ text: "已经显示的内容", interrupted: true, pending: false });
  });
  await waitFor(() => expect(screen.queryByRole("button", { name: "取消" })).toBeNull());
});

test("reopen marks unfinished persisted answer interrupted", async () => {
  const chat = newChat();
  chat.messages.push({ ...modelMessage("关闭前内容", []), pending: true });
  await idb.putChat(chat);
  render(<ChatPage />);
  await screen.findByText("关闭前内容");
  expect((await idb.getChat(chat.id))?.messages[0]).toMatchObject({ pending: false, interrupted: true });
});

test("stage failures persist without inventing another successful step", async () => {
  vi.mocked(streamChat).mockImplementation(async (_, event) => {
    await event({ event: "stage", stage: "embedding" });
    await event({ event: "stage", stage: "embedding", status: "failed" });
    await event({ event: "stage", stage: "composing" });
    await event({ event: "done", text: "暂时无法检索", interrupted: false });
  });
  await start();
  await screen.findByText("暂时无法检索");
  expect((await idb.listChats())[0].messages.at(-1)?.steps).toEqual([
    { stage: "embedding", status: "failed" }, { stage: "composing" },
  ]);
});

test("pagehide aborts the live request", async () => {
  let signal: AbortSignal;
  vi.mocked(streamChat).mockImplementation(async (_, event, abort) => {
    signal = abort;
    await event({ event: "delta", text: "离开前内容" });
    await new Promise((_, reject) => abort.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError"))));
  });
  await start();
  await screen.findByText("离开前内容");
  expect(screen.queryByText("已中断（回答不完整，可重新提问）")).toBeNull();
  expect((screen.getByRole("button", { name: "清空记录" }) as HTMLButtonElement).disabled).toBe(true);
  expect((document.querySelector('input[type="file"]') as HTMLInputElement).disabled).toBe(true);
  fireEvent(window, new Event("pagehide"));
  await waitFor(() => expect(signal.aborted).toBe(true));
  await waitFor(async () => expect((await idb.listChats())[0].messages.at(-1)?.pending).toBe(false));
});

test("rapid preference edits persist as one merged snapshot", async () => {
  render(<ChatPage />);
  await new Promise(resolve => setTimeout(resolve, 20));
  await userEvent.click(screen.getByRole("button", { name: /个人设置/ }));
  await userEvent.type(screen.getByPlaceholderText("如：信息工程学院"), "信息工程学院");
  await userEvent.type(screen.getByPlaceholderText("如：2024"), "2024");
  await userEvent.click(screen.getByText("指定文件", { exact: true }));
  await waitFor(async () => {
    expect(await idb.getPrefs()).toMatchObject({ college: "信息工程学院", entryYear: "2024", scopeMode: "files", docUids: ["doc-1"] });
  });
  expect(JSON.parse(localStorage.getItem("cpa.prefs") || "{}")).toMatchObject({ college: "信息工程学院", entryYear: "2024", scopeMode: "files", docUids: ["doc-1"] });
});
