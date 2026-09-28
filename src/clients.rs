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
/// **不设 `default_headers`，也一个 `accept-encoding` 都不发**：官方客户端没开任何解压
/// feature，reqwest 于是不会给请求补这个头（见 Cargo.toml 里 reqwest 那几行注）。这做不到
/// 靠在这里写一个空值——得让底层压根没有可声明的编码，也就是那几个 feature 真的不在。
///
/// TLS 后端就是 reqwest 的默认档（`default-tls` = native-tls），**刻意不调
/// `use_rustls_tls()`**：codex 的默认档同样是它（`TlsBackend::TransportDefault`），rustls
/// 在那边只用于自定义 CA 的回退路径。调了反而是对着回退路径抄。
///
/// `proxy` 为 `Some` 时挂上代理，其余形态与直连那份**逐字节相同**——代理只改走法，不改
/// 请求本身，否则「配了代理的号」就多出一处与别的号不同的指纹。
pub fn upstream_client(proxy: Option<&str>) -> Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .user_agent(config::CODEX_USER_AGENT.as_str())
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
            builder.proxy(
                reqwest::Proxy::all(url).with_context(|| format!("invalid proxy URL: {url}"))?,
            )
        }
        // 不配代理时**不调用 `.no_proxy()`**：保留默认的系统代理探测（环境变量
        // HTTPS_PROXY/ALL_PROXY，以及 reqwest `system-proxy` 那档的系统设置），那是全局兜底，
        // 与逐账号代理各管一层。codex 的默认档同样不关它（`ProxyRouting::TransportDefault`）。
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
    reqwest::Proxy::all(&url).with_context(|| format!("invalid proxy URL: {url}"))?;
    Ok(url)
}

/// 校验一条代理 URL 会不会被**真正当成代理**，成功时返回解析出的 URI。
///
/// **为什么 `reqwest::Proxy::all` 成功还不够**：那一步只要求「能解析成 `Uri`、且 scheme 与
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

/// 解析批量导入里的一行，返回 (名称, 原始 URL)；URL 的校验与规范化留给入库那一步。
///
/// 认这几种写法（第一个空白之后的内容当名称，可省）：
///
/// - `socks5h://user:pass@host:port` —— 完整 URL，原样用；
/// - `user:pass@host:port`、`host:port` —— 补上 `default_scheme`；
/// - `host:port:user:pass` —— 代理商导出最常见的格式。账号密码在这里做 percent-encode：
///   这种格式本来就不转义，密码里一个 `#` 或 `@` 原样拼进 URL 就会被切坏（见
///   [`check_proxy_url`]）。
///
/// 空行与 `#` 开头的注释行由调用方跳过。
pub fn parse_proxy_line(line: &str, default_scheme: &str) -> Result<(String, String)> {
    let line = line.trim();
    let (token, label) = match line.split_once(char::is_whitespace) {
        Some((t, rest)) => (t, rest.trim()),
        None => (line, ""),
    };
    if token.contains("://") {
        return Ok((label.to_string(), token.to_string()));
    }
    let scheme = format!("{}://", default_scheme.trim_end_matches("://"));
    anyhow::ensure!(
        PROXY_SCHEMES.contains(&scheme.as_str()),
        "unsupported default proxy scheme: {default_scheme}"
    );
    // 先认 host:port:user:pass，再认 `@`：这种格式的密码不转义，本身就可能带 `@`。
    // 第二段是端口号才算——`user:pass@host:port` 切出来第二段是 `pass@host`。
    let parts: Vec<&str> = token.splitn(4, ':').collect();
    let url = match parts.as_slice() {
        [host, port, user, pass] if port.parse::<u16>().is_ok() => {
            format!("{scheme}{}:{}@{host}:{port}", encode_userinfo(user), encode_userinfo(pass))
        }
        _ if token.contains('@') => format!("{scheme}{token}"),
        [host, port] => format!("{scheme}{host}:{port}"),
        _ => anyhow::bail!(
            "unrecognized proxy line (expected a URL, host:port, or host:port:user:pass): {token}"
        ),
    };
    Ok((label.to_string(), url))
}

/// userinfo 的 percent-encode：只放行 RFC 3986 的 unreserved 字符，其余一律转义。
fn encode_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// 代理测试打的地址：上游域名自己的 Cloudflare trace。
///
/// **打 `chatgpt.com` 而不是某个第三方查 IP 服务**：要回答的是「这个代理能不能连到上游」，
/// 换个目标测通了不说明任何事（不少代理/出口恰恰对 OpenAI 不通）。而这一个端点顺带回报
/// 出口 IP 与国家码，一条请求就把「通不通」和「从哪儿出去」都答了，也不碰任何账号与额度。
const PROXY_TEST_URL: &str = "https://chatgpt.com/cdn-cgi/trace";

/// 代理测试的总超时。住宅代理慢，但超过这个数的出口拿来转流式回复也没法用。
const PROXY_TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// 一次代理测试的结果。
#[derive(serde::Serialize)]
pub struct ProxyTestReport {
    /// 拿到了上游的 2xx 且读出了出口 IP。
    pub ok: bool,
    /// 规范化之后实际测的那条 URL（`socks5://` 已改成 `socks5h://`），与保存时入库的一致。
    pub proxy: String,
    /// 上游 HTTP 状态码；`0` = 请求没到上游（连不上代理、代理拒绝、超时）。
    pub status: u16,
    pub latency_ms: u128,
    /// 上游看到的出口 IP。
    pub ip: Option<String>,
    /// 出口所在国家/地区码（Cloudflare 的 `loc`，如 `US`）。
    pub loc: Option<String>,
    /// 接入的 Cloudflare 机房（`colo`，如 `LAX`）。
    pub colo: Option<String>,
    pub error: Option<String>,
}

