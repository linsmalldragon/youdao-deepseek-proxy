mod config;
mod openai;
mod sglang;
mod youdao;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::body::Body;
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures::stream::{self, StreamExt};
use serde_json::{json, Value};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use openai::{
    build_chat_input, chunk_frame, completion_response, done_frame, est_tokens, new_completion_id,
    new_id, now_secs, parse_tool_calls, prompt_texts, text_completion_chunk,
    text_completion_response, ChatRequest, CompletionRequest,
};
use youdao::{AggregatedChat, ChatStream, SseEvent, YoudaoClient};

use config::YoudaoConfig;

#[derive(Clone)]
struct AppState {
    client: YoudaoClient,
    cfg: Arc<YoudaoConfig>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = Arc::new(YoudaoConfig::from_env());
    let client = YoudaoClient::new(cfg.clone());
    let state = AppState { client, cfg: cfg.clone() };

    let app = Router::new()
        // OpenAI-compatible
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(text_completions))
        .route("/v1/models", get(list_models))
        // sglang-native
        .route("/generate", post(sglang::generate))
        .route("/server_info", get(sglang::server_info))
        .route("/get_server_info", get(sglang::server_info))
        .route("/get_model_info", get(sglang::get_model_info))
        .route("/model_info", get(sglang::model_info))
        // liveness
        .route("/health", get(health))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr: SocketAddr = cfg
        .bind
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid YOUDAO_BIND '{}': {e}", cfg.bind))?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("youdao-llm-proxy listening on http://{}", addr);
    tracing::info!(
        "upstream={} function={} translateType={} max_context={}",
        cfg.base_url,
        cfg.function_english_name,
        cfg.translate_type,
        cfg.max_context
    );
    tracing::info!(
        "endpoints: /v1/chat/completions /v1/completions /v1/models /generate /server_info /get_model_info /model_info /health"
    );
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

/// OpenAI model list. `max_model_len` is the retrievable context length — the
/// same field vllm/sglang expose in `/v1/models`.
async fn list_models(State(st): State<AppState>) -> Json<Value> {
    let max_ctx = st.cfg.max_context;
    Json(json!({
        "object": "list",
        "data": [
            {
                "id": sglang::SERVED_MODEL,
                "object": "model",
                "created": 0,
                "owned_by": "youdao",
                "max_model_len": max_ctx
            }
        ]
    }))
}

/// Resolve the signing key + token for one completion.
///
/// The dynamic `secretKey` comes from `/translate_llm/secret`; the `token` is
/// that endpoint's guest token unless `YOUDAO_TOKEN` overrides it (e.g. a
/// logged-in session's `ydtoken`).
async fn credentials(
    st: &AppState,
) -> Result<(String, String)> {
    let (secret_key, guest_token) = st.client.fetch_secret().await?;
    let token = st.cfg.token_override.clone().unwrap_or(guest_token);
    Ok((secret_key, token))
}

/// Run a single-turn chat to completion and return the aggregated result.
/// Used by the sglang-native `/generate` path (non-streaming).
async fn run_chat(st: &AppState, input: &str, round: u32) -> Result<AggregatedChat> {
    let (secret_key, token) = credentials(st).await?;
    let chat = st
        .client
        .start_chat(input, round, &secret_key, &token)
        .await?;
    chat.collect().await
}

/// Start a chat stream (used by the sglang-native `/generate` streaming path
/// and the legacy text-completions streaming path).
async fn start_chat_stream(st: &AppState, input: &str, round: u32) -> Result<ChatStream> {
    let (secret_key, token) = credentials(st).await?;
    st.client.start_chat(input, round, &secret_key, &token).await
}

