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
