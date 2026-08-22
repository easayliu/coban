//! 转发代理：Codex CLI → coban → ChatGPT 后端（`backend-api/codex`）。
//!
//! 透传请求体，只替换鉴权：校验来访 API Key 后选一个凭证，注入它的 OAuth access_token
//! 与 `chatgpt-account-id`，响应流式原样回传，顺带从 SSE 里嗅探用量与额度。
//!
//! 唯一不「透传」的是 **Chat Completions** 那类接入方（`/v1/chat/completions`）：上游只讲
//! Responses 一种线格式，故请求与响应都要翻译，见 [`crate::chat`]。选号、换号重试、限流头、
//! 用量嗅探、落库、计价两种线格式**共用同一份代码**，翻译只是夹在最外面的一进一出。

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;

use crate::chat;
use crate::config;
use crate::credentials::Credential;
use crate::store::{self, CredentialStore, QuotaSnapshot, UsageRecord};
use crate::web::AppState;

/// 一次转发最多尝试几个凭证（含第一次）。
///
/// 这是硬顶，防的是号池很大时一条请求把整个池子走穿：每次换号都要重发整个请求体。
/// 但它得**够走过一串已经没额度的号**——限流类拒绝是上游的瞬时快速拒绝（不生成、
/// 不花钱），一条请求连撞五六个满额的号在真实号池里再普通不过，而顶太低的后果是
/// 客户端拿到一个「其实还有好号能用」的 429（见 [`RotationBudget`]）。
const MAX_ATTEMPTS: usize = 16;

/// 记进日志的 UA 截断长度。完整 UA 可以很长，而认「谁在发」只需要前面那截。
const UA_MAX_LEN: usize = 120;

/// 解析不出恢复时刻时，额度类暂停退回的固定值（秒）。
///
/// 15 分钟：短到「万一猜错了、其实早就该回血」不至于白关一个号，长到不会每分钟把同一个
/// 号放回去再撞一次墙。两处共用（按阈值预停与撞上额度墙），各写一份必然会写出两个数。
const QUOTA_PAUSE_FALLBACK_SECS: i64 = 15 * 60;

/// 上游错误体记进日志的截断长度。那句给人看的话从来不长，长的是它回显的请求内容。
const UPSTREAM_MSG_MAX: usize = 300;

/// `debug` 那行原样打印请求体的截断长度。够看清头部的字段与前几条消息，又不至于把一条
/// 几百 KB 的对话整个刷进日志。
const REJECTED_BODY_MAX: usize = 4096;

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

    // `GET models` 要在选号之前截下来：它与转发不是一回事（见 openai_model_list）。
    if let Some(resp) = openai_model_list(&state, &path, &method, &uri).await {
        return resp;
    }

    // 请求体在重试间要重发多次，规范化/翻译只做一次（见 plan_request）。
    // 工具顺序要不要排，见 normalize_tool_order。默认不排。
    let sort_tools = state
        .store
        .get_setting_i64(store::NORMALIZE_TOOL_ORDER, store::DEFAULT_NORMALIZE_TOOL_ORDER)
        != 0;
    let Normalized { body, collapse, chat, prefix, input_len } =
        match plan_request(&path, body, sort_tools) {
            Ok(n) => n,
            // chat 那头的形状错误在 coban 这一层就判得出来，送到上游只换回一句指不到原因的 400。
            //
            // **这条路不产生上游请求**，于是既没有用量流水也没有额度快照——从页面上看那个号
            // 一切照旧（额度停在上一次的读数上，从没用过的号则一直是空），而客户端只看到一句
            // 400。所以这里必须留一行日志：它是「客户端发了什么 coban 不认」的唯一线索。
            Err(msg) => {
                tracing::info!(
                    path = %path,
                    ua = %ua_of(&headers).unwrap_or_default(),
                    reason = %msg,
                    "rejected an incoming request before forwarding"
                );
                return error_response(StatusCode::BAD_REQUEST, "invalid_request_error", msg);
            }
        };
    // chat 的体已经翻成 Responses 形状，那它就该打到那个端点上；其余按来访路径原样拼。
    let upstream_path: &str = if chat.is_some() { config::RESPONSES_PATH } else { &path };

    // 会话键。同时决定两件事：这条请求落在**哪个号**上，以及对上游呈现**哪个 `session_id`**
    // ——后者就是上游 prompt cache 的键（见 prefix_parts）。两件事必须用同一个键：
    // 落点变了而 session_id 没变（或反之），缓存照样丢。
    //
    // 客户端自报的会话 id 优先——那是真的会话身份；实测三个真实 codex 客户端一个都不发，
    // 所以绝大多数请求靠的是前缀指纹那条路。
    let session_key =
        incoming_session_id(&headers).or_else(|| prefix.as_ref().map(|p| p.key.clone()));
    // 租约状态与前缀漂移都在**选号之前**读一次并带着走：转发成功会把两张表都更新到这次的
    // 值上，之后再读永远是「没换号、没变过」，归因就永远看不见这两类（见 cache_reason）。
    let session = SessionCtx {
        lease: session_key
            .as_deref()
            .map_or(store::LeaseState::Absent, |k| state.store.lease_state(k)),
        // 客户端自报了会话头时也照样比前缀：那个头认的是「同一段对话」，而前缀变没变是另
        // 一件事——两者都成立时，「有会话头且前缀没变」才是真的该命中。
        drift: prefix
            .as_ref()
            .map_or(store::PrefixDrift::NoBaseline, |p| state.store.prefix_drift(p.segments())),
        key: session_key,
        prefix,
        input_len,
    };

    let started = Instant::now();
    let retry_max = state
        .store
        .get_setting_i64(store::RATE_LIMIT_RETRY_MAX, store::DEFAULT_RATE_LIMIT_RETRY_MAX);
    // 撞限流到底换不换号。关掉时限流类一次都不换，改成在同一个号上等一等再发
    // （见 [`RateLimitWait`]）。
    let rotate_on_rate_limit =
        state.store.get_setting_i64(store::RATE_LIMIT_ROTATE, store::DEFAULT_RATE_LIMIT_ROTATE)
            != 0;
    // 限流类拒绝一路换到号池走完（硬顶 MAX_ATTEMPTS），链路/上游故障由 retry_max 卡住。
    let mut budget = RotationBudget::new(retry_max, rotate_on_rate_limit);

    let mut tried: Vec<i64> = Vec::new();
    let mut last: Option<Response> = None;

    for attempt in 0..MAX_ATTEMPTS {
        let cred = match state.store.select(&tried, session.key()) {
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
                // 本地 RPM 满是限流的一种（而且连上游都没打），照 RateLimited 记：
                // 关掉换号开关时它也不换号，客户端拿到的是带 retry-after 的 429。
                if !budget.allows(Reject::RateLimited) {
                    break;
                }
                continue;
            }
            return internal_error(&e);
        }

        match forward_once(
            &state,
            &cred,
            &path,
            upstream_path,
            &method,
            &uri,
            &headers,
            &body,
            collapse,
            chat.as_ref(),
            &session,
            started,
            in_flight.clone(),
        )
        .await
        {
            Ok(Outcome::Done(resp)) => {
                // 这个号真的把请求发上去了，把会话租约续到它身上：同一段对话之后优先回到
                // 这里，号池增删不再动它（见 `CredentialStore::bind_session`）。
                //
                // 写在这里而不是选号处，是因为只有走到这一步才知道请求真的发得出去；也因此
                // 换号重试时拿到租约的是**最后成功的那个号**——上游的 prompt cache 现在热在
                // 它身上，而不是最初按落点选中的那个。
                if let Some(key) = session.key() {
                    state.store.bind_session(key, cred.id);
                }
                // 前缀 memo 与租约同时更新：两张表必须同步推进，否则「租约还在、底子没了」
                // 这种时序差会造出一类谁也解释不了的归因。
                if let Some(p) = &session.prefix {
                    state.store.note_prefix(p.segments());
                }
                return resp;
            }
            Ok(Outcome::TryNext(reject, resp)) => {
                let status = resp.status().as_u16();
                last = Some(resp);
                if !budget.allows(reject) {
                    tracing::info!(
                        cred_id = cred.id,
                        attempt = attempt + 1,
                        status,
                        "upstream rejected this credential and the retry budget is spent; \
                         handing the rejection back to the client"
                    );
                    break;
                }
                tracing::info!(
                    cred_id = cred.id,
                    attempt = attempt + 1,
                    status,
                    "upstream rejected this credential, trying the next one"
                );
            }
            Err(e) => {
                tracing::warn!(cred_id = cred.id, error = %format!("{e:#}"), "forwarding failed");
                last = Some(error_response(
                    StatusCode::BAD_GATEWAY,
                    "upstream_error",
                    format!("{e:#}"),
                ));
                // 取不到 token（刷新失败）也归在这一类：换个号是有意义的，但它可能是
                // 慢失败，不能无限换。
                if !budget.allows(Reject::Upstream) {
                    break;
                }
            }
        }
    }

    // 走到这里说明这条请求碰过的号全被拒了。attempts 是唯一能把「三个号都满额」和
    // 「硬顶到了、池子里还有号」区分开的线索，日志里必须有。
    if tried.len() >= MAX_ATTEMPTS {
        tracing::warn!(
            attempts = tried.len(),
            "hit the per-request attempt cap; every credential tried was rejected"
        );
    }
    last.unwrap_or_else(|| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_credential_available",
            "every credential failed for this request; check the accounts page",
        )
    })
}

/// 根路径上的 `/models`：同 [`handle_chat`] 的理由——base_url 配到根上的那类客户端，
/// 取模型清单时打的是这条路径。
pub async fn handle_models(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    handle(State(state), Path(config::MODELS_PATH.to_owned()), method, uri, headers, Bytes::new())
        .await
}

/// 根路径上的 `/chat/completions`：把 coban 当 OpenAI 兼容端点、而 base_url 里没带 `/v1`
/// 的那种配法。`/v1/chat/completions` 由 [`handle`] 的通配路由收，两条落到同一段逻辑——
/// 只收一条的话另一条是个 405，而客户端那头只显示「请求失败」，指不到路径上（同
/// [`crate::web`] 里两条转发路由并存的理由）。
pub async fn handle_chat(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle(State(state), Path(chat::PATH.to_owned()), method, uri, headers, body).await
}

/// 一次转发的结果：定局，还是「换个号再来」。
enum Outcome {
    Done(Response),
    /// 上游拒了这个凭证。附带的响应是兜底——换到最后全失败时交回它。
    TryNext(Reject, Response),
}

/// 换号的理由。三类失败的代价与终止性都不一样，故要分开记（见 [`RotationBudget`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reject {
    /// **这个号**现在满了：上游 429，或本地 RPM 闸。它当场被排掉（`exclude`）并打上冷却，
    /// 所以「再换一个」是严格向前走一遍号池，走得完、也走不回头。
    ///
    /// 只有这一类受 [`store::RATE_LIMIT_ROTATE`] 那道开关约束：满了是会自己回血的，
    /// 「等一等」才是一个真选项。
    RateLimited,
    /// **这个号**坏了：账号级错误（凭证失效、被封）。它当场被 `mark_banned` 停用，
    /// 换号同样是向前走一遍号池。
    ///
    /// **不受换号开关约束**：这不是「等一等就好」的事，不换只会让一个坏号把每条请求
    /// 都拖死，而池子里的好号一直闲着。
    Credential,
    /// **这条链路或上游本身**的问题：连不上、出站被掐。换号有意义（逐账号代理各走各的
    /// 出口），但它既不能证明哪个号坏了、也不会自己收敛，重发整个请求体的代价和超时
    /// 风险全在这一类上。
    Upstream,
}

/// 换号预算。
///
/// 分两类的理由：**限流不该由一个小数字来卡**。撞上限流的号已经被排掉并打了冷却，继续
/// 换就是把号池里还能用的号找出来——这正是把一堆号挂在 coban 后面的全部意义；而一个默认
/// 2 的预算会让「前三个号刚好都满额」变成客户端眼里的 429，哪怕后面还有十几个好号闲着。
/// 于是限流类拒绝只受 [`MAX_ATTEMPTS`] 这道硬顶约束。
///
/// [`store::RATE_LIMIT_RETRY_MAX`] 管的是另一类：链路/上游故障。那类失败慢（超时）、
/// 每次都要重发整个请求体、而且换号未必能好，正需要一个上限把注定打不通的请求早点判死。
///
/// `retry_max == 0` 仍是那个明确的关闭开关：一次都不换，上游的判决（含 429）原样交回
/// 客户端，让它自己退避。
///
/// [`store::RATE_LIMIT_ROTATE`] 只掐 [`Reject::RateLimited`] 那一类（`on_rate_limit`）：关掉之后撞 429
/// 一个号都不换，等待与重试改在 [`RateLimitWait`] 里就地做完；而链路/上游故障照旧按
/// `upstream_left` 换号——那与「这个号满没满」无关，不换只会让一条本可以打通的请求
/// 白白失败。
struct RotationBudget {
    rotate: bool,
    /// 撞限流要不要换号。
    on_rate_limit: bool,
    upstream_left: usize,
}

impl RotationBudget {
    fn new(retry_max: i64, on_rate_limit: bool) -> Self {
        let n = retry_max.max(0);
        Self { rotate: n > 0, on_rate_limit, upstream_left: n as usize }
    }

    /// 这次失败之后还要不要再换一个号。会扣掉相应的额度。
    fn allows(&mut self, reject: Reject) -> bool {
        if !self.rotate {
            return false;
        }
        match reject {
            Reject::RateLimited => self.on_rate_limit,
            Reject::Credential => true,
            Reject::Upstream => match self.upstream_left {
                0 => false,
                n => {
                    self.upstream_left = n - 1;
                    true
                }
            },
        }
    }
}

/// 关掉换号开关之后，撞 429 的那条请求「就地等一等再用同一个号发一遍」的额度。
///
/// 只在 [`store::RATE_LIMIT_ROTATE`] 关着时有额度；开着（默认）时 `left` 恒为 0，
/// 也就是撞 429 立刻换号，与这个类型没做过任何事一样。
///
/// **等多久由上游说了算，配置只给上限**：真正的等待时长是
/// [`rate_limit_cooldown`] 那三级取值（`retry-after` → 体里的恢复提示 → 冷却时长），
/// 而 [`store::RATE_LIMIT_WAIT_SECS`] 是「最多愿意等这么久」。等得比它还久的那种 429
/// 多半是额度用尽（几小时后才回血），挂着等只会让客户端自己先超时——那时当场把 429
/// 交回去，让客户端自己决定什么时候再来。
struct RateLimitWait {
    /// 还能就地重试几次。
    left: u32,
    /// 一次重试最多愿意等多久（秒）。
    max_wait: i64,
}

impl RateLimitWait {
    fn from_settings(store: &CredentialStore) -> Self {
        let rotate =
            store.get_setting_i64(store::RATE_LIMIT_ROTATE, store::DEFAULT_RATE_LIMIT_ROTATE) != 0;
        if rotate {
            return Self { left: 0, max_wait: 0 };
        }
        let left = store
            .get_setting_i64(
                store::RATE_LIMIT_WAIT_RETRY_MAX,
                store::DEFAULT_RATE_LIMIT_WAIT_RETRY_MAX,
            )
            .clamp(0, MAX_ATTEMPTS as i64) as u32;
        let max_wait =
            store.get_setting_i64(store::RATE_LIMIT_WAIT_SECS, store::DEFAULT_RATE_LIMIT_WAIT_SECS);
        Self { left, max_wait }
    }

    /// 这次 429 要不要就地等；`Some(d)` 是该睡多久。返回 `Some` 就扣掉一次额度。
    fn allows(&mut self, cooldown_secs: i64) -> Option<Duration> {
        if self.left == 0 || cooldown_secs > self.max_wait {
            return None;
        }
        self.left -= 1;
        Some(Duration::from_secs(cooldown_secs.max(0) as u64))
    }
}

