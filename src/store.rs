//! 多凭证的 SQLite 持久化层。
//!
//! 单连接 + `parking_lot::Mutex` 串行化；WAL + `synchronous=NORMAL`；STRICT 表 +
//! `CHECK`/`UNIQUE` 约束。token 轮换走单行 `UPDATE`，不重写整库。

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::credentials::{Credential, now_secs};

/// `credentials` 表的列清单。集中一处，免得 `SELECT` 与 [`row_to_cred`] 的下标对不上——
/// 那种错不会编译失败，只会让某一列静默取到隔壁的值。
const COLS: &str = "id, label, email, plan_type, account_id, id_token, access_token, \
                    refresh_token, expires_at, priority, disabled, rpm_limit, ban_reason, \
                    resume_at, proxy, created_at, updated_at";

/// 凭证 SQLite 存储。
pub struct CredentialStore {
    conn: Mutex<Connection>,
    /// 每凭证一把刷新锁，串行化 token 刷新。
    ///
    /// 上游刷新会**轮换 refresh_token**：并发刷新时后完成的那次会把已被作废的 token 写回
    /// 库，该凭证之后所有刷新都 `invalid_grant`，等于账号被自己废掉。
    refresh_locks: Mutex<HashMap<i64, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    /// 每账号 RPM 的限流窗口（进程内，窗口固定 [`RPM_WINDOW_SECS`]）。
    rpm_rate: RateWindow,
    /// 被上游 429 过的凭证的冷却表（进程内）。
    cooldown: Mutex<HashMap<i64, Instant>>,
    /// `settings` 全表的内存镜像。
    ///
    /// **每条转发请求要读好几项设置**（接入 key、RPM 默认值、重试次数…），逐项走 SQL 就是
    /// 每请求多次查询，且全部串行在上面那把全局 `conn` 锁上——转发路径的落库、后台的列表
    /// 查询都得排在它们后面。设置项极少变动，缓存住之后这些查询直接归零。
    ///
    /// 写路径只有 [`CredentialStore::set_setting`]/[`CredentialStore::delete_setting`] 两处，
    /// 都是先落库再更新缓存，故进程内不会漂移。**多进程共享同一个库时会读到陈旧值**——
    /// coban 是单进程本地代理，没有这个场景。
    settings: parking_lot::RwLock<HashMap<String, String>>,
}

/// 所有启用凭证都在限流冷却中。
#[derive(Debug)]
pub struct AllRateLimited {
    /// 最早一个能恢复的凭证还要等多少秒。
    pub retry_after_secs: i64,
}

impl std::fmt::Display for AllRateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "all credentials are rate limited; retry in {}s", self.retry_after_secs)
    }
}
impl std::error::Error for AllRateLimited {}

/// 该账号的 RPM 上限已打满。
#[derive(Debug)]
pub struct RpmLimited {
    pub limit: i64,
    pub retry_after_secs: i64,
}

impl std::fmt::Display for RpmLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "per-account RPM limit ({}) reached; retry in {}s",
            self.limit, self.retry_after_secs
        )
    }
}
impl std::error::Error for RpmLimited {}

// ---------- 进程内限流窗口 ----------

/// 滑动窗口计数器：记下每次放行的时刻，窗口外的自动淘汰。
///
/// **刻意放在进程内而不是库里**：这是每请求都要读写的高频路径，落库等于给每条转发加一次
/// 写事务（还要跟落账、选号抢同一把 `conn` 锁）。代价是重启后窗口清零、多进程各算各的——
/// coban 是单进程本地代理，两条都不构成问题。
#[derive(Default)]
struct RateWindow {
    hits: Mutex<HashMap<i64, VecDeque<Instant>>>,
}

impl RateWindow {
    /// 尝试占一个名额。`limit <= 0` 表示不限，直接放行。
    ///
    /// 返回 `Err(还要等几秒)` 表示已满——那个秒数取自**窗口内最早那次**的到期时刻，
    /// 而不是一个拍脑袋的固定值：客户端照着它退避一次就正好有名额，不会空转重试。
    fn take(&self, key: i64, limit: i64, window: Duration) -> Result<(), i64> {
        if limit <= 0 {
            return Ok(());
        }
        let now = Instant::now();
        let mut map = self.hits.lock();
        let q = map.entry(key).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) >= window) {
            q.pop_front();
        }
        if (q.len() as i64) < limit {
            q.push_back(now);
            return Ok(());
        }
        let oldest = q.front().copied().unwrap_or(now);
        let wait = window.saturating_sub(now.duration_since(oldest));
        Err(wait.as_secs().max(1) as i64)
    }

    /// 当前窗口内已用了几个名额（只读，不占名额）。
    fn used(&self, key: i64, window: Duration) -> i64 {
        let now = Instant::now();
        let mut map = self.hits.lock();
        let Some(q) = map.get_mut(&key) else { return 0 };
        while q.front().is_some_and(|t| now.duration_since(*t) >= window) {
            q.pop_front();
        }
        q.len() as i64
    }
}

// ---------- 设置项 ----------

/// 接入用的 API Key（网页可改；命令行/环境变量优先且令网页只读）。
pub const CLIENT_API_KEY: &str = "client_api_key";
/// 管理密码的 sha256。
pub const ADMIN_PASSWORD: &str = "admin_password_sha256";
/// 全局默认的每账号 RPM 上限（0 = 不限）。
pub const DEFAULT_RPM_LIMIT: &str = "default_rpm_limit";
/// 一条请求被上游拒掉后最多再换几个账号——**只管链路/上游故障那一类**（连不上、
/// token 刷不出来）。撞限流换号不受这个数字约束：那类拒绝会把号排掉并打上冷却，继续换
/// 就是把号池里还能用的号找出来，只受一条请求的尝试硬顶约束（见 `proxy::RotationBudget`）。
///
/// `0` 仍是总开关：一次都不换，上游的判决（含 429）原样交回客户端。
pub const RATE_LIMIT_RETRY_MAX: &str = "rate_limit_retry_max";
/// 同上的默认值。
pub const DEFAULT_RATE_LIMIT_RETRY_MAX: i64 = 2;
/// 额度用到百分之多少就暂停这个账号（0 = 不暂停）。
pub const QUOTA_PAUSE_PCT: &str = "quota_pause_pct";
/// 同上的默认值。留 10% 余量，免得一条长请求跑到一半正好把额度撞穿。
pub const DEFAULT_QUOTA_PAUSE_PCT: i64 = 90;
/// 撞 429 后该账号冷却多久（秒）。
pub const COOLDOWN_SECS: &str = "cooldown_secs";
/// 同上的默认值。
pub const DEFAULT_COOLDOWN_SECS: i64 = 60;

/// RPM 的窗口长度（秒）。
pub const RPM_WINDOW_SECS: u64 = 60;

/// 用量流水的保留期：超过就裁掉（终身口径落在 `credential_stats` 账本里，不受影响）。
///
/// 公开是因为它同时是**「能回看多远」的上限**：缓存命中率那条趋势线读的就是这张表
/// （见 [`CredentialStore::cache_series`]），接口按它夹住请求的时间跨度，好过让人问一段
/// 早被裁掉的历史、再收到一条无声变短的曲线。
pub const USAGE_LOG_RETENTION_SECS: i64 = 30 * 24 * 3600;

/// 算出该账号实际生效的 RPM 上限。
///
/// 三态：`> 0` 用它自己的；`0` 跟随全局默认；`< 0` 明确不限（**能顶掉全局默认**，
/// 这正是这一态存在的理由——否则「全局限 60、唯独这个号不限」表达不出来）。
pub fn effective_rpm_limit(cred_limit: i64, default_limit: i64) -> i64 {
    match cred_limit {
        0 => default_limit,
        n if n < 0 => 0,
        n => n,
    }
}

// ---------- 用量记录 ----------

/// 一次转发的用量记录，转发结束后落库。
#[derive(Debug, Default)]
pub struct UsageRecord {
    pub cred_id: Option<i64>,
    pub cred_label: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub path: String,
    /// 来访客户端自报的 UA（已截断）。
    pub ua: Option<String>,
    pub status: i64,
    /// 是否从响应里解析到用量。未解析到时下面各 token 列为空——**记空值而不是 0**，
    /// 0 会被平均值统计当成一次真实的「零消耗请求」。
    pub has_usage: bool,
    pub input_tokens: Option<i64>,
    pub cached_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    /// 首字节耗时（毫秒）。
    pub ttft_ms: Option<i64>,
    pub total_ms: Option<i64>,
    pub cost_usd: Option<f64>,
    /// 上游限流头的快照，见 [`QuotaSnapshot`]。
    pub quota: Option<QuotaSnapshot>,
}

/// 上游 `x-codex-*` 限流头的一次快照。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct QuotaSnapshot {
    pub primary_used_pct: Option<f64>,
    pub primary_window_minutes: Option<i64>,
    pub primary_reset_at: Option<String>,
    pub secondary_used_pct: Option<f64>,
    pub secondary_window_minutes: Option<i64>,
    pub secondary_reset_at: Option<String>,
    pub credits_has_credits: Option<bool>,
    pub credits_unlimited: Option<bool>,
    pub credits_balance: Option<f64>,
}

/// 上游的 `*_reset_at` → Unix 秒。
///
/// 它是个字符串，见过秒也见过毫秒，故超过阈值时按毫秒解。解不出来返回 `None`——猜一个错的
/// 重置时刻，会让窗口起点也跟着错，于是窗口统计算的是一段根本不对的时间。
pub fn parse_reset_at(raw: &str) -> Option<i64> {
    let n: i64 = raw.trim().parse().ok()?;
    let secs = if n > 100_000_000_000 { n / 1000 } else { n };
    (secs > 0).then_some(secs)
}

impl QuotaSnapshot {
    /// 是否一项都没解出来（此时不该覆盖账本里已有的快照）。
    pub fn is_empty(&self) -> bool {
        self.primary_used_pct.is_none()
            && self.secondary_used_pct.is_none()
            && self.credits_balance.is_none()
    }

    /// 把这次没报的那几项从上一份快照里补齐。
    ///
    /// 上游**并非每条响应都把九项报全**（见过只带 credits 那一组的）。一次响应只说明了
    /// 它真的带回来的那几项，对没带的那些它什么也没说——整份替换会把它们抹成 `null`，
    /// 表现是卡片上的主额度条突然空了一格，而那个号的额度其实一点没变。
    pub fn filled_from(mut self, older: &Self) -> Self {
        self.primary_used_pct = self.primary_used_pct.or(older.primary_used_pct);
        self.primary_window_minutes = self.primary_window_minutes.or(older.primary_window_minutes);
        self.primary_reset_at =
            self.primary_reset_at.take().or_else(|| older.primary_reset_at.clone());
        self.secondary_used_pct = self.secondary_used_pct.or(older.secondary_used_pct);
        self.secondary_window_minutes =
            self.secondary_window_minutes.or(older.secondary_window_minutes);
        self.secondary_reset_at =
            self.secondary_reset_at.take().or_else(|| older.secondary_reset_at.clone());
        self.credits_has_credits = self.credits_has_credits.or(older.credits_has_credits);
        self.credits_unlimited = self.credits_unlimited.or(older.credits_unlimited);
        self.credits_balance = self.credits_balance.or(older.credits_balance);
        self
    }

