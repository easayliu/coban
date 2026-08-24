//! 网页服务：授权登录 + 多凭证管理的 JSON 接口，其余路径由内嵌前端 SPA 兜底。

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::StatusCode,
    middleware,
    response::Response,
    routing::{any, delete, get, post},
};
use serde::{Deserialize, Serialize};

use crate::admin_ui;
use crate::auth;
use crate::credentials::Credential;
use crate::oauth::{self, PkceChallenge};
use crate::proxy;
use crate::quota_reset;
use crate::store::{self, CredentialStore};

/// 一次登录尝试还没换 token 之前，PKCE 上下文最多留多久。
///
/// 用户要在浏览器里完成授权再把回调 URL 粘回来，几分钟足够；留太久只是让过期的挑战
/// 一直占着位置。
const PKCE_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// 同时最多保留几个待完成的登录尝试。纯属防御——正常同时开几个标签页也就个位数，
/// 上限只是不让反复点「添加账号」把内存撑起来。超出时丢掉最旧的那个。
const PKCE_MAX_PENDING: usize = 32;

/// 转发接口的请求体上限。
///
/// axum 对 `Bytes` 提取器默认限 2MB，超过的请求进不了 handler 就被 413 拦掉——而带大段
/// 上下文/附件的合法 Codex 请求很容易超 2MB。放到 64MB 留出余量，真正的大小判决交给上游。
const PROXY_BODY_LIMIT: usize = 64 * 1024 * 1024;

/// 服务共享状态。
#[derive(Clone)]
pub struct AppState {
    /// 出站客户端池：不配代理的号共用直连那一份，配了代理的各有一份。
    pub clients: Arc<crate::clients::ClientPool>,
    /// 进行中的登录尝试：`state` → (PKCE 上下文, 创建时刻)。
    ///
    /// **按 state 索引而不是只留一份**：两个标签页同时点「添加账号」时，后一次
    /// `authorize` 会把前一次的 verifier/state 直接覆盖掉，前一个人粘贴回来就撞上
    /// 「state 不匹配」——一句会把人引去查 CSRF 的误导性报错，实际上只是两次登录互相踩了。
    pkce: Arc<parking_lot::Mutex<Vec<(String, PkceChallenge, std::time::Instant)>>>,
    /// 凭证存储。
    pub store: Arc<CredentialStore>,
    /// 接入用的 API Key（None 表示未由命令行/环境设置，改由库里的设置项决定）。
    pub client_key: Option<Arc<String>>,
    /// 管理密码（环境接管，明文；None 表示未由环境设置）。
    pub admin_env: Option<Arc<String>>,
    /// **在途请求数**：已进入转发入口、响应尚未走完的那些。
    ///
    /// 由 [`crate::proxy::InFlightGuard`] 增减，随响应流一起存活——流式回复要几十秒才走完，
    /// 只在 handler 返回时减一会把这类请求算成「瞬间就结束了」。
    pub in_flight: Arc<std::sync::atomic::AtomicI64>,
    /// `/v1/models` 的清单缓存。取一次要跑一趟上游，而有一类客户端每开个会话就问一遍
    /// （见 [`crate::proxy::ModelListCache`]）。
    pub models_cache: proxy::ModelListCache,
    /// 「这个会话捎来的加密推理在这个号上解不开」的记忆，见 [`crate::proxy::StaleReasoningMemo`]。
    pub stale_reasoning: proxy::StaleReasoningMemo,
    /// 「这个会话捎来的 `input` 项不合上游要求」的记忆，见 [`crate::proxy::InputRuleMemo`]。
    pub input_rules: proxy::InputRuleMemo,
    /// 「上游不收这个 schema 关键字」的记忆，见 [`crate::proxy::SchemaKeywordMemo`]。
    pub schema_keywords: proxy::SchemaKeywordMemo,
}

type ApiError = (StatusCode, String);

/// 启动网页服务 + 转发代理，绑定 `host:port`，可选自动打开浏览器。
pub async fn run(
    host: &str,
    port: u16,
    open_browser: bool,
    store: Arc<CredentialStore>,
    api_key: Option<String>,
    admin_password: Option<String>,
) -> Result<()> {
    let client_key = api_key.map(Arc::new);
    let state = AppState {
        clients: Arc::new(crate::clients::ClientPool::new()?),
        pkce: Arc::new(parking_lot::Mutex::new(Vec::new())),
        store,
        client_key: client_key.clone(),
        admin_env: admin_password.map(Arc::new),
        in_flight: Arc::default(),
        models_cache: Arc::default(),
        stale_reasoning: Arc::default(),
        input_rules: Arc::default(),
        schema_keywords: Arc::default(),
    };

    spawn_usage_pruner(state.store.clone());

    // 公开鉴权接口（无需登录）。
    let public = Router::new()
        .route("/auth/state", get(auth::state))
        .route("/auth/login", post(auth::login))
        .route("/auth/setup", post(auth::setup));

    // 需管理鉴权的接口（未设密码时中间件放行）。
    let protected = Router::new()
        .route("/authorize", get(authorize))
        .route("/exchange", post(exchange))
        .route("/import-auth-json", post(import_auth_json))
        .route("/credentials", get(list_credentials))
        .route("/credentials/priority", post(set_priorities))
        .route("/credentials/rpm-limit", post(set_rpm_limits))
        .route("/credentials/disabled", post(set_disabled_many))
        .route("/credentials/delete", post(delete_credentials))
        .route("/credentials/{id}", delete(delete_credential))
        .route("/credentials/{id}/disabled", post(set_disabled))
        .route("/credentials/{id}/priority", post(set_priority))
        .route("/credentials/{id}/label", post(set_label))
        .route("/credentials/{id}/rpm-limit", post(set_rpm_limit))
        .route("/credentials/{id}/proxy", post(set_proxy))
        .route("/credentials/{id}/refresh", post(refresh_credential))
        .route("/credentials/{id}/test", post(test_credential))
        .route("/credentials/{id}/models", get(list_credential_models))
        .route("/credentials/{id}/cooldown", delete(clear_cooldown))
        .route("/credentials/{id}/reset-credits", get(get_reset_credits))
        .route("/credentials/{id}/reset-credits/consume", post(consume_reset_credit))
        .route("/credentials/{id}/usage", get(list_credential_usage))
        .route("/usage", get(list_usage))
        .route("/metrics", get(get_metrics))
        .route("/metrics/cache-series", get(get_cache_series))
        .route("/metrics/cache-reasons", get(get_cache_reasons))
        .route("/settings", get(get_settings))
        .route("/settings/api-key", post(set_api_key))
        .route("/settings/default-rpm-limit", post(set_default_rpm_limit))
        .route("/settings/rate-limit-retry-max", post(set_rate_limit_retry_max))
        .route("/settings/rate-limit-rotate", post(set_rate_limit_rotate))
        .route("/settings/rate-limit-wait-secs", post(set_rate_limit_wait_secs))
        .route("/settings/rate-limit-wait-retry-max", post(set_rate_limit_wait_retry_max))
        .route("/settings/quota-pause-pct", post(set_quota_pause_pct))
        .route("/settings/cooldown-secs", post(set_cooldown_secs))
        .route("/settings/session-lease-secs", post(set_session_lease_secs))
        .route("/settings/normalize-tool-order", post(set_normalize_tool_order))
        .route("/settings/upstream-ua-mode", post(set_upstream_ua_mode))
        .route("/auth/password", post(auth::change_password))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_admin));

    // 失败的请求补一行「哪个方法打了哪条路径、回了几」。错误详情由 `internal`/`bad_request`
    // 各自记，方法与路径它们看不到，只能在这一层补——两行合起来才定位得到一次失败。
    let api = public.merge(protected).layer(middleware::from_fn(log_api_failures));

    let app = Router::new()
        .nest("/api", api)
        // `/v1/*`：给 codex CLI 的 `model_providers.*.base_url` 用。
        .route("/v1/{*path}", any(proxy::handle).layer(DefaultBodyLimit::max(PROXY_BODY_LIMIT)))
        // `/backend-api/codex/*`：把 coban 直接当 `chatgpt.com` 替身时的原路径。
        // 两条都收是因为接入方有两种配法，只支持一条的话另一条是个 404，
        // 而 codex 那头只显示「请求失败」，指不到路径上。
        .route(
            "/backend-api/codex/{*path}",
            any(proxy::handle).layer(DefaultBodyLimit::max(PROXY_BODY_LIMIT)),
        )
        // `/chat/completions`：OpenAI 兼容客户端里有一类把 base_url 配到根上（不带 `/v1`）。
        // 带 `/v1` 的那条由上面的通配路由收，两条落到同一段逻辑。
        .route(
            "/chat/completions",
            any(proxy::handle_chat).layer(DefaultBodyLimit::max(PROXY_BODY_LIMIT)),
        )
        // `/models`：同上，那类客户端取模型清单时打的是这条路径。
        .route("/models", any(proxy::handle_models))
        // 个别移动端/前置层会以 POST 打开首页；用 PRG 把最终文档历史落成 GET。
        .route("/", get(admin_ui::fallback).post(admin_ui::redirect_root_post))
        .fallback_service(get(admin_ui::fallback))
        .with_state(state);

    let bind = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("failed to bind {bind} (the port may be in use)"))?;

    let shown = if host == "0.0.0.0" || host == "::" { "127.0.0.1" } else { host };
    let url = format!("http://{shown}:{port}/");
    let base = url.trim_end_matches('/');

    tracing::info!(addr = %bind, url = %url, "coban started");
    tracing::info!("Codex setup: put this in ~/.codex/config.toml");
    tracing::info!("  [model_providers.coban]");
    tracing::info!("  name = \"coban\"");
    tracing::info!("  base_url = \"{base}/v1\"");
    tracing::info!("  wire_api = \"responses\"");
    match &client_key {
        Some(_) => tracing::info!("  env_key = \"COBAN_API_KEY\"   (value = your --api-key)"),
        None => tracing::info!(
            "  (no --api-key set, the proxy does not authenticate callers -- keep it local-only)"
        ),
    }
    if open_browser {
        open_in_browser(&url);
    }

    // `into_make_service_with_connect_info` 而不是直接交 `app`：登录失败要记来源，
    // 而对端地址只有这里能拿到（见 [`auth::client_ip`]）。
    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("the web server exited unexpectedly")?;
    Ok(())
}

