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

/// 官方客户端 `User-Agent` 的固定前缀（[`ORIGINATOR`] 加一个斜杠）。
///
/// 判「这条来访 UA 像不像官方客户端」用它，见 `proxy::UaMode`。要带上斜杠：少了它，
/// `codex_cli_rs_wrapper/1.0` 这种也会被当成官方客户端放过去。
pub const UA_PREFIX: &str = "codex_cli_rs/";

/// 逐号派生 UA 用的「机器画像」表：`(OS 名与版本, arch, 终端)`。
///
/// **为什么不是一个常量**：一个号一台机器才是真实形态。二十个号报一模一样的
/// `Mac OS 26.0.0; arm64) unknown`，这本身就是一簇可关联的指纹——上游不必看内容，
/// 光按 UA 分组就能把这些号归到一起。按 `account_id` 派生（见
/// [`crate::credentials::Credential::user_agent`]）之后，每个号看着是一台稳定的机器、
/// 跨重启不变，与「逐账号代理」是同一个思路的两半。
///
/// **只在真实机器之间确实会不同的字段上取值**：OS 名与版本、arch、终端
/// （官方客户端那一段取自 `TERM_PROGRAM`，取不到就是 `unknown`）。版本号一律用
/// [`CODEX_VERSION`]、**不打散**：打散等于让一部分号看着像几个月没升级过的客户端，
/// 正是那个常量的注在防的事；而「全都升到了最新版」本身是合理形态。
///
/// 表可以往后加，但**不能删、不能改顺序**：索引是按 `account_id` 算出来的，
/// 动了表就等于给一批号换了机器——一个号的 UA 突然从 Mac 变成 Ubuntu，比它一直报同一份
/// 更显眼。
pub const UA_PROFILES: &[(&str, &str, &str)] = &[
    ("Mac OS 26.0.0", "arm64", "unknown"),
    ("Mac OS 26.0.0", "arm64", "iTerm.app"),
    ("Mac OS 26.0.1", "arm64", "unknown"),
    ("Mac OS 26.0.1", "arm64", "Apple_Terminal"),
    ("Mac OS 26.0.1", "arm64", "vscode"),
    ("Mac OS 15.6.1", "arm64", "unknown"),
    ("Mac OS 15.6.1", "arm64", "iTerm.app"),
    ("Mac OS 15.6.1", "x86_64", "unknown"),
    ("Ubuntu 24.04", "x86_64", "unknown"),
    ("Ubuntu 24.04", "x86_64", "vscode"),
    ("Ubuntu 22.04", "x86_64", "unknown"),
    ("Debian 12", "x86_64", "unknown"),
];

/// 按一份画像拼出 UA 串：`codex_cli_rs/<版本> (<OS>; <arch>) <终端>`。
///
/// **版本号取自 [`CODEX_VERSION`] 而不是再写一遍**：两处各写一份的话，升级时必然漏掉
/// 一处，于是出现「UA 说 0.98、别处说 0.99」这种真实客户端不会有的自相矛盾。
pub fn user_agent((os, arch, term): (&str, &str, &str)) -> String {
    format!("{UA_PREFIX}{CODEX_VERSION} ({os}; {arch}) {term}")
}

/// **没有凭证语境**时用的 `User-Agent`：出站客户端的默认头、授权码换 token。
///
/// 凡是能追溯到某个凭证的请求都不该用它，而要用那个号派生的那份（见 [`UA_PROFILES`]）
/// ——转发、token 刷新、连通性测试、额度券那族接口都已经改成派生的。这里剩下的只有
/// 「还没有凭证」的那一小段：授权码换 token 时账号身份还没落库。
///
/// 取值是 [`UA_PROFILES`] 的第一项，不另写一份：另写一份就等于多一种 coban 才有的形态。
pub static CODEX_USER_AGENT: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| user_agent(UA_PROFILES[0]));

/// 改写 UA 时要一并清掉的来访侧客户端留痕头（精确匹配那族）。
///
/// 只在**真的改写了 UA** 的那条路上清（见 `proxy::build_forward_headers`）：UA 说这是
/// codex CLI、而 `x-stainless-lang: python` 说这是 OpenAI 的 Python SDK——这种自相矛盾比
/// 两者都老实报 python 更显眼。透传 UA 那一档不动它们，那时报的本来就是同一个客户端。
///
/// `openai-organization` / `openai-project` 是 API-key 模式的概念：拿着订阅 token 打
/// `backend-api/codex` 时上游不看它们，而官方客户端从不发。
pub const UA_REWRITE_STRIPPED_HEADERS: &[&str] = &["openai-organization", "openai-project"];

/// 同上，按前缀匹配的那族。
///
/// OpenAI 各语言 SDK（stainless 生成的那批）都会带一串 `x-stainless-*`
/// （`-lang`/`-os`/`-arch`/`-runtime`/`-runtime-version`/`-package-version`/`-retry-count`），
/// 逐个列等于跟着上游 SDK 的版本追，按前缀一次清干净。
pub const UA_REWRITE_STRIPPED_PREFIXES: &[&str] = &["x-stainless-"];

/// `Accept-Encoding`：与官方客户端逐字节一致。
///
/// **声明了就一定会被压。** 上游（Cloudflare）连几百字节的错误体都压，
/// `text/event-stream` 也不例外。这个常量与 Cargo.toml 里 wreq 的
/// gzip/brotli/zstd/deflate 四个 feature **是一套的，动其一必须动其二**——否则响应体
/// 是读不懂的压缩字节，用量统计、计价、账号级错误判定整片失效。
pub const ACCEPT_ENCODING: &str = "gzip, br";

