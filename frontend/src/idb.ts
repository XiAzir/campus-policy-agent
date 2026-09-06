/** 用户本地数据：聊天记录与个人设置（IndexedDB），刷新可恢复，支持导入/导出/清空。 */

import type { ChatRecord, StoredMessage, UserPrefs } from "./types";

const DB_NAME = "cpa-user";
const DB_VERSION = 1;
let prefsWrites: Promise<unknown> = Promise.resolve();

function open(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, DB_VERSION);
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains("chats")) {
        const s = db.createObjectStore("chats", { keyPath: "id" });
        s.createIndex("updatedAt", "updatedAt");
      }
      if (!db.objectStoreNames.contains("prefs")) {
        db.createObjectStore("prefs");
      }
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

function tx<T>(store: string, mode: IDBTransactionMode, fn: (s: IDBObjectStore) => IDBRequest<T>): Promise<T> {
  return open().then(
    (db) =>
      new Promise<T>((resolve, reject) => {
        const t = db.transaction(store, mode);
        const req = fn(t.objectStore(store));
        req.onerror = () => reject(req.error);
        t.oncomplete = () => { resolve(req.result); db.close(); };
        t.onabort = () => { reject(t.error || new Error("本地数据事务已中止")); db.close(); };
      })
  );
}

export const idb = {
  recoverInterrupted: async (): Promise<ChatRecord[]> => {
    const chats = await idb.listChats();
    for (const chat of chats) {
      if (chat.messages.some(m => m.pending)) {
        chat.messages.forEach(m => { if (m.pending) { m.pending = false; m.interrupted = true; } });
        await idb.putChat(chat);
      }
    }
    return chats;
  },
  listChats: (): Promise<ChatRecord[]> =>
    tx<ChatRecord[]>("chats", "readonly", (s) => s.getAll() as IDBRequest<ChatRecord[]>).then((rows) =>
      rows.sort((a, b) => b.updatedAt - a.updatedAt)
    ),
  getChat: (id: string): Promise<ChatRecord | undefined> =>
    tx<ChatRecord | undefined>("chats", "readonly", (s) => s.get(id) as IDBRequest<ChatRecord | undefined>),
  putChat: (c: ChatRecord): Promise<unknown> => tx("chats", "readwrite", (s) => s.put(c)),
  deleteChat: (id: string): Promise<unknown> => tx("chats", "readwrite", (s) => s.delete(id)),
  clearChats: (): Promise<unknown> => tx("chats", "readwrite", (s) => s.clear()),

  getPrefs: (): Promise<UserPrefs | undefined> =>
    prefsWrites.then(() => tx<UserPrefs | undefined>("prefs", "readonly", (s) => s.get("prefs") as IDBRequest<UserPrefs | undefined>)),
  putPrefs: (p: UserPrefs): Promise<unknown> => {
    const snapshot = structuredClone(p);
    prefsWrites = prefsWrites.catch(() => undefined).then(() => tx("prefs", "readwrite", (s) => s.put(snapshot, "prefs")));
    return prefsWrites;
  },

  /** 导出聊天与设置为 JSON（不含访问码/令牌）。 */
  exportAll: async (): Promise<Blob> => {
    const chats = await idb.listChats();
    const prefs = await idb.getPrefs();
    const payload = { format: "cpa-user-export", version: 1, exportedAt: new Date().toISOString(), chats, prefs: prefs || null };
    return new Blob([JSON.stringify(payload, null, 2)], { type: "application/json" });
  },

  /** 导入：合并聊天记录（同 id 覆盖为导入版本），设置覆盖。 */
  importAll: async (file: File): Promise<number> => {
    const text = await file.text();
    const payload = JSON.parse(text);
    if (payload?.format !== "cpa-user-export") throw new Error("不是本系统的导出文件");
    let count = 0;
    for (const chat of payload.chats || []) {
      await idb.putChat(chat);
      count++;
    }
    if (payload.prefs) await idb.putPrefs(payload.prefs);
    return count;
  },
};

export function newChat(): ChatRecord {
  const now = Date.now();
  return { id: crypto.randomUUID(), title: "新的对话", createdAt: now, updatedAt: now, messages: [] };
}

export function userMessage(text: string): StoredMessage {
  return { role: "user", text, citations: [], ts: Date.now() };
}

export function modelMessage(text: string, citations: StoredMessage["citations"], interrupted = false): StoredMessage {
  return { role: "model", text, citations, interrupted, ts: Date.now() };
}
