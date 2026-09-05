import { expect, test, vi } from "vitest";
import { api, setAuth, streamChat } from "../src/api";

test("download attaches user authorization and keeps filename", async () => {
  setAuth("user-token", "client");
  const fetcher = vi.fn(async () => new Response("file", { status: 200,
    headers: { "Content-Disposition": "attachment; filename*=UTF-8''policy.pdf" } }));
  vi.stubGlobal("fetch", fetcher);
  vi.stubGlobal("URL", Object.assign(URL, { createObjectURL: vi.fn(() => "blob:download"), revokeObjectURL: vi.fn() }));
  let filename = "";
  vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(function () { filename = this.download; });
  await api.downloadSource("doc-1");
  expect(fetcher.mock.calls[0][1]).toMatchObject({ headers: { Authorization: "Bearer user-token" } });
  expect(filename).toBe("policy.pdf");
});

function response(events: object[]) {
  return new Response(new ReadableStream({ start(controller) {
    controller.enqueue(new TextEncoder().encode(events.map(e => `data: ${JSON.stringify(e)}\n\n`).join("")));
    controller.close();
  } }), { status: 200 });
}

test("SSE awaits asynchronous persistence in event order", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => response([{ event: "delta", text: "a" }, { event: "done", text: "a", interrupted: false }])));
  const order: string[] = [];
  await streamChat({}, async ev => { await new Promise(r => setTimeout(r, 5)); order.push(ev.event); }, new AbortController().signal);
  expect(order).toEqual(["delta", "done"]);
});

test("SSE EOF without terminal event is an interruption", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => response([{ event: "delta", text: "a" }])));
  await expect(streamChat({}, () => {}, new AbortController().signal)).rejects.toThrow("中断");
});
