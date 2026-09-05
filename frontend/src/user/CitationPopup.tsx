import { useEffect, useRef, useState } from "react";
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
  const ctxFrom = Math.max(1, citation.line_start - 3);
  const ctxTo = citation.line_end + 3;
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    api
      .sourceText(citation.doc_uid, ctxFrom, ctxTo)
      .then(setText)
      .catch((e) => setError(e instanceof Error ? e.message : "读取失败"));
  }, [citation.doc_uid, ctxFrom, ctxTo]);

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" ref={ref} onClick={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <div>
            <h3>{citation.title}</h3>
            <div className="muted">
              {citation.section ? `${citation.section} · ` : ""}行 {citation.line_start}–{citation.line_end}
              {citation.page != null ? ` · PDF 第 ${citation.page} 页` : ""}
            </div>
          </div>
          <button className="ghost" onClick={onClose}>
            关闭
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

        <h4>标准化原文（含上下文，历史版本仍可打开）</h4>
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
          <a className="button" href={api.sourceFileUrl(citation.doc_uid)} target="_blank" rel="noreferrer">
            下载原文件
          </a>
        </div>
      </div>
    </div>
  );
}
