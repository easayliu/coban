//! Codex OAuth 常量与上游端点。
//!
//! 这些是 Codex CLI（`@openai/codex`）官方客户端使用的公开 OAuth 参数与请求端点，
//! coban 复用它们以完成「用 ChatGPT 订阅账号登录」并按官方客户端形态转发。
//!
//! **取值来源**：codex 发行二进制（`vendor/*/codex/codex`）内的字面量，以及本机
//! `~/.codex/auth.json` 里 id_token 的实际 claim。写死在这里而不是运行时探测：
//! 探测不到时的退化行为（拿一个猜的 client_id 去换 token）比编译期写死更难排查。
//!
//! 唯一的例外是**模型清单**——它随上游上新变化，写死那一刻就开始过期，故运行时向上游取
//! （见 [`MODELS_PATH`] 与 [`crate::proxy::list_models`]）。

// ---------- OAuth ----------

/// Codex CLI 公开 OAuth Client ID。
///
/// 同时是 id_token 的 `aud`——换回来的 token 若 `aud` 不是这个值，说明授权页上选的
/// 不是 Codex 这个应用，那种 token 打 `backend-api/codex` 会被拒。
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// OAuth 签发方（id_token 的 `iss`）。
pub const ISSUER: &str = "https://auth.openai.com";

/// 授权页地址（用户在浏览器打开、登录并同意授权）。
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";

/// Token 交换 / 刷新端点。
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// 官方客户端的 redirect_uri：codex CLI 登录时在本机 1455 端口起一个临时 HTTP 服务收回调。
///
/// **coban 不起那个服务，但必须报同一个值**：token 端点会把 `redirect_uri` 与授权时那次
/// 逐字节比对，报别的值直接 `invalid_grant`。于是登录走「手动粘贴」：浏览器最后会跳到一个
/// 连不上的 `localhost:1455/auth/callback?code=…&state=…`，地址栏里那串就是全部所需，
/// 用户把它整条粘回网页即可（见 [`crate::web::exchange`]）。
///
/// 也因此**不要**改成 coban 自己的地址：那要求授权页允许一个未注册的 redirect_uri，
/// OpenAI 侧会在授权阶段就拒掉。
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

/// 申请的 OAuth scope，与 Codex CLI 保持一致（含顺序——scope 集合也是指纹的一部分）。
///
/// `offline_access` 是 refresh_token 的准入：缺了它换回来的响应里没有 refresh_token，
/// 一小时后账号就掉线且无法自动续期，而报错只会是个平淡的 401。
pub const SCOPES: &str = "openid profile email offline_access";

/// id_token 里那组 ChatGPT 专有 claim 的命名空间前缀。
///
/// 完整 key 形如 `https://api.openai.com/auth.chatgpt_account_id`——**注意是带点号的
/// 扁平 key，不是嵌套对象**，`serde` 的默认结构体映射对不上，故 [`crate::oauth`] 里按
/// 字符串 key 直接取。踩过一次：按嵌套解出来永远是 `None`，而账号 id 拿不到时转发一律 401。
pub const ID_TOKEN_CLAIM_NS: &str = "https://api.openai.com/auth";

/// 提前多久刷新 access_token（秒）。
///
/// id_token/access_token 的有效期是 1 小时（实测 `exp - iat = 3600`），留 5 分钟余量：
/// 太短会让一条长流式请求跑到一半时 token 过期，太长则每次都在刷。
pub const REFRESH_LEEWAY_SECS: u64 = 300;

// ---------- 上游 ----------

/// Codex 的上游 API 基址。转发时把来访路径拼在它后面（见 [`crate::proxy::upstream_url`]）。
///
/// 这是**订阅（ChatGPT 账号）模式**专用的那条路径，与 API-key 模式的
/// `https://api.openai.com/v1` 不是一回事：后者按 token 计费、收 `sk-` 开头的 key，
/// 而 OAuth access_token 打过去会被拒。coban 只做订阅模式。
pub const UPSTREAM_BASE: &str = "https://chatgpt.com/backend-api/codex";

/// 模型清单端点（`UPSTREAM_BASE` 之后那一段）。
///
/// **必须带 `client_version` 查询参数**，缺了上游回 400（`Field required`）。返回
/// `{"models":[{slug, display_name, visibility, supported_in_api, priority, …}]}`，
/// 也就是 codex CLI 自己缓存到 `~/.codex/models_cache.json` 的那份。
///
/// 取值随上游上新变化，所以 coban 不写死模型清单——写死的那一刻它就开始过期
/// （见 [`crate::proxy::list_models`]）。
pub const MODELS_PATH: &str = "models";

/// 会话/推理端点（`UPSTREAM_BASE` 之后那一段）。转发的主路径，探测也打这里。
pub const RESPONSES_PATH: &str = "responses";

/// 官方客户端的 `originator` 头取值。上游按它区分「哪个 codex 前端发来的」。
///
/// 写死成 CLI 那个值而不是透传来访客户端的：coban 的接入方就是 codex CLI，透传等于把
/// 一个可被客户端任意改写的值直接送给上游，形态反而不稳定。
pub const ORIGINATOR: &str = "codex_cli_rs";

