//! 出站 HTTP 客户端与「逐账号代理」的客户端池。
//!
//! 一个凭证配了代理，它的**全部**出站流量就都得走那个代理——转发、token 刷新、连通性
//! 测试，一个都不能漏。漏一条的后果不是「慢一点」，而是那条请求带着真实出口 IP 打到
//! 上游，逐账号隔离当场失效，且从日志上完全看不出来。故取客户端的入口只有
//! [`ClientPool::for_credential`] 一个。

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::config;
use crate::credentials::Credential;

/// 构造发往上游的 HTTP 客户端。
///
/// `default_headers` 里的 `accept-encoding` **必须显式钉住**：开了解压 feature 后，
/// 底层会给「没带这个头」的请求补一个它自己的取值（顺序与写法都不是官方客户端会产生的）。
///
/// `proxy` 为 `Some` 时挂上代理，其余形态与直连那份**逐字节相同**——代理只改走法，不改
/// 请求本身，否则「配了代理的号」就多出一处与别的号不同的指纹。
pub fn upstream_client(proxy: Option<&str>) -> Result<wreq::Client> {
    use axum::http::{HeaderMap, HeaderValue, header::ACCEPT_ENCODING};

    let mut defaults = HeaderMap::new();
    defaults.insert(ACCEPT_ENCODING, HeaderValue::from_static(config::ACCEPT_ENCODING));

    let builder = wreq::Client::builder()
        .user_agent(config::CODEX_USER_AGENT.as_str())
        .default_headers(defaults)
        // 流式响应要逐块转出去，池里的连接闲置太久会被上游/中间设备静默断掉，
        // 下一次复用表现为一个没有响应体的连接错误。90 秒短于常见的 idle 超时。
        .pool_idle_timeout(std::time::Duration::from_secs(90));
    let builder = match proxy {
        // `Proxy::all` 覆盖 http 与 https 两种目标；上游只有 https，但写 all 才不会因为
        // 哪天多一个 http 目标就悄悄绕开代理。
        Some(url) => {
            // **建之前必须再校验一次**：`Proxy::all` 成功不等于代理会生效（见
            // [`check_proxy_url`]），而库里完全可能存着这样一条（手工改库也能塞进来）。
            // 校验不过就返回 Err，让 [`ClientPool::for_credential`] 把这个号整体判为
            // 不可用；绝不能建出一个「配置里有代理、实际直连」的客户端。
            check_proxy_url(url)?;
            builder
                .proxy(wreq::Proxy::all(url).with_context(|| format!("invalid proxy URL: {url}"))?)
        }
        // 不配代理时**不调用 `.no_proxy()`**：保留默认的环境变量代理探测
        // （HTTPS_PROXY/ALL_PROXY 等），那是全局兜底，与逐账号代理各管一层。
        None => builder,
    };
    builder.build().context("failed to build the upstream HTTP client")
}

/// 代理 URL 支持的协议。
///
/// **socks4/socks4a 刻意不收**：SOCKS4 协议里根本没有认证字段，URL 里写的 `user:pass@`
/// 会被静默丢掉；买来的代理十有八九要认证，结果就是一条看不出原因的连接失败。
/// socks5h 能覆盖它的全部用途，故在入口就拒掉。
const PROXY_SCHEMES: &[&str] = &["http://", "https://", "socks5://", "socks5h://"];

/// 入库时归一化的协议：本机解析 → 交给代理端解析。
///
/// coban 的出站目标永远是 `chatgpt.com` 这一个公网域名，不存在非本机解析不可的场景。
/// 而本机解析有两个实打实的坏处：把目标域名泄露给本地 DNS（花钱买的是出口隔离，本地
/// 解析器上却留了一串查询记录），以及解析出的 IP 是按**你**的位置就近的，再通过一个
/// 异地代理去连它，既绕远又与「真实用户从那个出口访问」的形态对不上。此外大量住宅代理
/// 压根不接受 IP 形式的连接请求，只回一个 `unexpected EOF`。
///
/// **归一化发生在入库那一刻，不是发请求时**：存 `socks5://` 却按 `socks5h://` 跑，
/// 库里的值与真实行为就对不上，下次出问题看着配置推不出行为。
const PROXY_SCHEME_UPGRADES: &[(&str, &str)] = &[("socks5://", "socks5h://")];

/// 校验一条代理 URL 能不能用，能则返回规范化后的串（去空白 + 协议归一化）。
///
/// **在入库那一刻校验，而不是发请求时**：存进去一条建不出客户端的代理，故障要等到下一次
/// 真有请求选中这个号才暴露，那时现场只剩一条「所有请求都失败」。
pub fn validate_proxy(raw: &str) -> Result<String> {
    let url = raw.trim();
    anyhow::ensure!(!url.is_empty(), "the proxy URL must not be empty");
    let url = match PROXY_SCHEME_UPGRADES.iter().find(|(from, _)| url.starts_with(from)) {
        Some((from, to)) => format!("{to}{}", &url[from.len()..]),
        None => url.to_string(),
    };
    // 校验归一化之后那串——存什么就验什么，免得验的和跑的是两条 URL。
    let uri = check_proxy_url(&url)?;
    anyhow::ensure!(
        matches!(uri.path(), "" | "/") && uri.query().is_none(),
        "the proxy URL must not have a path or query: {url}"
    );
    wreq::Proxy::all(&url).with_context(|| format!("invalid proxy URL: {url}"))?;
    Ok(url)
}