/// 每天裁剪一次用量流水。
///
/// `interval` 的首个 tick 立即触发，兼作启动清理；删除走 `spawn_blocking`，避免拿着
/// SQLite 锁占住异步线程。
fn spawn_usage_pruner(store: Arc<CredentialStore>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(24 * 3600));
        loop {
            tick.tick().await;
            let store = store.clone();
            match tokio::task::spawn_blocking(move || store.prune_usage_logs()).await {
                Ok(Ok(n)) if n > 0 => tracing::info!(rows = n, "pruned expired usage logs"),
                Ok(Err(e)) => tracing::warn!(error = %e, "failed to prune usage logs"),
                _ => {}
            }
        }
    });
}

/// 等待关闭信号：Ctrl-C 或（Unix 下）SIGTERM，收到后让 axum 排空在途请求再退出。
///
/// 容器内 coban 常以 PID 1 运行，内核对 PID 1 不套用信号默认动作——若不显式处理
/// SIGTERM，`docker stop`/`restart` 会空等 10 秒宽限期才 SIGKILL 强杀，表现为
/// 「重启很久」。注册处理器即可让重启秒停，且不切断流式响应。
async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c().await.expect("failed to install the Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install the SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received, shutting down gracefully ...");
}

/// 给失败的管理接口补一行方法 + 路径 + 状态码。
async fn log_api_failures(req: axum::extract::Request, next: middleware::Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let resp = next.run(req).await;
    if resp.status().is_client_error() || resp.status().is_server_error() {
        tracing::warn!(%method, %path, status = resp.status().as_u16(), "api request failed");
    }
    resp
}

// ---------- 授权 ----------

#[derive(Serialize)]
struct AuthorizeResp {
    /// 用户要在浏览器里打开的授权 URL。
    url: String,
    /// 回传给 `/exchange` 用的关联 id（就是 PKCE 的 state）。
    state: String,
}

/// 生成一次登录尝试的授权 URL。
async fn authorize(State(state): State<AppState>) -> Json<AuthorizeResp> {
    let pkce = PkceChallenge::generate();
    let url = pkce.authorize_url();
    let key = pkce.state.clone();
    remember_pkce(&state, pkce);
    Json(AuthorizeResp { url, state: key })
}

fn remember_pkce(state: &AppState, pkce: PkceChallenge) {
    let now = std::time::Instant::now();
    let mut pending = state.pkce.lock();
    pending.retain(|(_, _, t)| now.duration_since(*t) < PKCE_TTL);
    while pending.len() >= PKCE_MAX_PENDING {
        pending.remove(0);
    }
    pending.push((pkce.state.clone(), pkce, now));
}

fn take_pkce(state: &AppState, key: &str) -> Option<PkceChallenge> {
    let now = std::time::Instant::now();
    let mut pending = state.pkce.lock();
    pending.retain(|(_, _, t)| now.duration_since(*t) < PKCE_TTL);
    let idx = pending.iter().position(|(s, _, _)| s == key)?;
    Some(pending.remove(idx).1)
}

#[derive(Deserialize)]
struct ExchangeReq {
    /// 浏览器最后跳到的那条 `localhost:1455/auth/callback?...` URL（或只有 code 的一段）。
    callback: String,
    /// `/authorize` 返回的关联 id。**允许缺省**：用户可能刷新过页面、或从别处粘来一条
    /// 回调，此时按回调里的 state 反查。
    #[serde(default)]
    state: Option<String>,
}

/// 用授权码换 token 并入库。
async fn exchange(
    State(state): State<AppState>,
    Json(req): Json<ExchangeReq>,
) -> Result<Json<CredentialView>, ApiError> {
    let (code, cb_state) = oauth::parse_callback(&req.callback);
    if code.trim().is_empty() {
        return Err(bad_request("no authorization code found in what you pasted"));
    }
    // 关联 id 优先用请求带的，其次用回调里的——两者都没有时无从校验，直接判失败而不是
    // 「随便取一个待完成的挑战」：那样两个标签页并发登录会互相拿错 verifier。
    let key = req.state.as_deref().filter(|s| !s.is_empty()).or(cb_state.as_deref()).ok_or_else(
        || bad_request("the pasted URL has no state parameter; start the login again"),
    )?;
    let pkce = take_pkce(&state, key).ok_or_else(|| {
        bad_request("this login attempt expired or was already used; click Add account again")
    })?;
    // 回调里带了 state 就必须与挑战一致。不一致意味着粘错了（或真有 CSRF），
    // 继续下去只会换到一个属于别人那次登录的 token。
    if let Some(s) = &cb_state {
        if s != &pkce.state {
            return Err(bad_request(
                "state mismatch: the pasted URL belongs to a different login attempt",
            ));
        }
    }

    let set = oauth::exchange_code(state.clients.direct(), code.trim(), &pkce.verifier)
        .await
        .map_err(|e| bad_request(format!("{e:#}")))?;
    save_token_set(&state, set).map(Json)
}

