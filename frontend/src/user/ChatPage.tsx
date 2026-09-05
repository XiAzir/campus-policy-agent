import { useEffect, useMemo, useRef, useState } from "react";
import type { CatalogDoc, ChatRecord, Citation, ChatEvent, StoredMessage } from "../types";
import { api, prefsFromStorage, prefsToStorage, streamChat, cancelChat } from "../api";
import { idb, newChat, userMessage, modelMessage } from "../idb";
import type { UserPrefs } from "../types";
import CitationPopup from "./CitationPopup";

const EV_RE = /\[\[(EV\d+)\]\]/g;

function MessageBody({ text, citations, onOpen }: { text: string; citations: Citation[]; onOpen: (c: Citation) => void }) {
  const parts: (string | { ev: Citation })[] = [];
  let last = 0;
  for (const m of text.matchAll(EV_RE)) {
    const before = text.slice(last, m.index);
    if (before) parts.push(before);
    const c = citations.find((x) => x.evidence_id === m[1]);
    if (c) parts.push({ ev: c });
    else parts.push(m[0]);
    last = (m.index ?? 0) + m[0].length;
  }
  if (last < text.length) parts.push(text.slice(last));
  return (
    <div className="msg-body">
      {parts.map((p, i) =>
        typeof p === "string" ? (
          <span key={i}>{p}</span>
        ) : (
          <button key={i} className="cite-chip" onClick={() => onOpen(p.ev)}>
            原文 {p.ev.evidence_id.replace("EV", "")}
          </button>
        )
      )}
    </div>
  );
}