/// 用指定凭证发一次。
#[allow(clippy::too_many_arguments)]
async fn forward_once(
    state: &AppState,
    cred: &Credential,
    path: &str,
    upstream_path: &str,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &Bytes,
    collapse: bool,
    chat: Option<&ChatMode>,
    session: &SessionCtx,
    started: Instant,
    in_flight: InFlightGuard,
) -> anyhow::Result<Outcome> {
    let session_key = session.key();
    let client = state.clients.for_credential(cred)?;
    let mut token = state.store.valid_access_token(&state.clients, cred).await?;

    // chat 模式下**不带来访的 query**：那串是给 `/v1/chat/completions` 的，接在
    // `responses` 后面只是往上游送一堆它不认识的参数。
    let query = uri.query().filter(|_| chat.is_none());
    let url = upstream_url(upstream_path, query);
    let mut fwd_headers =
        build_forward_headers(headers, cred, &token, session_key.unwrap_or_default());
    if collapse || chat.is_some() {
        // 体里的 `stream` 已被我们钉成 true，`accept` 得跟着说 SSE：官方客户端不存在
        // 「体里要流、头里要 JSON」这种自相矛盾的形态，别让上游去猜。
        fwd_headers.insert(header::ACCEPT, HeaderValue::from_static("text/event-stream"));
    }
    if chat.is_some() {
        // 体已经被换成我们自己序列化的 JSON，来访那头的 content-type 不再作数
        // （有客户端发 `application/json; charset=utf-8` 之外的写法）。
        fwd_headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }

    // 这条请求最多发两遍：第一遍照客户端给的体发，上游解不开里面的加密推理时摘掉它、
    // **同一个号**再发一遍（见 [`strip_encrypted_reasoning`]）。所以体与头都得留着重用。
    let mut fwd_body = body.clone();
    let mut stripped = false;
    // 撞 401 之后强刷过一次 token 没有：只给一次，刷完还是 401 就是这个号真的坏了。
    let mut token_refreshed = false;
    // 这个会话在这个号上已经吃过一次「解不开」：直接摘掉，省下那次注定 400 的往返
    // （见 [`StaleReasoningMemo`]）。
    if stale_reasoning_known(&state.stale_reasoning, session_key, cred.id)
        && let Some(fixed) = strip_encrypted_reasoning(&fwd_body)
    {
        fwd_body = fixed;
        stripped = true;
    }

    // 撞 429 之后就地等的额度。关着换号开关时才有额度，见 [`RateLimitWait`]。
    let mut rate_limit_wait = RateLimitWait::from_settings(&state.store);

    let (up, quota) = loop {
        let req = client
            .request(wreq::Method::from_bytes(method.as_str().as_bytes())?, &url)
            .headers(fwd_headers.clone())
            .body(fwd_body.clone());

        let up = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                // 连接层失败不算这个账号的错（除非它配了个坏代理，而那在建客户端时就报了），
                // 换个号重试一次是有意义的——尤其逐账号代理各走各的出口。
                return Ok(Outcome::TryNext(
                    Reject::Upstream,
                    error_response(
                        StatusCode::BAD_GATEWAY,
                        "upstream_unreachable",
                        format!("could not reach upstream: {e}"),
                    ),
                ));
            }
        };

        let status = StatusCode::from_u16(up.status().as_u16())?;
        let quota = QuotaSnapshot::from_headers(up.headers());
        // 转发路径不关心停没停：这条请求已经在飞，暂停只影响后面的选号。
        let _ = maybe_pause_on_quota(state, cred, &quota);

        if status.is_success() {
            break (up, quota);
        }

        // 非 2xx：先把体读出来判一判是不是账号级问题，再决定换号还是交回客户端。
        let up_headers = up.headers().clone();
        let bytes = up.bytes().await.unwrap_or_default();
        log_usage(
            state,
            cred,
            path,
            headers,
            session,
            status.as_u16() as i64,
            None,
            &quota,
            started,
            None,
        );

        if let Some(reason) = detect_account_error(status, &bytes) {
            state.store.mark_banned(cred.id, &reason)?;
            return Ok(Outcome::TryNext(
                Reject::Credential,
                error_passthrough(status, &up_headers, bytes, chat),
            ));
        }
        // 401：上游不认这个号的鉴权。**它必须退出调度**——留在池子里的话，之后每一条落到
        // 它身上的请求都是同一个 401，而客户端那头看到的只是「请求失败」，与「服务挂了」
        // 分不开。
        //
        // 但先给一次强刷的机会：本地记的有效期可能就是错的（两边的钟差得多、或上游提前
        // 让它失效），那种情况下换一个 token 就好了，停用一个其实能用的号才是更大的损失。
        // 刷完再撞一次 401 才是定论。
        //
        // **上游把话说死了的那几种除外**（见 [`DEAD_AUTH_HINTS`]）：授权整个被作废时，
        // refresh_token 换回来的是一个同样不被认的 token，那一趟往返每条请求都要白等一遍。
        //
        // **403 不在此列**（仍只按 [`detect_account_error`] 的关键词判）：401 说的是「你是
        // 谁我不认」，那必然是这个号的事；而 403 可能来自边缘（Cloudflare 的机器人拦截页
        // 就是 403），那时被拒的是这条出站链路，不是账号——照 401 那样处置，一次风控能把
        // 整个号池一次性停光。
        if status == StatusCode::UNAUTHORIZED {
            // 上游把话说死了的那几种（见 [`DEAD_AUTH_HINTS`]）直接停用，不刷。
            if !detect_dead_auth(&bytes) && !token_refreshed {
                token_refreshed = true;
                match state.store.refresh_access_token(&state.clients, cred, &token).await {
                    Ok(fresh) if fresh != token => {
                        tracing::info!(
                            cred_id = cred.id,
                            "upstream rejected this credential's token; refreshed it and \
                             retrying once on the same credential"
                        );
                        token = fresh;
                        // 只换鉴权那一项：整份重建会把前面按 collapse/chat 钉过的
                        // accept 与 content-type 一起抹掉。
                        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}")) {
                            fwd_headers.insert(HeaderName::from_static("authorization"), v);
                        }
                        continue;
                    }
                    // 刷出来还是同一个（并发时别人刚刷过、而那个也被拒了）：没救了。
                    Ok(_) => {}
                    Err(e) => tracing::warn!(
                        cred_id = cred.id,
                        error = %format!("{e:#}"),
                        "upstream rejected this credential's token and refreshing it failed"
                    ),
                }
            }
            let reason = format!("upstream 401: {}", upstream_message(&bytes));
            tracing::warn!(
                cred_id = cred.id,
                reason = %reason,
                refreshed = token_refreshed,
                "upstream will not authenticate this credential; taking it out of the rotation"
            );
            state.store.mark_banned(cred.id, &reason)?;
            return Ok(Outcome::TryNext(
                Reject::Credential,
                error_passthrough(status, &up_headers, bytes, chat),
            ));
        }

        if status == StatusCode::TOO_MANY_REQUESTS {
            // 额度用尽与突发限流是两件事，处置也就不一样，见 [`detect_usage_limit`]。
            if detect_usage_limit(&bytes) {
                let secs = usage_limit_pause_secs(&up_headers, &bytes, &quota);
                tracing::warn!(
                    cred_id = cred.id,
                    pause_secs = secs,
                    "upstream says this credential's usage limit is reached; \
                     pausing it until the quota resets"
                );
                if let Err(e) = state.store.pause_for_rate_limit(cred.id, secs) {
                    // 落库失败就退回进程内冷却：这一轮别再选中它，比什么都不做强。
                    tracing::warn!(
                        cred_id = cred.id,
                        error = %format!("{e:#}"),
                        "could not pause the credential; falling back to an in-memory cooldown"
                    );
                    state.store.note_rate_limited(cred.id, secs);
                }
                // **不就地等**：恢复时刻在几小时之后，而这个号已经被停用了——在一个停用的
                // 号上等着重发，等于把刚下的判决当场推翻。
                return Ok(Outcome::TryNext(
                    Reject::RateLimited,
                    error_passthrough(status, &up_headers, bytes, chat),
                ));
            }
            let secs = rate_limit_cooldown(state, &up_headers, &bytes);
            // 冷却先打上，两条路都要：换号那条靠它把这个号排出选号，就地等那条靠它让
            // **别的**请求别再撞同一堵墙——而等的时长与冷却是同一个数，睡醒时它自己就
            // 到期了，不必也不该在这里提前抹掉。
            state.store.note_rate_limited(cred.id, secs);
            // 关掉换号开关的那种配法：等一等，再用同一个号发一遍。
            //
            // 重发不再占 RPM 名额（名额由调用点在选号后占过一次），理由同下面摘密文
            // 那条：为了一次自己发起的重试把这条客户端请求判死，换回来的只是同一个 429。
            if let Some(wait) = rate_limit_wait.allows(secs) {
                tracing::info!(
                    cred_id = cred.id,
                    wait_secs = secs,
                    retries_left = rate_limit_wait.left,
                    "upstream rate limited this credential; waiting it out on the same credential"
                );
                tokio::time::sleep(wait).await;
                continue;
            }
            tracing::info!(
                cred_id = cred.id,
                cooldown_secs = secs,
                "upstream rate limited this credential, cooling it down"
            );
            return Ok(Outcome::TryNext(
                Reject::RateLimited,
                error_passthrough(status, &up_headers, bytes, chat),
            ));
        }
        // 上游解不开客户端捎来的加密推理：摘掉再发一遍。**不换号**——密文绑在产出它的那个
        // 号上，换到第三个号一样解不开；也不能就这么把 400 交回去——客户端下一轮还会把同一
        // 段密文发回来，这段会话就此卡死。
        //
        // 重发不再占 RPM 名额（名额由调用点在选号后占过一次）：为了一个额外的修复请求把这条
        // 客户端请求判死，换回来的只是同一句 400。
        if !stripped
            && detect_stale_encrypted_content(status, &bytes)
            && let Some(fixed) = strip_encrypted_reasoning(&fwd_body)
        {
            tracing::info!(
                cred_id = cred.id,
                "upstream could not decrypt the reasoning carried by this request; \
                 retrying on the same credential without it"
            );
            note_stale_reasoning(&state.stale_reasoning, session_key, cred.id);
            fwd_body = fixed;
            stripped = true;
            continue;
        }
        // 其余（400/404/422…）是这条请求本身的问题，换号也不会好，原样交回。
        //
        // **这一行是唯一的排查材料**：原样交回之后 coban 这边什么都不剩，用量流水里只有
        // 一个光秃秃的 400，而客户端那头通常只显示一句「请求失败」。上游那句话 + 请求体的
        // 形状 + 谁发的，三样凑齐才定位得到是哪个接入方写错了哪个字段。
        tracing::warn!(
            cred_id = cred.id,
            status = status.as_u16(),
            path = %path,
            ua = %ua_of(headers).unwrap_or_default(),
            upstream = %upstream_message(&bytes),
            body = %body_shape(&fwd_body),
            "upstream rejected the request itself; it is the request that is wrong, not the account"
        );
        // 形状不够用时的下一步。默认不打：请求体里是用户的整段对话。
        tracing::debug!(
            cred_id = cred.id,
            body = %String::from_utf8_lossy(&fwd_body)
                .chars()
                .take(REJECTED_BODY_MAX)
                .collect::<String>(),
            "the body upstream rejected"
        );
        return Ok(Outcome::Done(error_passthrough(status, &up_headers, bytes, chat)));
    };

    if collapse {
        return Ok(Outcome::Done(
            collapse_upstream(state, cred, path, headers, session, chat, up, quota, started).await,
        ));
    }

    Ok(Outcome::Done(stream_upstream(
        state, cred, path, headers, session, chat, up, quota, started, in_flight,
    )))
}

/// 把上游的 SSE 收拢成一个一次性 JSON 响应。
///
/// 只在客户端没要流时走这里（见 [`Normalized`]）：体里的 `stream` 被钉成了 true，
/// 上游必然回 SSE，而这个客户端等的是一个 `response` 对象，把事件流原样交给它等于
/// 让它读一堆读不懂的 `data:` 行。
///
/// 代价是这条路径不再是流式的：整段响应读完才回。要延迟就该在请求里写 `stream: true`。
///
/// 终局那个对象**不能原样交出去**：它的 `output` 是空的，正文只在增量事件里
/// （见 [`fill_missing_output`]）。
#[allow(clippy::too_many_arguments)]
async fn collapse_upstream(
    state: &AppState,
    cred: &Credential,
    path: &str,
    req_headers: &HeaderMap,
    session: &SessionCtx,
    chat: Option<&ChatMode>,
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
                session,
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
        session,
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

    // 两种线格式在这里、也只在这里分道：chat 客户端等的是一个 `chat.completion` 对象，
    // Responses 客户端等的是那个 `response` 对象本身。前面读体、嗅探、落库、失败判定
    // 那几步两者一字不差地共用。
    let body = match chat {
        Some(mode) => match chat::aggregate(&bytes, &mode.model) {
            Ok(b) => b,
            Err((etype, message)) => {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    etype.as_deref().unwrap_or("upstream_error"),
                    message,
                );
            }
        },
        None => {
            let Some(mut resp) = sse_final_response(&bytes) else {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    "upstream_error",
                    "the upstream stream ended without a completed response",
                );
            };
            fill_missing_output(&mut resp, &bytes);
            serde_json::to_vec(&resp).unwrap_or_else(|_| {
                error_body("internal_error", "failed to serialize the response")
            })
        }
    };
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
    session: &SessionCtx,
    chat: Option<&ChatMode>,
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
        session_key: session.key.clone(),
        lease: session.lease,
        drift: session.drift,
        input_len: session.input_len,
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

    // chat 客户端读不懂 Responses 的事件流，得边收边翻（见 [`chat::StreamXlate`]）。
    // **嗅探排在翻译之前**，吃到的仍是上游原始字节——用量、计价、额度那套账因此与线格式
    // 无关，不会因为多了一种接入形态而出现两套读数。
    let body = match chat {
        None => Body::from_stream(stream),
        Some(mode) => {
            let xlate = Arc::new(parking_lot::Mutex::new(chat::StreamXlate::new(
                mode.model.clone(),
                mode.include_usage,
            )));
            let tail = xlate.clone();
            Body::from_stream(
                stream.map(move |chunk| chunk.map(|bytes| xlate.lock().feed(&bytes))).chain(
                    // 上游流走完还得补收尾：`[DONE]`，或者（流断在终局事件之前时）一条
                    // 错误事件。少了它，客户端会一直等一个不会来的结束标记。
                    futures_util::stream::once(async move {
                        Ok::<Bytes, wreq::Error>(tail.lock().flush())
                    }),
                ),
            )
        }
    };

    builder.body(body).unwrap_or_else(|e| internal_error_plain(&e.to_string()))
}

/// 拼上游 URL：`UPSTREAM_BASE` + 来访路径（+ 原样带上 query）。
/// 规范化后的请求体。
struct Normalized {
    /// 发给上游的体。非 `responses` 路径、或解不动的体，原样带回。
    body: Bytes,
    /// 客户端要的是一次性 JSON（体里的 `stream` 不是 `true`）。上游只出 SSE，所以这种
    /// 请求要在本层把流收拢回一个 JSON 体（见 [`collapse_upstream`]）。
    collapse: bool,
    /// 来访用的是 Chat Completions 线格式时，回程翻译要用到的那点形态；`None` 表示
    /// Responses 原样透传。
    chat: Option<ChatMode>,
    /// 这条请求的会话指纹与四段分段哈希（见 [`prefix_parts`]）。解不出体时为 `None`。
    prefix: Option<PrefixParts>,
    /// `input[]` 有几项（解不出体时 0）。给缓存归因用，见 [`cache_reason`]。
    input_len: usize,
}

/// chat 线格式下这次请求的形态：翻请求时定下，翻响应时要用。
struct ChatMode {
    /// 客户端请求的模型名。上游没报模型时回显它。
    model: String,
    /// 流式收尾要不要补一条只带 usage 的 chunk（`stream_options.include_usage`）。
    include_usage: bool,
}

/// 决定这条来访请求怎么送上游：翻线格式（chat），还是钉几个字段（responses）。
///
/// 两条路都在**换号重试之前**只做一次：重试要重发整个请求体，把翻译放进循环里等于每次
/// 换号都重做一遍，而结果逐字节相同。
fn plan_request(path: &str, body: Bytes, sort_tools: bool) -> Result<Normalized, String> {
    if path.trim_start_matches('/') == chat::PATH {
        let t = chat::translate_request(&body, sort_tools)?;
        return Ok(Normalized {
            body: t.body,
            // 「客户端没要流」在两种线格式里是同一件事，收拢那段代码也就共用。
            collapse: !t.stream,
            chat: Some(ChatMode { model: t.model, include_usage: t.include_usage }),
            prefix: t.prefix,
            input_len: t.input_len,
        });
    }
    Ok(normalize_responses_body(path, body, sort_tools))
}

/// 把 `responses` 请求体钉成上游要的样子：`store: false`、`stream: true`。
///
/// 上游对这几项都是硬约束，且各自的 400 长得一模一样地不讲道理：
/// - `store` 漏传或传 `true` → `Store must be set to false`（会话不落在 ChatGPT 侧）；
/// - `stream` 漏传或传 `false` → `Stream must be set to true`（这条路径只出 SSE）；
/// - `input` 给一段裸文本 → `Input must be a list`（见 [`normalize_input_shape`]）。
///
/// codex CLI 两项都带对了，但照 OpenAI 官方 Responses API 写的客户端不会——那边 `store`
/// 默认 `true`、`stream` 默认 `false`，两条默认值正好都踩在雷上。改写只此一处：这是上游的
/// 硬约束而不是用户的选择，让每个接入方各自去踩一遍没有意义。
///
/// 钉 `stream` 与钉 `store` 有个区别：`store` 改了客户端察觉不到，而 `stream` 改了会把
/// 一个 JSON 响应变成 SSE。所以这里同时记下「客户端本来没要流」，由 [`collapse_upstream`]
/// 把流收回成 JSON——只改体不管回程的话，客户端拿到的是一堆读不懂的 `data:` 行。
///
/// 顺手**把上游拒收的参数丢掉**（[`drop_unsupported_params`]）：带上去是整条请求 400，而
/// 客户端那头看到的只是一句「请求失败」。与 [`crate::chat`] 丢 `temperature` 那一堆同一个
/// 取舍——客户端设的采样参数与上限静默失效，好过每条请求都失败。codex CLI 这些一个都不发，
/// 照 OpenAI 官方 Responses API 写的客户端会发，`temperature` 尤其常见（SDK 与各类前端默认
/// 就带一个）。
///
/// 解不动的体（非 JSON、非对象）原样放过：判 400 是上游的事，这里不替它拦。
fn normalize_responses_body(path: &str, body: Bytes, sort_tools: bool) -> Normalized {
    if path.trim_start_matches('/') != config::RESPONSES_PATH {
        return Normalized { body, collapse: false, chat: None, prefix: None, input_len: 0 };
    }
    let Ok(serde_json::Value::Object(mut obj)) = serde_json::from_slice(&body) else {
        return Normalized { body, collapse: false, chat: None, prefix: None, input_len: 0 };
    };
    // 趁体已经解开算指纹：为此再解析一遍是白花的 CPU——真实流量里这个体有几百 KB。
    // **排在算指纹之前**：指纹里就含 tools 及其顺序，反过来的话前缀稳住了而落点还在跟着
    // 客户端那个乱序变——两件事必须用同一份顺序。裸文本的 `input` 同理，得先包成列表，
    // 否则指纹与 `input_len` 认的是一个上游根本不会接受的形状。
    let rewrote_input = normalize_input_shape(&mut obj);
    let reordered = sort_tools && normalize_tool_order(&mut obj);
    let prefix = prefix_parts(&obj);
    let input_len = obj.get("input").and_then(|v| v.as_array()).map_or(0, |a| a.len());
    let yes = Some(&serde_json::Value::Bool(true));
    let no = Some(&serde_json::Value::Bool(false));
    let collapse = obj.get("stream") != yes;
    // 先扫参数、再判快路径：要不要重新序列化取决于**真的丢掉了东西**，而不是某个字段在不在。
    let dropped = drop_unsupported_params(&mut obj);
    if !collapse && obj.get("store") == no && !dropped && !reordered && !rewrote_input {
        // 三项都已经对、也没有该丢的参数：不重新序列化（也就不会顺手改掉字段顺序）。
        return Normalized { body, collapse, chat: None, prefix, input_len };
    }
    obj.insert("store".to_owned(), serde_json::Value::Bool(false));
    obj.insert("stream".to_owned(), serde_json::Value::Bool(true));
    match serde_json::to_vec(&serde_json::Value::Object(obj)) {
        Ok(v) => Normalized { body: Bytes::from(v), collapse, chat: None, prefix, input_len },
        // 序列化一个刚解出来的 JSON 不会失败，真失败了也宁可发原体而不是空体。
        Err(_) => Normalized { body, collapse, chat: None, prefix, input_len },
    }
}