    /// 两个窗口里用得最多的那个百分比——判「该不该暂停这个号」时看它。
    pub fn peak_used_pct(&self) -> Option<f64> {
        match (self.primary_used_pct, self.secondary_used_pct) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }
}

/// 一条用量流水（发给前端）。
#[derive(Debug, serde::Serialize)]
pub struct UsageLog {
    pub id: i64,
    pub ts: i64,
    pub cred_id: Option<i64>,
    pub cred_label: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub path: String,
    /// 来访客户端自报的 UA（已截断）。认「谁在发」用它。
    pub ua: Option<String>,
    pub status: i64,
    pub input_tokens: Option<i64>,
    pub cached_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub total_ms: Option<i64>,
    pub cost_usd: Option<f64>,
}

/// 一页用量流水 + 这一轮筛选的合计。
#[derive(Debug, serde::Serialize)]
pub struct UsagePage {
    pub logs: Vec<UsageLog>,
    /// 同一套筛选条件下的总条数（供前端算页数）。
    pub total: i64,
    /// 同一套筛选条件下的总花费（USD）。
    pub total_cost: f64,
    /// 同一套筛选条件下的输入 token 合计（**已含命中缓存那部分**）与其中命中缓存的部分。
    ///
    /// 两个数一起回、由界面算缓存命中率（`cached / input`），而不是在这里算好一个百分比：
    /// **只有这两个原始数才能让人判断那个比率作不作数**——一屏 300 token 的小请求算出来的
    /// 「命中 0%」和 17K token 前缀上的「命中 94%」是两件事。
    ///
    /// 分母刻意用 `input_tokens` 而不是 `total_tokens`：输出 token 与缓存无关，掺进分母只会
    /// 把命中率按「这一轮模型说了多少话」稀释。缺失（没嗅探到 usage）的行按 0 计入——
    /// SQL 的 `SUM` 本来就跳过 NULL。
    pub total_input_tokens: i64,
    pub total_cached_tokens: i64,
    /// 本轮翻页的锚点（Unix 秒），下一页原样带回。
    pub anchor: Option<i64>,
}

/// 缓存命中率趋势里的一个**小时桶**：`ts` 是这一小时的起点（Unix 秒）。
///
/// 与 [`UsagePage`] 上那两项同一个取舍——回两个原始数，比率由界面算：一个 300 token 的
/// 小时里的「命中 0%」与 17K 前缀那种小时里的「命中 94%」是两件事，光看比率判断不了。
#[derive(Debug, Clone, serde::Serialize)]
pub struct CacheBucket {
    pub ts: i64,
    pub input_tokens: i64,
    pub cached_tokens: i64,
}

/// 一个额度窗口的**当前周期内**已经发生了什么。
///
/// 与终身账本（[`CredentialStats`] 上那几个 `*_total`）互补：账号跑了多久是一回事，
/// 「这一个 5 小时/一周窗口里压了多少」才是决定它此刻还能不能接活的那个数。三项一起给，
/// 因为它们**互相不成正比**——命中缓存的输入按十分之一计价，重度吃缓存的号会呈现
/// 「token 一大堆、花费很少」，只看其中一个会得出相反的结论。
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct WindowUsage {
    /// 窗口内经这个账号转发的流水条数（含失败的）。
    pub requests: i64,
    /// 窗口内的 token 总数。
    ///
    /// 取上游报的 `total_tokens`，缺它时退回 `input + output`。**不加 cached/reasoning**：
    /// 上游的 `input_tokens` 已含命中缓存那部分、`output_tokens` 已含 reasoning，
    /// 再加一次就是同一批 token 数两遍。
    pub tokens: i64,
    /// 窗口内的等价 API 费用（USD）。价目表认不出的模型那几条记 0（它们的 `cost_usd` 是
    /// NULL），所以这个数是**下限**。
    pub cost_usd: f64,
}

/// 每个凭证的终身账本 + 最新额度快照（发给前端）。
#[derive(Debug, Default, serde::Serialize)]
pub struct CredentialStats {
    pub last_used_at: Option<i64>,
    pub cost_total_usd: f64,
    pub request_total: i64,
    /// 输入 token 终身累计。**已含命中缓存的那部分**（上游报的 `input_tokens` 就是这个口径）。
    pub input_tokens_total: i64,
    /// 其中命中缓存的部分——是 `input_tokens_total` 的**子集**，不是另一笔。
    pub cached_tokens_total: i64,
    pub output_tokens_total: i64,
    pub snapshot_ts: Option<i64>,
    pub quota: Option<QuotaSnapshot>,
    /// 主/次额度窗口**当前周期内**的用量。上游没报这个窗口（没有重置时刻或窗口长度为 0）
    /// 时为 `None`——与「这个周期里一条都没跑」（各项为 0）是两件事，界面必须分开显示。
    pub primary_window: Option<WindowUsage>,
    pub secondary_window: Option<WindowUsage>,
}

// ---------- 打开与建表 ----------

impl CredentialStore {
    /// 数据库文件路径。默认 `~/.coban/coban.db`；`COBAN_HOME` 可覆盖基目录。
    pub fn db_path() -> Result<PathBuf> {
        let base = match std::env::var_os("COBAN_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::home_dir()
                .context("could not determine the user home directory")?
                .join(".coban"),
        };
        Ok(base.join("coban.db"))
    }

    /// 在默认路径打开（或新建）凭证库并初始化 schema。
    pub fn open_default() -> Result<Self> {
        let path = Self::db_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory: {}", parent.display()))?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("failed to open credential database: {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        init_schema(&conn)?;
        Ok(Self::with_conn(conn))
    }

    /// 内存库（**仅测试**）：schema 已初始化，进程退出即消失。
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        init_schema(&conn)?;
        Ok(Self::with_conn(conn))
    }

    fn with_conn(conn: Connection) -> Self {
        // 设置表整张读进内存。读失败（表还不存在等）就从空表起步，所有取值退回各自的
        // 默认值——绝不能因为读设置失败而让整个服务起不来。
        let settings = load_settings(&conn).unwrap_or_default();
        Self {
            conn: Mutex::new(conn),
            refresh_locks: Mutex::new(HashMap::new()),
            rpm_rate: RateWindow::default(),
            cooldown: Mutex::new(HashMap::new()),
            settings: parking_lot::RwLock::new(settings),
        }
    }
}

/// 建表 / 迁移。每次启动都跑，全部幂等。
fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS credentials (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            label         TEXT    NOT NULL DEFAULT '',
            email         TEXT,
            plan_type     TEXT,
            -- 转发必需：缺了它 `chatgpt-account-id` 头发不出去，上游一律 401。
            account_id    TEXT    NOT NULL,
            id_token      TEXT,
            access_token  TEXT    NOT NULL,
            refresh_token TEXT    NOT NULL,
            expires_at    INTEGER NOT NULL,
            priority      INTEGER NOT NULL DEFAULT 0,
            disabled      INTEGER NOT NULL DEFAULT 0 CHECK (disabled IN (0,1)),
            rpm_limit     INTEGER NOT NULL DEFAULT 0,
            ban_reason    TEXT,
            resume_at     INTEGER,
            proxy         TEXT,
            created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
            updated_at    INTEGER NOT NULL DEFAULT (unixepoch())
        ) STRICT;

        -- 同一个 refresh_token 只该存一份：重复登录同一账号时应当更新那一行而不是
        -- 再插一条（见 upsert）。没有这条约束的话，一个账号会攒出一串各自持有已作废
        -- token 的僵尸行。
        CREATE UNIQUE INDEX IF NOT EXISTS uq_credentials_refresh_token
            ON credentials(refresh_token);
        CREATE INDEX IF NOT EXISTS idx_credentials_priority
            ON credentials(priority, id);

        CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        ) STRICT;

        CREATE TABLE IF NOT EXISTS usage_logs (
            id             INTEGER PRIMARY KEY,
            ts             INTEGER NOT NULL DEFAULT (unixepoch()),
            cred_id        INTEGER,
            cred_label     TEXT    NOT NULL DEFAULT '',
            session_id     TEXT,
            model          TEXT,
            path           TEXT    NOT NULL DEFAULT '',
            ua             TEXT,
            status         INTEGER NOT NULL DEFAULT 0,
            has_usage      INTEGER NOT NULL DEFAULT 0 CHECK (has_usage IN (0,1)),
            input_tokens     INTEGER,
            cached_tokens    INTEGER,
            output_tokens    INTEGER,
            reasoning_tokens INTEGER,
            total_tokens     INTEGER,
            ttft_ms        INTEGER,
            total_ms       INTEGER,
            cost_usd       REAL,
            -- 上游限流头的原始快照（JSON，见 QuotaSnapshot）。字段变化时仍可回看。
            quota_raw      TEXT
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_usage_logs_ts ON usage_logs(ts);
        -- 账号列表的每一项统计都是「按 cred_id 分组、按 ts 卡窗口」。带上 ts 后这些聚合
        -- 只扫索引，不必回表逐行看时间。
        CREATE INDEX IF NOT EXISTS idx_usage_logs_cred_ts ON usage_logs(cred_id, ts);

        -- 账本：终身累计 + 最新额度快照，与 usage_logs 的插入在同一事务内更新。
        -- 分工：usage_logs 是流水，只保留近期（见 prune_usage_logs）；「最近使用 /
        -- 累计费用 / 最新快照」这些终身口径落在这里，才不随流水裁剪一起变小。
        CREATE TABLE IF NOT EXISTS credential_stats (
            cred_id        INTEGER PRIMARY KEY,
            last_used_at   INTEGER,
            cost_total_usd REAL    NOT NULL DEFAULT 0,
            request_total  INTEGER NOT NULL DEFAULT 0,
            -- token 终身累计。cached 是 input 的**子集**（上游报的 input 已含它），
            -- 求和展示时不能三个一起加，见 CredentialStats::billable_tokens。
            input_tokens_total  INTEGER NOT NULL DEFAULT 0,
            cached_tokens_total INTEGER NOT NULL DEFAULT 0,
            output_tokens_total INTEGER NOT NULL DEFAULT 0,
            snapshot_ts    INTEGER,
            quota_raw      TEXT
        ) STRICT;",
    )
    .context("failed to initialize credential database schema")?;
    migrate_token_totals(conn)?;
    Ok(())
}

