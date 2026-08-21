import { useEffect, useRef, useState, type ReactNode } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import axios from 'axios'
import {
  ActivityIcon, CircleCheckIcon, CircleXIcon, GaugeIcon, GlobeIcon, PencilIcon, RefreshCwIcon,
  ScrollTextIcon, TimerOffIcon, Trash2Icon,
} from 'lucide-react'
import {
  clearCooldown, deleteCredential, listCredentialModels, probeCredential, refreshCredential,
  setDisabled, setLabel, setPriority, setProxy, setRpmLimit,
  type Credential, type ProbeResult, type Quota, type WindowUsage,
} from '@/api/credentials'
import {
  cn, displayCredentialLabel, extractError, formatClockTime, formatFullTime,
  localizeBackendMessage,
} from '@/lib/utils'
import { localize, useI18n, type Language } from '@/lib/i18n'
import {
  AlertDialog, AlertDialogClose, AlertDialogDescription, AlertDialogFooter,
  AlertDialogHeader, AlertDialogPopup, AlertDialogTitle,
} from '@/components/ui/alert-dialog'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Badge, type BadgeProps } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  Combobox, ComboboxItem, ComboboxPopup, ComboboxTrigger, ComboboxValue,
} from '@/components/ui/combobox'
import {
  Dialog, DialogDescription, DialogHeader, DialogPanel, DialogPopup, DialogTitle,
} from '@/components/ui/dialog'
import { Empty, EmptyDescription, EmptyHeader, EmptyTitle } from '@/components/ui/empty'
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import { Form } from '@/components/ui/form'
import { MenuItem, MenuPopup, MenuSeparator } from '@/components/ui/menu'
import { Spinner } from '@/components/ui/spinner'
import { toastManager } from '@/components/ui/toast'

/** RPM 上限输入框的初值：跟随默认→空串；明确不限→0；独立上限→数值。 */
export function limitToInput(rpmLimit: number): string {
  if (rpmLimit === 0) return ''
  return rpmLimit < 0 ? '0' : String(rpmLimit)
}

/** 输入框内容 → 后端三态值：空=跟随全局默认(0)；0/负=该账号不限(-1)；正数=独立上限。 */
export function inputToLimit(v: string): number {
  const t = v.trim()
  if (t === '') return 0
  const n = Math.floor(Number(t))
  return Number.isFinite(n) && n > 0 ? n : -1
}

export type QuotaFreshness = 'current' | 'unknown' | 'expired'
export type QuotaLevel = 'empty' | 'ok' | 'warning' | 'critical'

/** 一个额度窗口解释后的样子。 */
export interface QuotaWindowMeta {
  /** `primary` / `secondary`——codex 只有这两个。 */
  key: 'primary' | 'secondary'
  /** 窗口长度（分钟），上游报的。写死成「5 小时 / 一周」会在别的套餐上显示错。 */
  windowMinutes: number | null
  /** 仍属于当前窗口的使用率百分比；明确已重置时为空。 */
  percentage: number | null
  /** 快照里的原值，不做「已重置就清空」处理。 */
  rawPercentage: number | null
  resetAt: number | null
  freshness: QuotaFreshness
  level: QuotaLevel
  /**
   * 这个窗口**当前周期内**的请求数 / token / 费用，由后端按「重置时刻 − 窗口长度」反推起点
   * 后从流水聚合。上游没报这个窗口时为 `null`。
   *
   * 与百分比是两个视角：百分比是上游说的「还剩多少」，这三项是「这段时间里到底发生了什么」。
   * 前者才是调度依据，后者用来解释它——19% 是 400 条小请求还是 3 条大的，只有这里看得出。
   */
  usage: WindowUsage | null
  /**
   * 上游**是否报告过这个窗口**（使用率与重置时刻有其一即算）。
   *
   * 与「暂无数据」是两回事，界面必须分开说：没有快照是「还没跑过请求，等等就有」，
   * 而有快照却缺这个窗口，意味着这个账号的额度模型里压根没有它，再等也不会出现。
   */
  reported: boolean
}

/**
 * credits 状态：基础额度满了之后还能不能继续跑，以及烧的是不是按量计费的钱。
 *
 * **`none` 是绝大多数账号的常态，不是故障**：一个普通 Plus/Pro 订阅压根没有额外 credits，
 * 上游对它恒回 `has_credits: false` + `balance: 0`。刻意**没有 `exhausted` 这一档**——
 * 单看这组头分不出「从来没有」和「用完了」（两者的取值一模一样），而把它判成「已用尽」
 * 就等于给每一个正常账号挂一条永久告警。
 *
 * 真正值得说的是反面：`available` / `unlimited` 意味着额度满了还会继续放行，**花的是钱**。
 */
export type CreditsState = 'none' | 'unlimited' | 'available' | 'unknown'

export interface QuotaRiskMeta {
  primary: QuotaWindowMeta
  secondary: QuotaWindowMeta
  windows: QuotaWindowMeta[]
  /** 是否已有额度快照；空态文案靠它区分「还没数据」与「无此窗口」。 */
  hasSnapshot: boolean
  /** 快照是什么时候的（Unix 秒）。过期的读数必须标出来，否则会被当成现状。 */
  snapshotTs: number | null
  nearLimit: boolean
  credits: CreditsState
  creditsBalance: number | null
}

export type CredentialStatusKind =
  | 'banned' | 'rate-limited' | 'disabled' | 'cooldown' | 'near-limit' | 'normal'

export interface CredentialStatusMeta {
  kind: CredentialStatusKind
  variant: BadgeProps['variant']
  label: string
  detail: string
  /** 是否需要人处理（「需处理」筛选与排序看它）。 */
  attention: boolean
  /** 排序权重：数值大者更该被先看到。 */
  rank: number
}

export interface CredentialEvaluation {
  credential: Credential
  quota: QuotaRiskMeta
  status: CredentialStatusMeta
  /** 现在能不能被选中转发。 */
  schedulable: boolean
  nearLimit: boolean
  quotaRisk: boolean
  needsAttention: boolean
}

