import { api } from './client'

export interface Settings {
  /**
   * 接入 key 的明文（空串 = 未设置）。
   *
   * 后端在管理鉴权之后才回它，为的是让设置页拼得出一段可直接粘进 `~/.codex/config.toml`
   * 的配置片段。未设管理密码时管理接口是敞开的，那时这个值也就跟着敞开——设置页因此把
   * 「设置管理密码」摆在同一屏里提示。
   */
  api_key: string
  /** 由 `--api-key` / `COBAN_API_KEY` 接管时为 true，网页上只读。 */
  env_managed: boolean
  /** 全局默认的每账号 RPM 上限（0 = 不限）。 */
  default_rpm_limit: number
  /** 撞上游限流后最多再换几个账号重试（0 = 不重试，把上游的 429 原样交回）。 */
  rate_limit_retry_max: number
  /** 额度用到百分之多少就暂停这个账号（0 = 不暂停）。 */
  quota_pause_pct: number
  /** 撞 429 后该账号冷却多久（秒）。 */
  cooldown_secs: number
  /** 管理密码是否已设置。未设置时管理接口是完全敞开的。 */
  admin_configured: boolean
  version: string
}

export async function getSettings(): Promise<Settings> {
  const { data } = await api.get('/settings')
  return data
}

// 所有写接口都回**最新的整份设置**：保存后界面要立刻按新值重绘（配置片段里就嵌着 key），
// 回一个空壳就得再拉一次，中间那一帧显示的是旧值。

/** 空串 = 清除接入 key（此后代理不校验来访身份）。 */
export async function setApiKey(apiKey: string): Promise<Settings> {
  const { data } = await api.post('/settings/api-key', { value: apiKey })
  return data
}

export async function setDefaultRpmLimit(limit: number): Promise<Settings> {
  const { data } = await api.post('/settings/default-rpm-limit', { value: limit })
  return data
}

export async function setRateLimitRetryMax(n: number): Promise<Settings> {
  const { data } = await api.post('/settings/rate-limit-retry-max', { value: n })
  return data
}

export async function setQuotaPausePct(pct: number): Promise<Settings> {
  const { data } = await api.post('/settings/quota-pause-pct', { value: pct })
  return data
}

export async function setCooldownSecs(secs: number): Promise<Settings> {
  const { data } = await api.post('/settings/cooldown-secs', { value: secs })
  return data
}