export default function ChatPage() {
  const [chats, setChats] = useState<ChatRecord[]>([]);
  const [current, setCurrent] = useState<ChatRecord | null>(null);
  const [prefs, setPrefs] = useState<UserPrefs>(prefsFromStorage);
  const [catalog, setCatalog] = useState<CatalogDoc[]>([]);
  const [input, setInput] = useState("");
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState("");
  const [activeCitation, setActiveCitation] = useState<Citation | null>(null);
  const [streamText, setStreamText] = useState("");
  const [showSettings, setShowSettings] = useState(false);
  const abortRef = useRef<AbortController | null>(null);
  const busyRef = useRef(false);
  const reqIdRef = useRef<string>("");
  const bottomRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    idb.recoverInterrupted().then(rows => { setChats(rows); setCurrent(rows[0] || null); });
    idb.getPrefs().then((p) => p && setPrefs(p));
    api.catalog().then((r) => setCatalog(r.documents)).catch(() => {});
    const interrupt = () => abortRef.current?.abort();
    const visibility = () => { if (document.hidden) interrupt(); };
    window.addEventListener("pagehide", interrupt);
    window.addEventListener("offline", interrupt);
    document.addEventListener("visibilitychange", visibility);
    return () => {
      interrupt();
      window.removeEventListener("pagehide", interrupt);
      window.removeEventListener("offline", interrupt);
      document.removeEventListener("visibilitychange", visibility);
    };
  }, []);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [current?.messages.length, streamText, status]);

  const domains = useMemo(() => {
    const s = new Set<string>();
    catalog.forEach((d) => Object.keys(d.domains).forEach((t) => s.add(t)));
    return [...s].sort();
  }, [catalog]);

  const saveChat = async (c: ChatRecord) => {
    await idb.putChat(c);
    setChats(await idb.listChats());
  };

  const updatePrefs = (p: Partial<UserPrefs>) => {
    const next = { ...prefs, ...p };
    setPrefs(next);
    prefsToStorage(next);
    idb.putPrefs(next);
  };

  const openChat = async (id: string) => {
    if (busyRef.current) return;
    const c = await idb.getChat(id);
    if (c) setCurrent(c);
  };

  const send = async () => {
    const question = input.trim();
    if (!question || busyRef.current) return;
    if (prefs.scopeMode === "files" && prefs.docUids.length === 0) {
      setStatus("请至少选择一份文件");
      return;
    }
    busyRef.current = true;
    setBusy(true);
    setStatus("连接中…");
    setInput("");
    const chat = current ? structuredClone(current) : newChat();
    const history = chat.messages.filter(m => !m.pending && !m.interrupted).map(m => ({ role: m.role, text: m.text }));
    chat.messages.push(userMessage(question));
    if (chat.title === "新的对话") chat.title = question.slice(0, 18);
    const selectedPrefs = structuredClone(prefs);
    let confirmed = false;
    try {
      await saveChat(chat);
      setCurrent({ ...chat });
      for (;;) {
        const reply = { ...modelMessage("", [], true), pending: true };
        chat.messages.push(reply);
        const persist = async () => {
          chat.updatedAt = Date.now();
          await saveChat(chat);
          setCurrent({ ...chat });
        };
        await persist();
        const ctrl = new AbortController();
        abortRef.current = ctrl;
        reqIdRef.current = "";
        let expansionReason = "";
        let terminal = false;
        const scope = {
          mode: selectedPrefs.scopeMode,
          doc_uids: selectedPrefs.docUids,
          domains: selectedPrefs.domains,
          year_mode: selectedPrefs.yearMode,
          expand_confirmed: confirmed,
        };
        try {
          await streamChat({ question, messages: history, scope,
            profile: { college: selectedPrefs.college, entry_year: selectedPrefs.entryYear } }, async (ev: ChatEvent) => {
            switch (ev.event) {
              case "queued":
                reqIdRef.current = ev.request_id;
                setStatus(`排队中（第 ${ev.position} 位）…`);
                break;
              case "started":
                reqIdRef.current = ev.request_id;
                setStatus("");
                break;
              case "retrieving": setStatus("正在检索资料…"); break;
              case "generating": setStatus(""); break;
              case "delta":
                reply.text += ev.text;
                // Persist before displaying: a closed page can recover every shown delta.
                await persist();
                break;
              case "citations": reply.citations = ev.citations; await persist(); break;
              case "done":
                terminal = true;
                reply.text = ev.text.trim();
                reply.pending = false;
                reply.interrupted = false;
                await persist();
                setStatus("");
                break;
              case "error":
                terminal = true;
                reply.pending = false;
                reply.interrupted = true;
                await persist();
                setStatus(ev.message);
                break;
              case "expand_request":
                terminal = true;
                expansionReason = ev.reason;
                reply.pending = false;
                reply.interrupted = true;
                await persist();
                break;
            }
          }, ctrl.signal);
          if (!terminal) throw new Error("连接提前结束，回答已中断");
        } catch (e) {
          reply.pending = false;
          reply.interrupted = true;
          await persist();
          setStatus((e as Error).name === "AbortError" ? "已中断，部分回答已保留" : e instanceof Error ? e.message : "网络中断");
        } finally {
          if (abortRef.current === ctrl) abortRef.current = null;
        }
        if (!expansionReason || ctrl.signal.aborted || confirmed) break;
        const ok = window.confirm(`需要超出你指定的范围检索：\n${expansionReason}\n\n是否允许本次扩展到全部资料？`);
        if (!ok) { setStatus("未扩展资料范围"); break; }
        confirmed = true;
        chat.messages.pop();
        await persist();
      }
    } catch (e) {
      setStatus(e instanceof Error ? e.message : "本地保存失败");
    } finally {
      busyRef.current = false;
      setBusy(false);
      setStreamText("");
      abortRef.current = null;
    }
  };

  const stop = () => {
    ctrlAbort();
    if (reqIdRef.current) cancelChat(reqIdRef.current);
  };
  const ctrlAbort = () => abortRef.current?.abort();

  const exportData = async () => {
    const blob = await idb.exportAll();
    const a = document.createElement("a");
    a.href = URL.createObjectURL(blob);
    a.download = `政策问答-本地数据-${new Date().toISOString().slice(0, 10)}.json`;
    a.click();
    URL.revokeObjectURL(a.href);
  };

  const importData = async (file: File) => {
    try {
      const n = await idb.importAll(file);
      setChats(await idb.listChats());
      const restored = await idb.getPrefs();
      if (restored) { setPrefs(restored); prefsToStorage(restored); }
      alert(`已导入 ${n} 段对话`);
    } catch (e) {
      alert(e instanceof Error ? e.message : "导入失败");
    }
  };

  const selectedDocs = (uids: string[]) =>
    catalog.filter((d) => uids.includes(d.doc_uid));

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="side-head">
          <strong>班级政策问答</strong>
          <button
            disabled={busy}
            onClick={async () => {
              const c = newChat();
              await saveChat(c);
              setCurrent(c);
            }}
          >
            ＋ 新对话
          </button>
        </div>
        <div className="chat-list">
          {chats.map((c) => (
            <div
              key={c.id}
              className={"chat-item" + (current?.id === c.id ? " active" : "")}
              onClick={() => openChat(c.id)}
            >
              <span className="chat-title">{c.title}</span>
              <button
                className="mini"
                disabled={busy}
                title="删除"
                onClick={async (e) => {
                  e.stopPropagation();
                  await idb.deleteChat(c.id);
                  if (current?.id === c.id) setCurrent(null);
                  setChats(await idb.listChats());
                }}
              >
                ✕
              </button>
            </div>
          ))}
          {chats.length === 0 && <div className="muted pad">暂无历史对话</div>}
        </div>
        <div className="side-foot">
          <button onClick={() => setShowSettings(!showSettings)}>⚙ 个人设置</button>
          <button onClick={exportData}>导出数据</button>
          <label className="button-like">
            导入数据
            <input
              type="file"
              accept="application/json"
              style={{ display: "none" }}
              onChange={(e) => e.target.files?.[0] && importData(e.target.files[0])}
            />
          </label>
          <button
            className="danger"
            onClick={async () => {
              if (!window.confirm("清空全部本地聊天记录？")) return;
              await idb.clearChats();
              setCurrent(null);
              setChats([]);
            }}
          >
            清空记录
          </button>
        </div>
      </aside>

      <main className="main">
        {showSettings && (
          <div className="settings">
            <h3>个人设置</h3>
            <div className="settings-grid">
              <label>
                学院
                <input value={prefs.college} onChange={(e) => updatePrefs({ college: e.target.value })} placeholder="如：信息工程学院" />
              </label>
              <label>
                入学年份
                <input value={prefs.entryYear} onChange={(e) => updatePrefs({ entryYear: e.target.value })} placeholder="如：2024" />
              </label>
            </div>
            <h4>资料来源</h4>
            <div className="scope-row">
              <label>
                <input type="radio" checked={prefs.scopeMode === "auto"} onChange={() => updatePrefs({ scopeMode: "auto" })} />
                自动选择领域
              </label>
              <label>
                <input type="radio" checked={prefs.scopeMode === "domains"} onChange={() => updatePrefs({ scopeMode: "domains" })} />
                指定领域
              </label>
              <label>
                <input type="radio" checked={prefs.scopeMode === "files"} onChange={() => updatePrefs({ scopeMode: "files" })} />
                指定文件
              </label>
            </div>
            {prefs.scopeMode === "domains" && (
              <>
                <div className="scope-row">
                  {domains.map((t) => (
                    <label key={t} className="chip">
                      <input
                        type="checkbox"
                        checked={prefs.domains.includes(t)}
                        onChange={(e) =>
                          updatePrefs({
                            domains: e.target.checked ? [...prefs.domains, t] : prefs.domains.filter((x) => x !== t),
                          })
                        }
                      />
                      {t}
                    </label>
                  ))}
                </div>
                <label className="chip">
                  <input
                    type="checkbox"
                    checked={prefs.yearMode === "past"}
                    onChange={(e) => updatePrefs({ yearMode: e.target.checked ? "past" : "current" })}
                  />
                  包含往年（已被替代的旧版）
                </label>
              </>
            )}
            {prefs.scopeMode === "files" && (
              <div className="file-picker">
                {catalog.map((d) => (
                  <label key={d.doc_uid} className="chip">
                    <input
                      type="checkbox"
                      checked={prefs.docUids.includes(d.doc_uid)}
                      onChange={(e) =>
                        updatePrefs({
                          docUids: e.target.checked ? [...prefs.docUids, d.doc_uid] : prefs.docUids.filter((x) => x !== d.doc_uid),
                        })
                      }
                    />
                    {d.title}
                  </label>
                ))}
              </div>
            )}
          </div>
        )}

        <div className="messages">
          {!current && (
            <div className="empty">
              <h2>问一个政策问题</h2>
              <p className="muted">回答区分结论、适用条件与原文依据；点击"原文"标记可查看行号上下文与原文件。</p>
            </div>
          )}
          {current?.messages.map((m: StoredMessage, i) => (
            <div key={i} className={"msg " + m.role}>
              {m.role === "model" ? (
                <>
                  <MessageBody text={m.text} citations={m.citations} onOpen={setActiveCitation} />
                  {m.citations.length > 0 && (
                    <div className="cite-list">
                      {m.citations.map((c) => (
                        <span key={c.evidence_id} className="cite-ref" onClick={() => setActiveCitation(c)}>
                          {c.title} · 行 {c.line_start}–{c.line_end}
                        </span>
                      ))}
                    </div>
                  )}
                  {m.interrupted && <div className="interrupted">已中断（回答不完整，可重新提问）</div>}
                </>
              ) : (
                <span>{m.text}</span>
              )}
            </div>
          ))}
          {busy && streamText && (
            <div className="msg model">
              <MessageBody text={streamText} citations={[]} onOpen={setActiveCitation} />
            </div>
          )}
          {status && <div className="status">{status}</div>}
          <div ref={bottomRef} />
        </div>

        <form
          className="composer"
          onSubmit={(e) => {
            e.preventDefault();
            send();
          }}
        >
          <textarea
            aria-label="问题"
            value={input}
            onChange={(e) => setInput(e.target.value)}
            placeholder="输入问题…（Enter 发送，Shift+Enter 换行）"
            rows={2}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
                e.preventDefault();
                send();
              }
            }}
          />
          {busy ? (
            <button type="button" className="danger" onClick={stop}>
              取消
            </button>
          ) : (
            <button type="submit" disabled={!input.trim()}>
              发送
            </button>
          )}
        </form>
        {selectedDocs(prefs.docUids).length > 0 && prefs.scopeMode === "files" && (
          <div className="scope-note">已限定 {selectedDocs(prefs.docUids).length} 份文件，超出范围需你确认</div>
        )}
      </main>

      {activeCitation && <CitationPopup citation={activeCitation} onClose={() => setActiveCitation(null)} />}
    </div>
  );
}
