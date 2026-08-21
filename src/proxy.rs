//! 转发代理：Codex CLI → coban → ChatGPT 后端（`backend-api/codex`）。
//!
//! 透传请求体，只替换鉴权：校验来访 API Key 后选一个凭证，注入它的 OAuth access_token
//! 与 `chatgpt-account-id`，响应流式原样回传，顺带从 SSE 里嗅探用量与额度。

use std::sync::Arc;
use std::time::Instant;

use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;

use crate::config;
use crate::credentials::Credential;
use crate::store::{self, CredentialStore, QuotaSnapshot, UsageRecord};
use crate::web::AppState;

/// 一次转发最多尝试几个凭证（含第一次）。
///
/// 上限存在的意义是**不让一次客户端请求把所有账号轮一遍**：每次重试都要重发整个请求体，
/// 账号多的时候一条打不通的请求能拖上几十秒，而客户端那头早就超时了。真正的换号次数
/// 还受 [`store::RATE_LIMIT_RETRY_MAX`] 限制，这里只是硬顶。
const MAX_ATTEMPTS: usize = 8;

/// 记进日志的 UA 截断长度。完整 UA 可以很长，而认「谁在发」只需要前面那截。
const UA_MAX_LEN: usize = 120;

/// 转发入口。
pub async fn handle(
    State(state): State<AppState>,
    Path(path): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // 在飞计数随响应流一起存活，故这个 guard 要交给最终的响应体持有（见 stream_upstream）。
    let in_flight = InFlightGuard::new(state.in_flight.clone());

    if let Some(expected) = effective_client_key(&state) {
        if !client_authorized(&headers, &expected) {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "invalid_api_key",
                "invalid API key; set the key configured in coban's access settings",
            );
        }
    }

    // 请求体在重试间要重发多次，规范化只做一次（见 normalize_responses_body）。
    let Normalized { body, collapse } = normalize_responses_body(&path, body);

    let started = Instant::now();
    let retry_max = state
        .store
        .get_setting_i64(store::RATE_LIMIT_RETRY_MAX, store::DEFAULT_RATE_LIMIT_RETRY_MAX);
    // 允许的总尝试数 = 1 次首发 + retry_max 次换号，再被 MAX_ATTEMPTS 硬顶住。
    let budget = ((retry_max.max(0) + 1) as usize).min(MAX_ATTEMPTS);

    let mut tried: Vec<i64> = Vec::new();
    let mut last: Option<Response> = None;

    for attempt in 0..budget {
        let cred = match state.store.select(&tried) {
            Ok(c) => c,
            Err(e) => {
                // 一个都挑不出来时：已经试过号的话把上一次的上游响应交回去（那是更贴近
                // 真相的错误），一次都没试过才回自己造的错。
                return last.unwrap_or_else(|| select_error_response(&e));
            }
        };
        tried.push(cred.id);

        if let Err(e) = state.store.take_rpm_slot(&cred) {
            if let Some(rl) = e.downcast_ref::<store::RpmLimited>() {
                tracing::debug!(
                    cred_id = cred.id,
                    limit = rl.limit,
                    "rpm limit reached, trying next"
                );
                last = Some(rate_limit_response(rl.retry_after_secs, &e.to_string()));
                continue;
            }
            return internal_error(&e);
        }

        match forward_once(
            &state,
            &cred,
            &path,
            &method,
            &uri,
            &headers,
            &body,
            collapse,
            started,
            in_flight.clone(),
        )
        .await
        {
            Ok(Outcome::Done(resp)) => return resp,
            Ok(Outcome::TryNext(resp)) => {
                tracing::info!(
                    cred_id = cred.id,
                    attempt = attempt + 1,
                    status = resp.status().as_u16(),
                    "upstream rejected this credential, trying the next one"
                );
                last = Some(resp);
            }
            Err(e) => {
                tracing::warn!(cred_id = cred.id, error = %format!("{e:#}"), "forwarding failed");
                last = Some(error_response(
                    StatusCode::BAD_GATEWAY,
                    "upstream_error",
                    format!("{e:#}"),
                ));
            }
        }
    }

    last.unwrap_or_else(|| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_credential_available",
            "every credential failed for this request; check the accounts page",
        )
    })
}

/// 一次转发的结果：定局，还是「换个号再来」。
enum Outcome {
    Done(Response),
    /// 上游拒了这个凭证（限流/额度/账号级错误）。附带的响应是兜底——重试全失败时交回它。
    TryNext(Response),
}

/// 用指定凭证发一次。
#[allow(clippy::too_many_arguments)]
async fn forward_once(
    state: &AppState,
    cred: &Credential,
    path: &str,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &Bytes,
    collapse: bool,
    started: Instant,
    in_flight: InFlightGuard,
) -> anyhow::Result<Outcome> {
    let client = state.clients.for_credential(cred)?;
    let token = state.store.valid_access_token(&state.clients, cred).await?;

    let url = upstream_url(path, uri.query());
    let mut fwd_headers = build_forward_headers(headers, cred, &token);
    if collapse {
        // 体里的 `stream` 已被我们钉成 true，`accept` 得跟着说 SSE：官方客户端不存在
        // 「体里要流、头里要 JSON」这种自相矛盾的形态，别让上游去猜。
        fwd_headers.insert(header::ACCEPT, HeaderValue::from_static("text/event-stream"));
    }

    let req = client
        .request(wreq::Method::from_bytes(method.as_str().as_bytes())?, &url)
        .headers(fwd_headers)
        .body(body.clone());

    let up = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            // 连接层失败不算这个账号的错（除非它配了个坏代理，而那在建客户端时就报了），
            // 换个号重试一次是有意义的——尤其逐账号代理各走各的出口。
            return Ok(Outcome::TryNext(error_response(
                StatusCode::BAD_GATEWAY,
                "upstream_unreachable",
                format!("could not reach upstream: {e}"),
            )));
        }
    };

    let status = StatusCode::from_u16(up.status().as_u16())?;
    let quota = QuotaSnapshot::from_headers(up.headers());
    // 转发路径不关心停没停：这条请求已经在飞，暂停只影响后面的选号。
    let _ = maybe_pause_on_quota(state, cred, &quota);

    // 非 2xx：先把体读出来判一判是不是账号级问题，再决定换号还是交回客户端。
    if !status.is_success() {
        let up_headers = up.headers().clone();
        let bytes = up.bytes().await.unwrap_or_default();
        log_usage(state, cred, path, headers, status.as_u16() as i64, None, &quota, started, None);

        if let Some(reason) = detect_account_error(status, &bytes) {
            state.store.mark_banned(cred.id, &reason)?;
            return Ok(Outcome::TryNext(passthrough(status, &up_headers, bytes)));
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            let secs = retry_after_secs(&up_headers).unwrap_or_else(|| {
                state.store.get_setting_i64(store::COOLDOWN_SECS, store::DEFAULT_COOLDOWN_SECS)
            });
            state.store.note_rate_limited(cred.id, secs);
            return Ok(Outcome::TryNext(passthrough(status, &up_headers, bytes)));
        }
        // 其余（400/404/422…）是这条请求本身的问题，换号也不会好，原样交回。
        return Ok(Outcome::Done(passthrough(status, &up_headers, bytes)));
    }

    if collapse {
        return Ok(Outcome::Done(
            collapse_upstream(state, cred, path, headers, up, quota, started).await,
        ));
    }

    Ok(Outcome::Done(stream_upstream(state, cred, path, headers, up, quota, started, in_flight)))
}

/// 把上游的 SSE 收拢成一个一次性 JSON 响应。
///
/// 只在客户端没要流时走这里（见 [`Normalized`]）：体里的 `stream` 被钉成了 true，
/// 上游必然回 SSE，而这个客户端等的是一个 `response` 对象，把事件流原样交给它等于
/// 让它读一堆读不懂的 `data:` 行。
///
/// 代价是这条路径不再是流式的：整段响应读完才回。要延迟就该在请求里写 `stream: true`。
async fn collapse_upstream(
    state: &AppState,
    cred: &Credential,
    path: &str,
    req_headers: &HeaderMap,
    up: wreq::Response,
    quota: QuotaSnapshot,
    started: Instant,
) -> Response {
    let status = StatusCode::from_u16(up.status().as_u16()).unwrap_or(StatusCode::OK);
    let up_headers = up.headers().clone();
    let bytes = match up.bytes().await {
        Ok(b) => b,
        Err(e) => {
            log_usage(
                state,
                cred,
                path,
                req_headers,
                status.as_u16() as i64,
                None,
                &quota,
                started,
                None,
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                format!("failed to read the upstream response body: {e}"),
            );
        }
    };

    // 用量与流式路径共用同一个嗅探器：SSE 的形态是一样的，两处各写一份解析必然走岔。
    let mut sniffer = UsageSniffer::default();
    sniffer.feed(&bytes);
    let model = sniffer.model.clone();
    log_usage(
        state,
        cred,
        path,
        req_headers,
        status.as_u16() as i64,
        model,
        &quota,
        started,
        sniffer.usage,
    );

    // 上游会先回 200 再在流里说这次生成失败；非流式客户端读不到那个事件，得翻成 HTTP 错误。
    if let Some((etype, message)) = sse_failure(&bytes) {
        return error_response(
            StatusCode::BAD_GATEWAY,
            etype.as_deref().unwrap_or("upstream_error"),
            message,
        );
    }

    let Some(resp) = sse_final_response(&bytes) else {
        return error_response(
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "the upstream stream ended without a completed response",
        );
    };

    let body = serde_json::to_vec(&resp)
        .unwrap_or_else(|_| error_body("internal_error", "failed to serialize the response"));
    let mut builder = Response::builder().status(status);
    for (name, value) in up_headers.iter() {
        // 逐条跳过的理由同 resp_builder；`content-type` 也不能照抄——上游说的是
        // `text/event-stream`，而这里交出去的是一个 JSON 对象。
        if matches!(
            name.as_str(),
            "content-length"
                | "content-encoding"
                | "transfer-encoding"
                | "connection"
                | "content-type"
        ) {
            continue;
        }
        builder = builder.header(name.as_str(), value.as_bytes());
    }
    builder
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|e| internal_error_plain(&e.to_string()))
}

