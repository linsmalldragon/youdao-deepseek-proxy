# youdao-deepseek-proxy

A small **Rust** microservice that bridges the LLM backend behind NetEase's
Youdao Dictionary app — `luna-ai.youdao.com` — and exposes it as a standard
**OpenAI / vLLM-compatible** API (`POST /v1/chat/completions`, JSON and SSE
streaming, including the model's `reasoning_content` chain of thought), plus
**sglang-native** endpoints (`/generate`, `/server_info`, `/get_model_info`,
`/model_info`).

It reproduces the exact request the Youdao macOS client sends, so any
OpenAI-compatible client (a chat UI, the `openai` SDK, a coding agent, etc.)
can be pointed at it to reach the DeepSeek model Youdao serves under the
hood. When a request carries `tools`, the proxy emulates OpenAI tool calling
end to end (see [Tool calling](#tool-calling)).

> ⚠️ **Read the [Legal](#legal) section before using.** This talks to the
> proprietary backend of a commercial app using reverse-engineed request
> signatures. It is for personal study and evaluation only.

## How it works

Youdao's Mac client is a WKWebView app. Its LLM calls go to
`https://luna-ai.youdao.com` (`/translate_llm/secret` + `/translate_llm/v3/chat`)
with an `application/x-www-form-urlencoded` body that must be **signed**:

```
sign = md5( k1=v1&k2=v2&...&key=<secretKey> )
```

The keys are the non-empty request params, sorted (a `BTreeMap`), with `key`
(a per-session `secretKey` fetched from `/secret`) appended last. `pointParam`
is the same sorted key list, comma-joined. This proxy implements that in
[`src/youdao/sign.rs`](src/youdao/sign.rs), then streams the upstream SSE back
as OpenAI chunks:

| Upstream event              | OpenAI frame                              |
| --------------------------- | ---------------------------------------- |
| `data.content` delta        | chunk with `delta.content`               |
| `data.reasoning` (R1-style) | chunk with `delta.reasoning_content`     |
| `end`                       | final `finish_reason: "stop"` chunk      |
| `error`                     | error frame                              |

## Endpoints

| Route                                  | Method | Description                                            |
| -------------------------------------- | ------ | ------------------------------------------------------ |
| `/v1/chat/completions`                 | POST   | OpenAI-compatible chat; `"stream": true` for SSE, `false` for one JSON object |
| `/v1/completions`                      | POST   | OpenAI legacy text completion                          |
| `/v1/models`                           | GET    | Single `deepseek-r1` entry with `max_model_len`        |
| `/generate`                            | POST   | sglang-native generation (`text` + `sampling_params`, batched) |
| `/server_info` (alias `/get_server_info`) | GET  | sglang server info incl. `max_total_num_tokens`        |
| `/get_model_info`                      | GET    | sglang model info incl. `max_context_len`              |
| `/model_info`                          | GET    | sglang model info incl. `max_model_len`                |
| `/health`                              | GET    | Liveness probe (`"ok"`)                                |

The request `model` field is echoed back; upstream always uses the configured
`function_english_name` (default `deepseek_r1`).

### Tool calling

The upstream Youdao LLM is a plain text model — it cannot emit OpenAI
`tool_calls` natively. When a request's `tools` array is non-empty the proxy
injects the tool schemas plus a `[TOOL_CALL]{...}` marker protocol into the
prompt, and parses the model's marker output back into OpenAI `tool_calls`
(`finish_reason: "tool_calls"`, one delta per call in streaming mode). The
parser is deliberately lenient: the model is flaky about the exact shape
(missing closing marker, flattened arguments, or a batched top-level
`commands` array), and all of those are normalized into calls.

Multi-turn agent history is flattened into the stateless single-`input` the
upstream endpoint expects, so an agentic loop (assistant `tool_calls` +
`tool`-role results) keeps working. `coding_demo.sh` in the repo drives a
small coding task end-to-end through the local service.

## Build

```bash
cargo build --release
# binary lands in target/release/youdao-llm-proxy
```

## Run

```bash
# guest-mode defaults: listens on 127.0.0.1:8080
./target/release/youdao-llm-proxy

# or, during development:
cargo run
```

## Prebuilt binaries (GitHub Releases)

Prebuilt binaries are published for every **version tag** (`vX.Y.Z`) at
[Releases](https://github.com/linsmalldragon/youdao-deepseek-proxy/releases).
Pick the asset for your platform, verify it, unpack, and run.

| Asset                                    | Platform                    |
| ---------------------------------------- | --------------------------- |
| `youdao-llm-proxy-aarch64-apple-darwin`  | macOS, Apple Silicon (arm64) |
| `youdao-llm-proxy-x86_64-unknown-linux-gnu` | Linux x86_64 (amd64)      |

```bash
# <tag> = the release tag you're downloading, e.g. v0.1.0
base=https://github.com/linsmalldragon/youdao-deepseek-proxy/releases/download/<tag>

# 1) download the asset + its SHA-256  (Apple Silicon example)
curl -LO ${base}/youdao-llm-proxy-aarch64-apple-darwin.tar.gz
curl -LO ${base}/youdao-llm-proxy-aarch64-apple-darwin.tar.gz.sha256

# 2) verify the download
shasum -a 256 -c youdao-llm-proxy-aarch64-apple-darwin.tar.gz.sha256   # macOS
# sha256sum -c youdao-llm-proxy-aarch64-apple-darwin.tar.gz.sha256     # Linux

# 3) unpack
tar -xzf youdao-llm-proxy-aarch64-apple-darwin.tar.gz

# 4) run  (the executable lives inside the unpacked folder)
./youdao-llm-proxy-aarch64-apple-darwin/youdao-llm-proxy
# -> listens on http://127.0.0.1:8080 by default

# optional: inject device credentials before starting
YOUDAO_YDUUID=... YOUDAO_IMEI=... YOUDAO_COOKIE=... \
  ./youdao-llm-proxy-aarch64-apple-darwin/youdao-llm-proxy
```

Notes:
- The archive contains `youdao-llm-proxy` (the executable) plus a `BUILD` file
  recording the version, target, and commit. If the binary lost its execute bit
  on unpack, `chmod +x` it.
- Match the asset to your CPU: the arm64 macOS binary will not run on an Intel
  Mac, and the Linux asset is x86_64 only.
- Releases are cut per version tag only. To publish one: bump `version` in
  `Cargo.toml`, commit it to `main`, then `git tag vX.Y.Z && git push origin
  vX.Y.Z` — the workflow builds every platform and publishes at that tag.

## Docker

Build the image locally and run it:

```bash
docker build -t youdao-llm-proxy .

# publish the port; pass device credentials via -e if needed
docker run --rm -p 8080:8080 \
  -e YOUDAO_YDUUID=... -e YOUDAO_IMEI=... -e YOUDAO_COOKIE=... \
  youdao-llm-proxy
# -> http://127.0.0.1:8080
```

The image binds `0.0.0.0:8080` by default (override with `-e
YOUDAO_BIND=...`) and runs as a non-root user.

A multi-arch image (`linux/amd64`, `linux/arm64`) is published to
[Docker Hub](https://hub.docker.com/r/linsmalldragon/youdao-deepseek-proxy)
whenever a `vX.Y.Z` tag is pushed:

```bash
docker pull linsmalldragon/youdao-deepseek-proxy:latest
# or pin a version:
docker pull linsmalldragon/youdao-deepseek-proxy:v0.1.0
```

## Configuration

Everything is a `YOUDAO_*` env var layered over built-in defaults that mirror
the Youdao macOS dict app. The full field list is in
[`src/config.rs`](src/config.rs).

| Variable              | Default                       | Purpose                              |
| --------------------- | ----------------------------- | ----------------------------------- |
| `YOUDAO_BASE_URL`     | `https://luna-ai.youdao.com`  | upstream                            |
| `YOUDAO_BIND`         | `127.0.0.1:8080`              | local listen address                |
| `YOUDAO_FUNCTION`     | `deepseek_r1`                 | LLM function (Youdao's "DeepSeek") |
| `YOUDAO_IMEI`         | *(empty)*                     | device identity (see note)         |
| `YOUDAO_YDUUID`       | *(empty)*                     | device identity (see note)         |
| `YOUDAO_COOKIE`       | *(empty)*                     | device cookies for the upstream     |
| `YOUDAO_TOKEN`        | *(empty)*                     | override the guest token (logged-in `ydtoken`) |
| `YOUDAO_MAX_CONTEXT`  | `131072`                      | context window reported to clients as `max_model_len` / `max_context_len` / `max_total_num_tokens` |

Other `YOUDAO_*` vars — `client`/`product`/version, `keyfrom`, request
headers, etc. — follow the defaults table in [`src/config.rs`](src/config.rs).

### Device identity note

The upstream gates LLM calls behind a **coherent device session**: a
registered `yduuid`, a guest token that matches that `yduuid`, and the
matching device cookies. The pure guest defaults let the service compile and
start, but a real chat call usually needs your environment's device
credentials via `YOUDAO_IMEI` / `YOUDAO_YDUUID` / `YOUDAO_COOKIE` (or a
logged-in `YOUDAO_TOKEN`).

## Usage

```bash
# non-streaming
curl -s http://127.0.0.1:8080/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-r1","stream":false,
       "messages":[{"role":"user","content":"你好"}]}'

# streaming (SSE)
curl -sN http://127.0.0.1:8080/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-r1","stream":true,
       "messages":[{"role":"user","content":"写一首五言绝句"}]}'
```

The response carries both `content` and `reasoning_content` (the chain of
thought) when the upstream model provides it.

### OpenAI SDK

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:8080/v1", api_key="unused")

# plain chat
resp = client.chat.completions.create(
    model="deepseek-r1",
    messages=[{"role": "user", "content": "写一首五言绝句"}],
)
print(resp.choices[0].message.content)

# tool calling: run the returned calls yourself, append the results as
# `tool`-role messages (with `tool_call_id`), and call again until
# `finish_reason` is "stop"
TOOLS = [{
    "type": "function",
    "function": {
        "name": "Bash",
        "description": "Run a shell command on the local machine.",
        "parameters": {
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"],
        },
    },
}]
resp = client.chat.completions.create(
    model="deepseek-r1",
    messages=[{"role": "user", "content": "检查本机 brew 是否可用"}],
    tools=TOOLS,
    tool_choice="auto",
)
for tc in resp.choices[0].message.tool_calls or []:
    print(tc.function.name, tc.function.arguments)
```

### sglang-native

```bash
# one-shot generation
curl -s http://127.0.0.1:8080/generate \
  -H 'Content-Type: application/json' \
  -d '{"text":"你好","stream":false}'

# retrieve the context window
curl -s http://127.0.0.1:8080/model_info
# -> "max_model_len": 131072  (override the default with YOUDAO_MAX_CONTEXT)
```

## Security

- Device identifiers (`YOUDAO_IMEI`, `YOUDAO_YDUUID`, `YOUDAO_COOKIE`) and
  `YOUDAO_TOKEN` are **your credentials**. Supply them only through the
  environment for local use. **Never** commit them to a repository or send
  them anywhere.
- `YOUDAO_FIXED_SECRET` is a key embedded in the app for the `/secret` call.
  It is not a personal credential, but it is proprietary to the app.

## Legal

This project reverse-engineers the private backend and request-signing scheme
of a commercial app (NetEase Youdao). It is provided **as-is, for personal
study and evaluation only**, without any warranty. Use of the upstream backend
may be subject to Youdao's terms of service and to the laws of your
jurisdiction. You are solely responsible for how you use this service.

## License

[MIT](LICENSE).
