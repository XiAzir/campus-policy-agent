import { useEffect, useState } from "react";
import { Download, FileText, LoaderCircle, X } from "lucide-react";
import Modal from "../ui/Modal";
import type { Citation } from "../types";
import { api } from "../api";

/** 引用弹窗：标准化原文行号 + 上下文 + 对应版本原文件下载。 */
export default function CitationPopup({
  citation,
  onClose,
}: {
  citation: Citation;
  onClose: () => void;
}) {
  const [text, setText] = useState<Awaited<ReturnType<typeof api.sourceText>> | null>(null);
  const [error, setError] = useState("");
  const [downloading, setDownloading] = useState(false);
  const ctxFrom = Math.max(1, citation.line_start - 3);
  const ctxTo = citation.line_end + 3;

  useEffect(() => {
    let active = true;
    setText(null);
    setError("");
    api
      .sourceText(citation.doc_uid, ctxFrom, ctxTo)
      .then(value => { if (active) setText(value); })
      .catch((e) => { if (active) setError(e instanceof Error ? e.message : "读取失败"); });
    return () => { active = false; };
  }, [citation.doc_uid, ctxFrom, ctxTo]);

  return (
    <Modal onClose={onClose} label="原文追溯">
        <div className="modal-head">
          <div>
            <span className="eyebrow"><FileText size={14} /> 原文追溯</span>
            <h3>{citation.title}</h3>
            <div className="muted">
              {citation.section ? `${citation.section} · ` : ""}行 {citation.line_start}–{citation.line_end}
              {citation.page != null ? ` · PDF 第 ${citation.page} 页` : ""}
            </div>
          </div>
          <button className="icon-button" title="关闭" aria-label="关闭" onClick={onClose}>
            <X size={20} />
          </button>
        </div>

        {citation.quote.length > 0 && (
          <div className="quote-box">
            {citation.quote.map((l, i) => (
              <div key={i} className="quote-line">
                {l}
              </div>
            ))}
          </div>
        )}

        <h4 className="context-heading">原文上下文</h4>
        {!text && !error && <div className="loading-inline" role="status"><LoaderCircle size={17} className="spin" /> 正在读取原文</div>}
        {text?.deactivated_kind && <p className="version-notice">历史版本 · {text.deactivated_kind === "manual" ? "已手动停用" : "已被新版替代"}</p>}
        {error && <div className="error">{error}</div>}
        {text && (
          <div className="lines-box">
            {text.lines.map((l, i) => {
              const m = l.match(/^L(\d+): (.*)$/);
              const lineNo = m ? Number(m[1]) : 0;
              const inRange = lineNo >= citation.line_start && lineNo <= citation.line_end;
              return (
                <div key={i} className={"line" + (inRange ? " hl" : "")}>
                  <span className="lineno">{m ? `L${m[1]}` : ""}</span>
                  <span>{m ? m[2] : l}</span>
                </div>
              );
            })}
          </div>
        )}
        <div className="row-actions">
          <button className="primary-button" disabled={downloading} onClick={async () => {
            setDownloading(true);
            try { await api.downloadSource(citation.doc_uid); }
            catch (e) { setError(e instanceof Error ? e.message : "下载失败"); }
            finally { setDownloading(false); }
          }}><Download size={16} />{downloading ? "下载中" : "下载原文件"}</button>
        </div>
    </Modal>
  );
}