const currentUnixSeconds = () => Math.floor(Date.now() / 1000)

/**
 * 使用率统一截断成界面展示的整数百分比，颜色与告警只读这个值。
 * 这样 89.9% 会显示为 89% 且不告警，真正达到 90% 时才同时变红并进入额度风险。
 */
export function quotaPercentage(usedPct: number | null): number | null {
  if (usedPct == null || !Number.isFinite(usedPct)) return null
  return Math.floor(Math.min(100, Math.max(0, usedPct)) + 1e-9)
}

export function quotaLevel(usedPct: number | null): QuotaLevel {
  const percentage = quotaPercentage(usedPct)
  if (percentage == null) return 'empty'
  if (percentage >= 90) return 'critical'
  if (percentage >= 70) return 'warning'
  return 'ok'
}

/** 上游的 `*_reset_at` 是个字符串（见过 Unix 秒也见过毫秒），统一成 Unix 秒。 */
function parseResetAt(raw: string | null | undefined): number | null {
  if (!raw) return null
  const n = Number(raw)
  if (!Number.isFinite(n) || n <= 0) return null
  return n > 100_000_000_000 ? Math.floor(n / 1000) : Math.floor(n)
}

function evaluateWindow(
  key: 'primary' | 'secondary',
  usedPct: number | null,
  windowMinutes: number | null,
  resetRaw: string | null,
  usage: WindowUsage | null,
  now: number,
): QuotaWindowMeta {
  const resetAt = parseResetAt(resetRaw)
  const freshness: QuotaFreshness =
    resetAt == null ? 'unknown' : resetAt <= now ? 'expired' : 'current'
  // 窗口已经过了重置点：那个百分比说的是上一个周期，照着它画进度条会让一个刚重置、
  // 满血的账号显示成 100%。
  const live = freshness === 'expired' ? null : usedPct
  return {
    key,
    windowMinutes,
    percentage: quotaPercentage(live),
    rawPercentage: usedPct,
    resetAt,
    freshness,
    level: quotaLevel(live),
    usage,
    // **判「上游有没有这个窗口」要看窗口本身，不能看使用率**：实测 Pro 账号只有 primary，
    // secondary 那组头回的是 `0% / 0 分钟 / 空重置时刻`——按「使用率非空即算」判，它会被
    // 画成一条 0% 的进度条，读起来像「还有一整个窗口的额度没用」，与事实正好相反。
    // 零长度的窗口不是窗口，故要求「有长度」或「有重置时刻」，另留一条非零使用率的兜底
    // （万一哪天上游只报使用率、不报窗口元数据）。
    reported: (windowMinutes ?? 0) > 0 || resetAt != null || (usedPct ?? 0) > 0,
  }
}

/**
 * 窗口长度 → 人话标签（`300` → `5h`，`10080` → `7d`）。上游没给就退回通用名。
 *
 * 只取 `key` 与 `windowMinutes` 两项，好让连通性测试那份**单次响应**的额度读数也能共用
 * 同一套标签——它手上没有卡片那份解释过的 {@link QuotaWindowMeta}，而两处标签不一致时，
 * 弹窗与卡片说的就像是两个不同的窗口。
 */
export function quotaWindowLabel(
  w: Pick<QuotaWindowMeta, 'key' | 'windowMinutes'>,
  language: Language,
): string {
  const generic = w.key === 'primary'
    ? localize(language, '主额度', 'Primary')
    : localize(language, '次额度', 'Secondary')
  const m = w.windowMinutes
  if (m == null || m <= 0) return generic
  if (m % (60 * 24) === 0) return `${m / (60 * 24)}d`
  if (m % 60 === 0) return `${m / 60}h`
  return `${m}m`
}

/** 把最新额度快照解释成当前可展示的窗口与风险。 */
export function quotaRiskMeta(cred: Credential, now = currentUnixSeconds()): QuotaRiskMeta {
  const q: Quota | null = cred.stats?.quota ?? null
  const primary = evaluateWindow(
    'primary', q?.primary_used_pct ?? null, q?.primary_window_minutes ?? null,
    q?.primary_reset_at ?? null, cred.stats?.primary_window ?? null, now,
  )
  const secondary = evaluateWindow(
    'secondary', q?.secondary_used_pct ?? null, q?.secondary_window_minutes ?? null,
    q?.secondary_reset_at ?? null, cred.stats?.secondary_window ?? null, now,
  )
  const windows = [primary, secondary]

  // `has_credits: false` 是**没有额外 credits**（普通订阅的常态），不是「用尽了」；
  // 见 CreditsState 的注。
  let credits: CreditsState = 'unknown'
  if (q == null) credits = 'unknown'
  else if (q.credits_unlimited === true) credits = 'unlimited'
  else if (q.credits_has_credits === true) credits = 'available'
  else if (q.credits_has_credits === false) credits = 'none'

  return {
    primary,
    secondary,
    windows,
    hasSnapshot: q != null,
    snapshotTs: cred.stats?.snapshot_ts ?? null,
    nearLimit: windows.some((w) => (w.percentage ?? -1) >= 90),
    credits,
    creditsBalance: q?.credits_balance ?? null,
  }
}

function quotaWarningDetail(quota: QuotaRiskMeta, language: Language): string {
  const hot = quota.windows
    .filter((w) => (w.percentage ?? -1) >= 90)
    .map((w) => `${quotaWindowLabel(w, language)} ${w.percentage}%`)
    .join(localize(language, '、', ', '))
  return localize(language, `额度接近上限：${hot}`, `Quota nearly exhausted: ${hot}`)
}

