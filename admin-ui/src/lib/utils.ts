import { clsx, type ClassValue } from 'clsx'
import { twMerge } from 'tailwind-merge'
import type { Language } from '@/lib/i18n'

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs))
}

// OAuth 未返回可用账号身份且用户未填写备注时，后端会生成这个精确格式的兜底名。
// 它是展示占位符而非用户数据，所以按当前界面语言显示；其余名称必须原样保留。
const GENERATED_CREDENTIAL_LABEL = /^(?:账号|Account)\s+(\d+)$/

export function displayCredentialLabel(label: string, language: Language = 'zh-CN'): string {
  const match = label.trim().match(GENERATED_CREDENTIAL_LABEL)
  if (!match) return label
  return language === 'zh-CN' ? `账号 ${match[1]}` : `Account ${match[1]}`
}

type LocalizedBackendMessage = readonly [chinese: string, english: string]

// 管理 API 以前直接返回中文，当前版本改为英文。错误响应仍是纯文本以兼容已有调用方，
// 因此前端在这里识别两代稳定文案，而不是把服务端输出直接塞进中英文界面。
const FIXED_BACKEND_MESSAGES: readonly LocalizedBackendMessage[] = [
  ['需要管理密码', 'admin password required'],
  ['尚未设置管理密码', 'no admin password has been set yet'],
  ['密码错误', 'wrong password'],
  ['已设置管理密码', 'an admin password is already set'],
  ['密码至少 4 位', 'password must be at least 4 characters'],
  ['管理密码由环境变量接管，无法在网页修改', 'the admin password is managed by an environment variable and cannot be changed from the web UI'],
  ['凭证不存在', 'credential not found'],
  ['设备绑定不存在（可能已过期或已换到其它账号）', 'device binding not found (it may have expired or moved to another credential)'],
  ['请至少选择一个账号', 'select at least one credential'],
  ['名称不能为空', 'the name must not be empty'],
  ['请填写要测试的模型名', 'specify the model name to test'],
  ['接入 Key 已由环境变量 COBAN_API_KEY 接管，无法在网页修改', 'the inbound key is managed by the COBAN_API_KEY environment variable and cannot be changed from the web UI'],
  ['这次登录已过期或未找到，请重新点「添加账号」生成授权链接', 'this login attempt expired or was not found; click \'Add account\' again to generate a new authorization link'],
  ['粘贴内容格式应为 `code#state`', 'the pasted value must look like `code#state`'],
  ['state 不匹配，可能存在 CSRF 或粘贴错误；请重新登录', 'state mismatch, possibly CSRF or a bad paste; please log in again'],
  ['无效的 API Key', 'invalid API key'],
  ['缺少有效的设备身份（metadata.user_id）', 'missing a usable device identity (metadata.user_id)'],
  ['所有凭证的设备数均已达上限，暂无可用名额', 'all credentials have reached their device limits; no slot is available'],
  ['没有可用凭证，请先登录', 'no available credentials; add an account first'],
  ['已试过的凭证之外没有其它可用账号', 'no other available credentials remain after excluding those already tried'],
  ['token 刷新仍在后台完成', 'the token refresh continues in the background'],
  ['等待上游响应未完成', 'still waiting on the upstream response'],
]

function inLanguage([chinese, english]: LocalizedBackendMessage, language: Language): string {
  return language === 'zh-CN' ? chinese : english
}

function joinBackendDetail(prefix: string, detail: string, language: Language): string {
  if (!detail) return prefix
  return `${prefix}${language === 'zh-CN' ? '：' : ': '}${detail}`
}

function colonDetail(message: string, prefix: string): string | null {
  if (message === prefix) return ''
  if (!message.startsWith(prefix)) return null
  const match = message.slice(prefix.length).match(/^\s*[:：]\s*(.*)$/s)
  return match ? match[1].trim() : null
}

