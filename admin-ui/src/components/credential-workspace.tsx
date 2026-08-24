import { useCallback, useEffect, useMemo, useRef, useState, type RefObject } from 'react'
import {
  ArrowUpDownIcon,
  ChevronLeftIcon,
  ChevronRightIcon,
  DatabaseZapIcon,
  LayersIcon,
  LayoutGridIcon,
  ListFilterIcon,
  ListIcon,
  PlusIcon,
  ActivityIcon,
  RadioIcon,
  RefreshCwIcon,
  SearchIcon,
  ShieldCheckIcon,
  TimerResetIcon,
  TriangleAlertIcon,
  XIcon,
} from 'lucide-react'
import { useQuery } from '@tanstack/react-query'
import type { Credential } from '@/api/credentials'
import { getMetrics } from '@/api/metrics'
import { BatchActionsBar } from '@/components/batch-actions-bar'
import {
  CacheHitSparkline,
  aggregateCacheHitRate,
  cacheTotalsText,
} from '@/components/cache-hit-chart'
import {
  CacheHitTrendDialog,
  DEFAULT_CACHE_RANGE,
  useCacheSeries,
} from '@/components/cache-hit-trend-dialog'
import { CredentialCard } from '@/components/credential-card'
import { CredentialLoadingState } from '@/components/credential-loading'
import {
  SORTS,
  SORT_DIR_DEFAULT,
  evaluateCredential,
  planKey,
  quotaWindowTitle,
  sortCreds,
  type CredentialEvaluation,
  type PlanKey,
  type SortDir,
  type SortKey,
} from '@/components/credential-shared'
import { CredentialListHeader, CredentialRow } from '@/components/credential-row'
import { LiveTrafficMetric, OverviewMetric, OverviewMetricSkeleton } from '@/components/overview-metric'
import { Button, buttonVariants } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Card, CardHeader, CardTitle } from '@/components/ui/card'
import {
  Empty,
  EmptyContent,
  EmptyDescription,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
} from '@/components/ui/empty'
import { InputGroup, InputGroupAddon, InputGroupInput } from '@/components/ui/input-group'
import {
  Menu,
  MenuPopup,
  MenuRadioGroup,
  MenuRadioItem,
  MenuTrigger,
} from '@/components/ui/menu'
import {
  Pagination as CossPagination,
  PaginationContent,
  PaginationItem,
  PaginationLink,
} from '@/components/ui/pagination'
import { Select, SelectItem, SelectPopup, SelectTrigger, SelectValue } from '@/components/ui/select'
import { Skeleton } from '@/components/ui/skeleton'
import { Table, TableBody, TableCaption } from '@/components/ui/table'
import { ToggleGroup, ToggleGroupItem, ToggleGroupSeparator } from '@/components/ui/toggle-group'
import { Toolbar, ToolbarGroup, ToolbarSeparator } from '@/components/ui/toolbar'
import { useI18n, type Language } from '@/lib/i18n'
import { useMediaQuery } from '@/lib/media'
import { useDebounced } from '@/lib/use-debounced'
import { cn, displayCredentialLabel, extractError, formatPercent } from '@/lib/utils'

export type CredentialFilterKey =
  | 'all'
  | 'schedulable'
  | 'attention'
  | 'enabled'
  | 'disabled'
  | 'abnormal'
  | 'nearLimit'
  | 'cooldown'
  | 'paused'
  | 'proxied'

/**
 * 套餐筛选与上面那组状态筛选**是两个维度，各自独立**：一个说「这个号现在怎么样」，另一个说
 * 「这个号是什么档位」。合进同一个单选列表的话，「Pro 里有哪些需要处理」这种最常问的问题就
 * 提不出来——选了 Pro 就丢掉了状态条件。故单开一列，两个菜单同时生效（取交集）。
 */
export type CredentialTierFilterKey = 'all' | PlanKey

export type CredentialViewMode = 'card' | 'list'

/**
 * 每页条数用整数档（10 / 20 / 50）——报数用的数字，读起来就该是圆的。
 *
 * 曾经改成 12 / 24 / 48：那时卡片网格最多三列，每页 10 个会排成「3+3+3+1」，最后一行右边空
 * 两格，像加载失败或数据缺了一块，所以让条数被列数整除。**卡片改成最多两列之后这个理由就没了**
 * ——10 / 20 / 50 都能被 1 和 2 整除，两种列数下最后一行都是满的。
 *
 * 真要再放开到三列，宁可回来动这里，也**不要**把最后一行的卡片拉宽填满：那会让同一页里的卡片
 * 不一样大，卡片之间就没法照着同一个位置比读数了。表格不在乎整除，这三个数对它只是行数。
 */
export const CREDENTIAL_PAGE_SIZES = [10, 20, 50] as const
export type CredentialPageSize = (typeof CREDENTIAL_PAGE_SIZES)[number]

const PAGE_SIZE_ITEMS = CREDENTIAL_PAGE_SIZES.map((size) => ({
  size,
  value: String(size),
}))

type LocalizedLabel = readonly [chinese: string, english: string]

const FILTERS: {
  key: CredentialFilterKey
  label: LocalizedLabel
  match: (evaluation: CredentialEvaluation) => boolean
}[] = [
  { key: 'all', label: ['全部', 'All'], match: () => true },
  {
    key: 'schedulable',
    label: ['可调度', 'Schedulable'],
    match: (evaluation) => evaluation.schedulable,
  },
  {
    key: 'attention',
    label: ['需处理', 'Needs attention'],
    match: (evaluation) => evaluation.needsAttention,
  },
  { key: 'enabled', label: ['启用', 'Enabled'], match: ({ credential }) => !credential.disabled },
  { key: 'disabled', label: ['停用', 'Disabled'], match: ({ credential }) => credential.disabled },
  {
    key: 'abnormal',
    label: ['异常（已封禁）', 'Abnormal (banned)'],
    match: ({ credential }) => !!credential.ban_reason,
  },
  { key: 'nearLimit', label: ['用量风险', 'Usage risk'], match: (evaluation) => evaluation.quotaRisk },
  {
    key: 'cooldown',
    label: ['冷却中', 'Cooling down'],
    match: ({ credential }) => !credential.disabled && credential.cooldown_secs > 0,
  },
  {
    key: 'paused',
    label: ['限流暂停', 'Rate-limit paused'],
    match: ({ credential }) => credential.resume_at != null,
  },
  {
    key: 'proxied',
    label: ['走代理', 'Behind a proxy'],
    match: ({ credential }) => !!credential.proxy,
  },
]

/**
 * 档位选项按**从高到低**排，与 [`planKey`] 那张权重表口径一致——下拉里读到的顺序就是套餐的
 * 贵贱顺序，不必再去对照徽标颜色。档位名是上游的商品名，中英文一样，故不做翻译。
 */
const TIER_FILTERS: { key: CredentialTierFilterKey; label: LocalizedLabel }[] = [
  { key: 'all', label: ['全部套餐', 'All plans'] },
  { key: 'enterprise', label: ['Enterprise', 'Enterprise'] },
  { key: 'team', label: ['Team', 'Team'] },
  { key: 'pro', label: ['Pro', 'Pro'] },
  { key: 'plus', label: ['Plus', 'Plus'] },
  { key: 'free', label: ['Free', 'Free'] },
  { key: 'unknown', label: ['未知', 'Unknown'] },
]

const SORT_LABELS: Record<SortKey, LocalizedLabel> = {
  priority: ['优先级', 'Priority'],
  status: ['状态', 'Status'],
  name: ['名称', 'Name'],
  plan: ['套餐', 'Plan'],
  usagePrimary: ['主额度使用率', 'Primary usage'],
  usageSecondary: ['次额度使用率', 'Secondary usage'],
  rpm: ['RPM 上限', 'RPM limit'],
  cost: ['累计花费', 'Total cost'],
  requests: ['请求数', 'Requests'],
  tokens: ['token 数', 'Tokens'],
  recent: ['最近使用', 'Last used'],
  created: ['添加时间', 'Date added'],
}