// ---------- 上游限流头 ----------

/// 「主」「次」是**这两组头的名字，不是窗口的名字**——别按名字猜窗口长度。
///
/// 实测（2026-08，一批 ChatGPT Pro 号，六次真实请求覆盖三个号）：`primary` 报的是**周**窗口
/// （`primary-window-minutes: 10080`），而 `secondary` 整组是空的（长度 0、重置时刻空串）。
/// 上游同时还发了另一族带代号的头（`x-codex-bengalfox-*`，自带
/// `bengalfox-limit-name: GPT-5.3-Codex-Spark`，其 primary 是 300 分钟即 5 小时），以及一个
/// `x-codex-active-limit: premium`——看着像是「当前生效的那一族落在通用的 primary/secondary
/// 槽里，别的族各挂自己的代号前缀」。那一族 coban 不读：代号随时会变，而它自带的 limit-name
/// 说明那是某个模型的限额，不是账号的。
///
/// 所以窗口长度**只能从 `*_WINDOW_MINUTES` 读**，界面上的列名也是这么算出来的
/// （见 admin-ui 的 `quotaWindowTitle`）。
pub const RL_PRIMARY_USED_PCT: &str = "x-codex-primary-used-percent";
/// 主额度窗口的长度（分钟）。**唯一**能说出那是哪个窗口的东西，见上面那条注。
pub const RL_PRIMARY_WINDOW_MINUTES: &str = "x-codex-primary-window-minutes";
/// 主额度窗口的重置时刻。
pub const RL_PRIMARY_RESET_AT: &str = "x-codex-primary-reset-at";
/// 次额度窗口的已用百分比。**上游可能整组不发**（见上面那条注）。
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
/// - **鉴权类**（`authorization`/`api-key`/`x-api-key`/`cookie`/`chatgpt-account-id`）：这几个
///   是 coban 要换掉的东西本身。透传等于把来访客户端的接入 key 直接送给上游，而选中凭证的
///   token 反而被顶掉。`api-key`/`x-api-key` 一定要在这份清单里：[`crate::proxy`] 的
///   `client_authorized` 认这两种写法，用它们接入的客户端，那个 key 就是 coban 的接入 key
///   ——不掐掉就是把它原样发到 chatgpt.com，而上游压根不看这个头。
/// - **逐跳类**（`host`/`connection`/`content-length`/`transfer-encoding` 等）：描述的是
///   「来访这一跳」的连接，与发往上游那一跳无关。`content-length` 尤其致命——改写过 body
///   之后它就是个错的长度，上游按它截断请求体，报错是一句指不到原因的 400。
/// - **中间层留痕类**（`x-forwarded-*`/`via`/`forwarded`）：把「这条请求经过了代理」
///   直接写在头里，与整个转发形态的目标相反。
pub const HOP_BY_HOP_HEADERS: &[&str] = &[
    "authorization",
    "api-key",
    "x-api-key",
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

// ---------- 额度重置券 ----------

/// 额度重置券那族端点的基址。
///
/// **不在 [`UPSTREAM_BASE`] 之下**：转发走的是 `backend-api/codex`，而这三条接口挂在
/// `backend-api/wham`（Codex 桌面端「重置用量」那个按钮打的就是它们）。拼到 codex 那条
/// 路径后面只会得到 404。
///
/// **来源是 sub2api 的生产实现**（`backend/internal/service/openai_quota_service.go` 的
/// `QueryUsage` / `ResetCredit`），不是官方 CLI 的抓包——codex CLI 压根没有「重置额度」
/// 这个命令，这一族只有桌面端会打。理由同 [`crate::oauth::PkceChallenge::authorize_url`]
/// 那两个非标准参数：一个跑在生产上的第三方实现，比「我在二进制里搜不到」更强的证据。
pub const WHAM_BASE: &str = "https://chatgpt.com/backend-api/wham";

/// 用量与重置券张数（`WHAM_BASE` 之后那一段）。
///
/// 回的 JSON 里 `rate_limit_reset_credits.available_count` 就是「还能重置几次」。它与
/// [`WHAM_RESET_CREDITS_PATH`] 的差别是：这里只有一个总数，那里还带每张券的过期时刻。
pub const WHAM_USAGE_PATH: &str = "usage";

/// 重置券清单（`WHAM_BASE` 之后那一段）。回的是张数 + 每张的 `expires_at`。
pub const WHAM_RESET_CREDITS_PATH: &str = "rate-limit-reset-credits";

/// 兑换一张重置券（`WHAM_BASE` 之后那一段）。POST，体里要带 `redeem_request_id`。
///
/// 那个 id 是**幂等键**：同一个 id 重发不会扣第二张券。所以它必须每次现生成一个新的
/// （见 `crate::quota_reset::consume`），写死一个常量等于第二次点「重置」什么也不会发生。
pub const WHAM_RESET_CONSUME_PATH: &str = "rate-limit-reset-credits/consume";

/// wham 那族接口的 `originator`。
///
/// **与转发用的 [`ORIGINATOR`] 不同**：这一族是桌面端的接口，报 `codex_cli_rs` 等于说
/// 「CLI 在调一个 CLI 没有的功能」。同一个账号既跑 CLI 又开着桌面端是常态，两族各报
/// 各自的形态才对得上。
pub const WHAM_ORIGINATOR: &str = "Codex Desktop";

/// wham 那族接口的 `openai-beta`。缺了实测直接 4xx。
pub const WHAM_OPENAI_BETA: &str = "codex-1";