async fn chat_completions(
    State(st): State<AppState>,
    Json(req): Json<ChatRequest>,
) -> Response {
    // `Some` = non-empty tool list → this is a tool-calling (coding-agent) turn.
    let tools = req.tools.as_deref().filter(|t| !t.is_empty());
    let (input, round) = build_chat_input(&req.messages, tools, req.tool_choice.as_ref());
    if input.is_empty() {
        return error_response(
            "invalid_request_error",
            "no user message content to send upstream",
        );
    }
    let model_out = req.model.clone().unwrap_or_else(|| "deepseek-r1".into());

    let (secret_key, token) = match credentials(&st).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("secret fetch failed: {e}");
            return error_response("upstream_error", &format!("secret fetch failed: {e}"));
        }
    };

    let chat = match st
        .client
        .start_chat(&input, round, &secret_key, &token)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("upstream chat error: {e}");
            return error_response("upstream_error", &format!("upstream error: {e}"));
        }
    };

    // Tool-calling turn: the model's `[TOOL_CALL]` markers must be parsed from
    // the complete reply, so `tool_stream_response` aggregates it: reasoning
    // deltas stream through the moment they arrive (the visible "thinking"),
    // while the marker-stripped content and the parsed tool-call deltas are
    // emitted together only once the reply is complete — that is what keeps
    // raw marker text out of `content`. The finish chunk, a trailing usage
    // frame and `[DONE]` follow.
    if tools.is_some() {
        if req.stream {
            return tool_stream_response(
                chat,
                &model_out,
                &input,
                tools.map(|t| t.len()).unwrap_or(0),
            );
        }
        let agg = match chat.collect().await {
            Ok(a) => a,
            Err(e) => {
                return error_response("upstream_error", &format!("stream error: {e}"));
            }
        };
        let (cleaned, tool_calls) = parse_tool_calls(&agg.content);
        let finish = if agg.error.is_some() {
            "error"
        } else if !tool_calls.is_empty() {
            "tool_calls"
        } else {
            "stop"
        };
        tracing::info!(
            n_tools = tools.map(|t| t.len()).unwrap_or(0),
            n_tool_calls = tool_calls.len(),
            stream = req.stream,
            finish,
            "chat_completions tool turn"
        );
        if tool_calls.is_empty() {
            // The model saw the tool protocol but chose not to (or emitted
            // nothing parseable). Log a short head of the raw reply so a
            // "it narrated instead of calling a tool" regression is visible.
            let head = cleaned.chars().take(160).collect::<String>();
            tracing::warn!("tool turn produced no tool_calls; content head: {head}");
        }
        let body = completion_response(
            &new_id(),
            &model_out,
            &input,
            &cleaned,
            &agg.reasoning,
            finish,
            &tool_calls,
        );
        return Json(body).into_response();
    }

    // Plain (no-tools) turn — unchanged live streaming / aggregated JSON.
    if req.stream {
        stream_response(chat, &model_out)
    } else {
        let agg = match chat.collect().await {
            Ok(a) => a,
            Err(e) => {
                return error_response("upstream_error", &format!("stream error: {e}"));
            }
        };
        let finish = if agg.error.is_some() { "error" } else { "stop" };
        let body = completion_response(
            &new_id(),
            &model_out,
            &input,
            &agg.content,
            &agg.reasoning,
            finish,
            &[],
        );
        Json(body).into_response()
    }
}

