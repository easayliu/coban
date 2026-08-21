import React from 'react'
import ReactDOM from 'react-dom/client'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { EllipsisVerticalIcon, PlusIcon } from 'lucide-react'
import { AddAccount } from '@/components/add-account'
import { AppFooter } from '@/components/app-footer'
import { LanguageSwitcher } from '@/components/language-switcher'
import { ThemeSwitcher } from '@/components/theme-switcher'
import {
  CREDENTIAL_PAGE_SIZES,
  CredentialWorkspace,
  type CredentialFilterKey,
  type CredentialPageSize,
  type CredentialTierFilterKey,
  type CredentialViewMode,
} from '@/components/credential-workspace'
import type { SortDir, SortKey } from '@/components/credential-shared'
import { LogoMark } from '@/components/logo-mark'
import { Button, buttonVariants } from '@/components/ui/button'
import { Menu, MenuItem, MenuPopup, MenuTrigger } from '@/components/ui/menu'
import { AnchoredToastProvider, ToastProvider } from '@/components/ui/toast'
import { TooltipProvider } from '@/components/ui/tooltip'
import { LanguageProvider, parseLanguage, useI18n } from '@/lib/i18n'
import { initTheme } from '@/lib/theme'
import type { Credential, CredentialStats, Quota, UsageLog, UsagePage } from '@/api/credentials'
import './index.css'

/**
 * 离线预览：不连后端，用一批手写的账号跑生产同一套 CredentialWorkspace，
 * 用来验收卡片层级、页脚排布、额度窗口的有/无/已重置三态，以及各状态徽章的配色。
 *
 * 打开方式：`pnpm dev` 后访问 `/preview.html`（`?lang=en` 切英文、`?view=list` 看表格）。
 * 它不进 index.html 的入口，也就不会被打进 dist——rust-embed 只拷 dist。
 */

initTheme()

const now = Math.floor(Date.now() / 1000)
const params = new URLSearchParams(window.location.search)
const previewLanguage = parseLanguage(params.get('lang'))

/** 上游那两组 `x-codex-*` 头解出来的样子。reset 是字符串，与后端逐字一致。 */
function quota(fields: Partial<Quota>): Quota {
  return {
    primary_used_pct: null,
    primary_window_minutes: null,
    primary_reset_at: null,
    secondary_used_pct: null,
    secondary_window_minutes: null,
    secondary_reset_at: null,
    credits_has_credits: null,
    credits_unlimited: null,
    credits_balance: null,
    ...fields,
  }
}

/**
 * 一条预览用的凭证。
 *
 * `stats` 收 `Partial`（而不是跟着 `Partial<Credential>` 走成「要么整份、要么没有」）：
 * 账本字段会随功能增加，每加一个就去补 10 处 mock 是纯粹的摩擦，而 mock 关心的从来只是
 * 其中一两项。
 */
function credential(
  fields: Partial<Omit<Credential, 'stats'>>
    & Pick<Credential, 'id' | 'label'>
    & { stats?: Partial<CredentialStats> },
): Credential {
  const { stats, ...rest } = fields
  return {
    email: null,
    plan_type: 'plus',
    account_id_masked: '…a1b2c3',
    priority: 1,
    disabled: false,
    rpm_limit: 0,
    rpm_limit_effective: 0,
    rpm: 0,
    ban_reason: null,
    resume_at: null,
    proxy: null,
    expires_in_secs: 3 * 3600,
    cooldown_secs: 0,
    created_at: now - 6 * 24 * 3600,
    updated_at: now - 120,
    ...rest,
    stats: {
      last_used_at: now - 300,
      cost_total_usd: 0,
      request_total: 0,
      input_tokens_total: 0,
      cached_tokens_total: 0,
      output_tokens_total: 0,
      snapshot_ts: null,
      quota: null,
      primary_window: null,
      secondary_window: null,
      ...stats,
    },
  }
}