/// `input` 给的是一段裸文本时，包成上游要的那一条用户消息。回「是否真的改过」。
///
/// OpenAI 官方 Responses API 允许 `input` 直接给字符串（`input: "hi"` 是那条消息的简写，
/// 官方 SDK 的第一个示例就是这么写的），而订阅这条路径只认列表，回的是
/// `Input must be a list`——客户端那头看到的只是一句「请求失败」，指不到是哪个字段。
/// 与钉 `store`/`stream` 同一个取舍：这是上游的硬约束而不是用户的选择，让每个接入方各自
/// 去踩一遍没有意义。codex CLI 本来就发列表，这段对它是空动作。
///
/// **只认字符串这一种**。列表原样放过；别的形状（数字、对象、null）不猜——那不是官方
/// 允许的写法，替它包一层只会把一个明确的 400 变成一段语义可疑的请求。
fn normalize_input_shape(obj: &mut serde_json::Map<String, serde_json::Value>) -> bool {
    let Some(text) = obj.get("input").and_then(|v| v.as_str()).map(str::to_owned) else {
        return false;
    };
    // 内容块的形状照 [`crate::chat`] 翻出来的那份：两条线格式发上去的用户消息必须长得
    // 一样，否则同一段对话走两条路会算出两个指纹，缓存白丢一次。
    obj.insert(
        "input".to_owned(),
        serde_json::json!([{
            "role": "user",
            "content": [{ "type": "input_text", "text": text }],
        }]),
    );
    true
}

/// 上游拒收、且**丢了客户端察觉不到**的参数。逐个实测过，回的都是
/// `Unsupported parameter: <名字>`。
///
/// 丢掉的代价是客户端设的采样参数、输出上限与观测选项静默失效；带上去的代价是**每一条请求
/// 都 400**——后者更糟，且 400 不换号重试（见 [`forward_once`] 那头对状态码的分流），表现就是
/// 这个接入方彻底不可用。`max_output_tokens` 也在这份清单里：它是最早被实测出来的一个。
///
/// 反过来，实测**能**过的（一个都不许动）：`tools`/`tool_choice`/`parallel_tool_calls`/
/// `reasoning`（含 `summary`）/`text`（含 `verbosity` 与 `json_schema`）/`include`/
/// `prompt_cache_key`/`stream_options`。
const UNSUPPORTED_PARAMS: &[&str] = &[
    "temperature",
    "top_p",
    "presence_penalty",
    "frequency_penalty",
    "seed",
    "stop",
    "user",
    "metadata",
    "logit_bias",
    "logprobs",
    "top_logprobs",
    "top_k",
    "safety_identifier",
    "truncation",
    "max_tool_calls",
    "max_output_tokens",
];

/// 丢掉上游拒收的参数，返回**有没有真丢掉过东西**（快路径靠这个决定要不要重新序列化）。
///
/// 分两档，界线是「丢了会不会改变语义」：
/// - [`UNSUPPORTED_PARAMS`]：无条件丢。
/// - 带着默认值才丢的那几个：`n: 1`、`background: false`、`service_tier: "auto"`（这一个上游
///   的措辞不一样：`Unsupported service_tier: auto`）。官方 SDK 会把它们填成默认值发出去，
///   那时丢掉没有语义损失；而 `n: 2` 是真要两条、`background: true` 是真要异步任务，静默丢掉
///   会让客户端拿到一个它没要的东西，那时不如把上游那句 400 照原样交回去。值的形状不对
///   （如 `n: 1.0`）也归到这一支：不敢当默认值处置，一样交给上游判。
///
/// **`previous_response_id` 刻意不丢**：这条路径 `store` 被钉成 `false`，会话根本不在上游，
/// 丢掉的后果是模型看不见前面几轮却照样答——静默答错比一句明确的 400 难查得多。
fn drop_unsupported_params(obj: &mut serde_json::Map<String, serde_json::Value>) -> bool {
    let mut dropped = false;
    for k in UNSUPPORTED_PARAMS {
        dropped |= obj.remove(*k).is_some();
    }
    for (k, default) in [
        ("n", serde_json::json!(1)),
        ("background", serde_json::json!(false)),
        ("service_tier", serde_json::json!("auto")),
    ] {
        if obj.get(k) == Some(&default) {
            obj.remove(k);
            dropped = true;
        }
    }
    dropped
}

/// 摘掉请求体里捎来的**加密推理**（`input` 里 `type: "reasoning"` 的那些项），返回改写后的
/// 体；里面本来就没有这东西则返回 `None`——那说明这条 400 不是这个病，重发一遍白发一次。
///
/// 上游那串 `encrypted_content`（`gAAA…`）**绑在产出它的那个账号上**：拿到别的号上去解，回的
/// 就是 400 `The encrypted content … could not be verified`。而这条代理天生就会换号——粘性
/// 落点那个号一旦停用/冷却/额度暂停/RPM 打满/被这一轮重试排掉，同一段会话就落到了同档的
/// 下一个号上（见 [`store::CredentialStore::select`]），而客户端手里攥着的还是上一个号产的
/// 密文。客户端做过历史压缩、换了模型档位从而指纹变了，也是同一回事。
///
/// 丢**整项**而不是只摘掉 `encrypted_content` 字段：一个只剩 `summary` 的 reasoning 项是官方
/// 客户端不会产生的形状，上游怎么判说不好；而「`input` 里干脆没有 reasoning 项」正是没开
/// `include: reasoning.encrypted_content` 的客户端天天在发的东西，稳。
///
/// 代价是模型看不见前面几轮的思考过程（可见的消息、工具调用与结果一项不动），换来的是这段
/// 会话还能接着往下走——而不是从这一轮起每一次都 400。
fn strip_encrypted_reasoning(body: &Bytes) -> Option<Bytes> {
    let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(body).ok()?;
    let mut touched = false;
    {
        let input = obj.get_mut("input")?.as_array_mut()?;
        let before = input.len();
        input.retain(|item| item.get("type").and_then(|t| t.as_str()) != Some("reasoning"));
        touched |= input.len() != before;
        // 别的项上万一也挂着密文，一样摘掉：判据是「上游解不开某段密文」，而不是「reasoning 项」。
        for item in input.iter_mut() {
            if let Some(o) = item.as_object_mut() {
                touched |= o.remove("encrypted_content").is_some();
            }
        }
    }
    if !touched {
        return None;
    }
    // `include: ["reasoning.encrypted_content"]` 刻意留着：它管的是**回程**——这一轮之后的
    // 密文由现在这个号产出，而这段会话往后就粘在它上面了，下一轮还能接着用上。
    serde_json::to_vec(&serde_json::Value::Object(obj)).ok().map(Bytes::from)
}

/// 「**这个会话**捎来的密文在**这个号**上解不开」的记忆：`(会话键, 凭证 id)` 的有界集合。
///
/// 为什么需要它：修复一次并不能一劳永逸。客户端每轮都把整段历史发回来，里面永远躺着上一个
/// 号产的那段密文——不记住的话，从换号那一轮起**每一轮**都要先吃一个注定失败的 400 再被
/// [`forward_once`] 摘掉重发，白搭一次上游往返，用量页上还多一条 400。记住之后就直接摘掉再
/// 发，那次往返省掉了。
///
/// 只活在进程内存里（同 [`store::CredentialStore`] 的冷却/RPM 窗口）：重启后最多再交一次
/// 那笔学费，而落库要为一个纯粹的性能优化摊上一张表与一轮清理。
pub type StaleReasoningMemo = Arc<parking_lot::Mutex<std::collections::VecDeque<(String, i64)>>>;

/// 记忆体的上限。会话有生有灭，这个集合没有别的回收时机，只能靠先进先出顶住。
///
/// 满了顶掉最老的那条：被顶掉的会话若还活着，下一轮再交一次学费、重新记上——退化成没有记忆
/// 的行为，而不是出错。查找是一次线性扫（几百个 32 字符的键比一次哈希分配还便宜），故不为它
/// 再搭一个 HashSet。
const STALE_REASONING_MEMO_MAX: usize = 512;

/// 这个会话在这个号上是不是已经吃过一次「解不开」。
fn stale_reasoning_known(
    memo: &StaleReasoningMemo,
    session_key: Option<&str>,
    cred_id: i64,
) -> bool {
    let Some(key) = session_key.filter(|k| !k.is_empty()) else { return false };
    memo.lock().iter().any(|(k, id)| *id == cred_id && k == key)
}

/// 记下「这个会话在这个号上解不开」。没有会话键就不记：那时落点本来就无从固定，记了也没有
/// 谁能对上号。
fn note_stale_reasoning(memo: &StaleReasoningMemo, session_key: Option<&str>, cred_id: i64) {
    let Some(key) = session_key.filter(|k| !k.is_empty()) else { return };
    let mut memo = memo.lock();
    if memo.iter().any(|(k, id)| *id == cred_id && k == key) {
        return;
    }
    while memo.len() >= STALE_REASONING_MEMO_MAX {
        memo.pop_front();
    }
    memo.push_back((key.to_owned(), cred_id));
}

/// 把 `tools[]` 按名字排定序，返回**顺序是否真的动过**。
///
/// 为什么要排：工具定义**连同顺序**都进上游的 prompt cache 前缀（见 [`prefix_parts`]
/// 里那份官方口径）。客户端每轮把工具列表顺序打乱一次，就是每轮 100% 未命中——而且指纹跟着
/// 变、落点也换，两头一起丢。排一遍就把一个不稳定的客户端强行稳住。
///
/// 为什么**默认不排**（见 [`store::DEFAULT_NORMALIZE_TOOL_ORDER`]）：这不是上游的硬约束，
/// 而是我们替客户端做的决定。官方 codex CLI 的工具顺序本来就是固定的，那时排序一分不赚，
/// 却让发上去的数组顺序成了官方客户端永远不会产生的那一种。工具顺序对模型也不是完全无意义
/// （它是弱优先级暗示），静默改掉不合适。
///
/// 排序键是「名字 + 整条序列化」两级：
/// - 名字取 `name`（函数工具）→ `server_label`（MCP）→ `type`（内建工具没有名字）。
/// - 同名或都没名字时按整条序列化定序。只按名字排的话，两条同名工具的相对次序仍随客户端
///   发来的顺序变，那等于没排。
pub fn normalize_tool_order(obj: &mut serde_json::Map<String, serde_json::Value>) -> bool {
    let Some(serde_json::Value::Array(tools)) = obj.get_mut("tools") else { return false };
    if tools.len() < 2 {
        return false;
    }
    let sort_key = |t: &serde_json::Value| {
        let ident = ["name", "server_label", "type"]
            .iter()
            .find_map(|k| t.get(*k).and_then(|v| v.as_str()))
            .unwrap_or_default()
            .to_owned();
        (ident, t.to_string())
    };
    let keys: Vec<(String, String)> = tools.iter().map(&sort_key).collect();
    // 已经是有序的就什么都不做：那时重排是空动作，而**报「动过了」会白白触发一次重新
    // 序列化**——那会把整个 body 的 key 顺序改掉（见 Cargo.toml 里 preserve_order 的注）。
    if keys.windows(2).all(|w| w[0] <= w[1]) {
        return false;
    }
    tools.sort_by_cached_key(sort_key);
    true
}

/// 一条请求的会话身份，加上给缓存未命中归因所需的那点上下文。
struct SessionCtx {
    /// 实际用的会话键：客户端自报的会话头优先，没有就是前缀指纹。
    key: Option<String>,
    /// 请求**进来那一刻**这个键的租约状态。见 [`store::CredentialStore::lease_state`] 上
    /// 那句「必须在选号之前取」。
    lease: store::LeaseState,
    /// 请求**进来那一刻**四段分段哈希与上次比的结果。同上那句「必须在选号之前取」。
    drift: store::PrefixDrift,
    /// 四段分段哈希本身。转发成功后要拿它更新 memo（见 [`store::CredentialStore::note_prefix`]）。
    prefix: Option<PrefixParts>,
    /// `input[]` 有几项。
    input_len: usize,
}

impl SessionCtx {
    fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }
}

/// 这条请求的缓存结局，以及未命中时**为什么**。落进 `usage_logs.cache_reason`。
///
/// 命中率那条曲线只回答得了「低了」。低的原因有好几种，处置完全不同，而它们在曲线上长得
/// 一模一样。这个函数把它们分开：
///
/// - `hit`：上游报了命中的输入 token。
/// - `first_turn`：第一次见这个会话键，而 `input[]` 只有一项——新对话的第一轮，本来就该
///   miss，不是问题。
/// - 第一次见这个键、而 `input[]` 已经好几项：一段多轮对话带着一个从没见过的前缀身份出现。
///   到这里**再看四段分段哈希是哪一段变了**（[`store::PrefixDrift`]），拆成四类:
///   - `model_switched`：换了模型。上游按模型分开存缓存，这次未命中是必然的，没什么可修。
///   - `instructions_changed`：系统提示变了。真凶多半是提示里带了随时间走的东西——工作目录、
///     git 分支、当天日期（日期这条每条对话每天恰好触发一次）。
///   - `tools_changed`：工具集合或顺序动了（MCP 重连之类）。**只有这一类**是工具排序那个开关
///     治得了的（见 [`normalize_tool_order`]）。
///   - `new_conversation`：连这段对话的底子都没见过。真新对话、`codex resume`、上下文压缩，
///     以及 coban 刚重启（memo 在内存里）都落在这里——**这一类是假阳性的去处**，把它与上面
///     三类分开，剩下那三类才干净得能拿来定位。
/// - `rotated`：这个键有租约，但这次没落在租约那个号上——原来那个号在冷却/RPM 满/被停用。
///   上游的 prompt cache 是按账号存的，换号就是从零开始。
/// - `lease_expired`：租约过期了（会话停得太久），落点因此重新算过。
/// - `upstream_cold`：租约有效、落点也没变、前缀身份也没变，上游那边就是没有。要么它自己的
///   缓存过期了，要么这个键被**假共享**了——两条开头完全一样的对话会算出同一个指纹、共用
///   同一个上游 session，交替请求互相踢掉对方（见 [`prefix_parts`] 的注）。
/// - `no_usage`：没嗅探到用量（错误响应、客户端提前断开）。谈不上命中与否。
/// - `unattributed`：租约机制被关掉了（`session_lease_secs = 0`），上面那几类分不出来。
/// - `no_session`：这条请求压根没有会话身份（体里没有 `input`，`models` 那类）。
///
/// **一定回一个值，绝不回 `None`**：这一列留空的含义已经被占掉了——`NULL` 专指
/// 「归因上线之前写下的旧流水」（迁移不回填，见 `store::migrate_cache_reason`）。让活着的
/// 请求也写 NULL，就会把「那时还没有这个功能」和「这条请求没有会话」混成同一桶，而前者是
/// 会自己老化掉的历史包袱、后者是当下的事实，混在一起两个都读不出来。
fn cache_reason(
    key: Option<&str>,
    lease: store::LeaseState,
    drift: store::PrefixDrift,
    input_len: usize,
    cred_id: i64,
    usage: Option<Usage>,
) -> &'static str {
    if key.is_none_or(str::is_empty) {
        return "no_session";
    }
    let Some(u) = usage else { return "no_usage" };
    if u.cached_tokens > 0 {
        return "hit";
    }
    match lease {
        store::LeaseState::Off => "unattributed",
        // 过期与换号同时发生时报过期：那才是根因，换号是它的后果。
        store::LeaseState::Expired(_) => "lease_expired",
        store::LeaseState::Live(id) if id != cred_id => "rotated",
        store::LeaseState::Live(_) => "upstream_cold",
        store::LeaseState::Absent if input_len <= 1 => "first_turn",
        // 走到这里是「多轮对话 + 没见过的键」。总键只说得出「前缀变了」，分段哈希才说得出
        // 是哪一段——而那四种的处置完全不同（一个必然、一个要改客户端、一个有开关可治、
        // 一个压根不是问题）。
        store::LeaseState::Absent => match drift {
            store::PrefixDrift::Model => "model_switched",
            store::PrefixDrift::Instructions => "instructions_changed",
            store::PrefixDrift::Tools => "tools_changed",
            // 四段都对得上，键却没见过：只可能是租约那张表先被 GC 掉了而 memo 还留着。
            // 归到 lease_expired——落点确实重算过，那正是这一类的含义。
            store::PrefixDrift::Same => "lease_expired",
            store::PrefixDrift::NoBaseline => "new_conversation",
        },
    }
}

