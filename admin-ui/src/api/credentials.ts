import { api } from './client'

/**
 * 上游报告的一个额度窗口。
 *
 * codex 只给两个窗口：`primary`（通常 5 小时）与 `secondary`（通常一周/一月）。窗口长度
 * 是上游按分钟报的，不写死——同一个账号在不同套餐下窗口长度不同，写死只会显示错。
 */
export interface QuotaWindow {
  used_pct: number | null
  window_minutes: number | null
  reset_at: string | null
}

/**
 * 订阅账号最新额度快照（来自上游 `x-codex-*` 头）。
 *
 * 快照只在**真的解出了限流头**的那次请求后更新，故它可能明显早于最近一次请求——
 * 界面要按 `snapshot_ts` 标出「这是什么时候的读数」，否则一个过期的 12% 会被当成现状。
 */
export interface Quota {
  primary_used_pct: number | null
  primary_window_minutes: number | null
  primary_reset_at: string | null
  secondary_used_pct: number | null
  secondary_window_minutes: number | null
  secondary_reset_at: string | null
  /** 额外 credits：额度满了也能继续跑，烧的是按量计费的钱。 */
  credits_has_credits: boolean | null
  credits_unlimited: boolean | null
  credits_balance: number | null
}

/**
 * 一个额度窗口**当前周期内**已经发生了什么。
 *
 * 与终身账本（`*_total`）互补：账号跑了多久是一回事，「这一个窗口里压了多少」才是决定它
 * 此刻还能不能接活的那个数。三项一起看，因为它们**互相不成正比**——命中缓存的输入按十
 * 分之一计价，重度吃缓存的号会呈现「token 一大堆、花费很少」。
 */
export interface WindowUsage {
  /** 窗口内经这个账号转发的请求数（含失败的）。 */
  requests: number
  /** 窗口内的 token 总数（输入 + 输出，两者已分别含缓存与 reasoning，不重复计）。 */
  tokens: number
  /** 窗口内的等价 API 费用。价目表认不出的模型记 0，所以这是**下限**。 */
  cost_usd: number
}

/**
 * 额度重置券的一次读数。
 *
 * 上游给订阅账号发的一次性券：额度撞墙之后兑一张，5h/周窗口当场归零，不用等重置时刻。
 * 与 {@link Quota} 是两件事——那份说「用了多少」，随每条转发顺带更新；这份说「还能重置
 * 几次」，只有点了查询/重置才去问上游，故自带 `fetched_at`。
 */
export interface ResetCredits {
  /** 还能重置几次。 */
  available_count: number
  /**
   * 每张券的过期时刻（上游给的原串，多为 RFC3339）。
   *
   * 后端不解析、原样转出，交给浏览器的 `Date` 认；也因此**不按过期时刻自动作废**，
   * 界面显示的是「`fetched_at` 那一刻的读数」。可能比 `available_count` 短——上游只在券
   * 清单那条接口上给过期时刻，退回 `/wham/usage` 时只有总数。
   */
  expires_at: string[]
  /** 这份读数是什么时候取的（Unix 秒）。 */
  fetched_at: number
}

/** 每个账号的终身账本。流水会被裁剪，这些数不会。 */
export interface CredentialStats {
  last_used_at: number | null
  cost_total_usd: number
  request_total: number
  /** 输入 token 累计。**已含命中缓存的那部分**（上游报的 `input_tokens` 就是这个口径）。 */
  input_tokens_total: number
  /** 其中命中缓存的部分——是 `input_tokens_total` 的**子集**，不是另一笔，求和时别加两次。 */
  cached_tokens_total: number
  output_tokens_total: number
  /** 上面那份 quota 是什么时候的读数（Unix 秒）。 */
  snapshot_ts: number | null
  quota: Quota | null
  /**
   * 额度重置券的最新读数。
   *
   * `null` = **这个号还没查过**，不是「没有券」——两者界面上必须分开：前者点一下查询就
   * 知道，后者是已知事实。
   */
  reset_credits: ResetCredits | null
  /**
   * 主/次额度窗口当前周期内的用量。
   *
   * 上游没报这个窗口（没有重置时刻、或窗口长度为 0）时是 `null`——与「这个周期里一条都
   * 没跑」（各项为 0）是两件事，界面必须分开显示：前者再等也不会出现，后者等等就有。
   */
  primary_window: WindowUsage | null
  secondary_window: WindowUsage | null
}