// 已封禁：故意用一条长的上游错误，覆盖「原因只进悬浮提示、不再撑高卡片」这个回归场景。
const banned = credential({
  id: 1,
  label: 'burksupperclassmens946205@yahoo.com',
  email: 'burksupperclassmens946205@yahoo.com',
  plan_type: 'chatgpt_pro',
  account_id_masked: '…9f31d0',
  priority: 0,
  ban_reason: '[401] invalid_grant: The refresh token has been revoked; re-authorise this account.',
  expires_in_secs: 0,
  created_at: now - 7 * 3600,
  stats: {
    last_used_at: now - 4 * 3600,
    cost_total_usd: 52.36,
    request_total: 1842,
    primary_window: { requests: 412, tokens: 18_400_000, cost_usd: 27.48 },
    secondary_window: { requests: 1_804, tokens: 96_200_000, cost_usd: 141.06 },
    input_tokens_total: 24_800_000,
    cached_tokens_total: 19_400_000,
    output_tokens_total: 1_260_000,
    snapshot_ts: now - 4 * 3600,
    quota: quota({
      primary_used_pct: 100,
      primary_window_minutes: 300,
      primary_reset_at: String(now - 2 * 60),
      secondary_used_pct: 82,
      secondary_window_minutes: 10080,
      secondary_reset_at: String(now + 36 * 3600),
      credits_has_credits: false,
      credits_balance: 0,
    }),
  },
})

// 主额度将满（≥90%）：这一档要进「需处理」，状态徽章带悬浮提示，进度条转红。
const nearLimit = credential({
  id: 2,
  label: 'codex-primary-almost-full',
  plan_type: 'plus',
  account_id_masked: '…4c77ab',
  priority: 1,
  rpm: 12,
  stats: {
    last_used_at: now - 45,
    cost_total_usd: 18.42,
    request_total: 964,
    snapshot_ts: now - 45,
    quota: quota({
      primary_used_pct: 96.4,
      primary_window_minutes: 300,
      primary_reset_at: String(now + 42 * 60),
      secondary_used_pct: 61,
      secondary_window_minutes: 10080,
      secondary_reset_at: String(now + 3 * 24 * 3600),
      credits_has_credits: true,
      credits_balance: 1240,
    }),
  },
})

// 被上游限流暂停：到点自动恢复，不需要任何人处理，所以不进「需处理」。
const rateLimited = credential({
  id: 3,
  label: 'team-shared@example.com',
  email: 'team-shared@example.com',
  plan_type: 'team',
  account_id_masked: '…be0142',
  priority: 0,
  resume_at: now + 26 * 60,
  proxy: 'socks5h://user:pass@10.0.0.7:1080',
  rpm_limit: 120,
  rpm_limit_effective: 120,
  rpm: 100,
  stats: {
    last_used_at: now - 30,
    // 页脚最挤的那一档：三位数费用 + 带上限的 RPM（17 个字符），窄卡片必须排两行。
    cost_total_usd: 214.6,
    request_total: 12_480,
    primary_window: { requests: 5_900, tokens: 729_000_000, cost_usd: 470.54 },
    secondary_window: { requests: 12_400, tokens: 1_910_000_000, cost_usd: 1_204.9 },
    input_tokens_total: 181_000_000,
    cached_tokens_total: 152_000_000,
    output_tokens_total: 9_400_000,
    snapshot_ts: now - 30,
    quota: quota({
      primary_used_pct: 74,
      primary_window_minutes: 300,
      primary_reset_at: String(now + 88 * 60),
      secondary_used_pct: 93.2,
      secondary_window_minutes: 43200,
      secondary_reset_at: String(now + 11 * 24 * 3600),
      credits_unlimited: true,
    }),
  },
})

// 撞过 429 正在冷却：秒数在状态提示里走，卡片不为它单独加一行。
const cooling = credential({
  id: 4,
  label: 'enterprise-pool-02',
  plan_type: 'enterprise',
  account_id_masked: '…77e5c9',
  priority: 2,
  cooldown_secs: 725,
  rpm: 3,
  expires_in_secs: 2 * 60, // token 快过期：元信息行那枚黄色时钟只在这一档出现。
  stats: {
    last_used_at: now - 12,
    cost_total_usd: 9.04,
    request_total: 331,
    snapshot_ts: now - 12,
    quota: quota({
      primary_used_pct: 44,
      primary_window_minutes: 300,
      primary_reset_at: String(now + 2 * 3600),
      secondary_used_pct: 12,
      secondary_window_minutes: 10080,
      secondary_reset_at: String(now + 5 * 24 * 3600),
      credits_has_credits: true,
      credits_balance: 86_400,
    }),
  },
})

