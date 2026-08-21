//! 管理界面登录鉴权（可选）。
//!
//! 密码以 sha256 存于 SQLite（`admin_password_sha256`），或由 `COBAN_ADMIN_PASSWORD`
//! 环境接管。未设置时中间件放行（本机 dev 友好）；设置后 `/api/*` 管理接口需带
//! `Authorization: Bearer <password>`。转发代理 `/v1/*` 不走这里。

use axum::{
    Json,
    extract::{ConnectInfo, Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store;
use crate::web::AppState;

type ApiError = (StatusCode, String);

/// sha256 十六进制。
pub fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    crate::credentials::hex_lower(&h.finalize())
}

/// 生效的管理密码哈希：环境接管优先，否则用库中存的哈希；都无则 None（未启用鉴权）。
fn admin_hash(state: &AppState) -> Option<String> {
    if let Some(pw) = &state.admin_env {
        return Some(sha256_hex(pw));
    }
    state.store.get_setting(store::ADMIN_PASSWORD).ok().flatten().filter(|s| !s.is_empty())
}

/// 是否已启用管理鉴权（环境接管或库里存了哈希）。
///
/// 给那些「开着鉴权才允许」的接口用：管理接口在未设密码时是**完全敞开**的
/// （见 [`require_admin`]），而个别接口给出去的东西比「能改配置」更重
/// （如导出含明文 token 的迁移文件），它们得自己确认这道门锁着。
pub fn admin_configured(state: &AppState) -> bool {
    admin_hash(state).is_some()
}

/// 中间件：未设密码放行；已设则校验 `Authorization: Bearer <password>`。
pub async fn require_admin(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(hash) = admin_hash(&state) else {
        return next.run(req).await;
    };
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|pw| sha256_hex(pw.trim()) == hash)
        .unwrap_or(false);
    if ok {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "admin password required").into_response()
    }
}

#[derive(Serialize)]
pub struct StateResp {
    /// 是否已设置管理密码（true = 需登录）。
    configured: bool,
    /// 是否由环境变量接管（true = 网页不可改）。
    env_managed: bool,
}

/// 鉴权状态（公开）。
pub async fn state(State(state): State<AppState>) -> Json<StateResp> {
    Json(StateResp {
        configured: admin_hash(&state).is_some(),
        env_managed: state.admin_env.is_some(),
    })
}

#[derive(Deserialize)]
pub struct PwReq {
    password: String,
}

fn ok_json() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    // 错误详情只回给客户端、服务端不留痕的话，500 在日志里查不到。
    let msg = e.to_string();
    tracing::error!(error = %msg, "auth endpoint internal error");
    (StatusCode::INTERNAL_SERVER_ERROR, msg)
}

/// 记进日志的来源标识：优先前置层给的 `x-forwarded-for` 首段（coban 常挂在反代后面，
/// 直接取对端只会得到反代自己），退回 `x-real-ip`，都没有才用 TCP 对端地址。
///
/// 这两个头是客户端可伪造的，所以只当**线索**用，不作任何判决依据；真要防爆破得在
/// 前置层做。
///
/// `peer` 由 `ConnectInfo` 提取，取决于 [`crate::web::run`] 里那句
/// `into_make_service_with_connect_info`——换掉它这几个接口会一律 500。
fn client_ip(headers: &header::HeaderMap, peer: std::net::SocketAddr) -> String {
    let from_header = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .or_else(|| headers.get("x-real-ip").and_then(|v| v.to_str().ok()))
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match from_header {
        Some(ip) => ip.to_owned(),
        None => peer.ip().to_string(),
    }
}

/// 校验密码（供前端登录确认，公开）。
pub async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: header::HeaderMap,
    Json(req): Json<PwReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // 这是全项目唯一能看出「有人在猜管理密码」的地方，不带来源等于记了个寂寞。
    let ip = client_ip(&headers, peer);
    match admin_hash(&state) {
        None => Err((StatusCode::BAD_REQUEST, "no admin password has been set yet".into())),
        Some(h) if sha256_hex(req.password.trim()) == h => {
            tracing::info!(%ip, "admin login succeeded");
            Ok(ok_json())
        }
        _ => {
            tracing::warn!(%ip, "admin login failed: wrong password");
            Err((StatusCode::UNAUTHORIZED, "wrong password".into()))
        }
    }
}

/// 首次设置密码（仅未配置时，公开）。
pub async fn setup(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: header::HeaderMap,
    Json(req): Json<PwReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if admin_hash(&state).is_some() {
        return Err((StatusCode::BAD_REQUEST, "an admin password is already set".into()));
    }
    let pw = req.password.trim();
    if pw.len() < 4 {
        return Err((StatusCode::BAD_REQUEST, "password must be at least 4 characters".into()));
    }
    state.store.set_setting(store::ADMIN_PASSWORD, &sha256_hex(pw)).map_err(internal)?;
    // 这个接口在未设密码时是**公开**的，谁先访问到谁就把密码定下来——比任何一项设置变更
    // 都更该留痕。
    tracing::info!(ip = %client_ip(&headers, peer), "admin password set for the first time");
    Ok(ok_json())
}

/// 修改/清除密码（已鉴权；环境接管时禁止）。空串 = 清除。
pub async fn change_password(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: header::HeaderMap,
    Json(req): Json<PwReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.admin_env.is_some() {
        return Err((
            StatusCode::BAD_REQUEST,
            "the admin password is managed by an environment variable and cannot be changed from the web UI".into(),
        ));
    }
    let pw = req.password.trim();
    let cleared = pw.is_empty();
    if cleared {
        state.store.delete_setting(store::ADMIN_PASSWORD).map_err(internal)?;
    } else {
        if pw.len() < 4 {
            return Err((StatusCode::BAD_REQUEST, "password must be at least 4 characters".into()));
        }
        state.store.set_setting(store::ADMIN_PASSWORD, &sha256_hex(pw)).map_err(internal)?;
    }
    tracing::info!(ip = %client_ip(&headers, peer), cleared, "admin password changed");
    Ok(ok_json())
}