/// 官方客户端版本号，随 `User-Agent` 与 `x-codex-*` 一起报给上游。
///
/// 落后不致命——真实用户升级也有先后——但落得太多就成了「一个几个月没升级过的客户端在
/// 不停发请求」。取最近一次核对过的发行版（`@openai/codex` 0.148.0，与本机
/// `~/.codex/models_cache.json` 里的 `client_version` 一致）。
///
/// **它还决定上游给出的模型清单**：[`MODELS_PATH`] 要求带 `client_version` 查询参数，
/// 上游按它裁剪返回哪些模型——实测报 `0.98.0` 只回 3 个（`gpt-5.4`/`gpt-5.4-mini`/
/// `codex-auto-review`），报 `0.148.0` 才回全部 9 个。所以这个常量落后的表现不只是指纹旧，
/// 而是**连通性测试的模型下拉里少一大半模型**。
pub const CODEX_VERSION: &str = "0.148.0";

/// coban 自身发起的账号级请求（token 刷新）默认带的 `User-Agent`。
///
/// 形态照官方客户端：`codex_cli_rs/<版本> (<OS>; <arch>) <终端>`。转发 `/v1/*` 时以
/// 来访客户端自己的 UA 为准（转发头覆盖此默认值），故这里只影响刷新那类请求——
/// 一个持有订阅 refresh_token 却不带 UA 的客户端非常显眼。
///
/// **版本号取自 [`CODEX_VERSION`] 而不是再写一遍**：两处各写一份的话，升级时必然漏掉
/// 一处，于是出现「UA 说 0.98、别处说 0.99」这种真实客户端不会有的自相矛盾。
pub static CODEX_USER_AGENT: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!("codex_cli_rs/{CODEX_VERSION} (Mac OS 26.0.0; arm64) unknown")
});

/// `Accept-Encoding`：与官方客户端逐字节一致。
///
/// **声明了就一定会被压。** 上游（Cloudflare）连几百字节的错误体都压，
/// `text/event-stream` 也不例外。这个常量与 Cargo.toml 里 wreq 的
/// gzip/brotli/zstd/deflate 四个 feature **是一套的，动其一必须动其二**——否则响应体
/// 是读不懂的压缩字节，用量统计、计价、账号级错误判定整片失效。
pub const ACCEPT_ENCODING: &str = "gzip, br";

// ---------- 上游限流头 ----------

/// 主额度窗口（通常是 5 小时）的已用百分比。
pub const RL_PRIMARY_USED_PCT: &str = "x-codex-primary-used-percent";
/// 主额度窗口的长度（分钟）。
pub const RL_PRIMARY_WINDOW_MINUTES: &str = "x-codex-primary-window-minutes";
/// 主额度窗口的重置时刻。
pub const RL_PRIMARY_RESET_AT: &str = "x-codex-primary-reset-at";
/// 次额度窗口（通常是 7 天/月）的已用百分比。
pub const RL_SECONDARY_USED_PCT: &str = "x-codex-secondary-used-percent";
/// 次额度窗口的长度（分钟）。
pub const RL_SECONDARY_WINDOW_MINUTES: &str = "x-codex-secondary-window-minutes";
/// 次额度窗口的重置时刻。
pub const RL_SECONDARY_RESET_AT: &str = "x-codex-secondary-reset-at";
/// 该账号是否还有可用的额外 credits。
pub const RL_CREDITS_HAS_CREDITS: &str = "x-codex-credits-has-credits";
/// 该账号的 credits 是否无限。
pub const RL_CREDITS_UNLIMITED: &str = "x-codex-credits-unlimited";
/// 该账号剩余 credits 数额。
pub const RL_CREDITS_BALANCE: &str = "x-codex-credits-balance";

/// 转发时**不**从来访请求复制给上游的头。
///
/// 分三类，缺一不可：
/// - **鉴权类**（`authorization`/`cookie`/`chatgpt-account-id`）：这几个是 coban 要换掉的
///   东西本身。透传等于把来访客户端的接入 key 直接送给上游，而选中凭证的 token 反而被顶掉。
/// - **逐跳类**（`host`/`connection`/`content-length`/`transfer-encoding` 等）：描述的是
///   「来访这一跳」的连接，与发往上游那一跳无关。`content-length` 尤其致命——改写过 body
///   之后它就是个错的长度，上游按它截断请求体，报错是一句指不到原因的 400。
/// - **中间层留痕类**（`x-forwarded-*`/`via`/`forwarded`）：把「这条请求经过了代理」
///   直接写在头里，与整个转发形态的目标相反。
pub const HOP_BY_HOP_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "chatgpt-account-id",
    "host",
    "connection",
    "proxy-connection",
    "keep-alive",
    "content-length",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "accept-encoding",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-real-ip",
    "forwarded",
    "via",
];
