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
        .route("/credentials/{id}/usage", get(list_credential_usage))
        .route("/usage", get(list_usage))
        .route("/metrics", get(get_metrics))
        .route("/settings", get(get_settings))
        .route("/settings/api-key", post(set_api_key))
        .route("/settings/default-rpm-limit", post(set_default_rpm_limit))
        .route("/settings/rate-limit-retry-max", post(set_rate_limit_retry_max))
        .route("/settings/quota-pause-pct", post(set_quota_pause_pct))
        .route("/settings/cooldown-secs", post(set_cooldown_secs))
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
    /// `~/.codex/auth.json` 的内容。
    content: String,
}

/// 从既有的 `~/.codex/auth.json` 导入一个已登录账号。
///
/// 存在的理由：机器上已经 `codex login` 过的账号不必再走一遍浏览器授权，而在无图形界面
/// 的服务器上「打开浏览器粘回调」这条路本身就很难走。
async fn import_auth_json(
    State(state): State<AppState>,
    Json(req): Json<ImportAuthJsonReq>,
) -> Result<Json<CredentialView>, ApiError> {
    let v: serde_json::Value = serde_json::from_str(req.content.trim())
        .map_err(|e| bad_request(format!("that is not valid JSON: {e}")))?;
    let tokens = v.get("tokens").unwrap_or(&v);
    let get = |k: &str| tokens.get(k).and_then(|x| x.as_str()).map(str::to_owned);

    let access_token =
        get("access_token").ok_or_else(|| bad_request("no tokens.access_token in that file"))?;
    let refresh_token = get("refresh_token").ok_or_else(|| {
        bad_request("no tokens.refresh_token in that file (was it an API-key login?)")
    })?;
    let id_token = get("id_token");
    let claims = id_token.as_deref().map(oauth::Claims::parse).unwrap_or_default();
    // account_id 优先取 auth.json 自己那份，缺了再从 id_token 的 claim 里找。
    let account_id = get("account_id").or_else(|| claims.account_id.clone());

    save_token_set(
        &state,
        oauth::TokenSet {
            access_token,
            refresh_token,
            // 导入的 token 大概率已经过期或将过期——**记成 0 让它立刻走一次刷新**，
            // 而不是照抄 `last_refresh + 3600` 猜一个可能已经过的时刻。第一条请求会
            // 顺带把它换成新的，且刷新失败能立刻暴露出「这份文件已经不能用了」。
            expires_at: 0,
            id_token,
            claims: oauth::Claims { account_id, ..claims },
        },
    )
    .map(Json)
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
async fn refresh_credential(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<CredentialView>, ApiError> {
    let cred = state.store.get(id).map_err(internal)?.ok_or_else(not_found)?;
    let client = state.clients.for_credential(&cred).map_err(|e| bad_request(format!("{e:#}")))?;
    // **失败要在服务端留痕**：`bad_request` 只把详情回给浏览器，而这条是排查授权问题时
    // 唯一有信息量的一行（上游的 error code 就在里面）。日志里只剩一句「400」等于没记。
    let set = oauth::refresh_token(&client, &cred.refresh_token).await.map_err(|e| {
        tracing::warn!(cred_id = id, label = %cred.label, error = %format!("{e:#}"), "manual token refresh failed");
        bad_request(format!("{e:#}"))
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
}

async fn get_metrics(State(state): State<AppState>) -> Result<Json<MetricsResp>, ApiError> {
    let list = state.store.list().map_err(internal)?;
    let mut cost = 0.0;
    let mut requests = 0;
    let mut rpm = 0;
    for c in &list {
        let s = state.store.stats_of(c.id).unwrap_or_default();
        cost += s.cost_total_usd;
        requests += s.request_total;
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
    }))
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
    quota_pause_pct: i64,
    cooldown_secs: i64,
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
        quota_pause_pct: s.get_setting_i64(store::QUOTA_PAUSE_PCT, store::DEFAULT_QUOTA_PAUSE_PCT),
        cooldown_secs: s.get_setting_i64(store::COOLDOWN_SECS, store::DEFAULT_COOLDOWN_SECS),
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
        };
        for _ in 0..PKCE_MAX_PENDING * 2 {
            remember_pkce(&state, PkceChallenge::generate());
        }
        assert!(state.pkce.lock().len() <= PKCE_MAX_PENDING);
    }
}
