import { useEffect, useMemo, useRef, useState } from "react";
import type { CatalogDoc, ChatRecord, Citation, ChatEvent, StoredMessage } from "../types";
import { api, prefsFromStorage, prefsToStorage, streamChat, cancelChat } from "../api";
import { idb, newChat, userMessage, modelMessage } from "../idb";
import type { UserPrefs } from "../types";
import CitationPopup from "./CitationPopup";
import MessageBody from "./MessageBody";
import CitationCards from "./CitationCards";
import WorkSteps from "./WorkSteps";
import { ArrowDown, ArrowRight, ArrowUp, BookOpen, Check, ChevronRight, CircleHelp, Copy, Download, GraduationCap,
  History, LibraryBig, LogOut, Menu, MessageCircle, Plus, Search, Settings2, SlidersHorizontal, Square, Trash2, Upload, UserRound, X } from "lucide-react";
import Brand from "../ui/Brand";
import Modal from "../ui/Modal";
import CatalogView from "./CatalogView";
import { logout } from "../api";

export default function ChatPage() {
  const [chats, setChats] = useState<ChatRecord[]>([]);
  const [current, setCurrent] = useState<ChatRecord | null>(null);
  const [prefs, setPrefs] = useState<UserPrefs>(prefsFromStorage);
  const [catalog, setCatalog] = useState<CatalogDoc[]>([]);
  const [input, setInput] = useState("");
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState("");
  const [activeCitation, setActiveCitation] = useState<Citation | null>(null);
  const [showSettings, setShowSettings] = useState(false);
  const [view, setView] = useState<"chat" | "catalog">("chat");
  const [mobileNav, setMobileNav] = useState(false);
  const [historyQuery, setHistoryQuery] = useState("");
  const [catalogLoading, setCatalogLoading] = useState(true);
  const [catalogError, setCatalogError] = useState("");
  const [copied, setCopied] = useState<number | null>(null);
  const [awayFromBottom, setAwayFromBottom] = useState(false);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const importRef = useRef<HTMLInputElement>(null);
  const messagesRef = useRef<HTMLDivElement>(null);
  const followRef = useRef(true);
  const abortRef = useRef<AbortController | null>(null);
  const busyRef = useRef(false);
  const reqIdRef = useRef<string>("");
  const bottomRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    idb.recoverInterrupted().then(rows => { setChats(rows); setCurrent(rows[0] || null); });
    if (!localStorage.getItem("cpa.prefs")) {
      idb.getPrefs().then((p) => {
        if (p) { setPrefs(p); prefsToStorage(p); }
      });
    }
    loadCatalog();
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
    if (followRef.current) bottomRef.current?.scrollIntoView({ behavior: "instant" });
  }, [current?.messages.length, current?.messages.at(-1)?.text, status]);

  useEffect(() => {
    if (inputRef.current) {
      inputRef.current.style.height = "auto";
      inputRef.current.style.height = `${Math.min(inputRef.current.scrollHeight, 160)}px`;
    }
  }, [input]);

  const loadCatalog = () => {
    setCatalogLoading(true);
    setCatalogError("");
    api.catalog().then(r => setCatalog(r.documents)).catch(() => setCatalogError("资料目录暂时无法加载"))
      .finally(() => setCatalogLoading(false));
  };

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
    setPrefs(currentPrefs => {
      const next = { ...currentPrefs, ...p };
      prefsToStorage(next);
      void idb.putPrefs(next);
      return next;
    });
  };

  const openChat = async (id: string) => {
    if (busyRef.current) return;
    const c = await idb.getChat(id);
    if (c) { setCurrent(c); setView("chat"); setMobileNav(false); setStatus(""); followRef.current = true; }
  };

  const send = async () => {
    const question = input.trim();
    if (!question || busyRef.current) return;
    if (prefs.scopeMode === "files" && prefs.docUids.length === 0) {
      setStatus("请至少选择一份文件");
      return;
    }
    busyRef.current = true;
    followRef.current = true;
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
        const reply: StoredMessage = { ...modelMessage("", [], true), pending: true, steps: [] };
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
              case "stage":
                reply.steps!.push({ stage: ev.stage });
                setStatus("");
                await persist();
                break;
              case "queued":
                reqIdRef.current = ev.request_id;
                setStatus(`排队中（第 ${ev.position} 位）…`);
                break;
              case "started":
                reqIdRef.current = ev.request_id;
                setStatus("");
                break;
              case "retrieving": if (!reply.steps?.length) setStatus("正在检索资料…"); break;
              case "generating": if (!reply.steps?.length) setStatus("正在处理问题…"); break;
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
                reply.interrupted = ev.interrupted;
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
    if (busyRef.current) return;
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

  const startNew = async () => {
    if (busyRef.current) return;
    const chat = newChat();
    await saveChat(chat);
    setCurrent(chat);
    setView("chat");
    setStatus("");
    setMobileNav(false);
    setInput("");
    inputRef.current?.focus();
  };
  const selectView = (next: "chat" | "catalog") => { setView(next); setMobileNav(false); };
  const ask = (question: string) => { setInput(question); setView("chat"); inputRef.current?.focus(); };
  const scopeLabel = prefs.scopeMode === "auto" ? "自动选择领域" : prefs.scopeMode === "domains" ? "指定领域" : `已选 ${prefs.docUids.length} 份文件`;
  const recommendations = [
    { tag: "社会实践", title: "三下乡什么时候报名？", detail: "报名时间 · 材料准备", icon: GraduationCap, tone: "pink" },
    { tag: "校园生活", title: "学生宿舍有哪些管理规定？", detail: "住宿安排 · 日常管理", icon: BookOpen, tone: "blue" },
    { tag: "评奖评优", title: "奖学金申请需要满足什么条件？", detail: "申请资格 · 评选流程", icon: CircleHelp, tone: "green" },
  ];

  return (
    <div className="chat-shell">
      <a href="#chat-content" className="skip-link">跳到主要内容</a>
      <header className="topbar">
        <button className="icon-button mobile-menu" aria-label="打开导航" onClick={() => setMobileNav(true)}><Menu size={21} /></button>
        <Brand />
        <nav className="top-nav" aria-label="主要导航">
          <button className={view === "chat" ? "active" : ""} onClick={() => selectView("chat")}><MessageCircle size={17} /> 政策问答</button>
          <button className={view === "catalog" ? "active" : ""} onClick={() => selectView("catalog")}><LibraryBig size={17} /> 资料库</button>
        </nav>
        <div className="topbar-end"><span className="campus-tag"><GraduationCap size={16} /> 校园专属</span>
          <button className="avatar user-avatar" title="个人设置" aria-label="打开个人资料" onClick={() => setShowSettings(true)}><UserRound size={18} /></button>
        </div>
      </header>
      <div className="workspace">
      {mobileNav && <button className="nav-scrim" aria-label="关闭导航" onClick={() => setMobileNav(false)} />}
      <aside className={"chat-sidebar" + (mobileNav ? " is-open" : "")} aria-label="对话导航">
        <div className="mobile-side-head"><Brand /><button className="icon-button" aria-label="关闭导航" onClick={() => setMobileNav(false)}><X size={20} /></button></div>
        <button className="new-chat-button" disabled={busy} onClick={startNew}><Plus size={19} /> 开启新对话</button>
        <nav className="side-nav" aria-label="工作区">
          <button className={view === "chat" ? "active" : ""} onClick={() => selectView("chat")}><MessageCircle size={18} /> 政策问答 <ChevronRight size={14} /></button>
          <button className={view === "catalog" ? "active" : ""} onClick={() => selectView("catalog")}><LibraryBig size={18} /> 政策资料库 <span>{catalog.length}</span></button>
        </nav>
        <div className="history-heading"><span><History size={14} /> 最近对话</span><small>{chats.length}</small></div>
        <label className="search-field history-search"><Search size={14} /><input value={historyQuery} onChange={e => setHistoryQuery(e.target.value)} placeholder="搜索历史对话" aria-label="搜索历史对话" /></label>
        <div className="history-list">
          {chats.filter(chat => chat.title.includes(historyQuery.trim())).map((c) => (
            <div key={c.id} className={"history-item" + (current?.id === c.id ? " active" : "")}>
              <button className="history-open" disabled={busy} onClick={() => openChat(c.id)}><MessageCircle size={15} /><span>{c.title}</span></button>
              <button
                className="icon-button history-delete"
                disabled={busy}
                title="删除对话" aria-label={`删除对话：${c.title}`}
                onClick={async (e) => {
                  e.stopPropagation();
                  if (!window.confirm(`删除对话“${c.title}”？`)) return;
                  await idb.deleteChat(c.id);
                  if (current?.id === c.id) setCurrent(null);
                  setChats(await idb.listChats());
                }}
              >
                <Trash2 size={14} />
              </button>
            </div>
          ))}
          {chats.length === 0 && <div className="history-empty"><MessageCircle size={25} /><span>还没有对话记录</span></div>}
          {chats.length > 0 && !chats.some(c => c.title.includes(historyQuery.trim())) && <p className="muted pad">没有匹配的对话</p>}
        </div>
        <div className="sidebar-bottom">
          <button className="profile-button" onClick={() => { setShowSettings(true); setMobileNav(false); }}><span className="avatar user-avatar"><UserRound size={18} /></span>
            <span><strong>{prefs.college || "同学，你好"}</strong><small>{prefs.entryYear ? `${prefs.entryYear} 级` : "个人设置"}</small></span><Settings2 size={17} />
          </button>
          <div className="data-actions">
          <button className="icon-button" title="导出数据" aria-label="导出数据" onClick={exportData}><Download size={17} /></button>
          <button className="icon-button" disabled={busy} title="导入数据" aria-label="导入数据" onClick={() => importRef.current?.click()}><Upload size={17} /></button>
            <input
              ref={importRef}
              type="file"
              disabled={busy}
              accept="application/json"
              hidden
              onChange={(e) => { if (e.target.files?.[0]) void importData(e.target.files[0]); e.target.value = ""; }}
            />
          <button
            className="icon-button danger" title="清空记录" aria-label="清空记录"
            disabled={busy}
            onClick={async () => {
              if (busyRef.current) return;
              if (!window.confirm("清空全部本地聊天记录？")) return;
              await idb.clearChats();
              setCurrent(null);
              setChats([]);
            }}
          >
            <Trash2 size={17} />
          </button>
          <span className="data-divider" />
          <button className="icon-button" disabled={busy} title="退出登录" aria-label="退出登录" onClick={() => { if (window.confirm("退出当前登录？本地记录会保留。")) logout(); }}><LogOut size={17} /></button>
          </div>
        </div>
      </aside>

      <main className="chat-main" id="chat-content">
        <div className="chat-toolbar"><div><span className="online-dot" /><strong>{view === "chat" ? "校园政策助手" : "我的资料库"}</strong><span className="toolbar-subtitle">{view === "chat" ? "有据可循，有问有答" : "现行政策与原文"}</span></div>
          <button className="scope-button" onClick={() => setShowSettings(true)}><SlidersHorizontal size={15} /><span>{scopeLabel}</span></button></div>
        {showSettings && (
          <Modal label="个人设置" onClose={() => setShowSettings(false)}>
          <div className="settings">
            <div className="modal-head"><div><span className="eyebrow">MY PREFERENCES</span><h3>个人设置</h3></div><button className="icon-button" aria-label="关闭" onClick={() => setShowSettings(false)}><X size={20} /></button></div>
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
          </Modal>
        )}
        {view === "catalog" ? <CatalogView documents={catalog} loading={catalogLoading} error={catalogError} onRetry={loadCatalog} onOpen={setActiveCitation}
          onSelect={uid => { updatePrefs({ scopeMode: "files", docUids: [uid] }); setView("chat"); }} /> : <>
        <div className="messages" ref={messagesRef} onScroll={() => {
          const element = messagesRef.current;
          if (element) { const away = element.scrollHeight - element.scrollTop - element.clientHeight > 100; followRef.current = !away; setAwayFromBottom(away); }
        }}>
          {!current?.messages.length && (
            <div className="welcome">
              <div className="welcome-intro"><div className="welcome-symbol"><BookOpen size={34} strokeWidth={1.7} /></div><span className="eyebrow">HELLO, CAMPUS!</span>
                <h1>校园里的事，<span>问个明白。</span></h1><p>你好，同学。今天有什么想了解的？</p></div>
              <div className="section-heading"><h2>从一个问题开始</h2><span>你可能想问</span></div>
              <div className="suggestion-grid">{recommendations.map(item => <button className={`suggestion-card ${item.tone}`} key={item.tag} onClick={() => ask(item.title)}>
                <span className="suggestion-top"><span className="suggestion-icon"><item.icon size={21} /></span><small>{item.tag}</small><ArrowRight size={16} /></span>
                <strong>{item.title}</strong><span className="suggestion-detail">{item.detail}</span>
              </button>)}</div>
              <div className="library-banner"><img src="/images/campus-library.jpg" alt="图书馆里的政策与知识资料" /><div><span className="eyebrow">THE CAMPUS COLLECTION</span><h2>每个答案，都有出处。</h2>
                <p>{catalogLoading ? "正在连接资料库" : catalogError || `${catalog.length} 份现行资料 · ${domains.length} 个政策领域`}</p></div>
                <button onClick={() => setView("catalog")}>浏览资料库 <ArrowRight size={16} /></button></div>
            </div>
          )}
          {current?.messages.map((m: StoredMessage, i) => (
            <article key={`${current.id}-${i}`} className={"message-row " + m.role}>
              <span className={"avatar " + (m.role === "model" ? "assistant-avatar" : "user-avatar")}>{m.role === "model" ? <BookOpen size={19} /> : <UserRound size={18} />}</span>
              <div className="message-content"><div className="message-heading"><strong>{m.role === "model" ? "校园知事" : "我"}</strong>{m.role === "model" && <span className="assistant-tag">政策助手</span>}<time>{new Date(m.ts).toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" })}</time></div>
              <div className={"msg " + m.role}>
              {m.role === "model" ? (
                <>
                  <WorkSteps steps={m.steps || []} pending={m.pending} interrupted={m.interrupted} />
                  <MessageBody text={m.text} citations={m.citations} onOpen={setActiveCitation} />
                  <CitationCards citations={m.citations} onOpen={setActiveCitation} />
                  {m.interrupted && !m.pending && <div className="interrupted">已中断（回答不完整，可重新提问）</div>}
                </>
              ) : (
                <span>{m.text}</span>
              )}
              </div>
              {m.role === "model" && m.text && !m.pending && <div className="message-actions"><button className="icon-button" title={copied === i ? "已复制" : "复制回答"} aria-label={copied === i ? "已复制" : "复制回答"} onClick={async () => {
                try { await navigator.clipboard.writeText(m.text); setCopied(i); } catch { setStatus("复制失败，请检查浏览器权限"); }
              }}>{copied === i ? <Check size={15} /> : <Copy size={15} />}</button><span>{m.citations.length ? `${m.citations.length} 处原文依据` : ""}</span></div>}
              </div>
            </article>
          ))}
          {status && <div className="status" role="status">{status}</div>}
          <div ref={bottomRef} />
        </div>
        <div className="composer-area">
        {awayFromBottom && <button className="scroll-bottom icon-button" aria-label="回到底部" title="回到底部" onClick={() => { followRef.current = true; bottomRef.current?.scrollIntoView({ behavior: "smooth" }); }}><ArrowDown size={18} /></button>}
        <form
          className="composer"
          onSubmit={(e) => {
            e.preventDefault();
            send();
          }}
        >
          <textarea
            ref={inputRef}
            aria-label="问题"
            value={input}
            onChange={(e) => setInput(e.target.value)}
            placeholder="有什么校园政策想了解？"
            rows={2}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
                e.preventDefault();
                send();
              }
            }}
          />
          <div className="composer-bottom"><button type="button" className="composer-scope" onClick={() => setShowSettings(true)}><LibraryBig size={15} /> {scopeLabel}</button>
          <span className="composer-model"><span className="online-dot" /> 政策问答</span>
          {busy ? (
            <button type="button" className="send-button stop-button" title="取消" aria-label="取消" onClick={stop}>
              <Square size={17} fill="currentColor" />
            </button>
          ) : (
            <button className="send-button" title="发送" aria-label="发送" type="submit" disabled={!input.trim()}>
              <ArrowUp size={22} />
            </button>
          )}</div>
        </form>
        <div className="composer-disclaimer">回答仅供参考，具体要求以学校现行文件为准</div>
        {selectedDocs(prefs.docUids).length > 0 && prefs.scopeMode === "files" && (
          <div className="scope-note">已限定 {selectedDocs(prefs.docUids).length} 份文件，超出范围需你确认</div>
        )}
        </div></>}
      </main>
      </div>
      {activeCitation && <CitationPopup citation={activeCitation} onClose={() => setActiveCitation(null)} />}
    </div>
  );
}