/// 把上游响应流式转出去，边转边嗅探用量。
#[allow(clippy::too_many_arguments)]
fn stream_upstream(
    state: &AppState,
    cred: &Credential,
    path: &str,
    req_headers: &HeaderMap,
    up: wreq::Response,
    quota: QuotaSnapshot,
    started: Instant,
    in_flight: InFlightGuard,
) -> Response {
    let builder = resp_builder(&up);
    let sniffer = Arc::new(parking_lot::Mutex::new(UsageSniffer::default()));

    // 落库交给一个 Drop guard：流可能因为客户端断开而提前结束，那种情况下也该留下记录，
    // 而「读到流末尾再记」在断开时压根不会执行。
    let guard = UsageLogGuard {
        store: state.store.clone(),
        sniffer: sniffer.clone(),
        cred_id: cred.id,
        cred_label: cred.label.clone(),
        session_id: incoming_session_id(req_headers),
        path: path.to_owned(),
        ua: ua_of(req_headers),
        status: up.status().as_u16() as i64,
        quota,
        started,
        _in_flight: in_flight,
    };

    let stream = up.bytes_stream().map(move |chunk| {
        // guard 被 move 进闭包：闭包（连同 guard）随流一起析构，客户端提前断开也照样落库。
        let _keep = &guard;
        if let Ok(bytes) = &chunk {
            sniffer.lock().feed(bytes);
        }
        chunk
    });

    builder.body(Body::from_stream(stream)).unwrap_or_else(|e| internal_error_plain(&e.to_string()))
}

/// 拼上游 URL：`UPSTREAM_BASE` + 来访路径（+ 原样带上 query）。
/// 规范化后的请求体。
struct Normalized {
    /// 发给上游的体。非 `responses` 路径、或解不动的体，原样带回。
    body: Bytes,
    /// 客户端要的是一次性 JSON（体里的 `stream` 不是 `true`）。上游只出 SSE，所以这种
    /// 请求要在本层把流收拢回一个 JSON 体（见 [`collapse_upstream`]）。
    collapse: bool,
}

/// 把 `responses` 请求体钉成上游要的样子：`store: false`、`stream: true`。
///
/// 上游对这两项都是硬约束，且各自的 400 长得一模一样地不讲道理：
/// - `store` 漏传或传 `true` → `Store must be set to false`（会话不落在 ChatGPT 侧）；
/// - `stream` 漏传或传 `false` → `Stream must be set to true`（这条路径只出 SSE）。
///
/// codex CLI 两项都带对了，但照 OpenAI 官方 Responses API 写的客户端不会——那边 `store`
/// 默认 `true`、`stream` 默认 `false`，两条默认值正好都踩在雷上。改写只此一处：这是上游的
/// 硬约束而不是用户的选择，让每个接入方各自去踩一遍没有意义。
///
/// 钉 `stream` 与钉 `store` 有个区别：`store` 改了客户端察觉不到，而 `stream` 改了会把
/// 一个 JSON 响应变成 SSE。所以这里同时记下「客户端本来没要流」，由 [`collapse_upstream`]
/// 把流收回成 JSON——只改体不管回程的话，客户端拿到的是一堆读不懂的 `data:` 行。
///
/// 解不动的体（非 JSON、非对象）原样放过：判 400 是上游的事，这里不替它拦。
fn normalize_responses_body(path: &str, body: Bytes) -> Normalized {
    if path.trim_start_matches('/') != config::RESPONSES_PATH {
        return Normalized { body, collapse: false };
    }
    let Ok(serde_json::Value::Object(mut obj)) = serde_json::from_slice(&body) else {
        return Normalized { body, collapse: false };
    };
    let yes = Some(&serde_json::Value::Bool(true));
    let no = Some(&serde_json::Value::Bool(false));
    let collapse = obj.get("stream") != yes;
    if !collapse && obj.get("store") == no {
        // 两项已经都对：不重新序列化（也就不会顺手改掉字段顺序）。
        return Normalized { body, collapse };
    }
    obj.insert("store".to_owned(), serde_json::Value::Bool(false));
    obj.insert("stream".to_owned(), serde_json::Value::Bool(true));
    match serde_json::to_vec(&serde_json::Value::Object(obj)) {
        Ok(v) => Normalized { body: Bytes::from(v), collapse },
        // 序列化一个刚解出来的 JSON 不会失败，真失败了也宁可发原体而不是空体。
        Err(_) => Normalized { body, collapse },
    }
}

fn upstream_url(path: &str, query: Option<&str>) -> String {
    let path = path.trim_start_matches('/');
    match query.filter(|q| !q.is_empty()) {
        Some(q) => format!("{}/{}?{}", config::UPSTREAM_BASE, path, q),
        None => format!("{}/{}", config::UPSTREAM_BASE, path),
    }
}

/// 构造发往上游的头：来访头去掉逐跳/鉴权项后照抄，再补上这个凭证的身份。
fn build_forward_headers(
    incoming: &HeaderMap,
    cred: &Credential,
    token: &str,
) -> wreq::header::HeaderMap {
    let mut out = wreq::header::HeaderMap::new();
    for (name, value) in incoming.iter() {
        if config::HOP_BY_HOP_HEADERS.contains(&name.as_str()) {
            continue;
        }
        out.insert(name.clone(), value.clone());
    }

    let set = |out: &mut wreq::header::HeaderMap, name: &'static str, v: &str| {
        if let Ok(value) = HeaderValue::from_str(v) {
            out.insert(HeaderName::from_static(name), value);
        }
    };
    set(&mut out, "authorization", &format!("Bearer {token}"));
    // 这个头与 access_token 是一对：上游认的是两件一起，缺任何一半都是 401。
    set(&mut out, "chatgpt-account-id", &cred.account_id);
    set(&mut out, "originator", config::ORIGINATOR);
    // 会话 id 按账号 + 来访会话派生，见 Credential::session_id 的注。
    let fingerprint = incoming_session_id(incoming).unwrap_or_default();
    set(&mut out, "session_id", &cred.session_id(&fingerprint));
    // 解压 feature 开着，声明什么就可能收到什么压缩形态；这一项要与官方客户端一致。
    set(&mut out, "accept-encoding", config::ACCEPT_ENCODING);
    if !out.contains_key(header::USER_AGENT.as_str()) {
        set(&mut out, "user-agent", config::CODEX_USER_AGENT.as_str());
    }
    out
}

/// 取来访请求自报的会话 id。codex CLI 用 `session_id` 头，也有客户端写成 `x-session-id`。
fn incoming_session_id(headers: &HeaderMap) -> Option<String> {
    ["session_id", "x-session-id", "conversation_id"]
        .iter()
        .find_map(|n| headers.get(*n))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// 来访 UA（截断后入库）。
fn ua_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.chars().take(UA_MAX_LEN).collect())
}

// ---------- 响应构造 ----------

/// 按上游响应构造回给客户端的响应头。
///
/// **`content-length` 与 `content-encoding` 都不能照抄**：上游那个长度是压缩后的字节数，
/// 而我们这一层已经解压过了（wreq 的解压 feature），照抄会让客户端按一个错的长度截断，
/// 表现为 SSE 流莫名其妙断在中间。
fn resp_builder(up: &wreq::Response) -> axum::http::response::Builder {
    let mut builder = Response::builder().status(up.status().as_u16());
    for (name, value) in up.headers().iter() {
        if matches!(
            name.as_str(),
            "content-length" | "content-encoding" | "transfer-encoding" | "connection"
        ) {
            continue;
        }
        builder = builder.header(name.as_str(), value.as_bytes());
    }
    builder
}

/// 把上游的非流式响应原样交回（同样跳过长度/编码头）。
fn passthrough(status: StatusCode, headers: &wreq::header::HeaderMap, body: Bytes) -> Response {
    let mut builder = Response::builder().status(status);
    for (name, value) in headers.iter() {
        if matches!(
            name.as_str(),
            "content-length" | "content-encoding" | "transfer-encoding" | "connection"
        ) {
            continue;
        }
        builder = builder.header(name.as_str(), value.as_bytes());
    }
    builder.body(Body::from(body)).unwrap_or_else(|e| internal_error_plain(&e.to_string()))
}

/// 造一个 OpenAI 形态的错误响应体。
///
/// **形态必须与上游一致**：codex CLI 按 `error.message` 取文案，回一个别的形状会让它
/// 显示成一句空错误，用户看到的是「请求失败」四个字加一片空白。
fn error_body(etype: &str, message: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "error": { "message": message, "type": etype, "code": etype }
    }))
    .unwrap_or_else(|_| b"{\"error\":{\"message\":\"internal error\"}}".to_vec())
}

fn error_response(status: StatusCode, etype: &str, message: impl AsRef<str>) -> Response {
    let body = error_body(etype, message.as_ref());
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|e| internal_error_plain(&e.to_string()))
}