/// 给 `credential_stats` 补上 token 累计那三列。
///
/// **`CREATE TABLE IF NOT EXISTS` 不会改动已存在的表**，所以旧库要靠 `ALTER TABLE` 补列。
/// 幂等：先问 `pragma_table_info`，有了就直接返回。STRICT 表上 `ADD COLUMN` 要求带
/// `DEFAULT`（旧行按它取值），这三列都是 `DEFAULT 0`。
///
/// 补完顺手从 `usage_logs` 回填一次。账本是**终身**口径而流水只留 30 天（见
/// [`USAGE_LOG_RETENTION_SECS`]），所以这次回填只能覆盖还没被裁掉的那一段——不完整，
/// 但比让每个账号的 token 数从 0 重新开始要贴近事实。**只在真的新加了列的那一次执行**，
/// 不会重复累加。
fn migrate_token_totals(conn: &Connection) -> Result<()> {
    let has_column: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('credential_stats') WHERE name = ?1")?
        .exists(params!["input_tokens_total"])?;
    if has_column {
        return Ok(());
    }
    conn.execute_batch(
        "ALTER TABLE credential_stats ADD COLUMN input_tokens_total  INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE credential_stats ADD COLUMN cached_tokens_total INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE credential_stats ADD COLUMN output_tokens_total INTEGER NOT NULL DEFAULT 0;",
    )
    .context("failed to add token total columns to credential_stats")?;
    let filled = conn.execute(
        "UPDATE credential_stats SET
             input_tokens_total  = COALESCE((SELECT SUM(input_tokens)  FROM usage_logs
                                             WHERE cred_id = credential_stats.cred_id), 0),
             cached_tokens_total = COALESCE((SELECT SUM(cached_tokens) FROM usage_logs
                                             WHERE cred_id = credential_stats.cred_id), 0),
             output_tokens_total = COALESCE((SELECT SUM(output_tokens) FROM usage_logs
                                             WHERE cred_id = credential_stats.cred_id), 0)",
        [],
    )?;
    tracing::info!(
        credentials = filled,
        "schema migrated: token totals added and backfilled from the retained usage logs"
    );
    Ok(())
}

/// 把 `settings` 整张表读进内存。
fn load_settings(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut stmt = conn.prepare("SELECT key, value FROM settings")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
}

/// 会话键 → 档内固定的那一个号（HRW / rendezvous 选择）。
///
/// **不用「哈希取模」**：候选集一变（某个号进冷却、被额度暂停、这次已经试过），取模会把几乎
/// 所有键重新打乱一遍，而每一次改落点都是一整段前缀的缓存未命中——真实流量里那是十几万
/// token 一次。HRW 只让原本落在消失那个号上的键（约 1/N）改主，其余不动。
///
/// 同分按 id 定序：选号必须可复现，否则同一个会话在两条并发请求上会分到两个号。
fn sticky_pick(ids: &[i64], key: &str) -> Option<i64> {
    use sha2::{Digest, Sha256};
    ids.iter()
        .max_by_key(|id| {
            let mut h = Sha256::new();
            h.update(key.as_bytes());
            h.update([0u8]); // 分隔符，避免拼接歧义
            h.update(id.to_be_bytes());
            let d = h.finalize();
            let mut head = [0u8; 8];
            head.copy_from_slice(&d[..8]);
            (u64::from_be_bytes(head), **id)
        })
        .copied()
}

/// 一行 → [`Credential`]。列序必须与 [`COLS`] 一致。
fn row_to_cred(row: &Row) -> rusqlite::Result<Credential> {
    Ok(Credential {
        id: row.get(0)?,
        label: row.get(1)?,
        email: row.get(2)?,
        plan_type: row.get(3)?,
        account_id: row.get(4)?,
        id_token: row.get(5)?,
        access_token: row.get(6)?,
        refresh_token: row.get(7)?,
        expires_at: row.get::<_, i64>(8)? as u64,
        priority: row.get(9)?,
        disabled: row.get::<_, i64>(10)? != 0,
        rpm_limit: row.get(11)?,
        ban_reason: row.get(12)?,
        resume_at: row.get::<_, Option<i64>>(13)?.map(|v| v as u64),
        proxy: row.get(14)?,
        created_at: row.get::<_, i64>(15)? as u64,
        updated_at: row.get::<_, i64>(16)? as u64,
    })
}

// ---------- 凭证 CRUD ----------

impl CredentialStore {
    /// 插入或更新一条凭证。
    ///
    /// **按 `account_id` 去重而不是按 refresh_token**：同一个账号重新登录一次会拿到全新的
    /// refresh_token，按 token 去重就会给同一个账号攒出第二行——两行各持一半的用量历史，
    /// 而且老那行的 token 已经作废、每次选中它都是一次失败的转发。账号 id 是稳定的，
    /// 认它才对得上「同一个账号」。
    pub fn upsert(
        &self,
        label: &str,
        email: Option<&str>,
        plan_type: Option<&str>,
        account_id: &str,
        id_token: Option<&str>,
        access_token: &str,
        refresh_token: &str,
        expires_at: u64,
    ) -> Result<(Credential, bool)> {
        anyhow::ensure!(
            !account_id.trim().is_empty(),
            "this credential has no ChatGPT account id; forwarding would fail with 401. \
             Make sure you authorized the Codex app (not a plain OpenAI API key)."
        );
        let conn = self.conn.lock();
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM credentials WHERE account_id = ?1",
                params![account_id],
                |r| r.get(0),
            )
            .optional()?;
        let id = match existing {
            Some(id) => {
                conn.execute(
                    "UPDATE credentials SET email = ?1, plan_type = ?2, id_token = ?3, \
                         access_token = ?4, refresh_token = ?5, expires_at = ?6, \
                         ban_reason = NULL, resume_at = NULL, updated_at = unixepoch() \
                     WHERE id = ?7",
                    params![
                        email,
                        plan_type,
                        id_token,
                        access_token,
                        refresh_token,
                        expires_at as i64,
                        id
                    ],
                )?;
                id
            }
            None => {
                conn.execute(
                    "INSERT INTO credentials
                         (label, email, plan_type, account_id, id_token, access_token,
                          refresh_token, expires_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        label,
                        email,
                        plan_type,
                        account_id,
                        id_token,
                        access_token,
                        refresh_token,
                        expires_at as i64
                    ],
                )
                .context("failed to insert credential")?;
                conn.last_insert_rowid()
            }
        };
        let cred = conn.query_row(
            &format!("SELECT {COLS} FROM credentials WHERE id = ?1"),
            params![id],
            row_to_cred,
        )?;
        Ok((cred, existing.is_none()))
    }

    /// 列出全部凭证（按优先级、id 排序）。
    pub fn list(&self) -> Result<Vec<Credential>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare(&format!("SELECT {COLS} FROM credentials ORDER BY priority, id"))?;
        let rows = stmt.query_map([], row_to_cred)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 按 id 取一条。
    pub fn get(&self, id: i64) -> Result<Option<Credential>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(
                &format!("SELECT {COLS} FROM credentials WHERE id = ?1"),
                params![id],
                row_to_cred,
            )
            .optional()?)
    }

    /// 删除一条，返回是否真的删掉了。
    ///
    /// 连带删掉账本与流水：不删的话，id 复用时新账号会凭空继承一段历史用量。
    /// （`credentials.id` 是 AUTOINCREMENT，正常不复用，但导入/迁移过的库不保证。）
    pub fn delete(&self, id: i64) -> Result<bool> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let n = tx.execute("DELETE FROM credentials WHERE id = ?1", params![id])?;
        tx.execute("DELETE FROM credential_stats WHERE cred_id = ?1", params![id])?;
        tx.execute("DELETE FROM usage_logs WHERE cred_id = ?1", params![id])?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// 清空全部凭证，返回删掉几条。
    pub fn clear(&self) -> Result<usize> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let n = tx.execute("DELETE FROM credentials", [])?;
        tx.execute("DELETE FROM credential_stats", [])?;
        tx.execute("DELETE FROM usage_logs", [])?;
        tx.commit()?;
        Ok(n)
    }

    /// 停用 / 启用。
    ///
    /// 手动启用会一并清掉 `ban_reason` 与 `resume_at`：这两项是自动停用留下的痕迹，
    /// 人一旦明确说「开着」，就不该再被一个陈旧的自动判定关回去。
    pub fn set_disabled(&self, id: i64, disabled: bool) -> Result<()> {
        let conn = self.conn.lock();
        if disabled {
            conn.execute(
                "UPDATE credentials SET disabled = 1, updated_at = unixepoch() WHERE id = ?1",
                params![id],
            )?;
        } else {
            conn.execute(
                "UPDATE credentials SET disabled = 0, ban_reason = NULL, resume_at = NULL, \
                     updated_at = unixepoch() WHERE id = ?1",
                params![id],
            )?;
        }
        drop(conn);
        if !disabled {
            self.cooldown.lock().remove(&id);
        }
        Ok(())
    }

    pub fn set_priority(&self, id: i64, priority: i64) -> Result<()> {
        self.update_col("priority", id, priority)
    }

    pub fn set_rpm_limit(&self, id: i64, limit: i64) -> Result<()> {
        self.update_col("rpm_limit", id, limit)
    }

    pub fn set_label(&self, id: i64, label: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE credentials SET label = ?1, updated_at = unixepoch() WHERE id = ?2",
            params![label, id],
        )?;
        Ok(())
    }

    /// 设置 / 清除该账号的出站代理。`None` 或空串表示直连。
    ///
    /// 校验在 [`crate::clients::validate_proxy`]，**入库前就得过**——存进去一条建不出
    /// 客户端的代理，故障要等到下次选中这个号才暴露。
    pub fn set_proxy(&self, id: i64, proxy: Option<&str>) -> Result<()> {
        let normalized = match proxy.map(str::trim).filter(|s| !s.is_empty()) {
            Some(raw) => Some(crate::clients::validate_proxy(raw)?),
            None => None,
        };
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE credentials SET proxy = ?1, updated_at = unixepoch() WHERE id = ?2",
            params![normalized, id],
        )?;
        Ok(())
    }

    fn update_col(&self, col: &str, id: i64, value: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            &format!("UPDATE credentials SET {col} = ?1, updated_at = unixepoch() WHERE id = ?2"),
            params![value, id],
        )?;
        Ok(())
    }

    /// 刷新成功后写回新 token。
    pub fn update_tokens(
        &self,
        id: i64,
        access_token: &str,
        refresh_token: &str,
        expires_at: u64,
        id_token: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE credentials SET access_token = ?1, refresh_token = ?2, expires_at = ?3, \
                 id_token = COALESCE(?4, id_token), updated_at = unixepoch() WHERE id = ?5",
            params![access_token, refresh_token, expires_at as i64, id_token, id],
        )?;
        Ok(())
    }

    /// 用最新的 id_token 回填账号身份（邮箱、套餐档位）。
    ///
    /// 档位会变——用户从 Plus 升到 Pro、团队席位被收回，都只体现在新的 id_token 里。
    /// 不同步的话界面上永远显示注册那天的档位，而选号策略是人照着这个界面定的。
    ///
    /// 刷新响应没带 id_token 时退回库里存的那份：它至少反映上一次登录时的状态，
    /// 比把已有的档位抹成空好。解不出任何一项就什么都不写（`COALESCE` 只覆盖有值的列）。
    pub fn sync_identity(&self, cred: &Credential, new_id_token: Option<&str>) -> Result<()> {
        let Some(token) = new_id_token.or(cred.id_token.as_deref()) else { return Ok(()) };
        let claims = crate::oauth::Claims::parse(token);
        if claims.email.is_none() && claims.plan_type.is_none() {
            return Ok(());
        }
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE credentials SET email = COALESCE(?1, email), \
                 plan_type = COALESCE(?2, plan_type), updated_at = unixepoch() WHERE id = ?3",
            params![claims.email, claims.plan_type, cred.id],
        )?;
        Ok(())
    }

    /// 标记账号级失效（封号 / refresh_token 作废）并停用。
    ///
    /// 与限流停用的区别是**不设 `resume_at`**：这类失效不会自己好，等下去只是让每一轮
    /// 选号都白试一次。要重新启用得人工介入（重新登录或手动打开）。
    pub fn mark_banned(&self, id: i64, reason: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE credentials SET disabled = 1, ban_reason = ?1, resume_at = NULL, \
                 updated_at = unixepoch() WHERE id = ?2",
            params![reason, id],
        )?;
        tracing::warn!(
            cred_id = id,
            reason,
            "credential disabled: account-level error from upstream"
        );
        Ok(())
    }

    /// 因限流暂停这个账号一段时间（到点自动恢复）。
    pub fn pause_for_rate_limit(&self, id: i64, secs: i64) -> Result<()> {
        let until = now_secs() as i64 + secs.max(1);
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE credentials SET disabled = 1, resume_at = ?1, updated_at = unixepoch() \
             WHERE id = ?2",
            params![until, id],
        )?;
        Ok(())
    }

    /// 连通性测试通过后，把**因限流被自动暂停**的号放回池子；真的改了才返回 `true`。
    ///
    /// 条件卡在 `resume_at IS NOT NULL` 上——那是限流暂停独有的标记：封号走
    /// [`Self::mark_banned`]（它把 `resume_at` 清成 NULL），人工停用压根不设。少了这个条件，
    /// 一次手动探活就能把人工关掉的号打开、或让一个已封禁的号重回轮转。
    pub fn resume_if_rate_limited(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "UPDATE credentials SET disabled = 0, resume_at = NULL, updated_at = unixepoch() \
             WHERE id = ?1 AND resume_at IS NOT NULL",
            params![id],
        )?;
        Ok(n > 0)
    }

    /// 记一次上游限流：把该凭证放进冷却表，冷却期内选号跳过它。
    pub fn note_rate_limited(&self, id: i64, secs: i64) {
        let until = Instant::now() + Duration::from_secs(secs.max(1) as u64);
        self.cooldown.lock().insert(id, until);
    }

    /// 该凭证还要冷却几秒（不在冷却中返回 0）。
    pub fn cooldown_secs(&self, id: i64) -> i64 {
        let now = Instant::now();
        let mut map = self.cooldown.lock();
        match map.get(&id) {
            Some(&until) if until > now => (until - now).as_secs().max(1) as i64,
            Some(_) => {
                map.remove(&id);
                0
            }
            None => 0,
        }
    }

    /// 手动清掉冷却。
    pub fn clear_cooldown(&self, id: i64) {
        self.cooldown.lock().remove(&id);
    }
}