function statusFromQuota(
  cred: Credential,
  quota: QuotaRiskMeta,
  language: Language,
): CredentialStatusMeta {
  // 顺序即优先级：越靠前的越该盖过后面的。封禁排第一——它是唯一一个「不处理就永远不会好」的。
  if (cred.ban_reason && cred.resume_at == null) {
    return {
      kind: 'banned', variant: 'destructive',
      label: localize(language, '账号异常', 'Account error'),
      detail: localizeBackendMessage(cred.ban_reason, language),
      attention: true, rank: 5,
    }
  }
  if (cred.resume_at != null) {
    return {
      kind: 'rate-limited', variant: 'warning',
      label: localize(language, '限流暂停', 'Rate limited'),
      detail: localize(
        language,
        `已被上游限流，${formatFullTime(cred.resume_at)} 自动恢复`,
        `Paused by upstream rate limits; resumes at ${formatFullTime(cred.resume_at)}`,
      ),
      attention: false, rank: 4,
    }
  }
  if (cred.disabled) {
    return {
      kind: 'disabled', variant: 'secondary',
      label: localize(language, '已停用', 'Disabled'),
      detail: localize(language, '手动停用，不参与调度', 'Manually disabled; not scheduled'),
      attention: false, rank: 3,
    }
  }
  if (cred.cooldown_secs > 0) {
    return {
      kind: 'cooldown', variant: 'warning',
      label: localize(language, '冷却中', 'Cooling down'),
      detail: localize(
        language,
        `撞上游限流后冷却，还有 ${cred.cooldown_secs} 秒`,
        `Cooling down after an upstream 429; ${cred.cooldown_secs}s left`,
      ),
      attention: false, rank: 4,
    }
  }
  if (quota.nearLimit) {
    return {
      kind: 'near-limit', variant: 'warning',
      label: localize(language, '用量将满', 'Usage nearly full'),
      detail: quotaWarningDetail(quota, language), attention: true, rank: 2,
    }
  }
  return {
    kind: 'normal', variant: 'success',
    label: localize(language, '运行正常', 'Healthy'),
    detail: localize(language, '账号运行正常，可参与调度', 'This account is healthy and available for scheduling'),
    attention: false, rank: 0,
  }
}

/** 卡片、列表、概览、筛选和排序共同消费的一份账号状态解释。 */
export function evaluateCredential(
  cred: Credential,
  now = currentUnixSeconds(),
  language: Language = 'zh-CN',
): CredentialEvaluation {
  const quota = quotaRiskMeta(cred, now)
  const status = statusFromQuota(cred, quota, language)
  const nearLimit = !cred.disabled && quota.nearLimit
  return {
    credential: cred,
    quota,
    status,
    schedulable: !cred.disabled && !cred.ban_reason && cred.cooldown_secs <= 0,
    nearLimit,
    // 只看使用率。**「没有额外 credits」不算额度风险**：那是普通订阅的常态（见
    // CreditsState），算进来的话「额度风险」这个筛选会把每一个健康账号都框进去，
    // 于是这个筛选再也筛不出任何东西。
    quotaRisk: nearLimit,
    needsAttention: status.attention,
  }
}

/** 兼容卡片与紧凑列表：返回仍属于当前窗口的使用率。 */
export function liveQuota(
  cred: Credential,
  now = currentUnixSeconds(),
): { primary: number | null; secondary: number | null } {
  const quota = quotaRiskMeta(cred, now)
  return { primary: quota.primary.percentage, secondary: quota.secondary.percentage }
}

/** 账号是否处于「额度将满」（停用的不算；已重置的窗口不算）。 */
export function isNearLimit(cred: Credential, now = currentUnixSeconds()): boolean {
  return evaluateCredential(cred, now).nearLimit
}

/**
 * 账号是否异常——**只看被上游封禁**。
 *
 * token 过期不算：刷新是惰性的（选号之后、发请求之前必刷），所以闲置一夜的健康账号第二天
 * 必然是「已过期」，下一个请求会自动把它刷好。把它算成异常，等于每天早上给一批好号刷上
 * 红色、排到最前、塞进「需处理」——而这里真正要回答的是「refresh_token 还灵不灵」，
 * 那个答案在 `ban_reason`：刷新被上游明确拒掉时后端会写进去。
 *
 * 限流暂停（`resume_at != null`）同样不算：到点自己就回调度池了，不需要任何人处理。
 */
export function isAbnormal(cred: Credential): boolean {
  return !!cred.ban_reason && cred.resume_at == null
}

// ---------- 排序 ----------
//
// 排序模型放这里，列表表头与工具栏下拉共用同一份定义，避免两处各写一套导致
// 「表头能排的维度和下拉里的对不上」。

export type SortKey =
  | 'priority' | 'status' | 'name' | 'plan'
  | 'usagePrimary' | 'usageSecondary' | 'rpm'
  | 'cost' | 'requests' | 'tokens' | 'recent' | 'created'

export type SortDir = 'asc' | 'desc'

export const SORTS: { key: SortKey; label: string }[] = [
  { key: 'priority', label: '优先级' },
  { key: 'status', label: '状态' },
  { key: 'name', label: '名称' },
  { key: 'plan', label: '套餐' },
  { key: 'usagePrimary', label: '主额度使用率' },
  { key: 'usageSecondary', label: '次额度使用率' },
  { key: 'rpm', label: 'RPM 上限' },
  { key: 'cost', label: '累计花费' },
  { key: 'requests', label: '请求数' },
  { key: 'tokens', label: 'token 数' },
  { key: 'recent', label: '最近使用' },
  { key: 'created', label: '添加时间' },
]

export const SORT_KEYS = SORTS.map((s) => s.key)

/**
 * 各维度首次选中时的默认方向——按「用户多半想先看什么」定：
 * 优先级/名称是升序（P0 在前、A→Z），其余都是降序（最严重、用得最多、最贵、最近的排前面）。
 * 再次点击同一维度会翻转方向，此处只决定初值。
 */
export const SORT_DIR_DEFAULT: Record<SortKey, SortDir> = {
  priority: 'asc',
  name: 'asc',
  status: 'desc',
  plan: 'desc',
  usagePrimary: 'desc',
  usageSecondary: 'desc',
  rpm: 'desc',
  cost: 'desc',
  requests: 'desc',
  tokens: 'desc',
  recent: 'desc',
  created: 'desc',
}

