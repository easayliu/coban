//! OAuth PKCE 授权流程：生成挑战、构造授权 URL、交换与刷新 token、解析 id_token claim。
//!
//! 与 luban（Claude）那套的关键差别：账号身份**不在**交换响应里，而在 id_token 的
//! claim 里（`chatgpt_account_id` / `chatgpt_plan_type` / `email`），故每次交换与刷新
//! 之后都要解一次 JWT（见 [`Claims::parse`]）。

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::Rng;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config;
use crate::credentials::now_secs;

/// 一组 OAuth token（交换或刷新得到），交由 [`crate::store`] 落库。
#[derive(Debug, Clone)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: String,
    /// 过期的 Unix 时间戳（秒）。
    pub expires_at: u64,
    /// id_token（JWT 原串）。
    pub id_token: Option<String>,
    /// 从 id_token 解出的账号信息。
    pub claims: Claims,
}

/// id_token 里我们关心的那几项 claim。
#[derive(Debug, Clone, Default)]
pub struct Claims {
    pub email: Option<String>,
    /// `chatgpt_account_id`——转发时 `chatgpt-account-id` 头的取值，缺了转发必 401。
    pub account_id: Option<String>,
    /// `chatgpt_plan_type`：`plus`/`pro`/`team`/`enterprise`/`free`。
    pub plan_type: Option<String>,
}

impl Claims {
    /// 解析 id_token 的 payload 段。
    ///
    /// **刻意不验签**：验签要拉 `auth.openai.com` 的 JWKS 并跟着它轮换，而这个 token 是
    /// coban 自己刚从 token 端点通过 TLS 取回来的，签名能证明的东西（「确实由 OpenAI 签发」）
    /// TLS 那一跳已经证明过了。真正的判决权也不在这里——account_id 对不对，最终由上游的
    /// 401 说话。故这里只做「取出字段」，任何一项缺失都不当致命错误处理。
    ///
    /// 那几个 claim 挂在一个**以命名空间为 key 的嵌套对象**下：
    ///
    /// ```json
    /// { "https://api.openai.com/auth": { "chatgpt_account_id": "…", "chatgpt_plan_type": "pro" } }
    /// ```
    ///
    /// **别被点号形式骗了**：拿脚本递归打印 claim 时，嵌套 key 常被拼成
    /// `https://api.openai.com/auth.chatgpt_account_id` 显示，看着像个扁平串——照那个写法
    /// 去取永远是 `None`。这条踩过一次，代价是**浏览器授权整条路不可用**：那条路没有
    /// auth.json 里 `tokens.account_id` 那样的兜底，account_id 解不出来就等于登录失败。
    /// 故这里以嵌套为准，另留一条扁平兜底（万一哪天上游真改成扁平的，不至于当场全挂），
    /// 两种形态都有回归测试钉住。
    pub fn parse(id_token: &str) -> Self {
        let Some(payload) = id_token.split('.').nth(1) else {
            return Self::default();
        };
        let Ok(raw) = URL_SAFE_NO_PAD.decode(payload) else {
            return Self::default();
        };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw) else {
            return Self::default();
        };
        // **签发方对不上就整条不认**：一个别处签的 JWT 里当然也能有个
        // `chatgpt_account_id`，照单全收就等于让任何人塞一条假凭证进来。这一步拦不住
        // 伪造（没验签），但拦得住「粘错了文件」这类真实得多的情况，且报错精确。
        if v.get("iss").and_then(|x| x.as_str()) != Some(config::ISSUER) {
            return Self::default();
        }
        let ns = v.get(config::ID_TOKEN_CLAIM_NS);
        let claim = |name: &str| {
            ns.and_then(|o| o.get(name))
                // 扁平兜底：上游改形态时不至于当场全挂。
                .or_else(|| v.get(format!("{}.{name}", config::ID_TOKEN_CLAIM_NS)))
                .and_then(|x| x.as_str())
                .map(str::to_owned)
        };
        Self {
            email: v.get("email").and_then(|x| x.as_str()).map(str::to_owned),
            account_id: claim("chatgpt_account_id"),
            plan_type: claim("chatgpt_plan_type"),
        }
    }
}

/// 一次登录尝试的 PKCE 上下文，需在交换 token 时回传。
#[derive(Clone)]
pub struct PkceChallenge {
    pub verifier: String,
    pub challenge: String,
    pub state: String,
}