/** 对外的凭证视图（后端已脱敏，无明文 token）。 */
export interface Credential {
  id: number
  label: string
  email: string | null
  /** ChatGPT 套餐档位：`plus` / `pro` / `team` / `enterprise` / `free`。 */
  plan_type: string | null
  /** 账号 id 的掩码尾段，够区分两个账号又不至于把完整标识散出去。 */
  account_id_masked: string
  priority: number
  disabled: boolean
  /** 三态：>0 本账号独立上限；0 跟随全局默认；<0 本账号明确不限。 */
  rpm_limit: number
  /**
   * 三态折算后**真正生效**的上限（0 = 不限）。
   *
   * 由后端算好：前端自己算就得同时知道那条三态规则与全局默认值，两处各写一份迟早对不上，
   * 而对不上的表现是界面显示的上限与实际拦截的上限不是一个数。
   */
  rpm_limit_effective: number
  /** 最近 60 秒该账号已转发多少条（进程内计数，重启清零）。 */
  rpm: number
  /** 非空表示被自动停用（封号 / refresh token 失效），需要人工处理。 */
  ban_reason: string | null
  /** 非空表示因限流暂停，到这个时刻自动恢复（Unix 秒）。 */
  resume_at: number | null
  proxy: string | null
  expires_in_secs: number
  /** 还要冷却几秒（0 = 不在冷却中）。 */
  cooldown_secs: number
  created_at: number
  updated_at: number
  stats: CredentialStats
}

/** 一条用量流水。 */
export interface UsageLog {
  id: number
  ts: number
  cred_id: number | null
  cred_label: string
  /**
   * 这条请求实际用的会话键：客户端自报的会话头优先，没有就是请求前缀的指纹。
   *
   * 实测三个真实 codex 客户端一个都不发会话头，所以这一列绝大多数是指纹——它同时也是
   * 「同一段对话」的判据，落点与上游 session 都跟着它走。
   */
  session_id: string | null
  /** 这条请求的缓存结局 / 未命中原因；null = 没有会话身份（`models` 那类）。见 `CACHE_REASONS`。 */
  cache_reason: string | null
  model: string | null
  path: string
  /** 来访客户端自报的 UA（已截断）。 */
  ua: string | null
  status: number
  input_tokens: number | null
  cached_tokens: number | null
  output_tokens: number | null
  reasoning_tokens: number | null
  total_tokens: number | null
  ttft_ms: number | null
  total_ms: number | null
  cost_usd: number | null
}

/** 发起一次登录，拿到授权 URL 与本次尝试的关联 id。 */
export async function getAuthorizeUrl(): Promise<{ url: string; state: string }> {
  const { data } = await api.get('/authorize')
  return data
}

/**
 * 用回调 URL 换 token。
 *
 * `callback` 收整条 `http://localhost:1455/auth/callback?code=…` 或只有 code 的一段，
 * 两种都行（后端 `parse_callback` 负责认）。
 */
export async function exchangeCode(callback: string, state?: string): Promise<Credential> {
  const { data } = await api.post('/exchange', { callback, state })
  return data
}

/** 批量导入里被跳过的一个账号。 */
export interface SkippedAccount {
  name: string
  reason: string
}

/**
 * 一次导入的结果。单个账号也是这个形状（`imported` 长度 1），所以渲染只有一条路径。
 */
export interface ImportReport {
  imported: Credential[]
  skipped: SkippedAccount[]
}

/**
 * 导入已登录的账号。
 *
 * 认 `~/.codex/auth.json`、裸 token 对象、以及带 `accounts` 数组的批量导出（sub2api 等）
 * 三种形态——由后端 `import_one` 归一，前端不必先判是哪一种。
 */
export async function importAuthJson(content: string): Promise<ImportReport> {
  const { data } = await api.post('/import-auth-json', { content })
  return data
}

export async function listCredentials(): Promise<Credential[]> {
  const { data } = await api.get('/credentials')
  return data
}

/** 一页流水 + 这一轮筛选的合计。 */
export interface UsagePage {
  logs: UsageLog[]
  /** 同一套筛选条件下的总条数（供前端算页数）。 */
  total: number
  /** 同一套筛选条件下的总花费（USD）。 */
  total_cost: number
  /**
   * 同一套筛选条件下的输入 token 合计（**已含命中缓存那部分**）与其中命中缓存的部分。
   *
   * 用 `cacheHitRate` 算这一段流水的缓存命中率。没嗅探到 usage 的那些行按 0 计入，
   * 于是两个数都是 0 时表示「这段流水里没有可谈的缓存率」。
   */
  total_input_tokens: number
  total_cached_tokens: number
  /**
   * 本轮翻页的锚点（Unix 秒）。
   *
   * 首次请求不传，之后每页原样带回：不钉住它的话，翻页期间新落的流水会把记录整体
   * 往后挤，第 2 页看到的正是第 1 页刚看过的那几条。
   */
  anchor: number | null
}

