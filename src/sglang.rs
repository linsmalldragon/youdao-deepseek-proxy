//! sglang-native endpoints, so the proxy also speaks the protocol a real
//! sglang server exposes (not just the OpenAI-compatible subset):
//!
//! - `POST /generate`      — sglang-native generation (`text` + `sampling_params`).
//! - `GET  /server_info`   — server/model info incl. the context limit.
//! - `GET  /get_server_info` — deprecated alias of `/server_info`.
//! - `GET  /get_model_info`  — model info incl. `max_context_len`.
//! - `GET  /model_info`     — newer model-info shape (incl. `max_model_len`).
//!
//! The context limit is reported consistently with vllm/sglang so a client can
//! retrieve it (`max_model_len` / `max_context_len` / `max_total_num_tokens`).

use axum::body::Body;
use axum::extract::State;
use axum::http::header;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures::stream::{self, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::openai::{done_frame, est_tokens};
use crate::youdao::SseEvent;
use crate::AppState;

/// Served model identity as seen by OpenAI/sglang clients.
pub const SERVED_MODEL: &str = "deepseek-r1";

/// sglang-native generation request (subset we care about).
#[derive(Debug, Deserialize)]
pub struct GenerateRequest {
    /// A single prompt string, or a list of prompt strings (batch).
    #[serde(default)]
    pub text: Option<Value>,
    // Accepted but not forwarded (Youdao's endpoint manages its own sampling).
    #[serde(default)]
    #[allow(dead_code)]
    pub sampling_params: Option<Value>,
    #[serde(default)]
    pub stream: bool,
    // Request id, accepted for interface parity (not used).
    #[serde(default)]
    #[allow(dead_code)]
    pub rid: Option<String>,
}

/// Flatten `GenerateRequest::text` (string or list) to its entries.
fn generate_texts(req: &GenerateRequest) -> Vec<String> {
    match &req.text {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

/// POST /generate — sglang-native. Non-streaming returns a single object
/// (`{text, meta_info}`) or a list when the request batched multiple texts.
pub async fn generate(
    State(st): State<AppState>,
    Json(req): Json<GenerateRequest>,
) -> Response {
    let mut texts = generate_texts(&req);
    texts.retain(|t| !t.is_empty());
    if texts.is_empty() {
        return sglang_error_response("no non-empty 'text' provided in /generate request");
    }

    if req.stream {
        return generate_stream_response(st, texts);
    }

    let mut results = Vec::new();
    for t in &texts {
        let prompt_tokens = est_tokens(t);
        match crate::run_chat(&st, t, 1).await {
            Ok(out) => {
                let finish = match &out.error {
                    Some((_, msg)) => json!({ "type": "abort", "message": msg }),
                    None => json!({ "type": "stop" }),
                };
                let completion_tokens = est_tokens(&out.content) + est_tokens(&out.reasoning);
                results.push(json!({
                    "text": out.content,
                    "meta_info": {
                        "finish_reason": finish,
                        "prompt_tokens": prompt_tokens,
                        "completion_tokens": completion_tokens,
                        "cached_tokens": 0,
                        "text": out.content
                    }
                }));
            }
            Err(e) => {
                results.push(json!({
                    "text": String::new(),
                    "meta_info": {
                        "finish_reason": { "type": "abort", "message": e.to_string() },
                        "prompt_tokens": prompt_tokens,
                        "completion_tokens": 0,
                        "cached_tokens": 0,
                        "text": String::new()
                    }
                }));
            }
        }
    }

    let body = if results.len() == 1 {
        results.remove(0)
    } else {
        Value::Array(results)
    };
    Json(body).into_response()
}

/// Streaming state: texts are processed one at a time, sequentially. For the
/// active text we hold its upstream event stream plus a running text buffer;
/// when a text's stream ends we move on to the next queued text.
struct GenState {
    st: AppState,
    pending: std::collections::VecDeque<String>,
    pts: std::collections::VecDeque<u32>,
    active: Option<futures::stream::BoxStream<'static, anyhow::Result<SseEvent>>>,
    buf: String,
    cur_pt: u32,
}

/// POST /generate with `stream: true` — SSE of cumulative text per input.
fn generate_stream_response(st: AppState, texts: Vec<String>) -> Response {
    let pts: Vec<u32> = texts.iter().map(|s| est_tokens(s)).collect();

    let state = GenState {
        st,
        pending: texts.into_iter().collect(),
        pts: pts.into_iter().collect(),
        active: None,
        buf: String::new(),
        cur_pt: 0,
    };

    let body: futures::stream::BoxStream<'static, std::result::Result<Bytes, anyhow::Error>> =
        stream::unfold(state, |mut state| async move {
            loop {
                // No active stream: start the next queued text's upstream stream.
                if state.active.is_none() {
                    let t = match state.pending.pop_front() {
                        Some(t) => t,
                        None => return None,
                    };
                    state.cur_pt = state.pts.pop_front().unwrap_or(0);
                    state.buf.clear();
                    match crate::start_chat_stream(&state.st, &t, 1).await {
                        Ok(chat) => state.active = Some(chat.into_stream()),
                        Err(e) => {
                            let frame = format!(
                                "data: {}\n\n",
                                json!({ "error": { "message": e.to_string(), "type": "upstream_error" } })
                            );
                            return Some((Ok::<_, anyhow::Error>(Bytes::from(frame)), state));
                        }
                    }
                }

                let active = state.active.as_mut().unwrap();
                match active.next().await {
                    Some(ev) => {
                        let bytes = match ev {
                            Ok(SseEvent::Message { content, .. }) => {
                                if content.is_empty() {
                                    Bytes::new()
                                } else {
                                    state.buf.push_str(&content);
                                    Bytes::from(generate_frame(&state.buf, state.cur_pt, None))
                                }
                            }
                            Ok(SseEvent::End(_)) => {
                                let frame =
                                    generate_frame(&state.buf, state.cur_pt, Some(json!({ "type": "stop" })));
                                state.active = None;
                                Bytes::from(frame)
                            }
                            Ok(SseEvent::Error { msg, .. }) => {
                                let frame = generate_frame(
                                    &state.buf,
                                    state.cur_pt,
                                    Some(json!({ "type": "abort", "message": msg })),
                                );
                                state.active = None;
                                Bytes::from(frame)
                            }
                            Ok(SseEvent::Other(..)) => Bytes::new(),
                            Err(e) => {
                                let frame = format!(
                                    "data: {}\n\n",
                                    json!({ "error": { "message": e.to_string(), "type": "upstream_error" } })
                                );
                                state.active = None;
                                Bytes::from(frame)
                            }
                        };
                        return Some((Ok::<_, anyhow::Error>(bytes), state));
                    }
                    // The active upstream stream closed; advance to the next text.
                    None => {
                        state.active = None;
                        continue;
                    }
                }
            }
        })
        .filter_map(|res| async move {
            match res {
                Ok(b) if !b.is_empty() => Some(Ok::<_, anyhow::Error>(b)),
                Ok(_) => None,
                Err(e) => Some(Err(e)),
            }
        })
        .chain(stream::once(async {
            Ok::<Bytes, anyhow::Error>(Bytes::from(done_frame()))
        }))
        .boxed();

    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(body))
        .unwrap()
}

/// One sglang `/generate` SSE frame: cumulative `text` plus `meta_info`.
fn generate_frame(text: &str, prompt_tokens: u32, finish: Option<Value>) -> String {
    let completion_tokens = est_tokens(text);
    let obj = json!({
        "text": text,
        "meta_info": {
            "finish_reason": finish,
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "cached_tokens": 0,
            "text": text
        }
    });
    format!("data: {}\n\n", obj)
}

fn sglang_error_response(message: &str) -> Response {
    let body = json!({
        "error": {
            "message": message,
            "type": "invalid_request_error",
            "code": "invalid_request_error"
        }
    });
    (StatusCode::BAD_REQUEST, Json(body)).into_response()
}

/// GET /server_info (and /get_server_info) — report the context limit the way
/// sglang does, so clients can retrieve it.
pub async fn server_info(State(st): State<AppState>) -> Json<Value> {
    let c = &st.cfg;
    let max_ctx = c.max_context;
    let model_path = format!("youdao://{}", c.function_english_name);
    Json(json!({
        "model_path": model_path,
        "served_model_name": SERVED_MODEL,
        "tokenizer_path": model_path,
        "model_type": c.function_english_name,
        "architectures": ["DeepseekV3ForCausalLM"],
        "max_total_num_tokens": max_ctx,
        "max_req_len": max_ctx,
        "max_context_len": max_ctx,
        "context_length": max_ctx,
        "model": {
            "path": model_path,
            "model_type": c.function_english_name,
            "context_len": max_ctx,
            "max_total_num_tokens": max_ctx
        },
        "version": env!("CARGO_PKG_VERSION"),
        "frontend": "youdao-proxy"
    }))
}

/// GET /get_model_info — legacy sglang shape (exposes `max_context_len`).
pub async fn get_model_info(State(st): State<AppState>) -> Json<Value> {
    let c = &st.cfg;
    Json(json!({
        "model_path": format!("youdao://{}", c.function_english_name),
        "tokenizer_mode": "auto",
        "chat_template": c.function_english_name,
        "model_name": SERVED_MODEL,
        "served_model_name": SERVED_MODEL,
        "is_multimodal_model": false,
        "max_context_len": c.max_context
    }))
}

/// GET /model_info — newer sglang shape (exposes `max_model_len`).
pub async fn model_info(State(st): State<AppState>) -> Json<Value> {
    let c = &st.cfg;
    Json(json!({
        "model_path": format!("youdao://{}", c.function_english_name),
        "served_model_name": SERVED_MODEL,
        "tokenizer_path": format!("youdao://{}", c.function_english_name),
        "is_generation": true,
        "model_type": c.function_english_name,
        "architectures": ["DeepseekV3ForCausalLM"],
        "max_model_len": c.max_context
    }))
}