/// Build an SSE `Response` for a tool-calling turn that streams progressively:
/// `reasoning_content` deltas are forwarded the moment they arrive from
/// upstream (this is the visible "thinking" stream), while the visible content
/// and any tool calls are withheld until the reply is complete — the
/// `[TOOL_CALL]` markers have to be parsed from the full text before they can
/// be surfaced as OpenAI `tool_calls` (and the marker JSON must never leak
/// into `content`). When the upstream stream ends we emit the marker-stripped
/// `content`, the parsed `tool_calls` (one delta each), the finishing chunk,
/// a trailing `usage` frame and `[DONE]`.
fn tool_stream_response(
    chat: ChatStream,
    model_out: &str,
    input: &str,
    n_tools: usize,
) -> Response {
    let id = new_id();
    let model = model_out.to_string();
    let first = Bytes::from(chunk_frame(&id, &model, json!({ "role": "assistant" }), None));

    type BodyItem = std::result::Result<Bytes, anyhow::Error>;
    let (tx, rx) = tokio::sync::mpsc::channel::<BodyItem>(64);

    // The spawned task owns the upstream stream; the channel drains into the
    // response body below.
    let id2 = id.clone();
    let model2 = model.clone();
    let input2 = input.to_string();
    tokio::spawn(async move {
        use futures::StreamExt;
        let mut full_content = String::new();
        let mut upstream_err: Option<String> = None;

        // Forward reasoning deltas live; accumulate the content so it can be
        // parsed for tool calls once the stream is complete.
        let mut it = chat.into_stream();
        while let Some(ev) = it.next().await {
            match ev {
                Ok(SseEvent::Message {
                    content,
                    reasoning,
                }) => {
                    full_content.push_str(&content);
                    if let Some(r) = &reasoning {
                        if !r.is_empty() {
                            let delta = json!({ "reasoning_content": r });
                            if tx
                                .send(Ok(Bytes::from(chunk_frame(
                                    &id2, &model2, delta, None,
                                ))))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
                Ok(SseEvent::End(_)) => {}
                Ok(SseEvent::Error { code: _, msg }) => {
                    upstream_err = Some(msg);
                    break;
                }
                Ok(SseEvent::Other(..)) => {}
                Err(e) => {
                    upstream_err = Some(e.to_string());
                    break;
                }
            }
        }

        let (cleaned, tool_calls) = parse_tool_calls(&full_content);
        let finish = if upstream_err.is_some() {
            "error"
        } else if !tool_calls.is_empty() {
            "tool_calls"
        } else {
            "stop"
        };
        tracing::info!(
            n_tools,
            n_tool_calls = tool_calls.len(),
            finish,
            "chat_completions tool turn (streaming)"
        );
        if tool_calls.is_empty() && upstream_err.is_none() {
            let head = cleaned.chars().take(160).collect::<String>();
            tracing::warn!("tool turn produced no tool_calls; content head: {head}");
        }

        // Marker-stripped content, one delta.
        if !cleaned.is_empty() {
            let delta = json!({ "content": cleaned });
            if tx
                .send(Ok(Bytes::from(chunk_frame(
                    &id2, &model2, delta, None,
                ))))
                .await
                .is_err()
            {
                return;
            }
        }
        // Parsed tool calls, one delta each.
        for (i, tc) in tool_calls.iter().enumerate() {
            let tc_delta = json!({
                "index": i,
                "id": tc.get("id"),
                "type": "function",
                "function": {
                    "name": tc.pointer("/function/name"),
                    "arguments": tc.pointer("/function/arguments"),
                }
            });
            if tx
                .send(Ok(Bytes::from(chunk_frame(
                    &id2,
                    &model2,
                    json!({ "tool_calls": [tc_delta] }),
                    None,
                ))))
                .await
                .is_err()
            {
                return;
            }
        }
        // Finishing chunk.
        if tx
            .send(Ok(Bytes::from(chunk_frame(
                &id2,
                &model2,
                Value::Object(serde_json::Map::new()),
                Some(finish),
            ))))
            .await
            .is_err()
        {
            return;
        }

        // Trailing usage frame + [DONE].
        let prompt_tokens = est_tokens(&input2);
        let completion_tokens = est_tokens(&cleaned);
        let usage_frame = json!({
            "id": id2,
            "object": "chat.completion.chunk",
            "created": now_secs(),
            "model": model2,
            "choices": [],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens
            }
        });
        let _ = tx
            .send(Ok(Bytes::from(format!("data: {usage_frame}\n\n"))))
            .await;
        let _ = tx.send(Ok(Bytes::from(done_frame()))).await;
        // drop sender -> close the channel
    });

    let channel_stream: futures::stream::BoxStream<'static, BodyItem> =
        futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })
        .boxed();

    let body_stream: futures::stream::BoxStream<'static, BodyItem> =
        stream::once(async move { Ok::<Bytes, anyhow::Error>(first) })
            .chain(channel_stream)
            .boxed();

    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(body_stream))
        .unwrap()
}

/// Build an SSE `Response` that streams OpenAI chunks from the upstream stream.
fn stream_response(
    chat: youdao::ChatStream,
    model_out: &str,
) -> Response {
    let id = new_id();
    let model = model_out.to_string();

    let first = Bytes::from(chunk_frame(
        &id,
        &model,
        json!({ "role": "assistant" }),
        None,
    ));

    type BodyItem = std::result::Result<Bytes, anyhow::Error>;

    let main: futures::stream::BoxStream<'static, BodyItem> = chat
        .into_stream()
        .map(move |ev| Ok::<_, anyhow::Error>(event_to_bytes(ev, &id, &model)))
        .filter_map(|item| async move {
            match item {
                Ok(b) if !b.is_empty() => Some(Ok(b)),
                Ok(_) => None,
                Err(e) => Some(Err(e)),
            }
        })
        .boxed();

    let body_stream: futures::stream::BoxStream<'static, BodyItem> =
        stream::once(async move { Ok::<Bytes, anyhow::Error>(first) })
            .chain(main)
            .chain(stream::once(async move {
                Ok::<Bytes, anyhow::Error>(Bytes::from(done_frame()))
            }))
            .boxed();

    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(body_stream))
        .unwrap()
}

/// Map one upstream SSE event to an OpenAI SSE frame (empty bytes when there is
/// nothing to forward).
fn event_to_bytes(ev: Result<SseEvent>, id: &str, model: &str) -> Bytes {
    match ev {
        Ok(SseEvent::Message { content, reasoning }) => {
            let mut delta = serde_json::Map::new();
            if !content.is_empty() {
                delta.insert("content".to_string(), json!(content));
            }
            if let Some(r) = reasoning {
                if !r.is_empty() {
                    delta.insert("reasoning_content".to_string(), json!(r));
                }
            }
            if delta.is_empty() {
                return Bytes::new();
            }
            Bytes::from(chunk_frame(id, model, Value::Object(delta), None))
        }
        Ok(SseEvent::End(_)) => {
            Bytes::from(chunk_frame(id, model, Value::Object(serde_json::Map::new()), Some("stop")))
        }
        Ok(SseEvent::Error { msg, .. }) => Bytes::from(
            format!(
                "data: {}\n\n",
                json!({ "error": { "message": msg, "type": "upstream_error", "code": "upstream_error" } })
            ),
        ),
        Ok(SseEvent::Other(..)) => Bytes::new(),
        Err(e) => Bytes::from(
            format!(
                "data: {}\n\n",
                json!({ "error": { "message": e.to_string(), "type": "upstream_error" } })
            ),
        ),
    }
}

