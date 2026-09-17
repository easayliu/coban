import { api } from './client'

export interface Metrics {
  credentials_total: number
  credentials_enabled: number
  /** 全池最近一个窗口内转发的请求总数（各账号之和）。 */
  rpm: number
  /** 上面那个数的窗口长度（秒）。由后端回，别在前端写死 60。 */
  window_secs: number
  /**
   * 在途请求数：已进入转发入口、响应尚未走完的那些。
   *
   * 流式回复要几十秒才走完，所以这个数在正常使用下就该是非零的——它反映的是并发，
   * 不是「积压」。
   */
  in_flight: number
  cost_total_usd: number
  requests_total: number
  /**
   * 全池终身累计的输入 token（**已含命中缓存那部分**）与其中命中缓存的部分。
   *
   * 后端只回这两个原始数、不回算好的比率：命中率作不作数取决于这两个数本身的量级
   * （300 token 上的「命中 0%」与 17K 前缀上的「命中 94%」是两件事）。用
   * `cacheHitRate(input_tokens_total, cached_tokens_total)` 算。
   */
  input_tokens_total: number
  cached_tokens_total: number
}

export async function getMetrics(): Promise<Metrics> {
  const { data } = await api.get('/metrics')
  return data
}

/** 缓存命中率趋势里的一个小时桶。`ts` 是这一小时的起点（Unix 秒）。 */
export interface CacheSeriesPoint {
  ts: number
  /** 这一小时的输入 token 合计（**已含命中缓存那部分**）。 */
  input_tokens: number
  cached_tokens: number
}

export interface CacheSeries {
  /** 这条曲线的真实起点（Unix 秒）。后端会按流水保留期夹住跨度，所以别用请求的 hours 反推。 */
  since: number
  /** 桶宽（秒），当前固定 3600。由后端回，别在前端写死。 */
  bucket_secs: number
  /**
   * **只有真的跑过请求的那些小时**，按时间升序。
   *
   * 静默的小时刻意缺席：那种小时里「命中率」这件事不存在，补一个 0 会被画成一根落到底的
   * 柱子，读起来像「那会儿缓存崩了」。画图那头据此留空。
   */
  points: CacheSeriesPoint[]
}

/**
 * 拉一段全池缓存命中率的逐小时流水。
 *
 * 桶固定是小时、由浏览器按自己的时区合成「天」（见 `bucketCacheSeries`）：小时的边界与
 * 时区无关，而服务端按 UTC 切出来的「一天」在 UTC+8 看是 08:00–08:00。
 */
export async function getCacheSeries(hours: number): Promise<CacheSeries> {
  const { data } = await api.get('/metrics/cache-series', { params: { hours } })
  return data
}

/**
 * 一类缓存结局在这段时间里的合计。
 *
 * `input_tokens` 与 `cached_tokens` 都给，而**排序看的是两者之差**（白付的那部分）：按请求
 * 条数排出来的原因榜是骗人的——一条 200K 前缀的对话未命中和一条 2K 小请求未命中，账单上差
 * 一百倍，按条数看却各占一条。
 */
export interface CacheReasonStat {
  /** 原因标识。含义见 `CACHE_REASONS`（前端那份对照表）。 */
  reason: string
  requests: number
  input_tokens: number
  cached_tokens: number
}

export interface CacheReasons {
  /** 真实起点（Unix 秒），同 `CacheSeries.since`。 */
  since: number
  /** 已按白付 token 从多到少排好。前端不要重排——那个口径由后端定。 */
  reasons: CacheReasonStat[]
}

/** 拉一段缓存未命中的原因分布。回答命中率曲线的下一个问题：为什么低。 */
export async function getCacheReasons(hours: number): Promise<CacheReasons> {
  const { data } = await api.get('/metrics/cache-reasons', { params: { hours } })
  return data
}

/** 一对「要的模型 → 实际服务的模型」在这段时间里的合计。 */
export interface ModelRoutingPair {
  /** 客户端要的那个。 */
  req_model: string
  /** 上游实际给的那个。必然与 `req_model` 不同——相同的请求后端压根不记。 */
  model: string
  requests: number
  /** 这一对上花掉的钱，按**实际服务的**模型计价（账单认的是它）。 */
  cost_usd: number
}

/** 一个账号这段时间里被改路由的情况。 */
export interface ModelRoutingAccount {
  cred_id: number
  /**
   * 分母：经这个号、**看得出上游给了哪个模型**的请求数。
   *
   * 错误响应那类不算（上游没生成，谈不上路由），所以它比这个号的总条数小。口径由后端定，
   * 前端不要拿别的数当分母——一批 429 会把这个比例凭空稀释一半，而那批请求根本没有路由可言。
   */
  observed: number
  /** 其中要的与给的不是同一个模型的那些。**必然大于 0**。 */
  routed: number
  /** 已按条数从多到少排好。前端不要重排。 */
  pairs: ModelRoutingPair[]
}

export interface ModelRouting {
  /** 真实起点（Unix 秒），同 `CacheSeries.since`。 */
  since: number
  /** **只有真被改过路由的号在里面**：一条都没有的号压根不回。 */
  accounts: ModelRoutingAccount[]
}

/**
 * 拉一段各账号「上游有多少请求没给要的那个模型」。
 *
 * 明细里一行一条说得出「这一条被改了」，说不出「这件事在这个号上有多常发生」——而后者才
 * 决定要不要去动那个号上的模型配置。按号分是因为改路由是上游**对着某个账号**做的决定。
 */
export async function getModelRouting(hours: number): Promise<ModelRouting> {
  const { data } = await api.get('/metrics/model-routing', { params: { hours } })
  return data
}
