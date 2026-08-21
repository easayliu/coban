//! 用 rust-embed 把 `admin-ui/dist` 前端构建产物内嵌进二进制并提供静态服务。
//!
//! SPA fallback + 按路径设置缓存策略。

use axum::{
    body::Body,
    http::{Response, StatusCode, Uri, header},
    response::{IntoResponse, Redirect},
};
use rust_embed::Embed;

/// 内嵌前端构建产物（编译期从 `admin-ui/dist` 读取）。
#[derive(Embed)]
#[folder = "admin-ui/dist"]
struct Asset;

/// 把误发到首页的 POST 文档导航转换为 GET，避免浏览器刷新时要求重新提交表单。
pub async fn redirect_root_post() -> Redirect {
    Redirect::to("/")
}

/// 作为整个应用的 fallback：命中静态资源则返回，否则 SPA fallback 到 index.html。
/// （`/api/*` 与 `/v1/*` 由主路由先行匹配，不会走到这里。）
pub async fn fallback(uri: Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');

    if path.contains("..") {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("Invalid path"))
            .expect("build response");
    }

    if let Some(content) = Asset::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream().to_string();
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime)
            .header(header::CACHE_CONTROL, cache_control(path))
            .body(Body::from(content.data.into_owned()))
            .expect("build response");
    }

    // 非资源路径（无扩展名）→ SPA fallback 到 index.html。
    if !is_asset_path(path) {
        return serve_index();
    }

    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::from("Not found"))
        .expect("build response")
}

fn serve_index() -> Response<Body> {
    match Asset::get("index.html") {
        Some(content) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(content.data.into_owned()))
            .expect("build response"),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("Frontend not built yet. Run `pnpm build` in the admin-ui directory."))
            .expect("build response"),
    }
}

/// 带 hash 的 `assets/*` 可以永久缓存；html 必须 no-cache，否则发了新版用户还拿着旧壳。
fn cache_control(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "no-cache"
    } else if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    }
}

fn is_asset_path(path: &str) -> bool {
    path.rsplit('/').next().map(|f| f.contains('.')).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        http::{Method, Request},
        routing::get,
    };
    use tower::ServiceExt;

    fn app() -> Router {
        Router::new()
            .route("/", get(fallback).post(redirect_root_post))
            .fallback_service(get(fallback))
    }

    /// SPA 路由（无扩展名）要兜底到 index.html，否则刷新子页面就是 404。
    #[tokio::test]
    async fn spa_fallback_serves_unknown_get_route() {
        let response = app()
            .oneshot(Request::builder().uri("/settings").body(Body::empty()).unwrap())
            .await
            .unwrap();
        // dist 还没构建时也不该 500——退化成一句「先构建前端」的 404。
        assert!(matches!(response.status(), StatusCode::OK | StatusCode::NOT_FOUND));
    }

    /// SPA 只允许由 GET/HEAD 打开。把 POST 也兜底成 index.html 的话，浏览器会把页面记作
    /// 表单提交结果，之后在移动端刷新便弹出「确认重新提交表单」。
    #[tokio::test]
    async fn root_post_redirects_instead_of_rendering() {
        let response = app()
            .oneshot(Request::builder().method(Method::POST).uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers().get(header::LOCATION).unwrap(), "/");
    }

    #[tokio::test]
    async fn path_traversal_is_rejected() {
        let response = app()
            .oneshot(Request::builder().uri("/../Cargo.toml").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_ne!(response.status(), StatusCode::OK);
    }
}