export interface UsageQuery {
  limit?: number
  offset?: number
  until?: number
}

export async function listCredentialUsage(id: number, query: UsageQuery = {}): Promise<UsagePage> {
  const { data } = await api.get(`/credentials/${id}/usage`, { params: query })
  return data
}

export async function listUsage(query: UsageQuery = {}): Promise<UsagePage> {
  const { data } = await api.get('/usage', { params: query })
  return data
}

export async function deleteCredential(id: number): Promise<void> {
  await api.delete(`/credentials/${id}`)
}

export async function setDisabled(id: number, disabled: boolean): Promise<Credential> {
  const { data } = await api.post(`/credentials/${id}/disabled`, { value: disabled })
  return data
}

export async function setPriority(id: number, priority: number): Promise<Credential> {
  const { data } = await api.post(`/credentials/${id}/priority`, { value: priority })
  return data
}

export async function setLabel(id: number, label: string): Promise<Credential> {
  const { data } = await api.post(`/credentials/${id}/label`, { value: label })
  return data
}

export async function setRpmLimit(id: number, rpmLimit: number): Promise<Credential> {
  const { data } = await api.post(`/credentials/${id}/rpm-limit`, { value: rpmLimit })
  return data
}

/** 空串 = 清除代理（直连）。 */
export async function setProxy(id: number, proxy: string | null): Promise<Credential> {
  const { data } = await api.post(`/credentials/${id}/proxy`, { value: proxy ?? '' })
  return data
}

/**
 * 上游模型清单里的一项。
 *
 * 与 codex CLI 缓存到 `~/.codex/models_cache.json` 的是同一份数据（同一个上游端点）。
 */
export interface UpstreamModel {
  /** 传给上游的模型名，`model` 字段填的就是它。 */
  slug: string
  /** 给人看的名字（`GPT-5.6-Sol`）。 */
  display_name: string | null
  description: string | null
  /**
   * `list` = codex 自己的模型选择器里会列出来；`hide` = 内部项或别名。
   *
   * `hide` 的**照样能用**，只是会被上游解析成别的模型（实测 `gpt-reserve` 与
   * `codex-auto-review` 都变成 `gpt-5.6-luna`），所以默认不进下拉——它们不是独立的模型，
   * 列出来只会让人以为多了两个可选项。
   */
  visibility: string | null
  /**
   * 上游标的「能不能走 API」。
   *
   * **不能当过滤条件**：实测 `gpt-5.3-codex-spark` 标着 `false`，走 `/responses` 却照样
   * 200。含义不明，只作参考。
   */
  supported_in_api: boolean | null
  /** 上游给的排序权重，小者靠前（后端已按它排好）。 */
  priority: number | null
}

/**
 * 取这个账号当前可用的模型清单。
 *
 * 后端向上游现取（不消耗额度），所以它随上游上新/下线自动跟上。取不到会抛——调用方应退回
 * 内置兜底清单，**下拉框不能因此变空**。
 */
export async function listCredentialModels(id: number): Promise<UpstreamModel[]> {
  const { data } = await api.get<UpstreamModel[]>(`/credentials/${id}/models`, { timeout: 20_000 })
  return data
}

/** 一次连通性测试的结果。 */
export interface ProbeResult {
  /** 上游是否 2xx **且**流里没有失败事件。 */
  ok: boolean
  /** 上游 HTTP 状态码；**0 表示请求根本没到上游**（取 token 失败/连不上/超时），原因见 error。 */
  status: number
  /** 从发出到读完响应的耗时（毫秒）。 */
  latency_ms: number
  /** 上游实际回报的模型名；别名会在上游解析成具体版本，故可能与请求的不同。 */
  model: string | null
  /** 上游错误类型（`error.type`）。 */
  error_type: string | null
  /** 失败原因原文。 */
  error: string | null
  /** 本次响应带回的额度快照（与卡片上那份同一套读法）；响应没带这组头时为 null。 */
  quota: Quota | null
  /** 上游 `retry-after`（秒）。只有 429 才有，是**这次拒绝**给出的等待时间。 */
  retry_after_secs: number | null
}

/**
 * 连通性测试：用**这一个**账号向上游发一条最小请求，看它能不能用该模型。
 *
 * 不走负载均衡选号、不占 RPM 名额、不换号重试，但账号状态照真实流量的口径更新（429 打
 * 冷却、命中封号特征自动停用、通过则解除限流暂停），并写一条用量流水（卡片上的额度与累计
 * 花费据此更新）——因为它真的打到了上游，也真的消耗一点点订阅额度。
 *
 * **上游拒绝同样是 200 + 一份结果**（状态码在 `status` 里），不是 HTTP 错误；只有「凭证不
 * 存在」「模型名没填」才会抛。
 */
