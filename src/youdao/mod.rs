pub mod sign;

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;
use reqwest::multipart::{Form, Part};
use serde_json::Value;

use crate::config::YoudaoConfig;
use sign::{md5_hex, sign_biz};

/// A parsed, decodable SSE frame from the upstream LLM stream.
#[derive(Debug, Clone)]
pub enum SseEvent {
    /// A content delta. `content` is the visible text; `reasoning` is the
    /// model's chain-of-thought (present for R1-style models), if any.
    Message { content: String, reasoning: Option<String> },
    /// Stream finished; carries the trailing payload (e.g. `suggest`).
    End(Option<Value>),
    /// Upstream reported an error event.
    Error { code: i64, msg: String },
    /// Anything else (e.g. `begin`) — retained raw for diagnostics.
    #[allow(dead_code)]
    Other(String, Option<Value>),
}

#[derive(Clone)]
pub struct YoudaoClient {
    http: reqwest::Client,
    cfg: Arc<YoudaoConfig>,
}

/// A running chat stream: an async iterator over decoded SSE events.
pub struct ChatStream {
    stream: futures::stream::BoxStream<'static, Result<SseEvent>>,
}

impl ChatStream {
    /// Consume the stream into a single aggregated completion.
    pub async fn collect(self) -> Result<AggregatedChat> {
        let mut agg = AggregatedChat {
            content: String::new(),
            reasoning: String::new(),
            end: None,
            error: None,
        };
        use futures::StreamExt;
        let mut it = self.stream;
        while let Some(ev) = it.next().await {
            match ev? {
                SseEvent::Message { content, reasoning } => {
                    agg.content.push_str(&content);
                    if let Some(r) = reasoning {
                        agg.reasoning.push_str(&r);
                    }
                }
                SseEvent::End(v) => agg.end = v,
                SseEvent::Error { code, msg } => agg.error = Some((code, msg)),
                SseEvent::Other(..) => {}
            }
        }
        Ok(agg)
    }

    /// Access the underlying event stream (used by the OpenAI streaming layer).
    pub fn into_stream(self) -> futures::stream::BoxStream<'static, Result<SseEvent>> {
        self.stream
    }
}

/// Aggregated (non-streaming) result of a chat completion.
#[derive(Debug, Default)]
pub struct AggregatedChat {
    pub content: String,
    pub reasoning: String,
    pub end: Option<Value>,
    pub error: Option<(i64, String)>,
}

impl YoudaoClient {
    pub fn new(cfg: Arc<YoudaoConfig>) -> Self {
        let mut builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .connect_timeout(std::time::Duration::from_secs(15));
        if let Some(cookie) = &cfg.cookie {
            if let Ok(hv) = cookie.parse::<reqwest::header::HeaderValue>() {
                builder = builder.default_headers({
                    let mut h = reqwest::header::HeaderMap::new();
                    h.insert(reqwest::header::COOKIE, hv);
                    h
                });
            }
        }
        let http = builder
            .user_agent(cfg.client.as_str())
            .build()
            .expect("build http client");
        Self { http, cfg }
    }

    fn cfg(&self) -> &YoudaoConfig {
        &self.cfg
    }