fn rate_limit_response(retry_after_secs: i64, message: &str) -> Response {
    let body = error_body("rate_limit_exceeded", message);
    Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::RETRY_AFTER, retry_after_secs.max(1).to_string())
        .body(Body::from(body))
        .unwrap_or_else(|e| internal_error_plain(&e.to_string()))
}

fn internal_error(e: &anyhow::Error) -> Response {
    tracing::error!(error = %format!("{e:#}"), "proxy internal error");
    error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", format!("{e:#}"))
}

/// 连 `Response::builder()` 都失败时的最后兜底（几乎不可能发生，但 unwrap 会让整个连接挂掉）。
fn internal_error_plain(msg: &str) -> Response {
    tracing::error!(error = msg, "failed to build response");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
}

/// 把选号失败翻成对客户端有意义的响应。
fn select_error_response(e: &anyhow::Error) -> Response {
    if let Some(rl) = e.downcast_ref::<store::AllRateLimited>() {
        return rate_limit_response(rl.retry_after_secs, &rl.to_string());
    }
    error_response(StatusCode::SERVICE_UNAVAILABLE, "no_credential_available", format!("{e:#}"))
}

// ---------- 鉴权 ----------

/// 生效的接入 key：命令行/环境变量优先，其次库里存的；都没有则不校验来访身份。
fn effective_client_key(state: &AppState) -> Option<String> {
    if let Some(k) = &state.client_key {
        return Some(k.as_str().to_owned());
    }
    state.store.get_setting(store::CLIENT_API_KEY).ok().flatten().filter(|s| !s.is_empty())
}

/// 校验来访身份。
///
/// 三种写法都收：`Authorization: Bearer <key>`、裸的 `Authorization: <key>`、
/// 以及 `api-key`/`x-api-key` 头——不同的接入方（codex CLI、各种 SDK、curl 手测）
/// 习惯不同，只认一种的话报错都是「无效 key」，而用户看着自己明明填对了。
fn client_authorized(headers: &HeaderMap, expected: &str) -> bool {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.strip_prefix("Bearer ").unwrap_or(v))
        .or_else(|| headers.get("api-key").and_then(|v| v.to_str().ok()))
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()));
    presented.map(|p| constant_time_eq(p.trim(), expected)) == Some(true)
}

/// 定长比较。短路比较会让「猜对了前几个字符」在耗时上体现出来。
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---------- 上游错误判定 ----------

/// 账号级错误的判据。命中即认为这个号本身不可用（不是这条请求的问题）。
///
/// 只在 401/403 上判，且要求文本里同时出现「主体」与「状态」两类词——单看
/// `unauthorized` 会把「这条请求的 token 刚好过期」也算成封号，把一个好账号误关。
const BAN_STATES: &[&str] =
    &["suspend", "ban", "deactivat", "disabled", "terminated", "revoked", "not active"];
const BAN_SUBJECTS: &[&str] = &["account", "organization", "workspace", "subscription"];

/// 判断这次非 2xx 是不是账号级问题，是则返回原因（入库到 `ban_reason`）。
fn detect_account_error(status: StatusCode, body: &[u8]) -> Option<String> {
    if !matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return None;
    }
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    let hit_state = BAN_STATES.iter().any(|k| text.contains(k));
    let hit_subject = BAN_SUBJECTS.iter().any(|k| text.contains(k));
    (hit_state && hit_subject).then(|| {
        let msg = serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v.pointer("/error/message").and_then(|m| m.as_str()).map(str::to_owned))
            .unwrap_or_else(|| String::from_utf8_lossy(body).chars().take(200).collect());
        format!("upstream {status}: {msg}")
    })
}

/// 从 `retry-after` 头取秒数。
fn retry_after_secs(headers: &wreq::header::HeaderMap) -> Option<i64> {
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok())
        .map(|v| v.ceil() as i64)
        .filter(|v| *v > 0)
}

/// 额度用过阈值就把这个号暂停到窗口重置，**真的停了才返回 `true`**。
///
/// 阈值默认 90%：留出余量，免得一条长请求跑到一半正好把额度撞穿——那时上游是直接掐断
/// 流，客户端拿到的是半截响应。设成 0 表示不暂停。
///
/// 返回值给 [`probe`] 用：它成功时要把限流暂停的号放回池子，而刚被这里按阈值停掉的号
/// 不该被同一次探活立刻放回去——否则下一条真实请求撞上额度墙，再停一次。
fn maybe_pause_on_quota(state: &AppState, cred: &Credential, quota: &QuotaSnapshot) -> bool {
    let pct = state.store.get_setting_i64(store::QUOTA_PAUSE_PCT, store::DEFAULT_QUOTA_PAUSE_PCT);
    if pct <= 0 {
        return false;
    }
    let Some(used) = quota.peak_used_pct() else { return false };
    if used < pct as f64 {
        return false;
    }
    // 暂停到窗口重置为止；解析不出重置时刻就退回一个保守的固定值。
    let secs = quota.secs_until_reset().unwrap_or(15 * 60);
    tracing::warn!(
        cred_id = cred.id,
        used_pct = used,
        pause_secs = secs,
        "quota threshold reached, pausing credential"
    );
    match state.store.pause_for_rate_limit(cred.id, secs) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(cred_id = cred.id, error = %e, "failed to pause the credential");
            false
        }
    }
}

// ---------- 用量嗅探 ----------

/// 从 SSE 流里刮出用量。
///
/// **按行喂而不是攒整个响应**：一次长回复的 SSE 有几 MB，全攒下来只为读末尾那一个
/// `response.completed` 事件，等于给每条并发请求都挂一份响应体在内存里。
#[derive(Default)]
pub struct UsageSniffer {
    /// 上一块结尾那半行（chunk 边界不保证落在换行上）。
    pending: String,
    /// 最后一次看到的用量。取最后一个而不是第一个：`response.completed` 才是终值，
    /// 中途的 `response.in_progress` 也可能带 usage，但那是不完整的读数。
    pub usage: Option<Usage>,
    pub model: Option<String>,
    /// 首字节时刻（相对转发开始）。
    pub first_byte: Option<Instant>,
}

/// 一次响应报告的 token 用量。
#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub input_tokens: i64,
    pub cached_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    pub total_tokens: i64,
}

/// 单行 SSE `data:` 的长度上限。超过就丢弃这一行——正常的事件行是几 KB 级别，
/// 一个不带换行的巨大响应体会把 `pending` 撑成无界缓冲。
const MAX_SSE_LINE: usize = 1024 * 1024;

impl UsageSniffer {
    /// 喂一块响应字节。
    pub fn feed(&mut self, chunk: &Bytes) {
        if self.first_byte.is_none() {
            self.first_byte = Some(Instant::now());
        }
        // SSE 是 UTF-8；多字节字符被切在 chunk 边界时 lossy 会产生替换字符，但那只会
        // 出现在正文里，不影响我们要找的 ASCII 结构。
        self.pending.push_str(&String::from_utf8_lossy(chunk));
        while let Some(idx) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=idx).collect();
            self.consume_line(line.trim_end());
        }
        if self.pending.len() > MAX_SSE_LINE {
            self.pending.clear();
        }
    }

    fn consume_line(&mut self, line: &str) {
        let Some(data) = line.strip_prefix("data:") else { return };
        let data = data.trim();
        // 先用一次廉价的子串判断挡掉绝大多数事件行（增量文本），只有可能带 usage 的
        // 才付一次 JSON 解析——解析每一行的话，一次长回复要解析几千次。
        if !data.contains("\"usage\"") && !data.contains("\"model\"") {
            return;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else { return };
        // 事件体形如 {"type":"response.completed","response":{...}}；也兼容裸的响应对象。
        let resp = v.get("response").unwrap_or(&v);
        if let Some(m) = resp.get("model").and_then(|m| m.as_str()) {
            self.model = Some(m.to_owned());
        }
        if let Some(u) = resp.get("usage").filter(|u| u.is_object()) {
            let n = |p: &str| u.pointer(p).and_then(|x| x.as_i64()).unwrap_or(0);
            self.usage = Some(Usage {
                input_tokens: n("/input_tokens"),
                cached_tokens: n("/input_tokens_details/cached_tokens"),
                output_tokens: n("/output_tokens"),
                reasoning_tokens: n("/output_tokens_details/reasoning_tokens"),
                total_tokens: n("/total_tokens"),
            });
        }
    }
}

/// 流结束（或客户端断开）时把这次转发落库。
struct UsageLogGuard {
    store: Arc<CredentialStore>,
    sniffer: Arc<parking_lot::Mutex<UsageSniffer>>,
    cred_id: i64,
    cred_label: String,
    session_id: Option<String>,
    path: String,
    ua: Option<String>,
    status: i64,
    quota: QuotaSnapshot,
    started: Instant,
    /// 只为让在飞计数活到流的末尾。
    _in_flight: InFlightGuard,
}