/// OpenAI legacy `/v1/completions` (text completions). Forwards the last
/// non-empty `prompt` entry upstream (the endpoint is single-turn) and shapes
/// the reply as a `text_completion`.
async fn text_completions(
    State(st): State<AppState>,
    Json(req): Json<CompletionRequest>,
) -> Response {
    let texts = prompt_texts(&req.prompt);
    let input = texts
        .iter()
        .rev()
        .find(|s| !s.is_empty())
        .cloned()
        .unwrap_or_default();
    if input.is_empty() {
        return error_response("invalid_request_error", "no non-empty 'prompt' provided");
    }
    let model_out = req.model.clone().unwrap_or_else(|| "deepseek-r1".into());

    if req.stream {
        let chat = match start_chat_stream(&st, &input, 1).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("upstream chat error: {e}");
                return error_response("upstream_error", &format!("upstream error: {e}"));
            }
        };
        return stream_text_completion(chat, &model_out, &input);
    }

    let agg = match run_chat(&st, &input, 1).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("upstream completion error: {e}");
            return error_response("upstream_error", &format!("upstream error: {e}"));
        }
    };
    let finish = if agg.error.is_some() { "error" } else { "stop" };
    let body = text_completion_response(&new_completion_id(), &model_out, &input, &agg.content, finish);
    Json(body).into_response()
}

/// Build an SSE `Response` streaming legacy `text_completion` chunks.
fn stream_text_completion(chat: ChatStream, model_out: &str, input: &str) -> Response {
    let id = new_completion_id();
    let model = model_out.to_string();
    let prompt = input.to_string();

    let first = Bytes::from(text_completion_chunk(&id, &model, &prompt, "", None));

    type BodyItem = std::result::Result<Bytes, anyhow::Error>;

    let main: futures::stream::BoxStream<'static, BodyItem> = chat
        .into_stream()
        .map(move |ev| Ok::<_, anyhow::Error>(text_event_to_bytes(ev, &id, &model, &prompt)))
        .filter_map(|item| async move {
            match item {
                Ok(b) if !b.is_empty() => Some(Ok(b)),
                Ok(_) => None,
                Err(e) => Some(Err(e)),
            }
        })
        .boxed();

    let body_stream: futures::stream::BoxStream<'static, BodyItem> =
        stream::once(async move { Ok::<Bytes, anyhow::Error>(first) })
            .chain(main)
            .chain(stream::once(async move {
                Ok::<Bytes, anyhow::Error>(Bytes::from(done_frame()))
            }))
            .boxed();

    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(body_stream))
        .unwrap()
}

/// Map one upstream SSE event to a legacy text-completion SSE frame (empty
/// bytes when there is nothing to forward).
fn text_event_to_bytes(
    ev: Result<SseEvent>,
    id: &str,
    model: &str,
    prompt: &str,
) -> Bytes {
    match ev {
        Ok(SseEvent::Message { content, .. }) => {
            if content.is_empty() {
                return Bytes::new();
            }
            Bytes::from(text_completion_chunk(id, model, prompt, &content, None))
        }
        Ok(SseEvent::End(_)) => {
            Bytes::from(text_completion_chunk(id, model, prompt, "", Some("stop")))
        }
        Ok(SseEvent::Error { msg, .. }) => Bytes::from(
            format!(
                "data: {}\n\n",
                json!({ "error": { "message": msg, "type": "upstream_error", "code": "upstream_error" } })
            ),
        ),
        Ok(SseEvent::Other(..)) => Bytes::new(),
        Err(e) => Bytes::from(
            format!(
                "data: {}\n\n",
                json!({ "error": { "message": e.to_string(), "type": "upstream_error" } })
            ),
        ),
    }
}

fn error_response(err_type: &str, message: &str) -> Response {
    let body = json!({
        "error": { "message": message, "type": err_type, "code": err_type }
    });
    (
        axum::http::StatusCode::BAD_GATEWAY,
        Json(body),
    )
        .into_response()
}