    /// GET `/translate_llm/secret` -> `(dynamic secretKey, guest token)`.
    ///
    /// Signed with the FIXED embedded key over `client&mysticTime&product&key`,
    /// with `pointParam` the fixed string `client,mysticTime,product`.
    pub async fn fetch_secret(&self) -> Result<(String, String)> {
        let c = self.cfg();
        let mystic_time = now_millis();
        let sign_str = format!(
            "client={}&mysticTime={}&product={}&key={}",
            c.client, mystic_time, c.product, c.secret_key()
        );
        let sign = md5_hex(&sign_str);
        let url = format!("{}/translate_llm/secret", c.base_url);
        let mystic_time_s = mystic_time.to_string();
        // The app uses one device yduuid for both /secret and /chat; keep the
        // guest token consistent with the chat's yduuid when one is configured.
        let secret_yduuid: String = if !c.yduuid.is_empty() {
            c.yduuid.clone()
        } else {
            c.secret_yduuid.clone()
        };

        let mut query: Vec<(&str, &str)> = vec![
            ("sign", sign.as_str()),
            ("client", c.client.as_str()),
            ("product", c.product.as_str()),
            ("appVersion", c.app_version.as_str()),
            ("vendor", c.vendor.as_str()),
            ("pointParam", "client,mysticTime,product"),
            ("mysticTime", mystic_time_s.as_str()),
            ("keyfrom", c.secret_keyfrom.as_str()),
            ("mid", c.mid.as_str()),
            ("screen", c.screen.as_str()),
            ("model", c.model.as_str()),
            ("network", c.network.as_str()),
            ("abtest", "0"),
            ("yduuid", secret_yduuid.as_str()),
            ("keyid", "ai-translate-llm-pre"),
        ];
        // The app's /secret call carries the current ydtoken when one exists
        // (guest state -> none). Mirror that when an override token is set.
        if let Some(tok) = &c.token_override {
            query.push(("token", tok.as_str()));
        }

        let res = self
            .http
            .get(&url)
            .query(&query[..])
            .send()
            .await
            .context("request /translate_llm/secret")?;
        let status = res.status();
        let body: Value = res
            .json()
            .await
            .context("parse /translate_llm/secret body")?;
        if !status.is_success() {
            anyhow::bail!(
                "/translate_llm/secret returned {} : {}",
                status.as_u16(),
                truncate(&body.to_string(), 300)
            );
        }
        let data = body
            .get("data")
            .and_then(|d| d.as_object())
            .ok_or_else(|| anyhow!("secret response missing data object: {}", body))?;
        let secret_key = data
            .get("secretKey")
            .and_then(|v| v.as_str())
            .context("secret.data.secretKey missing")?
            .to_string();
        let token = data
            .get("token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok((secret_key, token))
    }

    /// Start a chat completion. Returns a stream of decoded SSE events.
    ///
    /// Mirrors the app: `getChat` injects the common params + `token`, then
    /// module-49920 builds a `FormData` (skipping empty/null/undefined) and
    /// POSTs it. The response is a `text/event-stream` of
    /// `begin`/`message`/`end`/`error` frames.
    pub async fn start_chat(
        &self,
        input: &str,
        round_no: u32,
        secret_key: &str,
        token: &str,
    ) -> Result<ChatStream> {
        let c = self.cfg();
        let mut biz: BTreeMap<String, String> = BTreeMap::new();
        // business fields (from the chat caller in 5048/423)
        biz.insert("free".into(), c.free_flag.clone());
        biz.insert("input".into(), urlencoding::encode(input).into_owned());
        biz.insert("singleBox".into(), "false".into());
        biz.insert("functionEnglishName".into(), c.function_english_name.clone());
        biz.insert("roundNo".into(), round_no.to_string());
        biz.insert("showSuggest".into(), "1".into());
        biz.insert("translateType".into(), c.translate_type.clone());
        biz.insert("id".into(), random_uuid_v4());
        biz.insert("useTerm".into(), c.use_term.clone());
        // common params injected by getChat
        biz.insert("vendor".into(), c.vendor.clone());
        biz.insert("screen".into(), c.screen.clone());
        biz.insert("model".into(), c.model.clone());
        biz.insert("imei".into(), c.imei.clone());
        biz.insert("network".into(), c.network.clone());
        biz.insert("mid".into(), c.mid.clone());
        biz.insert("yduuid".into(), c.yduuid.clone());
        biz.insert("client".into(), c.client.clone());
        biz.insert("appVersion".into(), c.app_version.clone());
        // token (guest token from /secret, or a configured override)
        biz.insert("token".into(), token.to_string());
        // 30933 genParamV3 defaults for the chat path (keyid = ai-translate-llm)
        biz.insert("product".into(), c.product.clone());
        biz.insert("keyid".into(), "ai-translate-llm".into());
        biz.insert("keyfrom".into(), c.keyfrom.clone());
        biz.insert("mysticTime".into(), now_millis().to_string());

        let signed = sign_biz(&biz, secret_key);

        // 49920: append every non-empty field to the multipart form.
        let mut form = Form::new();
        for (k, v) in &signed {
            if !v.is_empty() {
                form = form.part(k.clone(), Part::text(v.clone()));
            }
        }

        let url = format!("{}/translate_llm/v3/chat", c.base_url);
        let mut req = self.http.post(&url).multipart(form);
        // Mirror the WKWebView app's request headers.
        req = req
            .header(reqwest::header::USER_AGENT, c.user_agent.clone())
            .header(reqwest::header::ORIGIN, c.origin.clone())
            .header(reqwest::header::ACCEPT_LANGUAGE, c.accept_language.clone())
            .header(reqwest::header::ACCEPT, "*/*");
        if let Some(cookie) = &c.cookie {
            req = req.header(reqwest::header::COOKIE, cookie.clone());
        }
        let res = req.send().await.context("POST /translate_llm/v3/chat")?;
        let status = res.status();
        let ctype = res
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if !ctype.contains("text/event-stream") {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!(
                "chat returned {} ({}) body: {}",
                status.as_u16(),
                ctype,
                truncate(&body, 500)
            );
        }

        // Buffer-free streaming: turn the byte stream into a stream of
        // decoded SSE events, each unfold poll yielding exactly one frame.
        let byte_stream: futures::stream::BoxStream<'static, Result<Vec<u8>>> = res
            .bytes_stream()
            .map(|c| c.map(|b| b.to_vec()).map_err(anyhow::Error::from))
            .boxed();

        let stream =
            futures::stream::unfold((byte_stream, SseDecoder::new()), |(mut bytes, mut decoder)| {
                async move {
                    loop {
                        // A complete frame may already sit in the buffer.
                        if let Some(ev) = decoder.take_event() {
                            return Some((Ok(ev), (bytes, decoder)));
                        }
                        match bytes.next().await {
                            Some(Ok(chunk)) => {
                                decoder.absorb(&chunk);
                                continue;
                            }
                            Some(Err(e)) => {
                                return Some((
                                    Err(anyhow!("SSE read: {e}")),
                                    (bytes, decoder),
                                ));
                            }
                            None => return None,
                        }
                    }
                }
            });

        Ok(ChatStream {
            stream: Box::pin(stream),
        })
    }
}

// ---------------------------------------------------------------------------
// SSE decoding
// ---------------------------------------------------------------------------

/// Incremental SSE decoder. Absorbs raw bytes and yields complete frames.
struct SseDecoder {
    buf: Vec<u8>,
}

impl SseDecoder {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Append raw bytes (stripping a leading UTF-8 BOM on first non-empty use).
    fn absorb(&mut self, chunk: &[u8]) {
        if self.buf.is_empty() && chunk.starts_with(&[0xEF, 0xBB, 0xBF]) {
            self.buf.extend_from_slice(&chunk[3..]);
        } else {
            self.buf.extend_from_slice(chunk);
        }
    }

