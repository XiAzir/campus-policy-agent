import { BookOpen } from "lucide-react";

export default function Brand({ compact = false }: { compact?: boolean }) {
  return <span className={"brand" + (compact ? " compact" : "")}>
    <span className="brand-mark"><BookOpen size={23} strokeWidth={2.3} /></span>
    {!compact && <span className="brand-word">校园<span>知事</span><small>班级政策问答</small></span>}
  </span>;
}