impl PkceChallenge {
    /// 生成新的 PKCE 挑战：随机 verifier、S256 challenge、随机 state。
    ///
    /// **verifier 与 state 都取十六进制**（64 字节 → 128 hex / 32 字节 → 64 hex），
    /// 与 sub2api 一致，它在 `GenerateCodeVerifier` 上明确写着「OpenAI uses hex encoding
    /// instead of base64url」。RFC 7636 两种都合法（43–128 个 unreserved 字符），实测
    /// base64url 也能换到 token，所以这不是「必须」——但排查授权问题时，与一个已知能跑通
    /// 的实现逐项对齐能少一个变量，而这一项的代价只是串长一点。
    ///
    /// challenge 仍是 base64url：那是 RFC 7636 对 S256 的硬性规定，与 verifier 的编码无关。
    pub fn generate() -> Self {
        let verifier = random_hex(64);
        let state = random_hex(32);

        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());

        Self { verifier, challenge, state }
    }

    /// 构造用户需要在浏览器打开的授权 URL。
    ///
    /// 构造用户需要在浏览器打开的授权 URL。
    ///
    /// 末尾两个非标准参数的**依据是 sub2api 的生产实现**（`internal/pkg/openai/oauth.go`
    /// 的 `BuildAuthorizationURLForPlatform`），不是官方客户端的抓包——在 codex v0.98.0 的
    /// 发行二进制里这两个字面量一次都搜不到，多半是运行时拼的或那条路径没进 strings。
    /// 一个能跑通的第三方实现是比「我搜不到」更强的证据，故跟着带；来源写在这里，
    /// 免得下次又有人（包括我自己）拿 grep 的空结果把它们删掉。
    ///
    /// - `id_token_add_organizations=true`：让 id_token 带上 `organizations`（团队号归属）。
    /// - `codex_cli_simplified_flow=true`：走 codex 那套简化授权确认。
    pub fn authorize_url(&self) -> String {
        let params = [
            ("response_type", "code"),
            ("client_id", config::CLIENT_ID),
            ("redirect_uri", config::REDIRECT_URI),
            ("scope", config::SCOPES),
            ("code_challenge", &self.challenge),
            ("code_challenge_method", "S256"),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("state", &self.state),
        ];
        let query: Vec<String> =
            params.iter().map(|(k, v)| format!("{}={}", k, urlencode(v))).collect();
        format!("{}?{}", config::AUTHORIZE_URL, query.join("&"))
    }
}

/// token 端点的响应结构。
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    /// **刷新响应里可能没有这一项**：OAuth 允许服务端不轮换 refresh_token，此时响应里
    /// 压根不带它。按必填解会让整次刷新失败、账号被误判为失效，故留 `Option`，
    /// 由 [`refresh_token`] 决定沿用旧的（见那里的注）。
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    /// 有效期（秒）。缺失时按 1 小时兜底——实测就是 3600，且宁可早刷一次也不要因为
    /// 一个缺失字段把 `expires_at` 记成 0（那会让每条请求都触发一次刷新）。
    #[serde(default)]
    expires_in: Option<u64>,
}

impl TokenResponse {
    /// 转成入库形态。`fallback_refresh` 是上一份 refresh_token，供响应未轮换时沿用。
    fn into_token_set(self, fallback_refresh: Option<&str>) -> Result<TokenSet> {
        let refresh_token =
            self.refresh_token.or_else(|| fallback_refresh.map(str::to_owned)).context(
                "the token response contained no refresh_token (is offline_access in scope?)",
            )?;
        let claims = self.id_token.as_deref().map(Claims::parse).unwrap_or_default();
        Ok(TokenSet {
            access_token: self.access_token,
            refresh_token,
            expires_at: now_secs() + self.expires_in.unwrap_or(3600),
            id_token: self.id_token,
            claims,
        })
    }
}

/// **上游的 refresh_token 是单次可用的**：换一次就作废，响应里给的那个才是下一次能用的。
///
/// 这条性质决定了两件事，改这一块前务必记住：
///
/// 1. **并发刷新必然废号**——两个请求同时拿同一个 token 去刷，后到的那个必然撞
///    `refresh_token_reused`。故刷新走每凭证一把锁（见
///    [`crate::store::CredentialStore::valid_access_token`]）。
/// 2. **刷新请求不能盲目重试**——请求已经打到上游、只是响应没收到的话，token 其实已经
///    轮换了，拿同一个再试一次得到的是 `refresh_token_reused`，于是一个健康账号被自己的
///    重试判成永久失效。sub2api 那边是带指数退避直接重试的；这里刻意不这么做，只在
///    「确定没发出去」的连接层错误上重试，见 [`crate::store::CredentialStore::valid_access_token`]。
///
/// 判据清单取自 sub2api 的 `isNonRetryableRefreshError`，另补上实测遇到的
/// `refresh_token_invalidated`（它那份清单里没有，可能是较新的错误码）。
pub const PERMANENT_REFRESH_ERRORS: &[&str] = &[
    "invalid_grant",
    "refresh_token_reused",
    "refresh_token_invalidated",
    "invalid_client",
    "unauthorized_client",
    "access_denied",
];

