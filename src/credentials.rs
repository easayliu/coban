//! 凭证记录模型与刷新判定。持久化在 SQLite，见 [`crate::store`]。

use std::time::{SystemTime, UNIX_EPOCH};

use crate::config;

/// 一条 Codex / ChatGPT OAuth 凭证（对应 SQLite 一行）。
#[derive(Debug, Clone)]
pub struct Credential {
    pub id: i64,
    /// 用户可编辑的显示名（默认取账号邮箱）。
    pub label: String,
    /// 账号邮箱（来自 id_token 的 `email` claim），用于界面展示与去重提示。
    pub email: Option<String>,
    /// ChatGPT 订阅档位（`plus`/`pro`/`team`/`enterprise`/`free`），来自 id_token 的
    /// `chatgpt_plan_type`。`None` 表示这条 id_token 里没有——通常是 API-key 模式误导入。
    pub plan_type: Option<String>,
    /// **`chatgpt-account-id` 头的取值**，来自 id_token 的 `chatgpt_account_id`。
    ///
    /// 这一列不是可选的展示字段而是**转发的必要条件**：`backend-api/codex` 认的是
    /// 「access_token + 与之匹配的 account id」两件一起，缺了这个头上游一律 401，而报错
    /// 里只字不提是哪一半缺失。故入库时拿不到它就拒绝保存（见 [`crate::oauth::TokenSet`]），
    /// 别留一条永远转发失败的凭证在库里。
    pub account_id: String,
    /// 登录时拿到的 id_token（JWT）。存着是为了在不重新登录的前提下重新解析 claim
    /// （档位变更、账号 id 回填），刷新时上游会一起换新。
    pub id_token: Option<String>,
    pub access_token: String,
    pub refresh_token: String,
    /// access_token 过期的 Unix 时间戳（秒）。
    pub expires_at: u64,
    /// 优先级：数值小者优先（代理轮换时先取）。
    pub priority: i64,
    /// 是否停用（停用的凭证不参与转发）。
    pub disabled: bool,
    /// 该账号每分钟最多转发多少条请求（RPM 上限）。三态：`> 0` 本账号独立上限；
    /// `0` 跟随全局默认；`< 0` 本账号明确不限。生效值见
    /// [`crate::store::effective_rpm_limit`]。
    pub rpm_limit: i64,
    /// 自动检测到的上游账号级错误原因（如封号 / refresh_token 失效）；`None` 表示未被
    /// 自动停用（手动停用或未停用皆为 `None`）。见
    /// [`crate::store::CredentialStore::mark_banned`]。
    pub ban_reason: Option<String>,
    /// 被上游限流自动停用后，**到点自动重新启用**的 Unix 时间戳（秒）；`None` 表示不自动
    /// 恢复（人工停用、封号，或压根没停用）。
    ///
    /// 这一列是「限流停用」与「人工/封号停用」的唯一区分点：选号时惰性把到点的号启用回来，
    /// 而人工关掉的号不该被任何自动逻辑打开。
    pub resume_at: Option<u64>,
    /// 该账号专用的出站代理（`socks5h://`/`http://` 等）；`None` 或空串表示直连。
    ///
    /// 配了之后这个号的**全部**出站流量都走它——转发、token 刷新、连通性测试。漏掉任何
    /// 一条都会让那条请求带着真实出口 IP 打到上游，逐账号隔离当场失效，且日志上看不出来。
    /// 故取客户端只有 [`crate::clients::ClientPool::for_credential`] 一个入口。
    pub proxy: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl Credential {
    /// 距离过期的剩余秒数（已过期返回 0）。
    pub fn expires_in_secs(&self) -> u64 {
        self.expires_at.saturating_sub(now_secs())
    }

    /// 是否已过期或即将过期（进入刷新窗口）。
    pub fn needs_refresh(&self) -> bool {
        self.expires_in_secs() <= config::REFRESH_LEEWAY_SECS
    }

    /// 该凭证对上游呈现的稳定 `session_id`：`sha256(account_id ⊕ 会话指纹)` 派生的 UUID v4
    /// 形态串。
    ///
    /// 官方 codex CLI 每条会话发一个 UUID，同一会话内恒定。coban 这边**必须按账号派生而
    /// 不是直接透传来访客户端那个**：多个客户端共用一个号时，透传会让上游看到同一账号下
    /// 冒出大量互不相关的 session；而全账号共用一个固定值又会让所有对话看着像同一条超长
    /// 会话。折中是「账号 + 来访会话」两级派生：同一客户端会话恒定，跨账号互不相同。
    ///
    /// `fingerprint` 为空则退化为仅按账号派生（等价单会话）。
    pub fn session_id(&self, fingerprint: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(self.account_id.as_bytes());
        if !fingerprint.is_empty() {
            hasher.update([0u8]); // 分隔符，避免拼接歧义
            hasher.update(fingerprint.as_bytes());
        }
        let digest = hasher.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        uuid_from_bytes(bytes)
    }

    /// 该凭证对上游呈现的 `User-Agent`：按 `account_id` 从 [`config::UA_PROFILES`] 里
    /// 挑一份「机器画像」拼出来。
    ///
    /// 与 [`Self::session_id`] 同一个思路：**按账号派生，不透传来访客户端那份**。理由见
    /// [`config::UA_PROFILES`] 的注——全池共用一条 UA 是一簇可关联的指纹，而透传来访的
    /// 那份会造出「`originator` 说 codex CLI、UA 说 python SDK」这种官方客户端产生不出来
    /// 的组合。
    ///
    /// 取 `account_id` 而不是 `id` 或 `label`：那两个都会变（重新导入换 id、改名换 label），
    /// 一变就等于这个号换了台机器。`account_id` 是账号本身，只有重新登录另一个账号才会变。
    ///
    /// 哈希前缀里那个 `"ua"` 是**域分隔**：与 `session_id` 用同一份摘要的话，两个派生值之间
    /// 就有了可推算的关系，而它们本该互不相干。
    pub fn user_agent(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"ua");
        hasher.update([0u8]); // 分隔符，避免拼接歧义
        hasher.update(self.account_id.as_bytes());
        let digest = hasher.finalize();
        // 取 8 字节足够均匀，也不必关心表长是不是 2 的幂。
        let mut pick = [0u8; 8];
        pick.copy_from_slice(&digest[..8]);
        let idx = u64::from_be_bytes(pick) as usize % config::UA_PROFILES.len();
        config::user_agent(config::UA_PROFILES[idx])
    }
}