#[derive(Deserialize)]
struct ImportAuthJsonReq {
    /// `~/.codex/auth.json` 的内容，或一份带 `accounts` 数组的批量导出。
    content: String,
}

/// 一次导入的结果。
///
/// **单个账号也走这个形状**：让「导入一个」成为「导入 N 个」的退化情形，前端只需一条
/// 渲染路径。否则同一个接口按输入形态回两种结构，客户端得先判类型再取字段。
#[derive(Serialize)]
struct ImportReport {
    imported: Vec<CredentialView>,
    skipped: Vec<SkippedAccount>,
}

/// 批量导入里被跳过的一个账号。带上名字才定位得到是哪一个出的问题。
#[derive(Serialize)]
struct SkippedAccount {
    name: String,
    reason: String,
}

/// 导入已登录的账号：`~/.codex/auth.json`，或一份带 `accounts` 数组的批量导出。
///
/// 存在的理由：机器上已经 `codex login` 过的账号不必再走一遍浏览器授权，而在无图形界面
/// 的服务器上「打开浏览器粘回调」这条路本身就很难走。
///
/// 认三种形态，都在 [`import_one`] 里归一：
/// - `~/.codex/auth.json` —— token 在 `tokens` 子对象里；
/// - 裸的 token 对象 —— 根上直接是 `access_token`/`refresh_token`；
/// - 批量导出（sub2api 等）—— 根上一个 `accounts` 数组，每项的 token 在 `credentials` 里。
///
/// 批量时**逐个独立处理，坏的跳过而不是整批失败**：23 个账号里有 1 个 token 已作废，
/// 让另外 22 个也进不来是没道理的。跳过的连同原因一起回给调用方。
async fn import_auth_json(
    State(state): State<AppState>,
    Json(req): Json<ImportAuthJsonReq>,
) -> Result<Json<ImportReport>, ApiError> {
    let v: serde_json::Value = serde_json::from_str(req.content.trim())
        .map_err(|e| bad_request(format!("that is not valid JSON: {e}")))?;

    let accounts: Vec<&serde_json::Value> = match v.get("accounts").and_then(|a| a.as_array()) {
        Some(list) => list.iter().collect(),
        None => vec![&v],
    };
    if accounts.is_empty() {
        return Err(bad_request("the accounts array in that file is empty"));
    }

    let mut imported = Vec::new();
    let mut skipped = Vec::new();
    for (i, acc) in accounts.iter().enumerate() {
        match import_one(&state, acc) {
            Ok(view) => imported.push(view),
            Err((_, reason)) => skipped.push(SkippedAccount { name: name_of(acc, i), reason }),
        }
    }

    // 一个都没成时回 400 而不是「200 加一份全是 skipped 的报告」：整份文件不可用是
    // 请求级失败，前端的错误分支才是该走的那条。原因回第一条——它们通常同因同源。
    if imported.is_empty() {
        let first = skipped.into_iter().next().map(|s| s.reason).unwrap_or_default();
        return Err(bad_request(first));
    }
    Ok(Json(ImportReport { imported, skipped }))
}

/// 从一个账号对象里取 token 存库。
///
/// `credentials`（批量导出）/ `tokens`（auth.json）/ 根对象，按这个顺序找 token 所在的层。
fn import_one(state: &AppState, acc: &serde_json::Value) -> Result<CredentialView, ApiError> {
    let tokens = acc.get("credentials").or_else(|| acc.get("tokens")).unwrap_or(acc);
    let get = |k: &str| tokens.get(k).and_then(|x| x.as_str()).map(str::to_owned);

    let access_token =
        get("access_token").ok_or_else(|| bad_request("no tokens.access_token in that file"))?;
    let refresh_token = get("refresh_token").ok_or_else(|| {
        bad_request("no tokens.refresh_token in that file (was it an API-key login?)")
    })?;
    let id_token = get("id_token");
    let claims = id_token.as_deref().map(oauth::Claims::parse).unwrap_or_default();
    // account_id 优先取文件自己那份（auth.json 叫 `account_id`，批量导出叫
    // `chatgpt_account_id`），都缺了再从 id_token 的 claim 里找。
    let account_id = get("account_id")
        .or_else(|| get("chatgpt_account_id"))
        .or_else(|| claims.account_id.clone());

    let expires_at = import_expires_at(&access_token, tokens);

    let mut view = save_token_set(
        state,
        oauth::TokenSet {
            access_token,
            refresh_token,
            expires_at,
            id_token,
            claims: oauth::Claims { account_id, ..claims },
        },
    )?;

    // 导出里带了优先级就沿用。**失败不算导入失败**：凭证本身已经存好了，为一个可以
    // 在界面上改的字段把整条判为跳过，反而要人再导一遍。
    if let Some(p) = acc.get("priority").and_then(|x| x.as_i64()) {
        match state.store.set_priority(view.id, p) {
            // 视图是 `save_token_set` 里就生成的，那时还没落这个值——不同步回去的话，
            // 报告里回的是 0 而库里是 1，看报告推不出库里的状态。
            Ok(()) => view.priority = p,
            Err(e) => {
                tracing::warn!(cred_id = view.id, error = %e, "imported but could not apply priority")
            }
        }
    }
    Ok(view)
}

/// 定下一条导入进来的凭证的过期时刻。
///
/// **要从 access_token 自己身上读，不能一律记 0**。记 0 等于「导进来立刻强刷一次」，而
/// 「refresh_token 还能不能用」与「access_token 还剩多久」是两件独立的事：前者作废、后者
/// 还剩好几天，是导入场景里最常见的一种状态——导出那一刻两边持有的是同一个 refresh_token，
/// 谁先刷一次，另一边连同整条授权链当场作废（上游回 `refresh_token_invalidated`），而
/// access_token 完全不受影响，照样能转发到它自己的 `exp` 为止。一律记 0 的话，这种号在
/// 导入的第一秒就被我们自己用一次必然失败的刷新废掉，明明还能用好几天。
///
/// 取值顺序：
/// 1. access_token 的 `exp` —— token 自证，且 [`oauth::access_token_expires_at`] 会校验
///    签发方，伪造不进来；
/// 2. 文件里的 `expires_at` —— sub2api 这类批量导出带，实测与 1 逐个吻合（14 个号里 13 个
///    完全相同、1 个差 1 秒的取整），留着是为了将来 access_token 不再是 JWT 时还有依据；
/// 3. 都没有 → 0，保持原来的行为：立刻刷一次，让「这份文件已经不能用了」当场暴露，而不是
///    等到第一条转发才发现。
fn import_expires_at(access_token: &str, tokens: &serde_json::Value) -> u64 {
    oauth::access_token_expires_at(access_token)
        .or_else(|| json_epoch_secs(tokens.get("expires_at")))
        .unwrap_or(0)
}

/// 读一个可能是数字、也可能是数字字符串的 Unix 秒字段。
///
/// 两种形态都收是因为导出方不止一家：JSON 里写 `1788092074` 与 `"1788092074"` 的都见过，
/// 而只认其中一种的代价是「时间戳明明在文件里，却当成缺失退回 0」——那正是这个字段要
/// 避免的结果，且从表现上看不出原因。
fn json_epoch_secs(v: Option<&serde_json::Value>) -> Option<u64> {
    let v = v?;
    v.as_u64().or_else(|| v.as_str()?.trim().parse().ok())
}

