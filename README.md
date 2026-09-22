# youdao-deepseek-proxy

A small **Rust** microservice that bridges the LLM backend behind NetEase's
Youdao Dictionary app — `luna-ai.youdao.com` — and exposes it as a standard
**OpenAI / vLLM-compatible** `POST /v1/chat/completions` endpoint (JSON and
SSE streaming, including the model's `reasoning_content` chain of thought).

It reproduces the exact request the Youdao macOS client sends, so any
OpenAI-compatible client (a chat UI, the `openai` SDK, etc.) can be pointed at
it to reach the DeepSeek model Youdao serves under the hood.

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

| Route                    | Method | Description                                            |
| ------------------------ | ------ | ------------------------------------------------------ |
| `/v1/chat/completions`   | POST   | OpenAI-compatible chat; `"stream": true` for SSE, `false` for one JSON object |
| `/v1/models`             | GET    | Lists a single `deepseek-r1` entry                     |
| `/health`                | GET    | Liveness probe (`"ok"`)                                |

The request `model` field is echoed back; upstream always uses the configured
`function_english_name` (default `deepseek_r1`).

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