/// 把 16 字节按 UUID v4 的形态（版本位/变体位就位）格式化成带连字符的小写串。
///
/// 派生出来的东西要长得**像**客户端生成的随机 UUID：版本位不对的话，一个按 RFC 校验
/// UUID 的上游能一眼看出这不是 v4。
pub fn uuid_from_bytes(mut b: [u8; 16]) -> String {
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 1
    let h = hex_lower(&b);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// 把字节切片编码为小写十六进制字符串。
pub fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// 当前 Unix 时间戳（秒）。
pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cred(account_id: &str) -> Credential {
        Credential {
            id: 1,
            label: "t".into(),
            email: None,
            plan_type: None,
            account_id: account_id.into(),
            id_token: None,
            access_token: "a".into(),
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
        }
    }

    #[test]
    fn session_id_is_stable_per_account_and_fingerprint() {
        let c = cred("acct-1");
        assert_eq!(c.session_id("fp"), c.session_id("fp"));
        assert_ne!(c.session_id("fp"), c.session_id("other"));
        assert_ne!(c.session_id("fp"), cred("acct-2").session_id("fp"));
    }

    /// UA 是**按账号定死的**：同一个号每次算出来都一样，不同号可以撞（表就那么长），
    /// 但整池不能只落在一份画像上。
    #[test]
    fn user_agent_is_stable_per_account_and_spread_over_profiles() {
        let c = cred("acct-1");
        assert_eq!(c.user_agent(), c.user_agent());

        let seen: std::collections::HashSet<String> =
            (0..200).map(|i| cred(&format!("acct-{i}")).user_agent()).collect();
        // 200 个号只压在一两份画像上，说明取摘要那一步偏了——那时逐号派生就没意义了。
        assert!(
            seen.len() >= config::UA_PROFILES.len() / 2,
            "only {} distinct UAs across 200 accounts",
            seen.len()
        );
    }

    /// 派生出来的 UA 必须**自己就被认成官方客户端**：`UaMode::Auto` 判的是同一个前缀，
    /// 认不出来的话改写完还会被再改写一遍（而且下一档的判断就没有不动点了）。
    #[test]
    fn user_agent_looks_like_the_official_client() {
        let ua = cred("acct-1").user_agent();
        assert!(ua.starts_with(config::UA_PREFIX), "{ua}");
        assert!(ua.starts_with(&format!("{}{}", config::UA_PREFIX, config::CODEX_VERSION)), "{ua}");
        // 形态：`codex_cli_rs/<版本> (<OS>; <arch>) <终端>`
        assert!(ua.contains("; ") && ua.contains(") "), "{ua}");
        assert_eq!(config::UA_PREFIX, format!("{}/", config::ORIGINATOR));
    }

    /// 派生串必须过 UUID v4 的形态校验：长度、连字符位置、版本位与变体位。
    #[test]
    fn session_id_looks_like_uuid_v4() {
        let s = cred("acct-1").session_id("fp");
        assert_eq!(s.len(), 36);
        assert_eq!(s.as_bytes()[14], b'4', "version nibble must be 4: {s}");
        assert!(matches!(s.as_bytes()[19], b'8' | b'9' | b'a' | b'b'), "variant bits: {s}");
        let parts: Vec<&str> = s.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
    }
}