// 上游只报了 primary，压根没有 secondary（实测 Pro 号就这样）：卡片应收成单列，
// 不留半个空格——分两列却只填一格，看起来像另一半加载失败了。
const onlyPrimary = credential({
  id: 5,
  label: 'pro-single-window',
  plan_type: 'pro',
  account_id_masked: '…10b7f4',
  priority: 1,
  rpm: 9,
  created_at: now - 9 * 24 * 3600,
  stats: {
    last_used_at: now - 30,
    cost_total_usd: 3.41,
    request_total: 71,
    primary_window: { requests: 71, tokens: 412_000, cost_usd: 0.83 },
    input_tokens_total: 412_000,
    cached_tokens_total: 96_000,
    output_tokens_total: 38_000,
    snapshot_ts: now - 30,
    quota: quota({
      primary_used_pct: 44,
      primary_window_minutes: 300,
      primary_reset_at: String(now + 2 * 3600),
      // 零长度 + 空重置时刻的 secondary 不是窗口，不该被画成一条 0% 的进度条。
      secondary_used_pct: 0,
      secondary_window_minutes: 0,
      secondary_reset_at: null,
    }),
  },
})

// 窗口已经过了重置点：上游那份使用率作废，按 0% 画，倒计时整段不出现。
const windowReset = credential({
  id: 6,
  label: 'window-already-reset',
  plan_type: 'plus',
  account_id_masked: '…33c8de',
  priority: 3,
  stats: {
    last_used_at: now - 3 * 3600,
    cost_total_usd: 0.84,
    request_total: 27,
    // 快照本身很旧：卡片要把「更新于 3 小时前」标出来，否则这个 0% 会被当成现状。
    snapshot_ts: now - 3 * 3600,
    quota: quota({
      primary_used_pct: 88,
      primary_window_minutes: 300,
      primary_reset_at: String(now - 20 * 60),
      secondary_used_pct: 35,
      secondary_window_minutes: 10080,
      secondary_reset_at: String(now + 2 * 24 * 3600),
      credits_has_credits: false,
    }),
  },
})

// 手动停用：不参与调度，卡片整体压暗，其余信息照常可读。
const disabled = credential({
  id: 7,
  label: 'kept-for-later@example.com',
  email: 'kept-for-later@example.com',
  plan_type: 'free',
  account_id_masked: '…5ad901',
  priority: 4,
  disabled: true,
  rpm_limit: -1,
  rpm_limit_effective: 0,
  created_at: now - 40 * 24 * 3600,
  stats: {
    last_used_at: now - 9 * 24 * 3600,
    cost_total_usd: 1.07,
    request_total: 58,
    snapshot_ts: now - 9 * 24 * 3600,
    quota: quota({
      primary_used_pct: 6,
      primary_window_minutes: 300,
      primary_reset_at: String(now - 9 * 24 * 3600 + 300 * 60),
    }),
  },
})

// 刚加进来、还没转发过任何请求：没有快照，空态说的是「等等就有」，
// 与「有快照但上游没这个窗口」不是一回事。
const fresh = credential({
  id: 8,
  label: '未命名',
  plan_type: null,
  account_id_masked: '…c204e8',
  priority: 5,
  created_at: now - 90,
  updated_at: now - 90,
  stats: { last_used_at: null, cost_total_usd: 0, request_total: 0, snapshot_ts: null, quota: null },
})

// 有快照、但上游一个窗口都没报：再等也不会出现，文案必须和上面那条区分开。
const snapshotWithoutWindows = credential({
  id: 9,
  label: 'snapshot-without-windows',
  plan_type: 'plus',
  account_id_masked: '…8e77b1',
  priority: 5,
  stats: {
    last_used_at: now - 600,
    cost_total_usd: 0.12,
    request_total: 4,
    primary_window: { requests: 0, tokens: 0, cost_usd: 0 },
    input_tokens_total: 6_120,
    cached_tokens_total: 0,
    output_tokens_total: 940,
    snapshot_ts: now - 600,
    quota: quota({ credits_has_credits: true, credits_balance: 500 }),
  },
})

const previewCredentials: Credential[] = [
  banned,
  nearLimit,
  rateLimited,
  cooling,
  onlyPrimary,
  windowReset,
  disabled,
  fresh,
  snapshotWithoutWindows,
]

const queryClient = new QueryClient({
  defaultOptions: {
    // 预览页不连后端：所有查询都靠 setQueryData 预置，失败也不要一遍遍重试。
    queries: { retry: false, refetchOnWindowFocus: false, staleTime: Infinity },
  },
})

