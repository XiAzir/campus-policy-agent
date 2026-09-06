import type { Element, Root, RootContent } from "hast";

function textContent(node: RootContent): string {
  if (node.type === "text") return node.value;
  if (node.type === "element") return node.children.map(textContent).join("");
  return "";
}

function sectionDepth(node: RootContent): number | null {
  if (node.type !== "element") return null;
  if (/^h[1-6]$/.test(node.tagName)) return Number(node.tagName[1]);
  // The model also uses a standalone bold paragraph as a section title.
  if (node.tagName === "p" && node.children.some(child => child.type === "element" && child.tagName === "strong") &&
      node.children.every(child => child.type === "element" ? child.tagName === "strong" :
        child.type === "text" && /^[\s:：]*$/.test(child.value))) return 7;
  return null;
}

function isEvidenceTitle(node: RootContent): boolean {
  const title = textContent(node).trim().replace(/^(?:[一二三四五六七八九十]+|\d+)[、.．]\s*/, "").replace(/[:：]\s*$/, "");
  return title === "原文依据";
}

export default function rehypeFoldEvidence() {
  return (tree: Root) => {
    const children: RootContent[] = [];
    for (let index = 0; index < tree.children.length; index++) {
      const node = tree.children[index];
      const depth = sectionDepth(node);
      if (depth === null || !isEvidenceTitle(node)) {
        children.push(node);
        continue;
      }
      let end = index + 1;
      for (; end < tree.children.length; end++) {
        const next = tree.children[end];
        const nextDepth = sectionDepth(next);
        if (nextDepth !== null && (nextDepth <= depth || nextDepth === 7)) break;
        if (next.type === "element" && next.tagName === "hr") break;
      }
      const section: Element = {
        type: "element", tagName: "details", properties: { className: ["evidence-section"] },
        children: [
          { type: "element", tagName: "summary", properties: {}, children: [{ type: "text", value: "原文依据" }] },
          { type: "element", tagName: "div", properties: { className: ["evidence-content"] },
            children: tree.children.slice(index + 1, end).filter(child => child.type !== "doctype") },
        ],
      };
      children.push(section);
      index = end - 1;
    }
    tree.children = children;
  };
}