// ---------- 设置 ----------

impl CredentialStore {
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self.settings.read().get(key).cloned())
    }

    /// 取一个整数设置，缺失/不合法时用默认值。
    ///
    /// 不合法就退回默认而不是报错：设置是从网页写进来的字符串，一个手抖存进去的
    /// `"6O"` 不该让整条转发路径失败。
    pub fn get_setting_i64(&self, key: &str, default: i64) -> i64 {
        self.settings.read().get(key).and_then(|v| v.trim().parse().ok()).unwrap_or(default)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        {
            let conn = self.conn.lock();
            conn.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
        }
        // 先落库再更新缓存：反过来的话，落库失败会留下一个「内存里改了、库里没改」的
        // 状态，重启后设置悄悄变回去。
        self.settings.write().insert(key.to_owned(), value.to_owned());
        Ok(())
    }

    pub fn delete_setting(&self, key: &str) -> Result<()> {
        {
            let conn = self.conn.lock();
            conn.execute("DELETE FROM settings WHERE key = ?1", params![key])?;
        }
        self.settings.write().remove(key);
        Ok(())
    }
}

// ---------- 选号 ----------

impl CredentialStore {
    /// 挑一个可用凭证。
    ///
    /// 规则（依次）：
    /// 1. 把 `resume_at` 到点的号惰性启用回来——**只动 `resume_at` 非空的那些**，
    ///    人工关掉的号绝不能被自动打开；
    /// 2. 跳过停用的、还在冷却中的、RPM 已打满的；
    /// 3. 优先级小者优先；同档内取**最久未使用**的那个（按账本的 `last_used_at`），
    ///    这样同档账号是轮流用而不是把第一个榨干。
    ///
    /// `exclude` 里的 id 不参与——重试换号时用它排掉刚失败的那个。
    ///
    /// 全被限流时返回 [`AllRateLimited`]（带最短等待秒数），调用点据此回一个带
    /// `retry-after` 的 429，而不是一句没有下文的「没有可用账号」。
    ///
    /// `sticky` 是这条请求的会话键（见 [`crate::proxy`] 的 `prefix_fingerprint`）。给了就在
    /// **最优优先级档内**按它固定落点，没给则按档内 LRU 轮换。
    ///
    /// 只在档内粘：跨档粘会让一个低优先级的号因为哈希落点抢在高优先级前面，那是把分档这件
    /// 事本身推翻了。而档内那个号一旦不可用（停用/冷却/额度暂停/RPM 满/已试过），它压根
    /// 不在候选里，落点自然移到同档的下一个——降级不需要额外分支。
    pub fn select(&self, exclude: &[i64], sticky: Option<&str>) -> Result<Credential> {
        self.resume_due()?;
        let all = self.list()?;
        anyhow::ensure!(!all.is_empty(), "no credentials saved yet; add an account in the web UI");

        let default_rpm = self.get_setting_i64(DEFAULT_RPM_LIMIT, 0);
        let mut soonest: Option<i64> = None;
        let mut candidates: Vec<(i64, i64, i64)> = Vec::new(); // (priority, last_used_at, id)

        for c in &all {
            if c.disabled || exclude.contains(&c.id) {
                continue;
            }
            let cooling = self.cooldown_secs(c.id);
            if cooling > 0 {
                soonest = Some(soonest.map_or(cooling, |s: i64| s.min(cooling)));
                continue;
            }
            let limit = effective_rpm_limit(c.rpm_limit, default_rpm);
            // 这里只**看**用量不占名额：真正的占位在 [`Self::take_rpm_slot`]，由转发路径
            // 在确定要用这个号之后调用。两步分开是因为选号可能被后续检查否掉，
            // 那时候名额已经占掉就白扣了一个。
            if limit > 0 && self.rpm_rate.used(c.id, Duration::from_secs(RPM_WINDOW_SECS)) >= limit
            {
                soonest = Some(
                    soonest.map_or(RPM_WINDOW_SECS as i64, |s: i64| s.min(RPM_WINDOW_SECS as i64)),
                );
                continue;
            }
            candidates.push((c.priority, self.last_used_at(c.id).unwrap_or(0), c.id));
        }

        if candidates.is_empty() {
            if let Some(secs) = soonest {
                anyhow::bail!(AllRateLimited { retry_after_secs: secs });
            }
            anyhow::bail!(
                "no enabled credentials available ({} saved, all disabled or excluded)",
                all.len()
            );
        }
        candidates.sort_unstable();
        let id = match sticky.filter(|k| !k.is_empty()) {
            Some(key) => {
                let top = candidates[0].0;
                let tier: Vec<i64> =
                    candidates.iter().filter(|c| c.0 == top).map(|c| c.2).collect();
                sticky_pick(&tier, key).unwrap_or(candidates[0].2)
            }
            None => candidates[0].2,
        };
        all.into_iter().find(|c| c.id == id).context("selected credential vanished")
    }

    /// 占一个 RPM 名额。已满时返回 [`RpmLimited`]。
    pub fn take_rpm_slot(&self, cred: &Credential) -> Result<()> {
        let default_rpm = self.get_setting_i64(DEFAULT_RPM_LIMIT, 0);
        let limit = effective_rpm_limit(cred.rpm_limit, default_rpm);
        match self.rpm_rate.take(cred.id, limit, Duration::from_secs(RPM_WINDOW_SECS)) {
            Ok(()) => Ok(()),
            Err(wait) => anyhow::bail!(RpmLimited { limit, retry_after_secs: wait }),
        }
    }

    /// 该账号最近一个 RPM 窗口内已经发了多少条（只读，不占名额）。
    ///
    /// 给界面用：一个「当前 12 / 上限 60」比单看上限有用得多——上限是配置，这个数才是
    /// 现状。窗口在进程内存里，重启即清零（同 [`RateWindow`] 的取舍）。
    pub fn current_rpm(&self, cred_id: i64) -> i64 {
        self.rpm_rate.used(cred_id, Duration::from_secs(RPM_WINDOW_SECS))
    }

    /// 把到点的限流暂停解除。
    fn resume_due(&self) -> Result<()> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "UPDATE credentials SET disabled = 0, resume_at = NULL, updated_at = unixepoch() \
             WHERE resume_at IS NOT NULL AND resume_at <= ?1",
            params![now_secs() as i64],
        )?;
        if n > 0 {
            tracing::info!(count = n, "rate-limit pause expired; credentials re-enabled");
        }
        Ok(())
    }

    fn last_used_at(&self, cred_id: i64) -> Option<i64> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT last_used_at FROM credential_stats WHERE cred_id = ?1",
            params![cred_id],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()
        .ok()
        .flatten()
        .flatten()
    }
}

// ---------- token 刷新 ----------

