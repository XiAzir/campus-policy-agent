import { ArrowUpRight, ChevronDown, FileText } from "lucide-react";
import type { Citation } from "../types";

export default function CitationCards({ citations, onOpen }: {
  citations: Citation[]; onOpen: (citation: Citation) => void;
}) {
  if (!citations.length) return null;
  return <section className="cite-list" aria-label="原文依据">
    <div className="source-label"><FileText size={14} /> 原文依据 <span>{citations.length}</span></div>
    {citations.map(c => <details className="source-card" key={c.evidence_id}>
      <summary>
        <span className="source-index">{c.evidence_id.slice(2).padStart(2, "0")}</span>
        <span className="source-name"><strong>{c.title}</strong><small>
          {c.section ? `${c.section} · ` : ""}行 {c.line_start}–{c.line_end}
          {c.page != null && ` · PDF 第 ${c.page} 页`}
        </small></span>
        <ChevronDown size={16} className="source-chevron" />
      </summary>
      <div className="source-detail">
        {c.quote.length > 0 && <blockquote>{c.quote.map((line, index) => <p key={index}>{line}</p>)}</blockquote>}
        <button className="text-action" onClick={() => onOpen(c)}>查看完整原文 <ArrowUpRight size={15} /></button>
      </div>
    </details>)}
  </section>;
}