/// 用一条**尚未保存**的代理 URL 连一次上游，看它通不通、出口在哪。
///
/// 校验不过时返回 Err（调用方该回 400）；连接层面的失败算测试结果，放进 `error`。
/// **现建一个客户端、不进 [`ClientPool`]**：测的可能是一条最后不保存的 URL，塞进池子只会
/// 留下一个没人用的连接池。
pub async fn test_proxy(raw: &str) -> Result<ProxyTestReport> {
    let proxy = validate_proxy(raw)?;
    let client = upstream_client(Some(&proxy))?;
    let started = std::time::Instant::now();
    let mut report = ProxyTestReport {
        ok: false,
        proxy,
        status: 0,
        latency_ms: 0,
        ip: None,
        loc: None,
        colo: None,
        error: None,
    };
    let result = async {
        let resp = client.get(PROXY_TEST_URL).timeout(PROXY_TEST_TIMEOUT).send().await?;
        let status = resp.status();
        let body = resp.text().await?;
        Ok::<_, reqwest::Error>((status, body))
    }
    .await;
    report.latency_ms = started.elapsed().as_millis();
    match result {
        Ok((status, body)) => {
            report.status = status.as_u16();
            let field = |key: &str| {
                body.lines()
                    .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
                    .map(|v| v.trim().to_owned())
                    .filter(|v| !v.is_empty())
            };
            report.ip = field("ip");
            report.loc = field("loc");
            report.colo = field("colo");
            report.ok = status.is_success() && report.ip.is_some();
            if !report.ok {
                let snippet: String = body.chars().take(300).collect();
                report.error = Some(format!("upstream answered {status}: {snippet}"));
            }
        }
        // `{:#}` 不适用于 reqwest::Error；把 source 链拼出来，否则只剩一句「error sending request」，
        // 看不出是代理拒绝认证、连不上还是超时。
        Err(e) => {
            let mut msg = e.to_string();
            let mut src = std::error::Error::source(&e);
            while let Some(s) = src {
                msg.push_str(": ");
                msg.push_str(&s.to_string());
                src = s.source();
            }
            report.error = Some(msg);
        }
    }
    Ok(report)
}

/// 出站客户端池：不配代理的号共用直连那一份，配了代理的按代理 URL 各缓存一份。
///
/// 缓存的理由不是省内存而是**连接复用**：每次现建一个客户端等于每条请求都重新握手，
/// TLS 指纹倒是没变，但连接建立的时序模式与真实客户端完全不同，且慢得多。
pub struct ClientPool {
    direct: reqwest::Client,
    by_proxy: parking_lot::Mutex<HashMap<String, Arc<reqwest::Client>>>,
}

impl ClientPool {
    pub fn new() -> Result<Self> {
        Ok(Self {
            direct: upstream_client(None)?,
            by_proxy: parking_lot::Mutex::new(HashMap::new()),
        })
    }

    /// 取直连客户端（登录换 token 这类还没有凭证的场景用）。
    pub fn direct(&self) -> &reqwest::Client {
        &self.direct
    }

    /// 取该凭证该用的客户端。
    ///
    /// **配了代理却建不出客户端时返回 Err，绝不退回直连**——退回直连就是拿真实 IP 去打
    /// 上游，恰恰是配代理要避免的事，而且从日志上看这条请求「成功了」。
    pub fn for_credential(&self, cred: &Credential) -> Result<Arc<reqwest::Client>> {
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

    /// 「不发 `accept-encoding`」这件事只有**看字节**才算数：它不是我们写进去的一个值，
    /// 而是「底层没有可声明的编码」这个状态的副产物，任何人给客户端加回一个解压开关都会让
    /// 它悄悄冒出来。故这条测试起一个真的监听端口，把请求原文读出来断言。
    ///
    /// 同时钉住 UA：没有凭证语境时这条路上的身份就是那份画像。
    #[tokio::test]
    async fn the_upstream_client_announces_no_accept_encoding() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            sock.write_all(b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n").await.unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });

        let client = upstream_client(None).unwrap();
        client.get(format!("http://{addr}/whatever")).send().await.unwrap();
        let req = server.await.unwrap();

        assert!(
            !req.to_ascii_lowercase().contains("accept-encoding"),
            "官方客户端一个 accept-encoding 都不发，这条请求里却有:\n{req}"
        );
        assert!(req.contains(config::UA_PREFIX), "UA 该是那份画像:\n{req}");
    }

    #[test]
    fn parses_import_lines() {
        let p = |l| parse_proxy_line(l, "socks5h").unwrap();
        assert_eq!(p("http://h:8080 日本 1"), ("日本 1".into(), "http://h:8080".into()));
        assert_eq!(p("h.example:1080"), ("".into(), "socks5h://h.example:1080".into()));
        assert_eq!(p("u:p@h:1080"), ("".into(), "socks5h://u:p@h:1080".into()));
        assert_eq!(p("h:1080:us/er:pa#ss"), ("".into(), "socks5h://us%2Fer:pa%23ss@h:1080".into()));
        assert!(validate_proxy(&p("h:1080:u:pa#s@").1).is_ok());
        assert!(parse_proxy_line("h:1:2", "socks5h").is_err());
        assert!(parse_proxy_line("h:1", "ftp").is_err());
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
