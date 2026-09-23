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
    /// OpenAI tool definitions. The upstream Youdao LLM is a plain text model
    /// that cannot natively emit `tool_calls`, so when these are present the
    /// proxy injects them into the prompt and parses the model's marker output
    /// back into OpenAI `tool_calls` (see `parse_tool_calls`).
    #[serde(default)]
    pub tools: Option<Vec<serde_json::Value>>,
    /// `"auto"` | `"required"` | `"none"` or a forced function object.
    #[serde(default)]
    pub tool_choice: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Message {
    pub role: String,
    pub content: serde_json::Value,
    /// Assistant tool calls carried in conversation history
    /// (`function.arguments` is a JSON string, per OpenAI).
    #[serde(default)]
    pub tool_calls: Option<Vec<serde_json::Value>>,
    /// `tool_call_id` linking a `tool`-role result to the call that produced it.
    #[serde(default)]
    pub tool_call_id: Option<String>,
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

/// Build the single `input` string the Youdao endpoint consumes by flattening
/// the ENTIRE conversation into one transcript. The upstream endpoint is
/// stateless and takes a single `input` text, so all history (system, user,
/// assistant, tool) must be inlined for the model to see prior turns — sending
/// only the latest user message makes each turn look like a fresh conversation.
///
/// When `tools` is non-empty, a tool-calling protocol block is prepended: the
/// upstream model is a plain text LLM that cannot natively emit OpenAI
/// `tool_calls`, so it is told the tool schemas and a marker format
/// (`[TOOL_CALL]...[/TOOL_CALL]`); the proxy then parses that output back into
/// OpenAI `tool_calls` via `parse_tool_calls`. Assistant-history `tool_calls`
/// and `tool`-role results are inlined so the model retains "what I called and
/// what it returned".
///
/// `round_no` is the count of user turns so far (a fresh single turn -> 1).
pub fn build_chat_input(
    messages: &[Message],
    tools: Option<&[serde_json::Value]>,
    tool_choice: Option<&serde_json::Value>,
) -> (String, u32) {
    let mut user_turns = 0u32;
    let mut lines: Vec<String> = Vec::with_capacity(messages.len() + 1);

    if let Some(tools) = tools {
        if !tools.is_empty() {
            lines.push(tool_protocol(tools, tool_choice));
        }
    }

    for m in messages {
        let label: &str = match m.role.as_str() {
            "user" => {
                user_turns += 1;
                "用户"
            }
            "assistant" => "助手",
            "system" => "系统",
            "tool" => "工具结果",
            other => other,
        };

        let mut body = m.text();
        if m.role == "tool" {
            // Label the result with the call id so the model can map each
            // result to the call that produced it (helps it stop re-checking
            // facts it has already established).
            if let Some(id) = &m.tool_call_id {
                body = format!("[这是 {id} 的返回结果] {body}");
            }
        }
        if let Some(calls) = &m.tool_calls {
            if !calls.is_empty() {
                let rendered = calls
                    .iter()
                    .map(|c| {
                        let id = c.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let name = c
                            .pointer("/function/name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("?");
                        let args = c
                            .pointer("/function/arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}");
                        format!("[我调用了工具 {name} (id={id}) 参数: {args}]")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                body = if body.is_empty() { rendered } else { format!("{body}\n{rendered}") };
            }
        }

        lines.push(format!("{}：{}", label, body));
    }

    let input = lines.join("\n");
    (input, user_turns.max(1))
}

/// Prepend a tool-calling protocol block to the transcript: the list of tool
/// schemas plus the marker format the model must use to request a tool, and
/// how `tool_choice` constrains this turn.
fn tool_protocol(
    tools: &[serde_json::Value],
    tool_choice: Option<&serde_json::Value>,
) -> String {
    let tools_json = serde_json::to_string(tools).unwrap_or_else(|_| "[]".to_string());
    let choice_hint = match tool_choice {
        Some(v) if v.as_str() == Some("required") => "本轮必须至少调用一个工具。".to_string(),
        Some(v) if v.as_str() == Some("none") => "本轮不要调用工具，直接给出最终答案。".to_string(),
        Some(v) if v.get("type").and_then(|t| t.as_str()) == Some("function") => {
            let name = v.pointer("/function/name").and_then(|n| n.as_str()).unwrap_or("");
            format!("本轮必须调用工具 \"{}\"。", name)
        }
        _ => "按需调用工具；如果当前这一步不需要调用工具，直接给出最终答案。".to_string(),
    };
    // Note the doubled braces: `{{`/`}}` render literal `{`/`}` inside the
    // marker example; `{tools_json}`/`{choice_hint}` are the interpolations.
    format!(
        "【工具调用协议】\n\
         你可以调用下列工具来完成用户任务。可用工具（name / description / parameters 的 JSON Schema）：\n\
         <available_tools>\n{tools_json}\n</available_tools>\n\
         {choice_hint}\n\
         调用工具时，输出标记块，每个工具调用一个、可连续多个，格式如下：\n\
         [TOOL_CALL]{{\"name\":\"<工具名>\",\"arguments\":{{...}}}}[/TOOL_CALL]\n\
         其中工具名必须取自上面的工具列表；参数是符合该工具 parameters 的 JSON 对象（无参数用 {{}}）。\n\
         当任务完成、不再需要调用工具时，直接输出最终答案，不要再输出 [TOOL_CALL] 标记。\n\
         【行为准则（务必遵守）】\n\
         1. 行动优先：直接调用工具推进下一步，不要反复复述或重新规划同一件事；\n\
         2. 基于“工具结果”推进：已经得到结果的事实不要再重新查证（例如已确认 kubectl 不存在，就不要再 which kubectl），\n\
            结果不足以推进时换一条不同的、更直接的命令；\n\
         3. 回复保持简短：先给结论/下一步，再给必要的工具调用，避免与执行无关的大段解释；\n\
         4. 一旦信息足够回答用户，立即停止调用工具，直接输出最终答案；\n\
         5. 如果你在本轮描述了下一步动作（例如“先检查 X，然后安装 Y”），必须在本轮以对应的 [TOOL_CALL] 立即执行它，\n\
            禁止只写计划就结束；只有任务确实完成、没有可执行的下一步时，才允许以纯文本收尾；\n\
         6. [TOOL_CALL] 块内 JSON 顶层只允许 name 和 arguments 两个字段，所有参数一律放进 arguments 对象；\n\
            要并行推进多条命令就输出多个 [TOOL_CALL] 块，禁止自造 commands 等其它顶层字段。\n\n"
    )
}

/// Parse the model's marker output into OpenAI-style tool calls.
///
/// The upstream model is a plain text LLM, so we tell it to emit tool calls as
/// `[TOOL_CALL]{...}` blocks. It is *flaky* about the exact shape — it sometimes
/// omits the closing `[/TOOL_CALL]` marker, flattens the arguments into
/// top-level fields (e.g. `{"name":"Bash","command":"..."}` instead of
/// `{"name":"Bash","arguments":{"command":"..."}}`), wraps the JSON in
/// parens (`[TOOL_CALL]({"name":...})`), batches several commands into a
/// top-level `commands` array, or emits a broken block outright (an object
/// that never closes, a stray `]` where the final `}` should be). So this
/// parser is lenient: it grabs the first balanced-JSON object after each
/// opening marker (with or without a closing marker, with or without a
/// paren wrapper) and normalizes it to one or more OpenAI tool calls.
///
/// A marker whose JSON cannot be parsed is a failed tool call, not prose —
/// keeping it would leak raw `[TOOL_CALL]{...}` text into the visible reply
/// and into the conversation history on the next turn. Broken blocks are
/// therefore dropped from `content` (a WARN carries a head of the raw text);
/// the model typically re-emits the intended call on the following turn.
/// A marker with *no* JSON after it is kept as literal text, and any later
/// markers are still parsed.
///
/// Returns the cleaned `content` (unparseable blocks removed) and the
/// `tool_calls`, each shaped `{id, type:"function", function:{name, arguments:"<json>"}}`.
pub fn parse_tool_calls(content: &str) -> (String, Vec<serde_json::Value>) {
    const OPEN: &str = "[TOOL_CALL]";

    let mut cleaned = String::with_capacity(content.len());
    let mut calls: Vec<serde_json::Value> = Vec::new();
    let mut i = 0usize;
    let n = content.len();

    while i < n {
        match content[i..].find(OPEN) {
            None => {
                cleaned.push_str(&content[i..]);
                break;
            }
            Some(rel) => {
                let mark_start = i + rel;
                cleaned.push_str(&content[i..mark_start]);
                // Skip wrapper chars (whitespace and `(`) to the first '{'.
                // The model sometimes wraps the JSON in parens:
                // `[TOOL_CALL]({"name":...})` — only `(` / whitespace are
                // skipped, so real prose before a `{` still reads as "no
                // JSON after the marker" (kept literal, not swallowed).
                let mut k = mark_start + OPEN.len();
                while k < n {
                    let ch = content[k..].chars().next().unwrap();
                    if ch.is_whitespace() || ch == '(' {
                        k += ch.len_utf8();
                    } else {
                        break;
                    }
                }

                if k < n && content[k..].starts_with('{') {
                    match extract_balanced_json(content, k) {
                        Some((json_str, end)) => match serde_json::from_str::<serde_json::Value>(
                            &json_str,
                        ) {
                            Ok(obj) => {
                                let (name, args_list) = normalize_tool_call(&obj);
                                for args in args_list {
                                    let id = format!("call_{}", calls.len());
                                    calls.push(serde_json::json!({
                                        "id": id,
                                        "type": "function",
                                        "function": { "name": &name, "arguments": args }
                                    }));
                                }
                                i = consume_past_close_marker(content, end);
                                continue;
                            }
                            Err(e) => {
                                // Balanced braces but not valid JSON: drop the
                                // block instead of leaking raw marker text.
                                tracing::warn!(
                                    error = %e,
                                    head = %drop_head(&json_str),
                                    "dropping unparseable [TOOL_CALL] block"
                                );
                                i = consume_past_close_marker(content, end);
                                continue;
                            }
                        },
                        None => {
                            // The object never closes — it may run straight
                            // into the next marker or end the reply. Drop this
                            // block up to the next marker (or EOF) so later
                            // calls still parse.
                            let region_end = content[k..]
                                .find(OPEN)
                                .map(|r| k + r)
                                .unwrap_or(n);
                            tracing::warn!(
                                head = %drop_head(&content[k..region_end]),
                                "dropping unbalanced [TOOL_CALL] block"
                            );
                            i = region_end;
                            continue;
                        }
                    }
                }
                // No JSON after the marker: keep it as literal text, keep
                // scanning — a later marker may still be a valid call.
                cleaned.push_str(OPEN);
                i = mark_start + OPEN.len();
            }
        }
    }
    (cleaned, calls)
}

/// Move past an optional `[/TOOL_CALL]` closing marker (and the whitespace
/// before it) that follows a parsed JSON span.
fn consume_past_close_marker(content: &str, end: usize) -> usize {
    const CLOSE: &str = "[/TOOL_CALL]";
    let mut m = end;
    while m < content.len() {
        let ch = content[m..].chars().next().unwrap();
        // Skip wrapper chars on the closing side (whitespace and `)` — the
        // close of the paren-wrapped shape `[TOOL_CALL]({json})`), then the
        // optional `[/TOOL_CALL]` marker.
        if ch.is_whitespace() || ch == ')' {
            m += ch.len_utf8();
        } else {
            break;
        }
    }
    if content[m..].starts_with(CLOSE) {
        m += CLOSE.len();
    }
    m
}

/// Short head of a dropped raw block for WARN logs.
fn drop_head(s: &str) -> String {
    s.chars().take(160).collect()
}

/// Given `s[start..]` begins with `{`, return the first balanced JSON object
/// `(substring, end_index_past_closing_brace)`. Braces inside string literals
/// (and escaped quotes) are ignored. `None` if no closing brace is found.
fn extract_balanced_json(s: &str, start: usize) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut esc = false;
    let mut i = start;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some((s[start..=i].to_string(), i + 1));
                    }
                }
                // A top-level `[` is also a legal array value (e.g. the
                // batched `"commands":[...]` shape the model sometimes emits),
                // so only stop when we've run past a truncated object
                // straight into the next `[TOOL_CALL]` marker.
                b'[' if depth >= 1 && s[i..].starts_with("[TOOL_CALL]") => return None,
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// Normalize one parsed tool-call JSON object to an OpenAI
/// `(name, [arguments...])` pair — one arguments-object string per call.
///
/// Shape precedence (the model is flaky about which one it emits):
/// 1. a non-empty `arguments` object (or a JSON-string `arguments`);
/// 2. a top-level `commands` array — the batched shape the model sometimes
///    invents (`{"name":"Bash","commands":[{...},{...}],"arguments":{}}`);
///    each entry becomes its own call;
/// 3. flattened top-level fields (minus the structural keys).
fn normalize_tool_call(obj: &serde_json::Value) -> (String, Vec<String>) {
    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut args_list: Vec<String> = Vec::new();
    let args_from_arguments = match obj.get("arguments") {
        Some(serde_json::Value::Object(m)) if !m.is_empty() => Some(obj["arguments"].to_string()),
        Some(serde_json::Value::String(s)) => {
            let v = serde_json::from_str::<serde_json::Value>(s)
                .unwrap_or_else(|_| serde_json::Value::Object(Default::default()));
            Some(v.to_string())
        }
        _ => None,
    };
    if let Some(a) = args_from_arguments {
        args_list.push(a);
    } else if let Some(cmds) = obj.get("commands").and_then(|v| v.as_array()) {
        for c in cmds {
            match c {
                serde_json::Value::Object(_) => args_list.push(c.to_string()),
                serde_json::Value::String(s) => {
                    args_list.push(serde_json::json!({ "command": s }).to_string())
                }
                _ => {}
            }
        }
    }
    if args_list.is_empty() {
        let mut m = serde_json::Map::new();
        for (k, v) in obj.as_object().into_iter().flatten() {
            if k == "name" || k == "description" || k == "commands" || k == "arguments" {
                continue;
            }
            m.insert(k.clone(), v.clone());
        }
        args_list.push(serde_json::Value::Object(m).to_string());
    }
    (name, args_list)
}

/// Build an OpenAI non-streaming completion response body.
///
/// `input` is the user prompt the upstream saw, used to fill the `usage`
/// counts (estimated, see `est_tokens`). `tool_calls` is forwarded verbatim
/// when the upstream emitted tool calls (empty for plain text turns).
pub fn completion_response(
    id: &str,
    model: &str,
    input: &str,
    content: &str,
    reasoning: &str,
    finish: &str,
    tool_calls: &[serde_json::Value],
) -> serde_json::Value {
    let mut message = serde_json::json!({
        "role": "assistant",
        "content": content,
    });
    // Surface the model's chain-of-thought when present (DeepSeek R1).
    if !reasoning.is_empty() {
        message["reasoning_content"] = serde_json::json!(reasoning);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = serde_json::json!(tool_calls);
    }
    let prompt_tokens = est_tokens(input);
    let completion_tokens = est_tokens(content) + est_tokens(reasoning);
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
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens
        }
    })
}

