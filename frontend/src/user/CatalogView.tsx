import { useState } from "react";
import { ArrowUpRight, FileText, LibraryBig, Search } from "lucide-react";
import type { CatalogDoc, Citation } from "../types";

export default function CatalogView({ documents, loading, error, onRetry, onOpen, onSelect }: {
  documents: CatalogDoc[]; loading: boolean; error: string; onRetry: () => void;
  onOpen: (citation: Citation) => void; onSelect: (uid: string) => void;
}) {
  const [query, setQuery] = useState("");
  const [domain, setDomain] = useState("");
  const domains = [...new Set(documents.flatMap(doc => Object.keys(doc.domains)))];
  const filtered = documents.filter(doc => `${doc.title} ${doc.department}`.includes(query.trim()) && (!domain || domain in doc.domains));
  return <div className="catalog-view">
    <div className="view-heading"><div><span className="eyebrow">CAMPUS LIBRARY</span><h1>政策资料库</h1><p>{documents.length} 份现行资料</p></div><LibraryBig size={32} /></div>
    <label className="search-field catalog-search"><Search size={17} /><input aria-label="搜索资料" placeholder="搜索政策名称、发布部门" value={query} onChange={e => setQuery(e.target.value)} /></label>
    <div className="domain-tabs"><button className={!domain ? "selected" : ""} onClick={() => setDomain("")}>全部</button>
      {domains.map(tag => <button key={tag} className={domain === tag ? "selected" : ""} onClick={() => setDomain(tag)}>{tag}</button>)}
    </div>
    {loading ? <p className="muted" role="status">正在读取资料目录…</p> : error ? <div role="alert" className="error">{error} <button onClick={onRetry}>重新加载</button></div> :
      <div className="catalog-grid">{filtered.map(doc => <article className="document-card" key={doc.doc_uid}>
        <div className="document-top"><span className="document-icon"><FileText size={24} /></span><span className="current-badge">现行有效</span></div>
        <h2>{doc.title}</h2><p className="document-meta">{doc.department || "未标注部门"} · {doc.effective_date || "生效日期未标注"}</p>
        <div className="document-tags">{Object.keys(doc.domains).map(tag => <span key={tag}>{tag}</span>)}</div>
        <div className="document-actions"><button className="text-action" onClick={() => onOpen({ evidence_id: "EV0", doc_uid: doc.doc_uid, doc_hash: doc.doc_hash,
          title: doc.title, line_start: 1, line_end: Math.min(doc.line_count, 30), quote: [], page: null, section: null })}>查看原文 <ArrowUpRight size={14} /></button>
          <button className="soft-button" onClick={() => onSelect(doc.doc_uid)}>基于此文提问</button></div>
      </article>)}</div>}
    {!loading && !error && !filtered.length && <div className="view-empty"><LibraryBig size={36} /><h2>{documents.length ? "未找到匹配的资料" : "还没有已发布资料"}</h2></div>}
  </div>;
}
