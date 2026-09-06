import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { SKIP, visit } from "unist-util-visit";
import type { Root, RootContent, PhrasingContent } from "mdast";
import type { Citation } from "../types";

export default function MessageBody({ text, citations, onOpen }: {
  text: string; citations: Citation[]; onOpen: (citation: Citation) => void;
}) {
  // Only verified evidence markers become buttons. Code and ordinary links stay literal.
  const remarkCitations = () => (tree: Root) => {
    visit(tree, node => {
      if (node.type === "link" || node.type === "linkReference") return SKIP;
      if (!("children" in node)) return;
      node.children = (node.children as RootContent[]).flatMap<RootContent>(child => {
        if (child.type !== "text") return [child];
        const pieces: PhrasingContent[] = [];
        let offset = 0;
        for (const match of child.value.matchAll(/\[\[(EV\d+)\]\]/g)) {
          const citation = citations.find(c => c.evidence_id === match[1]);
          if (!citation) continue;
          pieces.push({ type: "text", value: child.value.slice(offset, match.index) });
          pieces.push({ type: "link", url: `#source-${citation.evidence_id}`,
            data: { hProperties: { "data-evidence-id": citation.evidence_id } },
            children: [{ type: "text", value: `原文 ${citation.evidence_id.slice(2)}` }] });
          offset = match.index! + match[0].length;
        }
        if (!pieces.length) return [child];
        pieces.push({ type: "text", value: child.value.slice(offset) });
        return pieces;
      }) as typeof node.children;
    });
  };

  return <div className="msg-body markdown-body">
    <Markdown skipHtml remarkPlugins={[remarkGfm, remarkCitations]} components={{
      a: ({ node, href, children }) => {
        const id = node?.properties["data-evidence-id"];
        const citation = citations.find(c => c.evidence_id === id);
        if (citation) return <button className="cite-chip" onClick={() => onOpen(citation)}>{children}</button>;
        if (!href || !/^https?:\/\//i.test(href)) return <span>{children}</span>;
        return <a href={href} target="_blank" rel="noopener noreferrer" referrerPolicy="no-referrer">{children}</a>;
      },
      img: ({ alt }) => <span className="muted">{alt || "图片"}</span>,
      table: ({ children }) => <div className="markdown-table" tabIndex={0}><table>{children}</table></div>,
    }}>{text}</Markdown>
  </div>;
}