/// OpenAI legacy (text) completion request: `prompt` may be a string or a
/// list of strings; only the last non-empty entry is forwarded upstream.
#[derive(Debug, Deserialize)]
pub struct CompletionRequest {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub prompt: serde_json::Value,
    #[serde(default)]
    pub stream: bool,
    // Accepted but not forwarded (Youdao's endpoint manages its own sampling).
    #[serde(default)]
    #[allow(dead_code)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    #[allow(dead_code)]
    pub temperature: Option<f64>,
}

/// Flatten `CompletionRequest::prompt` (string or list) to its entries.
pub fn prompt_texts(prompt: &serde_json::Value) -> Vec<String> {
    match prompt {
        serde_json::Value::String(s) => vec![s.clone()],
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

/// Build an OpenAI legacy non-streaming completion response body.
pub fn text_completion_response(
    id: &str,
    model: &str,
    prompt: &str,
    text: &str,
    finish: &str,
) -> serde_json::Value {
    let prompt_tokens = est_tokens(prompt);
    let completion_tokens = est_tokens(text);
    serde_json::json!({
        "id": id,
        "object": "text_completion",
        "created": now_secs(),
        "model": model,
        "prompt": prompt,
        "choices": [
            { "text": text, "index": 0, "finish_reason": finish }
        ],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens
        }
    })
}

/// Build one OpenAI legacy streaming chunk as an SSE `data:` frame.
pub fn text_completion_chunk(
    id: &str,
    model: &str,
    prompt: &str,
    delta: &str,
    finish: Option<&str>,
) -> String {
    let obj = serde_json::json!({
        "id": id,
        "object": "text_completion",
        "created": now_secs(),
        "model": model,
        "choices": [
            { "text": delta, "index": 0, "finish_reason": finish }
        ],
        "prompt": prompt
    });
    format!("data: {}\n\n", obj)
}

/// Rough token estimate for `usage` accounting (no local tokenizer):
/// non-ASCII (mostly CJK) chars count ~1 token each, ASCII ~1 per 4 chars.
/// Close enough for cost/limit bookkeeping against the upstream.
pub fn est_tokens(s: &str) -> u32 {
    if s.is_empty() {
        return 0;
    }
    let mut cjk = 0u32;
    let mut ascii = 0u32;
    for c in s.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            cjk += 1;
        }
    }
    cjk + ascii / 4 + 1
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