queryClient.setQueryData(['auth-state'], { configured: false, env_managed: false })
queryClient.setQueryData(['metrics'], {
  credentials_total: previewCredentials.length,
  credentials_enabled: previewCredentials.filter((c) => !c.disabled).length,
  rpm: 124,
  window_secs: 60,
  in_flight: 3,
  cost_total_usd: previewCredentials.reduce((sum, c) => sum + c.stats.cost_total_usd, 0),
  requests_total: previewCredentials.reduce((sum, c) => sum + c.stats.request_total, 0),
  input_tokens_total: previewCredentials.reduce((sum, c) => sum + c.stats.input_tokens_total, 0),
  cached_tokens_total: previewCredentials.reduce((sum, c) => sum + c.stats.cached_tokens_total, 0),
})

/**
 * 缓存命中率趋势的假数据：近 7 天逐小时。
 *
 * 刻意造出三种形态，因为图上这三样长得必须不一样：**有流量且高命中**（日常）、**有流量但
 * 命中掉下去**（第 4 天，粘性被打断的样子）、**整段静默**（第 2 天下半夜，图上该留空而不是
 * 画一根落到底的柱子）。
 */
const previewCacheSeries = (() => {
  const hourNow = Math.floor(Date.now() / 1000 / 3600) * 3600
  const points: { ts: number; input_tokens: number; cached_tokens: number }[] = []
  for (let h = 7 * 24 - 1; h >= 0; h--) {
    const ts = hourNow - h * 3600
    const day = Math.floor(h / 24)
    // 第 2 天的凌晨 0–8 点整段没有请求：后端不会回这些小时，这里也就不塞。
    if (day === 5 && h % 24 < 8) continue
    // 一天里的活跃时段之外流量稀薄，凌晨干脆没有。
    const hourOfDay = new Date(ts * 1000).getHours()
    if (hourOfDay < 8 && day !== 3) continue
    const input = 6_000 + ((h * 733) % 9_000)
    // 第 4 天命中率掉到六成上下（换过号，客户端手里的前缀对不上了）。
    const hitRate = day === 3 ? 0.58 + ((h % 5) * 0.01) : 0.93 + ((h % 4) * 0.015)
    points.push({ ts, input_tokens: input, cached_tokens: Math.round(input * hitRate) })
  }
  return { since: hourNow - 7 * 24 * 3600, bucket_secs: 3600, points }
})()

queryClient.setQueryData(['cache-series', 7 * 24], previewCacheSeries)
queryClient.setQueryData(['cache-series', 24], {
  ...previewCacheSeries,
  since: previewCacheSeries.since,
  points: previewCacheSeries.points.filter((p) => p.ts >= Math.floor(Date.now() / 1000) - 24 * 3600),
})
queryClient.setQueryData(['cache-series', 30 * 24], previewCacheSeries)

const previewUsageLogs: UsageLog[] = Array.from({ length: 12 }, (_, index) => ({
  id: 1200 - index,
  ts: now - index * 73,
  cred_id: nearLimit.id,
  cred_label: nearLimit.label,
  session_id: `sess_${String(index % 3).padStart(2, '0')}f2c1a9`,
  model: index % 3 === 0 ? 'gpt-5-codex' : 'gpt-5',
  path: '/backend-api/codex/responses',
  ua: 'codex_cli_rs/0.47.0 (Mac OS 15.3; arm64)',
  status: index === 5 ? 429 : index === 9 ? 500 : 200,
  input_tokens: index === 9 ? null : 12_480 + index * 913,
  // cached 是 input 的子集（约 93%，长会话的常态），不能比它大——否则命中率一律顶到 100%。
  cached_tokens: index === 9 ? null : 11_640 + index * 805,
  output_tokens: index === 9 ? null : 840 + index * 47,
  reasoning_tokens: index === 9 ? null : 320 + index * 29,
  total_tokens: index === 9 ? null : 32_040 + index * 1760,
  ttft_ms: index === 9 ? null : 480 + index * 63,
  total_ms: index === 9 ? null : 3_280 + index * 211,
  cost_usd: index === 9 ? null : 0.0184 + index * 0.0027,
}))
queryClient.setQueryData<UsagePage>(
  ['credential-usage', nearLimit.id, 0, 25],
  {
    total: 37,
    total_cost: 1.2846,
    total_input_tokens: previewUsageLogs.reduce((sum, l) => sum + (l.input_tokens ?? 0), 0),
    total_cached_tokens: previewUsageLogs.reduce((sum, l) => sum + (l.cached_tokens ?? 0), 0),
    anchor: previewUsageLogs[0]?.id ?? null,
    logs: previewUsageLogs,
  },
)