/**
 * 上游给的套餐字符串 → 归一化的档位键。**排序、配色、筛选三处共用这一个判定**。
 *
 * 三处各写一份 `includes` 链的话，同一个账号在两处判成不同档位是迟早的事。
 * 取值是子串匹配而非全等：上游写法不统一（`pro`/`chatgpt_pro` 都见过）。
 */
export type PlanKey = 'enterprise' | 'team' | 'pro' | 'plus' | 'free' | 'unknown'

export function planKey(plan: string | null): PlanKey {
  const t = (plan ?? '').toLowerCase()
  if (t.includes('enterprise')) return 'enterprise'
  if (t.includes('team') || t.includes('business')) return 'team'
  if (t.includes('pro')) return 'pro'
  if (t.includes('plus')) return 'plus'
  if (t.includes('free')) return 'free'
  return 'unknown'
}

/** 档位排序权重：贵的在前。与 [`planKey`] 同源，别再写第二份顺序表。 */
const PLAN_RANK: Record<PlanKey, number> = {
  enterprise: 5, team: 4, pro: 3, plus: 2, free: 1, unknown: 0,
}

export function planLabel(plan: string | null, language: Language): string {
  switch (planKey(plan)) {
    case 'enterprise': return 'Enterprise'
    case 'team': return 'Team'
    case 'pro': return 'Pro'
    case 'plus': return 'Plus'
    case 'free': return 'Free'
    default: return localize(language, '未知', 'Unknown')
  }
}

/**
 * 档位配色。**刻意避开绿 / 黄 / 红**：那三色在卡片上紧挨着状态徽章，而状态徽章的绿
 * 就是「运行正常」——同色不同义最容易读错，一个 Pro 的绿标看着就像又一枚状态。
 *
 * 「Pro 是蓝的」是业界通行写法；Team / Enterprise 要腾到紫族才排得出高低，那两档用的是
 * 本文件专用的 `--plan` / `--plan-high`（定义在 index.css），不参与状态语义。
 */
export function planBadgeVariant(plan: string | null): BadgeProps['variant'] {
  switch (planKey(plan)) {
    case 'enterprise':
      return 'planHigh'
    case 'team':
      return 'plan'
    case 'pro':
      return 'info'
    case 'plus':
      return 'secondary'
    default:
      return 'outline'
  }
}

function sortValue(key: SortKey, cred: Credential, now: number): number | string {
  const evaluation = evaluateCredential(cred, now)
  switch (key) {
    case 'status': return evaluation.status.rank
    case 'name': return displayCredentialLabel(cred.label)
    case 'plan': return PLAN_RANK[planKey(cred.plan_type)]
    case 'usagePrimary': return evaluation.quota.primary.percentage ?? -1
    case 'usageSecondary': return evaluation.quota.secondary.percentage ?? -1
    case 'rpm': return cred.rpm_limit
    case 'cost': return cred.stats?.cost_total_usd ?? 0
    case 'requests': return cred.stats?.request_total ?? 0
    // 与卡片页脚同一个口径：cached 是 input 的子集，不另加（见 CredentialStats 的注）。
    case 'tokens':
      return (cred.stats?.input_tokens_total ?? 0) + (cred.stats?.output_tokens_total ?? 0)
    case 'recent': return cred.stats?.last_used_at ?? 0
    case 'created': return cred.created_at
    case 'priority':
    default: return cred.priority
  }
}

/**
 * 按维度 + 方向排序（不改原数组）。
 *
 * 同值时一律按 id 升序兜底，保证顺序稳定——否则相同优先级的账号会在每次重新渲染时互相换位。
 */
export function sortCreds(
  list: Credential[],
  key: SortKey,
  dir: SortDir,
  now = currentUnixSeconds(),
  language: Language = 'zh-CN',
): Credential[] {
  const sign = dir === 'asc' ? 1 : -1
  const values = new Map(list.map((c) => [c.id, sortValue(key, c, now)]))
  return [...list].sort((a, b) => {
    const av = values.get(a.id)!
    const bv = values.get(b.id)!
    const compared = typeof av === 'string' && typeof bv === 'string'
      ? av.localeCompare(bv, language)
      : Number(av) - Number(bv)
    return sign * compared || a.id - b.id
  })
}

// ---------- 写操作 ----------

/**
 * 卡片视图与列表视图共用的写操作。各视图自行管理编辑态，这里只封装请求与失败提示，
 * 避免两处重复维护同一套 mutation。
 */
export function useCredentialActions(cred: Credential, onRenamed?: () => void) {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  const invalidate = () => qc.invalidateQueries({ queryKey: ['credentials'] })
  const failure = (title: string, error: unknown) => toastManager.add({
    title,
    description: extractError(error, language),
    type: 'error',
  })

  const rename = useMutation({
    mutationFn: (label: string) => setLabel(cred.id, label),
    onSuccess: () => { onRenamed?.(); invalidate() },
    onError: (e) => failure(t('重命名失败', 'Rename failed'), e),
  })
  const toggle = useMutation({
    mutationFn: (disabled: boolean) => setDisabled(cred.id, disabled),
    // 乐观更新：开关是高频操作，等一次往返才动会让人以为没点上，于是连点两下。
    onMutate: async (disabled) => {
      await qc.cancelQueries({ queryKey: ['credentials'] })
      const previous = qc.getQueryData<Credential[]>(['credentials'])
      qc.setQueryData<Credential[]>(['credentials'], (current) => current?.map((item) => (
        item.id === cred.id ? { ...item, disabled } : item
      )))
      return { previous }
    },
    onError: (e, _disabled, context) => {
      if (context?.previous) qc.setQueryData(['credentials'], context.previous)
      failure(t('操作失败', 'Operation failed'), e)
    },
    onSettled: () => invalidate(),
  })
  const prio = useMutation({
    mutationFn: (p: number) => setPriority(cred.id, p),
    onSuccess: invalidate,
    onError: (e) => failure(t('设置优先级失败', 'Failed to set priority'), e),
  })
  const rpmLimit = useMutation({
    mutationFn: (n: number) => setRpmLimit(cred.id, n),
    onSuccess: () => {
      toastManager.add({ title: t('已保存 RPM 上限', 'RPM limit saved'), type: 'success' })
      invalidate()
    },
    onError: (e) => failure(t('设置 RPM 上限失败', 'Failed to set the RPM limit'), e),
  })
  const proxy = useMutation({
    mutationFn: (url: string | null) => setProxy(cred.id, url),
    onSuccess: () => {
      toastManager.add({ title: t('已保存出站代理', 'Outbound proxy saved'), type: 'success' })
      invalidate()
    },
    onError: (e) => failure(t('设置出站代理失败', 'Failed to set the outbound proxy'), e),
  })
  const refresh = useMutation({
    mutationFn: () => refreshCredential(cred.id),
    onSuccess: () => { toastManager.add({ title: t('已刷新', 'Refreshed'), type: 'success' }); invalidate() },
    onError: (e) => failure(t('刷新失败', 'Refresh failed'), e),
  })
  const remove = useMutation({
    mutationFn: () => deleteCredential(cred.id),
    onSuccess: () => { toastManager.add({ title: t('已删除', 'Deleted'), type: 'success' }); invalidate() },
    onError: (e) => failure(t('删除失败', 'Delete failed'), e),
  })
  const cooldown = useMutation({
    mutationFn: () => clearCooldown(cred.id),
    onSuccess: () => { toastManager.add({ title: t('已解除冷却', 'Cooldown cleared'), type: 'success' }); invalidate() },
    onError: (e) => failure(t('解除冷却失败', 'Failed to clear cooldown'), e),
  })

  return { rename, toggle, prio, rpmLimit, proxy, refresh, remove, cooldown }
}

