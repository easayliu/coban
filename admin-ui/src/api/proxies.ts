import { api } from './client'

/** 代理池里的一条。 */
export interface SavedProxy {
  id: number
  label: string
  /** 规范化后的完整 URL，含 user:pass——展示时用 `proxyMaskedUrl` 打码。 */
  url: string
  created_at: number
  /** 正在用这条代理的账号名称（按 URL 对上）。 */
  credential_labels: string[]
}

/** 一次代理测试的结果，见后端 `clients::test_proxy`。 */
export interface ProxyTestResult {
  ok: boolean
  /** 规范化后实际测的 URL（socks5:// 已改成 socks5h://）。 */
  proxy: string
  /** 上游状态码；0 = 请求没到上游。 */
  status: number
  latency_ms: number
  /** 上游看到的出口 IP。 */
  ip: string | null
  /** 出口国家/地区码（如 `US`）。 */
  loc: string | null
  /** 接入的 Cloudflare 机房（如 `LAX`）。 */
  colo: string | null
  error: string | null
}

export interface ProxyImportReport {
  added: number
  duplicated: number
  /** [行号, 原因]，行号从 1 起。 */
  invalid: [number, string][]
}

/** 行里没写协议时补的那个。 */
export type ProxyImportScheme = 'http' | 'https' | 'socks5h'

export async function listProxies(): Promise<SavedProxy[]> {
  const { data } = await api.get<SavedProxy[]>('/proxies')
  return data
}

/**
 * 经这条代理连一次 `chatgpt.com`，看通不通、出口在哪。收的是 URL，未保存的也能测。
 * URL 本身不合法时后端回 400。
 */
export async function testProxy(url: string): Promise<ProxyTestResult> {
  const { data } = await api.post<ProxyTestResult>('/proxies/test', { value: url })
  return data
}

/** `label` 留空时后端按 host:port 起名。 */
export async function addProxy(label: string, url: string): Promise<SavedProxy> {
  const { data } = await api.post<SavedProxy>('/proxies', { label, url })
  return data
}

/** 地址变了的话，正在用旧地址的账号会被后端一并改过去。 */
export async function updateProxy(id: number, label: string, url: string): Promise<SavedProxy> {
  const { data } = await api.post<SavedProxy>(`/proxies/${id}`, { label, url })
  return data
}

/** 从池里删掉，不动账号——在用的号照旧走那个地址。返回实际删掉的条数。 */
export async function deleteProxies(ids: number[]): Promise<number> {
  const { data } = await api.post<{ deleted: number }>('/proxies/delete', { ids })
  return data.deleted
}

export async function importProxies(text: string, scheme: ProxyImportScheme): Promise<ProxyImportReport> {
  const { data } = await api.post<ProxyImportReport>('/proxies/import', { text, scheme })
  return data
}