impl CredentialStore {
    /// 取该凭证当前可用的 access_token，必要时先刷新。
    ///
    /// 刷新走**每凭证一把锁**：并发刷新会让后完成的那次把已被作废的 refresh_token 写回
    /// 库，此后该账号所有刷新都 `invalid_grant`。拿到锁之后**重新读一次库**——等锁期间
    /// 很可能别人已经刷好了，不重读就会拿着旧 token 再刷一次，正是这把锁要避免的事。
    ///
    /// 刷新被上游明确拒掉（`invalid_grant`）时把这个号标记为封禁并停用：那种状态不会
    /// 自己好，留着只会让每一轮选号都白试一次。
    pub async fn valid_access_token(
        &self,
        clients: &crate::clients::ClientPool,
        cred: &Credential,
    ) -> Result<String> {
        if !cred.needs_refresh() {
            return Ok(cred.access_token.clone());
        }
        let lock = self.refresh_lock(cred.id);
        let _guard = lock.lock().await;

        // 等锁期间别人可能已经刷过了。
        let fresh = self.get(cred.id)?.context("credential was deleted while refreshing")?;
        if !fresh.needs_refresh() {
            return Ok(fresh.access_token);
        }

        let client = clients.for_credential(&fresh)?;
        match refresh_with_retry(&client, &fresh.refresh_token).await {
            Ok(set) => {
                self.update_tokens(
                    fresh.id,
                    &set.access_token,
                    &set.refresh_token,
                    set.expires_at,
                    set.id_token.as_deref(),
                )?;
                // 档位可能在这次刷新里变了，顺手同步——这是唯一一条会定期跑到的路径。
                if let Err(e) = self.sync_identity(&fresh, set.id_token.as_deref()) {
                    tracing::warn!(cred_id = fresh.id, error = %e, "failed to sync account identity");
                }
                tracing::info!(cred_id = fresh.id, label = %fresh.label, "access token refreshed");
                Ok(set.access_token)
            }
            Err(e) => {
                // 只有「不会自己好」的那几种才停用（见 [`crate::oauth::is_permanent_refresh_error`]）。
                // 网络抖动、上游 5xx 也会走到这里，那些留着，下一条请求自然会再试一次——
                // 把它们也判成失效的话，一次机房抖动就能把整池账号关光。
                if crate::oauth::is_permanent_refresh_error(&format!("{e:#}")) {
                    self.mark_banned(
                        fresh.id,
                        "refresh token rejected by upstream (re-login required)",
                    )?;
                }
                Err(e)
            }
        }
    }

    #[allow(clippy::doc_markdown)]
    fn refresh_lock(&self, cred_id: i64) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        self.refresh_locks.lock().entry(cred_id).or_default().clone()
    }
}