/// 校验一条代理 URL 会不会被**真正当成代理**，成功时返回解析出的 URI。
///
/// **为什么 `wreq::Proxy::all` 成功还不够**：那一步只要求「能解析成 `Uri`、且 scheme 与
/// authority 都在」，它连 scheme 是不是代理协议都不看。真正决定代理生不生效的是库内部的
/// 环境 URI 解析，它认不出来时**返回 `None` 而不报错**，`build()` 照样成功，于是拿到一个
/// 「配置里有代理、实际没有代理」的客户端：请求带着真实 IP 直连打上游，日志上完全看不出来。
/// 实测会这样的几条：
///
/// - `socks5://u:pa#ss@h:1080` —— 密码里的裸 `#` 被当成 fragment 切掉，authority 塌成 `u:pa`；
/// - `socks5://h:notaport`、`socks5://h:99999` —— 端口不是合法 u16；
/// - `ftp://h:21` —— scheme 压根不是代理协议。
fn check_proxy_url(url: &str) -> Result<axum::http::Uri> {
    use axum::http::{Uri, uri::Authority};

    anyhow::ensure!(
        PROXY_SCHEMES.iter().any(|s| url.starts_with(s)),
        "unsupported proxy scheme (expected one of: {})",
        PROXY_SCHEMES.join(", ")
    );
    let uri: Uri = url.parse().with_context(|| format!("invalid proxy URL: {url}"))?;
    let authority = uri.authority().with_context(|| format!("the proxy URL has no host: {url}"))?;
    // userinfo 由库单独取走（`rsplit_once('@')`），要能自成一个合法 authority 的是
    // host:port 那半。
    let host_port = authority.as_str().rsplit_once('@').map_or(authority.as_str(), |(_, hp)| hp);
    let host_port: Authority = host_port.parse().with_context(|| {
        format!(
            "invalid host:port in the proxy URL: {url} \
             (special characters in user:pass@ must be percent-encoded, e.g. # as %23)"
        )
    })?;
    anyhow::ensure!(!host_port.host().is_empty(), "the proxy URL has no host: {url}");
    anyhow::ensure!(
        host_port.port_u16().is_some() || matches!(uri.scheme_str(), Some("http" | "https")),
        "the proxy URL needs an explicit port: {url}"
    );
    Ok(uri)
}

/// 出站客户端池：不配代理的号共用直连那一份，配了代理的按代理 URL 各缓存一份。
///
/// 缓存的理由不是省内存而是**连接复用**：每次现建一个客户端等于每条请求都重新握手，
/// TLS 指纹倒是没变，但连接建立的时序模式与真实客户端完全不同，且慢得多。
pub struct ClientPool {
    direct: wreq::Client,
    by_proxy: parking_lot::Mutex<HashMap<String, Arc<wreq::Client>>>,
}

impl ClientPool {
    pub fn new() -> Result<Self> {
        Ok(Self {
            direct: upstream_client(None)?,
            by_proxy: parking_lot::Mutex::new(HashMap::new()),
        })
    }

    /// 取直连客户端（登录换 token 这类还没有凭证的场景用）。
    pub fn direct(&self) -> &wreq::Client {
        &self.direct
    }

    /// 取该凭证该用的客户端。
    ///
    /// **配了代理却建不出客户端时返回 Err，绝不退回直连**——退回直连就是拿真实 IP 去打
    /// 上游，恰恰是配代理要避免的事，而且从日志上看这条请求「成功了」。
    pub fn for_credential(&self, cred: &Credential) -> Result<Arc<wreq::Client>> {
        let Some(proxy) = cred.proxy.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(Arc::new(self.direct.clone()));
        };
        let mut map = self.by_proxy.lock();
        if let Some(c) = map.get(proxy) {
            return Ok(c.clone());
        }
        let client = Arc::new(upstream_client(Some(proxy)).with_context(|| {
            format!("credential #{} has an unusable proxy configured: {proxy}", cred.id)
        })?);
        map.insert(proxy.to_owned(), client.clone());
        Ok(client)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socks5_is_normalized_to_socks5h() {
        assert_eq!(
            validate_proxy("socks5://user:pw@h.example:1080").unwrap(),
            "socks5h://user:pw@h.example:1080"
        );
        assert_eq!(validate_proxy("  http://h.example:8080  ").unwrap(), "http://h.example:8080");
    }

    /// 这些全是「`Proxy::all` 会成功、代理却不生效」的形态，必须在入库时就拒掉。
    #[test]
    fn rejects_silently_broken_proxy_urls() {
        for bad in [
            "",
            "h.example:1080",               // 没有 scheme
            "ftp://h.example:21",           // 不是代理协议
            "socks4://h.example:1080",      // 带不了认证，见 PROXY_SCHEMES
            "socks5://u:pa#ss@h:1080",      // 裸 # 把 authority 截断
            "socks5://h.example:99999",     // 端口越界
            "socks5://h.example:notaport",  // 端口不是数字
            "socks5://h.example:1080/path", // 路径会被静默丢掉
        ] {
            assert!(validate_proxy(bad).is_err(), "should reject: {bad:?}");
        }
    }
}
