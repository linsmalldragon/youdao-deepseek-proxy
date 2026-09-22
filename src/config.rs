use std::env;

/// Configuration for the Youdao upstream (luna-ai) that the proxy mimics.
///
/// Defaults mirror the Youdao Mac dictionary app (not-logged-in / guest state):
/// `client`/`product` = "macdict", `appVersion` = native build, network "wired".
/// Every field can be overridden via `YOUDAO_*` environment variables so the
/// service can be pointed at a logged-in session or a different build without
/// recompiling.
#[derive(Debug, Clone)]
pub struct YoudaoConfig {
    /// Upstream base URL, e.g. `https://luna-ai.youdao.com`.
    pub base_url: String,
    /// Fixed secret used ONLY for the `/translate_llm/secret` call itself.
    pub fixed_secret_key: String,
    /// The client identifier sent in requests (mac dict = "macdict").
    pub client: String,
    /// The product identifier (mac dict = "macdict").
    pub product: String,
    /// Native app version string.
    pub app_version: String,
    /// Vendor (empty on mac -> dropped by signing).
    pub vendor: String,
    /// Model field (app sends "1").
    pub model: String,
    /// screen (empty on mac -> dropped).
    pub screen: String,
    /// imei (empty on mac -> dropped).
    pub imei: String,
    /// mid (device mid; "0" when unknown).
    pub mid: String,
    /// network type ("wired" on mac).
    pub network: String,
    /// yduuid for the chat path. When empty it is dropped by the signer.
    pub yduuid: String,
    /// keyfrom for the chat path. The mac build's 8208 constant is "mac.main"
    /// (49920's `n.keyfrom||"1"` only applies when the native value is absent).
    pub keyfrom: String,
    /// keyfrom for the `/secret` call — the same common-params `keyfrom`
    /// (the app's secret path sends it as a plain, non-signed param).
    pub secret_keyfrom: String,
    /// yduuid for the `/secret` call (the app's default guest value).
    pub secret_yduuid: String,
    /// LLM function name, e.g. "deepseek_r1".
    pub function_english_name: String,
    /// translateType sent to the LLM endpoint ("LLM" for the advanced/DeepSeek model).
    pub translate_type: String,
    /// `free` flag the app sends in the chat body (assistant path uses `false`).
    pub free_flag: String,
    /// `useTerm` field the chat body carries (app sends "0").
    pub use_term: String,
    /// The `User-Agent` the WKWebView app sends on LLM calls
    /// (`... AppleWebKit/... youdaodict/<ver> (jsbridge/...`).
    pub user_agent: String,
    /// `Origin` header sent by the app's webview (`file://` for the mac dict).
    pub origin: String,
    /// `Accept-Language` sent by the app.
    pub accept_language: String,
    /// Optional fixed token to inject (instead of the guest token from /secret).
    /// Set this to the app's real `ydtoken` (logged-in session) when the LLM
    /// endpoints are gated behind a logged-in token.
    pub token_override: Option<String>,
    /// Extra cookie header to send on upstream requests. Set this to the captured
    /// device cookies, e.g. `OUTFOX_SEARCH_USER_ID=...; DICT_DOCTRANS_SESSION_ID=...`
    pub cookie: Option<String>,
    /// Listen bind address for the OpenAI-compatible server.
    pub bind: String,
    /// Context length (in tokens) the proxy reports to clients as
    /// `max_model_len` (`/v1/models`, same field vllm/sglang expose),
    /// `max_context_len` (`/get_model_info`) and `max_total_num_tokens`
    /// (`/server_info`). Youdao does not publish an authoritative limit for
    /// its LLM endpoint, so this is a configurable default matching the
    /// model's self-reported window (128K). Override via YOUDAO_MAX_CONTEXT.
    pub max_context: u32,
}