    /// If the buffer holds at least one complete frame (terminated by `\n\n`),
    /// parse and consume it, returning the event. Otherwise `None`.
    fn take_event(&mut self) -> Option<SseEvent> {
        let text = String::from_utf8_lossy(&self.buf).into_owned();
        let pos = text.find("\n\n")?;
        let frame = &text[..pos];
        // Advance the buffer past the consumed frame (byte length of frame + "\n\n").
        let consumed = frame.len() + 2;
        if consumed <= self.buf.len() {
            self.buf.drain(..consumed);
        } else {
            self.buf.clear();
        }
        parse_frame(frame)
    }
}

/// Parse one SSE frame (text between blank-line separators).
///
/// Handles both classic `event:` + `data:` SSE and `data:`-only payloads
/// where the JSON carries its own type field.
fn parse_frame(frame: &str) -> Option<SseEvent> {
    let mut event: Option<String> = None;
    let mut data: Vec<String> = Vec::new();
    for line in frame.lines() {
        let line = line.trim_end();
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.trim().to_string());
        }
    }
    let data_joined = data.join("\n");
    if data_joined.is_empty() {
        return event.map(|e| SseEvent::Other(e, None));
    }
    let json: Value = serde_json::from_str(&data_joined).unwrap_or(Value::Null);

    let name = event
        .clone()
        .or_else(|| {
            json.get("type")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| json.get("event").and_then(|v| v.as_str()).map(|s| s.to_string()))
        });

    match name.as_deref() {
        Some("message") => {
            let content = extract_content(&json);
            let reasoning = json
                .get("reasoning_content")
                .or_else(|| json.get("reasoningContent"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some(SseEvent::Message { content, reasoning })
        }
        Some("end") => Some(SseEvent::End(Some(json))),
        Some("error") => {
            let code = json.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
            let msg = json
                .get("msg")
                .or_else(|| json.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(SseEvent::Error { code, msg })
        }
        _ => Some(SseEvent::Other(
            name.unwrap_or_else(|| "unknown".into()),
            Some(json),
        )),
    }
}

/// Pull the visible text out of a `message` frame.
fn extract_content(json: &Value) -> String {
    if let Some(s) = json.get("content").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    if let Some(inner) = json.get("data") {
        if let Some(s) = inner.get("content").and_then(|v| v.as_str()) {
            return s.to_string();
        }
    }
    String::new()
}

fn truncate(s: &str, n: usize) -> &str {
    if s.len() <= n {
        s
    } else {
        &s[..n]
    }
}

fn now_millis() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Generate a v4-style UUID (RFC 4122 variant) for the per-request `id` field.
/// Reads 16 bytes of entropy from `/dev/urandom`; falls back to a time/pid
/// mix if that file is unavailable.
fn random_uuid_v4() -> String {
    use std::io::Read;
    let mut b = [0u8; 16];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| {
        let _ = f.read_exact(&mut b);
        Ok(())
    }) {
        Ok(()) => {}
        Err(_) => {
            let t = now_millis() as u64;
            let p = std::process::id() as u64;
            for i in 0..16 {
                let shift = (i % 8) * 8;
                let tbyte = ((t >> shift) as u8) & 0xff;
                let pbyte = ((p >> shift) as u8) & 0xff;
                b[i] = tbyte ^ pbyte ^ (i as u8);
            }
        }
    }
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3],
        b[4], b[5],
        b[6], b[7],
        b[8], b[9],
        b[10], b[11], b[12], b[13], b[14], b[15]
    )
}
