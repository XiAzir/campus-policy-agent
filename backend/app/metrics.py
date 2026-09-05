"""Per-request numeric telemetry; no prompts, answers, tools or credentials."""

from contextvars import ContextVar
from dataclasses import asdict, dataclass


@dataclass
class TurnMetrics:
    model_calls: int = 0
    embedding_calls: int = 0
    prompt_tokens: int = 0
    output_tokens: int = 0
    thought_tokens: int = 0
    embedding_tokens: int = 0
    model_usage_reported: bool = False
    embedding_usage_reported: bool = False
    retrieval_s: float = 0.0

    def public(self):
        return asdict(self)


current_metrics: ContextVar[TurnMetrics | None] = ContextVar("turn_metrics", default=None)


def call_started(kind):
    metrics = current_metrics.get()
    if metrics:
        name = "model_calls" if kind == "model" else "embedding_calls"
        setattr(metrics, name, getattr(metrics, name) + 1)


def model_usage(data):
    metrics = current_metrics.get()
    if metrics and data:
        metrics.model_usage_reported = True
        metrics.prompt_tokens += int(data.get("promptTokenCount", 0))
        metrics.output_tokens += int(data.get("candidatesTokenCount", 0))
        metrics.thought_tokens += int(data.get("thoughtsTokenCount", 0))