impl Default for YoudaoConfig {
    fn default() -> Self {
        Self {
            base_url: "https://luna-ai.youdao.com".into(),
            // The fixed secret embedded in the app for the /secret call.
            fixed_secret_key: "EZAmCfVOH2CrBGMtPrtIPUzyv3bheLdk".into(),
            client: "macdict".into(),
            product: "macdict".into(),
            app_version: "11.3.20".into(),
            // Device descriptors captured from the mac dict app (not a credential).
            vendor: "store".into(),
            model: "MacBookPro15,1".into(),
            screen: "50*50".into(),
            imei: String::new(), // device id -> supply via YOUDAO_IMEI
            mid: "15.7.9".into(),
            network: "wifi".into(),
            yduuid: String::new(), // device id -> supply via YOUDAO_YDUUID
            keyfrom: "macdict.11.3.20.mac".into(),
            secret_keyfrom: "macdict.11.3.20.mac".into(),
            secret_yduuid: "abcdefg".into(),
            function_english_name: "deepseek_r1".into(),
            translate_type: "LLM".into(),
            free_flag: "false".into(),
            use_term: "0".into(),
            user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) youdaodict/11.3.20 (jsbridge/1.0;".into(),
            origin: "file://".into(),
            accept_language: "zh-CN,zh-Hans;q=0.9".into(),
            token_override: None,
            cookie: None,
            bind: "127.0.0.1:8080".into(),
            // The Youdao LLM endpoint is a large-context model; the app does not
            // expose an authoritative limit, so report a 128K window by default.
            max_context: 131_072,
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_opt(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.is_empty())
}

impl YoudaoConfig {
    /// Build config from environment overrides layered on top of the defaults.
    pub fn from_env() -> Self {
        let mut c = Self::default();
        c.base_url = env_or("YOUDAO_BASE_URL", &c.base_url);
        c.fixed_secret_key = env_or("YOUDAO_FIXED_SECRET", &c.fixed_secret_key);
        c.client = env_or("YOUDAO_CLIENT", &c.client);
        c.product = env_or("YOUDAO_PRODUCT", &c.product);
        c.app_version = env_or("YOUDAO_APP_VERSION", &c.app_version);
        c.vendor = env_or("YOUDAO_VENDOR", &c.vendor);
        c.model = env_or("YOUDAO_MODEL", &c.model);
        c.screen = env_or("YOUDAO_SCREEN", &c.screen);
        c.imei = env_or("YOUDAO_IMEI", &c.imei);
        c.mid = env_or("YOUDAO_MID", &c.mid);
        c.network = env_or("YOUDAO_NETWORK", &c.network);
        c.yduuid = env_or("YOUDAO_YDUUID", &c.yduuid);
        c.keyfrom = env_or("YOUDAO_KEYFROM", &c.keyfrom);
        c.secret_keyfrom = env_or("YOUDAO_SECRET_KEYFROM", &c.secret_keyfrom);
        c.secret_yduuid = env_or("YOUDAO_SECRET_YDUUID", &c.secret_yduuid);
        c.function_english_name = env_or("YOUDAO_FUNCTION", &c.function_english_name);
        c.translate_type = env_or("YOUDAO_TRANSLATE_TYPE", &c.translate_type);
        c.free_flag = env_or("YOUDAO_FREE", &c.free_flag);
        c.use_term = env_or("YOUDAO_USE_TERM", &c.use_term);
        c.user_agent = env_or("YOUDAO_USER_AGENT", &c.user_agent);
        c.origin = env_or("YOUDAO_ORIGIN", &c.origin);
        c.accept_language = env_or("YOUDAO_ACCEPT_LANGUAGE", &c.accept_language);
        c.token_override = env_opt("YOUDAO_TOKEN");
        c.cookie = env_opt("YOUDAO_COOKIE");
        c.bind = env_or("YOUDAO_BIND", &c.bind);
        c.max_context = env::var("YOUDAO_MAX_CONTEXT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(c.max_context);
        c
    }

    /// Key used to sign the `/secret` request (the fixed embedded key).
    pub fn secret_key(&self) -> &str {
        &self.fixed_secret_key
    }
}
