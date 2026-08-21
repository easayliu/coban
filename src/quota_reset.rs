//! 额度重置券：查还剩几张，以及花一张把额度窗口重置掉。
//!
//! 上游给订阅账号发的一种一次性券（`rate_limit_reset_credit`）。额度撞墙之后兑一张，
//! 5 小时/周窗口当场归零，不用等到重置时刻——对一个多账号池来说，这是「整池都满了」时
//! 唯一能立刻恢复产能的手段。
//!
//! 三条端点与那组头见 [`config::WHAM_BASE`]，来源是 sub2api 的生产实现。分工：
//!
//! - [`query`]：`GET /wham/rate-limit-reset-credits` 要张数与每张的过期时刻，缺张数时
//!   退回 `GET /wham/usage` 里的 `rate_limit_reset_credits.available_count`。
//! - [`consume`]：`POST /wham/rate-limit-reset-credits/consume` 兑一张。
//!
//! **券是花掉就没有的**（上游不退），所以这个模块里凡是「兑换之后」的步骤全部按
//! best-effort 写：复查张数失败、放回轮转失败，都只是日志 + 结果里少一项，绝不能把一次
//! 已经成功的兑换报成失败——那会让人再点一次，第二张券就这么没了。

use std::time::Duration;

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::http::{HeaderName, HeaderValue};

use crate::config;
use crate::credentials::Credential;
use crate::store::ResetCredits;
use crate::web::AppState;

/// 单条 wham 请求（含读完响应体）的上限。
///
/// 这几条都是小 JSON 接口，正常几百毫秒。给 20 秒是留给代理链路，不是留给上游慢——
/// 界面上是个按钮在转圈，等更久不如报出来让人重试。
const TIMEOUT: Duration = Duration::from_secs(20);

/// 错误原文带回前端时截到多少个**字符**（按字节切会把多字节字符劈成半个）。
const ERROR_MAX_CHARS: usize = 240;

/// 一次兑换的结果。
#[derive(Debug, Default, serde::Serialize)]
pub struct ResetOutcome {
    /// 上游给这次兑换的结果码（实测 `success`）。上游没报就是 `None`——不编一个。
    pub code: Option<String>,
    /// 上游报的「这次重置了几个窗口」。
    pub windows_reset: Option<i64>,
    /// 被兑掉那张券的过期时刻（原样转出，不解析）。
    pub credit_expires_at: Option<String>,
    /// 兑换之后重新查的一份张数读数。
    ///
    /// `None` = 复查没成功。**这不代表兑换失败**：券已经花掉了，界面此时该显示的是
    /// 「重置成功、张数待刷新」，而不是回到兑换前的旧数字。
    pub credits: Option<ResetCredits>,
    /// 有没有把这个号从「限流暂停」里放回轮转（`false` = 它本来就没被限流暂停）。
    pub resumed: bool,
}

/// 查这个号还剩几张重置券，并把读数落库。
///
/// **顺带落库是有意的**：张数只能向上游问，而卡片在刷新页面后也要显示它。落一份下来，
/// 界面就与额度快照同一套办法——显示读数 + 读数时刻（见 [`ResetCredits::fetched_at`]），
/// 而不是每次进页面都替所有账号各打一趟上游。
///
/// 不消耗券、不消耗额度、不写用量流水。
pub async fn query(state: &AppState, cred: &Credential) -> Result<ResetCredits> {
    let (client, token) = prepare(state, cred).await?;

    // 先问券清单：它带每张的过期时刻，而 /wham/usage 只有一个总数。
    let listed = fetch(&client, cred, &token, config::WHAM_RESET_CREDITS_PATH, None).await;
    let fallback_reason = match &listed {
        Ok(bytes) => match parse_credits(bytes) {
            Some(credits) => return finish_query(state, cred, credits),
            // 200 但既没张数也没券列表：字段名换过了，或者这个账号压根没有这个东西。
            // 两种都值得再问一次 usage——那边的字段名是另一个。
            None => "the credit list had no readable count".to_owned(),
        },
        Err(e) => format!("{e:#}"),
    };

    // 退回 /wham/usage：只拿得到总数（没有过期时刻），但总数才是「还能重置几次」这个问题
    // 的答案，缺过期时刻只是提示里少一行。
    let bytes = fetch(&client, cred, &token, config::WHAM_USAGE_PATH, None)
        .await
        .with_context(|| format!("the credit list also failed: {fallback_reason}"))?;
    let available_count = parse_usage_count(&bytes).with_context(|| {
        format!(
            "the upstream reported no reset-credit count on either endpoint \
             (credit list: {fallback_reason})"
        )
    })?;
    finish_query(state, cred, ResetCredits::new(available_count, Vec::new()))
}