impl Drop for UsageLogGuard {
    fn drop(&mut self) {
        let s = self.sniffer.lock();
        let usage = s.usage;
        let model = s.model.clone();
        let ttft = s.first_byte.map(|t| t.duration_since(self.started).as_millis() as i64);
        drop(s);

        let cost = match (&model, usage) {
            (Some(m), Some(u)) => {
                crate::pricing::estimate_usd(m, u.input_tokens, u.cached_tokens, u.output_tokens)
            }
            _ => None,
        };
        let rec = UsageRecord {
            cred_id: Some(self.cred_id),
            cred_label: std::mem::take(&mut self.cred_label),
            session_id: self.session_id.take(),
            model,
            path: std::mem::take(&mut self.path),
            ua: self.ua.take(),
            status: self.status,
            has_usage: usage.is_some(),
            input_tokens: usage.map(|u| u.input_tokens),
            cached_tokens: usage.map(|u| u.cached_tokens),
            output_tokens: usage.map(|u| u.output_tokens),
            reasoning_tokens: usage.map(|u| u.reasoning_tokens),
            total_tokens: usage.map(|u| u.total_tokens),
            ttft_ms: ttft,
            total_ms: Some(self.started.elapsed().as_millis() as i64),
            cost_usd: cost,
            quota: Some(std::mem::take(&mut self.quota)),
        };
        spawn_usage_log(self.store.clone(), rec);
    }
}

/// 非流式路径（错误响应）的落库。
#[allow(clippy::too_many_arguments)]
fn log_usage(
    state: &AppState,
    cred: &Credential,
    path: &str,
    req_headers: &HeaderMap,
    status: i64,
    model: Option<String>,
    quota: &QuotaSnapshot,
    started: Instant,
    usage: Option<Usage>,
) {
    let rec = UsageRecord {
        cred_id: Some(cred.id),
        cred_label: cred.label.clone(),
        session_id: incoming_session_id(req_headers),
        model,
        path: path.to_owned(),
        ua: ua_of(req_headers),
        status,
        has_usage: usage.is_some(),
        input_tokens: usage.map(|u| u.input_tokens),
        cached_tokens: usage.map(|u| u.cached_tokens),
        output_tokens: usage.map(|u| u.output_tokens),
        reasoning_tokens: usage.map(|u| u.reasoning_tokens),
        total_tokens: usage.map(|u| u.total_tokens),
        ttft_ms: None,
        total_ms: Some(started.elapsed().as_millis() as i64),
        cost_usd: None,
        quota: Some(quota.clone()),
    };
    spawn_usage_log(state.store.clone(), rec);
}

/// 落库放到阻塞线程池：SQLite 的写事务会拿住那把全局 `conn` 锁，直接在异步线程上跑
/// 会把整个 runtime 的一个 worker 堵住。
fn spawn_usage_log(store: Arc<CredentialStore>, rec: UsageRecord) {
    tokio::task::spawn_blocking(move || {
        if let Err(e) = store.insert_usage_log(&rec) {
            tracing::warn!(error = %e, "failed to record usage");
        }
    });
}

// ---------- 在飞计数 ----------

/// 在途请求计数器：构造时 +1，析构时 -1。
///
/// **必须活到响应流走完**，不能在 handler 返回时就减——流式回复要几十秒才走完，
/// 只在返回时减一会把这类请求算成「瞬间就结束了」，并发数永远显示成 0～1。
pub struct InFlightGuard(Arc<std::sync::atomic::AtomicI64>);

impl InFlightGuard {
    pub fn new(counter: Arc<std::sync::atomic::AtomicI64>) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(counter)
    }
}