function PreviewWorkspace() {
  const [query, setQuery] = React.useState('')
  const [filter, setFilter] = React.useState<CredentialFilterKey>('all')
  const [tier, setTier] = React.useState<CredentialTierFilterKey>('all')
  const [sort, setSort] = React.useState<SortKey>('priority')
  const [dir, setDir] = React.useState<SortDir>('asc')
  const [view, setView] = React.useState<CredentialViewMode>(
    params.get('view') === 'list' ? 'list' : 'card',
  )
  const [selected, setSelected] = React.useState<Set<number>>(new Set())
  const [page, setPage] = React.useState(1)
  const [pageSize, setPageSize] = React.useState<CredentialPageSize>(CREDENTIAL_PAGE_SIZES[0])

  return (
    <CredentialWorkspace
      data={{
        credentials: previewCredentials,
        isLoading: params.get('state') === 'loading',
        isError: false,
        isRefetchError: false,
        isFetching: false,
      }}
      state={{ query, filter, tier, sort, dir, view, selected, page, pageSize }}
      actions={{
        onQueryChange: setQuery,
        onFilterChange: setFilter,
        onTierChange: setTier,
        onSortChange: (key, nextDir) => {
          setSort(key)
          setDir(nextDir)
        },
        onViewChange: setView,
        onSelectedChange: setSelected,
        onPageChange: setPage,
        onPageSizeChange: setPageSize,
        onRetry: () => undefined,
        onAdd: () => undefined,
      }}
    />
  )
}

function PreviewHeader({ onAdd }: { onAdd: () => void }) {
  const { t } = useI18n()

  React.useEffect(() => {
    document.title = t('coban · 界面预览', 'coban · UI Preview')
  }, [t])

  return (
    <header className="app-header sticky top-0 z-20 border-b bg-background/92 backdrop-blur-md">
      <div className="page-frame flex h-14 items-center justify-between gap-3 sm:h-16">
        <div className="flex min-w-0 items-center gap-2.5 sm:gap-3">
          <div className="brand-mark flex size-8 shrink-0 items-center justify-center rounded-lg">
            <LogoMark className="size-[1.125rem]" />
          </div>
          <div className="min-w-0">
            <div className="text-sm font-semibold leading-none tracking-tight">Coban</div>
            <div className="mt-1 hidden whitespace-nowrap text-xs text-muted-foreground sm:block">
              Codex Gateway
            </div>
          </div>
        </div>

        <div className="flex items-center gap-2 sm:hidden">
          <Button size="icon-lg" aria-label={t('添加账号', 'Add account')} onClick={onAdd}>
            <PlusIcon />
          </Button>
          <LanguageSwitcher compact />
          <ThemeSwitcher compact />
          <Menu>
            <MenuTrigger
              className={buttonVariants({ size: 'icon-lg', variant: 'outline' })}
              aria-label={t('更多操作', 'More actions')}
            >
              <EllipsisVerticalIcon />
            </MenuTrigger>
            <MenuPopup align="end">
              <MenuItem onClick={onAdd}>
                <PlusIcon />{t('添加账号', 'Add account')}
              </MenuItem>
            </MenuPopup>
          </Menu>
        </div>

        <div className="hidden items-center gap-2 sm:flex">
          <LanguageSwitcher />
          <ThemeSwitcher />
          <Button size="sm" onClick={onAdd}>
            <PlusIcon />{t('添加账号', 'Add account')}
          </Button>
        </div>
      </div>
    </header>
  )
}

function Preview() {
  const [adding, setAdding] = React.useState(false)
  return (
    <>
      <div className="app-shell flex min-h-dvh flex-col text-foreground">
        <PreviewHeader onAdd={() => setAdding(true)} />
        <main className="page-frame relative flex-1 py-4 pb-8 sm:py-5 sm:pb-10">
          <PreviewWorkspace />
        </main>
        <AppFooter />
      </div>
      <AddAccount open={adding} onOpenChange={setAdding} />
    </>
  )
}

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <LanguageProvider initialLanguage={previewLanguage} persist={false}>
      <QueryClientProvider client={queryClient}>
        <ToastProvider position="top-right">
          <TooltipProvider>
            <AnchoredToastProvider>
              <div className="relative isolate min-h-svh">
                <Preview />
              </div>
            </AnchoredToastProvider>
          </TooltipProvider>
        </ToastProvider>
      </QueryClientProvider>
    </LanguageProvider>
  </React.StrictMode>,
)
