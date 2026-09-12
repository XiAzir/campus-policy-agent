use crate::config::Config;
use crate::metrics::TurnMetrics;
use futures_util::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::Value;
use std::fmt;
use std::time::Duration;

#[derive(Debug)]
pub enum LlmError {
    Unreachable(String),
    BadStatus(u16),
    InvalidResponse(String),
    EmbeddingFailed(String),
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(msg) => write!(f, "模型服务不可达：{}", msg),
            Self::BadStatus(code) => write!(f, "模型调用失败 HTTP {}", code),
            Self::InvalidResponse(msg) => write!(f, "模型响应结构异常：{}", msg),
            Self::EmbeddingFailed(msg) => write!(f, "向量化失败：{}", msg),
        }
    }
}

impl std::error::Error for LlmError {}

// Count serialized bytes without allocating another copy of a potentially large part.
struct JsonBudget(usize);
impl std::io::Write for JsonBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        if self.0 > 2 * 1024 * 1024 {
            return Err(std::io::Error::other("context budget exceeded"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub enum StreamEvent {
    Text(String),
    Parts(Vec<Value>),
}

pub struct GeminiClient {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: String,
}

impl GeminiClient {
    pub fn new(config: &Config) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| panic!("无法初始化主模型 HTTP 客户端，请检查代理与 TLS 配置"));

        Self {
            client,
            base_url: config.gemini_base_url.trim_end_matches('/').to_string(),
            model: config.gemini_model.clone(),
            api_key: config.gemini_api_key.clone(),
        }
    }

    pub fn with_client(
        client: reqwest::Client,
        base_url: String,
        model: String,
        api_key: String,
    ) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            api_key,
        }
    }

    pub async fn stream(
        &self,
        contents: Vec<Value>,
        system: Option<String>,
        tools: Option<Value>,
        temperature: f64,
        metrics: Option<&TurnMetrics>,
        mut on_event: impl FnMut(StreamEvent) -> futures_util::future::BoxFuture<'static, ()>,
    ) -> Result<(), LlmError> {
        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            self.base_url, self.model
        );

        let mut payload = serde_json::json!({
            "contents": contents,
            "generationConfig": {
                "temperature": temperature
            }
        });

        if let Some(sys_text) = system {
            payload["systemInstruction"] = serde_json::json!({
                "parts": [{ "text": sys_text }]
            });
        }
        if let Some(t) = tools {
            payload["tools"] = t;
        }

        if let Some(m) = metrics {
            m.inc_model_calls();
        }

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-goog-api-key",
            self.api_key
                .parse()
                .map_err(|_| LlmError::InvalidResponse("无效的 x-goog-api-key 请求头".into()))?,
        );

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .json(&payload)
            .send()
            .await
            .map_err(|e| LlmError::Unreachable(e.without_url().to_string()))?;

        if !response.status().is_success() {
            return Err(LlmError::BadStatus(response.status().as_u16()));
        }

        let mut byte_stream = response.bytes_stream();
        let mut buffer = Vec::new();
        let mut event_data = String::new();
        let mut total_bytes = 0usize;
        let mut answer_bytes = 0usize;
        let mut collected_parts: Vec<Value> = Vec::new();
        let mut part_budget = JsonBudget(0);
        let mut final_usage: Option<Value> = None;

        while let Some(chunk_res) = byte_stream.next().await {
            let chunk =
                chunk_res.map_err(|e| LlmError::Unreachable(e.without_url().to_string()))?;
            total_bytes = total_bytes.saturating_add(chunk.len());
            if total_bytes > 8 * 1024 * 1024 {
                return Err(LlmError::InvalidResponse("模型流超过 8 MiB".into()));
            }
            buffer.extend_from_slice(&chunk);

            let mut consumed = 0;
            while let Some(relative) = buffer[consumed..].iter().position(|b| *b == b'\n') {
                let idx = consumed + relative;
                if relative > 1024 * 1024 {
                    return Err(LlmError::InvalidResponse("SSE 事件超过 1 MiB".into()));
                }
                let line = std::str::from_utf8(&buffer[consumed..=idx])
                    .map_err(|_| LlmError::InvalidResponse("SSE 包含非法 UTF-8".into()))?;
                consumed = idx + 1;
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if let Some(data) = trimmed.strip_prefix("data:") {
                    event_data.push_str(data.strip_prefix(' ').unwrap_or(data));
                    event_data.push('\n');
                    if event_data.len() > 1024 * 1024 {
                        return Err(LlmError::InvalidResponse("SSE 事件超过 1 MiB".into()));
                    }
                    continue;
                }
                if !trimmed.is_empty() || event_data.is_empty() {
                    continue;
                }
                let data = std::mem::take(&mut event_data);
                if data.trim() == "[DONE]" {
                    continue;
                }
                let json_obj = serde_json::from_str::<Value>(&data)
                    .map_err(|_| LlmError::InvalidResponse("SSE data 不是有效 JSON".into()))?;

                if let Some(usage) = json_obj.get("usageMetadata") {
                    final_usage = Some(usage.clone());
                }

                if let Some(candidates) = json_obj.get("candidates").and_then(|c| c.as_array()) {
                    let first_cand_opt = candidates.first();
                    if let Some(first_cand) = first_cand_opt {
                        let parts_opt = first_cand
                            .get("content")
                            .and_then(|c| c.get("parts"))
                            .and_then(|p| p.as_array());
                        if let Some(parts) = parts_opt {
                            for p in parts {
                                if collected_parts.len() >= 4096 {
                                    return Err(LlmError::InvalidResponse(
                                        "模型响应 parts 数量超过 4096".into(),
                                    ));
                                }
                                serde_json::to_writer(&mut part_budget, p).map_err(|_| {
                                    LlmError::InvalidResponse("模型上下文超过 2 MiB".into())
                                })?;
                                collected_parts.push(p.clone());
                                // 必须不是 thought 才外发
                                let is_thought =
                                    p.get("thought").and_then(|t| t.as_bool()).unwrap_or(false);
                                let text_opt = p.get("text").and_then(|t| t.as_str());
                                if let (false, Some(txt)) = (is_thought, text_opt) {
                                    answer_bytes = answer_bytes.saturating_add(txt.len());
                                    if answer_bytes > 256 * 1024 {
                                        return Err(LlmError::InvalidResponse(
                                            "回答超过 256 KiB".into(),
                                        ));
                                    }
                                    // Small frames bound the downstream queue by both count and bytes.
                                    let mut start = 0;
                                    while start < txt.len() {
                                        let mut end = (start + 12 * 1024).min(txt.len());
                                        while !txt.is_char_boundary(end) {
                                            end -= 1;
                                        }
                                        on_event(StreamEvent::Text(txt[start..end].to_string()))
                                            .await;
                                        start = end;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Compact only once per network chunk, not once per SSE line.
            buffer.drain(..consumed);
            if buffer.len() > 1024 * 1024 {
                return Err(LlmError::InvalidResponse("SSE 行超过 1 MiB".into()));
            }
        }
        if !buffer.is_empty() || !event_data.is_empty() || collected_parts.is_empty() {
            return Err(LlmError::InvalidResponse(
                "模型流不完整或没有有效候选内容".into(),
            ));
        }

        if let (Some(m), Some(usage)) = (metrics, final_usage) {
            let prompt = usage
                .get("promptTokenCount")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let output = usage
                .get("candidatesTokenCount")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let thought = usage
                .get("thoughtsTokenCount")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            m.record_model_usage(prompt, output, thought);
        }

        on_event(StreamEvent::Parts(collected_parts)).await;
        Ok(())
    }
}

pub async fn embed_query(
    client: &reqwest::Client,
    config: &Config,
    text: &str,
    dims: Option<usize>,
    metrics: Option<&TurnMetrics>,
) -> Result<Vec<f32>, LlmError> {
    let target_dims = dims.unwrap_or(config.embed_dims);
    if !(1..=8192).contains(&target_dims) {
        return Err(LlmError::EmbeddingFailed("向量维度必须为 1..8192".into()));
    }
    let payload = serde_json::json!({
        "model": config.siliconflow_model,
        "input": [text],
        "dimensions": target_dims
    });

    if let Some(m) = metrics {
        m.inc_embedding_calls();
    }

    let url = format!(
        "{}/embeddings",
        config.siliconflow_base_url.trim_end_matches('/')
    );
    let res = client
        .post(&url)
        .header(
            "Authorization",
            format!("Bearer {}", config.siliconflow_api_key),
        )
        .json(&payload)
        .send()
        .await
        .map_err(|e| LlmError::Unreachable(e.without_url().to_string()))?;

    if !res.status().is_success() {
        return Err(LlmError::BadStatus(res.status().as_u16()));
    }

    const MAX_EMBEDDING_RESPONSE: usize = 1024 * 1024;
    if res
        .content_length()
        .is_some_and(|length| length > MAX_EMBEDDING_RESPONSE as u64)
    {
        return Err(LlmError::InvalidResponse("向量响应超过 1 MiB".into()));
    }
    let mut bytes = Vec::new();
    let mut stream = res.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| LlmError::Unreachable(e.without_url().to_string()))?;
        if chunk.len() > MAX_EMBEDDING_RESPONSE.saturating_sub(bytes.len()) {
            return Err(LlmError::InvalidResponse("向量响应超过 1 MiB".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    let res_json: Value = serde_json::from_slice(&bytes)
        .map_err(|_| LlmError::InvalidResponse("向量响应不是有效 JSON".into()))?;

    if let (Some(m), Some(usage)) = (metrics, res_json.get("usage")) {
        let tokens = usage
            .get("total_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        m.record_embedding_usage(tokens);
    }

    let vec_arr = res_json
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
        .and_then(|d| d.get("embedding"))
        .and_then(|e| e.as_array())
        .ok_or_else(|| LlmError::InvalidResponse("缺少 embedding 数据".into()))?;

    if vec_arr.len() != target_dims {
        return Err(LlmError::EmbeddingFailed(format!(
            "返回维度 {} 与预期 {} 不一致",
            vec_arr.len(),
            target_dims
        )));
    }

    let mut vec = Vec::with_capacity(target_dims);
    let mut norm_sq = 0.0f64;
    for v in vec_arr {
        let val = v
            .as_f64()
            .ok_or_else(|| LlmError::EmbeddingFailed("向量包含非法非浮点值".into()))?
            as f32;
        if !val.is_finite() {
            return Err(LlmError::EmbeddingFailed(
                "向量包含非有限值(NaN/Inf)".into(),
            ));
        }
        norm_sq += f64::from(val) * f64::from(val);
        vec.push(val);
    }

    let norm = norm_sq.sqrt();
    if norm <= 0.0 || !norm.is_finite() {
        return Err(LlmError::EmbeddingFailed("查询向量为零向量或无效".into()));
    }

    for x in &mut vec {
        *x = (f64::from(*x) / norm) as f32;
    }

    Ok(vec)
}
