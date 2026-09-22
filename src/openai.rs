use serde::Deserialize;

/// OpenAI-compatible chat completion request (subset we care about).
#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    #[serde(default)]
    pub model: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub stream: bool,
    // Accepted but not forwarded (Youdao's endpoint manages its own sampling).
    #[serde(default)]
    #[allow(dead_code)]
    pub temperature: Option<f64>,
    #[serde(default)]
    #[allow(dead_code)]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Message {
    pub role: String,
    pub content: serde_json::Value,
}

impl Message {
    /// Message content as a plain string (handles string or parts-array forms).
    pub fn text(&self) -> String {
        match &self.content {
            serde_json::Value::String(s) => s.clone(),
            // some clients send content as an array of parts
            serde_json::Value::Array(parts) => parts
                .iter()
                .map(|p| {
                    p.get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join(""),
            other => other.to_string(),
        }
    }
}

/// Extract the user's latest input from the message list. This is the field
/// the Youdao endpoint actually consumes (`input`). `round_no` is the count of
/// user turns so far (a fresh single turn -> 1).
pub fn extract_input_and_round(messages: &[Message]) -> (String, u32) {
    let mut user_turns = 0u32;
    let mut last_user = String::new();
    for m in messages {
        if m.role == "user" {
            user_turns += 1;
            last_user = m.text();
        }
    }
    (last_user, user_turns.max(1))
}

/// Build an OpenAI non-streaming completion response body.
pub fn completion_response(
    id: &str,
    model: &str,
    content: &str,
    reasoning: &str,
    finish: &str,
) -> serde_json::Value {
    let mut message = serde_json::json!({
        "role": "assistant",
        "content": content,
    });
    // Surface the model's chain-of-thought when present (DeepSeek R1).
    if !reasoning.is_empty() {
        message["reasoning_content"] = serde_json::json!(reasoning);
    }
    serde_json::json!({
        "id": id,
        "object": "chat.completion",
        "created": now_secs(),
        "model": model,
        "choices": [
            {
                "index": 0,
                "message": message,
                "finish_reason": finish,
            }
        ],
        "usage": {
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "total_tokens": 0
        }
    })
}

/// Build one OpenAI streaming chunk as an SSE `data:` frame.
pub fn chunk_frame(id: &str, model: &str, delta: serde_json::Value, finish: Option<&str>) -> String {
    let obj = serde_json::json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": now_secs(),
        "model": model,
        "choices": [
            {
                "index": 0,
                "delta": delta,
                "finish_reason": finish,
            }
        ]
    });
    format!("data: {}\n\n", obj)
}

pub fn done_frame() -> String {
    "data: [DONE]\n\n".to_string()
}

fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Generate a chatcmpl-style id.
pub fn new_id() -> String {
    // Deterministic-ish, no entropy source needed; use time + counter.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .to_string();
    format!("chatcmpl-{}-{}", now_secs(), n)
}
