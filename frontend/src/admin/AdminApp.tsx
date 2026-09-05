import { useEffect, useState } from "react";
import type { AdminPackage, CatalogDoc, VersionInfo } from "../types";
import {
  adminLogout,
  api,
  getAdminToken,
  setAdminAuth,
} from "../api";

export default function AdminApp() {
  const [authed, setAuthed] = useState(!!getAdminToken());
  if (!authed) return <AdminLogin onOk={() => setAuthed(true)} />;
  return <Dashboard onLogout={() => { adminLogout(); setAuthed(false); }} />;
}

function AdminLogin({ onOk }: { onOk: () => void }) {
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError("");
    try {
      const r = await api.adminLogin(password);
      setAdminAuth(r.admin_token);
      onOk();
    } catch (err) {
      setError(err instanceof Error ? err.message : "登录失败");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="center-page">
      <form className="login-card" onSubmit={submit}>
        <h1>管理端</h1>
        <p className="muted">仅管理员使用。请勿外传本页面地址。</p>
        <input
          type="password"
          placeholder="管理员密码"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          autoFocus
          required
        />
        {error && <div className="error">{error}</div>}
        <button type="submit" disabled={busy || !password}>
          {busy ? "验证中…" : "登录"}
        </button>
      </form>
    </div>
  );
}

type Tab = "packages" | "documents" | "settings" | "backup";

function Dashboard({ onLogout }: { onLogout: () => void }) {
  const [tab, setTab] = useState<Tab>("packages");

  return (
    <div className="app admin">
      <aside className="sidebar">
        <div className="side-head">
          <strong>管理控制台</strong>
        </div>
        <div className="chat-list">
          {(
            [
              ["packages", "资料包"],
              ["documents", "资料与版本"],
              ["settings", "设置"],
              ["backup", "备份与恢复"],
            ] as [Tab, string][]
          ).map(([t, label]) => (
            <div key={t} className={"chat-item" + (tab === t ? " active" : "")} onClick={() => setTab(t)}>
              <span className="chat-title">{label}</span>
            </div>
          ))}
        </div>
        <div className="side-foot">
          <button onClick={onLogout}>退出登录</button>
        </div>
      </aside>
      <main className="main admin-main">
        {tab === "packages" && <PackagesTab />}
        {tab === "documents" && <DocumentsTab />}
        {tab === "settings" && <SettingsTab />}
        {tab === "backup" && <BackupTab />}
      </main>
    </div>
  );
}