/// 兑一张券，把这个号的额度窗口重置掉。
///
/// 兑换成功后还做两件事，都是 best-effort（见模块头）：
///
/// 1. **把号放回轮转**：解除限流暂停 + 清掉冷却。额度重置了却还在暂停里，等于花了一张券
///    什么也没换到——sub2api 那边同样在兑换后接一次 `RecoverAccountState`（见其
///    `openai_oauth_handler.go` 的 step 1，理由也一样）。人工停用与封号**不碰**。
/// 2. **复查张数**并落库，让界面上的数字跟着降下来。
pub async fn consume(state: &AppState, cred: &Credential) -> Result<ResetOutcome> {
    let (client, token) = prepare(state, cred).await?;

    // 幂等键每次现生成：上游按它去重，写死或复用一个就等于第二次点「重置」什么也不发生。
    let redeem_request_id = random_uuid();
    let body = serde_json::json!({ "redeem_request_id": redeem_request_id });
    let body = serde_json::to_vec(&body).context("failed to build the redeem request body")?;
    let bytes =
        fetch(&client, cred, &token, config::WHAM_RESET_CONSUME_PATH, Some(body.into())).await?;

    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    let mut outcome = ResetOutcome {
        code: str_field(&v, "code"),
        windows_reset: v.get("windows_reset").and_then(serde_json::Value::as_i64),
        credit_expires_at: v.get("credit").and_then(|c| str_field(c, "expires_at")),
        ..Default::default()
    };
    tracing::info!(
        cred_id = cred.id, label = %cred.label,
        code = %outcome.code.as_deref().unwrap_or("-"),
        windows_reset = outcome.windows_reset.unwrap_or(-1),
        "redeemed a quota reset credit"
    );

    // 券已经花掉了，从这里往下的失败一律只记日志。
    match state.store.resume_if_rate_limited(cred.id) {
        Ok(resumed) => outcome.resumed = resumed,
        Err(e) => tracing::error!(
            cred_id = cred.id, error = %e,
            "redeemed the credit but could not lift the rate-limit pause"
        ),
    }
    state.store.clear_cooldown(cred.id);

    outcome.credits = match query(state, cred).await {
        Ok(credits) => Some(credits),
        Err(e) => {
            tracing::warn!(
                cred_id = cred.id, error = %format!("{e:#}"),
                "redeemed the credit but re-reading the remaining count failed"
            );
            None
        }
    };
    Ok(outcome)
}

/// 落库 + 返回。张数每次取到就覆盖：这不是「上游只报了一部分」的那种快照
/// （对比 [`crate::store::QuotaSnapshot::filled_from`]），一次响应说的就是全部。
fn finish_query(
    state: &AppState,
    cred: &Credential,
    credits: ResetCredits,
) -> Result<ResetCredits> {
    if let Err(e) = state.store.save_reset_credits(cred.id, &credits) {
        // 落库失败不影响这次回答——只是下次进页面得再问一趟上游。
        tracing::warn!(
            cred_id = cred.id, error = %format!("{e:#}"),
            "could not cache the reset-credit reading"
        );
    }
    tracing::info!(
        cred_id = cred.id, label = %cred.label,
        available = credits.available_count, expiries = credits.expires_at.len(),
        "read the reset-credit count"
    );
    Ok(credits)
}

/// 取客户端与 access_token。
///
/// **不给取 token 套 timeout**：刷新会轮换 refresh_token，中途取消就把号废了
/// （理由详见 [`crate::proxy`] 的 `probe_token`）。请求本身各自有超时。
async fn prepare(
    state: &AppState,
    cred: &Credential,
) -> Result<(std::sync::Arc<wreq::Client>, String)> {
    let client = state.clients.for_credential(cred)?;
    let token = state.store.valid_access_token(&state.clients, cred).await?;
    Ok((client, token))
}

/// 发一条 wham 请求并读完响应体。`body` 为 `Some` 时是 POST。
async fn fetch(
    client: &wreq::Client,
    cred: &Credential,
    token: &str,
    path: &str,
    body: Option<Bytes>,
) -> Result<Bytes> {
    let url = format!("{}/{}", config::WHAM_BASE, path.trim_start_matches('/'));
    let method = if body.is_some() { wreq::Method::POST } else { wreq::Method::GET };
    let mut req = client.request(method, &url).headers(wham_headers(cred, token, body.is_some()));
    if let Some(body) = body {
        req = req.body(body);
    }

    let up = tokio::time::timeout(TIMEOUT, req.send())
        .await
        .with_context(|| format!("{path} timed out (cap {}s)", TIMEOUT.as_secs()))?
        .with_context(|| format!("the request to {path} failed"))?;
    let status = up.status();
    let bytes = tokio::time::timeout(TIMEOUT, up.bytes())
        .await
        .with_context(|| format!("reading the {path} response timed out"))?
        .context("failed to read the upstream response body")?;
    anyhow::ensure!(
        status.is_success(),
        "upstream returned {} for {path}: {}",
        status.as_u16(),
        truncate(&String::from_utf8_lossy(&bytes))
    );
    Ok(bytes)
}