export const CREDENTIAL_FILTER_KEYS = FILTERS.map((filter) => filter.key)
export const CREDENTIAL_TIER_FILTER_KEYS = TIER_FILTERS.map((item) => item.key)

/**
 * 表格视图的下限宽度（= Tailwind 的 xl）。
 *
 * 两件事共用它：首屏默认视图，以及窄屏的强制降级——十几列的表压到手机上每列只剩二十几个
 * 像素，那时不管偏好是什么都得回卡片。一个说「这么宽该给表格」而另一个说「这么窄得回卡片」，
 * 两个数不一样的话，中间那段窗口会来回跳。
 */
export const LIST_VIEW_MEDIA = '(min-width: 80rem)'

export const CREDENTIAL_VIEW_MODES = ['card', 'list'] as const

export function preferredInitialCredentialView(): CredentialViewMode {
  return typeof window !== 'undefined' && window.matchMedia(LIST_VIEW_MEDIA).matches
    ? 'list'
    : 'card'
}

/** 额度 reset 与相对时间都依赖当前时刻；30 秒 tick 与接口刷新节奏一致。 */
function useNowSeconds(): number {
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1000))

  useEffect(() => {
    const update = () => setNow(Math.floor(Date.now() / 1000))
    const onVisibilityChange = () => {
      if (!document.hidden) update()
    }
    const interval = window.setInterval(update, 30_000)
    window.addEventListener('focus', update)
    document.addEventListener('visibilitychange', onVisibilityChange)
    return () => {
      window.clearInterval(interval)
      window.removeEventListener('focus', update)
      document.removeEventListener('visibilitychange', onVisibilityChange)
    }
  }, [])

  return now
}

/**
 * `/` 与 ⌘K / Ctrl+K 聚焦搜索框——列表型控制台的通用约定。
 *
 * 已经在输入的时候不抢键（否则打不出 `/`）；弹层/对话框打开时也不抢，
 * 否则焦点会跳到被遮住的输入框上，模态里反而按不动。
 */
function useSearchHotkey(ref: RefObject<HTMLInputElement | null>): void {
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const slash = event.key === '/' && !event.metaKey && !event.ctrlKey && !event.altKey
      const commandK = (event.key === 'k' || event.key === 'K') && (event.metaKey || event.ctrlKey)
      if (!slash && !commandK) return
      const target = event.target as HTMLElement | null
      if (target?.isContentEditable) return
      if (target && /^(input|textarea|select)$/i.test(target.tagName)) return
      if (target?.closest('[role="dialog"], [role="alertdialog"], [role="menu"], [role="listbox"]')) return
      const input = ref.current
      if (!input) return
      event.preventDefault()
      input.focus()
      input.select()
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [ref])
}

/**
 * 搜索匹配的字段。除名称和 #id 外还收了套餐、组织类型与当前状态文案——
 * 「max」「team」「已封禁 / banned」这类词是排查时最先想敲进去的，只匹配名称会全部落空。
 * 状态用的是界面上那句本地化文案，所见即可搜。
 */
function matchQuery(evaluation: CredentialEvaluation, query: string, language: Language): boolean {
  const value = query.trim().toLowerCase()
  if (!value) return true
  const credential = evaluation.credential
  if (`#${credential.id}`.includes(value) || String(credential.id) === value) return true
  return [
    credential.label,
    displayCredentialLabel(credential.label, language),
    credential.email ?? '',
    credential.plan_type ?? '',
    credential.account_id_masked,
    evaluation.status.label,
  ].some((field) => field.toLowerCase().includes(value))
}

interface CredentialWorkspaceData {
  credentials?: Credential[]
  isLoading: boolean
  isError: boolean
  isRefetchError: boolean
  isFetching: boolean
  error?: unknown
}

interface CredentialWorkspaceState {
  query: string
  filter: CredentialFilterKey
  tier: CredentialTierFilterKey
  sort: SortKey
  dir: SortDir
  view: CredentialViewMode
  selected: Set<number>
  page: number
  pageSize: CredentialPageSize
}

interface CredentialWorkspaceActions {
  onQueryChange: (value: string) => void
  onFilterChange: (value: CredentialFilterKey) => void
  onTierChange: (value: CredentialTierFilterKey) => void
  onSortChange: (key: SortKey, dir: SortDir) => void
  onViewChange: (value: CredentialViewMode) => void
  onSelectedChange: (value: Set<number>) => void
  onPageChange: (value: number) => void
  onPageSizeChange: (value: CredentialPageSize) => void
  onRetry: () => void
  onAdd: () => void
}

export interface CredentialWorkspaceProps {
  data: CredentialWorkspaceData
  state: CredentialWorkspaceState
  actions: CredentialWorkspaceActions
}

function WorkspaceToolbarSkeleton() {
  return (
    <div
      className="grid w-full grid-cols-[minmax(0,1fr)_auto] items-stretch gap-2 sm:flex sm:flex-row sm:flex-wrap sm:items-center xl:justify-end"
      aria-hidden="true"
    >
      <Skeleton className="col-span-2 h-9 sm:h-8 sm:min-w-56 sm:flex-1 xl:max-w-64" />
      <div className="grid min-w-0 grid-cols-2 gap-1 sm:flex">
        <Skeleton className="h-9 min-w-0 sm:h-8 sm:w-24" />
        <Skeleton className="h-9 min-w-0 sm:h-8 sm:w-28" />
      </div>
      <Skeleton className="h-9 w-[4.5rem] justify-self-end sm:ml-auto sm:h-8 sm:w-16 xl:ml-0" />
    </div>
  )
}

/**
 * 账号页唯一的工作区组件。真实页面和离线预览共同使用这棵组件树，避免概览、工具栏、
 * 列表与分页在两处独立演进后产生视觉和交互差异。
 */