function PackagesTab() {
  const [packages, setPackages] = useState<AdminPackage[]>([]);
  const [preview, setPreview] = useState<AdminPackage | null>(null);
  const [progress, setProgress] = useState(-1);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");

  const refresh = () => api.adminPackages().then((r) => setPackages(r.packages));
  useEffect(() => {
    refresh().catch((e) => setError(String(e)));
  }, []);

  const upload = async (file: File) => {
    setError("");
    setNotice("");
    setProgress(0);
    try {
      await api.adminUpload(file, setProgress);
      setNotice("上传完成，已生成草稿；请打开预览核对元数据后发布");
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : "上传失败");
    } finally {
      setProgress(-1);
    }
  };

  const publish = async (pkg: AdminPackage) => {
    if (!window.confirm(`确认发布《${pkg.original_filename}》？新版就绪才会切换现行版本。`)) return;
    setError("");
    const replacements: Record<string, string | null> = {};
    for (const d of pkg.documents) {
      replacements[d.doc_hash] = d.replaces_doc_uid;
    }
    try {
      await api.adminPublish(pkg.id, replacements);
      setNotice("发布成功");
      setPreview(null);
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : "发布失败");
    }
  };

  return (
    <div>
      <h2>资料包导入</h2>
      <p className="muted">
        只导入本地预处理 Skill 生成的资料包（zip）。服务器会做安全校验：路径穿越、解压规模、哈希与向量一致性、重复导入均被拒绝。
      </p>
      <input
        type="file"
        accept=".zip"
        onChange={(e) => e.target.files?.[0] && upload(e.target.files[0])}
        disabled={progress >= 0}
      />
      {progress >= 0 && <div className="progress"><div style={{ width: `${progress}%` }} /></div>}
      {notice && <div className="notice">{notice}</div>}
      {error && <div className="error">{error}</div>}

      <table className="table">
        <thead>
          <tr>
            <th>#</th>
            <th>文件</th>
            <th>状态</th>
            <th>资料数</th>
            <th>分块</th>
            <th>大小</th>
            <th>导入时间</th>
            <th>操作</th>
          </tr>
        </thead>
        <tbody>
          {packages.map((p) => (
            <tr key={p.id}>
              <td>{p.id}</td>
              <td>{p.original_filename}</td>
              <td>
                <span className={"badge " + p.status}>{p.status === "draft" ? "草稿" : "已发布"}</span>
              </td>
              <td>{p.doc_count}</td>
              <td>{p.chunk_count}</td>
              <td>{(p.size / 1e6).toFixed(1)} MB</td>
              <td>{p.imported_at?.slice(0, 19).replace("T", " ")}</td>
              <td>
                <button
                  onClick={async () => {
                    const r = await api.adminPackage(p.id);
                    setPreview(r);
                  }}
                >
                  预览
                </button>
                {p.status === "draft" && (
                  <button
                    className="danger"
                    onClick={async () => {
                      if (!window.confirm("丢弃该草稿？")) return;
                      await api.adminDiscard(p.id);
                      await refresh();
                    }}
                  >
                    丢弃
                  </button>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>

      {preview && (
        <div className="modal-backdrop" onClick={() => setPreview(null)}>
          <div className="modal wide" onClick={(e) => e.stopPropagation()}>
            <div className="modal-head">
              <h3>预览：{preview.original_filename}（{preview.status === "draft" ? "草稿" : "已发布"}）</h3>
              <button className="ghost" onClick={() => setPreview(null)}>
                关闭
              </button>
            </div>
            <p className="muted">
              预处理版本 {preview.preprocessing_version} · 向量 {preview.embed_model} [{preview.embed_dim} 维] ·{" "}
              {preview.chunk_count} 分块 {preview.vector_shape ? `(${preview.vector_shape.join("×")})` : ""}
            </p>
            {preview.documents.map((d) => (
              <DocPreview key={d.doc_hash} pkg={preview} doc={d} onChanged={() => api.adminPackage(preview.id).then(setPreview)} />
            ))}
            {preview.status === "draft" && (
              <div className="row-actions">
                <button onClick={() => publish(preview)}>发布全部（{preview.documents.length} 份资料）</button>
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

function DocPreview({ pkg, doc, onChanged }: { pkg: AdminPackage; doc: AdminPackage["documents"][0]; onChanged: () => void }) {
  const [editing, setEditing] = useState(false);
  const [fields, setFields] = useState({
    title: doc.title,
    department: doc.department,
    effective_date: doc.effective_date ?? "",
    notes: doc.notes,
  });
  const [error, setError] = useState("");

  const save = async () => {
    setError("");
    try {
      await api.adminPatchMeta(pkg.id, doc.doc_hash, {
        ...fields,
        effective_date: fields.effective_date || null,
      });
      setEditing(false);
      onChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : "保存失败");
    }
  };

  return (
    <div className="doc-preview">
      <div className="doc-head">
        <strong>{doc.edited ? "✎ " : ""}{doc.title}</strong>
        <span className="muted">
          {doc.original_filename} · {doc.doc_type.toUpperCase()} · {doc.line_count} 行 · {doc.chunk_count} 分块
        </span>
      </div>
      <div className="doc-meta">
        <span>部门：{doc.department}</span>
        <span>生效：{doc.effective_date ?? "未填写"}</span>
        <span>适用：{doc.audience.join("、") || "未填写"}</span>
        <span>
          领域：
          {doc.domains.map((x) => `${x.tag}${x.section_ids.length ? `(§${x.section_ids.length})` : ""}`).join("、") || "未标注"}
        </span>
        <span>替代：{doc.replaces_doc_uid ? `指向 ${doc.replaces_doc_uid.slice(0, 8)}…` : "无"}{doc.replaces_unresolved ? "（目标不存在！发布时将失败）" : ""}</span>
      </div>
      {doc.notes && <div className="muted">备注：{doc.notes}</div>}
      {error && <div className="error">{error}</div>}
      {pkg.status === "draft" && (
        <div className="row-actions">
          {editing ? (
            <>
              <input value={fields.title} onChange={(e) => setFields({ ...fields, title: e.target.value })} placeholder="标题" />
              <input value={fields.department} onChange={(e) => setFields({ ...fields, department: e.target.value })} placeholder="发布部门" />
              <input
                value={fields.effective_date}
                onChange={(e) => setFields({ ...fields, effective_date: e.target.value })}
                placeholder="生效日期 YYYY-MM-DD"
              />
              <input value={fields.notes} onChange={(e) => setFields({ ...fields, notes: e.target.value })} placeholder="备注" />
              <button onClick={save}>保存</button>
              <button className="ghost" onClick={() => setEditing(false)}>
                取消
              </button>
            </>
          ) : (
            <button className="ghost" onClick={() => setEditing(true)}>
              修正元数据
            </button>
          )}
        </div>
      )}
    </div>
  );
}

function DocumentsTab() {
  const [docs, setDocs] = useState<CatalogDoc[]>([]);
  const [history, setHistory] = useState<{ uid: string; versions: VersionInfo[] } | null>(null);
  const [error, setError] = useState("");

  const refresh = () => api.adminDocuments().then((r) => setDocs(r.documents));
  useEffect(() => {
    refresh().catch((e) => setError(String(e)));
  }, []);

  const act = async (fn: () => Promise<unknown>) => {
    setError("");
    try {
      await fn();
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : "操作失败");
    }
  };

  const kindLabel = (k: string) => (k === "" ? "现行" : k === "superseded" ? "替代停用" : "手动停用");

  return (
    <div>
      <h2>资料与版本</h2>
      {error && <div className="error">{error}</div>}
      <table className="table">
        <thead>
          <tr>
            <th>标题</th>
            <th>部门</th>
            <th>生效</th>
            <th>状态</th>
            <th>发布时间</th>
            <th>操作</th>
          </tr>
        </thead>
        <tbody>
          {docs.map((d) => (
            <tr key={d.doc_uid}>
              <td>{d.title}</td>
              <td>{d.department}</td>
              <td>{d.effective_date ?? "—"}</td>
              <td>
                <span className={"badge kind-" + (d.deactivated_kind || "current")}>{kindLabel(d.deactivated_kind)}</span>
              </td>
              <td>{d.published_at?.slice(0, 19).replace("T", " ")}</td>
              <td>
                {d.deactivated_kind !== "manual" && (
                  <button className="danger" onClick={() => act(() => api.adminDeactivate(d.doc_uid))}>
                    停用
                  </button>
                )}
                {d.deactivated_kind !== "" && (
                  <button onClick={() => act(() => api.adminEnable(d.doc_uid))}>启用</button>
                )}
                <button
                  onClick={async () => {
                    const r = await api.versions(d.doc_uid);
                    setHistory({ uid: d.doc_uid, versions: r.versions });
                  }}
                >
                  版本链
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>

      {history && (
        <div className="modal-backdrop" onClick={() => setHistory(null)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <div className="modal-head">
              <h3>版本链</h3>
              <button className="ghost" onClick={() => setHistory(null)}>
                关闭
              </button>
            </div>
            {history.versions.map((v) => (
              <div key={v.doc_uid} className="version-row">
                <strong>{v.is_current ? "●" : "○"} {v.title}</strong>
                <span className="muted">
                  发布 {v.published_at?.slice(0, 10)} · 生效 {v.effective_date ?? "—"} ·{" "}
                  {v.is_current ? "现行" : v.deactivated_kind === "manual" ? "手动停用" : "替代停用"}
                </span>
              </div>
            ))}
            <p className="muted">重新启用已被替代的旧版前，须先在新版上"解除替代关系"（通过新版资料所在行的操作）。历史引用始终可打开。</p>
          </div>
        </div>
      )}
    </div>
  );
}

function SettingsTab() {
  const [code, setCode] = useState("");
  const [oldPw, setOldPw] = useState("");
  const [newPw, setNewPw] = useState("");
  const [status, setStatus] = useState<import("../types").AdminStatus | null>(null);
  const [error, setError] = useState("");
  const [msg, setMsg] = useState("");

  const refresh = () => api.adminStatus().then(setStatus);
  useEffect(() => {
    refresh().catch((e) => setError(String(e)));
  }, []);

  return (
    <div>
      <h2>设置</h2>
      {error && <div className="error">{error}</div>}
      {msg && <div className="notice">{msg}</div>}
      <div className="panel">
        <h3>访问码</h3>
        <p className="muted">更换后旧码立即失效；已登录用户不受影响。</p>
        <div className="row-actions">
          <input value={code} onChange={(e) => setCode(e.target.value)} placeholder="新访问码（≥4 位）" />
          <button
            onClick={async () => {
              setError("");
              try {
                await api.adminSetAccessCode(code);
                setCode("");
                setMsg("访问码已更新，旧码立即失效");
              } catch (e) {
                setError(e instanceof Error ? e.message : "设置失败");
              }
            }}
          >
            更新访问码
          </button>
        </div>
      </div>
      <div className="panel">
        <h3>管理员密码</h3>
        <div className="row-actions">
          <input type="password" value={oldPw} onChange={(e) => setOldPw(e.target.value)} placeholder="当前密码" />
          <input type="password" value={newPw} onChange={(e) => setNewPw(e.target.value)} placeholder="新密码（≥8 位）" />
          <button
            onClick={async () => {
              setError("");
              try {
                await api.adminResetPassword(oldPw, newPw);
                setOldPw("");
                setNewPw("");
                setMsg("管理员密码已重置");
              } catch (e) {
                setError(e instanceof Error ? e.message : "重置失败");
              }
            }}
          >
            重置密码
          </button>
        </div>
      </div>
      {status && (
        <div className="panel">
          <h3>运行状态</h3>
          <div className="doc-meta">
            <span>
              磁盘：{status.disk.used_gb} / {status.disk.total_gb} GB（剩 {status.disk.free_gb} GB，阈值 {status.disk.warn_gb} GB）
            </span>
            <span>资料：现行 {status.counts.current} / 共 {status.counts.documents}</span>
            <span>分块：{status.counts.chunks}</span>
            <span>数据目录：{status.data_dir_mb} MB</span>
          </div>
        </div>
      )}
    </div>
  );
}

function BackupTab() {
  const [error, setError] = useState("");
  const [msg, setMsg] = useState("");
  const [busy, setBusy] = useState(false);

  return (
    <div>
      <h2>备份与恢复</h2>
      <p className="muted">
        备份包含资料、版本与管理设置（SQLite 快照方式，不暂停读写）；不含聊天记录与 API 密钥。恢复期间暂停写入，失败自动回滚。
      </p>
      {error && <div className="error">{error}</div>}
      {msg && <div className="notice">{msg}</div>}
      <div className="row-actions">
        <button
          onClick={async () => {
            setError("");
            try {
              const blob = await api.adminBackupBlob();
              const a = document.createElement("a");
              a.href = URL.createObjectURL(blob);
              a.download = `campus-policy-backup-${new Date().toISOString().slice(0, 10)}.zip`;
              a.click();
              URL.revokeObjectURL(a.href);
            } catch (e) {
              setError(e instanceof Error ? e.message : "备份失败");
            }
          }}
        >
          下载备份
        </button>
        <label className="button-like">
          {busy ? "恢复中…" : "上传备份并恢复"}
          <input
            type="file"
            accept=".zip"
            style={{ display: "none" }}
            onChange={async (e) => {
              const f = e.target.files?.[0];
              if (!f) return;
              if (!window.confirm("恢复将覆盖当前全部资料与管理设置，且不可撤销。确认继续？")) return;
              setBusy(true);
              setError("");
              try {
                const r = await api.adminRestore(f);
                setMsg(`恢复完成：${JSON.stringify(r.summary)}`);
              } catch (err) {
                setError(err instanceof Error ? err.message : "恢复失败");
              } finally {
                setBusy(false);
              }
            }}
          />
        </label>
      </div>
    </div>
  );
}