/// 这次刷新失败是不是**不会自己好**的那种（需要重新授权）。
///
/// **大小写不敏感**：错误文本来自上游的 JSON 体与我们自己拼的上下文，大小写不受控，
/// sub2api 在这一点上专门有条测试（`case_insensitive`）。
pub fn is_permanent_refresh_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    PERMANENT_REFRESH_ERRORS.iter().any(|k| lower.contains(k))
}

/// 刷新时申报的 scope。见 [`refresh_token`] 的说明——与登录时那串**不同**，不含
/// `offline_access`。
const REFRESH_SCOPES: &str = "openid profile email";

/// 用授权码交换 token。`code` 与 `verifier` 必须来自同一次 [`PkceChallenge`]。
pub async fn exchange_code(client: &wreq::Client, code: &str, verifier: &str) -> Result<TokenSet> {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", config::REDIRECT_URI),
        ("client_id", config::CLIENT_ID),
        ("code_verifier", verifier),
    ];
    post_token(client, &form, None).await.context("failed to exchange the authorization code")
}

/// 用 refresh_token 换一组新 token。
///
/// **上游会轮换 refresh_token**：响应里带了新的就必须用新的，旧的当场作废。它没带时
/// 才沿用旧的——把这条判断写在 [`TokenResponse::into_token_set`] 一处，别在调用点各写
/// 一遍，那种地方漏一个就是「刷新成功但库里存了个已作废的 token」，此后该账号所有刷新
/// 全部失败，等于账号被自己废掉。
///
/// scope 用 [`REFRESH_SCOPES`] 而不是登录时那串：codex 二进制里刷新用的字面量是
/// `openid profile email`，不含 `offline_access`。实测两者（以及完全不带 scope）在
/// 上游行为一致，但既然官方发哪串是可查的，就跟着它，不自己发明。
pub async fn refresh_token(client: &wreq::Client, refresh: &str) -> Result<TokenSet> {
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh),
        ("client_id", config::CLIENT_ID),
        ("scope", REFRESH_SCOPES),
    ];
    post_token(client, &form, Some(refresh)).await.context("failed to refresh the access token")
}

/// 向 token 端点发一次表单 POST 并解析响应。
async fn post_token(
    client: &wreq::Client,
    form: &[(&str, &str)],
    fallback_refresh: Option<&str>,
) -> Result<TokenSet> {
    let resp = client
        .post(config::TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(urlencode_form(form))
        .send()
        .await
        .context("request to the token endpoint failed")?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        // 错误体是 `{"error":"invalid_grant","error_description":"…"}` 这种短结构，
        // 不含 token，可以整条带进错误——它是判断「授权码过期」还是「verifier 不匹配」
        // 的唯一线索。
        bail!("the token endpoint returned {}: {}", status, text.trim());
    }
    // **成功响应绝不能拼进错误消息**：里面是这个账号的 access/refresh token，而 error
    // 会一路走到日志与后台页面。serde 的报错自带字段名，定位足够了。
    let parsed: TokenResponse = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse the token response ({} bytes)", text.len()))?;
    parsed.into_token_set(fallback_refresh)
}

/// 从粘回来的回调 URL（或裸 code）里取出 `code` 与 `state`。
///
/// 用户能拿到的东西有两种形态：整条 `http://localhost:1455/auth/callback?code=…&state=…`
/// （地址栏直接复制），以及只有 code 那一段（有人会自己截）。两种都收——只认一种的话，
/// 另一种的报错是「授权码无效」，而用户看着自己明明粘了 code，完全对不上。
pub fn parse_callback(input: &str) -> (String, Option<String>) {
    let input = input.trim();
    let query = match input.split_once('?') {
        Some((_, q)) => q,
        // 没有 `?`：可能是裸的 `code=…&state=…`，也可能就是一个裸 code。
        None => input,
    };
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("code", v)) => code = Some(urldecode(v)),
            Some(("state", v)) => state = Some(urldecode(v)),
            _ => {}
        }
    }
    // 一个 `=` 都没有 → 整串就是 code 本身。
    (code.unwrap_or_else(|| input.to_owned()), state)
}