/// wham 那族接口的出站头。
///
/// **不复用转发那份 `build_forward_headers`**：这一族要报桌面端的 `originator` 与
/// `openai-beta`（见 [`config::WHAM_ORIGINATOR`]），而且没有会话，`session_id` 不该出现
/// ——把一个派生的会话 id 带到一个跟会话无关的接口上，只是多一处与真实客户端不同的地方。
///
/// `sec-fetch-*` / `oai-language` / `priority` 这几个**整组照抄 sub2api，不逐个裁剪**：
/// 哪几个是上游真的在看，没法在本机证伪，而裁错一个的表现是 403——与「授权失效」长得
/// 一模一样，会把人引去查 token。UA 仍是 CLI 那份：coban 只有那一种出站形态，为这三条
/// 接口另建一个浏览器指纹的客户端（TLS 指纹也得跟着换）不值得。
fn wham_headers(cred: &Credential, token: &str, post: bool) -> wreq::header::HeaderMap {
    let mut out = wreq::header::HeaderMap::new();
    let mut set = |name: &'static str, v: &str| {
        if let Ok(value) = HeaderValue::from_str(v) {
            out.insert(HeaderName::from_static(name), value);
        }
    };
    set("authorization", &format!("Bearer {token}"));
    // 与 access_token 是一对：上游认的是两件一起，缺任何一半都是 401。
    set("chatgpt-account-id", &cred.account_id);
    set("openai-beta", config::WHAM_OPENAI_BETA);
    set("originator", config::WHAM_ORIGINATOR);
    set("oai-language", "zh-CN");
    set("accept", "application/json");
    set("accept-encoding", config::ACCEPT_ENCODING);
    set("user-agent", config::CODEX_USER_AGENT.as_str());
    set("sec-fetch-site", "none");
    set("sec-fetch-mode", "no-cors");
    set("sec-fetch-dest", "empty");
    set("priority", "u=4, i");
    if post {
        set("content-type", "application/json");
    }
    out
}

/// 券清单响应 → 张数 + 各张的过期时刻。都读不出来时返回 `None`（调用方据此退回 usage）。
///
/// 认三种形态，因为上游这一族的字段名见过不止一种写法，而 sub2api 那边为此打了两个补丁
/// （`fix/openai-reset-credit-malformed-details`、`fix/reset-credit-count-fallback`）：
///
/// - 顶层直接是一个券数组；
/// - 对象里带 `available_count`（数字或**字符串**）；
/// - 对象里带券列表，键见过 `credits` / `rate_limit_reset_credits` / `items` / `data`。
///
/// 张数以上游明说的 `available_count` 为准，它没报时数券列表里「可用」的条数——**只数
/// 属于额度重置那一类且状态是 available 的**：同一个列表里混着别的券种或已兑换的历史时，
/// 全部计数会得出一个偏大的数字，而偏大的表现是界面上有券、点下去上游说没有。
fn parse_credits(bytes: &[u8]) -> Option<ResetCredits> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let declared = v.get("available_count").or_else(|| v.get("availableCount")).and_then(as_count);
    let list = match &v {
        serde_json::Value::Array(items) => Some(items.as_slice()),
        _ => ["credits", "rate_limit_reset_credits", "items", "data"]
            .iter()
            .find_map(|k| v.get(*k).and_then(serde_json::Value::as_array))
            .map(Vec::as_slice),
    };

    let mut counted = 0i64;
    let mut expires_at = Vec::new();
    for item in list.unwrap_or_default() {
        let reset_type = str_field(item, "reset_type").or_else(|| str_field(item, "resetType"));
        if let Some(kind) = &reset_type
            && !kind.eq_ignore_ascii_case("codex_rate_limits")
        {
            continue;
        }
        if let Some(status) = str_field(item, "status")
            && !status.eq_ignore_ascii_case("available")
        {
            continue;
        }
        counted += 1;
        if let Some(exp) = str_field(item, "expires_at").or_else(|| str_field(item, "expiresAt")) {
            expires_at.push(exp);
        }
    }

    match (declared, list.is_some()) {
        (Some(n), _) => Some(ResetCredits::new(n, expires_at)),
        (None, true) => Some(ResetCredits::new(counted, expires_at)),
        (None, false) => None,
    }
}