pub(crate) fn now_secs() -> i64 {
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

/// Generate a cmpl-style id (legacy text completions).
pub fn new_completion_id() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .to_string();
    format!("cmpl-{}-{}", now_secs(), n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_closing_marker_canonical_arguments() {
        let content = "我先看看。[TOOL_CALL]{\"name\":\"Bash\",\"arguments\":{\"command\":\"kubectl get pods\"}}[/TOOL_CALL]\n然后修。";
        let (cleaned, calls) = parse_tool_calls(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].pointer("/function/name").unwrap(), "Bash");
        assert_eq!(
            calls[0].pointer("/function/arguments").unwrap().as_str().unwrap(),
            "{\"command\":\"kubectl get pods\"}"
        );
        assert!(!cleaned.contains("[TOOL_CALL]"));
        assert!(cleaned.contains("我先看看。") && cleaned.contains("然后修。"));
    }

    #[test]
    fn parses_opening_marker_only_flattened_fields() {
        // Mirrors the workant deviation: opening marker only (no closing),
        // arguments flattened into top-level fields, plus a `description`.
        let content = concat!(
            "我来查看集群中 pod 的状态，先了解整体情况。\n",
            "[TOOL_CALL]{\"name\":\"Bash\",\"command\":\"kubectl get pods -A -o wide 2>&1 | head -100\",\"description\":\"查看所有命名空间的 pod 状态\"}\n",
            "[TOOL_CALL]{\"name\":\"Bash\",\"command\":\"kubectl get nodes -o wide 2>&1\",\"description\":\"查看节点状态\"}",
        );
        let (cleaned, calls) = parse_tool_calls(content);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].pointer("/function/name").unwrap(), "Bash");
        assert_eq!(
            calls[0].pointer("/function/arguments").unwrap().as_str().unwrap(),
            "{\"command\":\"kubectl get pods -A -o wide 2>&1 | head -100\"}"
        );
        assert_eq!(
            calls[1].pointer("/function/arguments").unwrap().as_str().unwrap(),
            "{\"command\":\"kubectl get nodes -o wide 2>&1\"}"
        );
        // No stray markers or JSON leaked into the visible content.
        assert!(!cleaned.contains("[TOOL_CALL]"));
        assert!(!cleaned.contains("kubectl"));
        assert!(cleaned.contains("我来查看集群中 pod 的状态"));
    }

    #[test]
    fn parses_batched_commands_array_shape() {
        // Regression: live session "排查并修复本地服务故障" — the model batched
        // commands into a top-level `commands` array with an empty
        // `arguments` object. The old extractor aborted at the array's `[`
        // and leaked the whole marker into the visible content.
        let content = concat!(
            "继续精确定位。\n\n",
            "[TOOL_CALL]{\"name\":\"Bash\",\"commands\":[",
            "{\"command\":\"df -h /var/lib/containerd 2>/dev/null; kubectl top node 2>/dev/null | head\",\"description\":\"查看 d1 节点磁盘与资源占用\"},",
            "{\"command\":\"kubectl exec -n default my-release-etcd-0 -- sh -c 'etcdctl endpoint health' 2>&1\",\"description\":\"检查 etcd 集群健康\"}",
            "],\"arguments\":{}}"
        );
        let (cleaned, calls) = parse_tool_calls(content);
        assert!(!cleaned.contains("[TOOL_CALL]"), "marker leaked: {cleaned}");
        assert!(cleaned.contains("继续精确定位"));
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].pointer("/function/name").unwrap(), "Bash");
        let args0: serde_json::Value = serde_json::from_str(
            calls[0].pointer("/function/arguments").unwrap().as_str().unwrap(),
        )
        .unwrap();
        assert!(args0["command"].as_str().unwrap().starts_with("df -h /var/lib/containerd"));
        assert_eq!(args0["description"], "查看 d1 节点磁盘与资源占用");
        let args1: serde_json::Value = serde_json::from_str(
            calls[1].pointer("/function/arguments").unwrap().as_str().unwrap(),
        )
        .unwrap();
        assert!(args1["command"].as_str().unwrap().starts_with("kubectl exec"));
    }

    #[test]
    fn leaves_plain_text_intact() {
        let (cleaned, calls) = parse_tool_calls("答案就是 42。");
        assert!(calls.is_empty());
        assert_eq!(cleaned, "答案就是 42。");
    }

    #[test]
    fn drops_unbalanced_final_marker_keeps_earlier_calls() {
        // Regression, live session "排查kubectl服务故障原因": the model emitted
        // two valid markers, then a final one whose JSON object never closed
        // (a stray `]` where the outer `}` should be). The old "keep it as
        // literal text" fallback leaked the raw `[TOOL_CALL]{...}` block into
        // the visible reply while the earlier calls still executed.
        let content = concat!(
            "\n\nd1 节点出现 DiskPressure 并触发驱逐，另有 milvus-proxy 探针失败。继续查节点资源与异常 Pod 详情。\n\n",
            "[TOOL_CALL]{\"name\":\"Bash\",\"arguments\":{\"command\":\"kubectl describe node d1 2>&1 | tail -60\",\"description\":\"查看 d1 节点资源状况与驱逐原因\"}}\n",
            "[TOOL_CALL]{\"name\":\"Bash\",\"arguments\":{\"command\":\"kubectl describe pod my-release-milvus-proxy-0 -n default 2>&1 | tail -40\",\"description\":\"查看 milvus-proxy 异常详情\"}}\n",
            "[TOOL_CALL]{\"name\":\"Bash\",\"arguments\":{\"command\":\"kubectl top nodes 2>&1\",\"description\":\"查看节点资源使用概况\"}]",
        );
        let (cleaned, calls) = parse_tool_calls(content);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].pointer("/function/name").unwrap(), "Bash");
        let args0: serde_json::Value =
            serde_json::from_str(calls[0].pointer("/function/arguments").unwrap().as_str().unwrap())
                .unwrap();
        assert!(args0["command"].as_str().unwrap().starts_with("kubectl describe node d1"));
        let args1: serde_json::Value =
            serde_json::from_str(calls[1].pointer("/function/arguments").unwrap().as_str().unwrap())
                .unwrap();
        assert!(args1["command"]
            .as_str()
            .unwrap()
            .starts_with("kubectl describe pod my-release-milvus-proxy-0"));
        assert!(cleaned.contains("d1 节点出现 DiskPressure"));
        assert!(!cleaned.contains("[TOOL_CALL]"), "marker leaked: {cleaned}");
        assert!(!cleaned.contains("kubectl"), "broken block leaked: {cleaned}");
    }

    #[test]
    fn drops_balanced_but_invalid_json_span() {
        // A raw control character inside a JSON string keeps the braces
        // balanced but makes serde_json reject the span; drop it and keep
        // parsing the later valid marker.
        let content = format!(
            "继续定位。\n\
             [TOOL_CALL]{{\"name\":\"Bash\",\"arguments\":{{\"command\":\"df -h\nwhoami\",\"description\":\"查磁盘\"}}}}\n\
             [TOOL_CALL]{{\"name\":\"Bash\",\"arguments\":{{\"command\":\"kubectl get pods\",\"description\":\"查 Pod\"}}}}"
        );
        let (cleaned, calls) = parse_tool_calls(&content);
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].pointer("/function/arguments").unwrap().as_str().unwrap(),
            "{\"command\":\"kubectl get pods\",\"description\":\"查 Pod\"}"
        );
        assert!(cleaned.contains("继续定位"));
        assert!(!cleaned.contains("[TOOL_CALL]"), "marker leaked: {cleaned}");
        assert!(!cleaned.contains("df -h"), "broken block leaked: {cleaned}");
    }

    #[test]
    fn drops_unbalanced_block_that_runs_into_next_marker() {
        // An unclosed object that runs straight into the next marker used to
        // abort the whole parse (everything kept literal, zero calls). Now
        // the broken block is dropped and the later marker still parses.
        let content = concat!(
            "[TOOL_CALL]{\"name\":\"Bash\",\"arguments\":{\"command\":\"echo hi\"}\n",
            "[TOOL_CALL]{\"name\":\"Bash\",\"arguments\":{\"command\":\"uptime\"}}",
        );
        let (cleaned, calls) = parse_tool_calls(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].pointer("/function/arguments").unwrap().as_str().unwrap(),
            "{\"command\":\"uptime\"}"
        );
        assert_eq!(cleaned, "");
    }

    #[test]
    fn keeps_bare_marker_literal_and_parses_later_marker() {
        // A marker with no JSON at all stays literal text (the model may be
        // quoting the protocol); a later valid marker still parses.
        let content = "格式是 [TOOL_CALL] 后跟 JSON，例如：\n[TOOL_CALL]{\"name\":\"Bash\",\"arguments\":{\"command\":\"uptime\"}}";
        let (cleaned, calls) = parse_tool_calls(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].pointer("/function/arguments").unwrap().as_str().unwrap(),
            "{\"command\":\"uptime\"}"
        );
        assert!(cleaned.contains("格式是 [TOOL_CALL] 后跟 JSON"));
        assert!(!cleaned.contains("uptime"));
    }

    #[test]
    fn parses_paren_wrapped_json_marker() {
        // Regression, live session "排查本地 kubectl服务故障" (2026-09-23): the
        // model wrapped every tool-call JSON in parens,
        // `[TOOL_CALL]({"name":...})[/TOOL_CALL]`. The old parser required `{`
        // to follow the opening marker (modulo whitespace), saw `(`, and fell
        // into the "no JSON, keep literal" branch — leaking every marker into
        // the visible content and yielding zero tool_calls.
        let content = concat!(
            "\n\n我来查 d4 节点上的 pod 分布、内存配额和历史惩罚记录。\n\n",
            "[TOOL_CALL]({\"name\":\"Bash\",\"arguments\":{\"command\":\"kubectl get pods -A --field-selector spec.nodeName=d4 -o wide\",\"description\":\"查看 d4 节点上的所有 pod 分布\"}})[/TOOL_CALL]\n",
            "[TOOL_CALL]({\"name\":\"Bash\",\"arguments\":{\"command\":\"kubectl describe node d4 | grep -A30 'Non-terminated Pods'\",\"description\":\"查看 d4 节点的 pod 资源分配情况\"}})[/TOOL_CALL]",
        );
        let (cleaned, calls) = parse_tool_calls(content);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].pointer("/function/name").unwrap(), "Bash");
        let args0: serde_json::Value =
            serde_json::from_str(calls[0].pointer("/function/arguments").unwrap().as_str().unwrap())
                .unwrap();
        assert!(args0["command"]
            .as_str()
            .unwrap()
            .starts_with("kubectl get pods -A --field-selector spec.nodeName=d4"));
        assert_eq!(
            calls[1].pointer("/function/arguments").unwrap().as_str().unwrap(),
            "{\"command\":\"kubectl describe node d4 | grep -A30 'Non-terminated Pods'\",\"description\":\"查看 d4 节点的 pod 资源分配情况\"}"
        );
        assert!(cleaned.contains("我来查 d4 节点上的 pod 分布"));
        assert!(!cleaned.contains("[TOOL_CALL]"), "marker leaked: {cleaned}");
        assert!(!cleaned.contains("kubectl"), "paren-wrapped block leaked: {cleaned}");
    }
}
