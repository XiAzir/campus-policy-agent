import { Check, ChevronDown, Circle, LoaderCircle, OctagonPause } from "lucide-react";
import type { WorkStage, WorkStep } from "../types";

const labels: Record<WorkStage, string> = {
  analyzing: "分析问题与资料范围", embedding: "生成查询向量", searching: "检索关键词与向量索引",
  reading: "读取原文上下文", versions: "查询政策版本", composing: "整理依据与生成回答", verifying: "核验引用编号",
};

export default function WorkSteps({ steps, pending, interrupted }: {
  steps: WorkStep[]; pending?: boolean; interrupted?: boolean;
}) {
  if (!steps.length) return null;
  const title = pending ? labels[steps.at(-1)!.stage] : interrupted ? "处理已中断" : "处理完成";
  return <details className="work-steps" open={pending || undefined}>
    <summary><span className={pending ? "step-running" : interrupted ? "interrupted" : "step-complete"}>
      {pending ? <LoaderCircle className="spin" size={15} /> : interrupted ? <OctagonPause size={15} /> : <Check size={15} />}
      <span role={pending ? "status" : undefined}>{title}{pending ? "中" : ""}</span>
    </span><small>{steps.length} 个步骤</small><ChevronDown size={14} /></summary>
    <ol>{steps.map((step, index) => {
      const last = index === steps.length - 1;
      return <li key={index} className={last && pending ? "step-running" : ""}>
        {last && pending ? <LoaderCircle className="spin" size={13} /> : last && interrupted ? <Circle size={13} /> : <Check size={13} />}
        {labels[step.stage]}
      </li>;
    })}</ol>
  </details>;
}