export function CredentialWorkspace({ data, state, actions }: CredentialWorkspaceProps) {
  const { language, locale, t } = useI18n()
  const {
    credentials,
    isLoading,
    isError,
    isRefetchError,
    isFetching,
    error,
  } = data
  const {
    query,
    filter,
    tier,
    sort,
    dir,
    view,
    selected,
    page,
    pageSize,
  } = state
  /**
   * **实际渲染哪种视图**：窄屏一律卡片，哪怕存下来的偏好是表格（见 [LIST_VIEW_MEDIA]）。
   *
   * 只改渲染、**不改存下来的偏好**：在桌面选了卡片的人，用手机看一眼再回桌面，还是卡片。
   * 视图切换那组按钮也因此只在 xl 以上出现——窄屏下它只有一个有效值，摆出来是个假选择。
   */
  const effectiveView: CredentialViewMode = useMediaQuery(LIST_VIEW_MEDIA) ? view : 'card'
  const pool = credentials ?? []
  const debouncedQuery = useDebounced(query)
  const searchRef = useRef<HTMLInputElement>(null)
  useSearchHotkey(searchRef)
  const now = useNowSeconds()
  // 实时指标单独轮询，10 秒一次：全局 RPM 与在途并发都是秒级变化的量，跟着账号列表那份
  // 30 秒的节奏走就成了「一直在看十几秒前的现场」。这个接口只有两条查询，拉得起。
  const metricsQuery = useQuery({ queryKey: ['metrics'], queryFn: getMetrics, refetchInterval: 10_000 })
  /**
   * 全池缓存命中率：**一段时间**（默认近 7 天）的，不是终身累计的。
   *
   * 终身那个数是个几乎不动的分数——跑了一个月之后，今天把粘性调好或调坏，它当天最多挪
   * 零点几个百分点，等于看不见。而这一格存在的意义恰恰是「我刚改的东西有没有用」，所以
   * 口径必须是窗口。终身那个留着，作为参照写在趋势对话框的脚注里。
   *
   * 按 token 加权（两个合计相除），不是各账号/各时段命中率的平均——后者会让一个只跑过两条
   * 小请求的号与主力号等权，一眼看去像是命中率崩了。
   */
  const cacheSeries = useCacheSeries(DEFAULT_CACHE_RANGE)
  const poolCache = aggregateCacheHitRate(cacheSeries.slots)
  const [cacheTrendOpen, setCacheTrendOpen] = useState(false)
  const numberFormatter = useMemo(() => new Intl.NumberFormat(locale), [locale])
  const formatNumber = (value: number) => numberFormatter.format(value)
  const filterItems = useMemo(
    () => FILTERS.map((item) => ({ ...item, label: t(...item.label) })),
    [t],
  )
  const tierItems = useMemo(
    () => TIER_FILTERS.map((item) => ({ ...item, label: t(...item.label) })),
    [t],
  )
  const activeFilterLabel = filterItems.find((item) => item.key === filter)?.label
    ?? t(...FILTERS[0].label)
  const activeTierLabel = tierItems.find((item) => item.key === tier)?.label
    ?? t(...TIER_FILTERS[0].label)
  const evaluatedPool = useMemo(
    () => pool.map((credential) => evaluateCredential(credential, now, language)),
    [pool, now, language],
  )
  /**
   * 列表那两个额度列该叫什么：拿**上游真的报了的窗口长度**算，而不是照抄「主/次额度」。
   *
   * 「主/次」是上游那两组头的名字，不是窗口的名字。实测（2026-08，一批 Pro 号）
   * `x-codex-primary-*` 报的是**周**窗口（10080 分钟），`x-codex-secondary-*` 整组是空的。
   * 所以那张表此前一列叫「主额度」实际是周额度，另一列叫「次额度」而永远显示「—」。
   *
   * 只有池子里所有报过这个窗口的账号**长度一致**时才敢用具体名字：不同套餐的窗口长度可以
   * 不一样，那时任何一个具体名字都会在别的行上是错的，退回通用名。
   */
  const quotaTitles = useMemo(() => {
    const titleOf = (key: 'primary' | 'secondary') => {
      const seen = new Set(
        evaluatedPool
          .map((evaluation) => evaluation.quota[key])
          .filter((w) => w.reported)
          .map((w) => w.windowMinutes),
      )
      if (seen.size !== 1) return null
      return quotaWindowTitle([...seen][0], language)
    }
    return { primary: titleOf('primary'), secondary: titleOf('secondary') }
  }, [evaluatedPool, language])
  /**
   * 那两个额度列在不在：**上游整池都没报过的窗口，那一列每行都是「—」**，占着 8rem 什么也
   * 没说，而这张表已经宽到要横向滚了。实测你这批 Pro 号上 `x-codex-secondary-*` 整组是空
   * 的，于是「次额度」那一列从来只有一排破折号。
   *
   * 一个号的快照都还没有时**两列都留着**：那时「没报」和「还没问过」分不开，收掉列等于让人
   * 以为这个界面不看额度。等真拿到快照、确认上游不报，再收。
   */
  const quotaColumns = useMemo(() => {
    const anySnapshot = evaluatedPool.some((evaluation) => evaluation.quota.hasSnapshot)
    const shown = (key: 'primary' | 'secondary') =>
      !anySnapshot || evaluatedPool.some((evaluation) => evaluation.quota[key].reported)
    return { primary: shown('primary'), secondary: shown('secondary') }
  }, [evaluatedPool])
  /**
   * 排序项。两个额度项的名字跟着列名走——列头写「周」而排序菜单写「主额度使用率」，那句
   * 「按主额度使用率排序」在表上指不到任何一列。
   */
  const sortItems = useMemo(
    () => SORTS.map(({ key }) => {
      const window = key === 'usagePrimary'
        ? quotaTitles.primary
        : key === 'usageSecondary'
          ? quotaTitles.secondary
          : null
      if (window) {
        return { key, label: t(`${window}额度使用率`, `${window} usage`) }
      }
      return { key, label: t(...SORT_LABELS[key]) }
    }),
    [t, quotaTitles],
  )
  const activeSortLabel = sortItems.find((item) => item.key === sort)?.label
    ?? t(...SORT_LABELS.priority)

  const sorted = useMemo(() => {
    const match = FILTERS.find((item) => item.key === filter)?.match ?? (() => true)
    return sortCreds(
      evaluatedPool
        .filter((evaluation) => (
          match(evaluation)
          && (tier === 'all' || planKey(evaluation.credential.plan_type) === tier)
          && matchQuery(evaluation, debouncedQuery, language)
        ))
        .map((evaluation) => evaluation.credential),
      sort,
      dir,
      now,
      language,
    )
  }, [evaluatedPool, sort, dir, filter, tier, debouncedQuery, now, language])

  const metrics = useMemo(() => {
    const filterCounts: Record<CredentialFilterKey, number> = {
      all: 0,
      schedulable: 0,
      attention: 0,
      enabled: 0,
      disabled: 0,
      abnormal: 0,
      nearLimit: 0,
      cooldown: 0,
      paused: 0,
      proxied: 0,
    }
    // 与状态筛选那份一样，按**整池**统计而不是按当前可见的那一屏：下拉里的数字要回答
    // 「切过去能看到几个」，跟着当前筛选走的话每选一次数字就变一次，等于没有参考价值。
    const tierCounts: Record<CredentialTierFilterKey, number> = {
      all: 0,
      enterprise: 0,
      team: 0,
      pro: 0,
      plus: 0,
      free: 0,
      unknown: 0,
    }
    let nearLimitCount = 0
    let pausedCount = 0
    // 「需处理」那行小字的两项。**按状态种类数**，而不是复用旁边那几个计数器：
    // `abnormal` 数的是「带封禁原因」（含已被限流盖过去的），`nearLimit` 数的是额度过线
    // （也可能被限流/停用盖过去），拿它们去拆 `attention` 必然对不上——屏幕上就是
    // 「需处理 2 · 1 异常 · 2 将满」。只有从同一个 status.kind 上数出来的才加得起来。
    const attentionKinds = { banned: 0, nearLimit: 0 }

    for (const evaluation of evaluatedPool) {
      const credential = evaluation.credential
      filterCounts.all += 1
      tierCounts.all += 1
      tierCounts[planKey(credential.plan_type)] += 1
      if (evaluation.schedulable) filterCounts.schedulable += 1
      if (evaluation.needsAttention) {
        filterCounts.attention += 1
        if (evaluation.status.kind === 'banned') attentionKinds.banned += 1
        else if (evaluation.status.kind === 'near-limit') attentionKinds.nearLimit += 1
      }
      if (credential.disabled) filterCounts.disabled += 1
      else filterCounts.enabled += 1
      if (credential.ban_reason) filterCounts.abnormal += 1
      if (evaluation.quotaRisk) filterCounts.nearLimit += 1
      // 口径必须与上面 'cooldown' 那条筛选完全一致，否则芯片上的计数和点开后的条数对不上。
      if (!credential.disabled && credential.cooldown_secs > 0) filterCounts.cooldown += 1
      if (credential.resume_at != null) filterCounts.paused += 1
      if (credential.proxy) filterCounts.proxied += 1
      if (evaluation.nearLimit) nearLimitCount += 1
      if (credential.resume_at != null) pausedCount += 1
    }

    return {
      filterCounts,
      tierCounts,
      attentionKinds,
      nearLimitCount,
      pausedCount,
    }
  }, [evaluatedPool])

  const count = pool.length
  const total = sorted.length
  const schedulableCount = metrics.filterCounts.schedulable
  const attentionCount = metrics.filterCounts.attention
  const quotaRiskCount = metrics.filterCounts.nearLimit
  const pausedCount = metrics.pausedCount
  /**
   * 两个筛选按钮**静止时的**文案。
   *
   * 状态那个筛选的第一项在菜单里叫「全部」——放在一列状态里读得通，但搬到按钮上就成了
   * 「什么的全部」，尤其旁边那个按钮写着「全部套餐」。所以按钮上补两个字，菜单里不动。
   *
   * 选中之后按钮改成「那一项的名字 + 命中数」：菜单里每项都带数，而按钮不带的话，筛完得
   * 去翻底部分页那行才知道筛出了几个。未筛选时不带数——那个数就是右上角「24 个账号」。
   */
  const filterTriggerLabel = filter === 'all' ? t('全部状态', 'All statuses') : activeFilterLabel
  const filterTriggerCount = filter === 'all' ? null : metrics.filterCounts[filter]
  const tierTriggerCount = tier === 'all' ? null : metrics.tierCounts[tier]
  const filtering = filter !== 'all' || tier !== 'all' || debouncedQuery.trim() !== ''
  const pageCount = Math.max(1, Math.ceil(total / pageSize))
  const current = Math.min(page, pageCount)
  const pageItems = sorted.slice((current - 1) * pageSize, current * pageSize)
  /**
   * 「需处理」下面那行小字：把这个数**拆成它自己的组成部分**。
   *
   * 只列 `attention: true` 的那两类（异常、将满，见 credential-shared 的 statusFromQuota）。
   * 冷却与限流暂停曾经也列在这里，但它们 `attention: false`——不需要人管，到点自己好。
   * 于是屏幕上出现过「需处理 0 · 4 限流暂停」：小字在数一堆没被上面那个数计进去的东西，
   * 读起来像我们算错了。限流暂停自己有一格，冷却在卡片上看得到。
   *
   * 「没有额外 credits」曾经也算一项，但那是普通订阅的常态而不是风险（见 CreditsState），
   * 于是这行小字里每个账号都占一条，把真正将满的那几个淹掉了。
   */
  const attentionStatus = [
    metrics.attentionKinds.banned > 0
      ? t(
          `${formatNumber(metrics.attentionKinds.banned)} 异常`,
          `${formatNumber(metrics.attentionKinds.banned)} banned`,
        )
      : '',
    metrics.attentionKinds.nearLimit > 0
      ? t(
          `${formatNumber(metrics.attentionKinds.nearLimit)} 将满`,
          `${formatNumber(metrics.attentionKinds.nearLimit)} near limit`,
        )
      : '',
  ].filter(Boolean).join(' · ') || undefined

  const clearSelection = () => actions.onSelectedChange(new Set())
  /**
   * 一次把搜索 + 状态 + 套餐全清掉。
   *
   * 三处各有各的清除入口（搜索框里的 ✕、两个菜单里的「全部」），但「我想回到全部账号」是
   * 一个意图，让人点三次是把内部结构摊给用户看。空结果那一屏本来就有这个按钮，工具栏里
   * 反而没有——而人是在工具栏里筛的。
   */
  const clearFilters = () => {
    actions.onQueryChange('')
    actions.onFilterChange('all')
    actions.onTierChange('all')
    actions.onPageChange(1)
    clearSelection()
  }
  const changeQuery = (value: string) => {
    actions.onQueryChange(value)
    actions.onPageChange(1)
    clearSelection()
  }
  const changeFilter = (value: CredentialFilterKey) => {
    actions.onFilterChange(value)
    actions.onPageChange(1)
    clearSelection()
  }
  const changeTier = (value: CredentialTierFilterKey) => {
    actions.onTierChange(value)
    actions.onPageChange(1)
    clearSelection()
  }
  const changeSort = (key: SortKey) => {
    actions.onSortChange(
      key,
      key === sort ? (dir === 'asc' ? 'desc' : 'asc') : SORT_DIR_DEFAULT[key],
    )
    actions.onPageChange(1)
  }
  // 勾选回调必须在多次渲染之间保持同一个引用，否则 memo 过的卡片/行每次都要重渲染
  // （搜索框每敲一个字就是一轮）。改成收 id 的形式，就不用为每张卡片现做一个闭包；
  // 最新的 selected 与 setter 走 ref 读取，避免闭包读到上一轮的集合把别人的勾选覆盖掉。
  const selectedRef = useRef(selected)
  selectedRef.current = selected
  const onSelectedChangeRef = useRef(actions.onSelectedChange)
  onSelectedChangeRef.current = actions.onSelectedChange
  const pageItemsRef = useRef(pageItems)
  pageItemsRef.current = pageItems
  /**
   * shift 范围选的锚点：**最后一次不按 shift 勾的那一行**。
   *
   * 桌面惯例（Finder / 资源管理器 / Gmail 一路）是连续 shift 点击都从同一个锚点重新展开，
   * 而不是以上一次 shift 点击处为界——所以只有普通点击才更新它。
   */
  const anchorRef = useRef<number | null>(null)
  /**
   * `extend`：按着 shift 点的。把锚点到这一行之间**整段**设成这一行的新状态，一次勾一屏。
   *
   * 范围按**当前这一页看到的顺序**算（`pageItems`），不是按 id 或者全池顺序——用户眼里的
   * 「这两行之间」就是排序筛选之后屏幕上的那一段。锚点已经不在本页（翻过页、改过筛选）时
   * 退回单选，免得勾中一堆看不见的行。
   *
   * 语义是**加法**：范围之外已经勾上的不会被清掉（复选框列表的通行做法，Gmail / GitHub /
   * Jira 都是这样；Finder 那种「整份选择就是这一段」适合单选高亮，不适合一格一个复选框）。
   * 所以先勾到远处、再 shift 点近处，远端那一截仍留着——那是用户自己勾的，不该被悄悄丢掉。
   *
   * 引用要稳定（卡片/行都是 memo 的），所以一律走 ref，不进依赖数组。
   */
  const toggleSelected = useCallback((id: number, checked: boolean, extend = false) => {
    const next = new Set(selectedRef.current)
    const items = pageItemsRef.current
    const to = items.findIndex((item) => item.id === id)
    const from = anchorRef.current == null
      ? -1
      : items.findIndex((item) => item.id === anchorRef.current)
    if (extend && from >= 0 && to >= 0) {
      const [start, end] = from <= to ? [from, to] : [to, from]
      for (let i = start; i <= end; i += 1) {
        if (checked) next.add(items[i].id)
        else next.delete(items[i].id)
      }
    } else {
      if (checked) next.add(id)
      else next.delete(id)
      anchorRef.current = id
    }
    onSelectedChangeRef.current(next)
  }, [])
  const selectMetric = (key: CredentialFilterKey) => changeFilter(filter === key ? 'all' : key)

  return (
    <div className="space-y-2 sm:space-y-3" data-slot="credential-workspace">
      <Card
        render={<section aria-labelledby="page-title" />}
        className="overflow-hidden rounded-xl"
      >
        <CardHeader
          className={cn(
            'grid gap-2 p-2.5 sm:p-3',
            (isLoading || count > 0) && 'xl:grid-cols-[auto_minmax(0,1fr)] xl:items-center',
          )}
        >
          <div className="flex min-w-0 items-center justify-between gap-3 xl:justify-start">
            <div className="flex min-w-0 items-center gap-2.5">
              <CardTitle
                render={<h1 id="page-title" />}
                className="min-w-0 text-lg leading-tight tracking-tight"
              >
                {t('账号池', 'Account pool')}
              </CardTitle>
              {!isLoading && (
                <Badge variant="secondary" size="sm" className="text-2xs">
                  {t(
                    `${formatNumber(count)} 个账号`,
                    `${formatNumber(count)} ${count === 1 ? 'account' : 'accounts'}`,
                  )}
                </Badge>
              )}
            </div>
            <div
              className="flex shrink-0 items-center gap-1.5 text-2xs text-muted-foreground"
              aria-live="polite"
              aria-atomic="true"
            >
              {isRefetchError ? (
                <>
                  <TriangleAlertIcon className="size-3.5 text-destructive-foreground" aria-hidden />
                  <button
                    type="button"
                    className="rounded-sm font-medium text-destructive-foreground underline-offset-2 hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/60"
                    onClick={actions.onRetry}
                  >
                    {t('刷新失败，重试', 'Refresh failed. Retry')}
                  </button>
                </>
              ) : (
                // 自动刷新指示器同时是手动刷新入口：等下一轮 30 秒才能确认操作结果，
                // 是这类常驻列表最常见的抱怨，而这块本来就在讲「数据有多新」。
                <button
                  type="button"
                  className="inline-flex items-center gap-1.5 rounded-sm px-1 py-0.5 transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/60 disabled:hover:text-muted-foreground"
                  onClick={actions.onRetry}
                  disabled={isLoading || isFetching}
                  title={isLoading
                    ? t('正在加载账号数据', 'Loading account data')
                    : t('每 30 秒自动刷新，点击立即刷新', 'Refreshes automatically every 30 seconds. Click to refresh now')}
                  aria-label={t('立即刷新账号数据', 'Refresh account data now')}
                >
                  <span className="flex size-3.5 shrink-0 items-center justify-center" aria-hidden>
                    {isLoading || isFetching ? (
                      <RefreshCwIcon className="size-3.5 animate-spin" />
                    ) : (
                      <span className="size-1.5 rounded-full bg-success" />
                    )}
                  </span>
                  <span className="min-w-14 text-left">
                    {isLoading ? t('正在加载', 'Loading') : t('30 秒刷新', '30s refresh')}
                  </span>
                </button>
              )}
            </div>
          </div>

          {isLoading ? (
            <WorkspaceToolbarSkeleton />
          ) : count > 0 && (
            <Toolbar className="grid w-full grid-cols-[minmax(0,1fr)_auto] items-stretch gap-2 border-0 bg-transparent p-0 sm:flex sm:flex-row sm:flex-wrap sm:items-center xl:justify-end">
              <InputGroup className="col-span-2 sm:min-w-56 sm:flex-1 xl:max-w-64">
                <InputGroupAddon><SearchIcon /></InputGroupAddon>
                <InputGroupInput
                  ref={searchRef}
                  value={query}
                  onChange={(event) => changeQuery(event.target.value)}
                  onKeyDown={(event) => {
                    // Esc 先清空、再退出输入框：清空和失焦是两个不同的意图，一次按键只做一件。
                    if (event.key !== 'Escape') return
                    event.preventDefault()
                    if (query) changeQuery('')
                    else event.currentTarget.blur()
                  }}
                  placeholder={t('搜索名称、#id、套餐或状态', 'Search name, #id, plan or status')}
                  aria-label={t('搜索账号', 'Search accounts')}
                />
                <InputGroupAddon align="inline-end">
                  {query ? (
                    <Button
                      size="icon-xs"
                      variant="ghost"
                      onClick={() => changeQuery('')}
                      aria-label={t('清除搜索', 'Clear search')}
                    >
                      <XIcon />
                    </Button>
                  ) : (
                    // 只在指针设备上提示：触屏没有物理按键，画个 kbd 只是噪声。
                    <kbd
                      className="pointer-events-none hidden rounded border bg-muted px-1 font-sans text-2xs text-muted-foreground pointer-fine:inline-block"
                      aria-hidden
                    >
                      /
                    </kbd>
                  )}
                </InputGroupAddon>
              </InputGroup>

              <ToolbarSeparator orientation="vertical" className="hidden sm:block" />
              <ToolbarGroup className="grid min-w-0 grid-cols-2 sm:flex sm:flex-wrap">
                <Menu>
                  <MenuTrigger
                    aria-label={t(`筛选：${activeFilterLabel}`, `Filter: ${activeFilterLabel}`)}
                    className={cn(
                      buttonVariants({ variant: filter === 'all' ? 'outline' : 'secondary' }),
                      'w-full min-w-0 justify-between max-sm:[&_svg]:hidden sm:w-auto',
                    )}
                  >
                    <ListFilterIcon />
                    <span className="min-w-0 truncate">
                      {filterTriggerLabel}
                    </span>
                    {filterTriggerCount != null && (
                      <span className="shrink-0 tnum text-xs text-muted-foreground">
                        {formatNumber(filterTriggerCount)}
                      </span>
                    )}
                  </MenuTrigger>
                  <MenuPopup align="end" className="w-52">
                    <MenuRadioGroup value={filter}>
                      {filterItems.map((item) => (
                        <MenuRadioItem key={item.key} value={item.key} onClick={() => changeFilter(item.key)}>
                          <span className="flex min-w-0 flex-1 items-center justify-between gap-4">
                            <span>{item.label}</span>
                            <span className="tnum text-xs text-muted-foreground">
                              {formatNumber(metrics.filterCounts[item.key])}
                            </span>
                          </span>
                        </MenuRadioItem>
                      ))}
                    </MenuRadioGroup>
                  </MenuPopup>
                </Menu>

                <Menu>
                  <MenuTrigger
                    aria-label={t(`套餐：${activeTierLabel}`, `Plan: ${activeTierLabel}`)}
                    className={cn(
                      buttonVariants({ variant: tier === 'all' ? 'outline' : 'secondary' }),
                      'w-full min-w-0 justify-between max-sm:[&_svg]:hidden sm:w-auto',
                    )}
                  >
                    <LayersIcon />
                    <span className="min-w-0 truncate">
                      {activeTierLabel}
                    </span>
                    {tierTriggerCount != null && (
                      <span className="shrink-0 tnum text-xs text-muted-foreground">
                        {formatNumber(tierTriggerCount)}
                      </span>
                    )}
                  </MenuTrigger>
                  <MenuPopup align="end" className="w-52">
                    <MenuRadioGroup value={tier}>
                      {tierItems.map((item) => (
                        <MenuRadioItem key={item.key} value={item.key} onClick={() => changeTier(item.key)}>
                          <span className="flex min-w-0 flex-1 items-center justify-between gap-4">
                            <span className="min-w-0 truncate">{item.label}</span>
                            <span className="tnum text-xs text-muted-foreground">
                              {formatNumber(metrics.tierCounts[item.key])}
                            </span>
                          </span>
                        </MenuRadioItem>
                      ))}
                    </MenuRadioGroup>
                  </MenuPopup>
                </Menu>

                {/* 有筛选生效时才出现，位置在两个筛选之后：它清的是筛选，不是排序。窄屏独占
                    一整行——它是这一组里唯一一个「动作」，挤在两个下拉旁边会被当成第三个筛选。 */}
                {filtering && (
                  <Button
                    variant="ghost"
                    onClick={clearFilters}
                    // 它连搜索一起清，但按钮上只写「清除筛选」——搜索框自己有个 ✕，写全了
                    // 反而长。完整语义放在悬浮/读屏文案里（同空结果那一屏那个按钮）。
                    title={t('清除筛选与搜索', 'Clear filters and search')}
                    aria-label={t('清除筛选与搜索', 'Clear filters and search')}
                    className="max-sm:col-span-2 max-sm:w-full"
                  >
                    <XIcon />
                    {t('清除筛选', 'Clear filters')}
                  </Button>
                )}

                {/* 排序与筛选之间加一条线：三个按钮排一起看着像三个同类，而它改的是顺序、
                    不是集合。窄屏本来就各占一行，不必再加线。 */}
                <ToolbarSeparator orientation="vertical" className="hidden sm:block" />

                <Menu>
                  <MenuTrigger
                    aria-label={t(
                      `排序：${activeSortLabel}，${dir === 'asc' ? '升序' : '降序'}`,
                      `Sort by ${activeSortLabel}, ${dir === 'asc' ? 'ascending' : 'descending'}`,
                    )}
                    className={cn(
                      buttonVariants({ variant: 'outline' }),
                      'w-full min-w-0 justify-between max-sm:col-span-2 max-sm:[&_svg]:hidden sm:w-auto',
                    )}
                  >
                    <ArrowUpDownIcon />
                    <span className="min-w-0 truncate max-[22rem]:hidden">
                      {activeSortLabel} {dir === 'asc' ? '↑' : '↓'}
                    </span>
                    <span className="hidden shrink-0 max-[22rem]:inline">
                      {t('排序', 'Sort')} {dir === 'asc' ? '↑' : '↓'}
                    </span>
                  </MenuTrigger>
                  <MenuPopup align="end" className="w-48">
                    <MenuRadioGroup value={sort}>
                      {sortItems.map((item) => (
                        <MenuRadioItem key={item.key} value={item.key} onClick={() => changeSort(item.key)}>
                          <span className="flex min-w-0 flex-1 items-center justify-between gap-4">
                            <span>{item.label}</span>
                            {sort === item.key && (
                              <span className="text-xs text-muted-foreground">
                                {dir === 'asc'
                                  ? t('升序', 'Ascending')
                                  : t('降序', 'Descending')}
                              </span>
                            )}
                          </span>
                        </MenuRadioItem>
                      ))}
                    </MenuRadioGroup>
                  </MenuPopup>
                </Menu>
              </ToolbarGroup>

              {/* 分隔线跟着它后面那组一起消失：只留一条竖线挂在工具栏末尾，看着像画坏了。 */}
              <ToolbarSeparator orientation="vertical" className="hidden xl:ml-0 xl:block" />
              <ToolbarGroup className="hidden self-center justify-end xl:flex">
                <ToggleGroup
                  value={[effectiveView]}
                  onValueChange={(values) => {
                    const next = values[values.length - 1]
                    if (next === 'card' || next === 'list') actions.onViewChange(next)
                  }}
                  variant="outline"
                  aria-label={t('账号视图', 'Account view')}
                >
                  <ToggleGroupItem
                    value="card"
                    aria-label={t('卡片视图', 'Card view')}
                    title={t('卡片视图', 'Card view')}
                  >
                    <LayoutGridIcon />
                  </ToggleGroupItem>
                  <ToggleGroupSeparator />
                  <ToggleGroupItem
                    value="list"
                    aria-label={t('表格视图', 'Table view')}
                    title={t('表格视图', 'Table view')}
                  >
                    <ListIcon />
                  </ToggleGroupItem>
                </ToggleGroup>
              </ToolbarGroup>
            </Toolbar>
          )}
        </CardHeader>

        {isLoading ? (
          <section
            aria-label={t('正在加载账号池概览', 'Loading account pool overview')}
            className="grid grid-cols-2 border-t lg:grid-cols-6"
          >
            <OverviewMetricSkeleton className="border-r border-b lg:border-b-0" />
            <OverviewMetricSkeleton className="border-b lg:border-r lg:border-b-0" />
            <OverviewMetricSkeleton className="border-r border-b lg:border-b-0" />
            <OverviewMetricSkeleton className="border-b lg:border-r lg:border-b-0" />
            <OverviewMetricSkeleton className="col-span-2 border-b lg:col-span-1 lg:border-r lg:border-b-0" />
            <OverviewMetricSkeleton className="col-span-2 lg:col-span-1" />
          </section>
        ) : count > 0 && (
          <section
            aria-label={t('账号池概览', 'Account pool overview')}
            className="grid grid-cols-2 border-t lg:grid-cols-6"
          >
            <OverviewMetric
              className="border-r border-b lg:border-b-0"
              label={t('可调度账号', 'Schedulable accounts')}
              value={`${formatNumber(schedulableCount)}/${formatNumber(count)}`}
              // 不放小字：`20/24` 已经把「4 个不可用」说完了，再写一遍是拿值做减法。
              // 不可用的原因（异常/冷却/限流暂停/手动停用）在悬浮提示与卡片上。
              statusHint={t(
                `${formatNumber(count)} 个号里 ${formatNumber(schedulableCount)} 个现在能被调度；其余的是异常、冷却、限流暂停或手动停用。`,
                `${formatNumber(schedulableCount)} of ${formatNumber(count)} accounts can be scheduled right now; the rest are banned, cooling down, rate-limit paused, or manually disabled.`,
              )}
              icon={ShieldCheckIcon}
              tone={schedulableCount > 0 ? 'ok' : 'bad'}
              active={filter === 'schedulable'}
              onClick={() => selectMetric('schedulable')}
            />
            <OverviewMetric
              className="border-b lg:border-r lg:border-b-0"
              label={t('需处理', 'Needs attention')}
              value={formatNumber(attentionCount)}
              status={attentionStatus}
              icon={TriangleAlertIcon}
              // 红也按同一个口径：一个「带封禁原因但正被限流盖着」的号不该让这一格变红，
              // 而它的值里并不包含那个号。
              tone={metrics.attentionKinds.banned > 0 ? 'bad' : attentionCount > 0 ? 'warn' : 'neutral'}
              active={filter === 'attention'}
              onClick={() => selectMetric('attention')}
            />
            <OverviewMetric
              className="border-r border-b lg:border-b-0"
              label={t('用量风险', 'Usage risk')}
              value={formatNumber(quotaRiskCount)}
              // 这一格的小字曾经是「N 将满」——与上面那个数一字不差，纯粹占地方。
              // 「将满」是什么意思放进悬浮提示。
              statusHint={t(
                '额度已过警戒线（默认 90%）的账号数。这些号会被暂时排到候选末尾，等窗口重置。',
                'Accounts past the quota warning threshold (90% by default). They drop to the back of the rotation until their window resets.',
              )}
              icon={RadioIcon}
              tone={quotaRiskCount > 0 ? 'warn' : 'neutral'}
              active={filter === 'nearLimit'}
              onClick={() => selectMetric('nearLimit')}
            />
            <OverviewMetric
              className="border-b lg:border-r lg:border-b-0"
              label={t('限流暂停', 'Rate-limit paused')}
              value={formatNumber(pausedCount)}
              // 只在真有号被暂停时说话：那句「到点自动恢复」是看到这个数之后唯一想知道的事
              // （不用管它）。为 0 时的「全部在池中」不改变任何判断，是纯噪声。
              status={pausedCount > 0 ? t('到点自动恢复', 'Resume automatically') : undefined}
              icon={TimerResetIcon}
              tone={pausedCount > 0 ? 'warn' : 'neutral'}
              active={filter === 'paused'}
              onClick={() => selectMetric('paused')}
            />
            {/* 缓存命中率不来自账号列表，点开是趋势而不是筛选——它讲的是「转发出去的请求质量
                如何」，而那是随时间走的东西，一个当下的数字看不出「改动有没有用」。摆在实时
                流量左边：两格都是流量的属性，凑在一起读。
                窄屏各占一整行：这一格的 status 是一串 token 数，挤成半格就只剩省略号。 */}
            <OverviewMetric
              className="col-span-2 border-b lg:col-span-1 lg:border-r lg:border-b-0"
              label={t('缓存命中率 · 近 7 天', 'Cache hit rate · 7d')}
              value={formatPercent(poolCache.rate)}
              trend={
                poolCache.rate == null ? undefined : (
                  <CacheHitSparkline slots={cacheSeries.slots} className="shrink-0" />
                )
              }
              // 有数时不放 status：这一格塞不下第三样东西，两个 token 数会被截成「命…」。
              // 它们跟着 statusHint 进悬浮提示，也在点开的趋势里。
              status={poolCache.rate == null ? t('暂无用量', 'No usage yet') : undefined}
              statusHint={poolCache.rate == null
                ? undefined
                : t(
                    `${cacheTotalsText(poolCache.cachedTokens, poolCache.inputTokens, t)}（按 token 加权，不是各账号命中率的平均）。点开看趋势。`,
                    `${cacheTotalsText(poolCache.cachedTokens, poolCache.inputTokens, t)} (token-weighted, not an average of per-account rates). Click for the trend.`,
                  )}
              icon={DatabaseZapIcon}
              tone={poolCache.rate == null ? 'neutral' : poolCache.rate >= 0.5 ? 'ok' : 'warn'}
              onClick={() => setCacheTrendOpen(true)}
            />
            {/* 唯一一格不来自账号列表、也点不动的指标：它讲的是「此刻代理在干什么」，
                而不是「池子里有几个号处于某状态」。窄屏独占一整行，别把它挤成半格。 */}
            <LiveTrafficMetric
              className="col-span-2 lg:col-span-1"
              label={t('实时流量', 'Live traffic')}
              value={metricsQuery.data ? formatNumber(metricsQuery.data.rpm) : '—'}
              unit="RPM"
              detail={metricsQuery.data
                ? t(
                    `${formatNumber(metricsQuery.data.in_flight)} 在途`,
                    `${formatNumber(metricsQuery.data.in_flight)} in flight`,
                  )
                : t('读取中', 'Loading')}
              live={(metricsQuery.data?.in_flight ?? 0) > 0}
              hint={t(
                `全池实时流量：最近 ${metricsQuery.data?.window_secs ?? 60} 秒转发的请求总数（各账号 RPM 之和），以及此刻已进入转发、响应还没走完的在途请求数。每 10 秒刷新。`,
                `Live traffic across the pool: requests forwarded in the last ${metricsQuery.data?.window_secs ?? 60} seconds (the sum of every account's RPM), plus the requests in flight right now — accepted for forwarding but not finished responding. Refreshed every 10 seconds.`,
              )}
              icon={ActivityIcon}
            />
          </section>
        )}
      </Card>

      <section className="min-w-0" aria-labelledby="account-list-title">
        <h2 id="account-list-title" className="sr-only">{t('账号列表', 'Account list')}</h2>
        <p className="sr-only" aria-live="polite">
          {isLoading
            ? t('正在加载账号', 'Loading accounts')
            : filtering
            ? t(
                `筛选出 ${formatNumber(total)} 个，共 ${formatNumber(count)} 个账号`,
                `${formatNumber(total)} ${total === 1 ? 'match' : 'matches'} out of ${formatNumber(count)} ${count === 1 ? 'account' : 'accounts'}`,
              )
            : t(
                `共 ${formatNumber(count)} 个账号`,
                `${formatNumber(count)} ${count === 1 ? 'account' : 'accounts'} total`,
              )}
        </p>
        <div className="min-w-0 space-y-3 sm:space-y-4">

          {count > 0 && selected.size > 0 && (
            <div className="relative">
              <BatchActionsBar
                all={sorted}
                selected={selected}
                onSelectedChange={actions.onSelectedChange}
                onClear={clearSelection}
              />
            </div>
          )}

          {isLoading ? (
            <div className="relative">
              <CredentialLoadingState view={effectiveView} selectable count={pageSize} />
            </div>
          ) : isError && !credentials ? (
            <Card><ErrorState error={error} onRetry={actions.onRetry} /></Card>
          ) : count === 0 ? (
            <Card><EmptyState onAdd={actions.onAdd} /></Card>
          ) : total === 0 ? (
            <Card>
              <Empty>
                <EmptyHeader>
                  <EmptyMedia variant="icon"><SearchIcon /></EmptyMedia>
                  <EmptyTitle>{t('没有符合条件的账号', 'No matching accounts')}</EmptyTitle>
                  <EmptyDescription>
                    {t(
                      '尝试清除当前筛选条件或搜索关键字。',
                      'Try clearing the current filters or search terms.',
                    )}
                  </EmptyDescription>
                </EmptyHeader>
                <EmptyContent>
                  <Button
                    variant="outline"
                    onClick={() => {
                      actions.onQueryChange('')
                      changeFilter('all')
                      changeTier('all')
                    }}
                  >
                    {t('清除筛选与搜索', 'Clear filters and search')}
                  </Button>
                </EmptyContent>
              </Empty>
            </Card>
          ) : effectiveView === 'list' ? (
            <Table
              variant="card"
              // 最小宽度跟着**这一档真的会显示的那几列**收：少一列就少它自己那点宽度。写死一个数
              // 的话 table-auto 会把省下来的宽度摊回给其余列——那一列是收掉了，横向滚动条却还在。
              //
              // 下限跟着**真的会渲染的那几列**收：少一列就少它自己那点宽度。写死一个数的话
              // table-auto 会把省下来的宽度摊回给其余列——那一列是收掉了，横向滚动条却还在。
              //
              // 各列宽度之和 64.5rem（含行尾那列开关 w-14；请求 / Token 不在表上，见 [COL] 的
              // `cost`），下限给到 68.5rem，多出的 4rem 归账号名那列（唯一自适应的一列）。
              // 额度列每少一个再减 10rem（w-40，见 [QuotaMeter]）。
              //
              // 刻意低于画布净宽（76rem = 1216px，卡片视图也是这个数）：拿画布宽度当下限等于把
              // 横向滚动条焊死——窗口只要被竖向滚动条吃掉十几个像素就得左右拖。
              //
              // 不需要 `table-fixed` 与 `xl:` 前缀：这张表只在 ≥80rem 渲染（窄屏强制走卡片，
              // 见 [LIST_VIEW_MEDIA]），条件写了也永远为真。
              className={cn(
                'table-auto',
                quotaColumns.primary && quotaColumns.secondary
                  ? 'min-w-[68.5rem]'
                  : quotaColumns.primary || quotaColumns.secondary
                    ? 'min-w-[58.5rem]'
                    : 'min-w-[48.5rem]',
              )}
            >
              <TableCaption className="sr-only">{t('账号列表', 'Account list')}</TableCaption>
              <CredentialListHeader
                quotaTitles={quotaTitles}
                quotaColumns={quotaColumns}
                selectable
                sort={sort}
                dir={dir}
                onSortChange={changeSort}
                allSelected={sorted.length > 0 && sorted.every((item) => selected.has(item.id))}
                onSelectAll={(checked) => actions.onSelectedChange(
                  checked ? new Set(sorted.map((item) => item.id)) : new Set(),
                )}
              />
              <TableBody>
                {pageItems.map((item) => (
                  <CredentialRow
                    key={item.id}
                    cred={item}
                    now={now}
                    quotaColumns={quotaColumns}
                    quotaTitles={quotaTitles}
                    selectable
                    selected={selected.has(item.id)}
                    onSelectedChange={toggleSelected}
                  />
                ))}
              </TableBody>
            </Table>
          ) : (
            /* **最多两列**，写死列数而不是让 auto-fill 自己排。
               卡片只在窄屏出现（≥80rem 是表格，见 [LIST_VIEW_MEDIA]），那一档最宽也就 1279px，
               两列各约 600px 顶格。三列起每行要扫的东西太多，而卡片本身又高——一屏装不下一整行
               的结果是既没扫完也没比着。两列是这类内容卡片的常规上限。
               1 → 2 列的门槛取 52rem：那时净画布 784px，两列各 384px，**正好是卡片自己的
               `@sm/card` 断点**（Tailwind 的 `--container-sm` = 24rem）——卡片到这个宽度才
               展开成「头像 + 页脚单行 + 额度两列」，再窄就退成更高的堆叠版。所以门槛不是随手
               挑的：跨过去的那一刻，两张卡刚好都还是展开态。
               列数固定成 2 之后，每页条数（10 / 20 / 50，见 [CREDENTIAL_PAGE_SIZES]）都除得开，
               最后一行不会留空格。 */
            <ul className="relative grid list-none grid-cols-1 items-stretch gap-3 p-0 min-[52rem]:grid-cols-2 sm:gap-4">
              {pageItems.map((item) => (
                <CredentialCard
                  key={item.id}
                  cred={item}
                  now={now}
                  selectable
                  selected={selected.has(item.id)}
                  onSelectedChange={toggleSelected}
                />
              ))}
            </ul>
          )}

          {/* 门槛是「有数据」而不是「有多页」：每页条数选择器住在这条里，按 pageCount > 1 收掉的话，
              一旦挑了 50 而账号不足 50，整条连同选择器一起消失，人就被锁死在 50 上换不回来了。
              页码那半截自己在里面按 pageCount 收（见 [AccountPagination]）。 */}
          {!isLoading && total > 0 && (
            <div className="relative py-2">
              <AccountPagination
                total={total}
                page={current}
                pageCount={pageCount}
                pageSize={pageSize}
                onPageChange={actions.onPageChange}
                onPageSizeChange={(size) => {
                  actions.onPageSizeChange(size)
                  actions.onPageChange(1)
                }}
              />
            </div>
          )}
        </div>
      </section>

      {/* 趋势对话框挂在最外层：它讲的是全池，不属于上面任何一段。终身口径顺手传进去当参照。 */}
      <CacheHitTrendDialog
        open={cacheTrendOpen}
        onOpenChange={setCacheTrendOpen}
        metrics={metricsQuery.data}
      />
    </div>
  )
}

