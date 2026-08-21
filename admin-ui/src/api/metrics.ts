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
}

export async function getMetrics(): Promise<Metrics> {
  const { data } = await api.get('/metrics')
  return data
}