export type CredentialActions = ReturnType<typeof useCredentialActions>

/**
 * ⋯ 菜单内容（刷新 / 重命名 / 上限 / 代理 / 删除），卡片与列表共用。
 *
 * 删除只往外抛意图，确认框由调用方渲染在菜单之外——菜单一关，挂在它里面的弹窗会跟着
 * 卸载，确认框根本来不及显示。
 */
export function CredentialMenuContent({
  cred, actions, onRename, onRpmLimit, onProxy, onUsage, onTest, onRequestDelete,
}: {
  cred: Credential
  actions: CredentialActions
  onRename: () => void
  onRpmLimit: () => void
  onProxy: () => void
  onUsage: () => void
  onTest: () => void
  onRequestDelete: () => void
}) {
  const { t } = useI18n()
  const { refresh, cooldown } = actions
  return (
    <MenuPopup align="end">
      <MenuItem onClick={() => refresh.mutate()} disabled={refresh.isPending}>
        <RefreshCwIcon className={refresh.isPending ? 'animate-spin' : undefined} />
        {t('刷新 token', 'Refresh token')}
      </MenuItem>
      <MenuItem onClick={onTest}>
        <ActivityIcon />
        {t('连通性测试', 'Connectivity test')}
      </MenuItem>
      {cred.cooldown_secs > 0 && (
        <MenuItem onClick={() => cooldown.mutate()} disabled={cooldown.isPending}>
          <TimerOffIcon />
          {t('解除冷却', 'Clear cooldown')}
        </MenuItem>
      )}
      <MenuItem onClick={onRename}>
        <PencilIcon />
        {t('重命名', 'Rename')}
      </MenuItem>
      <MenuItem onClick={onRpmLimit}>
        <GaugeIcon />
        {t('RPM 上限', 'RPM limit')}
      </MenuItem>
      <MenuItem onClick={onProxy}>
        <GlobeIcon />
        {t('出站代理', 'Outbound proxy')}
      </MenuItem>
      <MenuItem onClick={onUsage}>
        <ScrollTextIcon />
        {t('用量明细', 'Usage details')}
      </MenuItem>
      <MenuSeparator />
      <MenuItem variant="destructive" onClick={onRequestDelete}>
        <Trash2Icon />
        {t('删除账号', 'Delete account')}
      </MenuItem>
    </MenuPopup>
  )
}

/**
 * 延迟挂载：`open` 第一次为真时才渲染 children，此后一直保留。
 *
 * 每张卡片都挂着几个弹窗，若全部随卡片一起渲染，账号一多首屏就要构造上百个隐藏的
 * 对话框树。保留（而不是关闭即卸载）是为了让关闭动画能播完。
 */
export function DeferredMount({ open, children }: { open: boolean; children: ReactNode }) {
  const mounted = useRef(false)
  if (open) mounted.current = true
  return mounted.current ? <>{children}</> : null
}

/** 删除确认框。删除是不可逆的（连带清掉该账号的用量历史），故要求二次确认。 */
export function DeleteCredentialDialog({
  cred, open, onOpenChange, onConfirm, pending,
}: {
  cred: Credential
  open: boolean
  onOpenChange: (open: boolean) => void
  onConfirm: () => void
  pending: boolean
}) {
  const { t } = useI18n()
  return (
    <AlertDialog open={open} onOpenChange={onOpenChange}>
      <AlertDialogPopup>
        <AlertDialogHeader>
          <AlertDialogTitle>{t('删除账号', 'Delete account')}</AlertDialogTitle>
          <AlertDialogDescription>
            {t(
              `确定要删除「${displayCredentialLabel(cred.label)}」吗？该账号的用量历史会一并清除，此操作不可撤销。`,
              `Delete "${displayCredentialLabel(cred.label)}"? Its usage history is removed as well. This cannot be undone.`,
            )}
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogClose>{t('取消', 'Cancel')}</AlertDialogClose>
          <Button variant="destructive" onClick={onConfirm} disabled={pending}>
            {pending && <Spinner />}
            {t('删除', 'Delete')}
          </Button>
        </AlertDialogFooter>
      </AlertDialogPopup>
    </AlertDialog>
  )
}

/**
 * 模型下拉的**兜底**清单，只在向上游取清单失败时用。
 *
 * 正常路径是 `GET /credentials/{id}/models`——上游现给的那一份，随上新/下线自动跟上
 * （见 `proxy::list_models`）。写死一份当主来源的话，它从写下那一刻就开始过期：缺新模型、
 * 留着已下线的，而用户拿它去测只会收到一串 400。
 *
 * 兜底存在的唯一理由是**下拉框绝不能变空**——那正是这个功能最初坏掉的样子。取值是
 * 2026-08-21 拿一个 ChatGPT Pro 账号逐个探过、确实回 200 的那几个。
 */