function AccountPagination({
  total,
  page,
  pageCount,
  pageSize,
  onPageChange,
  onPageSizeChange,
}: {
  total: number
  page: number
  pageCount: number
  pageSize: CredentialPageSize
  onPageChange: (page: number) => void
  onPageSizeChange: (pageSize: CredentialPageSize) => void
}) {
  const { locale, t } = useI18n()
  const numberFormatter = useMemo(() => new Intl.NumberFormat(locale), [locale])
  const formatNumber = (value: number) => numberFormatter.format(value)
  const pageSizeItems = useMemo(
    () => PAGE_SIZE_ITEMS.map(({ size, value }) => ({
      value,
      label: t(`${numberFormatter.format(size)} 个`, `${numberFormatter.format(size)} items`),
    })),
    [numberFormatter, t],
  )
  const from = (page - 1) * pageSize + 1
  const to = Math.min(page * pageSize, total)
  const start = Math.max(1, Math.min(page - 2, pageCount - 4))
  const pages = Array.from({ length: Math.min(5, pageCount) }, (_, index) => start + index)
  const navigate = (event: React.MouseEvent<HTMLAnchorElement>, next: number) => {
    event.preventDefault()
    if (next >= 1 && next <= pageCount) onPageChange(next)
  }

  return (
    <div className="grid grid-cols-[1fr_auto] items-center gap-3 text-xs text-muted-foreground md:grid-cols-[1fr_auto_1fr]">
      <span className="min-w-0">
        <span className="sm:hidden">
          <span className="tnum text-foreground">{formatNumber(from)}–{formatNumber(to)}</span>
          {' / '}
          <span className="tnum text-foreground">{formatNumber(total)}</span>
        </span>
        <span className="hidden sm:inline">
          {t('第 ', 'Showing ')}
          <span className="tnum text-foreground">{formatNumber(from)}–{formatNumber(to)}</span>
          {t(' 个，共 ', ' of ')}
          <span className="tnum text-foreground">{formatNumber(total)}</span>
          {t(' 个账号', ` ${total === 1 ? 'account' : 'accounts'}`)}
        </span>
      </span>
      {/* 只有一页时页码没有意义，收掉；计数与每页条数留着。 */}
      {pageCount > 1 && (
        <CossPagination className="col-span-2 row-start-2 justify-center md:col-span-1 md:col-start-2 md:row-start-1">
          <PaginationContent>
            <PaginationItem>
              <PaginationLink
                href="#"
                size="icon-sm"
                className={cn(page <= 1 && 'pointer-events-none opacity-50')}
                aria-disabled={page <= 1}
                aria-label={t('上一页', 'Previous page')}
                onClick={(event) => navigate(event, page - 1)}
              >
                <ChevronLeftIcon />
              </PaginationLink>
            </PaginationItem>
            {pages.map((item) => (
              <PaginationItem key={item} className="max-sm:hidden">
                <PaginationLink
                  href="#"
                  size="icon-sm"
                  isActive={item === page}
                  aria-label={t(
                    `第 ${formatNumber(item)} 页`,
                    `Page ${formatNumber(item)}`,
                  )}
                  onClick={(event) => navigate(event, item)}
                >
                  <span className="tnum">{formatNumber(item)}</span>
                </PaginationLink>
              </PaginationItem>
            ))}
            <PaginationItem className="sm:hidden">
              <span className="tnum px-2 text-foreground">
                {formatNumber(page)} / {formatNumber(pageCount)}
              </span>
            </PaginationItem>
            <PaginationItem>
              <PaginationLink
                href="#"
                size="icon-sm"
                className={cn(page >= pageCount && 'pointer-events-none opacity-50')}
                aria-disabled={page >= pageCount}
                aria-label={t('下一页', 'Next page')}
                onClick={(event) => navigate(event, page + 1)}
              >
                <ChevronRightIcon />
              </PaginationLink>
            </PaginationItem>
          </PaginationContent>
        </CossPagination>
      )}
      <div className="row-start-1 flex items-center gap-2 justify-self-end md:col-start-3">
        <span className="max-sm:sr-only">{t('每页', 'Per page')}</span>
        <Select
          items={pageSizeItems}
          value={String(pageSize)}
          onValueChange={(value) => {
            const next = Number(value)
            if (CREDENTIAL_PAGE_SIZES.includes(next as CredentialPageSize)) {
              onPageSizeChange(next as CredentialPageSize)
            }
          }}
        >
          <SelectTrigger
            aria-label={t('每页账号数', 'Accounts per page')}
            size="sm"
            className="min-w-20"
          >
            <SelectValue />
          </SelectTrigger>
          <SelectPopup align="end">
            {pageSizeItems.map((item) => (
              <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>
            ))}
          </SelectPopup>
        </Select>
      </div>
    </div>
  )
}

