import { expect, test, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import AdminApp from "../src/admin/AdminApp";
import { api } from "../src/api";

vi.mock("../src/api", () => ({ getAdminToken: () => "admin-only", adminLogout: vi.fn(), setAdminAuth: vi.fn(),
  api: { adminPackages: vi.fn(async () => ({ packages: [] })),
    adminDocuments: vi.fn(async () => ({ documents: [{ doc_uid: "new", title: "新版", deactivated_kind: "", replaces_doc_uid: "old" }] })),
    adminVersions: vi.fn(async () => ({ versions: [{ doc_uid: "new", title: "新版", is_current: true }] })),
    adminUnlink: vi.fn(async () => ({ ok: true })),
    adminPackage: vi.fn(), adminPatchMeta: vi.fn(async () => ({ ok: true })),
  }
}));

test("admin-only session can inspect version chain and unlink replacement", async () => {
  vi.spyOn(window, "confirm").mockReturnValue(true);
  render(<AdminApp />);
  await userEvent.click(screen.getByText("资料与版本"));
  await userEvent.click(await screen.findByRole("button", { name: "版本链" }));
  expect(api.adminVersions).toHaveBeenCalledWith("new");
  await userEvent.click(await screen.findByRole("button", { name: "关闭" }));
  await userEvent.click(screen.getByRole("button", { name: "解除替代关系" }));
  await waitFor(() => expect(api.adminUnlink).toHaveBeenCalledWith("new"));
});

test("draft metadata can select replacement and confirmed audience", async () => {
  const document = { doc_hash: "hash", title: "待发布", department: "部门", effective_date: null, notes: "", audience: ["全校"], audience_scope: {}, domains: [], original_filename: "new.txt", doc_type: "txt", replaces_doc_uid: null };
  const pkg = { id: 1, status: "draft", original_filename: "new.zip", documents: [document], size: 100, doc_count: 1, chunk_count: 1 };
  vi.mocked(api.adminPackages).mockResolvedValueOnce({ packages: [pkg] } as never);
  vi.mocked(api.adminPackage).mockResolvedValue(pkg as never);
  render(<AdminApp />);
  await userEvent.click(await screen.findByRole("button", { name: "预览" }));
  await userEvent.click(await screen.findByRole("button", { name: "修正元数据" }));
  await userEvent.selectOptions(screen.getByRole("combobox", { name: "替代旧版" }), "new");
  await userEvent.type(screen.getByRole("textbox", { name: "适用学院" }), "信息工程学院");
  await userEvent.click(screen.getByRole("checkbox"));
  await userEvent.click(screen.getByRole("button", { name: "保存", exact: true }));
  await waitFor(() => expect(api.adminPatchMeta).toHaveBeenCalledWith(1, "hash", expect.objectContaining({ replaces: "new", audience_scope: { confirmed: true, colleges: ["信息工程学院"], entry_years: [] } })));
});