function localizeCompactDuration(value: string, language: Language): string {
  if (language === 'zh-CN') {
    const compact = [...value.matchAll(/(\d+)\s*([dhms])/g)]
    if (compact.length && compact.map((m) => m[0]).join('').replace(/\s/g, '') === value.replace(/\s/g, '')) {
      const unit: Record<string, string> = { d: '天', h: '小时', m: '分钟', s: '秒' }
      return compact.map((m) => `${m[1]} ${unit[m[2]]}`).join(' ')
    }
    return value
  }

  const chinese = [...value.matchAll(/(\d+)\s*(天|小时|分钟|秒)/g)]
  if (chinese.length && chinese.map((m) => m[0]).join('').replace(/\s/g, '') === value.replace(/\s/g, '')) {
    const unit: Record<string, string> = { 天: 'd', 小时: 'h', 分钟: 'm', 秒: 's' }
    return chinese.map((m) => `${m[1]}${unit[m[2]]}`).join(' ')
  }
  return value
}

function localizeKnownBackendMessage(message: string, language: Language, depth: number): string {
  const fixed = FIXED_BACKEND_MESSAGES.find(([chinese, english]) => message === chinese || message === english)
  if (fixed) return inLanguage(fixed, language)

  const localizeDetail = (detail: string) => depth < 3
    ? localizeKnownBackendMessage(detail, language, depth + 1)
    : detail

  for (const [chinese, english] of [
    ['请求 token 端点失败', 'request to the token endpoint failed'],
    ['取 token 失败', 'failed to get a token'],
    ['token 刷新任务异常退出', 'the token refresh task died'],
    ['读取上游响应体失败', 'failed to read the upstream response body'],
    ['无法定位用户主目录', 'could not determine the user home directory'],
    ['创建目录失败', 'failed to create directory'],
    ['打开凭证库失败', 'failed to open credential database'],
    ['插入凭证失败（refresh_token 可能已存在）', 'failed to insert credential (the refresh_token may already exist)'],
    ['读取新插入凭证失败', 'failed to read the newly inserted credential'],
    ['初始化凭证库 schema 失败', 'failed to initialize credential database schema'],
    ['清理无主用量日志失败', 'failed to purge orphaned usage logs'],
    ['清理无主设备绑定失败', 'failed to purge orphaned device bindings'],
    ['清理无主账本失败', 'failed to purge orphaned credential ledger entries'],
    ['清理无主设备费用失败', 'failed to purge orphaned device cost entries'],
    ['迁移 credentials 为 AUTOINCREMENT 失败', 'failed to migrate credentials to AUTOINCREMENT'],
  ] as const) {
    const detail = colonDetail(message, chinese) ?? colonDetail(message, english)
    if (detail != null) return joinBackendDetail(inLanguage([chinese, english], language), localizeDetail(detail), language)
  }

  const tokenEndpoint = message.match(/^(?:token endpoint returned|token 端点返回)\s+(.+?)(?:\s*[:：]\s*(.*))?$/i)
  if (tokenEndpoint) {
    const prefix = inLanguage(['Token 端点返回', 'Token endpoint returned'], language)
    const status = tokenEndpoint[1].trim()
    const detail = tokenEndpoint[2]?.trim() ?? ''
    return joinBackendDetail(`${prefix} ${status}`, localizeDetail(detail), language)
  }

  const parsedToken = message.match(/^failed to parse the token response\s*\((\d+)\s+bytes\)$/i)
  if (parsedToken) {
    return language === 'zh-CN'
      ? `无法解析 Token 响应（${parsedToken[1]} 字节）`
      : `Failed to parse the token response (${parsedToken[1]} bytes)`
  }
  for (const [chinese, english] of [['解析 token 响应失败', 'failed to parse the token response']] as const) {
    const detail = colonDetail(message, chinese) ?? colonDetail(message, english)
    if (detail != null) return joinBackendDetail(inLanguage(['无法解析 Token 响应', 'Failed to parse the token response'], language), localizeDetail(detail), language)
  }

  const probeTimeout = message.match(/^connectivity test timed out \(overall cap (\d+)s\)(?:\s*:\s*(.*))?$/i)
    ?? message.match(/^连通性测试超时（总上限\s*(\d+)\s*秒）(?:\s*[：:]\s*(.*))?$/)
  if (probeTimeout) {
    const prefix = language === 'zh-CN'
      ? `连通性测试超时（总上限 ${probeTimeout[1]} 秒）`
      : `Connectivity test timed out (overall cap ${probeTimeout[1]}s)`
    return joinBackendDetail(prefix, localizeDetail(probeTimeout[2]?.trim() ?? ''), language)
  }

  const upstreamRequest = message.match(/^upstream request failed\s*\[([^\]]+)]\s*:\s*(.*)$/i)
    ?? message.match(/^上游请求失败\s*\[([^\]]+)]\s*[：:]\s*(.*)$/)
  if (upstreamRequest) {
    const prefix = inLanguage(['上游请求失败', 'Upstream request failed'], language)
    return joinBackendDetail(`${prefix} [${upstreamRequest[1]}]`, localizeDetail(upstreamRequest[2].trim()), language)
  }

  const bodyTimeout = message.match(/^reading the upstream response body timed out \(overall cap (\d+)s\)$/i)
    ?? message.match(/^读取上游响应体超时（总上限\s*(\d+)\s*秒）$/)
  if (bodyTimeout) {
    return language === 'zh-CN'
      ? `读取上游响应体超时（总上限 ${bodyTimeout[1]} 秒）`
      : `Reading the upstream response body timed out (overall cap ${bodyTimeout[1]}s)`
  }

  const rateLimit = message.match(/^upstream rate limit: account quota exhausted, scheduling resumes automatically in about (.+)$/i)
    ?? message.match(/^上游限流：账号额度耗尽，约 (.+) 后自动恢复调度$/)
  if (rateLimit) {
    const duration = localizeCompactDuration(rateLimit[1].trim(), language)
    return language === 'zh-CN'
      ? `上游限流：账号额度耗尽，约 ${duration} 后自动恢复调度`
      : `Upstream rate limit: account quota exhausted; scheduling resumes automatically in about ${duration}`
  }

  const bareRateLimit = message.match(/^all credentials have reached the bare-request rate limit; retry in (\d+) seconds$/i)
    ?? message.match(/^所有凭证的裸请求速率均已达上限，请\s*(\d+)\s*秒后重试$/)
  if (bareRateLimit) {
    const seconds = bareRateLimit[1]
    return language === 'zh-CN'
      ? `所有凭证的裸请求速率均已达上限，请 ${seconds} 秒后重试`
      : `All credentials have reached the bare-request rate limit; retry in ${seconds} seconds`
  }

  const allRateLimited = message.match(/^all credentials are cooling down after upstream rate limits; retry in (\d+) seconds$/i)
    ?? message.match(/^所有凭证均处于上游限流冷却中，请\s*(\d+)\s*秒后重试$/)
  if (allRateLimited) {
    const seconds = allRateLimited[1]
    return language === 'zh-CN'
      ? `所有凭证均处于上游限流冷却中，请 ${seconds} 秒后重试`
      : `All credentials are cooling down after upstream rate limits; retry in ${seconds} seconds`
  }

  const refreshCouldNotDisable = message.match(/^credential #(\d+) refresh failed and could not be disabled\s*:\s*(.*)$/i)
    ?? message.match(/^凭证 #(\d+) 刷新失败且停用未生效\s*[：:]\s*(.*)$/)
  if (refreshCouldNotDisable) {
    const prefix = language === 'zh-CN'
      ? `凭证 #${refreshCouldNotDisable[1]} 刷新失败且停用未生效`
      : `Credential #${refreshCouldNotDisable[1]} refresh failed and could not be disabled`
    return joinBackendDetail(prefix, localizeDetail(refreshCouldNotDisable[2].trim()), language)
  }

  const allRefreshAttempts = message.match(/^all (\d+) credential refresh attempts failed; no credentials are available$/i)
    ?? message.match(/^连续 (\d+) 个凭证刷新失败，暂无可用账号$/)
  if (allRefreshAttempts) {
    const count = allRefreshAttempts[1]
    return language === 'zh-CN'
      ? `连续 ${count} 个凭证刷新失败，暂无可用账号`
      : `All ${count} credential refresh attempts failed; no credentials are available`
  }

  const refreshBan = message.match(/^\[refresh\s+(\d+)]\s*(.*)$/i)
  if (refreshBan) {
    const prefix = language === 'zh-CN'
      ? `刷新 Token 失败（HTTP ${refreshBan[1]}）`
      : `Token refresh failed (HTTP ${refreshBan[1]})`
    return joinBackendDetail(prefix, localizeDetail(refreshBan[2].trim()), language)
  }

  const upstreamBan = message.match(/^\[(\d{3})]\s*(.*)$/)
  if (upstreamBan) {
    const prefix = language === 'zh-CN'
      ? `上游账号错误（HTTP ${upstreamBan[1]}）`
      : `Upstream account error (HTTP ${upstreamBan[1]})`
    return joinBackendDetail(prefix, localizeDetail(upstreamBan[2].trim()), language)
  }

  return message
}

/**
 * 将 coban 自身的稳定错误文本按界面语言呈现；上游与传输层的未知细节保持原样，方便排障。
 *
 * 管理 API 仍是纯文本错误响应，且用户可能连到旧版本后端，因此同时识别中英文两套文案。
 */
export function localizeBackendMessage(message: string, language: Language = 'zh-CN'): string {
  const normalized = message.trim()
  return normalized ? localizeKnownBackendMessage(normalized, language, 0) : normalized
}

/**
 * 从 axios / Error 中提取用户友好的错误信息。
 *
 * 后端的错误体是纯文本，有就直接用（{@link localizeBackendMessage} 认得那几套稳定文案）。
 * **没有可读错误体时不要退回 axios 那句原文**：`Request failed with status code 405` 说的是
 * 「有个请求失败了」，用户从它推不出任何下一步动作，而这几种情形各自都有一句能指到病因的话。
 */
export function extractError(error: unknown, language: Language = 'zh-CN'): string {
  if (error && typeof error === 'object') {
    const e = error as {
      code?: string
      response?: { status?: number; data?: unknown }
      message?: string
    }
    if (typeof e.response?.data === 'string' && e.response.data.trim()) {
      return localizeBackendMessage(e.response.data, language)
    }

    const status = e.response?.status
    // 404/405 在管理 API 上有一个确定的含义：**这个接口不存在**。SPA 兜底只收 GET，于是
    // 打到一条后端还没有的 POST 路由上时，返回的正是一个空体 405。几乎总是因为跑的二进制
    // 比前端旧（前端是运行时从 dist 读的，后端不是），所以直接把「重启服务」说出来。
    if (status === 404 || status === 405) {
      return language === 'zh-CN'
        ? `后端没有这个接口（HTTP ${status}）：正在运行的 coban 可能比页面旧，重启服务后再试`
        : `The backend has no such endpoint (HTTP ${status}); the running coban may be older than this page — restart it and try again`
    }
    // 请求发出去了但没等到响应：超时与断链要分开说，前者该等/该重试，后者是服务没在跑。
    if (e.code === 'ECONNABORTED' || e.code === 'ETIMEDOUT') {
      return language === 'zh-CN' ? '请求超时，未收到后端响应' : 'The request timed out with no response from the backend'
    }
    if (e.code === 'ERR_NETWORK') {
      return language === 'zh-CN'
        ? '连不上后端服务，确认它还在运行'
        : 'Cannot reach the backend service; check that it is still running'
    }
    if (status) {
      return language === 'zh-CN' ? `请求失败（HTTP ${status}）` : `Request failed (HTTP ${status})`
    }
    if (e.message) return localizeBackendMessage(e.message, language)
  }
  return language === 'zh-CN' ? '未知错误' : 'Unknown error'
}

const DURATION_UNITS = [86400, 3600, 60, 1] as const

/**
 * 时长（秒）格式化为「1 天 6 小时」，用于配置项回显这类**静态时长**。
 *
 * 取最高的两个非零单位、不丢余数：这里是给用户核对自己填的秒数的，宁可啰嗦也不能糊
 * （108000 显示成「1 天」会让人以为填对了整天）。倒计时不要用它——倒计时得自己走，
 * 否则渲染完就冻住；界面上的到期/重置一律用 {@link formatClockTime} 显示绝对时刻。
 */
export function formatDuration(secs: number, language: Language = 'zh-CN'): string {
  if (secs <= 0) return language === 'zh-CN' ? '0 秒' : '0 seconds'
  let rest = Math.floor(secs)
  const parts: string[] = []
  for (const size of DURATION_UNITS) {
    const n = Math.floor(rest / size)
    rest -= n * size
    if (n > 0) {
      if (language === 'zh-CN') {
        const name = size === 86400 ? '天' : size === 3600 ? '小时' : size === 60 ? '分钟' : '秒'
        parts.push(`${n} ${name}`)
      } else {
        const unit = size === 86400 ? 'day' : size === 3600 ? 'hour' : size === 60 ? 'minute' : 'second'
        parts.push(`${n} ${unit}${n === 1 ? '' : 's'}`)
      }
    }
    if (parts.length === 2) break
  }
  return parts.join(' ')
}

const WEEKDAYS = ['周日', '周一', '周二', '周三', '周四', '周五', '周六']

const pad = (n: number) => String(n).padStart(2, '0')

/** 相差多少个自然日（按本地时区的午夜切分，不是 24 小时整除）。 */
function calendarDaysFromNow(d: Date): number {
  const now = new Date()
  const a = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime()
  const b = new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime()
  return Math.round((b - a) / 86_400_000)
}

/**
 * Unix 秒 → 本地时区的绝对时刻，粒度随距离放宽：
 * 今天 `21:30`／明天 `明天 03:00`／一周内 `周三 09:00`／更远 `8/5 09:00`。
 *
 * 用绝对时刻而非倒计时：既省掉了 ticker，也避开了浏览器时钟偏差——歪的是本地时钟时，
 * 倒计时会直接算错，而绝对时刻只是按本地时区渲染同一个瞬间，仍然对得上用户的表。
 */
export function formatClockTime(unixSecs: number, language: Language = 'zh-CN'): string {
  const d = new Date(unixSecs * 1000)
  const hm = `${pad(d.getHours())}:${pad(d.getMinutes())}`
  const days = calendarDaysFromNow(d)
  if (days === 0) return hm
  if (days === 1) return language === 'zh-CN' ? `明天 ${hm}` : `Tomorrow ${hm}`
  // 7 天开外星期几会重名（「周三」可能是今天也可能是下周三），改用日期。
  if (days > 1 && days < 7) {
    const weekday = language === 'zh-CN'
      ? WEEKDAYS[d.getDay()]
      : new Intl.DateTimeFormat('en-US', { weekday: 'short' }).format(d)
    return `${weekday} ${hm}`
  }
  return `${d.getMonth() + 1}/${d.getDate()} ${hm}`
}

/** Unix 秒 → 完整本地时间，给 title 兜底（粗粒度显示看不出具体是哪天）。 */
export function formatFullTime(unixSecs: number, _language: Language = 'zh-CN'): string {
  const d = new Date(unixSecs * 1000)
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
}

/** 复制文本到剪贴板：安全上下文用现代 API，否则回退 execCommand（http/局域网可用）。 */
export async function copyText(text: string): Promise<boolean> {
  if (!text) return false
  if (navigator.clipboard && window.isSecureContext) {
    try {
      await navigator.clipboard.writeText(text)
      return true
    } catch {
      // 继续走回退
    }
  }
  try {
    const ta = document.createElement('textarea')
    ta.value = text
    ta.setAttribute('readonly', '')
    ta.style.position = 'fixed'
    ta.style.left = '-9999px'
    document.body.appendChild(ta)
    ta.select()
    ta.setSelectionRange(0, text.length)
    const ok = document.execCommand('copy')
    document.body.removeChild(ta)
    return ok
  } catch {
    return false
  }
}

/**
 * 剩余秒数 → 紧凑倒计时：`45m`、`2h 5m`、`6d 0h`，不足一分钟为 `<1m`。
 *
 * 中英文共用这套缩写：它出现在卡片上最挤的那一行（窗口标签 + 进度条 + 百分比 + 倒计时），
 * 「6 天 0 小时」会把进度条压没，而 d/h/m 在两种语言里都读得懂。
 *
 * **必须配一个会走的时钟**：调用方传入的 `nowSecs` 要来自页面 tick（见 credential-workspace
 * 的 useNowSeconds，30 秒一跳且切回前台立刻校准），否则渲染完就冻在那一刻。需要精确到分秒
 * 的地方仍用 {@link formatFullTime} 给绝对时刻——倒计时受本地时钟偏差影响，只适合看个大概。
 */
export function formatCountdown(targetSecs: number, nowSecs: number): string {
  const left = Math.max(0, targetSecs - nowSecs)
  if (left < 60) return '<1m'
  const minutes = Math.floor(left / 60)
  if (minutes < 60) return `${minutes}m`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours}h ${minutes % 60}m`
  return `${Math.floor(hours / 24)}d ${hours % 24}h`
}

/**
 * 大数字压成一眼可读的短形式：`3542` → `3.5K`（英文）/`3542` → `3542`、`12000` → `1.2万`
 * （中文，随 locale 走）。
 *
 * 只用于空间紧张、量级比精确值更有用的地方（卡片上的请求数）。要对数的场合仍用
 * `toLocaleString`——`3.5K` 看不出到底是 3542 还是 3549。
 */
export function formatCompactNumber(n: number, locale: string): string {
  return new Intl.NumberFormat(locale, {
    notation: 'compact',
    maximumFractionDigits: 1,
  }).format(n)
}

/**
 * Token 数压成 `842` / `931K` / `1.2M` / `3.4B`，**不随界面语言变**。
 *
 * 刻意不走 {@link formatCompactNumber}：中文 locale 下它会给出「121万」，而 token 的量纲
 * 到处都是 K/M——官方价目表按 MTok 计价，卡片上紧挨着的就是那份价目算出来的费用，两个数换算
 * 单位不一致就没法互相印证。同理不做本地化千分位。
 *
 * 精确值放 `title`（用 `toLocaleString`）：`1.2M` 看不出是 1.15M 还是 1.24M。
 */
export function formatTokens(n: number): string {
  const abs = Math.abs(n)
  if (abs < 1_000) return String(Math.round(n))
  const units = [[1e9, 'B'], [1e6, 'M'], [1e3, 'K']] as const
  for (let i = 0; i < units.length; i++) {
    const [size, unit] = units[i]
    if (abs < size) continue
    const scaled = n / size
    // 三位有效数字以内保留一位小数（`1.2M`），到了三位整数就不再要小数（`931K`）。
    const value = Math.abs(scaled) >= 100 ? Math.round(scaled) : Number(scaled.toFixed(1))
    // 舍入后正好顶到 1000 时进位一档：999_999 该读成 `1M`，而不是 `1000K`。
    if (Math.abs(value) >= 1_000 && i > 0) {
      const [bigger, biggerUnit] = units[i - 1]
      return `${Number((n / bigger).toFixed(1))}${biggerUnit}`
    }
    return `${value}${unit}`
  }
  return String(Math.round(n))
}

/** 美元金额格式化：极小额多留几位小数，便于看清单次费用。 */
export function formatUsd(v: number): string {
  if (!v || v <= 0) return '$0.00'
  if (v < 0.01) return `$${v.toFixed(4)}`
  if (v < 1) return `$${v.toFixed(3)}`
  return `$${v.toFixed(2)}`
}

/** Unix 秒时间戳 → 相对指定时钟的「x 前」；传入时钟可让整页状态在同一轮同步更新。 */
export function relativeTime(
  unixSecs: number,
  nowSecs = Math.floor(Date.now() / 1000),
  language: Language = 'zh-CN',
): string {
  const diff = nowSecs - unixSecs
  if (diff < 60) return language === 'zh-CN' ? '刚刚' : 'Just now'
  const min = Math.floor(diff / 60)
  if (min < 60) {
    return language === 'zh-CN'
      ? `${min} 分钟前`
      : `${min} ${min === 1 ? 'minute' : 'minutes'} ago`
  }
  const hours = Math.floor(min / 60)
  if (hours < 24) {
    return language === 'zh-CN'
      ? `${hours} 小时前`
      : `${hours} ${hours === 1 ? 'hour' : 'hours'} ago`
  }
  const days = Math.floor(hours / 24)
  return language === 'zh-CN'
    ? `${days} 天前`
    : `${days} ${days === 1 ? 'day' : 'days'} ago`
}
