import type {
  AdminPackage,
  AdminStatus,
  CatalogDoc,
  SourceText,
  UserPrefs,
  VersionInfo,
} from "./types";

const TOKEN_KEY = "cpa.token";
const CLIENT_KEY = "cpa.clientId";
const ADMIN_KEY = "cpa.adminToken";

export function getToken(): string | null {
  return localStorage.getItem(TOKEN_KEY);
}
export function setAuth(token: string, clientId: string) {
  localStorage.setItem(TOKEN_KEY, token);
  localStorage.setItem(CLIENT_KEY, clientId);
}
export function getClientId(): string {
  return localStorage.getItem(CLIENT_KEY) || "anon";
}
export function logout() {
  localStorage.removeItem(TOKEN_KEY);
  localStorage.removeItem(CLIENT_KEY);
}
export function getAdminToken(): string | null {
  return localStorage.getItem(ADMIN_KEY);
}
export function setAdminAuth(token: string) {
  localStorage.setItem(ADMIN_KEY, token);
}
export function adminLogout() {
  localStorage.removeItem(ADMIN_KEY);
}

export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

async function request<T>(path: string, init: RequestInit = {}, auth: "user" | "admin" | null = "user"): Promise<T> {
  const headers: Record<string, string> = { ...(init.headers as Record<string, string>) };
  if (auth === "user" && getToken()) headers["Authorization"] = `Bearer ${getToken()}`;
  if (auth === "admin") {
    const t = getAdminToken();
    if (!t) throw new ApiError(401, "未以管理员身份登录");
    headers["Authorization"] = `Bearer ${t}`;
  }
  const r = await fetch(path, { ...init, headers });
  if (r.status === 204) return undefined as T;
  const text = await r.text();
  let data: unknown = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    data = { detail: text };
  }
  if (!r.ok) {
    const detail = (data as { detail?: string })?.detail || `请求失败（${r.status}）`;
    if (r.status === 401 && auth === "user") logout();
    throw new ApiError(r.status, detail);
  }
  return data as T;
}

export const api = {
  state: () => request<{ access_code_set: boolean }>("/api/auth/state", {}, null),
  login: (code: string) =>
    request<{ token: string; client_id: string }>(
      "/api/auth/login",
      { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ code }) },
      null
    ),
  adminLogin: (password: string) =>
    request<{ admin_token: string }>(
      "/api/auth/admin/login",
      { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ password }) },
      null
    ),
  catalog: () => request<{ documents: CatalogDoc[] }>("/api/catalog"),
  sourceText: (docUid: string, from: number, to: number) =>
    request<SourceText>(`/api/source/${docUid}/text?frm=${from}&to=${to}`),
  sourceFileUrl: (docUid: string) => `/api/source/${docUid}/file`,
  versions: (docUid: string) => request<{ versions: VersionInfo[] }>(`/api/source/${docUid}/versions`),

  adminPackages: () => request<{ packages: AdminPackage[] }>("/api/admin/packages", {}, "admin"),
  adminPackage: (id: number) => request<AdminPackage>(`/api/admin/packages/${id}`, {}, "admin"),
  adminUpload: (file: File, onProgress: (pct: number) => void) =>
    new Promise<{ id: number }>((resolve, reject) => {
      const xhr = new XMLHttpRequest();
      xhr.open("POST", "/api/admin/packages");
      const t = getAdminToken();
      if (t) xhr.setRequestHeader("Authorization", `Bearer ${t}`);
      xhr.upload.onprogress = (e) => e.lengthComputable && onProgress(Math.round((e.loaded / e.total) * 100));
      xhr.onload = () => {
        if (xhr.status === 200) resolve(JSON.parse(xhr.responseText));
        else {
          let detail = `上传失败（${xhr.status}）`;
          try {
            detail = JSON.parse(xhr.responseText).detail || detail;
          } catch {}
          reject(new ApiError(xhr.status, detail));
        }
      };
      xhr.onerror = () => reject(new ApiError(0, "网络错误"));
      const fd = new FormData();
      fd.append("file", file);
      xhr.send(fd);
    }),
  adminPatchMeta: (pkgId: number, docHash: string, fields: Record<string, unknown>) =>
    request<{ ok: boolean }>(
      `/api/admin/packages/${pkgId}/documents/${docHash}`,
      { method: "PATCH", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ fields }) },
      "admin"
    ),
  adminPublish: (pkgId: number, replacements: Record<string, string | null>) =>
    request<{ ok: boolean }>(
      `/api/admin/packages/${pkgId}/publish`,
      { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ replacements }) },
      "admin"
    ),
  adminDiscard: (pkgId: number) => request<{ ok: boolean }>(`/api/admin/packages/${pkgId}`, { method: "DELETE" }, "admin"),
  adminDocuments: () => request<{ documents: CatalogDoc[] }>("/api/admin/documents", {}, "admin"),
  adminDeactivate: (uid: string) => request<{ ok: boolean }>(`/api/admin/documents/${uid}/deactivate`, { method: "POST" }, "admin"),
  adminEnable: (uid: string) => request<{ ok: boolean }>(`/api/admin/documents/${uid}/enable`, { method: "POST" }, "admin"),
  adminUnlink: (uid: string) => request<{ ok: boolean }>(`/api/admin/documents/${uid}/unlink`, { method: "POST" }, "admin"),
  adminSetAccessCode: (code: string) =>
    request<{ ok: boolean }>(
      "/api/admin/access-code",
      { method: "PUT", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ code }) },
      "admin"
    ),
  adminResetPassword: (oldPassword: string, newPassword: string) =>
    request<{ ok: boolean }>(
      "/api/auth/admin/password",
      { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ old_password: oldPassword, new_password: newPassword }) },
      "admin"
    ),
  adminStatus: () => request<AdminStatus>("/api/admin/status", {}, "admin"),
  adminBackupUrl: () => `/api/admin/backup?t=${Date.now()}`,
  adminBackupBlob: async () => {
    const t = getAdminToken();
    const r = await fetch("/api/admin/backup", { headers: { Authorization: `Bearer ${t}` } });
    if (!r.ok) throw new ApiError(r.status, "备份下载失败");
    return r.blob();
  },
  adminRestore: async (file: File) => {
    const t = getAdminToken();
    const fd = new FormData();
    fd.append("file", file);
    const r = await fetch("/api/admin/restore", { method: "POST", headers: { Authorization: `Bearer ${t}` }, body: fd });
    const data = await r.json().catch(() => ({}));
    if (!r.ok) throw new ApiError(r.status, data.detail || "恢复失败");
    return data;
  },
};

