mod config;
mod openai;
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

use openai::{chunk_frame, completion_response, done_frame, extract_input_and_round, new_id, ChatRequest};
use youdao::{SseEvent, YoudaoClient};

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
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/models", get(list_models))
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
        "upstream={} function={} translateType={} stream=OpenAI /v1/chat/completions",
        cfg.base_url,
        cfg.function_english_name,
        cfg.translate_type
    );
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

async fn list_models(_st: State<AppState>) -> Json<Value> {
    Json(json!({
        "object": "list",
        "data": [
            { "id": "deepseek-r1", "object": "model", "created": 0, "owned_by": "youdao" }
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

async fn chat_completions(
    State(st): State<AppState>,
    Json(req): Json<ChatRequest>,
) -> Response {
    let (input, round) = extract_input_and_round(&req.messages);
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
        let body =
            completion_response(&new_id(), &model_out, &agg.content, &agg.reasoning, finish);
        Json(body).into_response()
    }
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