/// 生成 n 字节随机数据的小写十六进制编码。
fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    rand::rng().fill_bytes(&mut buf);
    crate::credentials::hex_lower(&buf)
}

/// 把 `(key, value)` 列表编码成 `application/x-www-form-urlencoded` 请求体。
fn urlencode_form(form: &[(&str, &str)]) -> String {
    form.iter().map(|(k, v)| format!("{}={}", k, urlencode(v))).collect::<Vec<_>>().join("&")
}

/// 百分号编码（RFC 3986 的 unreserved 集合之外一律转义）。
///
/// **`+` 不能用来表示空格**：scope 那串里的空格若编码成 `+`，token 端点按字面收下，
/// 于是 scope 变成 `openid+profile+email+offline_access` 这么一个不存在的单项，
/// 换回来的 token 缺 offline_access——报错是一小时后的 401，与这里差着十万八千里。
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 百分号解码；`+` 按 query 惯例还原成空格。解不出的转义序列原样保留。
fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(b) => {
                    out.push(b);
                    i += 3;
                }
                Err(_) => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    /// 把一组 claim 拼成一个形态正确的 id_token（不签名，解析侧本就不验签）。
    fn fake_id_token(mut body: serde_json::Value) -> String {
        if body.get("iss").is_none() {
            body["iss"] = serde_json::json!(config::ISSUER);
        }
        let head = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256"}"#);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&body).unwrap());
        format!("{head}.{payload}.sig")
    }

    /// **真实形态**：claim 挂在以命名空间为 key 的嵌套对象下。逐字节照抄自一份真实
    /// id_token 的结构——按扁平点号去取会让 account_id 恒为 None，那等于浏览器授权
    /// 整条路不可用。
    #[test]
    fn claims_read_nested_namespaced_object() {
        let ns = config::ID_TOKEN_CLAIM_NS;
        let token = fake_id_token(serde_json::json!({
            "email": "a@b.com",
            ns: {
                "chatgpt_account_id": "acct-123",
                "chatgpt_plan_type": "pro",
                "chatgpt_user_id": "user-1",
            },
        }));
        let c = Claims::parse(&token);
        assert_eq!(c.email.as_deref(), Some("a@b.com"));
        assert_eq!(c.account_id.as_deref(), Some("acct-123"));
        assert_eq!(c.plan_type.as_deref(), Some("pro"));
    }

    /// 扁平兜底：上游哪天真改成扁平点号 key，也不该当场全挂。
    #[test]
    fn claims_fall_back_to_flat_namespaced_keys() {
        let ns = config::ID_TOKEN_CLAIM_NS;
        let token = fake_id_token(serde_json::json!({
            format!("{ns}.chatgpt_account_id"): "acct-flat",
            format!("{ns}.chatgpt_plan_type"): "plus",
        }));
        let c = Claims::parse(&token);
        assert_eq!(c.account_id.as_deref(), Some("acct-flat"));
        assert_eq!(c.plan_type.as_deref(), Some("plus"));
    }

    /// 形态不对的 token 不该 panic，也不该让登录整体失败——退化成「什么都没解出来」，
    /// 由调用点按「缺 account_id」拒绝入库并给出准确提示。
    #[test]
    fn claims_parse_tolerates_garbage() {
        for s in ["", "not-a-jwt", "a.b", "a.!!!.c"] {
            assert!(Claims::parse(s).account_id.is_none(), "input: {s}");
        }
    }

    /// 别处签的 token 里一样可以有 chatgpt_account_id；签发方对不上就整条不认。
    #[test]
    fn claims_reject_a_foreign_issuer() {
        let ns = config::ID_TOKEN_CLAIM_NS;
        let token = fake_id_token(serde_json::json!({
            "iss": "https://evil.example",
            ns: { "chatgpt_account_id": "acct-123" },
        }));
        assert!(Claims::parse(&token).account_id.is_none());
    }

    #[test]
    fn callback_accepts_full_url_bare_query_and_bare_code() {
        let (code, state) =
            parse_callback("http://localhost:1455/auth/callback?code=abc123&state=xyz");
        assert_eq!(code, "abc123");
        assert_eq!(state.as_deref(), Some("xyz"));

        let (code, state) = parse_callback("code=abc123&state=xyz");
        assert_eq!(code, "abc123");
        assert_eq!(state.as_deref(), Some("xyz"));

        let (code, state) = parse_callback("  abc123  ");
        assert_eq!(code, "abc123");
        assert_eq!(state, None);
    }

    /// 回调 URL 里的 code 常带百分号转义（`/` `+` `=` 都会被编码），不解码就换不到 token。
    #[test]
    fn callback_percent_decodes_values() {
        let (code, _) = parse_callback("http://x/cb?code=ab%2Fcd%3D&state=s");
        assert_eq!(code, "ab/cd=");
    }

    /// 分类清单取自 sub2api 的 `isNonRetryableRefreshError`，含它那条大小写不敏感的用例。
    #[test]
    fn permanent_refresh_errors_are_classified_case_insensitively() {
        for msg in [
            "invalid_grant",
            "Error: invalid_grant - token revoked",
            "INVALID_GRANT",
            r#"status 401, body: {"error":{"code":"refresh_token_reused"}}"#,
            r#"{"error":{"code":"refresh_token_invalidated"}}"#,
            "invalid_client",
            "unauthorized_client",
            "access_denied",
        ] {
            assert!(is_permanent_refresh_error(msg), "should be permanent: {msg}");
        }
        // 这些**必须**判成可重试：把它们当永久失效，一次机房抖动就能关光整池账号。
        for msg in [
            "network timeout",
            "connection reset by peer",
            "the token endpoint returned 502 Bad Gateway",
            "dns error",
        ] {
            assert!(!is_permanent_refresh_error(msg), "should be transient: {msg}");
        }
    }

    /// verifier/state 取十六进制（对齐 sub2api），且长度落在 RFC 7636 的 43–128 之内。
    #[test]
    fn pkce_verifier_and_state_are_hex() {
        let p = PkceChallenge::generate();
        assert_eq!(p.verifier.len(), 128);
        assert_eq!(p.state.len(), 64);
        for (name, v) in [("verifier", &p.verifier), ("state", &p.state)] {
            assert!(
                v.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{name}: {v}"
            );
        }
        assert!((43..=128).contains(&p.verifier.len()), "RFC 7636 requires 43..=128");
    }

    /// 这两个参数的依据是 sub2api 的生产实现；曾被我按「官方二进制里搜不到」删过一次，
    /// 这条测试连同 authorize_url 的注释一起挡住下一次误删。
    #[test]
    fn authorize_url_carries_the_sub2api_sourced_params() {
        let url = PkceChallenge::generate().authorize_url();
        assert!(url.contains("id_token_add_organizations=true"), "{url}");
        assert!(url.contains("codex_cli_simplified_flow=true"), "{url}");
    }

    /// scope 里的空格必须是 `%20` 而不是 `+`，理由见 [`urlencode`]。
    #[test]
    fn authorize_url_encodes_spaces_as_pct20() {
        let url = PkceChallenge::generate().authorize_url();
        assert!(url.contains("scope=openid%20profile%20email%20offline_access"), "{url}");
        assert!(!url.contains('+'), "a literal + in the query would be read as a space: {url}");
        assert!(url.contains("code_challenge_method=S256"));
    }

    /// PKCE 的 challenge 必须是 verifier 的 S256——这条错了授权页不会报错，
    /// 直到交换那一步才以 `invalid_grant` 出现。
    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let p = PkceChallenge::generate();
        let mut h = Sha256::new();
        h.update(p.verifier.as_bytes());
        assert_eq!(p.challenge, URL_SAFE_NO_PAD.encode(h.finalize()));
    }

    /// 刷新响应不带 refresh_token 时沿用旧的；带了则用新的。
    #[test]
    fn refresh_response_without_rotation_keeps_old_token() {
        let resp = TokenResponse {
            access_token: "at".into(),
            refresh_token: None,
            id_token: None,
            expires_in: Some(3600),
        };
        let set = resp.into_token_set(Some("old-rt")).unwrap();
        assert_eq!(set.refresh_token, "old-rt");

        let resp = TokenResponse {
            access_token: "at".into(),
            refresh_token: Some("new-rt".into()),
            id_token: None,
            expires_in: None,
        };
        let set = resp.into_token_set(Some("old-rt")).unwrap();
        assert_eq!(set.refresh_token, "new-rt");
    }
}