const FALLBACK_PROBE_MODELS = [
  'gpt-5.6-sol',
  'gpt-5.6-terra',
  'gpt-5.6-luna',
  'gpt-5.5',
  'gpt-5.4',
  'gpt-5.4-mini',
  'gpt-5.3-codex-spark',
] as const

/** 一条测试记录：同一个弹窗里连测多次时按时间倒序累积，方便横向比较不同模型。 */
interface ProbeEntry {
  /** 自增序号，仅用作列表 key。 */
  seq: number
  /** 本次请求的模型名（`result.model` 是上游回报的，可能不同）。 */
  model: string
  result: ProbeResult
}

/** 一次在途探测。session 用来丢弃关闭弹窗后才到达的旧结果。 */
interface ProbeRequest {
  model: string
  controller: AbortController
  session: number
}

/** 耗时：一秒以内给毫秒，再长给秒——`1420 ms` 读起来不如 `1.4 s`。 */
function formatLatency(ms: number): string {
  return ms < 1000 ? `${Math.round(ms)} ms` : `${(ms / 1000).toFixed(1)} s`
}

/**
 * 连通性测试弹窗：用**这一个**账号向上游发一条最小请求，测它能不能用某个模型。
 *
 * 卡片与列表共用。请求形态、代价与副作用见后端 `proxy::probe`——一句话：不选号、不占 RPM
 * 名额、不换号重试，但账号状态照真实流量的口径更新（429 打冷却、命中封号特征自动停用、
 * 通过则解除限流暂停），也会写一条用量流水，且真的花掉一点点订阅额度。
 *
 * **刻意不再只刷 token**：刷新只验证 refresh_token 与出站链路，答不了「这个号能不能用这个
 * 模型」——而额度耗尽、套餐不含某个模型、账号被限制到只剩几个模型，恰恰都只在真的打一条
 * 请求时才现形。想要那条不花额度的检查的话，⋯ 菜单里的「刷新 token」就是它。
 *
 * 结果列表在弹窗内累积（关掉即清空）：连测几个模型时，「codex-max 403 而 codex 200」这种
 * 对照只有并排看才成立，一次只留最后一条就得靠人脑记。
 */