impl Clone for InFlightGuard {
    /// 克隆时**也要 +1**：默认派生的 Clone 会复制那个 Arc 却不加计数，于是每个副本析构
    /// 都减一次，计数一路减到负数。
    fn clone(&self) -> Self {
        Self::new(self.0.clone())
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

// ---------- 模型清单 ----------

/// 取模型清单最多等多久。清单是个几 KB 的 JSON，正常几百毫秒；这里是人打开弹窗时在等，
/// 超时就退回内置兜底清单（见前端），比让下拉框一直转圈有用。
const MODEL_LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// 上游模型清单里的一项（只留界面要用的那几个字段）。
///
/// 上游还回 `instructions_template`（每个模型几 KB 的基座提示）、reasoning 档位等一大堆
/// 东西，**刻意不透传**：那会把一个几 KB 的响应变成几百 KB，而下拉框只需要名字。
#[derive(serde::Serialize, serde::Deserialize)]
pub struct UpstreamModel {
    /// 传给上游的模型名（`model` 字段填的就是它）。
    pub slug: String,
    /// 给人看的名字（`GPT-5.6-Sol`）。
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// `list` = codex 自己的模型选择器里会列出来；`hide` = 内部项或别名。
    ///
    /// **`hide` 的照样能用**——实测 `gpt-reserve`/`codex-auto-review` 都回 200，只是会被
    /// 上游解析成别的模型（都变成 `gpt-5.6-luna`）。所以这一项由前端决定要不要显示，
    /// 后端如实转出来，不在这里替上游做删减。
    #[serde(default)]
    pub visibility: Option<String>,
    /// 上游标的「能不能走 API」。
    ///
    /// **不可当作过滤条件**：实测 `gpt-5.3-codex-spark` 标着 `false`，走
    /// `/responses` 却照样 200。它的真实含义不明，如实转出来供参考即可。
    #[serde(default)]
    pub supported_in_api: Option<bool>,
    /// 上游给的排序权重，小者靠前（codex 的选择器就按它排）。
    #[serde(default)]
    pub priority: Option<i64>,
}

#[derive(serde::Deserialize)]
struct ModelListResponse {
    #[serde(default)]
    models: Vec<UpstreamModel>,
}

/// 用**指定**凭证向上游取当前可用的模型清单。
///
/// **不写死清单**：模型随上游上新/下线变化，写死的那一刻就开始过期，表现是连通性测试的
/// 下拉里缺新模型、留着已下线的。这条路径与转发共用同一条出站链路（含逐账号代理）与同一份
/// 身份头，所以取到的就是「这个号看得到的那些」。
///
/// 不消耗额度（是个 GET，不产生 token），也不写用量流水。
///
/// **`client_version` 决定上游返回什么**：报 [`config::CODEX_VERSION`]。实测报 `0.98.0`
/// 只回 3 个模型、报 `0.148.0` 回 9 个——上游按客户端版本裁剪，所以那个常量落后的表现是
/// 「下拉里少一大半」，不只是指纹旧。
pub async fn list_models(
    state: &AppState,
    cred: &Credential,
) -> anyhow::Result<Vec<UpstreamModel>> {
    use anyhow::Context;

    let client = state.clients.for_credential(cred)?;
    // 这里**不给取 token 套 timeout**：刷新会轮换 refresh_token，中途取消就把号废了
    // （详见 probe_token 的注）。清单请求本身有超时，而刷新最多也就一次往返。
    let token = state.store.valid_access_token(&state.clients, cred).await?;
    let url = format!(
        "{}?client_version={}",
        upstream_url(config::MODELS_PATH, None),
        config::CODEX_VERSION
    );
    let sent = client
        .request(wreq::Method::GET, url)
        .headers(synthetic_headers(cred, &token, "application/json"))
        .send();
    let up = tokio::time::timeout(MODEL_LIST_TIMEOUT, sent)
        .await
        .with_context(|| {
            format!("fetching the model list timed out (cap {}s)", MODEL_LIST_TIMEOUT.as_secs())
        })?
        // 传输层错误按种类标出来（前端认这个形状并本地化），与探测那条路同一套文案。
        .map_err(|e| {
            anyhow::anyhow!("upstream request failed [{}]: {e}", upstream_error_kind(&e))
        })?;

    let status = up.status();
    let bytes = tokio::time::timeout(MODEL_LIST_TIMEOUT, up.bytes())
        .await
        .context("reading the model list response timed out")?
        .context("failed to read the upstream response body")?;
    anyhow::ensure!(
        status.is_success(),
        "upstream returned {}: {}",
        status.as_u16(),
        parse_upstream_error(&bytes).1.chars().take(PROBE_ERROR_MAX_LEN).collect::<String>()
    );

    let parsed: ModelListResponse =
        serde_json::from_slice(&bytes).context("failed to parse the model list response")?;
    // 按上游给的权重排，缺权重的垫后——那是它自己选择器里的顺序，照抄比我们另定一套好。
    let mut models = parsed.models;
    models.sort_by_key(|m| m.priority.unwrap_or(i64::MAX));
    tracing::info!(
        cred_id = cred.id, label = %cred.label,
        count = models.len(),
        client_version = config::CODEX_VERSION,
        "fetched the upstream model list"
    );
    Ok(models)
}

// ---------- 连通性测试 ----------

/// 一次连通性测试最多等多久。
///
/// 上游客户端本身不设超时（流式转发可以跑很久），但测试是人在网页上等着的：它只发一条
/// `hi`，正常几百毫秒就该回来，超过这个数就是上游不通或被中间设备吞了——报出来比让页面
/// 一直转圈有用。这个上限覆盖**取/刷 token + 发请求 + 读完响应体**整条链路。
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 探测打到的上游路径（`UPSTREAM_BASE` 之后那一段）。与真实转发落到同一个端点。
const PROBE_PATH: &str = config::RESPONSES_PATH;

/// 用量流水里标出「这条是连通性测试」的 UA。
///
/// 借 `ua` 那一列而不新开一列：它本来就是「这条流量是谁发的」，而探测没有来访客户端，
/// 那一列本来就空着——写上 coban 自己反而比留空更准。
const PROBE_UA: &str = "coban/connectivity-test";

/// 探测请求体里的 `instructions`。
///
/// Responses API 要求这个字段**存在**（缺了直接 400）。刻意只放一句：官方那份 Codex 基座
/// 提示有几千 token，每点一次测试就烧一遍，而它对「这个号能不能用这个模型」这个问题一点
/// 信息都不增加。
///
/// **万一上游哪天开始校验内容**（表现为每个模型都稳定回同一条 400，而真实转发一切正常），
/// 改的就是这一个常量——换成官方客户端那份基座提示即可，代价只是每次测试多烧几千个
/// cached token。
const PROBE_INSTRUCTIONS: &str = "Reply with a single word.";

/// 错误原文进 JSON 前的截断长度。上游偶尔糊一整张 HTML 拦截页，看清病因不需要那么多。
const PROBE_ERROR_MAX_LEN: usize = 500;

/// 一次连通性测试的结果（[`crate::web`] 原样 JSON 回给前端）。
#[derive(serde::Serialize)]
pub struct ProbeReport {
    /// 上游是否 2xx **且**流里没有失败事件（见 [`sse_failure`]）。
    pub ok: bool,
    /// 上游 HTTP 状态码；**`0` 表示请求根本没到上游**（取 token 失败、连不上、超时），
    /// 此时原因在 `error` 里。
    pub status: u16,
    /// 从「开始发」到「响应体读完」的耗时（毫秒）。请求没发出去时是失败前的耗时。
    pub latency_ms: u128,
    /// 上游实际回报的模型名（解析到才有）。可能与请求的不同——别名会在上游解析成具体
    /// 版本，这正是「这个模型名到底指向什么」的答案。
    pub model: Option<String>,
    /// 上游错误类型（`error.type`，如 `usage_limit_reached`/`invalid_request_error`）。
    pub error_type: Option<String>,
    /// 失败原因原文（上游 `error.message`，解析不出就是整段响应体 / coban 侧的错误链）。
    pub error: Option<String>,
    /// 本次响应的额度快照，与卡片上那份同一套读法；响应没带这组头时为 `None`。
    pub quota: Option<QuotaSnapshot>,
    /// 上游 `retry-after`（秒）。只有 429 才有，且它是**这次拒绝**给出的等待时间，
    /// 比额度窗口的重置时刻更直接。
    pub retry_after_secs: Option<i64>,
}

impl ProbeReport {
    /// 请求没到上游（或没读到响应）时的结果：状态码留 0，原因写进 `error`。
    fn failed(latency_ms: u128, error: String) -> Self {
        Self {
            ok: false,
            status: 0,
            latency_ms,
            model: None,
            error_type: None,
            error: Some(error),
            quota: None,
            retry_after_secs: None,
        }
    }
}

/// 用**指定**凭证向上游发一条最小请求，测这个账号能不能用这个模型。
///
/// 与转发路径的两处刻意不同：
///
/// 1. **不选号**：直接用传进来的那一个凭证，也因此不占 RPM 名额、不参与换号重试——
///    测的就是这一个，换了号结论就不是它的了。停用/封禁的号照样能测，「它是不是已经
///    恢复了」正是要问的问题，所以 [`crate::web`] 那头只校验凭证存在。
/// 2. **请求由 coban 自己构造**（见 [`probe_body`]），不是转发来的客户端流量。形态照
///    官方客户端（同一份 [`build_forward_headers`]、同一条出站链路含逐账号代理），
///    但内容恒定，好让两次测试之间只有「账号 + 模型」在变。
///
/// 而**账号状态照真实流量的口径更新**：这条请求是真实的——真花额度、拿到的也是上游此刻
/// 的真实判决。于是 429 照样打冷却、命中封号特征照样 [`CredentialStore::mark_banned`]、
/// 额度过阈值照样暂停（[`maybe_pause_on_quota`]）；反过来，测试通过就把因限流暂停的号
/// 放回池子（[`CredentialStore::resume_if_rate_limited`]）并清掉冷却——上游此刻既然放行，
/// 不必干等到点（上游给的等待时间偏保守时，好号会被白白晾着）。否则弹窗报「已封禁」而
/// 卡片上一切如常，两边各说各话，用户还得自己动手把号停掉。
///
/// 它也**照常写一条用量流水**（[`log_probe_usage`]）：卡片上的额度快照与累计花费都出自
/// `usage_logs`，不写就等于「测出来的额度只活在弹窗里」，而这条请求确实拿到了此刻最新的
/// 限流头、也确实花了钱。那条以 [`PROBE_UA`] 标出，与真实流量可区分。
///
/// 代价是它**真的会消耗一点订阅额度**：一句 `hi` 加一句 [`PROBE_INSTRUCTIONS`]，
/// 回复几十个 token。
pub async fn probe(state: &AppState, cred: &Credential, model: &str) -> ProbeReport {
    let started = Instant::now();
    // 一个 deadline 覆盖取/刷 token、发送请求和读完响应体。只给 send() 套 timeout 不够：
    // 上游若只回响应头却不结束 body，或 token 刷新卡住，前端的按钮会永远转圈。
    let deadline = tokio::time::Instant::now() + PROBE_TIMEOUT;

    let token = match probe_token(state, cred, model, started, deadline).await {
        Ok(t) => t,
        Err(report) => return report,
    };

    let client = match state.clients.for_credential(cred) {
        Ok(c) => c,
        // 配了代理却建不出客户端：这个号整体不可用，绝不退回直连去测（见 ClientPool）。
        Err(e) => return ProbeReport::failed(started.elapsed().as_millis(), format!("{e:#}")),
    };
    let sent = client
        .request(wreq::Method::POST, upstream_url(PROBE_PATH, None))
        .headers(probe_headers(cred, &token))
        .body(probe_body(model))
        .send();

    let report = match tokio::time::timeout_at(deadline, sent).await {
        Err(_) => ProbeReport::failed(
            started.elapsed().as_millis(),
            format!(
                "connectivity test timed out (overall cap {}s): still waiting on the upstream response",
                PROBE_TIMEOUT.as_secs()
            ),
        ),
        Ok(Err(e)) => ProbeReport::failed(
            started.elapsed().as_millis(),
            format!("upstream request failed [{}]: {e}", upstream_error_kind(&e)),
        ),
        Ok(Ok(up)) => {
            let status =
                StatusCode::from_u16(up.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            // 限流头必须在 `bytes()` 之前读——它会把整个响应消费掉，之后就没有头可看了。
            // 200 与 429 都带这组头，后者尤其有用：能直接看出是哪个窗口满了、要等多久。
            let quota = QuotaSnapshot::from_headers(up.headers());
            let retry_after = retry_after_secs(up.headers());

            if status == StatusCode::TOO_MANY_REQUESTS {
                let secs = retry_after.unwrap_or_else(|| {
                    state.store.get_setting_i64(store::COOLDOWN_SECS, store::DEFAULT_COOLDOWN_SECS)
                });
                tracing::warn!(
                    cred_id = cred.id, label = %cred.label, model, cooldown_secs = secs,
                    "connectivity test hit an upstream 429, cooling this credential down"
                );
                state.store.note_rate_limited(cred.id, secs);
            }
            // 阈值机制先过一道：它把号停下时整条恢复分支都不走，否则一次手动探活会把刚按
            // 阈值停掉的号放回池子，下一条真实请求撞上额度墙再停一次。
            let parked = maybe_pause_on_quota(state, cred, &quota);
            if status.is_success() && !parked {
                probe_resume(state, cred, model);
            }

            match tokio::time::timeout_at(deadline, up.bytes()).await {
                // 已拿到真实状态码与限流头，只是 body 没有结束；保留这些信息并照样落一条
                // 流水——额度快照来自头，不依赖 body。
                Err(_) => {
                    log_probe_usage(state, cred, status, &UsageSniffer::default(), &quota, started);
                    ProbeReport {
                        ok: false,
                        status: status.as_u16(),
                        latency_ms: started.elapsed().as_millis(),
                        model: None,
                        error_type: None,
                        error: Some(format!(
                            "reading the upstream response body timed out (overall cap {}s)",
                            PROBE_TIMEOUT.as_secs()
                        )),
                        quota: (!quota.is_empty()).then_some(quota),
                        retry_after_secs: retry_after,
                    }
                }
                // 响应体读到一半断了：状态码与限流头都是真的，只是内容不完整，如实报出来。
                Ok(Err(e)) => {
                    log_probe_usage(state, cred, status, &UsageSniffer::default(), &quota, started);
                    ProbeReport {
                        ok: false,
                        status: status.as_u16(),
                        latency_ms: started.elapsed().as_millis(),
                        model: None,
                        error_type: None,
                        error: Some(format!("failed to read the upstream response body: {e}")),
                        quota: (!quota.is_empty()).then_some(quota),
                        retry_after_secs: retry_after,
                    }
                }
                Ok(Ok(bytes)) => {
                    // 整段 body 只喂一次嗅探器：落库与结果构造要的是同一份读数，
                    // 各解析一遍既白花 CPU，也可能因为两处写法漂移而给出两个模型名。
                    let mut sniffer = UsageSniffer::default();
                    sniffer.feed(&bytes);
                    log_probe_usage(state, cred, status, &sniffer, &quota, started);
                    // 命中封号特征照真实流量停用：判定器与转发共用同一个，测试报出「已封禁」
                    // 的同时列表里也变红，而不是弹窗一个结论、卡片另一个。
                    if let Some(reason) = detect_account_error(status, &bytes) {
                        tracing::warn!(
                            cred_id = cred.id, label = %cred.label,
                            status = status.as_u16(), reason = %reason,
                            "connectivity test detected an account-level error, disabling the credential"
                        );
                        if let Err(e) = state.store.mark_banned(cred.id, &reason) {
                            tracing::warn!(error = %e, "failed to disable the credential");
                        }
                    }
                    probe_report(
                        status,
                        &bytes,
                        &sniffer,
                        started.elapsed().as_millis(),
                        quota,
                        retry_after,
                    )
                }
            }
        }
    };

    tracing::info!(
        cred_id = cred.id, label = %cred.label, model,
        ok = report.ok,
        status = report.status,
        latency_ms = report.latency_ms,
        upstream_model = %report.model.as_deref().unwrap_or("-"),
        error = %report.error.as_deref().unwrap_or("-"),
        "connectivity test"
    );
    report
}

/// 取这个凭证当前可用的 access_token，必要时先刷新。失败时直接返回给调用方的那份结果。
///
/// **刷新不能直接套 `timeout`**：它会轮换 refresh_token，上游若已经换过、本地却在写库前
/// 被取消，旧 token 当场作废且新 token 永久丢失——一个好账号就这么废了。故放进独立任务：
/// `JoinHandle` 即使因等待超时被丢弃，任务仍会跑完并落库，页面只是不再等它。
async fn probe_token(
    state: &AppState,
    cred: &Credential,
    model: &str,
    started: Instant,
    deadline: tokio::time::Instant,
) -> Result<String, ProbeReport> {
    if !cred.needs_refresh() {
        return Ok(cred.access_token.clone());
    }
    let store = state.store.clone();
    let clients = state.clients.clone();
    let owned = cred.clone();
    let task = tokio::spawn(async move { store.valid_access_token(&clients, &owned).await });
    match tokio::time::timeout_at(deadline, task).await {
        Ok(Ok(Ok(token))) => Ok(token),
        Ok(Ok(Err(e))) => {
            tracing::warn!(
                cred_id = cred.id, label = %cred.label, model, error = %format!("{e:#}"),
                "connectivity test: getting an access_token failed"
            );
            Err(ProbeReport::failed(
                started.elapsed().as_millis(),
                format!("failed to get a token: {e:#}"),
            ))
        }
        Ok(Err(e)) => {
            tracing::error!(
                cred_id = cred.id, label = %cred.label, model, error = %e,
                "connectivity test: the token refresh task died"
            );
            Err(ProbeReport::failed(
                started.elapsed().as_millis(),
                format!("the token refresh task died: {e}"),
            ))
        }
        Err(_) => {
            tracing::warn!(
                cred_id = cred.id, label = %cred.label, model,
                timeout_secs = PROBE_TIMEOUT.as_secs(),
                "connectivity test: getting an access_token timed out; the refresh continues in the background"
            );
            Err(ProbeReport::failed(
                started.elapsed().as_millis(),
                format!(
                    "connectivity test timed out (overall cap {}s): the token refresh continues in the background",
                    PROBE_TIMEOUT.as_secs()
                ),
            ))
        }
    }
}

/// 测试通过后把这个号放回轮转：限流暂停解除 + 冷却清掉。
///
/// 只动限流那两档，人工停用与封号不碰（见 [`CredentialStore::resume_if_rate_limited`]）。
fn probe_resume(state: &AppState, cred: &Credential, model: &str) {
    match state.store.resume_if_rate_limited(cred.id) {
        Ok(true) => tracing::info!(
            cred_id = cred.id, label = %cred.label, model,
            "connectivity test passed, credential is back in the pool"
        ),
        Ok(false) => {}
        Err(e) => tracing::error!(
            cred_id = cred.id, label = %cred.label, error = %e,
            "connectivity test passed but persisting the resume failed"
        ),
    }
    state.store.clear_cooldown(cred.id);
}

/// coban **自己合成**的上游请求（探测、取模型清单）的公共头。
///
/// 鉴权、账号身份、`session_id`、UA 全走转发路径那份 [`build_forward_headers`]——两处各写
/// 一份的话，改了转发形态而漏改这里，测出来的就不是真实转发会走的那条路。`accept` 由调用方
/// 给：转发时它由来访客户端提供，合成请求得自己报，且两条路要的类型不同。
fn synthetic_headers(
    cred: &Credential,
    token: &str,
    accept: &'static str,
) -> wreq::header::HeaderMap {
    let mut headers = build_forward_headers(&HeaderMap::new(), cred, token);
    headers.insert(header::ACCEPT, HeaderValue::from_static(accept));
    headers
}

/// 探测请求（POST）的出站头：公共头 + `content-type`。
///
/// `stream: true` 的响应是 SSE，官方客户端也是这么声明 `accept` 的。
fn probe_headers(cred: &Credential, token: &str) -> wreq::header::HeaderMap {
    let mut headers = synthetic_headers(cred, token, "text/event-stream");
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers
}

/// 探测用的最小请求体。
///
/// 形态是订阅模式那条路径的硬要求，三处都不能省：`instructions` 缺了直接 400
/// （见 [`PROBE_INSTRUCTIONS`]）；`store` 必须为 `false`——这条路径不存会话；`stream`
/// 必须为 `true`，非流式会被上游拒。不带 `tools`：探测不需要工具，带了只是多一份
/// 可能因模型而异的校验面。
fn probe_body(model: &str) -> Bytes {
    let v = serde_json::json!({
        "model": model,
        "instructions": PROBE_INSTRUCTIONS,
        "input": [{ "role": "user", "content": [{ "type": "input_text", "text": "hi" }] }],
        "store": false,
        "stream": true,
    });
    // 常量结构，序列化不会失败；真失败了也会以上游 400 的形式如实报出来，不必在这里 panic。
    Bytes::from(serde_json::to_vec(&v).unwrap_or_default())
}

/// 传输层错误的粗分类，进错误文案的方括号里（前端按这个形状本地化）。
fn upstream_error_kind(e: &wreq::Error) -> &'static str {
    if e.is_connect() {
        "connect"
    } else if e.is_timeout() {
        "timeout"
    } else if e.is_request() {
        "request"
    } else {
        "transport"
    }
}

/// 把一次探测记进 `usage_logs`，口径与转发路径一致（同一个嗅探器、同一套计价）。
///
/// 差别只在 `ua` 标成 [`PROBE_UA`]、`session_id` 留空——探测没有来访客户端，也不该在上游
/// 那边凭空多出一条会话。
fn log_probe_usage(
    state: &AppState,
    cred: &Credential,
    status: StatusCode,
    sniffer: &UsageSniffer,
    quota: &QuotaSnapshot,
    started: Instant,
) {
    let usage = sniffer.usage;
    let model = sniffer.model.clone();
    let cost = match (&model, usage) {
        (Some(m), Some(u)) => {
            crate::pricing::estimate_usd(m, u.input_tokens, u.cached_tokens, u.output_tokens)
        }
        _ => None,
    };
    let rec = UsageRecord {
        cred_id: Some(cred.id),
        cred_label: cred.label.clone(),
        session_id: None,
        model,
        path: PROBE_PATH.to_owned(),
        ua: Some(PROBE_UA.to_owned()),
        status: status.as_u16() as i64,
        has_usage: usage.is_some(),
        input_tokens: usage.map(|u| u.input_tokens),
        cached_tokens: usage.map(|u| u.cached_tokens),
        output_tokens: usage.map(|u| u.output_tokens),
        reasoning_tokens: usage.map(|u| u.reasoning_tokens),
        total_tokens: usage.map(|u| u.total_tokens),
        // 一把读完，没有「首块」可言；总耗时已经说明一切。
        ttft_ms: None,
        total_ms: Some(started.elapsed().as_millis() as i64),
        cost_usd: cost,
        quota: Some(quota.clone()),
    };
    spawn_usage_log(state.store.clone(), rec);
}

/// 把上游响应翻译成一份结果。限流头由调用方先行解析（读 body 会把响应消费掉），
/// 成败两条路都带上。
fn probe_report(
    status: StatusCode,
    bytes: &[u8],
    sniffer: &UsageSniffer,
    latency_ms: u128,
    quota: QuotaSnapshot,
    retry_after_secs: Option<i64>,
) -> ProbeReport {
    let quota = (!quota.is_empty()).then_some(quota);
    if status.is_success() {
        // 2xx 也可能在流里夹一条失败事件，那种「HTTP 200 但没生成出来」必须报成失败。
        let failure = sse_failure(bytes);
        return ProbeReport {
            ok: failure.is_none(),
            status: status.as_u16(),
            latency_ms,
            model: sniffer.model.clone(),
            error_type: failure.as_ref().and_then(|(t, _)| t.clone()),
            error: failure.map(|(_, m)| truncate(&m)),
            quota,
            retry_after_secs,
        };
    }
    let (error_type, message) = parse_upstream_error(bytes);
    ProbeReport {
        ok: false,
        status: status.as_u16(),
        latency_ms,
        model: None,
        error_type,
        error: Some(truncate(&message)),
        quota,
        retry_after_secs,
    }
}

/// 从上游的错误响应里取 (`error.type`, `error.message`)。
///
/// 解不出 JSON、或解出来没有 message 时退回整段原文——上游拒绝的形状不止一种
/// （CDN 拦截页、网关错误页都不是 JSON），一句「未知错误」等于把唯一的线索丢了。
fn parse_upstream_error(bytes: &[u8]) -> (Option<String>, String) {
    let raw = String::from_utf8_lossy(bytes).trim().to_owned();
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) else { return (None, raw) };
    let s = |p: &str| v.pointer(p).and_then(|x| x.as_str()).map(str::to_owned);
    let etype = s("/error/type").or_else(|| s("/error/code")).or_else(|| s("/type"));
    let message = s("/error/message").or_else(|| s("/message")).or_else(|| s("/detail"));
    (etype, message.unwrap_or(raw))
}

/// 在一段 SSE 里找失败事件，返回 (`error.type`, `error.message`)。
///
/// 上游会先回 200 再在流里说这次生成失败（`response.failed` / `error` 事件）。只看状态码
/// 会把这种情形报成「通过」，而它恰恰是模型不可用时最常见的形状之一。
fn sse_failure(bytes: &[u8]) -> Option<(Option<String>, String)> {
    let text = String::from_utf8_lossy(bytes);
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else { continue };
        let data = data.trim();
        // 先用一次廉价的子串判断挡掉绝大多数事件行，只有可能带错误的才付一次 JSON 解析。
        if !data.contains("\"error\"") && !data.contains("response.failed") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else { continue };
        // 两种形状：{"type":"error", "error":{…}} 与 {"type":"response.failed","response":{"error":{…}}}。
        let Some(err) = v.get("error").or_else(|| v.pointer("/response/error")) else { continue };
        // `"error": null` 是正常事件里的常见字段（成功的 response 对象也带），不算失败。
        if err.is_null() {
            continue;
        }
        let s = |p: &str| err.pointer(p).and_then(|x| x.as_str()).map(str::to_owned);
        let message = s("/message")
            .or_else(|| err.as_str().map(str::to_owned))
            .unwrap_or_else(|| err.to_string());
        return Some((s("/type").or_else(|| s("/code")), message));
    }
    None
}