/// 会话键，**外加四段各自的短哈希**。
///
/// 总键只说得出「前缀变了」。变的是哪一段——换了模型、客户端改了 `instructions`、工具集或
/// 顺序动了、还是压根是另一段对话——揉进一个哈希之后就分不出来了，而这四种的处置完全不同。
/// 分段哈希配上 [`store::PrefixMemo`] 才把「没见过的前缀」从一次猜测变成一次诊断。
///
/// `head`（`input[0]` 的哈希）是这套东西的支点：对话越聊越长，但开头那一项不变，所以在其余
/// 三段变掉、总键因此对不上的时候，还能靠它认出「这是同一段对话」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixParts {
    /// 会话键。**逐字节都是约定**：它决定落点与上游 `session_id`，改一个 bit 就是全池落点
    /// 重算一遍、白丢一轮缓存。测试里钉了固定值防这件事。
    pub key: String,
    pub model: String,
    pub instructions: String,
    pub tools: String,
    pub head: String,
}

impl PrefixParts {
    pub fn segments(&self) -> store::PrefixSegments<'_> {
        store::PrefixSegments {
            head: &self.head,
            model: &self.model,
            instructions: &self.instructions,
            tools: &self.tools,
        }
    }
}

/// 从（已经是 Responses 形状的）请求体里算这条请求的**会话键**与四段分段哈希。
///
/// 上游的 prompt cache 键是「`prompt_cache_key` + 前缀哈希」，而这条后端把
/// `prompt_cache_key` 换成了**从 `session_id` 头派生**的值——实测：body 里传的那个被忽略
/// （响应回显的是另一个 UUID），前缀一字不差、只把 `session_id` 头换掉，17K token 的命中
/// 直接从 94% 归零。所以我们要的不是「对齐上游那个前缀哈希」——那是它自己算的——而是一个
/// 能认出「这是同一个会话」的东西，好让同一个会话固定落在同一个号上（[`store::CredentialStore::select`]）
/// 并对上游呈现同一个 `session_id`（[`Credential::session_id`]）。
///
/// 只取**不随轮次变化**的那几段：`model`、`instructions`、`tools`（含顺序）、`input` 的头
/// 一项。**整段 `input` 绝不能进**——它每轮都在长，哈出来的键每轮都变，等于没有粘性。
/// 官方文档里进入前缀的正是这几类（消息、工具定义与顺序、图片与其顺序、结构化输出 schema）。
///
/// 取 `input` 头一项而不是只取前三样：后者在同一台机器上的所有 codex 会话之间完全相同，
/// 会把所有会话钉到同一个号、同一个 cache key 上（官方建议单个 key 的流量控制在 15
/// 请求/分钟左右）。会话开头那条才是把会话彼此分开的东西，而它整段会话不会被改写——
/// 真实流量 96.2% 的命中率就是这个前缀逐字节稳定的证据。
///
/// 客户端做过历史压缩（改写了开头那条）时指纹会变，落点也跟着变——那时上游的前缀哈希
/// 同样已经变了，缓存本来就丢了，跟着换号不多亏什么。
pub fn prefix_parts(obj: &serde_json::Map<String, serde_json::Value>) -> Option<PrefixParts> {
    use sha2::{Digest, Sha256};

    // 没有 input 就没有会话可言（`models` 那类无体请求走到这里也是这一支）。
    let head_val = obj.get("input")?.as_array()?.first()?;
    let mut h = Sha256::new();
    let mut feed = |label: &[u8], v: Option<&serde_json::Value>| -> String {
        // 序列化而不是取 as_str：`tools` 是结构，嵌套与顺序都要进哈希。
        let bytes = v.and_then(|v| serde_json::to_vec(v).ok());
        // **喂进总哈希的顺序与字节一个都不许动**，理由见 [`PrefixParts::key`]。分段哈希
        // 另起一个 hasher，加它不影响总键。
        h.update(label);
        h.update([0u8]); // 分隔符：不同字段的内容不能因为拼接而串味
        if let Some(b) = &bytes {
            h.update(b);
        }
        h.update([0u8]);

        // 分段哈希也带上标签：否则两段内容恰好相同时会算出同一个值，比对时分不清谁是谁。
        // 取 8 字节——它只用来比「和上次一样吗」。
        let mut seg = Sha256::new();
        seg.update(label);
        seg.update([0u8]);
        if let Some(b) = &bytes {
            seg.update(b);
        }
        crate::credentials::hex_lower(&seg.finalize()[..8])
    };
    let model = feed(b"model", obj.get("model"));
    let instructions = feed(b"instructions", obj.get("instructions"));
    let tools = feed(b"tools", obj.get("tools"));
    let head = feed(b"head", Some(head_val));
    drop(feed);
    // 取前 16 字节：它只用来当键，不做任何密码学承诺。
    Some(PrefixParts {
        key: crate::credentials::hex_lower(&h.finalize()[..16]),
        model,
        instructions,
        tools,
        head,
    })
}

fn upstream_url(path: &str, query: Option<&str>) -> String {
    let path = path.trim_start_matches('/');
    match query.filter(|q| !q.is_empty()) {
        Some(q) => format!("{}/{}?{}", config::UPSTREAM_BASE, path, q),
        None => format!("{}/{}", config::UPSTREAM_BASE, path),
    }
}