export function ConnectivityTestDialog({
  cred, open, onOpenChange,
}: {
  cred: Credential
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const { t, language } = useI18n()
  const credentialLabel = displayCredentialLabel(cred.label, language)
  const qc = useQueryClient()
  const [model, setModel] = useState<string | null>(null)
  const [entries, setEntries] = useState<ProbeEntry[]>([])
  const seq = useRef(0)
  const session = useRef(0)
  const activeProbe = useRef<AbortController | null>(null)

  // 弹窗打开才去取（`enabled: open`）：账号一多，跟着卡片一起预取等于一次刷新打出几十条
  // 上游请求，而这份清单只有在真要测的时候才用得上。取一次就留着（清单一天也变不了几次）。
  const models = useQuery({
    queryKey: ['credential-models', cred.id],
    queryFn: () => listCredentialModels(cred.id),
    enabled: open,
    staleTime: 10 * 60 * 1000,
    retry: false,
  })

  // 只列 `visibility: 'list'` 的：`hide` 那些能用但会被上游解析成别的模型（实测
  // gpt-reserve / codex-auto-review 都变成 gpt-5.6-luna），列出来像是多了两个独立选项。
  // 后端已按上游的 priority 排好，这里不再重排。
  const fetched = models.data?.filter((m) => m.visibility !== 'hide').map((m) => m.slug) ?? []
  const items = fetched.length > 0 ? fetched : [...FALLBACK_PROBE_MODELS]
  // 选中项跟着清单走：清单还没到时 model 为 null，到了就默认选第一个（上游把最推荐的排在
  // 最前）。不写死初值——写死的那个可能压根不在这个号的清单里。
  const selected = model && items.includes(model) ? model : (items[0] ?? null)

  const probe = useMutation({
    mutationKey: ['credential-probe', cred.id],
    // 管理页即使被浏览器判为 offline，也应立即请求本机后端并得到明确失败，而不是静默 paused。
    networkMode: 'always',
    mutationFn: ({ model: requestModel, controller }: ProbeRequest) =>
      probeCredential(cred.id, requestModel, controller.signal),
    onSuccess: (result, request) => {
      if (request.session !== session.current) return
      const entrySeq = ++seq.current
      setEntries((prev) => [{ seq: entrySeq, model: request.model, result }, ...prev])
      // 测试是真实流量，账号状态照真实口径更新：可能刷新了过期 token（有效期变了）、可能
      // 停用了命中封号特征的号（ban_reason 变了）、也可能把限流暂停的号放回了池子——卡片
      // 得跟着变，别让弹窗一个说法、列表另一个说法。上游拒绝也走 onSuccess（接口恒 200
      // 带结果），所以这里就够了。
      qc.invalidateQueries({ queryKey: ['credentials'] })
    },
    // 这条是「请求没发出去」（账号已被删、管理密码失效等），与「上游拒绝」不同：
    // 后者是 200 + 一份带状态码的结果，会进上面的列表。
    onError: (e, request) => {
      if (request.session !== session.current || axios.isCancel(e)) return
      toastManager.add({
        title: t('测试失败', 'Test failed'),
        description: extractError(e, language),
        type: 'error',
      })
    },
    onSettled: (_result, _error, request) => {
      if (activeProbe.current === request.controller) activeProbe.current = null
    },
  })

  const submit = () => {
    const m = selected?.trim() ?? ''
    // mutation state 要到下一次 render 才更新；ref 同步挡住双击/连续回车造成的重复扣额度。
    if (!m || activeProbe.current) return
    const controller = new AbortController()
    activeProbe.current = controller
    probe.mutate({ model: m, controller, session: session.current })
  }

  const cancelProbe = () => {
    session.current += 1
    activeProbe.current?.abort()
    activeProbe.current = null
    probe.reset()
  }

  // 账号因筛选、分页或重新排序离开页面时，终止前端请求并丢弃旧结果，避免重开后继承 pending。
  useEffect(() => () => {
    session.current += 1
    activeProbe.current?.abort()
  }, [])

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next) {
          cancelProbe()
          seq.current = 0
          setEntries([])
        }
        onOpenChange(next)
      }}
    >
      <DialogPopup>
        <DialogHeader>
          <DialogTitle>{t('连通性测试', 'Connectivity test')}</DialogTitle>
          <DialogDescription>
            {t('使用「', 'Send a minimal request through "')}
            <span className="font-medium text-foreground [overflow-wrap:anywhere]">{credentialLabel}</span>
            {t('」发送一条最小请求，验证所选模型是否可用。', '" to verify that the selected model is available.')}
          </DialogDescription>
        </DialogHeader>
        <DialogPanel className="space-y-4">
          <Form className="space-y-4" onSubmit={(e) => { e.preventDefault(); submit() }}>
            <Field>
              <FieldLabel htmlFor={`probe-model-${cred.id}`}>{t('测试模型', 'Model to test')}</FieldLabel>
              <div className="flex w-full flex-col gap-2 sm:flex-row sm:items-center">
                <Combobox
                  items={items}
                  value={selected}
                  onValueChange={(value) => value && setModel(value)}
                  disabled={probe.isPending || models.isPending}
                >
                  <ComboboxTrigger id={`probe-model-${cred.id}`} className="min-w-0 flex-1">
                    <ComboboxValue
                      placeholder={models.isPending
                        ? t('正在取模型清单…', 'Loading models…')
                        : t('选择模型', 'Select a model')}
                    />
                  </ComboboxTrigger>
                  <ComboboxPopup
                    aria-label={t('选择测试模型', 'Select a model to test')}
                    inputPlaceholder={t('搜索模型', 'Search models')}
                    emptyText={t('没有匹配的模型', 'No matching models')}
                  >
                    {(item: string) => (
                      <ComboboxItem key={item} value={item}>{item}</ComboboxItem>
                    )}
                  </ComboboxPopup>
                </Combobox>
                <Button
                  type={probe.isPending ? 'button' : 'submit'}
                  variant={probe.isPending ? 'outline' : 'default'}
                  className="w-full sm:w-auto sm:shrink-0"
                  disabled={!probe.isPending && !selected}
                  onClick={probe.isPending ? cancelProbe : undefined}
                >
                  {probe.isPending ? <Spinner /> : <ActivityIcon />}
                  {probe.isPending ? t('取消测试', 'Cancel test') : t('开始测试', 'Start test')}
                </Button>
              </div>
              <FieldDescription>
                {models.isError
                  ? t(
                      `取模型清单失败（${extractError(models.error, language)}），下面是内置兜底清单，可能与该账号实际可用的模型不一致。`,
                      `Could not fetch the model list (${extractError(models.error, language)}); the built-in fallback list is shown and may not match what this account can actually use.`,
                    )
                  : t(
                      `清单来自上游（共 ${items.length} 个，随官方上新自动更新）。每次测试会消耗少量订阅额度，并计入该账号的请求数与花费；只想验证 token 与出站链路的话用「刷新 token」。`,
                      `The list comes from the upstream (${items.length} models, updated as OpenAI ships new ones). Each test uses a small amount of subscription quota and counts toward this account’s requests and cost; use "Refresh token" to check only the token and outbound route.`,
                    )}
              </FieldDescription>
            </Field>
          </Form>

          {entries.length === 0 ? (
            <Empty className="py-8">
              <EmptyHeader>
                <EmptyTitle className="text-base">{t('尚无测试结果', 'No test results yet')}</EmptyTitle>
                <EmptyDescription>
                  {t(
                    '选择模型并开始测试，结果会显示本次响应的额度读数或上游错误。',
                    'Select a model and start a test to see this response’s quota reading or the upstream error.',
                  )}
                </EmptyDescription>
              </EmptyHeader>
            </Empty>
          ) : (
            <ul className="space-y-2">
              {entries.map((e) => (
                <ProbeEntryRow key={e.seq} entry={e} />
              ))}
            </ul>
          )}
        </DialogPanel>
      </DialogPopup>
    </Dialog>
  )
}

/** 一条测试结果：成败徽章 + 模型名 + 状态码/耗时，失败时附上游错误原文。 */
function ProbeEntryRow({ entry }: { entry: ProbeEntry }) {
  const { t, language } = useI18n()
  const { model, result } = entry
  const Icon = result.ok ? CircleCheckIcon : CircleXIcon
  return (
    <li>
      <Alert variant={result.ok ? 'success' : 'error'}>
        <Icon aria-hidden />
        <AlertTitle className="flex min-w-0 flex-wrap items-center gap-2">
          <span className="min-w-0 [overflow-wrap:anywhere]" title={model}>{model}</span>
          <Badge variant={result.ok ? 'success' : 'error'} size="sm">
            {result.status > 0 ? `HTTP ${result.status}` : t('未送达上游', 'Not sent upstream')}
          </Badge>
          <span className="font-normal text-muted-foreground">{formatLatency(result.latency_ms)}</span>
          {/* 上游把别名解析成了别的版本：那才是「这个模型名到底指向什么」的答案。 */}
          {result.model && result.model !== model && (
            <span
              className="min-w-0 font-normal text-muted-foreground [overflow-wrap:anywhere]"
              title={t(`上游实际使用的模型：${result.model}`, `Model actually used upstream: ${result.model}`)}
            >
              → {result.model}
            </span>
          )}
        </AlertTitle>
        <AlertDescription>
          {result.error && (
            <p className="break-words">
              {result.error_type && (
                <span className="mr-1 text-destructive-foreground">{result.error_type}</span>
              )}
              {localizeBackendMessage(result.error, language)}
            </p>
          )}
          {result.quota && <ProbeQuotaLine quota={result.quota} retryAfterSecs={result.retry_after_secs} />}
        </AlertDescription>
      </Alert>
    </li>
  )
}

