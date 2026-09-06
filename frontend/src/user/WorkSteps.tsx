import { Check, ChevronDown, Circle, CircleX, LoaderCircle, OctagonPause } from "lucide-react";
import type { WorkStage, WorkStep } from "../types";

const labels: Record<WorkStage, string> = {
  analyzing: "分析问题与资料范围", embedding: "生成查询向量", searching: "检索关键词与向量索引",
  reading: "读取原文上下文", versions: "查询政策版本", composing: "整理依据与生成回答", verifying: "核验引用编号",
};

export default function WorkSteps({ steps, pending, interrupted }: {
  steps: WorkStep[]; pending?: boolean; interrupted?: boolean;
}) {
  steps = steps.filter(step => step && step.stage in labels);
  if (!steps.length) return null;
  const hasFailure = steps.some(step => step.status === "failed");
  const failedNow = pending && steps.at(-1)?.status === "failed";
  const title = failedNow ? "当前步骤未完成" : pending ? `正在${labels[steps.at(-1)!.stage]}` : interrupted ? "处理已中断" : hasFailure ? "处理结束，部分步骤未完成" : "处理完成";
  return <details className="work-steps" open={pending || undefined}>
    <summary><span className={pending && !failedNow ? "step-running" : interrupted || hasFailure ? "interrupted" : "step-complete"}>
      {failedNow || (!pending && hasFailure) ? <CircleX size={15} /> : pending ? <LoaderCircle className="spin" size={15} /> : interrupted ? <OctagonPause size={15} /> : <Check size={15} />}
      <span role={pending ? "status" : undefined}>{title}</span>
    </span><small>{steps.length} 个步骤</small><ChevronDown size={14} /></summary>
    <ol>{steps.map((step, index) => {
      const last = index === steps.length - 1;
      const failed = step.status === "failed";
      return <li key={index} className={failed ? "error" : last && pending ? "step-running" : ""}>
        {failed ? <CircleX size={13} /> : last && pending ? <LoaderCircle className="spin" size={13} /> : last && interrupted ? <Circle size={13} /> : <Check size={13} />}
        {labels[step.stage]}{failed && "（未完成）"}
      </li>;
    })}</ol>
  </details>;
}