/// 给一个账号对象取个能认出来的名字，用于跳过原因的定位。
fn name_of(acc: &serde_json::Value, index: usize) -> String {
    ["name", "email", "label"]
        .iter()
        .find_map(|k| acc.get(*k).and_then(|x| x.as_str()))
        .or_else(|| acc.get("credentials").and_then(|c| c.get("email")).and_then(|x| x.as_str()))
        .map(str::to_owned)
        .unwrap_or_else(|| format!("account #{}", index + 1))
}

/// 把一组 token 落库并返回视图。
fn save_token_set(state: &AppState, set: oauth::TokenSet) -> Result<CredentialView, ApiError> {
    let account_id =
        set.claims.account_id.as_deref().map(str::trim).filter(|s| !s.is_empty()).ok_or_else(
            || {
                bad_request(
                    "this login has no ChatGPT account id, so forwarding would fail with 401. \
                 Make sure you signed in with a ChatGPT subscription account.",
                )
            },
        )?;
    let label = set.claims.email.clone().unwrap_or_else(|| format!("account {account_id}"));
    let (cred, created) = state
        .store
        .upsert(
            &label,
            set.claims.email.as_deref(),
            set.claims.plan_type.as_deref(),
            account_id,
            set.id_token.as_deref(),
            &set.access_token,
            &set.refresh_token,
            set.expires_at,
        )
        .map_err(internal)?;
    tracing::info!(cred_id = cred.id, created, plan = ?cred.plan_type, "credential saved");
    Ok(view_of(state, &cred))
}

// ---------- 凭证管理 ----------

/// 一条凭证的对外视图。**不含任何 token 明文**——这个结构会进浏览器、进日志、进截图。
#[derive(Serialize)]
struct CredentialView {
    id: i64,
    label: String,
    email: Option<String>,
    plan_type: Option<String>,
    /// 账号 id 只回一个掩码后的尾段，够用来区分两个账号，又不至于把完整标识散出去。
    account_id_masked: String,
    priority: i64,
    disabled: bool,
    rpm_limit: i64,
    /// 三态折算后**真正生效**的 RPM 上限（0 = 不限）。
    ///
    /// 后端算好再发：前端要自己算的话就得同时知道那条三态规则与全局默认值，两处各写一份
    /// 迟早对不上，而对不上的表现是界面显示的上限与实际拦截的上限不是一个数。
    rpm_limit_effective: i64,
    /// 最近 60 秒该账号已转发多少条（进程内计数，重启清零）。
    rpm: i64,
    ban_reason: Option<String>,
    resume_at: Option<u64>,
    proxy: Option<String>,
    /// access_token 还有多少秒过期（0 = 已过期，会在下次请求时自动刷新）。
    expires_in_secs: u64,
    /// 最后一次变更（登录、刷新、改配置）的时刻，Unix 秒。
    updated_at: u64,
    /// 还要冷却几秒（0 = 不在冷却中）。
    cooldown_secs: i64,
    created_at: u64,
    stats: store::CredentialStats,
}

fn view_of(state: &AppState, c: &Credential) -> CredentialView {
    let default_rpm = state.store.get_setting_i64(store::DEFAULT_RPM_LIMIT, 0);
    CredentialView {
        id: c.id,
        label: c.label.clone(),
        email: c.email.clone(),
        plan_type: c.plan_type.clone(),
        account_id_masked: mask(&c.account_id),
        priority: c.priority,
        disabled: c.disabled,
        rpm_limit: c.rpm_limit,
        rpm_limit_effective: store::effective_rpm_limit(c.rpm_limit, default_rpm),
        rpm: state.store.current_rpm(c.id),
        ban_reason: c.ban_reason.clone(),
        resume_at: c.resume_at,
        proxy: c.proxy.clone(),
        expires_in_secs: c.expires_in_secs(),
        updated_at: c.updated_at,
        cooldown_secs: state.store.cooldown_secs(c.id),
        created_at: c.created_at,
        stats: state.store.stats_of(c.id).unwrap_or_default(),
    }
}

/// 只留尾部 6 位。短到不值得掩码时整条打码，避免「掩码后等于原文」。
fn mask(s: &str) -> String {
    let n = s.chars().count();
    if n <= 6 {
        return "*".repeat(n);
    }
    format!("…{}", s.chars().skip(n - 6).collect::<String>())
}

async fn list_credentials(
    State(state): State<AppState>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    let list = state.store.list().map_err(internal)?;
    Ok(Json(list.iter().map(|c| view_of(&state, c)).collect()))
}