/// 刷新一次 token，**只在确定请求没发出去时**才重试。
///
/// 上游的 refresh_token 是单次可用的（见 [`crate::oauth::PERMANENT_REFRESH_ERRORS`]），
/// 所以「失败就退避重试」这条常规做法在这里是有害的：请求已经打到上游、只是响应没收到的
/// 话，token 其实已经轮换，拿同一个再试一次得到的是 `refresh_token_reused`，于是一个健康
/// 账号被我们自己的重试判成永久失效。
///
/// 故重试条件收窄到「连上游都没连上」——`wreq` 的 connect 类错误。那种情形下上游不可能
/// 处理过这个 token，重试是安全的。收到了任何 HTTP 响应（含 5xx）就不再重试：那说明请求
/// 确实到达了，重试的收益远小于废掉一个号的代价。
async fn refresh_with_retry(
    client: &wreq::Client,
    refresh_token: &str,
) -> Result<crate::oauth::TokenSet> {
    const MAX_ATTEMPTS: u32 = 3;
    const BACKOFF_BASE: Duration = Duration::from_secs(1);

    let mut attempt = 1;
    loop {
        match crate::oauth::refresh_token(client, refresh_token).await {
            Ok(set) => return Ok(set),
            Err(e) if attempt < MAX_ATTEMPTS && is_connect_error(&e) => {
                // 指数退避，同 sub2api 的 `RetryBackoffSeconds * 2^(attempt-1)`。
                let backoff = BACKOFF_BASE * 2u32.pow(attempt - 1);
                tracing::warn!(
                    attempt,
                    backoff_secs = backoff.as_secs(),
                    error = %format!("{e:#}"),
                    "token refresh could not reach upstream, retrying"
                );
                tokio::time::sleep(backoff).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// 这个错误是不是「压根没连上上游」。
///
/// 判据只认 `wreq::Error::is_connect()`：超时（`is_timeout`）**刻意不算**——读超时时请求
/// 很可能已经被上游处理过了，重试就是拿一个已轮换的 token 去撞 `refresh_token_reused`。
fn is_connect_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<wreq::Error>().is_some_and(wreq::Error::is_connect))
}

// ---------- 用量 ----------

impl CredentialStore {
    /// 落一条用量流水，并在**同一个事务**里更新账本。
    ///
    /// 同事务是关键：分两次写的话，中间崩一次就会出现「流水有、账本没有」的偏差，
    /// 而账本是终身口径、永远补不回来（流水 30 天后就被裁掉了）。
    pub fn insert_usage_log(&self, rec: &UsageRecord) -> Result<()> {
        let quota_raw = rec
            .quota
            .as_ref()
            .filter(|q| !q.is_empty())
            .map(|q| serde_json::to_string(q))
            .transpose()?;
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO usage_logs
                 (cred_id, cred_label, session_id, model, path, ua, status, has_usage,
                  input_tokens, cached_tokens, output_tokens, reasoning_tokens, total_tokens,
                  ttft_ms, total_ms, cost_usd, quota_raw)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            params![
                rec.cred_id,
                rec.cred_label,
                rec.session_id,
                rec.model,
                rec.path,
                rec.ua,
                rec.status,
                rec.has_usage as i64,
                rec.input_tokens,
                rec.cached_tokens,
                rec.output_tokens,
                rec.reasoning_tokens,
                rec.total_tokens,
                rec.ttft_ms,
                rec.total_ms,
                rec.cost_usd,
                quota_raw,
            ],
        )?;
        if let Some(cred_id) = rec.cred_id {
            // 解析不到用量时各 token 记 0：账本是求和，`None` 与 0 在这里同义
            // （流水那边仍存 NULL，因为均值统计要分得清「没有读数」和「零消耗」）。
            tx.execute(
                "INSERT INTO credential_stats
                     (cred_id, last_used_at, cost_total_usd, request_total,
                      input_tokens_total, cached_tokens_total, output_tokens_total)
                 VALUES (?1, unixepoch(), ?2, 1, ?3, ?4, ?5)
                 ON CONFLICT(cred_id) DO UPDATE SET
                     last_used_at   = unixepoch(),
                     cost_total_usd = cost_total_usd + ?2,
                     request_total  = request_total + 1,
                     input_tokens_total  = input_tokens_total  + ?3,
                     cached_tokens_total = cached_tokens_total + ?4,
                     output_tokens_total = output_tokens_total + ?5",
                params![
                    cred_id,
                    rec.cost_usd.unwrap_or(0.0),
                    rec.input_tokens.unwrap_or(0),
                    rec.cached_tokens.unwrap_or(0),
                    rec.output_tokens.unwrap_or(0),
                ],
            )?;
            // 快照只在这次**真的解出了限流头**时才覆盖：一条没带头的响应（错误、翻译层
            // 就拒掉的请求、非流式的小接口）不该把上一次的额度读数抹成空。
            //
            // 带了头也**只覆盖它真的报了的那几项**，其余从上一份补齐（见
            // [`QuotaSnapshot::filled_from`]）。流水那一行仍存这次响应的原样读数——
            // 它记的是「这条请求看到了什么」，合并会把它变成一份没有哪条响应说过的数。
            if let Some(fresh) = rec.quota.as_ref().filter(|q| !q.is_empty()) {
                let older: Option<QuotaSnapshot> = tx
                    .query_row(
                        "SELECT quota_raw FROM credential_stats WHERE cred_id = ?1",
                        params![cred_id],
                        |r| r.get::<_, Option<String>>(0),
                    )
                    .optional()?
                    .flatten()
                    .and_then(|raw| serde_json::from_str(&raw).ok());
                let merged = match &older {
                    Some(old) => fresh.clone().filled_from(old),
                    None => fresh.clone(),
                };
                tx.execute(
                    "UPDATE credential_stats SET snapshot_ts = unixepoch(), quota_raw = ?1 \
                     WHERE cred_id = ?2",
                    params![serde_json::to_string(&merged)?, cred_id],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 取某个凭证的账本。
    pub fn stats_of(&self, cred_id: i64) -> Result<CredentialStats> {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT last_used_at, cost_total_usd, request_total, snapshot_ts, quota_raw,
                        input_tokens_total, cached_tokens_total, output_tokens_total
                 FROM credential_stats WHERE cred_id = ?1",
                params![cred_id],
                |r| {
                    Ok((
                        r.get::<_, Option<i64>>(0)?,
                        r.get::<_, f64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, i64>(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            last_used_at,
            cost_total_usd,
            request_total,
            snapshot_ts,
            quota_raw,
            input_tokens_total,
            cached_tokens_total,
            output_tokens_total,
        )) = row
        else {
            return Ok(CredentialStats::default());
        };
        // **先把锁放掉**：下面的 window_usage 要自己拿一次 conn，而这把锁不可重入
        // （parking_lot::Mutex），握着它调过去就是死锁——表现为整个账号列表接口卡住不返回。
        drop(conn);
        // 解不出来就当没有：一条坏掉的快照 JSON 不该让整个账号列表接口 500。
        let quota: Option<QuotaSnapshot> = quota_raw.and_then(|s| serde_json::from_str(&s).ok());
        let (primary_window, secondary_window) = self.window_usage(cred_id, quota.as_ref())?;
        Ok(CredentialStats {
            last_used_at,
            cost_total_usd,
            request_total,
            input_tokens_total,
            cached_tokens_total,
            output_tokens_total,
            snapshot_ts,
            quota,
            primary_window,
            secondary_window,
        })
    }

    /// 两个额度窗口**当前周期内**的请求数 / token / 费用。
    ///
    /// 窗口起点由快照反推：`重置时刻 - 窗口长度`。上游只报「还有多久重置」和「窗口多长」，
    /// 不报起点，而 coban **不写死窗口长度**（同一个账号在不同套餐下 5h/7d 各不相同，
    /// 见 `QuotaWindow` 的注），所以两项缺任何一个就判定这个窗口没被报告、返回 `None`。
    ///
    /// 统计从 `usage_logs` 现算而不是另立账本：窗口起点会随每次快照移动，累加式的账本
    /// 没法回退。流水的保留期（30 天，见 [`USAGE_LOG_RETENTION_SECS`]）远长于最长的窗口，
    /// 所以窗口内的行不会被裁掉。
    fn window_usage(
        &self,
        cred_id: i64,
        quota: Option<&QuotaSnapshot>,
    ) -> Result<(Option<WindowUsage>, Option<WindowUsage>)> {
        let Some(q) = quota else { return Ok((None, None)) };
        let start = |reset: &Option<String>, minutes: Option<i64>| -> Option<i64> {
            let reset_at = parse_reset_at(reset.as_deref()?)?;
            // 长度为 0 的窗口不是窗口：实测 Pro 账号的 secondary 那组头回的就是
            // `0% / 0 分钟 / 空重置时刻`，按它反推会得到「起点 = 重置时刻」的空窗口。
            Some(reset_at - minutes.filter(|m| *m > 0)? * 60)
        };
        let primary = start(&q.primary_reset_at, q.primary_window_minutes);
        let secondary = start(&q.secondary_reset_at, q.secondary_window_minutes);
        let Some(floor) = [primary, secondary].into_iter().flatten().min() else {
            return Ok((None, None));
        };

        let conn = self.conn.lock();
        // `ts >= ?4` 这个下界是给索引用的（idx_usage_logs_cred_ts）：没有它，SQLite 只能
        // 按 cred_id 定位，再把该账号 30 天的全部流水逐行走一遍靠 CASE 过滤——而窗口最长
        // 才一周，且这条 SQL 每次刷新账号列表都要按凭证跑一遍、全程持着那把全局 conn 锁。
        //
        // token 那一项各列逐个 COALESCE 成 0 再相加：没嗅探到用量的行（4xx/429）各列都是
        // NULL，而 SQLite 里 NULL + x = NULL，会把整条流水的 token 抹掉。
        let mut stmt = conn.prepare(
            "SELECT
                 SUM(CASE WHEN ts >= ?2 THEN 1 ELSE 0 END),
                 COALESCE(SUM(CASE WHEN ts >= ?2 THEN COALESCE(
                     total_tokens,
                     COALESCE(input_tokens, 0) + COALESCE(output_tokens, 0)) END), 0),
                 COALESCE(SUM(CASE WHEN ts >= ?2 THEN cost_usd END), 0),
                 SUM(CASE WHEN ts >= ?3 THEN 1 ELSE 0 END),
                 COALESCE(SUM(CASE WHEN ts >= ?3 THEN COALESCE(
                     total_tokens,
                     COALESCE(input_tokens, 0) + COALESCE(output_tokens, 0)) END), 0),
                 COALESCE(SUM(CASE WHEN ts >= ?3 THEN cost_usd END), 0)
               FROM usage_logs
              WHERE cred_id = ?1 AND ts >= ?4",
        )?;
        // `ts >= NULL` 恒为 NULL，于是没被报告的那个窗口在 SQL 里自然算出 0；
        // **要不要把它当成 0 由 Rust 这边定**——窗口不存在时返回 None，
        // 与「这个周期一条都没跑」区分开。
        let row = stmt.query_row(params![cred_id, primary, secondary, floor], |r| {
            Ok((
                WindowUsage { requests: r.get(0)?, tokens: r.get(1)?, cost_usd: r.get(2)? },
                WindowUsage { requests: r.get(3)?, tokens: r.get(4)?, cost_usd: r.get(5)? },
            ))
        })?;
        Ok((primary.map(|_| row.0), secondary.map(|_| row.1)))
    }

    /// 分页取用量流水（倒序），可按凭证过滤。
    ///
    /// `until` 是**翻页锚点**（Unix 秒，只取 `ts <= until` 的行）：不钉住它的话，翻页期间
    /// 新落的流水会把记录整体往后挤，用户在第 2 页看到的正是第 1 页刚看过的那几条。
    /// 首次请求传 `None`，响应里回一个锚点，之后每页原样带回。
    ///
    /// `total`/`total_cost` 与两项 token 合计按**同一套筛选条件**统计（含锚点），否则页码
    /// 算出来的页数与实际能翻到的页数对不上，合计也会与翻得到的那些行对不上。
    pub fn list_usage_page(
        &self,
        cred_id: Option<i64>,
        limit: i64,
        offset: i64,
        until: Option<i64>,
    ) -> Result<UsagePage> {
        let conn = self.conn.lock();
        let mut where_parts: Vec<String> = Vec::new();
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(id) = cred_id {
            where_parts.push(format!("cred_id = ?{}", args.len() + 1));
            args.push(Box::new(id));
        }
        if let Some(ts) = until {
            where_parts.push(format!("ts <= ?{}", args.len() + 1));
            args.push(Box::new(ts));
        }
        let where_sql = if where_parts.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", where_parts.join(" AND "))
        };

        let (total, total_cost, total_input_tokens, total_cached_tokens) = conn.query_row(
            &format!(
                "SELECT COUNT(*), COALESCE(SUM(cost_usd), 0),
                        COALESCE(SUM(input_tokens), 0), COALESCE(SUM(cached_tokens), 0)
                   FROM usage_logs {where_sql}"
            ),
            rusqlite::params_from_iter(args.iter().map(|a| a.as_ref())),
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, f64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            },
        )?;

        let sql = format!(
            "SELECT id, ts, cred_id, cred_label, session_id, model, path, ua, status,
                    input_tokens, cached_tokens, output_tokens, reasoning_tokens, total_tokens,
                    ttft_ms, total_ms, cost_usd
             FROM usage_logs {where_sql} ORDER BY id DESC LIMIT ?{} OFFSET ?{}",
            args.len() + 1,
            args.len() + 2
        );
        args.push(Box::new(limit));
        args.push(Box::new(offset));
        let mut stmt = conn.prepare(&sql)?;
        let map = |r: &Row| {
            Ok(UsageLog {
                id: r.get(0)?,
                ts: r.get(1)?,
                cred_id: r.get(2)?,
                cred_label: r.get(3)?,
                session_id: r.get(4)?,
                model: r.get(5)?,
                path: r.get(6)?,
                ua: r.get(7)?,
                status: r.get(8)?,
                input_tokens: r.get(9)?,
                cached_tokens: r.get(10)?,
                output_tokens: r.get(11)?,
                reasoning_tokens: r.get(12)?,
                total_tokens: r.get(13)?,
                ttft_ms: r.get(14)?,
                total_ms: r.get(15)?,
                cost_usd: r.get(16)?,
            })
        };
        let logs: Vec<UsageLog> = stmt
            .query_map(rusqlite::params_from_iter(args.iter().map(|a| a.as_ref())), map)?
            .collect::<rusqlite::Result<_>>()?;
        // 锚点取本轮最新那条的时间戳。这一页为空（越过末页）时沿用传入的锚点，
        // 免得回一个 None 让前端下一页又变成「不钉锚点」。
        let anchor = until.or_else(|| logs.first().map(|l| l.ts));
        Ok(UsagePage { logs, total, total_cost, total_input_tokens, total_cached_tokens, anchor })
    }

    /// 全池缓存命中率的**逐小时**流水合计，`since`（Unix 秒）之后的部分，按时间升序。
    ///
    /// 只回**有请求的那些小时**：没有请求的小时里「命中率」这件事根本不存在，回一个
    /// `0 / 0` 会被画成一根落到底的柱子，读起来像「那会儿缓存崩了」。画图那头据此留空。
    ///
    /// **桶固定是小时**，不按前端要的跨度分桶：小时的边界与时区无关（整小时偏移下哪个时区
    /// 都对得齐），而「天」的边界不是——服务端按 UTC 切出来的「一天」在 UTC+8 看是
    /// 08:00–08:00。所以这里只切小时，要看几天一根由浏览器按它自己的时区把小时合起来。
    /// 30 天也只有 720 个桶，而真实流量里非空的远少于此。
    ///
    /// 分母用 `input_tokens`（上游报的这个数本来就含命中那部分，见 [`CredentialStats`]），
    /// 不是 `total_tokens`：输出 token 与缓存无关，掺进分母只会把命中率按「这一轮模型说了
    /// 多少话」稀释。没嗅探到 usage 的行按 0 计入——`SUM` 本来就跳过 NULL。
    pub fn cache_series(&self, since: i64) -> Result<Vec<CacheBucket>> {
        const BUCKET_SECS: i64 = 3600;
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT (ts / ?2) * ?2 AS bucket,
                    COALESCE(SUM(input_tokens), 0), COALESCE(SUM(cached_tokens), 0)
               FROM usage_logs
              WHERE ts >= ?1
              GROUP BY bucket
              HAVING SUM(input_tokens) > 0
              ORDER BY bucket",
        )?;
        let rows = stmt.query_map(params![since, BUCKET_SECS], |r| {
            Ok(CacheBucket { ts: r.get(0)?, input_tokens: r.get(1)?, cached_tokens: r.get(2)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 裁掉过期的用量流水，返回删了几行。终身口径在账本里，不受影响。
    pub fn prune_usage_logs(&self) -> Result<usize> {
        let cutoff = now_secs() as i64 - USAGE_LOG_RETENTION_SECS;
        let conn = self.conn.lock();
        Ok(conn.execute("DELETE FROM usage_logs WHERE ts < ?1", params![cutoff])?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> CredentialStore {
        CredentialStore::open_in_memory().unwrap()
    }

    fn add(s: &CredentialStore, account: &str) -> Credential {
        s.upsert(
            account,
            None,
            None,
            account,
            None,
            "at",
            &format!("rt-{account}"),
            now_secs() + 3600,
        )
        .unwrap()
        .0
    }

    /// 同一账号重新登录必须更新原行，不能再插一条——否则同账号攒出一串各持已作废
    /// token 的僵尸行，用量历史也被劈成两半。
    #[test]
    fn relogin_updates_the_same_row() {
        let s = store();
        let (a, created) =
            s.upsert("l", None, None, "acct-1", None, "at1", "rt1", now_secs() + 3600).unwrap();
        assert!(created);
        let (b, created) = s
            .upsert("l", Some("e@x"), Some("pro"), "acct-1", None, "at2", "rt2", now_secs() + 7200)
            .unwrap();
        assert!(!created);
        assert_eq!(a.id, b.id);
        assert_eq!(s.list().unwrap().len(), 1);
        assert_eq!(b.access_token, "at2");
        assert_eq!(b.plan_type.as_deref(), Some("pro"));
    }

    /// 没有 account_id 的凭证一律拒收：存进去的话每条转发都 401，而报错指不到这里。
    #[test]
    fn credential_without_account_id_is_rejected() {
        let s = store();
        assert!(s.upsert("l", None, None, "  ", None, "at", "rt", 0).is_err());
    }

    /// 同档内取最久未使用的，这样账号是轮流用而不是把第一个榨干。
    #[test]
    fn selection_rotates_within_a_priority_tier() {
        let s = store();
        let a = add(&s, "a");
        let b = add(&s, "b");
        // 两个都没用过时按 id 定序（last_used_at 皆为 0）。
        assert_eq!(s.select(&[], None).unwrap().id, a.id);
        s.insert_usage_log(&UsageRecord { cred_id: Some(a.id), ..Default::default() }).unwrap();
        assert_eq!(s.select(&[], None).unwrap().id, b.id, "least-recently-used should win");
    }

    /// 带了会话键就固定落在同档的同一个号上——那是 prompt cache 能命中的前提。
    #[test]
    fn a_session_key_pins_to_one_credential() {
        let s = store();
        let ids: Vec<i64> = ["a", "b", "c", "d"].iter().map(|n| add(&s, n).id).collect();

        let pick = s.select(&[], Some("sess-1")).unwrap().id;
        assert!(ids.contains(&pick));
        // 反复选、以及中间有别的号被用过（LRU 会变），落点都不能动。
        for _ in 0..5 {
            assert_eq!(s.select(&[], Some("sess-1")).unwrap().id, pick);
        }
        for id in &ids {
            s.insert_usage_log(&UsageRecord { cred_id: Some(*id), ..Default::default() }).unwrap();
        }
        assert_eq!(s.select(&[], Some("sess-1")).unwrap().id, pick, "LRU 不该动粘性落点");

        // 不同会话要摊开到不同号上，否则粘性就成了「全钉在一个号上」。
        let spread: std::collections::HashSet<i64> =
            (0..40).map(|i| s.select(&[], Some(&format!("sess-{i}"))).unwrap().id).collect();
        assert!(spread.len() > 1, "40 个会话键全落在同一个号上: {spread:?}");
    }

    /// 粘性落点不可用时降级，且**只有落在它上面的键换主**——这就是不用哈希取模的理由：
    /// 每一次改落点都是一整段前缀的缓存未命中。
    #[test]
    fn losing_one_credential_only_remaps_the_keys_that_lived_on_it() {
        let s = store();
        for n in ["a", "b", "c", "d", "e"] {
            add(&s, n);
        }
        let keys: Vec<String> = (0..40).map(|i| format!("sess-{i}")).collect();
        let before: Vec<i64> = keys.iter().map(|k| s.select(&[], Some(k)).unwrap().id).collect();

        // 挑一个真的承载了键的号，把它打进冷却（额度暂停、被 exclude 是同一个效果）。
        let victim = before[0];
        s.note_rate_limited(victim, 60);
        let after: Vec<i64> = keys.iter().map(|k| s.select(&[], Some(k)).unwrap().id).collect();

        for (i, k) in keys.iter().enumerate() {
            if before[i] == victim {
                assert_ne!(after[i], victim, "{k} 的落点该让出去");
            } else {
                assert_eq!(after[i], before[i], "{k} 没落在出问题的号上，不该被打乱");
            }
        }
        // 让出去的键换到哪个号也必须是确定的，否则同一会话的并发请求会分到两个号。
        assert_eq!(
            keys.iter().map(|k| s.select(&[], Some(k)).unwrap().id).collect::<Vec<_>>(),
            after
        );
    }

    /// 粘性只在档内生效：跨档粘会让低优先级的号因为哈希落点抢在高优先级前面。
    #[test]
    fn stickiness_never_crosses_a_priority_tier() {
        let s = store();
        let top = add(&s, "top");
        let rest: Vec<i64> = ["b", "c", "d"].iter().map(|n| add(&s, n).id).collect();
        s.set_priority(top.id, -1).unwrap();

        for i in 0..30 {
            assert_eq!(
                s.select(&[], Some(&format!("sess-{i}"))).unwrap().id,
                top.id,
                "P-1 还能用时不该碰下面那一档"
            );
        }
        // 上面那档不可用了才落到下面，且落点仍然是固定的。
        s.note_rate_limited(top.id, 60);
        let pick = s.select(&[], Some("sess-7")).unwrap().id;
        assert!(rest.contains(&pick));
        assert_eq!(s.select(&[], Some("sess-7")).unwrap().id, pick);
    }

    /// 优先级压过轮换：P0 没打满之前不该碰 P1。
    #[test]
    fn priority_beats_rotation() {
        let s = store();
        let a = add(&s, "a");
        let b = add(&s, "b");
        s.set_priority(b.id, -1).unwrap();
        s.insert_usage_log(&UsageRecord { cred_id: Some(b.id), ..Default::default() }).unwrap();
        assert_eq!(s.select(&[], None).unwrap().id, b.id);
        assert_eq!(s.select(&[b.id], None).unwrap().id, a.id, "exclude should fall through to P0");
    }

    /// 冷却中的号跳过；全都在冷却时报 AllRateLimited 并带上最短等待秒数。
    #[test]
    fn cooling_credentials_are_skipped_then_reported() {
        let s = store();
        let a = add(&s, "a");
        let b = add(&s, "b");
        s.note_rate_limited(a.id, 30);
        assert_eq!(s.select(&[], None).unwrap().id, b.id);
        s.note_rate_limited(b.id, 10);
        let err = s.select(&[], None).unwrap_err();
        let rl = err.downcast_ref::<AllRateLimited>().expect("should be AllRateLimited");
        assert!(rl.retry_after_secs <= 10, "should report the soonest: {}", rl.retry_after_secs);
    }

    /// 手动启用要抹掉自动停用的痕迹，否则一个陈旧的判定会把号再关回去。
    #[test]
    fn manual_enable_clears_ban_and_resume() {
        let s = store();
        let a = add(&s, "a");
        s.mark_banned(a.id, "banned").unwrap();
        assert!(s.get(a.id).unwrap().unwrap().disabled);
        s.set_disabled(a.id, false).unwrap();
        let c = s.get(a.id).unwrap().unwrap();
        assert!(!c.disabled && c.ban_reason.is_none() && c.resume_at.is_none());
    }

    /// 限流暂停到点后由选号惰性恢复；人工停用的号不该被顺手打开。
    #[test]
    fn expired_pause_resumes_but_manual_disable_stays() {
        let s = store();
        let a = add(&s, "a");
        let b = add(&s, "b");
        s.set_disabled(b.id, true).unwrap();
        // 直接把 resume_at 写到过去：pause_for_rate_limit 会把秒数夹到 >= 1（一个 0 秒的
        // 暂停没有意义），所以「已经到点」这个状态只能这样造出来。
        s.pause_for_rate_limit(a.id, 60).unwrap();
        s.conn
            .lock()
            .execute("UPDATE credentials SET resume_at = 1 WHERE id = ?1", params![a.id])
            .unwrap();
        assert_eq!(s.select(&[], None).unwrap().id, a.id);
        assert!(s.get(b.id).unwrap().unwrap().disabled, "manual disable must survive");
    }

    /// 连通性测试通过后的恢复只认「限流暂停」这一档：人工停用与封号都得留着，否则一次
    /// 手动探活就能把人关掉的号打开、或让一个已封禁的号重回轮转。
    #[test]
    fn probe_resume_only_lifts_a_rate_limit_pause() {
        let s = store();
        let paused = add(&s, "paused");
        let manual = add(&s, "manual");
        let banned = add(&s, "banned");
        s.pause_for_rate_limit(paused.id, 3600).unwrap();
        s.set_disabled(manual.id, true).unwrap();
        s.mark_banned(banned.id, "suspended account").unwrap();

        assert!(s.resume_if_rate_limited(paused.id).unwrap(), "a rate-limit pause must lift");
        let back = s.get(paused.id).unwrap().unwrap();
        assert!(!back.disabled);
        assert!(back.resume_at.is_none());

        for id in [manual.id, banned.id] {
            assert!(!s.resume_if_rate_limited(id).unwrap(), "nothing to lift for #{id}");
            assert!(s.get(id).unwrap().unwrap().disabled, "#{id} must stay disabled");
        }
    }

    #[test]
    fn rpm_limit_blocks_after_quota_is_spent() {
        let s = store();
        let a = add(&s, "a");
        s.set_rpm_limit(a.id, 2).unwrap();
        let c = s.get(a.id).unwrap().unwrap();
        assert!(s.take_rpm_slot(&c).is_ok());
        assert!(s.take_rpm_slot(&c).is_ok());
        let err = s.take_rpm_slot(&c).unwrap_err();
        assert!(err.downcast_ref::<RpmLimited>().is_some(), "{err:#}");
    }

    /// `< 0` 必须能顶掉全局默认，否则「全局限 60、唯独这个号不限」表达不出来。
    #[test]
    fn per_credential_unlimited_overrides_global_default() {
        assert_eq!(effective_rpm_limit(0, 60), 60);
        assert_eq!(effective_rpm_limit(10, 60), 10);
        assert_eq!(effective_rpm_limit(-1, 60), 0);
    }

    /// 流水与账本同事务更新；空快照不覆盖已有快照。
    /// token 账本逐条累加，且 **cached 单独记一列而不混进 input**——它是 input 的子集，
    /// 界面求和时要能自己决定加不加（加了就把命中缓存的会话凭空放大一倍）。
    /// 窗口统计只数**当前周期内**的流水，起点由「重置时刻 − 窗口长度」反推。
    ///
    /// 这是这块最容易错的地方：窗口起点随每次快照移动，多算一条早于起点的流水，界面上
    /// 就会显示一个比上游那个百分比明显对不上的用量。
    #[test]
    fn window_usage_counts_only_the_current_period() {
        let s = store();
        let a = add(&s, "a");
        let b = add(&s, "b");
        let now = now_secs() as i64;
        // primary：10080 分钟（7 天）窗口，1 小时后重置 → 起点在 7 天前的 1 小时后。
        let reset = now + 3600;
        let quota = QuotaSnapshot {
            primary_used_pct: Some(19.0),
            primary_window_minutes: Some(10_080),
            primary_reset_at: Some(reset.to_string()),
            // 实测 Pro 账号的 secondary 就是这副样子：0 分钟 + 空重置时刻。
            secondary_used_pct: Some(0.0),
            secondary_window_minutes: Some(0),
            secondary_reset_at: Some(String::new()),
            ..Default::default()
        };

        let log =
            |cred_id: i64, ts_offset: i64, tokens: i64, cost: f64, q: Option<QuotaSnapshot>| {
                s.insert_usage_log(&UsageRecord {
                    cred_id: Some(cred_id),
                    has_usage: true,
                    input_tokens: Some(tokens),
                    output_tokens: Some(0),
                    total_tokens: Some(tokens),
                    cost_usd: Some(cost),
                    quota: q,
                    ..Default::default()
                })
                .unwrap();
                if ts_offset != 0 {
                    // 落库用的是 unixepoch() 默认值，只能事后改 ts 来造「窗口外」那几条。
                    let conn = s.conn.lock();
                    conn.execute(
                        "UPDATE usage_logs SET ts = ?1 WHERE id = (SELECT MAX(id) FROM usage_logs)",
                        params![now + ts_offset],
                    )
                    .unwrap();
                }
            };

        log(a.id, 0, 100, 1.0, Some(quota.clone())); // 窗口内
        log(a.id, -3600, 50, 0.5, None); // 窗口内（1 小时前）
        log(a.id, -8 * 24 * 3600, 900, 9.0, None); // 8 天前 → 窗口外
        log(b.id, 0, 7, 0.07, Some(quota)); // 另一个账号，不得串味

        let st = s.stats_of(a.id).unwrap();
        let w = st.primary_window.expect("primary window is reported");
        assert_eq!(w.requests, 2, "8 天前那条在窗口外");
        assert_eq!(w.tokens, 150);
        assert!((w.cost_usd - 1.5).abs() < 1e-9, "{}", w.cost_usd);
        // 终身账本照旧含全部三条。
        assert_eq!(st.request_total, 3);

        assert!(
            st.secondary_window.is_none(),
            "0 分钟 + 空重置时刻的窗口没被上游报告，必须是 None 而不是一组 0"
        );
        assert_eq!(s.stats_of(b.id).unwrap().primary_window.unwrap().requests, 1, "不得跨账号串");
    }

    /// 压根没有快照时两个窗口都是 `None`——不能拿终身累计冒充「本周期」。
    #[test]
    fn window_usage_is_absent_without_a_snapshot() {
        let s = store();
        let a = add(&s, "a");
        s.insert_usage_log(&UsageRecord {
            cred_id: Some(a.id),
            has_usage: true,
            total_tokens: Some(10),
            ..Default::default()
        })
        .unwrap();
        let st = s.stats_of(a.id).unwrap();
        assert!(st.primary_window.is_none() && st.secondary_window.is_none());
        assert_eq!(st.request_total, 1, "终身账本仍照常记");
    }

    #[test]
    fn token_totals_accumulate_per_request() {
        let s = store();
        let a = add(&s, "a");
        for (input, cached, output) in [(100, 40, 20), (300, 250, 5)] {
            s.insert_usage_log(&UsageRecord {
                cred_id: Some(a.id),
                has_usage: true,
                input_tokens: Some(input),
                cached_tokens: Some(cached),
                output_tokens: Some(output),
                ..Default::default()
            })
            .unwrap();
        }
        // 解析不到用量的那种（4xx，各 token 为 None）不该把累计搅乱。
        s.insert_usage_log(&UsageRecord { cred_id: Some(a.id), status: 400, ..Default::default() })
            .unwrap();

        let st = s.stats_of(a.id).unwrap();
        assert_eq!(st.request_total, 3, "失败请求照样算一条");
        assert_eq!(st.input_tokens_total, 400);
        assert_eq!(st.cached_tokens_total, 290);
        assert_eq!(st.output_tokens_total, 25);

        // 流水那头的合计（缓存命中率的两个原始数）与账本对得上：分母是 input（已含 cached），
        // 没报用量的那条按 0 计入，两个数都不能因为它变成 NULL。
        let page = s.list_usage_page(Some(a.id), 10, 0, None).unwrap();
        assert_eq!((page.total_input_tokens, page.total_cached_tokens), (400, 290));
    }

    /// 额度快照**只被它真的报了的那几项覆盖**。
    ///
    /// 三种响应轮着来：报全的、只报 credits 那一组的、一项都没报的。中间那种如果整份替换，
    /// 主额度读数会被抹成空——卡片上那条进度条突然空了一格，而账号额度一点没变；最后那种
    /// （翻译层就拒掉的请求、CDN 拦截页）压根不该动快照。
    #[test]
    fn a_partial_quota_response_does_not_wipe_what_it_did_not_report() {
        let s = store();
        let a = add(&s, "a");
        let full = QuotaSnapshot {
            primary_used_pct: Some(20.0),
            primary_window_minutes: Some(10080),
            primary_reset_at: Some("1787801499".into()),
            secondary_used_pct: Some(3.0),
            credits_balance: Some(0.0),
            ..Default::default()
        };
        let log = |q: Option<QuotaSnapshot>| {
            s.insert_usage_log(&UsageRecord { cred_id: Some(a.id), quota: q, ..Default::default() })
                .unwrap()
        };

        log(Some(full.clone()));
        assert_eq!(s.stats_of(a.id).unwrap().quota.unwrap().primary_used_pct, Some(20.0));

        // 只带 credits 那一组：它对两个窗口什么都没说，那两项就该维持原值。
        log(Some(QuotaSnapshot { credits_balance: Some(5.0), ..Default::default() }));
        let q = s.stats_of(a.id).unwrap().quota.unwrap();
        assert_eq!(q.credits_balance, Some(5.0), "报了的那项要更新");
        assert_eq!(q.primary_used_pct, Some(20.0), "没报的那项不能被抹成空");
        assert_eq!(q.primary_window_minutes, Some(10080));
        assert_eq!(q.primary_reset_at.as_deref(), Some("1787801499"));
        assert_eq!(q.secondary_used_pct, Some(3.0));

        // 一项都没解出来：整份快照都不动。
        log(None);
        log(Some(QuotaSnapshot::default()));
        let q = s.stats_of(a.id).unwrap().quota.unwrap();
        assert_eq!(q.primary_used_pct, Some(20.0));
        assert_eq!(q.credits_balance, Some(5.0));

        // 报了的项即使变小也照样覆盖（窗口重置后百分比会掉回去）。
        log(Some(QuotaSnapshot { primary_used_pct: Some(1.0), ..Default::default() }));
        assert_eq!(s.stats_of(a.id).unwrap().quota.unwrap().primary_used_pct, Some(1.0));
    }

    /// 旧库（`credential_stats` 没有 token 列）要能补列并从残留流水里回填一次。
    ///
    /// 直接把三列 DROP 掉来模拟旧库——比另建一套旧 schema 更贴近真实升级路径。
    #[test]
    fn migration_backfills_token_totals_from_retained_logs() {
        let s = store();
        let a = add(&s, "a");
        s.insert_usage_log(&UsageRecord {
            cred_id: Some(a.id),
            has_usage: true,
            input_tokens: Some(700),
            cached_tokens: Some(120),
            output_tokens: Some(90),
            ..Default::default()
        })
        .unwrap();

        {
            let conn = s.conn.lock();
            conn.execute_batch(
                "ALTER TABLE credential_stats DROP COLUMN input_tokens_total;
                 ALTER TABLE credential_stats DROP COLUMN cached_tokens_total;
                 ALTER TABLE credential_stats DROP COLUMN output_tokens_total;",
            )
            .unwrap();
            migrate_token_totals(&conn).unwrap();
            // 幂等：再跑一次不能把回填累加第二遍。
            migrate_token_totals(&conn).unwrap();
        }

        let st = s.stats_of(a.id).unwrap();
        assert_eq!(
            (st.input_tokens_total, st.cached_tokens_total, st.output_tokens_total),
            (700, 120, 90)
        );
    }

    #[test]
    fn usage_updates_ledger_and_keeps_last_quota_snapshot() {
        let s = store();
        let a = add(&s, "a");
        let quota = QuotaSnapshot { primary_used_pct: Some(42.0), ..Default::default() };
        s.insert_usage_log(&UsageRecord {
            cred_id: Some(a.id),
            cost_usd: Some(0.5),
            quota: Some(quota),
            ..Default::default()
        })
        .unwrap();
        s.insert_usage_log(&UsageRecord {
            cred_id: Some(a.id),
            cost_usd: Some(0.25),
            quota: Some(QuotaSnapshot::default()), // 这次没解出限流头
            ..Default::default()
        })
        .unwrap();
        let st = s.stats_of(a.id).unwrap();
        assert_eq!(st.request_total, 2);
        assert!((st.cost_total_usd - 0.75).abs() < 1e-9);
        assert_eq!(
            st.quota.and_then(|q| q.primary_used_pct),
            Some(42.0),
            "empty snapshot must not clobber"
        );
        let page = s.list_usage_page(Some(a.id), 10, 0, None).unwrap();
        assert_eq!(page.logs.len(), 2);
        assert_eq!(page.total, 2);
        assert!((page.total_cost - 0.75).abs() < 1e-9);
        assert!(page.anchor.is_some());
        // 两条流水都没报 token：合计是 0 而不是 NULL——界面据此显示「没有可谈的缓存率」。
        assert_eq!((page.total_input_tokens, page.total_cached_tokens), (0, 0));
    }

    /// 缓存命中率趋势：**按小时分桶、跨账号求和、没请求的小时不出现**。
    ///
    /// 空桶不能回一个 `0 / 0`：画图那头会把它画成一根落到底的柱子，读起来像「那会儿缓存
    /// 崩了」，而真相是那个小时一条请求都没有。
    #[test]
    fn cache_series_buckets_by_hour_and_skips_quiet_ones() {
        let s = store();
        let a = add(&s, "a");
        let b = add(&s, "b");
        let now = now_secs() as i64;
        let log = |cred_id: i64, ts: i64, input: Option<i64>, cached: Option<i64>| {
            s.insert_usage_log(&UsageRecord {
                cred_id: Some(cred_id),
                has_usage: input.is_some(),
                input_tokens: input,
                cached_tokens: cached,
                ..Default::default()
            })
            .unwrap();
            // 落库用的是 unixepoch() 默认值，只能事后改 ts 来把流水摆到指定的小时里。
            let conn = s.conn.lock();
            conn.execute(
                "UPDATE usage_logs SET ts = ?1 WHERE id = (SELECT MAX(id) FROM usage_logs)",
                params![ts],
            )
            .unwrap();
        };

        // 同一个小时里的两个账号要合成一个桶：这条曲线讲的是「全池」。
        let h0 = now / 3600 * 3600;
        log(a.id, h0 + 10, Some(1_000), Some(900));
        log(b.id, h0 + 20, Some(1_000), Some(100));
        // 上一个小时只有一条。中间那个小时刻意留空。
        log(a.id, h0 - 2 * 3600 + 30, Some(400), Some(0));
        // 没嗅探到用量的那条（多半是 4xx）：它自己的小时里没有可谈的命中率，不该出现。
        log(a.id, h0 - 5 * 3600, None, None);
        // 早于 since 的不算。
        log(a.id, h0 - 50 * 3600, Some(9_999), Some(9_999));

        let series = s.cache_series(h0 - 10 * 3600).unwrap();
        assert_eq!(series.len(), 2, "三个有请求的小时里，只有两个有可谈的命中率");
        // 升序：画图直接按顺序铺，不必自己再排。
        assert_eq!(series[0].ts, h0 - 2 * 3600);
        assert_eq!((series[0].input_tokens, series[0].cached_tokens), (400, 0));
        // 桶起点对齐到整小时，而不是那条流水自己的时间戳。
        assert_eq!(series[1].ts, h0);
        assert_eq!((series[1].input_tokens, series[1].cached_tokens), (2_000, 1_000));

        // 跨度夹得更短就只剩最近那个桶；一条都没有时回空数组而不是报错。
        assert_eq!(s.cache_series(h0).unwrap().len(), 1);
        assert!(s.cache_series(now + 3600).unwrap().is_empty());
    }

    /// 删号要连带清掉账本与流水，否则 id 复用时新账号继承一段历史。
    #[test]
    fn delete_removes_ledger_and_logs() {
        let s = store();
        let a = add(&s, "a");
        s.insert_usage_log(&UsageRecord { cred_id: Some(a.id), ..Default::default() }).unwrap();
        assert!(s.delete(a.id).unwrap());
        assert_eq!(s.stats_of(a.id).unwrap().request_total, 0);
        assert!(s.list_usage_page(Some(a.id), 10, 0, None).unwrap().logs.is_empty());
    }

    #[test]
    fn settings_survive_and_fall_back_on_garbage() {
        let s = store();
        assert_eq!(s.get_setting_i64(DEFAULT_RPM_LIMIT, 7), 7);
        s.set_setting(DEFAULT_RPM_LIMIT, "not-a-number").unwrap();
        assert_eq!(s.get_setting_i64(DEFAULT_RPM_LIMIT, 7), 7, "garbage must fall back");
        s.set_setting(DEFAULT_RPM_LIMIT, "30").unwrap();
        assert_eq!(s.get_setting_i64(DEFAULT_RPM_LIMIT, 7), 30);
        s.delete_setting(DEFAULT_RPM_LIMIT).unwrap();
        assert_eq!(s.get_setting_i64(DEFAULT_RPM_LIMIT, 7), 7);
    }
}