/// `/wham/usage` 响应 → 重置券张数。
fn parse_usage_count(bytes: &[u8]) -> Option<i64> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let credits = v.get("rate_limit_reset_credits").or_else(|| v.get("rateLimitResetCredits"))?;
    credits.get("available_count").or_else(|| credits.get("availableCount")).and_then(as_count)
}

/// 数字或数字字符串 → 非负整数。负数当没报——一个 `-1` 更像哨兵值而不是「欠着一张」。
fn as_count(v: &serde_json::Value) -> Option<i64> {
    let n = match v {
        serde_json::Value::Number(_) => v.as_i64()?,
        serde_json::Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    (n >= 0).then_some(n)
}

/// 取一个非空的字符串字段（空串与全空白当没有）。
fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// 一个 UUID v4 形态的随机串，用作兑换请求的幂等键。
fn random_uuid() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    crate::credentials::uuid_from_bytes(bytes)
}

fn truncate(s: &str) -> String {
    s.chars().take(ERROR_MAX_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实形态：对象 + `available_count` + 带过期时刻的券列表。
    #[test]
    fn reads_the_count_and_the_expiries() {
        let body = br#"{"available_count":2,"credits":[
            {"id":"a","reset_type":"codex_rate_limits","status":"available","expires_at":"2026-09-01T00:00:00Z"},
            {"id":"b","reset_type":"codex_rate_limits","status":"available","expires_at":"2026-09-08T00:00:00Z"}
        ]}"#;
        let c = parse_credits(body).unwrap();
        assert_eq!(c.available_count, 2);
        assert_eq!(c.expires_at, ["2026-09-01T00:00:00Z", "2026-09-08T00:00:00Z"]);
    }

    /// 上游没报总数时按列表数，但**只数额度重置那一类里状态可用的**：多数出来的那几张
    /// 会变成「界面上有券、点下去上游说没有」。
    #[test]
    fn counts_only_available_rate_limit_credits() {
        let body = br#"[
            {"reset_type":"codex_rate_limits","status":"available","expires_at":"2026-09-01T00:00:00Z"},
            {"reset_type":"codex_rate_limits","status":"redeemed","expires_at":"2026-08-01T00:00:00Z"},
            {"reset_type":"something_else","status":"available"},
            {"status":"available"}
        ]"#;
        let c = parse_credits(body).unwrap();
        assert_eq!(c.available_count, 2, "已兑换的和别的券种都不算");
        assert_eq!(c.expires_at, ["2026-09-01T00:00:00Z"], "只留可用那几张的过期时刻");
    }

    /// 张数写成字符串也认（同一个字段两种写法都见过），负数与非数字当没报。
    #[test]
    fn accepts_a_stringly_typed_count() {
        assert_eq!(
            parse_credits(br#"{"availableCount":"3","items":[]}"#).unwrap().available_count,
            3
        );
        assert_eq!(
            parse_credits(br#"{"available_count":-1,"credits":[]}"#).unwrap().available_count,
            0,
            "负数当没报，退回按列表数（空列表 = 0）"
        );
        assert_eq!(
            parse_credits(br#"{"available_count":"soon","data":[]}"#).unwrap().available_count,
            0
        );
    }

    /// 既没张数也没列表时必须返回 `None`——调用方靠它决定要不要退回 `/wham/usage`。
    /// 报一个编出来的 0 会让界面显示「没券」，而那个号可能有。
    #[test]
    fn unreadable_shapes_do_not_invent_a_zero() {
        assert!(parse_credits(b"{}").is_none());
        assert!(parse_credits(b"<html>attention required</html>").is_none());
        assert!(parse_credits(br#"{"detail":"unauthorized"}"#).is_none());
    }

    /// usage 那条路只拿总数，字段嵌在 `rate_limit_reset_credits` 里。
    #[test]
    fn reads_the_count_from_the_usage_endpoint() {
        let body = br#"{"plan_type":"pro","rate_limit_reset_credits":{"available_count":1}}"#;
        assert_eq!(parse_usage_count(body), Some(1));
        assert_eq!(parse_usage_count(br#"{"rate_limit_reset_credits":null}"#), None);
        assert_eq!(parse_usage_count(br#"{"plan_type":"pro"}"#), None);
    }

    /// 幂等键必须每次都不同：复用一个的话，第二次点「重置」上游会当成重发，
    /// 什么也不做却回一个成功。
    #[test]
    fn redeem_ids_are_fresh_uuids() {
        let (a, b) = (random_uuid(), random_uuid());
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(a.as_bytes()[14], b'4', "version nibble must be 4: {a}");
    }
}
