use crate::config::{OpenAiConfig, build_quiz_prompt};
use crate::llm::LlmChunk;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::Client;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 思考强度档位 → thinking budget_tokens 映射
/// （对应 low/high/max 三档，与 OpenAI/DeepSeek 的 reasoning_effort 对齐）
fn effort_to_budget(effort: &str) -> u32 {
    match effort {
        "low" => 4096,
        "max" => 24000,
        // "high" 及未知值兜底
        _ => 10000,
    }
}

pub struct AnthropicClient {
    http: Client,
    base_url: String,
    model: String,
    api_key: String,
    enable_thinking: bool,
    reasoning_effort: String,
}

impl AnthropicClient {
    pub fn new(config: &OpenAiConfig) -> Self {
        let http = Client::builder().build().expect("创建 HTTP 客户端失败");
        // 兜底：配置文件手动编辑导致空值时回退到默认 high
        let reasoning_effort = if config.reasoning_effort.is_empty() {
            "high".to_string()
        } else {
            config.reasoning_effort.clone()
        };
        Self {
            http,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            model: config.model.clone(),
            api_key: config.api_key.clone(),
            enable_thinking: config.enable_thinking,
            reasoning_effort,
        }
    }

    pub fn ask_stream(
        &self,
        question: &str,
        categories: Vec<String>,
        tx: mpsc::UnboundedSender<LlmChunk>,
        token: CancellationToken,
    ) {
        let prompt = build_quiz_prompt(&categories, question, self.enable_thinking);

        let mut body = serde_json::json!({
            "model": self.model,
            // Anthropic 强制要求 max_tokens；答题只需 1-4 序号，1024 足够
            "max_tokens": 1024,
            "stream": true,
            "messages": [
                {
                    "role": "user",
                    "content": prompt
                }
            ]
        });

        if self.enable_thinking {
            body["thinking"] = serde_json::json!({
                "type": "enabled",
                "budget_tokens": effort_to_budget(&self.reasoning_effort)
            });
        }

        let url = self.base_url.clone();
        let http = self.http.clone();
        let api_key = self.api_key.clone();

        tokio::spawn(async move {
            if token.is_cancelled() {
                return;
            }
            let resp = match http
                .post(&url)
                .header("Content-Type", "application/json")
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
                .timeout(std::time::Duration::from_secs(120))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(LlmChunk::Error(e.to_string()));
                    return;
                }
            };

            let status = resp.status();
            if !status.is_success() {
                let body_text = resp.text().await.unwrap_or_default();
                let preview = &body_text[..body_text.len().min(300)];
                let _ = tx.send(LlmChunk::Error(format!(
                    "LLM 请求失败 (HTTP {}): {}",
                    status, preview
                )));
                return;
            }

            let mut stream = resp.bytes_stream().eventsource();
            let mut full_content = String::new();

            while let Some(event) = stream.next().await {
                if token.is_cancelled() {
                    return;
                }
                match event {
                    Ok(event) => {
                        // Anthropic 用显式 event: 行标识事件类型
                        // message_stop 标记流结束（等价于 OpenAI 的 [DONE]）
                        if event.event == "message_stop" {
                            break;
                        }
                        // 仅处理内容增量事件
                        if event.event != "content_block_delta" {
                            continue;
                        }
                        let json: serde_json::Value = match serde_json::from_str(&event.data) {
                            Ok(j) => j,
                            Err(_) => continue,
                        };

                        let delta = &json["delta"];
                        match delta["type"].as_str() {
                            Some("thinking_delta") => {
                                if let Some(thinking) = delta["thinking"].as_str()
                                    && !thinking.is_empty()
                                {
                                    let _ = tx.send(LlmChunk::Thinking(thinking.to_string()));
                                }
                            }
                            Some("text_delta") => {
                                if let Some(text) = delta["text"].as_str()
                                    && !text.is_empty()
                                {
                                    full_content.push_str(text);
                                    let _ = tx.send(LlmChunk::Content(text.to_string()));
                                }
                            }
                            _ => {}
                        }
                    }
                    Err(e) => {
                        tracing::warn!("SSE stream error: {}", e);
                        break;
                    }
                }
            }

            if !token.is_cancelled() {
                let _ = tx.send(LlmChunk::Done(full_content));
            }
        });
    }
}