/// 在一段 SSE 里找终局的 `response` 对象（`response.completed` / `response.incomplete`）。
///
/// 这就是同一次请求在非流式下本该返回的那个体，所以收拢流时直接把它交出去。取最后一个：
/// 一段流里只会有一个终局事件，但真出现两个时后者才是终值。
///
/// 先用一次廉价的子串判断挡掉增量文本事件，只有可能是终局的才付一次 JSON 解析——逐行解析
/// 的话，一次长回复要解上千次。
fn sse_final_response(bytes: &[u8]) -> Option<serde_json::Value> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = None;
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else { continue };
        let data = data.trim();
        if !data.contains("response.completed") && !data.contains("response.incomplete") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else { continue };
        if !matches!(
            v.get("type").and_then(|t| t.as_str()),
            Some("response.completed") | Some("response.incomplete")
        ) {
            continue;
        }
        if let Some(resp) = v.get("response").filter(|r| r.is_object()) {
            out = Some(resp.clone());
        }
    }
    out
}

/// 错误原文截断到 [`PROBE_ERROR_MAX_LEN`] 个**字符**（不是字节——按字节切会把多字节
/// 字符劈成半个，前端拿到的是替换符）。
fn truncate(s: &str) -> String {
    s.chars().take(PROBE_ERROR_MAX_LEN).collect()
}