/**
 * 本次响应带回的额度：两个窗口各自的使用率与重置时刻，429 时另标出上游要求的等待时长。
 *
 * 这是**本次测试响应**里上游直接返回的说法；测试完成后也会写用量流水并刷新账号列表，
 * 但这里保留逐次结果，方便对照不同模型的状态与等待时间。
 */
function ProbeQuotaLine({
  quota, retryAfterSecs,
}: {
  quota: Quota
  retryAfterSecs: number | null
}) {
  const { t, language } = useI18n()

  const win = (
    key: 'primary' | 'secondary',
    usedPct: number | null,
    windowMinutes: number | null,
    resetRaw: string | null,
  ) => {
    const resetAt = parseResetAt(resetRaw)
    // 判据与卡片上的 evaluateWindow 一致：**零长度的窗口不是窗口**。实测 Pro 账号只有
    // primary，secondary 那组头回的是「0% / 0 分钟 / 空重置时刻」，按「使用率非空即算」
    // 判会画出一条 0% 的读数，读起来像「还有一整个窗口没用」，与事实正好相反。
    if ((windowMinutes ?? 0) <= 0 && resetAt == null && (usedPct ?? 0) <= 0) return null
    const label = quotaWindowLabel({ key, windowMinutes }, language)
    const pct = quotaPercentage(usedPct)
    return (
      <span
        key={key}
        className="tnum"
        title={resetAt != null
          ? t(
              `${label} 窗口 ${formatFullTime(resetAt, language)} 重置`,
              `${label} window resets at ${formatFullTime(resetAt, language)}`,
            )
          : undefined}
      >
        {label}{' '}
        {pct == null ? '—' : <span className={cn('font-medium', quotaToneClass(usedPct))}>{pct}%</span>}
        {resetAt != null && t(
          ` · ${formatClockTime(resetAt, language)} 重置`,
          ` · resets ${formatClockTime(resetAt, language)}`,
        )}
      </span>
    )
  }

  return (
    <div className="mt-1 flex flex-wrap items-center gap-x-2.5 gap-y-1 text-xs text-muted-foreground">
      {win('primary', quota.primary_used_pct, quota.primary_window_minutes, quota.primary_reset_at)}
      {win('secondary', quota.secondary_used_pct, quota.secondary_window_minutes, quota.secondary_reset_at)}
      {/* 429 才有。它是上游对**这次**拒绝给出的等待时间，比窗口重置时刻更直接。 */}
      {retryAfterSecs != null && (
        <span
          className="text-destructive-foreground"
          title={t(
            `上游 retry-after：${retryAfterSecs} 秒`,
            `Upstream retry-after: ${retryAfterSecs} ${retryAfterSecs === 1 ? 'second' : 'seconds'}`,
          )}
        >
          {t('需等待', 'Wait')} {formatWait(retryAfterSecs, language)}
        </span>
      )}
      {/* 套餐额度满了但上游动用 credits 放行：不 429、请求照常成功，只有这里能看出在花钱。 */}
      {quota.credits_has_credits && !quota.credits_unlimited && (
        <span
          className="text-warning-foreground"
          title={t(
            '本次请求由额外 credits 放行：套餐包含的额度已用完，正按量计费',
            'This request was served by extra credits: the plan’s included quota is exhausted and pay-as-you-go rates now apply',
          )}
        >
          {t('使用 credits', 'On credits')}
          {quota.credits_balance != null && ` · ${quota.credits_balance}`}
        </span>
      )}
    </div>
  )
}

/** 使用率配色，阈值与卡片额度条一致（≥90% 红、≥70% 橙）。 */
function quotaToneClass(usedPct: number | null): string {
  const level = quotaLevel(usedPct)
  if (level === 'critical') return 'text-destructive-foreground'
  if (level === 'warning') return 'text-warning-foreground'
  return 'text-foreground/80'
}

/** 等待时长：分钟以内给秒，一天以内给小时，再长给天。 */
function formatWait(secs: number, language: Language): string {
  if (secs < 60) {
    return localize(language, `${secs} 秒`, `${secs} ${secs === 1 ? 'second' : 'seconds'}`)
  }
  if (secs < 3600) {
    const minutes = Math.round(secs / 60)
    return localize(language, `${minutes} 分钟`, `${minutes} ${minutes === 1 ? 'minute' : 'minutes'}`)
  }
  if (secs < 86400) {
    const hours = (secs / 3600).toFixed(1)
    return localize(language, `${hours} 小时`, `${hours} hours`)
  }
  const days = (secs / 86400).toFixed(1)
  return localize(language, `${days} 天`, `${days} days`)
}

/** 开关的 title：停用中的账号说「启用」，反之亦然。 */
export function switchTitle(cred: Credential, language: Language): string {
  return cred.disabled
    ? localize(language, '启用该账号', 'Enable this account')
    : localize(language, '停用该账号', 'Disable this account')
}

export function statusMeta(cred: Credential, language: Language): CredentialStatusMeta {
  return evaluateCredential(cred, currentUnixSeconds(), language).status
}

/** token 剩余有效期的呈现。过期不是故障——下一个请求会自动刷。 */
export function credentialExpiryMeta(cred: Credential, language: Language): {
  label: string
  tone: 'muted' | 'warning'
} {
  const secs = cred.expires_in_secs
  if (secs <= 0) {
    return {
      label: localize(language, 'token 已过期（将自动刷新）', 'Token expired (auto-refreshes)'),
      tone: 'muted',
    }
  }
  const minutes = Math.floor(secs / 60)
  if (minutes < 5) {
    return {
      label: localize(language, `token ${minutes} 分钟后过期`, `Token expires in ${minutes} min`),
      tone: 'warning',
    }
  }
  return {
    label: localize(language, `token 有效 ${minutes} 分钟`, `Token valid for ${minutes} min`),
    tone: 'muted',
  }
}

export { cn }