export async function probeCredential(
  id: number,
  model: string,
  signal?: AbortSignal,
): Promise<ProbeResult> {
  const { data } = await api.post<ProbeResult>(
    `/credentials/${id}/test`,
    { model },
    {
      signal,
      // 后端总探测上限是 30 秒；再留 5 秒给本机与代理传输，避免断链让按钮永久 pending。
      timeout: 35_000,
    },
  )
  return data
}

/** 立刻刷新 token。只验证 refresh_token 与出站链路，不消耗额度、不碰模型。 */
export async function refreshCredential(id: number): Promise<Credential> {
  const { data } = await api.post(`/credentials/${id}/refresh`)
  return data
}

export async function clearCooldown(id: number): Promise<Credential> {
  const { data } = await api.delete(`/credentials/${id}/cooldown`)
  return data
}

/**
 * 查这个号还剩几张额度重置券。
 *
 * 后端向上游现问一趟并把读数落库，所以下次进页面卡片上就有数字了。不消耗券、不消耗额度、
 * 不写用量流水。问不到会抛——张数只能问上游，编一个 0 会被当成「这个号没券」。
 */
export async function getResetCredits(id: number): Promise<ResetCredits> {
  const { data } = await api.get<ResetCredits>(`/credentials/${id}/reset-credits`, {
    // 后端单条上限 20 秒，两条串起来最多 40 秒；再留 5 秒给本机与代理传输。
    timeout: 45_000,
  })
  return data
}

/** 一次兑换的结果。 */
export interface ResetResult {
  /** 上游给这次兑换的结果码（实测 `success`）。 */
  code: string | null
  /** 上游报的「这次重置了几个窗口」。 */
  windows_reset: number | null
  /** 被兑掉那张券的过期时刻。 */
  credit_expires_at: string | null
  /**
   * 兑换之后重新查的张数。
   *
   * `null` = 复查没成功，**不代表兑换失败**（券已经花掉了）。此时界面该说「已重置，张数待
   * 刷新」，而不是把兑换前那个旧数字继续摆着。
   */
  credits: ResetCredits | null
  /** 有没有把这个号从限流暂停里放回轮转（false = 它本来就没被暂停）。 */
  resumed: boolean
  /**
   * 有没有把额度读数归零（false = 这个号还没有任何快照，或上游明说一个窗口都没重置）。
   *
   * 与 `resumed` 是两件事：一个号可以「没被暂停过」但读数是 100%，也可以反过来。
   */
  quota_cleared: boolean
  /** 兑换后的凭证视图：兑换会顺手解除限流暂停与冷却，列表据此立刻摘掉那枚徽章。 */
  credential: Credential
}

/**
 * 兑一张重置券，把这个号的额度窗口重置掉。
 *
 * **不可撤销**：券花掉就没有，上游不退，所以调用前要二次确认。成功后后端顺手解除限流
 * 暂停与冷却（额度重置了却还在暂停里，等于白花一张）、并把额度读数归零（窗口当场就归零了，
 * 读数还停在 100% 会让人以为券白花了），人工停用与封号不碰。
 */
export async function consumeResetCredit(id: number): Promise<ResetResult> {
  const { data } = await api.post<ResetResult>(`/credentials/${id}/reset-credits/consume`, undefined, {
    // 兑换 + 复查两趟上游，给的余量同 getResetCredits。
    timeout: 45_000,
  })
  return data
}

// ---------- 批量 ----------
// 批量接口一律返回**全部**账号的最新视图：批量改优先级会连带改变列表顺序与分页，
// 只回改动的那几条前端没法自洽地合并。

export async function setPriorities(ids: number[], priority: number): Promise<Credential[]> {
  const { data } = await api.post('/credentials/priority', { ids, value: priority })
  return data
}

export async function setRpmLimits(ids: number[], rpmLimit: number): Promise<Credential[]> {
  const { data } = await api.post('/credentials/rpm-limit', { ids, value: rpmLimit })
  return data
}

export async function setDisabledMany(ids: number[], disabled: boolean): Promise<Credential[]> {
  const { data } = await api.post('/credentials/disabled', { ids, value: disabled })
  return data
}

export async function deleteCredentials(ids: number[]): Promise<Credential[]> {
  const { data } = await api.post('/credentials/delete', { ids })
  return data
}
