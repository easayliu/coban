import { memo, useState } from 'react'
import { ChevronDownIcon, ChevronUpIcon, EllipsisIcon } from 'lucide-react'
import { type Credential } from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import { CredentialProxyDialog } from '@/components/credential-proxy-dialog'
import { CredentialRpmDialog } from '@/components/credential-rpm-dialog'
import { CredentialUsageDialog } from '@/components/credential-usage-dialog'
import {
  ConnectivityTestDialog,
  CredentialMenuContent,
  DeferredMount,
  DeleteCredentialDialog,
  evaluateCredential,
  planBadgeVariant,
  planLabel,
  quotaWindowLabel,
  switchTitle,
  useCredentialActions,
  type QuotaWindowMeta,
  type SortDir,
  type SortKey,
} from '@/components/credential-shared'
import { Badge } from '@/components/ui/badge'
import { Button, buttonVariants } from '@/components/ui/button'
import { Checkbox } from '@/components/ui/checkbox'
import { Meter, MeterIndicator, MeterTrack } from '@/components/ui/meter'
import { Switch } from '@/components/ui/switch'
import { Menu, MenuTrigger } from '@/components/ui/menu'
import { TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { Tooltip, TooltipPopup, TooltipTrigger } from '@/components/ui/tooltip'
import {
  cacheHitRate, cn, displayCredentialLabel, formatCompactNumber, formatPercent, formatTokens,
  formatUsd, relativeTime,
} from '@/lib/utils'

/**
 * 列宽写死而不是让表格自适应：内容宽度逐行不同（账号名长短、有没有代理徽章），
 * 自适应会让同一列在翻页时左右跳。
 */
const COL = {
  select: 'w-10',
  account: 'w-auto',
  status: 'w-32',
  // 优先级从 w-20 收到 w-16：装的只是一枚 `P0` 徽章，多出来的宽度让给下面新增的两列，
  // 免得 account 那一格（唯一的 w-auto）被挤到账号名整行截断。
  priority: 'w-16',
  plan: 'w-24',
  quotaPrimary: 'w-32',
  quotaSecondary: 'w-32',
  rpm: 'w-24',
  recent: 'w-24',
  // 累计三兄弟：请求数 / token 数 / 花费，挨着放才好互相印证（几条请求、多少 token、
  // 折成多少钱）。都是紧凑记数（`1.2K` / `931K`），w-20 够用。
  requests: 'w-20',
  tokens: 'w-20',
  cost: 'w-24',
  action: 'w-10',
} as const

const METER_TONE: Record<string, string> = {
  ok: 'bg-success',
  warning: 'bg-warning',
  critical: 'bg-destructive',
  empty: 'bg-muted-foreground/40',
}

/**
 * 表格里的额度条。
 *
 * 与卡片不同，**没报告的窗口也要占位**：表格列宽固定，摘掉单元格会让整行错位，
 * 所以这里显式写「—」，由 title 说明是「上游没报这个窗口」还是「还没有快照」。
 */
function ListQuotaMeter({ window: w, hasSnapshot }: { window: QuotaWindowMeta; hasSnapshot: boolean }) {
  const { t, language } = useI18n()
  const label = quotaWindowLabel(w, language)
  if (!w.reported) {
    return (
      <span
        className="text-muted-foreground text-xs"
        title={hasSnapshot
          ? t(`上游没有报告 ${label} 窗口`, `Upstream does not report a ${label} window`)
          : t('还没有额度快照', 'No quota snapshot yet')}
      >
        —
      </span>
    )
  }
  const pct = w.percentage
  return (
    <Tooltip>
      <TooltipTrigger
        render={
          <div className="flex items-center gap-2">
            <Meter value={pct ?? 0} max={100} className="w-16">
              <MeterTrack>
                <MeterIndicator className={METER_TONE[w.level] ?? METER_TONE.empty} />
              </MeterTrack>
            </Meter>
            <span className="text-xs tabular-nums">{pct == null ? '—' : `${pct}%`}</span>
          </div>
        }
      />
      <TooltipPopup>
        {pct == null
          ? t(`${label}：窗口已重置`, `${label}: window has reset`)
          : `${label} · ${pct}%`}
      </TooltipPopup>
    </Tooltip>
  )
}

export function CredentialListHeader({
  selectable,
  sort,
  dir,
  onSortChange,
  allSelected,
  onSelectAll,
}: {
  selectable?: boolean
  sort: SortKey
  dir: SortDir
  onSortChange: (key: SortKey) => void
  allSelected?: boolean
  onSelectAll?: (next: boolean) => void
}) {
  const { t } = useI18n()
  // 数值列表头跟着单元格右对齐：数字右对齐后个位数落在同一条线上，一列扫下来能直接比大小。
  const sortable = (label: string, key: SortKey, numeric = false) => {
    const active = sort === key
    const Arrow = active && dir === 'asc' ? ChevronUpIcon : ChevronDownIcon
    return (
      <Button
        type="button"
        size="xs"
        variant="ghost"
        onClick={() => onSortChange(key)}
        className={cn(
          'w-full px-0 sm:text-sm',
          numeric ? 'justify-end text-right' : 'justify-start text-left',
        )}
        title={active
          ? t(`按${label}排序（点击切换升降序）`, `Sort by ${label} (click to reverse direction)`)
          : t(`按${label}排序`, `Sort by ${label}`)}
      >
        {label}
        <Arrow className={cn(!active && 'opacity-0')} />
      </Button>
    )
  }
  const sortProps = (key: SortKey) =>
    sort === key ? ({ 'aria-sort': dir === 'asc' ? 'ascending' : 'descending' } as const) : {}

  return (
    <TableHeader className="hidden xl:table-header-group">
      <TableRow>
        <TableHead className={cn(COL.select, selectable ? 'pl-4 pr-0' : 'p-0')}>
          {selectable && (
            <Checkbox
              checked={!!allSelected}
              onCheckedChange={(checked) => onSelectAll?.(checked)}
              aria-label={t('全选当前筛选结果', 'Select all filtered results')}
            />
          )}
        </TableHead>
        <TableHead className={COL.account} {...sortProps('name')}>
          {sortable(t('账号', 'Account'), 'name')}
        </TableHead>
        <TableHead className={COL.status} {...sortProps('status')}>
          {sortable(t('状态', 'Status'), 'status')}
        </TableHead>
        <TableHead className={COL.priority} {...sortProps('priority')}>
          {sortable(t('优先级', 'Priority'), 'priority')}
        </TableHead>
        <TableHead className={COL.plan} {...sortProps('plan')}>
          {sortable(t('套餐', 'Plan'), 'plan')}
        </TableHead>
        <TableHead className={COL.quotaPrimary} {...sortProps('usagePrimary')}>
          {sortable(t('主额度', 'Primary'), 'usagePrimary')}
        </TableHead>
        <TableHead className={COL.quotaSecondary} {...sortProps('usageSecondary')}>
          {sortable(t('次额度', 'Secondary'), 'usageSecondary')}
        </TableHead>
        <TableHead className={cn(COL.rpm, 'text-right')} {...sortProps('rpm')}>
          {sortable(t('RPM', 'RPM'), 'rpm', true)}
        </TableHead>
        <TableHead className={COL.recent} {...sortProps('recent')}>
          {sortable(t('最近使用', 'Last used'), 'recent')}
        </TableHead>
        <TableHead className={cn(COL.requests, 'text-right')} {...sortProps('requests')}>
          {sortable(t('请求数', 'Requests'), 'requests', true)}
        </TableHead>
        <TableHead className={cn(COL.tokens, 'text-right')} {...sortProps('tokens')}>
          {sortable(t('token 数', 'Tokens'), 'tokens', true)}
        </TableHead>
        <TableHead className={cn(COL.cost, 'text-right')} {...sortProps('cost')}>
          {sortable(t('累计花费', 'Total cost'), 'cost', true)}
        </TableHead>
        <TableHead className={COL.action}>
          <span className="sr-only">{t('操作', 'Actions')}</span>
        </TableHead>
      </TableRow>
    </TableHeader>
  )
}

export const CredentialRow = memo(function CredentialRow({
  cred,
  now,
  selectable = false,
  selected = false,
  onSelectedChange,
}: {
  cred: Credential
  now: number
  selectable?: boolean
  selected?: boolean
  onSelectedChange?: (id: number, next: boolean) => void
}) {
  const { t, language, locale } = useI18n()
  const [proxyOpen, setProxyOpen] = useState(false)
  const [rpmOpen, setRpmOpen] = useState(false)
  const [usageOpen, setUsageOpen] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [testing, setTesting] = useState(false)

  const actions = useCredentialActions(cred)
  const { toggle, remove, rename } = actions
  const { quota, status } = evaluateCredential(cred, now, language)
  const credentialLabel = displayCredentialLabel(cred.label, language)
  const lastUsed = cred.stats.last_used_at
  // **cached 不另加**：上游报的 input 已经含它，三个一起加会把命中缓存的会话凭空放大一倍。
  const tokens = cred.stats.input_tokens_total + cred.stats.output_tokens_total
  // 这个号的终身缓存命中率。只进 token 那一格的悬浮提示：列宽是写死的（见 COL），
  // 再加一列会把唯一自适应的账号名那格挤到整行截断。
  const credCacheRate = cacheHitRate(cred.stats.input_tokens_total, cred.stats.cached_tokens_total)

  return (
    <>
      <TableRow className={cn(selected && 'bg-accent/40', cred.disabled && 'opacity-70')}>
        <TableCell className={cn(COL.select, selectable ? 'pl-4 pr-0' : 'p-0')}>
          {selectable && (
            <Checkbox
              checked={selected}
              onCheckedChange={(next) => onSelectedChange?.(cred.id, !!next)}
              aria-label={t(`选择 ${credentialLabel}`, `Select ${credentialLabel}`)}
            />
          )}
        </TableCell>
        <TableCell className={COL.account}>
          <div className="flex min-w-0 items-center gap-2">
            <Switch
              checked={!cred.disabled}
              onCheckedChange={(next) => toggle.mutate(!next)}
              aria-label={switchTitle(cred, language)}
              title={switchTitle(cred, language)}
            />
            <div className="min-w-0">
              <div className="truncate text-sm font-medium">{credentialLabel}</div>
              <div className="truncate text-xs text-muted-foreground">
                {cred.account_id_masked}
                {cred.proxy && ` · ${t('代理', 'proxy')}`}
              </div>
            </div>
          </div>
        </TableCell>
        <TableCell className={COL.status}>
          <Tooltip>
            <TooltipTrigger
              render={<Badge variant={status.variant} className="cursor-default">{status.label}</Badge>}
            />
            <TooltipPopup className="max-w-72">{status.detail}</TooltipPopup>
          </Tooltip>
        </TableCell>
        <TableCell className={COL.priority}>
          <Badge variant="outline">P{cred.priority}</Badge>
        </TableCell>
        <TableCell className={COL.plan}>
          <Badge variant={planBadgeVariant(cred.plan_type)}>{planLabel(cred.plan_type, language)}</Badge>
        </TableCell>
        <TableCell className={COL.quotaPrimary}>
          <ListQuotaMeter window={quota.primary} hasSnapshot={quota.hasSnapshot} />
        </TableCell>
        <TableCell className={COL.quotaSecondary}>
          <ListQuotaMeter window={quota.secondary} hasSnapshot={quota.hasSnapshot} />
        </TableCell>
        <TableCell className={cn(COL.rpm, 'text-right tabular-nums')}>
          <span title={t('最近 60 秒 / 生效上限', 'Last 60s / effective limit')}>
            {cred.rpm}
            <span className="text-muted-foreground">
              {' / '}
              {cred.rpm_limit_effective > 0 ? cred.rpm_limit_effective : '∞'}
            </span>
          </span>
        </TableCell>
        <TableCell className={cn(COL.recent, 'text-xs text-muted-foreground')}>
          {lastUsed ? relativeTime(lastUsed, now, language) : '—'}
        </TableCell>
        <TableCell className={cn(COL.requests, 'text-right tabular-nums')}>
          <Tooltip>
            <TooltipTrigger render={<span className="cursor-default" />}>
              {formatCompactNumber(cred.stats.request_total, locale)}
            </TooltipTrigger>
            <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">
              {t(
                `经这个账号转发过 ${cred.stats.request_total.toLocaleString(locale)} 条请求（含失败的）`,
                `${cred.stats.request_total.toLocaleString(locale)} requests forwarded through this account (failures included)`,
              )}
            </TooltipPopup>
          </Tooltip>
        </TableCell>
        <TableCell className={cn(COL.tokens, 'text-right tabular-nums')}>
          <Tooltip>
            <TooltipTrigger render={<span className="cursor-default" />}>
              {formatTokens(tokens)}
            </TooltipTrigger>
            <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">
              {t(
                `累计 ${tokens.toLocaleString(locale)} token：输入 ${cred.stats.input_tokens_total.toLocaleString(locale)}（其中命中缓存 ${cred.stats.cached_tokens_total.toLocaleString(locale)}，命中率 ${formatPercent(credCacheRate)}）+ 输出 ${cred.stats.output_tokens_total.toLocaleString(locale)}`,
                `${tokens.toLocaleString(locale)} tokens total: ${cred.stats.input_tokens_total.toLocaleString(locale)} input (${cred.stats.cached_tokens_total.toLocaleString(locale)} cache hits, ${formatPercent(credCacheRate)} hit rate) + ${cred.stats.output_tokens_total.toLocaleString(locale)} output`,
              )}
            </TooltipPopup>
          </Tooltip>
        </TableCell>
        <TableCell className={cn(COL.cost, 'text-right tabular-nums')}>
          {formatUsd(cred.stats.cost_total_usd)}
        </TableCell>
        <TableCell className={COL.action}>
          <Menu>
            <MenuTrigger
              className={buttonVariants({ size: 'icon-sm', variant: 'ghost' })}
              aria-label={t('更多操作', 'More actions')}
            >
              <EllipsisIcon />
            </MenuTrigger>
            <CredentialMenuContent
              cred={cred}
              actions={actions}
              onRename={() => {
                // 列表行没有内联编辑位（列宽固定），改名走一次 prompt 就够——它只是个
                // 备注，真要精修可以去卡片视图。
                const next = window.prompt(t('账号备注', 'Account label'), cred.label)
                if (next?.trim()) rename.mutate(next.trim())
              }}
              onRpmLimit={() => setRpmOpen(true)}
              onProxy={() => setProxyOpen(true)}
              onUsage={() => setUsageOpen(true)}
              onTest={() => setTesting(true)}
              onRequestDelete={() => setConfirmDelete(true)}
            />
          </Menu>
        </TableCell>
      </TableRow>

      <DeferredMount open={rpmOpen}>
        <CredentialRpmDialog cred={cred} open={rpmOpen} onOpenChange={setRpmOpen} rpmLimit={actions.rpmLimit} />
      </DeferredMount>
      <DeferredMount open={proxyOpen}>
        <CredentialProxyDialog cred={cred} open={proxyOpen} onOpenChange={setProxyOpen} proxy={actions.proxy} />
      </DeferredMount>
      <DeferredMount open={usageOpen}>
        <CredentialUsageDialog cred={cred} open={usageOpen} onOpenChange={setUsageOpen} />
      </DeferredMount>
      <DeferredMount open={testing}>
        <ConnectivityTestDialog cred={cred} open={testing} onOpenChange={setTesting} />
      </DeferredMount>
      <DeferredMount open={confirmDelete}>
        <DeleteCredentialDialog
          cred={cred}
          open={confirmDelete}
          onOpenChange={setConfirmDelete}
          onConfirm={() => remove.mutate()}
          pending={remove.isPending}
        />
      </DeferredMount>
    </>
  )
})