async fn delete_credential(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !state.store.delete(id).map_err(internal)? {
        return Err(not_found());
    }
    tracing::info!(cred_id = id, "credential deleted");
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// 批量操作的请求体。`ids` 为空一律拒绝——一个空数组多半是前端筛选出错的产物，
/// 而「对 0 个对象执行」与「对全部执行」在界面上看着一模一样，静默成功最误导。
#[derive(Deserialize)]
struct BatchReq<T> {
    ids: Vec<i64>,
    value: T,
}

/// 批量操作一次最多影响多少条。上限存在的意义是让一次误操作的代价可控。
const BATCH_MAX: usize = 500;

fn check_ids(ids: &[i64]) -> Result<(), ApiError> {
    if ids.is_empty() {
        return Err(bad_request("no accounts selected"));
    }
    if ids.len() > BATCH_MAX {
        return Err(bad_request(format!("too many accounts in one request (max {BATCH_MAX})")));
    }
    Ok(())
}

/// 逐条执行并返回**全部**账号的最新视图。
///
/// 回全量而不是只回改动的那几条：批量改优先级会连带改变列表顺序与分页，前端拿到部分
/// 数据没法自洽地合并。列表本来就不大（账号数是个位数到几十）。
fn apply_batch(
    state: &AppState,
    ids: &[i64],
    mut op: impl FnMut(i64) -> anyhow::Result<()>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(ids)?;
    for &id in ids {
        // 一条失败就整体报错：批量操作最怕「部分成功且不说是哪部分」，那种状态没法重试。
        op(id).map_err(internal)?;
    }
    let list = state.store.list().map_err(internal)?;
    Ok(Json(list.iter().map(|c| view_of(state, c)).collect()))
}

async fn set_priorities(
    State(state): State<AppState>,
    Json(req): Json<BatchReq<i64>>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    apply_batch(&state, &req.ids, |id| state.store.set_priority(id, req.value))
}

async fn set_rpm_limits(
    State(state): State<AppState>,
    Json(req): Json<BatchReq<i64>>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    apply_batch(&state, &req.ids, |id| state.store.set_rpm_limit(id, req.value))
}

async fn set_disabled_many(
    State(state): State<AppState>,
    Json(req): Json<BatchReq<bool>>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    apply_batch(&state, &req.ids, |id| state.store.set_disabled(id, req.value))
}

#[derive(Deserialize)]
struct IdsReq {
    ids: Vec<i64>,
}

async fn delete_credentials(
    State(state): State<AppState>,
    Json(req): Json<IdsReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    let n = req.ids.len();
    let out = apply_batch(&state, &req.ids, |id| state.store.delete(id).map(|_| ()))?;
    tracing::info!(count = n, "credentials deleted in batch");
    Ok(out)
}

#[derive(Deserialize)]
struct BoolReq {
    value: bool,
}

#[derive(Deserialize)]
struct IntReq {
    value: i64,
}

#[derive(Deserialize)]
struct TextReq {
    value: String,
}

async fn set_disabled(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<BoolReq>,
) -> Result<Json<CredentialView>, ApiError> {
    state.store.set_disabled(id, req.value).map_err(internal)?;
    reload(&state, id)
}

async fn set_priority(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<IntReq>,
) -> Result<Json<CredentialView>, ApiError> {
    state.store.set_priority(id, req.value).map_err(internal)?;
    reload(&state, id)
}

async fn set_rpm_limit(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<IntReq>,
) -> Result<Json<CredentialView>, ApiError> {
    state.store.set_rpm_limit(id, req.value).map_err(internal)?;
    reload(&state, id)
}

async fn set_label(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<TextReq>,
) -> Result<Json<CredentialView>, ApiError> {
    let label = req.value.trim();
    if label.is_empty() {
        return Err(bad_request("the label must not be empty"));
    }
    state.store.set_label(id, label).map_err(internal)?;
    reload(&state, id)
}

async fn set_proxy(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<TextReq>,
) -> Result<Json<CredentialView>, ApiError> {
    // 校验失败是用户填错了，回 400 而不是 500——`validate_proxy` 的错误消息本身就是给
    // 人看的提示（哪个协议不收、端口为什么不合法）。
    state
        .store
        .set_proxy(id, Some(req.value.as_str()).filter(|s| !s.trim().is_empty()))
        .map_err(|e| bad_request(format!("{e:#}")))?;
    reload(&state, id)
}

/// 立刻刷新这个凭证的 token。
///
/// 只验证 refresh_token 与出站链路，**不碰模型也不花额度**——「这个号能不能用某个模型」
/// 由 [`test_credential`] 回答，它得真发一条请求。
///
/// 失败时与自动刷新那两条路**走同一套处置**（[`store::CredentialStore::note_refresh_failure`]）：
/// 判成永久失效就把号停用并写明理由。少了这一步的话，手动刷一个 refresh_token 已作废的号，
/// 看到的只是一句「刷新失败」，而库里它还是启用状态、`ban_reason` 是空的——选号照样挑它，
/// 每条转发各撞一次才知道不行。
async fn refresh_credential(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<CredentialView>, ApiError> {
    let cred = state.store.get(id).map_err(internal)?.ok_or_else(not_found)?;
    let client = state.clients.for_credential(&cred).map_err(|e| bad_request(format!("{e:#}")))?;
    // **失败要在服务端留痕**：`bad_request` 只把详情回给浏览器，而这条是排查授权问题时
    // 唯一有信息量的一行（上游的 error code 就在里面）。日志里只剩一句「400」等于没记。
    let set = oauth::refresh_token(&client, &cred.refresh_token, &cred.user_agent()).await.map_err(|e| {
        let banned = state.store.note_refresh_failure(&cred, &e);
        tracing::warn!(
            cred_id = id,
            label = %cred.label,
            banned = banned.unwrap_or("no"),
            error = %format!("{e:#}"),
            "manual token refresh failed"
        );
        // 停用了就在回给浏览器的那条里说清楚。用户按这个按钮是想知道「这个号还能不能用」，
        // 「失败了」与「失败了，而且它已经被关掉、要重新登录」是两个不同的答案。
        match banned {
            Some(code) => bad_request(format!(
                "{e:#}\n\nthis account has been disabled: the upstream rejected its refresh token \
                 ({code}) and that will not recover on its own — sign in again to restore it."
            )),
            None => bad_request(format!("{e:#}")),
        }
    })?;
    state
        .store
        .update_tokens(
            id,
            &set.access_token,
            &set.refresh_token,
            set.expires_at,
            set.id_token.as_deref(),
        )
        .map_err(internal)?;
    reload(&state, id)
}

/// 取这个账号当前可用的模型清单（给连通性测试的下拉用）。
///
/// **向上游现取而不是回一份写死的清单**：模型随上游上新/下线变化，写死的那一刻就开始过期
/// ——表现是下拉里缺新模型、留着已下线的，而用户拿它去测只会得到一串 400。详见
/// [`proxy::list_models`]。
///
/// 是个 GET，不消耗额度、不写用量流水。取不到时回 400 带原因，前端据此退回内置兜底清单
/// ——**下拉框绝不能因此变空**，那正是这个功能最初坏掉的样子。
async fn list_credential_models(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Vec<proxy::UpstreamModel>>, ApiError> {
    let cred = state.store.get(id).map_err(internal)?.ok_or_else(not_found)?;
    let models = proxy::list_models(&state, &cred).await.map_err(|e| {
        tracing::warn!(cred_id = id, label = %cred.label, error = %format!("{e:#}"), "fetching the model list failed");
        bad_request(format!("{e:#}"))
    })?;
    Ok(Json(models))
}

#[derive(Deserialize)]
struct TestReq {
    /// 要测的模型名（如 `gpt-5.1-codex`）。原样发给上游，不做白名单校验——模型名会随官方
    /// 上新变化，写死一份清单只会在下次上新时把新模型挡在外面，而「模型名不对」上游本来
    /// 就会回一条清清楚楚的 400/404，那正是这个功能要展示的东西。
    model: String,
}

/// 连通性测试：用**指定**账号向上游发一条最小请求，看这个号能不能用这个模型。
///
/// 停用/封禁的号也允许测——「它是不是已经恢复了」正是要问的问题，故这里只校验凭证存在。
/// 副作用与代价见 [`proxy::probe`]：不选号，但账号状态按真实流量的口径更新（429 打冷却、
/// 命中封号特征自动停用、通过则解除限流暂停），会写一条用量流水（卡片上的额度与花费据此
/// 更新），也真的会消耗一点点订阅额度。
///
/// **上游拒绝不是本接口的错误**：4xx/5xx 照样 200 返回一份结果，由前端展示状态码与原因。
/// 只有「凭证不存在」「模型名没填」才是 4xx——那两个是调用方的问题。
async fn test_credential(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<TestReq>,
) -> Result<Json<proxy::ProbeReport>, ApiError> {
    let model = req.model.trim();
    if model.is_empty() {
        return Err(bad_request("specify the model name to test"));
    }
    let cred = state.store.get(id).map_err(internal)?.ok_or_else(not_found)?;
    Ok(Json(proxy::probe(&state, &cred, model).await))
}

async fn clear_cooldown(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<CredentialView>, ApiError> {
    state.store.clear_cooldown(id);
    reload(&state, id)
}

/// 查这个号还剩几张额度重置券。
///
/// 向上游现问一趟并把读数落库（见 [`quota_reset::query`]）。不消耗券、不消耗额度、不写
/// 用量流水。取不到时回 400 带原因——张数是**只能问上游**的东西，编一个 0 会让人以为
/// 这个号没券。
async fn get_reset_credits(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<store::ResetCredits>, ApiError> {
    let cred = state.store.get(id).map_err(internal)?.ok_or_else(not_found)?;
    let credits = quota_reset::query(&state, &cred).await.map_err(|e| {
        tracing::warn!(cred_id = id, label = %cred.label, error = %format!("{e:#}"), "reading the reset-credit count failed");
        bad_request(format!("{e:#}"))
    })?;
    Ok(Json(credits))
}

/// 兑一张重置券，把这个号的额度窗口重置掉。
///
/// **不可撤销、券花掉就没有**，所以二次确认由前端做（见 admin-ui 的 `ResetQuotaDialog`）。
/// 停用/封禁的号也允许兑：额度与账号状态是两件事，而「先重置额度再处理停用原因」是合理
/// 顺序，这里只校验凭证存在。
///
/// 成功时连带回一份最新的凭证视图：兑换会顺手解除限流暂停与冷却（见
/// [`quota_reset::consume`]），列表要立刻把那枚「限流暂停」的徽章摘掉，而不是等下一轮轮询。
async fn consume_reset_credit(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<ResetResp>, ApiError> {
    let cred = state.store.get(id).map_err(internal)?.ok_or_else(not_found)?;
    let outcome = quota_reset::consume(&state, &cred).await.map_err(|e| {
        tracing::warn!(cred_id = id, label = %cred.label, error = %format!("{e:#}"), "redeeming a reset credit failed");
        bad_request(format!("{e:#}"))
    })?;
    // 兑换已经成功，这里再读一次库拿最新视图。读失败也不能把整次兑换报成失败——
    // 那会让人再点一次，第二张券就这么没了；回一份重载前的视图，列表下一轮轮询自会更新。
    let credential = match state.store.get(id) {
        Ok(Some(c)) => view_of(&state, &c),
        Ok(None) | Err(_) => view_of(&state, &cred),
    };
    Ok(Json(ResetResp { outcome, credential }))
}

/// 一次兑换的对外结果：上游那份结果 + 兑换后的凭证视图。
#[derive(Serialize)]
struct ResetResp {
    #[serde(flatten)]
    outcome: quota_reset::ResetOutcome,
    credential: CredentialView,
}

fn reload(state: &AppState, id: i64) -> Result<Json<CredentialView>, ApiError> {
    let c = state.store.get(id).map_err(internal)?.ok_or_else(not_found)?;
    Ok(Json(view_of(state, &c)))
}

// ---------- 用量与指标 ----------

#[derive(Deserialize)]
struct UsageQuery {
    #[serde(default = "default_usage_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
    /// 翻页锚点（Unix 秒）。首次不传，之后把响应里的 `anchor` 原样带回。
    #[serde(default)]
    until: Option<i64>,
}

fn default_usage_limit() -> i64 {
    25
}

/// 上限钉死：一个 `?limit=1000000` 会把整张流水表读进内存再序列化成 JSON。
const USAGE_PAGE_MAX: i64 = 200;

async fn list_usage(
    State(state): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> Result<Json<store::UsagePage>, ApiError> {
    let limit = q.limit.clamp(1, USAGE_PAGE_MAX);
    Ok(Json(state.store.list_usage_page(None, limit, q.offset.max(0), q.until).map_err(internal)?))
}

async fn list_credential_usage(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<UsageQuery>,
) -> Result<Json<store::UsagePage>, ApiError> {
    let limit = q.limit.clamp(1, USAGE_PAGE_MAX);
    Ok(Json(
        state.store.list_usage_page(Some(id), limit, q.offset.max(0), q.until).map_err(internal)?,
    ))
}

#[derive(Serialize)]
struct MetricsResp {
    credentials_total: usize,
    credentials_enabled: usize,
    /// 全池最近一个窗口内转发的请求总数（各账号之和）。
    rpm: i64,
    /// 上面那个数的窗口长度（秒）。**跟着回**，别让前端写死 60：改了后端窗口而前端还
    /// 按 60 解释，界面上那句「最近 60 秒」就成了假话。
    window_secs: i64,
    in_flight: i64,
    cost_total_usd: f64,
    requests_total: i64,
    /// 全池终身累计的输入 token（**已含命中缓存那部分**）与其中命中缓存的部分。
    ///
    /// 两个数一起回、由界面算全池缓存命中率（`cached / input`），同 [`store::UsagePage`] 上
    /// 那两项的取舍：先回原始数，比率与它作不作数由看的人判断。
    ///
    /// 各账号之和而不是另开一条 SQL：这两个数就躺在账本里，而这个接口本来已经在遍历账号
    /// 取花费与请求数了。
    input_tokens_total: i64,
    cached_tokens_total: i64,
}

async fn get_metrics(State(state): State<AppState>) -> Result<Json<MetricsResp>, ApiError> {
    let list = state.store.list().map_err(internal)?;
    let mut cost = 0.0;
    let mut requests = 0;
    let mut rpm = 0;
    let mut input_tokens = 0;
    let mut cached_tokens = 0;
    for c in &list {
        let s = state.store.stats_of(c.id).unwrap_or_default();
        cost += s.cost_total_usd;
        requests += s.request_total;
        input_tokens += s.input_tokens_total;
        cached_tokens += s.cached_tokens_total;
        rpm += state.store.current_rpm(c.id);
    }
    Ok(Json(MetricsResp {
        credentials_total: list.len(),
        credentials_enabled: list.iter().filter(|c| !c.disabled).count(),
        rpm,
        window_secs: store::RPM_WINDOW_SECS as i64,
        in_flight: state.in_flight.load(std::sync::atomic::Ordering::Relaxed),
        cost_total_usd: cost,
        requests_total: requests,
        input_tokens_total: input_tokens,
        cached_tokens_total: cached_tokens,
    }))
}

#[derive(Deserialize)]
struct CacheSeriesQuery {
    /// 回看多少小时。缺省 7 天——比 24 小时更能看出「调完粘性有没有效果」，又不至于把
    /// 一次调整摊薄在 30 天里。
    #[serde(default = "default_cache_series_hours")]
    hours: i64,
}

fn default_cache_series_hours() -> i64 {
    7 * 24
}

#[derive(Serialize)]
struct CacheSeriesResp {
    /// 这条曲线的起点（Unix 秒）。**由服务端回**而不是让前端自己算：夹过之后的真实起点
    /// 可能比它要的短（流水只留 30 天），x 轴该照真实的那个画。
    since: i64,
    /// 桶宽，固定 3600。写进响应而不是让前端假定：哪天服务端改了分桶，画图那头不该跟着错。
    bucket_secs: i64,
    points: Vec<store::CacheBucket>,
}

/// 全池缓存命中率的逐小时流水合计。比率不在这里算，见 [`store::CacheBucket`]。
async fn get_cache_series(
    State(state): State<AppState>,
    Query(q): Query<CacheSeriesQuery>,
) -> Result<Json<CacheSeriesResp>, ApiError> {
    // 上限就是流水的保留期：问得更远也只能拿到被裁剩下的那一段，不如把跨度如实夹住，
    // 好过回一条无声变短的曲线。
    let max_hours = store::USAGE_LOG_RETENTION_SECS / 3600;
    let hours = q.hours.clamp(1, max_hours);
    let since = crate::credentials::now_secs() as i64 - hours * 3600;
    let points = state.store.cache_series(since).map_err(internal)?;
    Ok(Json(CacheSeriesResp { since, bucket_secs: 3600, points }))
}

#[derive(Serialize)]
struct CacheReasonsResp {
    /// 同 [`CacheSeriesResp::since`]：夹过之后的真实起点。
    since: i64,
    reasons: Vec<store::CacheReasonStat>,
}

/// 缓存未命中的原因分布。回答命中率曲线的下一个问题:**为什么低**。
///
/// 比率与排序都在 [`store::CredentialStore::cache_reasons`] 那头定好（按白付的输入 token
/// 排，不是按条数）——那个口径不该让前端各自复述一遍。
async fn get_cache_reasons(
    State(state): State<AppState>,
    Query(q): Query<CacheSeriesQuery>,
) -> Result<Json<CacheReasonsResp>, ApiError> {
    let max_hours = store::USAGE_LOG_RETENTION_SECS / 3600;
    let hours = q.hours.clamp(1, max_hours);
    let since = crate::credentials::now_secs() as i64 - hours * 3600;
    let reasons = state.store.cache_reasons(since).map_err(internal)?;
    Ok(Json(CacheReasonsResp { since, reasons }))
}

// ---------- 设置 ----------

#[derive(Serialize)]
struct SettingsResp {
    /// 接入 key 的**明文**（空串 = 未设置）。
    ///
    /// **明文回给管理界面是有意的**：那边要拼一段可直接粘进 `~/.codex/config.toml` 的
    /// 配置片段，而 key 正是其中一半。这个接口在 [`auth::require_admin`] 之后，与「能改
    /// 设置、能看账号」是同一道门——一个已经能改 key 的人，看见现有 key 不构成新的暴露。
    ///
    /// 代价要写明：**未设管理密码时管理接口是完全敞开的**，那时能打到这个端口的人就能
    /// 读到这个 key。故设置页把「设置管理密码」摆在同一屏里提示。
    api_key: String,
    /// 接入 key 是否由命令行/环境变量接管（true = 网页不可改）。
    env_managed: bool,
    default_rpm_limit: i64,
    rate_limit_retry_max: i64,
    /// 撞 429 之后是换个号重发（true），还是就地等一等再发同一个号（false）。
    rate_limit_rotate: bool,
    /// 不换号时，一次就地重试最多愿意等多久（秒）。
    rate_limit_wait_secs: i64,
    /// 不换号时，同一个号最多就地重试几次。
    rate_limit_wait_retry_max: i64,
    quota_pause_pct: i64,
    cooldown_secs: i64,
    /// 会话落点的租约时长（秒，0 = 关闭）。
    session_lease_secs: i64,
    /// 转发前是否把 `tools[]` 按名字排序。
    normalize_tool_order: bool,
    /// 发往上游的 UA 怎么处理（0 透传 / 1 只改写不像官方客户端的 / 2 一律改写）。
    upstream_ua_mode: i64,
    /// 管理鉴权是否已开启（前端据此决定要不要显示「导出」这类高危操作）。
    admin_configured: bool,
    version: &'static str,
}

async fn get_settings(State(state): State<AppState>) -> Json<SettingsResp> {
    let s = &state.store;
    Json(SettingsResp {
        // 环境接管时回环境里那份：网页上是只读的，但配置片段仍要拼得出来。
        api_key: match &state.client_key {
            Some(k) => k.as_str().to_owned(),
            None => s.get_setting(store::CLIENT_API_KEY).ok().flatten().unwrap_or_default(),
        },
        env_managed: state.client_key.is_some(),
        default_rpm_limit: s.get_setting_i64(store::DEFAULT_RPM_LIMIT, 0),
        rate_limit_retry_max: s
            .get_setting_i64(store::RATE_LIMIT_RETRY_MAX, store::DEFAULT_RATE_LIMIT_RETRY_MAX),
        rate_limit_rotate: s
            .get_setting_i64(store::RATE_LIMIT_ROTATE, store::DEFAULT_RATE_LIMIT_ROTATE)
            != 0,
        rate_limit_wait_secs: s
            .get_setting_i64(store::RATE_LIMIT_WAIT_SECS, store::DEFAULT_RATE_LIMIT_WAIT_SECS),
        rate_limit_wait_retry_max: s.get_setting_i64(
            store::RATE_LIMIT_WAIT_RETRY_MAX,
            store::DEFAULT_RATE_LIMIT_WAIT_RETRY_MAX,
        ),
        quota_pause_pct: s.get_setting_i64(store::QUOTA_PAUSE_PCT, store::DEFAULT_QUOTA_PAUSE_PCT),
        cooldown_secs: s.get_setting_i64(store::COOLDOWN_SECS, store::DEFAULT_COOLDOWN_SECS),
        session_lease_secs: s
            .get_setting_i64(store::SESSION_LEASE_SECS, store::DEFAULT_SESSION_LEASE_SECS),
        normalize_tool_order: s
            .get_setting_i64(store::NORMALIZE_TOOL_ORDER, store::DEFAULT_NORMALIZE_TOOL_ORDER)
            != 0,
        upstream_ua_mode: s
            .get_setting_i64(store::UPSTREAM_UA_MODE, store::DEFAULT_UPSTREAM_UA_MODE),
        admin_configured: auth::admin_configured(&state),
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn set_api_key(
    State(state): State<AppState>,
    Json(req): Json<TextReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    if state.client_key.is_some() {
        return Err(bad_request(
            "the access key is managed by --api-key / COBAN_API_KEY and cannot be changed here",
        ));
    }
    let key = req.value.trim();
    if key.is_empty() {
        state.store.delete_setting(store::CLIENT_API_KEY).map_err(internal)?;
        tracing::warn!("access key cleared: the proxy no longer authenticates callers");
    } else {
        state.store.set_setting(store::CLIENT_API_KEY, key).map_err(internal)?;
        tracing::info!("access key updated");
    }
    // 回最新设置而不是 `{ok:true}`：前端保存后要立刻用新 key 重拼配置片段，
    // 回一个空壳就得再拉一次，中间那一帧显示的是旧值。
    Ok(get_settings(State(state)).await)
}

/// 写一个整数设置，带范围校验。
async fn set_int_setting(
    state: AppState,
    key: &str,
    value: i64,
    range: std::ops::RangeInclusive<i64>,
) -> Result<Json<SettingsResp>, ApiError> {
    if !range.contains(&value) {
        return Err(bad_request(format!(
            "value must be between {} and {}",
            range.start(),
            range.end()
        )));
    }
    state.store.set_setting(key, &value.to_string()).map_err(internal)?;
    tracing::info!(key, value, "setting changed");
    Ok(get_settings(State(state)).await)
}

async fn set_default_rpm_limit(
    State(state): State<AppState>,
    Json(req): Json<IntReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    set_int_setting(state, store::DEFAULT_RPM_LIMIT, req.value, 0..=100_000).await
}

async fn set_rate_limit_retry_max(
    State(state): State<AppState>,
    Json(req): Json<IntReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    set_int_setting(state, store::RATE_LIMIT_RETRY_MAX, req.value, 0..=8).await
}

/// 开关：撞上游 429 之后换个号重发，还是就地等一等再发同一个号
/// （见 `store::RATE_LIMIT_ROTATE`）。
async fn set_rate_limit_rotate(
    State(state): State<AppState>,
    Json(req): Json<IntReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    set_int_setting(state, store::RATE_LIMIT_ROTATE, req.value, 0..=1).await
}

/// 不换号时一次就地重试最多等多久（秒）。
///
/// 上限 3600：再往上就不是「等一等」而是把一条客户端请求挂死了——那种情形该让客户端
/// 自己拿着 429 走，而不是在服务端占着连接。
async fn set_rate_limit_wait_secs(
    State(state): State<AppState>,
    Json(req): Json<IntReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    set_int_setting(state, store::RATE_LIMIT_WAIT_SECS, req.value, 1..=3600).await
}

/// 不换号时同一个号最多就地重试几次（`0` = 一次都不等）。
async fn set_rate_limit_wait_retry_max(
    State(state): State<AppState>,
    Json(req): Json<IntReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    set_int_setting(state, store::RATE_LIMIT_WAIT_RETRY_MAX, req.value, 0..=8).await
}

async fn set_quota_pause_pct(
    State(state): State<AppState>,
    Json(req): Json<IntReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    set_int_setting(state, store::QUOTA_PAUSE_PCT, req.value, 0..=100).await
}

async fn set_cooldown_secs(
    State(state): State<AppState>,
    Json(req): Json<IntReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    set_int_setting(state, store::COOLDOWN_SECS, req.value, 1..=86_400).await
}

/// 开关：转发前要不要把 `tools[]` 按名字排一遍（见 `proxy::normalize_tool_order`）。
///
/// 布尔项走 `IntReq` 的 0/1 而不是另立一个请求体类型：设置那一族的写接口形状统一，
/// 前端也就只有一套调用方式。
async fn set_normalize_tool_order(
    State(state): State<AppState>,
    Json(req): Json<IntReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    set_int_setting(state, store::NORMALIZE_TOOL_ORDER, req.value, 0..=1).await
}

/// 发往上游的 `User-Agent` 怎么处理（见 `proxy::UaMode`）。
///
/// 三档走一个整数而不是两个布尔：`0/1/2` 是有序的三种收敛强度，拆成布尔组合会出现
/// 「透传 + 一律改写」这种没有意义的取值。
async fn set_upstream_ua_mode(
    State(state): State<AppState>,
    Json(req): Json<IntReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    set_int_setting(state, store::UPSTREAM_UA_MODE, req.value, 0..=2).await
}

/// 0 是合法值：把会话租约整个关掉，落点退回按会话键现算（见 `store::SessionLeases`）。
async fn set_session_lease_secs(
    State(state): State<AppState>,
    Json(req): Json<IntReq>,
) -> Result<Json<SettingsResp>, ApiError> {
    set_int_setting(state, store::SESSION_LEASE_SECS, req.value, 0..=86_400).await
}

// ---------- 杂项 ----------

fn bad_request(msg: impl Into<String>) -> ApiError {
    (StatusCode::BAD_REQUEST, msg.into())
}

fn not_found() -> ApiError {
    (StatusCode::NOT_FOUND, "not found".into())
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    // 错误详情只回给客户端、服务端不留痕的话，500 在日志里查不到。
    let msg = e.to_string();
    tracing::error!(error = %msg, "api endpoint internal error");
    (StatusCode::INTERNAL_SERVER_ERROR, msg)
}

/// 尽力打开浏览器。**失败只记一行日志**：服务本身照常跑，为一个便利功能退出没有道理。
fn open_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let cmd = ("open", vec![url]);
    #[cfg(target_os = "windows")]
    let cmd = ("cmd", vec!["/C", "start", "", url]);
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = ("xdg-open", vec![url]);

    match std::process::Command::new(cmd.0).args(&cmd.1).spawn() {
        Ok(_) => tracing::info!(url, "tried to open the browser"),
        Err(e) => {
            tracing::warn!(url, error = %e, "could not open a browser; open the url manually")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 掩码后不能等于原文，短串也要整条打掉。
    #[test]
    fn account_id_masking_never_leaks_the_whole_value() {
        assert_eq!(mask("abc"), "***");
        assert_eq!(mask("abcdef"), "******");
        let long = "e141d2d8-2d72-42af-b602-841b58000000";
        let masked = mask(long);
        assert!(masked.starts_with('…') && long.ends_with(masked.trim_start_matches('…')));
        assert!(masked.len() < long.len());
    }

    /// 把一组 claim 拼成形态正确的 access_token（不签名，解析侧本就不验签）。
    fn fake_access_token(exp: Option<u64>) -> String {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let mut body = serde_json::json!({ "iss": crate::config::ISSUER });
        if let Some(exp) = exp {
            body["exp"] = serde_json::json!(exp);
        }
        let head = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256"}"#);
        format!("{head}.{}.sig", URL_SAFE_NO_PAD.encode(serde_json::to_vec(&body).unwrap()))
    }

    /// access_token 自己声明的 `exp` 说了算。这条是导入的关键：refresh_token 已经作废、
    /// access_token 还剩几天的号，靠这个值才留得住——退回 0 的话它会在导入的第一秒被一次
    /// 必然失败的刷新废掉。
    #[test]
    fn import_takes_the_expiry_from_the_access_token() {
        let at = fake_access_token(Some(1_788_092_074));
        // 文件里那个字段存在也不抢：token 自证的那份能校验签发方，更可信。
        let tokens = serde_json::json!({ "expires_at": 1 });
        assert_eq!(import_expires_at(&at, &tokens), 1_788_092_074);
    }

    /// access_token 里没有 `exp`（或它压根不是 JWT）时退到文件里那个字段，数字与数字字符串
    /// 两种写法都收——只认一种的代价是「时间戳明明在文件里却当成缺失」，表现上看不出原因。
    #[test]
    fn import_falls_back_to_the_file_field_in_either_json_shape() {
        let no_exp = fake_access_token(None);
        for v in [serde_json::json!(1_788_092_074_u64), serde_json::json!("1788092074")] {
            let tokens = serde_json::json!({ "expires_at": v });
            assert_eq!(import_expires_at(&no_exp, &tokens), 1_788_092_074, "{tokens}");
            assert_eq!(import_expires_at("not-a-jwt", &tokens), 1_788_092_074, "{tokens}");
        }
    }

    /// 两处都取不到才退回 0 —— 保持原来的行为：立刻刷一次，让「这份文件不能用了」当场
    /// 暴露。别处签的 token 也走这条：它的 `exp` 一概不认。
    #[test]
    fn import_falls_back_to_an_immediate_refresh_when_nothing_is_known() {
        let empty = serde_json::json!({});
        assert_eq!(import_expires_at(&fake_access_token(None), &empty), 0);
        assert_eq!(import_expires_at("not-a-jwt", &empty), 0);
        assert_eq!(import_expires_at("", &serde_json::json!({ "expires_at": "soon" })), 0);
    }

    /// PKCE 表按 state 索引：并发登录不能互相踩，取走之后不可复用。
    #[test]
    fn pkce_entries_are_isolated_per_login_attempt() {
        let state = AppState {
            clients: Arc::new(crate::clients::ClientPool::new().unwrap()),
            pkce: Arc::new(parking_lot::Mutex::new(Vec::new())),
            store: Arc::new(CredentialStore::open_in_memory().unwrap()),
            client_key: None,
            admin_env: None,
            in_flight: Arc::default(),
            models_cache: Arc::default(),
            stale_reasoning: Arc::default(),
            input_rules: Arc::default(),
            schema_keywords: Arc::default(),
        };
        let a = PkceChallenge::generate();
        let b = PkceChallenge::generate();
        let (ka, kb) = (a.state.clone(), b.state.clone());
        let (va, vb) = (a.verifier.clone(), b.verifier.clone());
        remember_pkce(&state, a);
        remember_pkce(&state, b);

        assert_eq!(
            take_pkce(&state, &ka).unwrap().verifier,
            va,
            "must not get the other attempt's verifier"
        );
        assert!(take_pkce(&state, &ka).is_none(), "an attempt must not be reusable");
        assert_eq!(take_pkce(&state, &kb).unwrap().verifier, vb);
    }

    /// 待完成的登录尝试有上限，反复点「添加账号」不该把内存撑起来。
    #[test]
    fn pending_pkce_is_bounded() {
        let state = AppState {
            clients: Arc::new(crate::clients::ClientPool::new().unwrap()),
            pkce: Arc::new(parking_lot::Mutex::new(Vec::new())),
            store: Arc::new(CredentialStore::open_in_memory().unwrap()),
            client_key: None,
            admin_env: None,
            in_flight: Arc::default(),
            models_cache: Arc::default(),
            stale_reasoning: Arc::default(),
            input_rules: Arc::default(),
            schema_keywords: Arc::default(),
        };
        for _ in 0..PKCE_MAX_PENDING * 2 {
            remember_pkce(&state, PkceChallenge::generate());
        }
        assert!(state.pkce.lock().len() <= PKCE_MAX_PENDING);
    }
}