// ---------- 限流头解析 ----------

impl QuotaSnapshot {
    /// 从上游响应头里解出额度快照。一项都没有时返回一个空快照（调用点据此不覆盖账本）。
    pub fn from_headers(h: &wreq::header::HeaderMap) -> Self {
        let s = |name: &str| {
            h.get(name).and_then(|v| v.to_str().ok()).map(str::trim).map(str::to_owned)
        };
        let f = |name: &str| s(name).and_then(|v| v.parse::<f64>().ok());
        let i = |name: &str| s(name).and_then(|v| v.parse::<i64>().ok());
        let b = |name: &str| s(name).map(|v| matches!(v.as_str(), "true" | "1"));
        Self {
            primary_used_pct: f(config::RL_PRIMARY_USED_PCT),
            primary_window_minutes: i(config::RL_PRIMARY_WINDOW_MINUTES),
            primary_reset_at: s(config::RL_PRIMARY_RESET_AT),
            secondary_used_pct: f(config::RL_SECONDARY_USED_PCT),
            secondary_window_minutes: i(config::RL_SECONDARY_WINDOW_MINUTES),
            secondary_reset_at: s(config::RL_SECONDARY_RESET_AT),
            credits_has_credits: b(config::RL_CREDITS_HAS_CREDITS),
            credits_unlimited: b(config::RL_CREDITS_UNLIMITED),
            credits_balance: f(config::RL_CREDITS_BALANCE),
        }
    }