export function prefsFromStorage(): UserPrefs {
  try {
    const raw = localStorage.getItem("cpa.prefs");
    if (raw) return { college: "", entryYear: "", scopeMode: "auto", domains: [], docUids: [], yearMode: "current", ...JSON.parse(raw) };
  } catch {}
  return { college: "", entryYear: "", scopeMode: "auto", domains: [], docUids: [], yearMode: "current" };
}

export function prefsToStorage(p: UserPrefs) {
  localStorage.setItem("cpa.prefs", JSON.stringify(p));
}

/** 聊天 SSE：POST + 流式解析 data: 行；abort 触发服务端断线清理。 */
export async function streamChat(
  body: Record<string, unknown>,
  onEvent: (ev: import("./types").ChatEvent) => void,
  signal: AbortSignal
): Promise<void> {
  const r = await fetch("/api/chat", {
    method: "POST",
    headers: { "Content-Type": "application/json", Authorization: `Bearer ${getToken() || ""}` },
    body: JSON.stringify(body),
    signal,
  });
  if (!r.ok || !r.body) {
    let detail = `请求失败（${r.status}）`;
    try {
      detail = (await r.json()).detail || detail;
    } catch {}
    onEvent({ event: "error", message: detail });
    return;
  }
  const reader = r.body.getReader();
  const decoder = new TextDecoder();
  let buf = "";
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    buf += decoder.decode(value, { stream: true });
    let idx;
    while ((idx = buf.indexOf("\n\n")) >= 0) {
      const chunk = buf.slice(0, idx);
      buf = buf.slice(idx + 2);
      for (const line of chunk.split("\n")) {
        if (line.startsWith("data: ")) {
          try {
            onEvent(JSON.parse(line.slice(6)));
          } catch {}
        }
      }
    }
  }
}

export async function cancelChat(requestId: string) {
  try {
    await fetch("/api/chat/cancel", {
      method: "POST",
      headers: { "Content-Type": "application/json", Authorization: `Bearer ${getToken() || ""}` },
      body: JSON.stringify({ request_id: requestId }),
    });
  } catch {}
}