function EmptyState({ onAdd }: { onAdd: () => void }) {
  const { t } = useI18n()
  return (
    <Empty>
      <EmptyHeader>
        <EmptyMedia variant="icon"><PlusIcon /></EmptyMedia>
        <EmptyTitle>{t('建立第一个调度账号', 'Add your first schedulable account')}</EmptyTitle>
        <EmptyDescription>
          {t(
            '完成 Claude OAuth 授权后，账号会加入当前网关的调度池。',
            'After Claude OAuth authorization, the account joins this gateway’s scheduling pool.',
          )}
        </EmptyDescription>
      </EmptyHeader>
      <EmptyContent>
        <Button onClick={onAdd}>
          <PlusIcon />
          {t('添加第一个账号', 'Add first account')}
        </Button>
      </EmptyContent>
    </Empty>
  )
}

function ErrorState({ error, onRetry }: { error: unknown; onRetry: () => void }) {
  const { language, t } = useI18n()
  return (
    <Empty role="alert">
      <EmptyHeader>
        <EmptyMedia variant="icon"><TriangleAlertIcon /></EmptyMedia>
        <EmptyTitle>{t('暂时无法读取账号', 'Unable to load accounts')}</EmptyTitle>
        <EmptyDescription className="break-words">{extractError(error, language)}</EmptyDescription>
      </EmptyHeader>
      <EmptyContent>
        <Button variant="outline" onClick={onRetry}>
          <RefreshCwIcon />
          {t('重新加载', 'Reload')}
        </Button>
      </EmptyContent>
    </Empty>
  )
}