    /// 距离额度窗口重置还有几秒。
    ///
    /// 解不出来返回 `None`，调用点退回一个保守的固定值——猜一个错的重置时刻会让账号
    /// 要么提前放出来继续撞墙、要么被多关几个小时。字符串怎么解见
    /// [`store::parse_reset_at`]（窗口统计反推起点用的是同一份规则，两处不能各写一遍）。
    pub fn secs_until_reset(&self) -> Option<i64> {
        let raw = self.primary_reset_at.as_deref().or(self.secondary_reset_at.as_deref())?;
        let delta = store::parse_reset_at(raw)? - crate::credentials::now_secs() as i64;
        (delta > 0).then_some(delta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 探测体的三个硬要求（`instructions`/`store:false`/`stream:true`）不能被顺手改掉：
    /// 少任何一个，订阅模式那条路径都会直接 400/422，而报出来的是「模型不可用」。
    #[test]
    fn probe_body_keeps_the_required_shape() {
        let v: serde_json::Value =
            serde_json::from_slice(&probe_body("gpt-5.1-codex")).expect("probe body is valid JSON");
        assert_eq!(v["model"], "gpt-5.1-codex");
        assert_eq!(v["stream"], true);
        assert_eq!(v["store"], false);
        assert!(v["instructions"].as_str().is_some_and(|s| !s.is_empty()));
        assert_eq!(v["input"][0]["content"][0]["type"], "input_text");
        // 带 tools 只是多一份因模型而异的校验面，探测不需要。
        assert!(v.get("tools").is_none());
    }

    /// 上游拒绝的形状不止一种：JSON 的取 `error.type`/`error.message`，非 JSON（CDN 拦截页、
    /// 网关错误页）退回整段原文——丢掉它就等于丢掉唯一的线索。
    #[test]
    fn upstream_errors_keep_their_only_clue() {
        let (etype, msg) = parse_upstream_error(
            br#"{"error":{"type":"usage_limit_reached","message":"You have hit your limit."}}"#,
        );
        assert_eq!(etype.as_deref(), Some("usage_limit_reached"));
        assert_eq!(msg, "You have hit your limit.");

        let (etype, msg) = parse_upstream_error(b"<html>attention required</html>");
        assert!(etype.is_none());
        assert_eq!(msg, "<html>attention required</html>");

        // 有 JSON 但没有 message：整段回去比一句「未知错误」有用。
        let (etype, msg) = parse_upstream_error(br#"{"error":{"code":"model_not_found"}}"#);
        assert_eq!(etype.as_deref(), Some("model_not_found"));
        assert!(msg.contains("model_not_found"));
    }

    /// 「HTTP 200 但流里说失败了」必须报成失败——模型不可用时最常见的形状之一就是这个，
    /// 只看状态码会把它报成通过。
    #[test]
    fn a_failure_event_inside_a_200_stream_is_not_a_pass() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"model\":\"gpt-5.1-codex\",\"error\":null}}\n",
            "\n",
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_error\",\"message\":\"boom\"}}}\n",
        );
        let (etype, msg) = sse_failure(sse.as_bytes()).expect("the failure event is found");
        assert_eq!(etype.as_deref(), Some("server_error"));
        assert_eq!(msg, "boom");

        // `"error": null` 是成功事件里的常见字段，不能被当成失败。
        let ok_sse = concat!(
            "data: {\"type\":\"response.completed\",\"response\":{\"error\":null,",
            "\"model\":\"gpt-5.1-codex\",\"usage\":{\"input_tokens\":9,\"output_tokens\":2}}}\n",
        );
        assert!(sse_failure(ok_sse.as_bytes()).is_none());
    }

    /// 截断按字符走：按字节切会把多字节字符劈成半个，前端拿到的是一串替换符。
    #[test]
    fn error_truncation_never_splits_a_character() {
        let long = "限".repeat(PROBE_ERROR_MAX_LEN + 20);
        let cut = truncate(&long);
        assert_eq!(cut.chars().count(), PROBE_ERROR_MAX_LEN);
        assert!(!cut.contains('\u{fffd}'));
    }

    /// 探测头必须自带 `content-type` 与 `accept`（转发时这两个由来访客户端提供），
    /// 同时保留转发路径那份鉴权与账号身份。
    #[test]
    fn probe_headers_carry_auth_and_the_two_self_supplied_headers() {
        let cred = Credential {
            id: 1,
            label: "test".into(),
            email: None,
            plan_type: None,
            account_id: "acct-1".into(),
            id_token: None,
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: u64::MAX,
            priority: 0,
            disabled: false,
            rpm_limit: 0,
            ban_reason: None,
            resume_at: None,
            proxy: None,
            created_at: 0,
            updated_at: 0,
        };
        let out = probe_headers(&cred, "tok");
        assert_eq!(out.get("content-type").unwrap(), "application/json");
        assert_eq!(out.get("accept").unwrap(), "text/event-stream");
        assert_eq!(out.get("authorization").unwrap(), "Bearer tok");
        assert_eq!(out.get("chatgpt-account-id").unwrap(), "acct-1");
        assert!(out.get("session_id").is_some());
    }

    #[test]
    fn upstream_url_joins_path_and_query() {
        assert_eq!(
            upstream_url("responses", Some("stream=true")),
            "https://chatgpt.com/backend-api/codex/responses?stream=true"
        );
        // 前导斜杠不能拼出一个双斜杠的路径。
        assert_eq!(
            upstream_url("/responses", None),
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }

    fn norm(path: &str, b: &str) -> Normalized {
        normalize_responses_body(path, Bytes::from(b.to_owned()))
    }

    /// `store`/`stream` 的三种来法都要落到上游要的值，且别的字段一个不动。
    #[test]
    fn store_and_stream_are_pinned_on_responses() {
        let pin = |b: &str| -> serde_json::Value {
            serde_json::from_slice(&norm("responses", b).body).unwrap()
        };
        // 漏传（OpenAI 官方 API 那边 store 默认 true、stream 默认 false，两个都是雷）。
        let v = pin(r#"{"model":"gpt-5.4"}"#);
        assert_eq!(v["store"], false);
        assert_eq!(v["stream"], true);
        // 显式反着来。
        let v = pin(r#"{"model":"gpt-5.4","store":true,"stream":false}"#);
        assert_eq!(v["store"], false);
        assert_eq!(v["stream"], true);
        // 已经都对：原样，且别的字段还在。
        let v = pin(r#"{"model":"gpt-5.4","store":false,"stream":true}"#);
        assert_eq!(v["store"], false);
        assert_eq!(v["stream"], true);
        assert_eq!(v["model"], "gpt-5.4");
    }

    /// 「客户端本来要不要流」必须与改写分开记：钉了 `stream` 还得把回程的 SSE 收回成
    /// JSON，漏掉这一半客户端拿到的是读不懂的 `data:` 行。
    #[test]
    fn collapse_tracks_what_the_client_asked_for() {
        assert!(norm("responses", r#"{"model":"m"}"#).collapse, "漏传 stream 就是要 JSON");
        assert!(norm("responses", r#"{"stream":false}"#).collapse);
        assert!(!norm("responses", r#"{"stream":true}"#).collapse, "自己要了流就照流回");
        // 别的端点与解不动的体都不在这条路上，一律不收拢。
        assert!(!norm("models", r#"{"stream":false}"#).collapse);
        assert!(!norm("responses", "not json").collapse);
    }

    #[test]
    fn body_rewrite_only_touches_responses_and_valid_json() {
        // 别的端点（如 models）不碰。
        let raw = Bytes::from_static(br#"{"store":true}"#);
        assert_eq!(normalize_responses_body("models", raw.clone()).body, raw);
        // 前导斜杠仍要认出是 responses。
        let out = normalize_responses_body("/responses", raw.clone()).body;
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["store"], false);
        assert_eq!(v["stream"], true);
        // 解不动的体原样放过，不在这里替上游拦。
        let junk = Bytes::from_static(b"not json");
        assert_eq!(normalize_responses_body("responses", junk.clone()).body, junk);
    }

    /// 收拢流靠的是终局事件里那个 `response` 对象；增量事件与裸的同名字符串都不能骗过它。
    #[test]
    fn the_final_response_object_is_what_gets_collapsed() {
        let sse = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"response.completed\"}\n",
            "data: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp_1\"}}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\"}}\n",
        );
        let v = sse_final_response(sse.as_bytes()).expect("终局事件在这段流里");
        assert_eq!(v["id"], "resp_1");
        assert_eq!(v["status"], "completed");

        // 只有增量事件时没有终局对象——这种流不能当成一次成功的非流式响应交出去。
        assert!(
            sse_final_response(
                b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n"
            )
            .is_none()
        );
        // 未完成也是终局：客户端要的就是那个 status。
        let inc =
            "data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\"}}\n";
        assert_eq!(sse_final_response(inc.as_bytes()).unwrap()["status"], "incomplete");
    }

    fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn client_key_accepted_in_all_three_header_shapes() {
        assert!(client_authorized(&hm(&[("authorization", "Bearer k")]), "k"));
        assert!(client_authorized(&hm(&[("authorization", "k")]), "k"));
        assert!(client_authorized(&hm(&[("x-api-key", "k")]), "k"));
        assert!(!client_authorized(&hm(&[("authorization", "Bearer wrong")]), "k"));
        assert!(!client_authorized(&HeaderMap::new(), "k"));
    }

    /// 鉴权头、逐跳头、以及会暴露「经过了代理」的头都不能转出去。
    #[test]
    fn forward_headers_drop_hop_by_hop_and_inject_identity() {
        let cred = Credential {
            id: 1,
            label: "l".into(),
            email: None,
            plan_type: None,
            account_id: "acct-9".into(),
            id_token: None,
            access_token: "old".into(),
            refresh_token: "r".into(),
            expires_at: 0,
            priority: 0,
            disabled: false,
            rpm_limit: 0,
            ban_reason: None,
            resume_at: None,
            proxy: None,
            created_at: 0,
            updated_at: 0,
        };
        let incoming = hm(&[
            ("authorization", "Bearer client-key"),
            ("content-length", "123"),
            ("x-forwarded-for", "1.2.3.4"),
            ("chatgpt-account-id", "spoofed"),
            ("content-type", "application/json"),
        ]);
        let out = build_forward_headers(&incoming, &cred, "fresh-token");
        assert_eq!(out.get("authorization").unwrap(), "Bearer fresh-token");
        assert_eq!(out.get("chatgpt-account-id").unwrap(), "acct-9");
        assert_eq!(out.get("originator").unwrap(), config::ORIGINATOR);
        assert_eq!(out.get("content-type").unwrap(), "application/json");
        assert!(out.get("content-length").is_none(), "stale length truncates the body upstream");
        assert!(out.get("x-forwarded-for").is_none(), "would advertise the proxy hop");
        assert!(out.get("session_id").is_some());
    }

    /// 只有「主体 + 状态」两类词同时命中才算账号级问题——单看 unauthorized 会把
    /// 一次普通的 token 过期误判成封号，把好账号关掉。
    #[test]
    fn account_error_needs_both_subject_and_state() {
        let banned = br#"{"error":{"message":"Your account has been suspended"}}"#;
        assert!(detect_account_error(StatusCode::FORBIDDEN, banned).is_some());

        let expired = br#"{"error":{"message":"unauthorized: token expired"}}"#;
        assert!(detect_account_error(StatusCode::UNAUTHORIZED, expired).is_none());

        // 状态码不对就不判，哪怕文本命中。
        assert!(detect_account_error(StatusCode::TOO_MANY_REQUESTS, banned).is_none());
    }

    /// 取最后一个 usage：中途事件也可能带一个不完整的读数。
    #[test]
    fn sniffer_reads_usage_from_the_completed_event() {
        let mut s = UsageSniffer::default();
        for line in [
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n",
            "data: {\"type\":\"response.in_progress\",\"response\":{\"model\":\"gpt-5.1-codex\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5.1-codex\",\"usage\":{\"input_tokens\":100,\"input_tokens_details\":{\"cached_tokens\":40},\"output_tokens\":20,\"output_tokens_details\":{\"reasoning_tokens\":12},\"total_tokens\":120}}}\n",
        ] {
            s.feed(&Bytes::from(line));
        }
        let u = s.usage.expect("usage should be sniffed");
        assert_eq!(
            (u.input_tokens, u.cached_tokens, u.output_tokens, u.reasoning_tokens),
            (100, 40, 20, 12)
        );
        assert_eq!(s.model.as_deref(), Some("gpt-5.1-codex"));
        assert!(s.first_byte.is_some());
    }

    /// chunk 边界可以落在一行中间——按行切之前必须把上一块的残尾接回去。
    #[test]
    fn sniffer_survives_chunk_boundaries_mid_line() {
        let full = "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":7,\"output_tokens\":3}}}\n";
        let mut s = UsageSniffer::default();
        for i in (0..full.len()).step_by(9) {
            s.feed(&Bytes::from(full[i..(i + 9).min(full.len())].to_owned()));
        }
        assert_eq!(s.usage.map(|u| u.input_tokens), Some(7));
    }

    /// 一条没有换行的巨大响应体不能把 pending 撑成无界缓冲。
    #[test]
    fn sniffer_caps_the_pending_buffer() {
        let mut s = UsageSniffer::default();
        s.feed(&Bytes::from("x".repeat(MAX_SSE_LINE + 1024)));
        assert!(s.pending.len() <= MAX_SSE_LINE);
    }

    #[test]
    fn quota_snapshot_reads_codex_rate_limit_headers() {
        let mut h = wreq::header::HeaderMap::new();
        h.insert(config::RL_PRIMARY_USED_PCT, HeaderValue::from_static("93.5"));
        h.insert(config::RL_SECONDARY_USED_PCT, HeaderValue::from_static("12"));
        h.insert(config::RL_CREDITS_UNLIMITED, HeaderValue::from_static("false"));
        let q = QuotaSnapshot::from_headers(&h);
        assert_eq!(q.primary_used_pct, Some(93.5));
        assert_eq!(q.peak_used_pct(), Some(93.5));
        assert_eq!(q.credits_unlimited, Some(false));
        assert!(!q.is_empty());
        assert!(QuotaSnapshot::from_headers(&wreq::header::HeaderMap::new()).is_empty());
    }

    /// Clone 也要计数，否则每个副本析构都减一次，在飞数一路减成负数。
    #[test]
    fn in_flight_guard_counts_clones() {
        let counter = Arc::new(std::sync::atomic::AtomicI64::new(0));
        let a = InFlightGuard::new(counter.clone());
        let b = a.clone();
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 2);
        drop(b);
        drop(a);
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 0);
    }
}