/// 构造发往上游的头：来访头去掉逐跳/鉴权项后照抄，再补上这个凭证的身份。
///
/// `fingerprint` 是这条请求的会话键（见 [`prefix_parts`]）。它决定派生出来的
/// `session_id`，而那正是上游 prompt cache 的键——**不能在这里就地从头里取**：绝大多数
/// 客户端不发会话头，那样算出来的指纹恒为空，于是一个号上所有会话共用同一个 cache key。
fn build_forward_headers(
    incoming: &HeaderMap,
    cred: &Credential,
    token: &str,
    fingerprint: &str,
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
    // 会话 id 按账号 + 会话键派生，见 Credential::session_id 的注。
    set(&mut out, "session_id", &cred.session_id(fingerprint));
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

/// 上游错误体里那句**给人看的话**。见过的几种嵌法都认，都取不到就退回截断的原文。
///
/// 单独抽出来是因为原文常常是一整段带回显的 JSON，而排查只需要那一句
/// （`Input must be a list`、`Unsupported parameter: temperature`……）。
fn upstream_message(body: &[u8]) -> String {
    let text = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            ["/error/message", "/detail/message", "/message", "/detail"]
                .iter()
                .find_map(|ptr| v.pointer(ptr).and_then(|m| m.as_str()).map(str::to_owned))
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
    text.chars().take(UPSTREAM_MSG_MAX).collect()
}

/// 一份请求体的**形状**：顶层字段名 + 类型 + 长度，不含任何内容。
///
/// 上游把请求本身判死（400/422）时，这是排查的第一手材料——`Input must be a list` 这类
/// 形状错，看一眼 `input=string(23)` 就定位到了，不必去猜是哪个客户端怎么写的。
///
/// **只报形状不报内容**：请求体里是用户的整段对话，不该为了查一条 400 把它写进日志。
/// 要原文另有一行 `debug`（`RUST_LOG=coban=debug`）。布尔与数字照原样打——`stream=bool(false)`
/// 正是要看的东西，而它们不可能是对话内容；字符串只报字符数，唯独 `model` 例外，那是排查
/// 时最要紧的一个字段，也不可能是用户内容。
///
/// 字段顺序**照客户端发来的原样**（`preserve_order`），不排序：顺序本身就是「这是哪个
/// 客户端」的线索，排一遍等于把它抹掉。
fn body_shape(body: &[u8]) -> String {
    let Ok(serde_json::Value::Object(obj)) = serde_json::from_slice::<serde_json::Value>(body)
    else {
        return format!("<not a JSON object, {} bytes>", body.len());
    };
    obj.iter()
        .map(|(k, v)| match (k.as_str(), v) {
            ("model", serde_json::Value::String(m)) => format!("model={m}"),
            _ => format!("{k}={}", value_shape(v)),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 一个 JSON 值的类型与规模，见 [`body_shape`]。
fn value_shape(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "null".to_owned(),
        serde_json::Value::Bool(b) => format!("bool({b})"),
        serde_json::Value::Number(n) => format!("number({n})"),
        serde_json::Value::String(s) => format!("string({})", s.chars().count()),
        serde_json::Value::Array(a) => format!("array[{}]", a.len()),
        serde_json::Value::Object(o) => format!("object{{{}}}", o.len()),
    }
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

/// 非 2xx 的上游响应交回客户端时的形态。
///
/// Responses 那条原样交回。chat 那条要把错误体重塑成 OpenAI 的 `{"error":{…}}` 信封：
/// 上游的错误形状不止一种（实测见过 `{"error":{…}}`、`{"detail":"Unsupported parameter: …"}`，
/// CDN 的 HTML 拦截页也算一种），而 Chat Completions 客户端只认 `error.message`——拿到别的
/// 形状它显示的是一句空错误，用户看到「请求失败」四个字，指不到病因上。
///
/// 已经是那个形状的原样放过：上游自己的错误体还带 `param`/`code` 这些更细的线索，
/// 重塑一遍只会把它们丢掉。
fn error_passthrough(
    status: StatusCode,
    headers: &wreq::header::HeaderMap,
    bytes: Bytes,
    chat: Option<&ChatMode>,
) -> Response {
    if chat.is_none() || is_openai_error_shape(&bytes) {
        return passthrough(status, headers, bytes);
    }
    let (etype, message) = parse_upstream_error(&bytes);
    let body = error_body(etype.as_deref().unwrap_or("upstream_error"), &truncate(&message));
    let mut resp = passthrough(status, headers, Bytes::from(body));
    // 体换过了，`content-type` 得跟着换——上游那头可能说的是 `text/html`。
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}

/// 这段响应体是不是已经是 OpenAI 的错误信封（`{"error":{"message":"…"}}`）。
fn is_openai_error_shape(bytes: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| v.pointer("/error/message").map(|m| m.is_string()))
        .unwrap_or(false)
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

/// 401 里那些**刷新也救不回来**的说法。命中即当场停用，连那一次强刷都省掉。
///
/// 头一条是实测原话：`Encountered invalidated oauth token for user, failing request`。它说的
/// 是整个授权被作废了（改了密码、撤销了授权、会话被踢），而不是「这个 access_token 过期
/// 了」——拿 refresh_token 再换一次，换回来的是一个同样不被认的 token。
///
/// 省掉那一趟不是为了快：这个号还在池子里，**每一条**落到它身上的请求都要先陪它刷一次、
/// 再撞一次 401，而结局从第一条起就已经定了。
///
/// 只放「上游明说凭证作废」这一类。像 `token expired` 那种不许进——它正是强刷能治好的
/// 那一种，进了这份清单就等于把一个换个 token 就能用的号直接停掉。
const DEAD_AUTH_HINTS: &[&str] = &[
    "invalidated oauth token",
    "invalid_grant",
    "token has been revoked",
    "token was revoked",
    "token is no longer valid",
];

/// 这次 401 的鉴权是不是已经死透了，见 [`DEAD_AUTH_HINTS`]。
fn detect_dead_auth(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    DEAD_AUTH_HINTS.iter().any(|k| text.contains(k))
}

/// 判断这次非 2xx 是不是**一眼可辨**的账号级问题，是则返回原因（入库到 `ban_reason`）。
///
/// 命中这里的一律当场停用、不做任何补救：账号被停/被封不是换个 token 就能好的事。
/// 关键词没命中的 401 走 [`forward_once`] 里那条「先强刷一次 token」的路——那种 401 更
/// 可能只是凭证过期，不该与「这个账号被封了」同等处置。
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

/// 判断这次 400 是不是「上游解不开请求里捎来的加密推理」。命中即由 [`forward_once`] 摘掉那
/// 几项、**同一个号**重发一遍（见 [`strip_encrypted_reasoning`]）。
///
/// 上游的原话：`The encrypted content gAAA…= could not be verified. Reason: Encrypted content
/// could not be decrypted or parsed.`
///
/// 同时要求「主体」与「状态」两类词，同 [`detect_account_error`] 的理由：光看 `encrypted`
/// 会把别的 400 也算进来，而算错一次就是白把客户端的推理上下文摘掉一次。
fn detect_stale_encrypted_content(status: StatusCode, body: &[u8]) -> bool {
    if status != StatusCode::BAD_REQUEST {
        return false;
    }
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    text.contains("encrypted content")
        && ["could not be verified", "could not be decrypted", "could not be parsed"]
            .iter()
            .any(|k| text.contains(k))
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

/// 这条 429 是不是「额度用尽」（`usage_limit_reached`）。
///
/// 与突发限流的区别只有一条，但那一条决定了处置方式：**恢复时刻在几小时甚至几天之后**。
/// 那种号打一个进程内冷却是不够的——
/// - 冷却表在内存里，重启就没了，那个号立刻回到候选里再撞一次墙；
/// - 页面上它显示成「冷却中，还有 N 秒」，而 N 是个五位数，读起来像是出了故障；
/// - 冷却本来的用途是「几十秒后再来试试」，用它表达「这个号这周用完了」是名不副实。
///
/// 所以这一类改走 [`CredentialStore::pause_for_rate_limit`]：按恢复时刻**落库**暂停，
/// 到点由选号那头惰性放回池子（`resume_due`），界面上显示「限流暂停，X 时自动恢复」，
/// 也不算作需要人处理的账号。突发限流仍走冷却——几十秒的事，落库反而太重。
///
/// 判据取上游自己给的 `type`，同时认那句 message。2026-08 实测原话：
/// `{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached",
/// "plan_type":"pro","resets_at":…,"resets_in_seconds":438570}}`。两个都认是因为只认一个的话，
/// 上游哪天改掉其中之一，这个功能就悄悄没了——而表现是那个号每分钟回来撞一次墙。
fn detect_usage_limit(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    text.contains("usage_limit_reached") || text.contains("usage limit has been reached")
}

/// 额度用尽的号该暂停到多久之后（秒）。
///
/// 四级取值：`retry-after` 头 → 体里的恢复提示（`resets_in_seconds`）→ **这次响应带的额度
/// 快照**（`x-codex-*-reset-at`）→ 保守的固定值。
///
/// 第三级是这一类独有的：额度用尽那种 429 常常不给 `retry-after`，而它**照样带那组限流头**
/// （见 [`config`] 里 `RL_PRIMARY_RESET_AT` 那条注），于是体里没写恢复时刻时还能从头里读到。
/// 全都取不到才退回固定值——猜一个错的恢复时刻，要么让号提前放出来继续撞墙，要么把它多关
/// 几个小时。
fn usage_limit_pause_secs(
    headers: &wreq::header::HeaderMap,
    body: &[u8],
    quota: &QuotaSnapshot,
) -> i64 {
    retry_after_secs(headers)
        .or_else(|| reset_hint_secs(body))
        .or_else(|| quota.secs_until_reset())
        .unwrap_or(QUOTA_PAUSE_FALLBACK_SECS)
}

/// 撞 429 之后这个号该冷却多久（秒）。
///
/// 三级取值：`retry-after` 头 → 体里的恢复提示（见 [`reset_hint_secs`]）→ 设置里的固定值。
///
/// 中间那一级是必须的：额度用尽那种 429（`usage_limit_reached`）**常常不给
/// `retry-after`**，恢复时刻只写在体里，而固定值默认 60 秒——对一个几小时后才回血的号来说
/// 太短，它一分钟后就回到候选里，把后面每条请求的换号次数又耗在同一个号上一次。
fn rate_limit_cooldown(state: &AppState, headers: &wreq::header::HeaderMap, body: &[u8]) -> i64 {
    retry_after_secs(headers).or_else(|| reset_hint_secs(body)).unwrap_or_else(|| {
        state.store.get_setting_i64(store::COOLDOWN_SECS, store::DEFAULT_COOLDOWN_SECS)
    })
}

/// 限流体里可能写着的「多久之后恢复」字段名。
///
/// 上游把它塞在哪一层是会变的（见过 `detail`、`error`、顶层三种），所以按**键名**在整个
/// 体里找，而不是钉死几条 JSON 路径——钉死的话上游换一层嵌套就等于这个功能悄悄没了，
/// 表现是号池明明有好号、客户端却隔一分钟被拒一次。
const RESET_HINT_KEYS: &[&str] =
    &["resets_in_seconds", "reset_after_seconds", "retry_after_seconds", "retry_after"];

/// 在一段错误体里找恢复提示（秒）。
///
/// 深度封顶是防着上游哪天把整段对话回显在错误体里：那时递归的代价与体的大小成正比，
/// 而要找的字段从来只在最外面几层。
fn reset_hint_secs(body: &[u8]) -> Option<i64> {
    fn secs(v: &serde_json::Value) -> Option<i64> {
        // 字符串写法（`"resets_in_seconds":"3600"`）也认：同一个字段两种写法都见过，
        // 只认数字的话取不到就退回那个太短的固定值，而那正是要修的毛病。
        v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
            .map(|n| n.ceil() as i64)
            .filter(|n| *n > 0)
    }
    fn walk(v: &serde_json::Value, depth: u8) -> Option<i64> {
        if depth == 0 {
            return None;
        }
        match v {
            serde_json::Value::Object(m) => m
                .iter()
                .find_map(|(k, val)| {
                    RESET_HINT_KEYS.contains(&k.as_str()).then(|| secs(val)).flatten()
                })
                .or_else(|| m.values().find_map(|val| walk(val, depth - 1))),
            serde_json::Value::Array(a) => a.iter().find_map(|val| walk(val, depth - 1)),
            _ => None,
        }
    }
    walk(&serde_json::from_slice::<serde_json::Value>(body).ok()?, 6)
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
    let secs = quota.secs_until_reset().unwrap_or(QUOTA_PAUSE_FALLBACK_SECS);
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
    /// 这一行已经判定不用再攒了（不可能带用量，或超了上限），丢到行尾为止。
    ///
    /// **必须丢到换行**：把半行的残渣留在 `pending` 里，下一段字节会被接在一个 JSON
    /// 中途的位置上，之后整条流的行边界全错位。
    skipping: bool,
    /// 最后一次看到的用量。取最后一个而不是第一个：`response.completed` 才是终值，
    /// 中途的 `response.in_progress` 也可能带 usage，但那是不完整的读数。
    pub usage: Option<Usage>,
    pub model: Option<String>,
    /// 首字节时刻（相对转发开始）。
    pub first_byte: Option<Instant>,
}

/// 一次响应报告的 token 用量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: i64,
    pub cached_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    pub total_tokens: i64,
}

impl Usage {
    /// 从 Responses 的 `usage` 对象里读数。
    ///
    /// 嗅探（落库计价）与 chat 聚合（回给客户端的 usage）读的是同一份字段，故只留这一处：
    /// 各写一遍的话，上游哪天改了字段名只会改到其中一处，而两边的数字从此各说各话。
    pub fn from_json(u: &serde_json::Value) -> Self {
        let n = |p: &str| u.pointer(p).and_then(|x| x.as_i64()).unwrap_or(0);
        Self {
            input_tokens: n("/input_tokens"),
            cached_tokens: n("/input_tokens_details/cached_tokens"),
            output_tokens: n("/output_tokens"),
            reasoning_tokens: n("/output_tokens_details/reasoning_tokens"),
            total_tokens: n("/total_tokens"),
        }
    }
}

/// 攒到这个长度还没出现 [`may_carry_usage`] 那两个键名，就放弃这一行。
///
/// 巨行都出在 `response.output_item.done`（推理条目的 `encrypted_content` 是一大段
/// base64）与各种 `*.delta` 上，而它们压根不带 `"model"`/`"usage"`——**正文撞不上这个判据**：
/// 正文是 JSON 字符串，里头的引号是转义的，拼不出带引号的键名。所以这一步筛掉的正是那些
/// 又大又没用的行，而终局事件的 `"model"` 在开头几百字节就出现了，留 256KB 的余量足够
/// 扛住上游调整字段顺序。
const SNIFF_DECIDE_AT: usize = 256 * 1024;

/// 单行 SSE `data:` 的长度硬上限，超过就放弃这一行。
///
/// **不能设成「正常事件行的量级」**：非 lite 的模型（实测 `gpt-5.5`/`gpt-5.4` 的
/// `use_responses_lite` 是 false）会把整段 `output` 连 `encrypted_content` 一起塞在
/// `response.completed` 里，一个长回合的终局事件轻易上 MB。之前这里是 1MB，而流式路径是
/// 按 chunk 喂的——终局事件还没等到换行就先撞上限被清空，于是 `model`/`usage`/`cost`
/// 三样一起变成空，只有非流式那条路（整个体一把喂进来，`while` 先把长行 drain 掉）躲过。
/// 表现是**同一个模型走流式就没有计费，走非流式就有**。
///
/// 现在只有「看着像终局事件」的行才可能攒到这里（见 [`SNIFF_DECIDE_AT`]），所以放宽到
/// 16MB 也不会给并发流各挂一份响应体——真攒到这个数说明上游的形状变了，那时宁可丢一条
/// 记录也不能把内存吃穿，故仍留一道硬墙并记一条 warn。
const MAX_SSE_LINE: usize = 16 * 1024 * 1024;

/// 这一行有没有可能带上我们要的两样东西。
///
/// 嗅探的两处判断（攒到一半要不要放弃、整行到手要不要付一次 JSON 解析）用的是同一个判据，
/// 故只留这一处——各写一遍的话，放弃的条件比解析的条件严，就会出现「攒着不解析」或者更糟的
/// 「该攒的中途丢了」。
fn may_carry_usage(data: &str) -> bool {
    data.contains("\"usage\"") || data.contains("\"model\"")
}

impl UsageSniffer {
    /// 喂一块响应字节。
    pub fn feed(&mut self, chunk: &Bytes) {
        if self.first_byte.is_none() {
            self.first_byte = Some(Instant::now());
        }
        // SSE 是 UTF-8；多字节字符被切在 chunk 边界时 lossy 会产生替换字符，但那只会
        // 出现在正文里，不影响我们要找的 ASCII 结构。
        let text = String::from_utf8_lossy(chunk);
        let mut rest = text.as_ref();
        loop {
            // 上一行还在丢弃中：先跳到它的换行为止。
            if self.skipping {
                match rest.find('\n') {
                    Some(idx) => {
                        self.skipping = false;
                        rest = &rest[idx + 1..];
                    }
                    None => return,
                }
            }
            match rest.find('\n') {
                Some(idx) => {
                    self.pending.push_str(&rest[..idx]);
                    let line = std::mem::take(&mut self.pending);
                    self.consume_line(line.trim_end());
                    rest = &rest[idx + 1..];
                }
                None => {
                    self.pending.push_str(rest);
                    self.check_pending();
                    return;
                }
            }
        }
    }

    /// 半行攒到一定长度后决定还要不要继续攒。
    fn check_pending(&mut self) {
        if self.pending.len() >= SNIFF_DECIDE_AT && !may_carry_usage(&self.pending) {
            self.drop_line();
            return;
        }
        if self.pending.len() > MAX_SSE_LINE {
            tracing::warn!(
                len = self.pending.len(),
                "giving up on an oversized SSE line; this request will have no usage or cost"
            );
            self.drop_line();
        }
    }

    /// 放弃当前这一行：清掉已攒的部分，并记着要把剩下的字节丢到换行为止。
    fn drop_line(&mut self) {
        self.pending.clear();
        self.skipping = true;
    }

    fn consume_line(&mut self, line: &str) {
        let Some(data) = line.strip_prefix("data:") else { return };
        let data = data.trim();
        // 先用一次廉价的子串判断挡掉绝大多数事件行（增量文本），只有可能带 usage 的
        // 才付一次 JSON 解析——解析每一行的话，一次长回复要解析几千次。
        if !may_carry_usage(data) {
            return;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else { return };
        // 事件体形如 {"type":"response.completed","response":{...}}；也兼容裸的响应对象。
        let resp = v.get("response").unwrap_or(&v);
        if let Some(m) = resp.get("model").and_then(|m| m.as_str()) {
            self.model = Some(m.to_owned());
        }
        if let Some(u) = resp.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(Usage::from_json(u));
        }
    }
}

/// 流结束（或客户端断开）时把这次转发落库。
struct UsageLogGuard {
    store: Arc<CredentialStore>,
    sniffer: Arc<parking_lot::Mutex<UsageSniffer>>,
    cred_id: i64,
    cred_label: String,
    /// 这条请求实际用的会话键，以及给归因用的那两项（见 [`cache_reason`]）。
    /// **归因只能在这里算**：它要 `cached_tokens`，而那要等流走完才嗅探得到。
    session_key: Option<String>,
    lease: store::LeaseState,
    drift: store::PrefixDrift,
    input_len: usize,
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
            cache_reason: Some(cache_reason(
                self.session_key.as_deref(),
                self.lease,
                self.drift,
                self.input_len,
                self.cred_id,
                usage,
            )),
            session_id: self.session_key.take(),
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
    session: &SessionCtx,
    status: i64,
    model: Option<String>,
    quota: &QuotaSnapshot,
    started: Instant,
    usage: Option<Usage>,
) {
    // 与流式那条路同一套计价（见 UsageLogGuard::drop）。非流式路径也会带回真实用量——
    // 只在错误路径上是 None——漏了这一步，收拢与 chat 那两条路的花费会永远是空。
    let cost = match (&model, usage) {
        (Some(m), Some(u)) => {
            crate::pricing::estimate_usd(m, u.input_tokens, u.cached_tokens, u.output_tokens)
        }
        _ => None,
    };
    let rec = UsageRecord {
        cred_id: Some(cred.id),
        cred_label: cred.label.clone(),
        cache_reason: Some(cache_reason(
            session.key(),
            session.lease,
            session.drift,
            session.input_len,
            cred.id,
            usage,
        )),
        session_id: session.key.clone(),
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
        cost_usd: cost,
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

// ---------- OpenAI 形态的模型清单 ----------

/// 清单缓存的有效期。
///
/// OpenAI 兼容客户端里有一类每开一个会话就取一次清单，而上游那份几分钟内不会变。缓存太短
/// 等于每次都替客户端跑一趟上游，太长又会让刚上新的模型迟迟不出现。
const MODEL_LIST_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// 取清单最多换几个号。
///
/// 一个号最多等 [`MODEL_LIST_TIMEOUT`]，而这条请求是客户端在打开模型下拉时等着——把所有号
/// 轮一遍等于让它干等一分多钟，那头早就超时了。
const MODEL_LIST_MAX_CREDS: usize = 3;

/// `/v1/models` 的清单缓存：取到的时刻 + 模型 slug 列表。
pub type ModelListCache = Arc<parking_lot::Mutex<Option<(Instant, Vec<String>)>>>;

/// 这条请求是不是「OpenAI 兼容客户端在问有哪些模型」。判据见 [`openai_model_list`] 的注。
fn wants_openai_model_list(path: &str, method: &Method, uri: &Uri) -> bool {
    method == Method::GET
        && path.trim_start_matches('/') == config::MODELS_PATH
        && !uri.query().unwrap_or_default().contains("client_version")
}

/// `GET models` 的 OpenAI 兼容应答；不是这条路径就回 `None`，由转发照旧处理。
///
/// 上游的 `models` 端点讲的是 codex 自己那套：要求带 `client_version` 查询参数（缺了回 400
/// `Field required`），返回 `{"models":[{slug,…}]}`。OpenAI 兼容客户端两头都不对——它不带那个
/// 参数，也只认 `{"object":"list","data":[{id,…}]}`，所以原样透传的结果是一个 400 加一个空的
/// 模型下拉。这一层把两边接上。
///
/// **带 `client_version` 的请求不接管**：那是 codex CLI 在取它自己缓存的那份清单
/// （`~/.codex/models_cache.json`），要的是上游原样的形态，连每个模型几 KB 的
/// `instructions_template` 一起。翻成 OpenAI 形态等于把它要的东西删掉，而这个参数正好是
/// 「谁在问」的可靠判据——上游要求它必须有，所以 codex 那条一定带、别人一定不带。
async fn openai_model_list(
    state: &AppState,
    path: &str,
    method: &Method,
    uri: &Uri,
) -> Option<Response> {
    if !wants_openai_model_list(path, method, uri) {
        return None;
    }

    if let Some(slugs) = cached_model_slugs(state, MODEL_LIST_CACHE_TTL) {
        return Some(model_list_response(&slugs));
    }
    Some(match fetch_model_slugs(state).await {
        Ok(slugs) => {
            *state.models_cache.lock() = Some((Instant::now(), slugs.clone()));
            model_list_response(&slugs)
        }
        // 取不到时把上一份（哪怕已经过期）交出去：回错误的表现就是客户端那个空下拉框，
        // 而几分钟前的清单几乎肯定还是对的。真出了状况，下一条真实请求会如实报出来。
        Err(e) => match cached_model_slugs(state, std::time::Duration::MAX) {
            Some(slugs) => {
                tracing::warn!(
                    error = %format!("{:#}", e.inner()),
                    count = slugs.len(),
                    "could not refresh the model list, serving the cached one"
                );
                model_list_response(&slugs)
            }
            None => {
                tracing::warn!(
                    error = %format!("{:#}", e.inner()),
                    "could not fetch the model list"
                );
                e.into_response()
            }
        },
    })
}

/// 取清单失败的两种成因。
///
/// 分开是因为交给客户端的形状不一样：一个号都挑不出来是 coban 这边的状态（没号、全禁用、
/// 全在冷却），该与转发路径报同一个 503/429；号挑出来了却取不到，才是上游或出站链路的问题。
/// 混成一种的表现是「一台还没添加账号的 coban 报 502 上游错误」——把人引去查网络。
enum ModelListError {
    NoCredential(anyhow::Error),
    Upstream(anyhow::Error),
}

impl ModelListError {
    fn inner(&self) -> &anyhow::Error {
        match self {
            Self::NoCredential(e) | Self::Upstream(e) => e,
        }
    }

    fn into_response(self) -> Response {
        match self {
            // 选号失败的翻译与转发路径共用一份：AllRateLimited 要带 Retry-After。
            Self::NoCredential(e) => select_error_response(&e),
            Self::Upstream(e) => {
                error_response(StatusCode::BAD_GATEWAY, "upstream_error", format!("{e:#}"))
            }
        }
    }
}

/// 缓存里那份清单，`ttl` 之内才算有效（传 [`std::time::Duration::MAX`] 则不论新旧都要）。
fn cached_model_slugs(state: &AppState, ttl: std::time::Duration) -> Option<Vec<String>> {
    let cache = state.models_cache.lock();
    let (at, slugs) = cache.as_ref()?;
    (at.elapsed() < ttl).then(|| slugs.clone())
}

/// 挑个号去取清单，这个号取不到就换下一个。
///
/// 换号的理由与转发那条路一样：清单走的是**这个账号**的出站链路与身份，一个配了坏代理或
/// refresh_token 已废的号取不到，不代表别的号也取不到。
///
/// 空清单当失败：一份取回来是零个模型的清单，缓存下来就是五分钟的空下拉框，还不如换个号
/// 再问一次。
async fn fetch_model_slugs(state: &AppState) -> Result<Vec<String>, ModelListError> {
    let mut tried: Vec<i64> = Vec::new();
    let mut last: Option<anyhow::Error> = None;
    for _ in 0..MODEL_LIST_MAX_CREDS {
        // 取清单与会话无关，不带粘性键：让它按 LRU 轮着问，别把它钉在某个会话的号上。
        let cred = match state.store.select(&tried, None) {
            Ok(c) => c,
            // 挑不出号时，已经试过号的话报那个更贴近真相的错，一次都没试过才报选号本身的错。
            Err(e) => {
                return Err(match last {
                    Some(e) => ModelListError::Upstream(e),
                    None => ModelListError::NoCredential(e),
                });
            }
        };
        tried.push(cred.id);
        // 这里不占 RPM 名额：取清单是个 GET，不产生 token 也不写流水（见 list_models），
        // 拿它去扣一个转发名额等于让客户端的模型下拉挤掉一次真实请求。
        match list_models(state, &cred).await {
            Ok(models) => {
                // 只列 `visibility != hide` 的：`hide` 那些照样调得通，但会被上游解析成
                // 别的模型（见 UpstreamModel::visibility），摆进客户端的下拉里等于列一排
                // 选了不算的名字。字段缺失时保留——上游哪天改了字段名，宁可多列几个也不
                // 要交出一份空清单。设置页的下拉用的是同一条规则。
                let slugs: Vec<String> = models
                    .into_iter()
                    .filter(|m| m.visibility.as_deref() != Some("hide"))
                    .map(|m| m.slug)
                    .collect();
                if slugs.is_empty() {
                    last = Some(anyhow::anyhow!(
                        "credential {} sees no usable model in the upstream list",
                        cred.id
                    ));
                    continue;
                }
                return Ok(slugs);
            }
            Err(e) => {
                tracing::debug!(
                    cred_id = cred.id,
                    error = %format!("{e:#}"),
                    "fetching the model list failed, trying the next credential"
                );
                last = Some(e);
            }
        }
    }
    Err(ModelListError::Upstream(
        last.unwrap_or_else(|| anyhow::anyhow!("no credential could fetch the model list")),
    ))
}

/// 造 `{"object":"list","data":[{id,object,created,owned_by}]}`。
///
/// `created` 报当下：上游不给模型的发布时间，而这个字段有客户端会显示——留 0 的话那里
/// 写着 1970 年。
fn model_list_response(slugs: &[String]) -> Response {
    let now = crate::credentials::now_secs();
    let data: Vec<serde_json::Value> = slugs
        .iter()
        .map(|id| {
            serde_json::json!({
                "id": id,
                "object": "model",
                "created": now,
                "owned_by": "openai",
            })
        })
        .collect();
    let body = serde_json::to_vec(&serde_json::json!({ "object": "list", "data": data }))
        .unwrap_or_else(|_| error_body("internal_error", "failed to serialize the model list"));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|e| internal_error_plain(&e.to_string()))
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
                    // 冷却是在读体之前按头打的（读体会把响应消费掉，而体可能读不完）。
                    // 体读到了、头又没给 retry-after 时，以体里的恢复提示为准把它顶掉——
                    // 理由同 [`rate_limit_cooldown`]。
                    if status == StatusCode::TOO_MANY_REQUESTS
                        && retry_after.is_none()
                        && let Some(secs) = reset_hint_secs(&bytes)
                    {
                        state.store.note_rate_limited(cred.id, secs);
                    }
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
    // 合成请求没有来访会话，指纹留空——它们也不该去蹭真实会话的 prompt cache。
    let mut headers = build_forward_headers(&HeaderMap::new(), cred, token, "");
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
        // 探测自成一类：它没有会话、也不该在上游那边留下会话，混进「未归因」会让那一桶
        // 看着像是有一批真实请求归不了因。
        cache_reason: Some("probe"),
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
/// 这是同一次请求在非流式下该返回的那个体的**骨架**——id、status、model、usage、各项参数
/// 的回显都在里面，但 `output` 是空的，还要 [`fill_missing_output`] 补一道。取最后一个：
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

/// 给终局对象补上 `output`——**上游那个终局 `response` 的 `output` 是空数组**。
///
/// 实测：同一次请求的流里，正文只存在于 `response.output_text.delta` 与
/// `response.output_item.done` 事件，而 `response.completed` 携带的那个 `response` 对象
/// `"output": []`、`usage` 却记着真实的 output token 数。原样交出去的后果是**每个不开流的
/// Responses 客户端都拿到一个 200 加一句空回答**——而 200 不会触发任何一层重试，客户端那头
/// 看到的是「模型什么都没说」。
///
/// 只在 `output` 缺失或为空时补：上游哪天开始自己填了，这里就一个字都不动。
fn fill_missing_output(resp: &mut serde_json::Value, bytes: &[u8]) {
    let filled =
        resp.get("output").and_then(|o| o.as_array()).is_some_and(|items| !items.is_empty());
    if filled {
        return;
    }
    let items = sse_output_items(bytes);
    // 一条都没收集到（如流在第一个 item 之前就断了）：保持原样，不拿一个空数组去盖掉
    // 上游本来的写法（可能是缺这个字段，也可能是 `null`）。
    if items.is_empty() {
        return;
    }
    if let Some(obj) = resp.as_object_mut() {
        obj.insert("output".to_owned(), serde_json::Value::Array(items));
    }
}

/// 把一段 SSE 里的 `response.output_item.done` 收成 `output` 数组，按 `output_index` 排序。
///
/// **取 `item.done` 而不是自己从 delta 拼**：`item` 是上游给的完整条目，`message`、
/// `reasoning`、`function_call` 各自的形状（id、status、`content` 的注解、推理的
/// `encrypted_content`）都一字不差地带着。照 delta 拼的话，每多一种条目类型就要多猜一次它
/// 的形状，而猜错的表现是客户端拿到一个形状对不上的 `output`——比空数组更难查。
///
/// 同一个 `output_index` 出现两次时后到的排在后面（`sort_by_key` 是稳定排序）；没带
/// `output_index` 的按到达顺序排在当前末尾。
fn sse_output_items(bytes: &[u8]) -> Vec<serde_json::Value> {
    const DONE: &str = "response.output_item.done";
    let text = String::from_utf8_lossy(bytes);
    let mut items: Vec<(i64, serde_json::Value)> = Vec::new();
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else { continue };
        let data = data.trim();
        // 同 [`sse_final_response`]：先用一次廉价的子串判断挡掉增量事件，一次长回复的
        // delta 有上千行，逐行解 JSON 是白花的 CPU。
        if !data.contains(DONE) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else { continue };
        if v.get("type").and_then(|t| t.as_str()) != Some(DONE) {
            continue;
        }
        let Some(item) = v.get("item").filter(|i| i.is_object()) else { continue };
        let idx = v.get("output_index").and_then(|i| i.as_i64()).unwrap_or(items.len() as i64);
        items.push((idx, item.clone()));
    }
    items.sort_by_key(|(idx, _)| *idx);
    items.into_iter().map(|(_, item)| item).collect()
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

    /// 限流换号**不能被那个小预算卡住**：撞满额的号已经被排掉并打了冷却，继续换才能把
    /// 池子里还能用的号找出来。卡住的后果就是客户端拿到一个「其实还有好号闲着」的 429。
    #[test]
    fn rate_limited_credentials_do_not_spend_the_retry_budget() {
        let mut b = RotationBudget::new(2, true);
        // 默认预算是 2，但限流可以一路换下去（真正的上限是 MAX_ATTEMPTS）。
        for _ in 0..MAX_ATTEMPTS * 2 {
            assert!(b.allows(Reject::RateLimited));
        }
        // 链路故障仍旧只有 retry_max 次：那类失败慢、又不会自己收敛。
        assert!(b.allows(Reject::Upstream));
        assert!(b.allows(Reject::Upstream));
        assert!(!b.allows(Reject::Upstream));
        // 上游预算花光了也不影响继续换限流的号——各类额度各记各的。
        assert!(b.allows(Reject::RateLimited));
    }

    /// `0` 是那个明确的关闭开关：一次都不换，上游的判决原样交回客户端。
    #[test]
    fn a_zero_budget_still_means_no_rotation_at_all() {
        let mut b = RotationBudget::new(0, true);
        assert!(!b.allows(Reject::RateLimited));
        assert!(!b.allows(Reject::Credential));
        assert!(!b.allows(Reject::Upstream));
        // 负值（历史脏数据）按 0 算，不能变成「无限换」。
        let mut b = RotationBudget::new(-3, true);
        assert!(!b.allows(Reject::RateLimited));
    }

    /// 关掉换号开关：撞限流一个号都不换（等待改在 [`RateLimitWait`] 里就地做），而链路
    /// 故障照旧换——那与「这个号满没满」无关，跟着一起关掉只会让一条本可以打通的请求
    /// 白白失败。
    #[test]
    fn turning_off_rate_limit_rotation_still_rotates_on_upstream_failures() {
        let mut b = RotationBudget::new(2, false);
        assert!(!b.allows(Reject::RateLimited));
        // 坏号不在开关管辖之内：不换只会让一个被封的号把每条请求都拖死。
        assert!(b.allows(Reject::Credential));
        assert!(b.allows(Reject::Upstream));
        assert!(b.allows(Reject::Upstream));
        assert!(!b.allows(Reject::Upstream));
        // 限流那一类始终是关的，不会因为上游预算花光而变。
        assert!(!b.allows(Reject::RateLimited));
    }

    /// 就地等的两条边界：次数用完就不再等，以及**上游说要等的比上限还长就当场放弃**
    /// ——那种 429 多半是额度用尽（几小时后才回血），挂着等只会让客户端自己先超时。
    #[test]
    fn waiting_in_place_is_bounded_by_both_the_count_and_the_ceiling() {
        let mut w = RateLimitWait { left: 2, max_wait: 60 };
        assert_eq!(w.allows(30), Some(Duration::from_secs(30)));
        // 等得比上限久：不等，也**不扣**额度——这条请求就此把 429 交回客户端。
        assert_eq!(w.allows(438_570), None);
        assert_eq!(w.left, 1);
        assert_eq!(w.allows(60), Some(Duration::from_secs(60)));
        assert_eq!(w.allows(1), None);

        // 开着换号开关时额度恒为 0：撞 429 立刻换号，一秒都不等。
        let mut off = RateLimitWait { left: 0, max_wait: 60 };
        assert_eq!(off.allows(1), None);
    }

    /// 额度用尽那种 429 不该只打个进程内冷却：恢复时刻在几小时甚至几天之后，而冷却重启即失、
    /// 界面上还显示成一个五位数的「还有 N 秒」。它该按恢复时刻落库暂停，到点自己回池子。
    #[test]
    fn a_usage_limit_429_is_told_apart_from_a_burst_rate_limit() {
        // 上游 2026-08 的原话，一字未改。
        let exhausted = br#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","plan_type":"pro","resets_at":1787743750,"resets_in_seconds":438570}}"#;
        assert!(detect_usage_limit(exhausted));
        // message 那半边单独也认得出来：只认 type 的话，上游改个字这功能就悄悄没了。
        assert!(detect_usage_limit(br#"{"detail":"The usage limit has been reached"}"#));

        // 突发限流不能被误判成额度用尽——那会把一个几十秒就回血的号关上一刻钟。
        assert!(!detect_usage_limit(
            br#"{"error":{"type":"rate_limit_exceeded","message":"Too many requests"}}"#
        ));
        assert!(!detect_usage_limit(b""));

        // 暂停时长按体里的恢复提示走（这里是 5 天多），而不是那个 15 分钟的兜底。
        let none = wreq::header::HeaderMap::new();
        let empty = QuotaSnapshot::default();
        assert_eq!(usage_limit_pause_secs(&none, exhausted, &empty), 438_570);
        // 体里什么都没写时退回固定值：猜一个错的恢复时刻，要么提前放出来继续撞墙、
        // 要么白关几个小时。
        assert_eq!(
            usage_limit_pause_secs(&none, br#"{"error":{"type":"usage_limit_reached"}}"#, &empty),
            QUOTA_PAUSE_FALLBACK_SECS
        );
    }

    /// 额度用尽那种 429 常常不给 `retry-after`，恢复时刻只写在体里。取不到它就退回默认的
    /// 60 秒，那个号一分钟后回到候选里，再把下一条请求的换号次数耗在它身上一次。
    #[test]
    fn the_cooldown_falls_back_to_the_reset_hint_in_the_body() {
        // 上游 2026-08 的原话，一字未改：额度用满那种 429 **没有 `retry-after` 头**，
        // 恢复时刻只写在 `error.resets_in_seconds` 里（这里是 5 天）。少了这一级，那个号
        // 60 秒后就回到候选，把后面每条请求的换号次数又耗在它身上一次。
        let real = br#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","plan_type":"pro","resets_at":1787743750,"eligible_promo":null,"resets_in_seconds":438570}}"#;
        assert_eq!(reset_hint_secs(real), Some(438570));
        // 同一段体走判定器：它是限流，不是账号级错误——判成后者会把一个只是用满了的号
        // 直接停用，而它几天后自己就回血了。
        assert!(detect_account_error(StatusCode::TOO_MANY_REQUESTS, real).is_none());

        // 见过的三种嵌法都要认出来。
        assert_eq!(
            reset_hint_secs(
                br#"{"detail":{"type":"usage_limit_reached","resets_in_seconds":7200}}"#
            ),
            Some(7200)
        );
        assert_eq!(
            reset_hint_secs(
                br#"{"error":{"message":"limit reached","reset_after_seconds":"900"}}"#
            ),
            Some(900)
        );
        assert_eq!(reset_hint_secs(br#"{"retry_after":1.2}"#), Some(2), "秒数向上取整");
        // 没有提示、不是 JSON、提示不是个正数：都交给上层退回配置里的固定值。
        assert!(reset_hint_secs(br#"{"detail":"You have hit your usage limit."}"#).is_none());
        assert!(reset_hint_secs(b"<html>attention required</html>").is_none());
        assert!(reset_hint_secs(br#"{"resets_in_seconds":0}"#).is_none());
        assert!(reset_hint_secs(br#"{"resets_in_seconds":"soon"}"#).is_none());
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
        normalize_responses_body(path, Bytes::from(b.to_owned()), false)
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

    /// 上游拒收的参数一定要在这里丢掉：带上去是每条请求都 400（而 400 不换号重试，等于这个
    /// 接入方彻底不可用）。清单逐个实测过，回的都是 `Unsupported parameter: <名字>`。
    #[test]
    fn unsupported_parameters_are_dropped_on_responses() {
        let pin = |b: &str| -> serde_json::Value {
            serde_json::from_slice(&norm("responses", b).body).unwrap()
        };
        let v = pin(r#"{"model":"gpt-5.4","store":false,"stream":true,
                "temperature":0.7,"top_p":0.9,"presence_penalty":0.1,"frequency_penalty":0.1,
                "seed":1,"stop":["x"],"user":"u","metadata":{"a":"b"},"logit_bias":{"1":1},
                "logprobs":true,"top_logprobs":2,"top_k":5,"safety_identifier":"s",
                "truncation":"auto","max_tool_calls":3,"max_output_tokens":64}"#);
        for k in UNSUPPORTED_PARAMS {
            assert!(v.get(*k).is_none(), "`{k}` 不该出现在发往上游的体里");
        }
        // 丢一堆字段不能顺带把这条路上的硬约束与客户端别的参数搅了。
        assert_eq!(v["store"], false);
        assert_eq!(v["stream"], true);
        assert_eq!(v["model"], "gpt-5.4");
        // 与 store/stream 的改写正交：漏传那两项时同样要丢。
        let v = pin(r#"{"model":"gpt-5.4","temperature":0.7,"reasoning":{"effort":"high"}}"#);
        assert!(v.get("temperature").is_none());
        assert_eq!(v["store"], false);
        assert_eq!(v["stream"], true);
        assert_eq!(v["reasoning"]["effort"], "high");
        // 客户端本来要不要流不受影响。
        assert!(!norm("responses", r#"{"stream":true,"temperature":0.7}"#).collapse);
    }

    /// 实测能过的那些**一个都不许动**：多丢一个字段就是悄悄改掉了客户端的语义，而这条路径的
    /// 接入方是 codex CLI 本身，被丢掉的可能正是它要的东西。
    #[test]
    fn the_parameters_the_upstream_accepts_are_left_alone() {
        let raw = r#"{"model":"gpt-5.4","store":false,"stream":true,
            "instructions":"be brief","input":[{"role":"user","content":"hi"}],
            "tools":[{"type":"function","name":"shell"}],"tool_choice":"auto",
            "parallel_tool_calls":true,"reasoning":{"effort":"high","summary":"auto"},
            "text":{"verbosity":"low"},"include":["reasoning.encrypted_content"],
            "prompt_cache_key":"k1","stream_options":{"include_obfuscation":false},
            "previous_response_id":"resp_1"}"#;
        let n = norm("responses", raw);
        // 该丢的一个都没有、store/stream 也已经对：连重新序列化都不该发生（字段顺序本身就是
        // 「中间有没有代理」的指纹）。
        assert_eq!(n.body, Bytes::from(raw.to_owned()), "没东西可丢时不该重新序列化");
        // `previous_response_id` 刻意留着：这条路径 store 被钉成 false，会话不在上游，丢掉是
        // 让模型看不见前面几轮却照样答——静默答错比一句明确的 400 难查得多。
        let v: serde_json::Value = serde_json::from_slice(&n.body).unwrap();
        assert_eq!(v["previous_response_id"], "resp_1");
    }

    /// `n`/`background`/`service_tier`：带着默认值来等于没提要求，丢掉没有语义损失；非默认值
    /// 是客户端真的在要求什么，那时交给上游那句 400，不替它静默改掉。
    #[test]
    fn parameters_that_are_only_dropped_at_their_default_value() {
        let pin = |b: &str| -> serde_json::Value {
            serde_json::from_slice(&norm("responses", b).body).unwrap()
        };
        let v = pin(r#"{"model":"m","store":false,"stream":true,
                "n":1,"background":false,"service_tier":"auto"}"#);
        assert!(v.get("n").is_none());
        assert!(v.get("background").is_none());
        assert!(v.get("service_tier").is_none());
        // 非默认值原样带过去：`n:2` 是真要两条，`background:true` 是真要异步任务。
        let v = pin(r#"{"model":"m","store":false,"stream":true,
                "n":2,"background":true,"service_tier":"flex"}"#);
        assert_eq!(v["n"], 2);
        assert_eq!(v["background"], true);
        assert_eq!(v["service_tier"], "flex");
        // 形状不对（`n:1.0`）也不敢当默认值处置，一样交给上游判。
        assert_eq!(pin(r#"{"model":"m","store":false,"stream":true,"n":1.0}"#)["n"], 1.0);
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
        assert_eq!(normalize_responses_body("models", raw.clone(), false).body, raw);
        // 前导斜杠仍要认出是 responses。
        let out = normalize_responses_body("/responses", raw.clone(), false).body;
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["store"], false);
        assert_eq!(v["stream"], true);
        // 解不动的体原样放过，不在这里替上游拦。
        let junk = Bytes::from_static(b"not json");
        assert_eq!(normalize_responses_body("responses", junk.clone(), false).body, junk);
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

    /// 终局对象的 `output` 是空的（上游实测就这样），得从 `output_item.done` 补回来——
    /// 否则每个不开流的 Responses 客户端都拿到一个 200 加一句空回答。
    #[test]
    fn an_empty_output_is_refilled_from_the_item_events() {
        let sse = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,",
            "\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"content\":[]}}\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"Hi\"}\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,",
            "\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",",
            "\"content\":[{\"type\":\"output_text\",\"text\":\"Hi!\"}]}}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",",
            "\"status\":\"completed\",\"output\":[],\"usage\":{\"output_tokens\":7}}}\n",
        );
        let mut v = sse_final_response(sse.as_bytes()).unwrap();
        fill_missing_output(&mut v, sse.as_bytes());
        // 补的是上游给的完整条目，不是自己从 delta 拼的一个形状。
        assert_eq!(v["output"][0]["id"], "msg_1");
        assert_eq!(v["output"][0]["type"], "message");
        assert_eq!(v["output"][0]["role"], "assistant");
        assert_eq!(v["output"][0]["content"][0]["text"], "Hi!");
        // 骨架上的别的字段一个不动。
        assert_eq!(v["id"], "resp_1");
        assert_eq!(v["status"], "completed");
        assert_eq!(v["usage"]["output_tokens"], 7);
        // `added` 那条（`content` 还是空的）不能被当成条目收进去。
        assert_eq!(v["output"].as_array().unwrap().len(), 1);
    }

    /// 补 `output` 是**兜底**：上游哪天自己填了，或者一条都收集不到，都不许乱动。
    #[test]
    fn refilling_output_only_happens_when_it_is_actually_missing() {
        let item = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,",
            "\"item\":{\"id\":\"msg_2\",\"type\":\"message\"}}\n"
        );
        // 上游自己填了：一个字不动。
        let mut v = serde_json::json!({ "output": [{ "id": "upstream" }] });
        fill_missing_output(&mut v, item.as_bytes());
        assert_eq!(v["output"][0]["id"], "upstream");
        // 一条都收集不到：保持原样，不拿空数组去盖掉上游本来的写法。
        let mut v = serde_json::json!({ "status": "completed" });
        fill_missing_output(&mut v, b"data: {\"type\":\"response.output_text.delta\"}\n");
        assert!(v.get("output").is_none());
    }

    /// 条目按 `output_index` 归位：推理与正文分两条出来时，顺序错了客户端读到的就是倒着的
    /// 一段对话。
    #[test]
    fn output_items_are_ordered_by_their_index() {
        let sse = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":1,",
            "\"item\":{\"id\":\"msg_1\",\"type\":\"message\"}}\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,",
            "\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\"}}\n",
        );
        let items = sse_output_items(sse.as_bytes());
        assert_eq!(items[0]["id"], "rs_1", "index 0 的排在前面，哪怕它后到");
        assert_eq!(items[1]["id"], "msg_1");
        // 裸的同名字符串骗不过它（子串预判之后还有一次 type 校验）。
        let fake = "data: {\"type\":\"response.output_text.delta\",\
                    \"delta\":\"response.output_item.done\"}\n";
        assert!(sse_output_items(fake.as_bytes()).is_empty());
    }

    /// 线格式按来访路径分道：chat 的要翻译并改打 `responses`，其余走钉字段那条。
    #[test]
    fn the_wire_format_is_decided_by_the_incoming_path() {
        let chat_body = Bytes::from_static(
            br#"{"model":"m","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
        );
        // 两种路径写法（`/v1/chat/completions` 与根上的 `/chat/completions`）都要认出来。
        for path in ["chat/completions", "/chat/completions"] {
            let n = plan_request(path, chat_body.clone(), false).expect("translates");
            assert!(n.chat.is_some(), "{path} 应走 chat 翻译");
            assert!(!n.collapse, "客户端要了流就照流回");
            let v: serde_json::Value = serde_json::from_slice(&n.body).unwrap();
            assert!(v.get("messages").is_none(), "翻完的体里不该再有 chat 的字段");
            assert_eq!(v["input"][0]["role"], "user");
        }
        // 客户端没要流 → 与 responses 那条路同一个 collapse 语义。
        let n = plan_request(
            "chat/completions",
            Bytes::from_static(br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#),
            false,
        )
        .unwrap();
        assert!(n.collapse);

        // responses 那条不受影响，也不会被当成 chat。
        let n = plan_request("responses", Bytes::from_static(br#"{"model":"m"}"#), false).unwrap();
        assert!(n.chat.is_none());
        // 形状错误在这一层就拒，不送去上游换一句指不到原因的 400。
        assert!(plan_request("chat/completions", Bytes::from_static(b"{}"), false).is_err());
    }

    /// 只有「OpenAI 兼容客户端问模型」那一种请求才接管，codex CLI 那条必须放过去。
    #[test]
    fn only_the_openai_shaped_model_request_is_intercepted() {
        let want = |p: &str, m: Method, q: &str| {
            let uri: Uri = format!("http://x/v1/{p}{q}").parse().unwrap();
            wants_openai_model_list(p, &m, &uri)
        };
        assert!(want("models", Method::GET, ""));
        // 前导斜杠仍要认出是 models。
        assert!(want("/models", Method::GET, ""));
        // codex CLI 取的是它自己那份缓存清单（要 instructions_template），不能翻。
        assert!(!want("models", Method::GET, "?client_version=0.148.0"));
        // 别的路径、别的方法都不是这条路。
        assert!(!want("responses", Method::GET, ""));
        assert!(!want("models", Method::POST, ""));
    }

    /// 交出去的必须是 `{"object":"list","data":[…]}`——客户端只认这个形状，别的形状
    /// 表现就是一个空的模型下拉。
    #[tokio::test]
    async fn the_model_list_is_openai_shaped() {
        let resp = model_list_response(&["gpt-5.4-codex".to_owned(), "gpt-5.4".to_owned()]);
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get(header::CONTENT_TYPE).unwrap(), "application/json");

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["object"], "list");
        assert_eq!(v["data"][0]["id"], "gpt-5.4-codex");
        assert_eq!(v["data"][0]["object"], "model");
        assert_eq!(v["data"][1]["id"], "gpt-5.4");
        assert!(v["data"][0]["created"].as_i64().unwrap() > 0, "留 0 会被显示成 1970 年");
    }

    /// 会话键的**固定值**回归。
    ///
    /// 这个键决定落点与上游 `session_id`，改一个 bit 就是全池落点重算一遍、每段在跑的对话
    /// 各丢一次整前缀。分段哈希是后来加的，加它时总键必须一字节不动——这三个值就是那次
    /// 重构之前抄下来的。**任何让它们变的改动都得先想清楚代价**，别顺手更新期望值。
    #[test]
    fn the_session_key_never_changes_value() {
        assert_eq!(
            fp(r#"{"model":"m","instructions":"i","tools":[{"name":"t"}],"input":[{"role":"user","content":"hi"}]}"#).unwrap(),
            "7dfebc4da2fb14c16d11e8edbd8231ca"
        );
        assert_eq!(
            fp(r#"{"model":"m","input":[{"role":"user","content":"hi"}]}"#).unwrap(),
            "bf835d352a263ceb261c102605d7d4ab"
        );
        assert_eq!(fp(r#"{"input":[1]}"#).unwrap(), "a056ce87976b4301b4a3d2538f630262");
    }

    /// 分段哈希各认各的那一段：只动一段，就只有那一段的哈希变。
    #[test]
    fn each_segment_hash_tracks_only_its_own_field() {
        let parts = |body: &str| {
            let serde_json::Value::Object(obj) = serde_json::from_str(body).unwrap() else {
                panic!("object")
            };
            prefix_parts(&obj).unwrap()
        };
        let base = parts(
            r#"{"model":"m","instructions":"i","tools":[{"name":"t"}],"input":[{"role":"user","content":"hi"}]}"#,
        );
        let other_model = parts(
            r#"{"model":"OTHER","instructions":"i","tools":[{"name":"t"}],"input":[{"role":"user","content":"hi"}]}"#,
        );
        assert_ne!(base.model, other_model.model);
        assert_eq!(base.instructions, other_model.instructions);
        assert_eq!(base.tools, other_model.tools);
        assert_eq!(base.head, other_model.head, "换模型不该动这段对话的身份");

        let other_tools = parts(
            r#"{"model":"m","instructions":"i","tools":[{"name":"OTHER"}],"input":[{"role":"user","content":"hi"}]}"#,
        );
        assert_ne!(base.tools, other_tools.tools);
        assert_eq!(base.model, other_tools.model);
        assert_eq!(base.instructions, other_tools.instructions);

        // **head 在对话长大时不能变**：这是整套诊断的支点——其余三段变掉、总键因此对不上时，
        // 还得靠它认出「这是同一段对话」。
        let grown = parts(
            r#"{"model":"m","instructions":"i","tools":[{"name":"t"}],
                "input":[{"role":"user","content":"hi"},{"role":"assistant","content":"ok"}]}"#,
        );
        assert_eq!(base.head, grown.head);
        assert_eq!(base.key, grown.key);

        // 两段内容恰好相同时也不能撞：分段哈希各自带标签。
        let same =
            parts(r#"{"model":"x","instructions":"x","input":[{"role":"user","content":"hi"}]}"#);
        assert_ne!(same.model, same.instructions, "内容相同的两段不该算出同一个哈希");
    }

    /// 测试里只关心总键那一段（落点与上游 session_id 都跟着它走）。
    fn key(n: &Normalized) -> Option<&str> {
        n.prefix.as_ref().map(|p| p.key.as_str())
    }

    fn fp(body: &str) -> Option<String> {
        let serde_json::Value::Object(obj) = serde_json::from_str(body).unwrap() else {
            panic!("object")
        };
        prefix_parts(&obj).map(|p| p.key)
    }

    /// 指纹**必须在会话长大时保持不变**——整个粘性机制就架在这一条上：codex 每轮把历史
    /// 全量重传，如果指纹跟着变，落点与 session_id 每轮都变，缓存等于没有。
    #[test]
    fn the_fingerprint_survives_a_growing_conversation() {
        let turn1 = r#"{"model":"gpt-5.6-sol","instructions":"base prompt",
            "tools":[{"type":"function","name":"shell"}],
            "input":[{"role":"user","content":[{"type":"input_text","text":"first"}]}]}"#;
        // 第二轮：开头那条一字不变，后面又接了两项（助手回复 + 新提问）。
        let turn2 = r#"{"model":"gpt-5.6-sol","instructions":"base prompt",
            "tools":[{"type":"function","name":"shell"}],
            "input":[{"role":"user","content":[{"type":"input_text","text":"first"}]},
                     {"role":"assistant","content":[{"type":"output_text","text":"ok"}]},
                     {"role":"user","content":[{"type":"input_text","text":"second"}]}]}"#;
        assert_eq!(fp(turn1), fp(turn2));
        assert!(fp(turn1).is_some());
    }

    /// 前缀里任何一样变了，指纹就该变——上游那头的缓存也正是在这几样上失效的
    /// （官方文档：消息、工具定义与**顺序**、图片与其顺序、结构化输出 schema 都进前缀）。
    #[test]
    fn the_fingerprint_moves_when_the_prefix_moves() {
        let base = fp(r#"{"model":"m","instructions":"i","tools":[{"name":"a"},{"name":"b"}],
            "input":[{"role":"user","content":"one"}]}"#);
        let cases = [
            // 换模型：上游的缓存本来就不跨模型。
            r#"{"model":"OTHER","instructions":"i","tools":[{"name":"a"},{"name":"b"}],
                "input":[{"role":"user","content":"one"}]}"#,
            r#"{"model":"m","instructions":"OTHER","tools":[{"name":"a"},{"name":"b"}],
                "input":[{"role":"user","content":"one"}]}"#,
            // 只是把两个工具换了个顺序：官方明说顺序进前缀。
            r#"{"model":"m","instructions":"i","tools":[{"name":"b"},{"name":"a"}],
                "input":[{"role":"user","content":"one"}]}"#,
            // 开头那条被改写（客户端做了历史压缩）。
            r#"{"model":"m","instructions":"i","tools":[{"name":"a"},{"name":"b"}],
                "input":[{"role":"user","content":"OTHER"}]}"#,
        ];
        for c in cases {
            assert_ne!(base, fp(c), "前缀变了指纹却没变: {c}");
        }
        // 没有 input 就没有会话可言（`models` 那类无体请求走的也是这一支）。
        assert!(fp(r#"{"model":"m","instructions":"i"}"#).is_none());
        assert!(fp(r#"{"model":"m","input":[]}"#).is_none());
    }

    /// 归因的每一条分支都要走到：这几类在命中率曲线上长得一模一样，而处置完全不同。
    #[test]
    fn cache_reason_separates_the_ways_a_miss_can_happen() {
        use store::LeaseState;
        const A: i64 = 7;
        const B: i64 = 9;
        let usage = |cached: i64| {
            Some(Usage {
                input_tokens: 100_000,
                cached_tokens: cached,
                output_tokens: 1,
                reasoning_tokens: 0,
                total_tokens: 100_001,
            })
        };
        use store::PrefixDrift as D;
        // 大部分分支与前缀漂移无关，用 NoBaseline 走过场；漂移那几支单独在下一个测试里。
        let r = |lease, input_len, usage| {
            cache_reason(Some("k"), lease, D::NoBaseline, input_len, A, usage)
        };

        // 命中优先于一切：上游都报了命中，前面那些原因就不必再猜。
        assert_eq!(r(LeaseState::Absent, 1, usage(90_000)), "hit");
        assert_eq!(r(LeaseState::Live(A), 9, usage(90_000)), "hit");

        assert_eq!(r(LeaseState::Absent, 1, usage(0)), "first_turn");
        // 同样是「没见过这个键」，输入已经好几项就不是新对话，是前缀在变。
        assert_eq!(r(LeaseState::Absent, 9, usage(0)), "new_conversation");
        assert_eq!(r(LeaseState::Live(B), 9, usage(0)), "rotated");
        assert_eq!(r(LeaseState::Live(A), 9, usage(0)), "upstream_cold");
        assert_eq!(r(LeaseState::Off, 9, usage(0)), "unattributed");

        // 过期又换了号时报过期：那是根因，换号是它的后果。
        assert_eq!(r(LeaseState::Expired(A), 9, usage(0)), "lease_expired");
        assert_eq!(r(LeaseState::Expired(B), 9, usage(0)), "lease_expired");

        // 没有用量读数谈不上命中与否。
        assert_eq!(r(LeaseState::Live(A), 9, None), "no_usage");

        // 没有会话身份的请求自成一类，**不能回 None**：那一列留空专指升级前的旧流水，
        // 让活着的请求也写 NULL 就把两件事混成了一桶。
        assert_eq!(
            cache_reason(None, LeaseState::Absent, D::NoBaseline, 0, A, usage(0)),
            "no_session"
        );
        assert_eq!(
            cache_reason(Some(""), LeaseState::Absent, D::NoBaseline, 0, A, usage(0)),
            "no_session"
        );
    }

    /// 「没见过的键 + 多轮对话」这一支再按分段漂移拆成四类——这才是把一次猜测变成一次诊断
    /// 的地方，四类的处置完全不同。
    #[test]
    fn a_never_seen_key_is_diagnosed_by_which_segment_drifted() {
        use store::{LeaseState, PrefixDrift as D};
        let usage = Some(Usage {
            input_tokens: 100_000,
            cached_tokens: 0,
            output_tokens: 1,
            reasoning_tokens: 0,
            total_tokens: 100_001,
        });
        let r = |drift| cache_reason(Some("k"), LeaseState::Absent, drift, 9, 7, usage);

        assert_eq!(r(D::Model), "model_switched");
        assert_eq!(r(D::Instructions), "instructions_changed");
        assert_eq!(r(D::Tools), "tools_changed");
        assert_eq!(r(D::NoBaseline), "new_conversation");
        // 四段都没变而键没见过：租约表先被 GC 了，落点确实重算过。
        assert_eq!(r(D::Same), "lease_expired");

        // 漂移只在这一支起作用：第一轮仍然是 first_turn，租约还在时仍然按租约那几类走。
        assert_eq!(
            cache_reason(Some("k"), LeaseState::Absent, D::Instructions, 1, 7, usage),
            "first_turn"
        );
        assert_eq!(
            cache_reason(Some("k"), LeaseState::Live(7), D::Instructions, 9, 7, usage),
            "upstream_cold"
        );
    }

    /// 排 tools 的三件事：顺序被排定、指纹**跟着排完的顺序算**、已经有序时不动手。
    #[test]
    fn tool_order_normalization_stabilizes_both_order_and_fingerprint() {
        let shuffled = br#"{"model":"m","instructions":"i",
            "tools":[{"type":"function","name":"write"},{"type":"function","name":"apply_patch"},
                     {"type":"web_search"}],
            "input":[{"role":"user","content":"hi"}]}"#;
        // 排完的样子：没名字的内建工具按 `type` 字串一起参与排序，不是被推到最后。
        let sorted = br#"{"model":"m","instructions":"i",
            "tools":[{"type":"function","name":"apply_patch"},{"type":"web_search"},
                     {"type":"function","name":"write"}],
            "input":[{"role":"user","content":"hi"}]}"#;

        let names = |n: &Normalized| -> Vec<String> {
            let v: serde_json::Value = serde_json::from_slice(&n.body).unwrap();
            v["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| {
                    t.get("name").or_else(|| t.get("type")).unwrap().as_str().unwrap().to_owned()
                })
                .collect()
        };

        // 不开时原样放过：这是默认行为，别把「没开」也给排了。
        let off = plan_request("responses", Bytes::from_static(shuffled), false).unwrap();
        assert_eq!(names(&off), vec!["write", "apply_patch", "web_search"]);

        // 开了之后按排序键（名字，没名字则 type）走一遍字典序。
        let on = plan_request("responses", Bytes::from_static(shuffled), true).unwrap();
        assert_eq!(names(&on), vec!["apply_patch", "web_search", "write"]);

        // **排过的那份与本来就有序的那份要算出同一个指纹**——这才是排序的目的：前缀稳了，
        // 落点也不能再跟着客户端那个乱序换。
        let already = plan_request("responses", Bytes::from_static(sorted), true).unwrap();
        assert_eq!(key(&on), key(&already));
        assert_ne!(key(&off), key(&on), "不排的那份指纹本来就该不一样");
    }

    /// 已经有序时不许重新序列化：那会把整个 body 的 key 顺序改掉（见 Cargo.toml 里
    /// preserve_order 的注），而排序在这种输入上本该是个空动作。
    #[test]
    fn an_already_sorted_tool_list_is_left_byte_for_byte_alone() {
        let raw = Bytes::from_static(
            br#"{"model":"m","store":false,"stream":true,"instructions":"i","tools":[{"name":"a","type":"function"},{"name":"b","type":"function"}],"input":[{"role":"user","content":"hi"}]}"#,
        );
        let n = normalize_responses_body("responses", raw.clone(), true);
        assert_eq!(n.body, raw, "有序的输入该一个字节都不动");
    }

    /// 同名工具也要有确定的次序，否则「排过」只是错觉——客户端换个顺序发，落点还是会变。
    #[test]
    fn same_named_tools_still_get_a_deterministic_order() {
        let one = plan_request(
            "responses",
            Bytes::from_static(
                br#"{"model":"m","tools":[{"type":"mcp","server_label":"x","server_url":"b"},
                     {"type":"mcp","server_label":"x","server_url":"a"}],
                     "input":[{"role":"user","content":"hi"}]}"#,
            ),
            true,
        )
        .unwrap();
        let other = plan_request(
            "responses",
            Bytes::from_static(
                br#"{"model":"m","tools":[{"type":"mcp","server_label":"x","server_url":"a"},
                     {"type":"mcp","server_label":"x","server_url":"b"}],
                     "input":[{"role":"user","content":"hi"}]}"#,
            ),
            true,
        )
        .unwrap();
        assert_eq!(key(&one), key(&other), "server_label 撞了也得排得出确定的次序");
    }

    /// 上游把请求本身判死时，日志里那两样东西必须真的指得到病灶：上游那句话，以及
    /// 请求体的形状——而形状里**不能有对话内容**，那是用户的东西。
    #[test]
    fn the_rejected_request_is_described_without_leaking_its_content() {
        // 上游 400 的原话（形状照 `Input must be a list` 那次）。
        let err = br#"{"error":{"message":"Input must be a list","type":"invalid_request_error"}}"#;
        assert_eq!(upstream_message(err), "Input must be a list");
        // 解不出结构就退回原文，别把唯一的线索吞掉。
        assert_eq!(upstream_message(b"upstream is angry"), "upstream is angry");

        // 普通字符串再取字节：byte string 字面量只认 ASCII，而这里要的正是一段中文内容。
        let shape = body_shape(
            r#"{"model":"gpt-5","input":"写一句睡前故事","stream":false,"temperature":0.7,
                "tools":[{"name":"a"},{"name":"b"}],"text":{"verbosity":"low"},"metadata":null}"#
                .as_bytes(),
        );
        // 病灶一眼可见：input 是字符串不是列表。
        assert!(shape.contains("input=string(7)"), "{shape}");
        // 型号照打（排查最要紧的一个字段，且不可能是用户内容），布尔与数字也照打。
        assert!(shape.contains("model=gpt-5"), "{shape}");
        assert!(shape.contains("stream=bool(false)"), "{shape}");
        assert!(shape.contains("temperature=number(0.7)"), "{shape}");
        assert!(shape.contains("tools=array[2]"), "{shape}");
        assert!(shape.contains("text=object{1}"), "{shape}");
        assert!(shape.contains("metadata=null"), "{shape}");
        // **一个字的对话内容都不许出现**。
        assert!(!shape.contains("睡前"), "{shape}");
        // 字段顺序照客户端发来的原样，那本身就是「这是哪个客户端」的线索。
        assert!(shape.starts_with("model=gpt-5 input="), "{shape}");

        // 非 JSON 体不崩，报个长度就够了。
        assert_eq!(body_shape(b"not json at all"), "<not a JSON object, 15 bytes>");
    }

    /// `input` 给一段裸文本是官方 SDK 的头号写法（`client.responses.create(input="hi")`），
    /// 而订阅这条路径只认列表，原样转上去就是一句 `Input must be a list`。
    #[test]
    fn a_bare_string_input_is_wrapped_into_the_list_upstream_wants() {
        let n =
            plan_request("responses", Bytes::from_static(br#"{"model":"m","input":"hi"}"#), false)
                .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&n.body).unwrap();
        assert_eq!(v["input"][0]["role"], "user");
        assert_eq!(v["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(v["input"][0]["content"][0]["text"], "hi");
        // 包完才算指纹与项数：算在包之前的话，认的是一个上游根本不会接受的形状。
        assert_eq!(n.input_len, 1);
        assert!(key(&n).is_some());
        // 顺手确认 store/stream 也一并钉上了——重新序列化那条路要走全。
        assert_eq!(v["store"], false);
        assert_eq!(v["stream"], true);

        // 与 chat 那条路翻出来的用户消息**逐字节同形**：不同形的话，同一段对话走两条线
        // 格式会算出两个指纹，缓存白丢一次。
        let via_chat = plan_request(
            "chat/completions",
            Bytes::from_static(br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#),
            false,
        )
        .unwrap();
        let c: serde_json::Value = serde_json::from_slice(&via_chat.body).unwrap();
        assert_eq!(v["input"], c["input"]);

        // 列表原样放过，别的形状不猜——判 400 是上游的事。
        let untouched = plan_request(
            "responses",
            Bytes::from_static(
                br#"{"model":"m","store":false,"stream":true,"input":{"role":"user"}}"#,
            ),
            false,
        )
        .unwrap();
        assert_eq!(
            &untouched.body[..],
            br#"{"model":"m","store":false,"stream":true,"input":{"role":"user"}}"#
        );
    }

    /// 两条线格式都要报出 `input[]` 的项数——`first_turn` 与「没见过的前缀」那几类全靠它分开。
    #[test]
    fn both_wire_formats_report_the_input_length() {
        let one = plan_request(
            "responses",
            Bytes::from_static(br#"{"model":"m","input":[{"role":"user","content":"hi"}]}"#),
            false,
        )
        .unwrap();
        assert_eq!(one.input_len, 1);

        let grown = plan_request(
            "chat/completions",
            Bytes::from_static(
                br#"{"model":"m","messages":[{"role":"system","content":"s"},{"role":"user","content":"hi"},
                     {"role":"assistant","content":"ok"},{"role":"user","content":"more"}]}"#,
            ),
            false,
        )
        .unwrap();
        // system 进 instructions，不算一项输入；剩下三条才是。
        assert_eq!(grown.input_len, 3);

        // 解不出体（`models` 那类无体请求）时是 0，不是崩。
        assert_eq!(plan_request("models", Bytes::new(), false).unwrap().input_len, 0);
    }

    /// 两种线格式都要拿到指纹，且**同一段对话在 chat 那条路上长大时也不能变**。
    #[test]
    fn both_wire_formats_produce_a_session_key() {
        let n = plan_request(
            "responses",
            Bytes::from_static(
                br#"{"model":"m","instructions":"i","input":[{"role":"user","content":"hi"}]}"#,
            ),
            false,
        )
        .unwrap();
        assert!(key(&n).is_some());

        let turn1 = plan_request(
            "chat/completions",
            Bytes::from_static(
                br#"{"model":"m","messages":[{"role":"system","content":"s"},{"role":"user","content":"hi"}]}"#,
            ),
            false,
        )
        .unwrap();
        let turn2 = plan_request(
            "chat/completions",
            Bytes::from_static(
                br#"{"model":"m","messages":[{"role":"system","content":"s"},{"role":"user","content":"hi"},
                     {"role":"assistant","content":"ok"},{"role":"user","content":"more"}]}"#,
            ),
            false,
        )
        .unwrap();
        assert!(key(&turn1).is_some());
        assert_eq!(key(&turn1), key(&turn2), "chat 那条路上会话长大也不能换落点");
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
        let out = build_forward_headers(&incoming, &cred, "fresh-token", "fp");
        assert_eq!(out.get("authorization").unwrap(), "Bearer fresh-token");
        assert_eq!(out.get("chatgpt-account-id").unwrap(), "acct-9");
        assert_eq!(out.get("originator").unwrap(), config::ORIGINATOR);
        assert_eq!(out.get("content-type").unwrap(), "application/json");
        assert!(out.get("content-length").is_none(), "stale length truncates the body upstream");
        assert!(out.get("x-forwarded-for").is_none(), "would advertise the proxy hop");
        assert!(out.get("session_id").is_some());
    }

    /// 只有「主体 + 状态」两类词同时命中才算**一眼可辨**的账号级问题：那一类当场停用、
    /// 不做补救。一次普通的 token 过期不该走这条路——它先该被强刷一次 token 试试
    /// （见 [`forward_once`] 里那段 401 处置），刷完再撞 401 才停用。
    #[test]
    fn account_error_needs_both_subject_and_state() {
        let banned = br#"{"error":{"message":"Your account has been suspended"}}"#;
        assert!(detect_account_error(StatusCode::FORBIDDEN, banned).is_some());

        let expired = br#"{"error":{"message":"unauthorized: token expired"}}"#;
        assert!(detect_account_error(StatusCode::UNAUTHORIZED, expired).is_none());
        // 那条路停用时写进 ban_reason 的原因：得能指到「上游说了什么」，一句光秃秃的
        // 「401」在页面上等于没说。
        assert_eq!(
            format!("upstream 401: {}", upstream_message(expired)),
            "upstream 401: unauthorized: token expired"
        );

        // 状态码不对就不判，哪怕文本命中。
        assert!(detect_account_error(StatusCode::TOO_MANY_REQUESTS, banned).is_none());
        // 403 也不按 401 那套处置：机器人拦截页就是 403，照 401 处置会把整池一次停光。
        let edge = b"<html><title>Attention Required! | Cloudflare</title></html>";
        assert!(detect_account_error(StatusCode::FORBIDDEN, edge).is_none());
    }

    /// 「授权整个被作废」与「这个 token 过期了」得分开：前者刷新是白刷——换回来的是一个
    /// 同样不被认的 token，而这个号还在池子里，**每一条**落到它身上的请求都要先陪它刷一次
    /// 再撞一次 401，结局从第一条起就定了。
    #[test]
    fn a_dead_oauth_grant_skips_the_refresh_attempt() {
        // 实测原话。
        assert!(detect_dead_auth(b"Encountered invalidated oauth token for user, failing request"));
        // 包在 JSON 里、大小写不一样也得认出来。
        assert!(detect_dead_auth(
            br#"{"error":{"message":"Encountered INVALIDATED OAuth token for user"}}"#
        ));
        assert!(detect_dead_auth(br#"{"error":"invalid_grant"}"#));

        // 这一种恰恰是强刷能治好的，绝不能进那份清单——进了就等于把一个换个 token
        // 就能用的号直接停掉。
        assert!(!detect_dead_auth(br#"{"error":{"message":"unauthorized: token expired"}}"#));
        assert!(!detect_dead_auth(b""));
    }

    /// 上游解不开请求里捎来的加密推理时那句 400 要认出来：认不出来，这段会话从此每一轮都
    /// 400（客户端下一轮还会把同一段密文发回来）。
    #[test]
    fn stale_encrypted_reasoning_is_detected() {
        let stale = br#"{"error":{"message":"The encrypted content gAAAAABn...nZg= could not be verified. Reason: Encrypted content could not be decrypted or parsed."}}"#;
        assert!(detect_stale_encrypted_content(StatusCode::BAD_REQUEST, stale));

        // 状态码不对不判；别的 400 也不判——误判一次就是白把客户端的推理上下文摘掉一次。
        assert!(!detect_stale_encrypted_content(StatusCode::TOO_MANY_REQUESTS, stale));
        let other = br#"{"error":{"message":"Unsupported parameter: temperature"}}"#;
        assert!(!detect_stale_encrypted_content(StatusCode::BAD_REQUEST, other));
    }

    /// 摘掉的只有 reasoning 项：消息、工具调用与结果（含 `call_id` 的配对关系）一项不能动，
    /// 顺手丢掉一个 `function_call_output` 就是把这一轮的工具结果变没了。
    #[test]
    fn stripping_encrypted_reasoning_keeps_everything_else() {
        let body = Bytes::from(
            r#"{"model":"gpt-5.4","include":["reasoning.encrypted_content"],"input":[
                {"type":"message","role":"user","content":"hi"},
                {"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"gAAAA"},
                {"type":"function_call","name":"shell","call_id":"c1","arguments":"{}"},
                {"type":"function_call_output","call_id":"c1","output":"ok"}
            ]}"#
            .to_owned(),
        );
        let fixed = strip_encrypted_reasoning(&body).expect("有 reasoning 项就该改写");
        let v: serde_json::Value = serde_json::from_slice(&fixed).unwrap();
        let input = v["input"].as_array().unwrap();
        assert_eq!(input.len(), 3, "只该少掉那一项 reasoning");
        assert!(!input.iter().any(|i| i["type"] == "reasoning"));
        assert!(!String::from_utf8_lossy(&fixed).contains("gAAAA"), "密文一个字节都不该留下");
        assert_eq!(input[2]["call_id"], "c1", "工具结果与它的配对关系原样留着");
        // 回程那个 include 刻意留着：往后的密文由现在这个号产，下一轮还用得上。
        assert_eq!(v["include"][0], "reasoning.encrypted_content");

        // 里面根本没有密文时返回 None：那说明这条 400 是别的原因，重发一遍白发一次。
        let clean = Bytes::from(
            r#"{"input":[{"type":"message","role":"user","content":"hi"}]}"#.to_owned(),
        );
        assert!(strip_encrypted_reasoning(&clean).is_none());
        // 非 JSON / 没有 input 的体一样不重发。
        assert!(strip_encrypted_reasoning(&Bytes::from_static(b"not json")).is_none());
    }

    /// 记忆按「会话 + 号」两件一起认，且有上限：只按会话认会把同一段会话在别的号上也预摘
    /// 掉（那个号本来解得开自己产的密文），没有上限则一个长跑进程会被过期的会话键撑起来。
    #[test]
    fn the_stale_reasoning_memo_is_keyed_by_both_and_bounded() {
        let memo = StaleReasoningMemo::default();
        note_stale_reasoning(&memo, Some("sess-a"), 1);
        assert!(stale_reasoning_known(&memo, Some("sess-a"), 1));
        assert!(!stale_reasoning_known(&memo, Some("sess-a"), 2), "换个号是另一件事");
        assert!(!stale_reasoning_known(&memo, Some("sess-b"), 1), "换段会话是另一件事");
        // 没有会话键时不记也不认：那时落点本来就无从固定。
        note_stale_reasoning(&memo, None, 1);
        note_stale_reasoning(&memo, Some(""), 1);
        assert!(!stale_reasoning_known(&memo, None, 1));
        assert_eq!(memo.lock().len(), 1, "同一条记两遍也只占一个位置（重复记不该堆积）");
        note_stale_reasoning(&memo, Some("sess-a"), 1);
        assert_eq!(memo.lock().len(), 1);

        for i in 0..STALE_REASONING_MEMO_MAX * 2 {
            note_stale_reasoning(&memo, Some(&format!("sess-{i}")), 1);
        }
        assert!(memo.lock().len() <= STALE_REASONING_MEMO_MAX);
        // 顶掉的是最老的那条：最近这些还在，被顶掉的会话若还活着，下一轮再学一次。
        let last = format!("sess-{}", STALE_REASONING_MEMO_MAX * 2 - 1);
        assert!(stale_reasoning_known(&memo, Some(&last), 1));
        assert!(!stale_reasoning_known(&memo, Some("sess-a"), 1));
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
    ///
    /// 钉的是**硬墙那一道**：这一行带着 `"model"`，所以躲过了 [`SNIFF_DECIDE_AT`] 那一层
    /// （见 `sniffer_drops_a_giant_useless_line_and_resyncs`），只剩 [`MAX_SSE_LINE`] 拦它。
    #[test]
    fn sniffer_caps_the_pending_buffer() {
        let mut s = UsageSniffer::default();
        s.feed(&Bytes::from("data: {\"model\":\"gpt-5.5\",\"x\":\"".to_owned()));
        let mb = "x".repeat(1024 * 1024);
        for _ in 0..(MAX_SSE_LINE / mb.len() + 2) {
            s.feed(&Bytes::from(mb.clone()));
            assert!(s.pending.len() <= MAX_SSE_LINE + mb.len(), "{}", s.pending.len());
        }
        assert!(s.pending.is_empty(), "撞了硬墙就该把这一行丢掉");
    }

    /// **非 lite 的模型把整段 output 塞在终局事件里，那一行能有好几 MB**——它必须照样被读出
    /// 用量。这里是真出过的那个 bug：上限 1MB 时，流式（按 chunk 喂）的终局事件还没等到换行
    /// 就被清空，于是 model/usage/cost 三样一起空，而非流式（整个体一把喂）却是好的。所以
    /// 这条测试钉的是「两条路读出来的必须一样」。
    #[test]
    fn sniffer_keeps_usage_from_a_giant_completed_event() {
        // 形状照实测：encrypted_content 是一大段 base64，`model` 在 `output` 之前，
        // `usage` 在最后。
        let blob = "A".repeat(3 * 1024 * 1024);
        let line = format!(
            "data: {{\"type\":\"response.completed\",\"response\":{{\"model\":\"gpt-5.5\",\
             \"output\":[{{\"type\":\"reasoning\",\"encrypted_content\":\"{blob}\"}}],\
             \"usage\":{{\"input_tokens\":1000,\"input_tokens_details\":{{\"cached_tokens\":900}},\
             \"output_tokens\":50,\"output_tokens_details\":{{\"reasoning_tokens\":40}},\
             \"total_tokens\":1050}}}}}}\n"
        );

        // 流式：按 64KB 分块喂（真实 chunk 就是这个量级）。
        let mut streamed = UsageSniffer::default();
        for c in line.as_bytes().chunks(64 * 1024) {
            streamed.feed(&Bytes::from(c.to_vec()));
        }
        // 非流式：整个体一把喂。
        let mut whole = UsageSniffer::default();
        whole.feed(&Bytes::from(line.clone().into_bytes()));

        assert_eq!(streamed.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(streamed.usage, whole.usage, "流式与非流式必须读出同一份用量");
        let u = streamed.usage.expect("流式路径也要拿到用量");
        assert_eq!(
            (u.input_tokens, u.cached_tokens, u.output_tokens, u.reasoning_tokens, u.total_tokens),
            (1000, 900, 50, 40, 1050)
        );
    }

    /// 巨行里真正没用的那些（`output_item.done` 的密文、各种 delta）要早早放弃，且**放弃之后
    /// 得跳到行尾**——把半行残渣留在缓冲里，后面的字节会接在 JSON 中途，从此整条流的行边界
    /// 全错位，终局事件也就跟着读不到了。
    #[test]
    fn sniffer_drops_a_giant_useless_line_and_resyncs() {
        let mut s = UsageSniffer::default();
        let big = format!(
            "data: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"reasoning\",\
             \"encrypted_content\":\"{}\"}}}}\n",
            "A".repeat(4 * 1024 * 1024)
        );
        let chunk = 64 * 1024;
        for c in big.as_bytes().chunks(chunk) {
            s.feed(&Bytes::from(c.to_vec()));
            assert!(
                s.pending.len() <= SNIFF_DECIDE_AT + chunk,
                "没用的巨行不该越攒越多：{}",
                s.pending.len()
            );
        }
        // 丢完那一行之后，下一行照样要读得出来。
        s.feed(&Bytes::from(
            "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5.5\",\
             \"usage\":{\"input_tokens\":9,\"output_tokens\":2}}}\n"
                .to_owned(),
        ));
        assert_eq!(s.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(s.usage.map(|u| u.input_tokens), Some(9));
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
