import { memo, useState } from 'react'
import { ChevronDownIcon, ChevronUpIcon, EllipsisIcon } from 'lucide-react'
import { type Credential } from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import { CredentialProxyDialog } from '@/components/credential-proxy-dialog'
import { CredentialRenameDialog } from '@/components/credential-rename-dialog'
import { CredentialRpmDialog } from '@/components/credential-rpm-dialog'
import { CredentialUsageDialog } from '@/components/credential-usage-dialog'
import {
  accountIdTitle,
  ConnectivityTestDialog,
  CredentialMenuContent,
  DeferredMount,
  DeleteCredentialDialog,
  ResetQuotaDialog,
  evaluateCredential,
  isRangeSelect,
  planBadgeVariant,
  planLabel,
  proxyTitle,
  QuotaMeter,
  quotaCapacityEstimate,
  quotaWindowLabel,
  quotaWindowTitle,
  switchTitle,
  useCredentialActions,
  type QuotaWindowMeta,
  type SortDir,
  type SortKey,
} from '@/components/credential-shared'
import { Badge } from '@/components/ui/badge'
import { Button, buttonVariants } from '@/components/ui/button'
import { Checkbox } from '@/components/ui/checkbox'
import { Switch } from '@/components/ui/switch'
import { Menu, MenuTrigger } from '@/components/ui/menu'
import { TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { HINT_FOCUS_RING, Tooltip, TooltipPopup, TooltipTrigger } from '@/components/ui/tooltip'
import {
  cn, displayCredentialLabel, formatCountdown, formatFullTime, formatUsd, relativeTime,
} from '@/lib/utils'

/**
 * 列宽写死而不是让表格自适应：内容宽度逐行不同（账号名长短、有没有代理徽章），
 * 自适应会让同一列在翻页时左右跳。
 */
const COL = {
  select: 'w-10',
  account: 'w-auto',
  status: 'w-32',
  // 装的只是一枚 `P0` 徽章，w-16 够；省下的宽度归 account 那一格（唯一的 w-auto），
  // 免得账号名整行截断。
  priority: 'w-16',
  plan: 'w-24',
  // 额度两列的宽度由**条子上面那行**定，不是条子定：`1.5K req · 143M · $1,053.68` 这种
  // 最长的一行在 .625rem 下要 132px，w-40 的一格净宽 140px 刚好装下（见 [QuotaMeter]）。
  quotaPrimary: 'w-40',
  quotaSecondary: 'w-40',
  rpm: 'w-24',
  recent: 'w-24',
  /**
   * 累计花费。**终身累计只留这一列**。
   *
   * 请求数与 token 数原来也在这里（各 4.5rem）。画布是 76rem（.page-frame 80rem 减左右各 2rem，
   * 表格与卡片共用同一个数，见 index.css 那条注），其余各列之和已经 64.5rem——把那两列放回来
   * 就是 73.5rem，账号名那一格只剩 40 来个像素，整行全是省略号。
   *
   * 按列优先级挑剩下谁（宽表放不下时按决策价值分档：Ant Design 的 `responsive`、Carbon /
   * Spectrum 的 hide-at-breakpoint 都是这一套）：调度时要盯的是状态、额度、RPM、花费，请求数与
   * token 数是「看一眼就够」的量。它们仍在「用量」弹窗和窄屏卡片的页脚里，工具栏的排序菜单也
   * 照旧能按它们排。
   */
  cost: 'w-24',
  /**
   * 启用开关。**行尾、紧挨着 ⋯ 菜单**，不在账号那一格里。
   *
   * 数据表的通行排法是：前缘只放身份（勾选框 + 主标识），行内操作一律靠后缘（Material 的
   * row actions、Carbon 的 overflow menu、Polaris IndexTable 的 trailing actions 都是这个
   * 位置）。开关放在最前面有三处不划算：
   *
   * - 扫视是从左边起头的，那个位置该给账号名，不该给一个控件；
   * - 它与全选勾选框贴在一起，两个相邻热区一个是「选中」一个是「立刻停掉这个号」，误点的
   *   代价差得太远；
   * - 读屏顺序上，控件出现在它所属的那一行还没被念出来之前。
   *
   * 状态那一列讲的是「现在怎么了」（停用 / 额度用尽 / 冷却中），这一列是**改它的手柄**，
   * 两者分开：菜单里没有启用/停用项，所以开关必须留在表面。
   *
   * w-14：开关本体 30px（`--thumb-size` 4 那档）加左右内边距刚好，列头「启用」也是这个宽。
   */
  toggle: 'w-14',
  action: 'w-10',
} as const

/**
 * 表格里的额度条：用的就是卡片那根（见 [QuotaMeter]），分级配色、圆角粗细、百分比读法同源
 * ——同一个 100% 在两处长得不一样，会让人以为是两个不同的量。
 *
 * 窗口用量那三项（请求数 / token / 等价费用）也照卡片摆在条子上方，只是压成一行小字
 * ——它们讲的是**当前这个窗口**，而右边「累计花费」那列是终身累计，两个口径必须能对照着看。
 *
 * 距重置的倒计时直接跟在进度条后面，方便不打开悬浮提示也能看到；绝对重置时刻、窗口用量
 * 精确值和快照时刻仍放在悬浮提示里。快照时刻既然在这份提示里，就**不给共用组件传
 * snapshotTs**，否则它会在触发区内再挂一个原生 title，两层提示一起冒出来。
 *
 * 与卡片不同，**没报告的窗口也要占位**：表格列宽固定，摘掉单元格会让整行错位，
 * 所以这里显式写「—」，由提示说明是「上游没报这个窗口」还是「还没有快照」。
 */
function ListQuotaMeter({
  credentialLabel,
  window: w,
  hasSnapshot,
  snapshotTs,
  now,
  namedByHeader,
}: {
  credentialLabel: string
  window: QuotaWindowMeta
  hasSnapshot: boolean
  snapshotTs: number | null
  now: number
  /** 列头是否已经写着这个窗口的名字（见 credential-workspace 的 `quotaTitles`）。 */
  namedByHeader: boolean
}) {
  const { t, language, locale } = useI18n()
  const label = quotaWindowLabel(w, language)
  if (!w.reported) {
    // 与有数据那一格同一套提示（Tooltip 而不是原生 title）：一格里两种提示，键盘还只认其中
    // 一种，说不通。这一格更需要解释——光一个「—」看不出是「上游没报这个窗口」还是「还没快照」。
    return (
      <Tooltip>
        <TooltipTrigger
          render={<span tabIndex={0} />}
          className={cn('text-muted-foreground text-xs', HINT_FOCUS_RING)}
        >
          —
        </TooltipTrigger>
        <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">
          {hasSnapshot
            ? t(`上游没有报告 ${label} 窗口`, `Upstream does not report a ${label} window`)
            : t('还没有额度快照', 'No quota snapshot yet')}
        </TooltipPopup>
      </Tooltip>
    )
  }
  const windowTitle = quotaWindowTitle(w.windowMinutes, language)
  const estimate = quotaCapacityEstimate(w)
  // 通用名而不是列头那个名字：这句要在「列头写着周、这一行其实是 5 小时」时也读得对。
  const generic = w.key === 'primary' ? t('主额度', 'Primary') : t('次额度', 'Secondary')
  const scope = windowTitle
    ? t(`${generic}（${windowTitle}窗口）`, `${generic} (${windowTitle} window)`)
    : generic
  return (
    <Tooltip>
      <TooltipTrigger render={<div tabIndex={0} className={cn('min-w-0', HINT_FOCUS_RING)} />}>
        <QuotaMeter
          credentialLabel={credentialLabel}
          window={w}
          snapshotTs={null}
          now={now}
          usage="inline"
          showCountdown
          // 列头没写窗口名时才挂那枚 `7d`：整池窗口长度不一致才会这样，此时它是这一行唯一
          // 能说清「这 100% 是多长时间里的」的东西。长度未知的行不挂——那时标签会退成
          // 「主额度」三个字，把这格撑开。
          showWindowLabel={!namedByHeader && w.windowMinutes != null}
        />
      </TooltipTrigger>
      <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">
        <span className="block font-medium">
          {w.percentage == null
            ? t(`${scope} · 窗口已重置`, `${scope} · window has reset`)
            : `${scope} · ${w.percentage}%`}
        </span>
        {/* 格子里那行是紧凑记数（`1.5K` / `143M`），这里给精确值——`1.5K` 看不出是 1532
            还是 1549。三项都只算当前这个窗口，右边那几列才是终身累计。 */}
        {w.usage && (
          <span className="block">
            {t(
              `窗口内 ${w.usage.requests.toLocaleString(locale)} 条请求 · ${w.usage.tokens.toLocaleString(locale)} token · 等价 ${formatUsd(w.usage.cost_usd)}（按官方 API 价目估，不是账单）`,
              `This window: ${w.usage.requests.toLocaleString(locale)} requests · ${w.usage.tokens.toLocaleString(locale)} tokens · ${formatUsd(w.usage.cost_usd)} equivalent (estimated from official API rates, not a bill)`,
            )}
          </span>
        )}
        {estimate && (
          <span className="block">
            {t(
              `预估周期总额约 ${estimate.tokens != null ? `${estimate.tokens.toLocaleString(locale)} token` : '—'} / ${estimate.costUsd != null ? formatUsd(estimate.costUsd) : '—'}（按 ${estimate.usedPercentage}% 使用率反推；仅统计本服务记录，供参考）`,
              `Estimated cycle total: about ${estimate.tokens != null ? `${estimate.tokens.toLocaleString(locale)} tokens` : '—'} / ${estimate.costUsd != null ? formatUsd(estimate.costUsd) : '—'} (inferred from ${estimate.usedPercentage}% used; only this service's recorded traffic, for reference)`,
            )}
          </span>
        )}
        {w.resetAt != null && w.resetAt > now && (
          <span className="block">
            {t(
              `${formatCountdown(w.resetAt, now)} 后重置（${formatFullTime(w.resetAt, language)}）`,
              `Resets in ${formatCountdown(w.resetAt, now)} (${formatFullTime(w.resetAt, language)})`,
            )}
          </span>
        )}
        {/* 快照可能明显早于最近一次请求（只有解出限流头的那次才更新）。少了这句，一个过期的
            12% 会被当成现状。 */}
        <span className="block text-muted-foreground">
          {snapshotTs != null
            ? t(`快照于 ${formatFullTime(snapshotTs, language)}`, `Snapshot at ${formatFullTime(snapshotTs, language)}`)
            : t('还没有额度快照', 'No quota snapshot yet')}
        </span>
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
  quotaTitles,
  quotaColumns = { primary: true, secondary: true },
}: {
  selectable?: boolean
  sort: SortKey
  dir: SortDir
  onSortChange: (key: SortKey) => void
  allSelected?: boolean
  onSelectAll?: (next: boolean) => void
  /**
   * 两个额度列的列名，由**上游真的报了的窗口长度**算出来（见 `poolQuotaTitles`）。
   *
   * 「主额度 / 次额度」是上游那两组头的名字，不是窗口的名字：实测同一批 Pro 账号上
   * `x-codex-primary-*` 报的是**周**窗口，而「次额度」那一对压根是空的。列名照抄头的名字，
   * 于是这张表既没告诉人那是哪个窗口，还留着一列永远是「—」。
   *
   * 池子里各账号窗口长度不一致（不同套餐）时为 `null`，那时才退回通用名——那种情况下任何
   * 一个具体窗口名都会在别的行上是错的。
   */
  quotaTitles?: { primary: string | null; secondary: string | null }
  /**
   * 这两个额度列**在不在**（见 `poolQuotaColumns`）。
   *
   * 上游对某个窗口整池都没报时，那一列每一行都是「—」，占着 8rem 而一个字也没说。这张表
   * 已经宽到要横向滚了，那一列得让出去。行与表头**必须用同一份判断**，少一格就整行错位。
   */
  quotaColumns?: { primary: boolean; secondary: boolean }
}) {
  const { t } = useI18n()
  // 数值列表头跟着单元格右对齐：数字右对齐后个位数落在同一条线上，一列扫下来能直接比大小。
  const sortable = (label: string, key: SortKey, numeric = false) => {
    const active = sort === key
    const Arrow = active && dir === 'asc' ? ChevronUpIcon : ChevronDownIcon
    // 未激活的箭头只是透明，位置照占。摆在标签后面时，数值列的表头会被它整体往左顶开
    // 一个图标加间距的宽度，与贴着右边缘的数字错开半个字——所以数值列把箭头放到标签前面，
    // 让标签自己压住右边缘；文字列仍是标签在前，读起来是「列名 + 排序方向」。
    const arrow = <Arrow className={cn(!active && 'opacity-0')} />
    return (
      <Button
        type="button"
        size="xs"
        variant="ghost"
        onClick={() => onSortChange(key)}
        // border-0：ghost 那圈透明边框也是 1px 实宽，会把表头文字整体推离单元格内边距，
        // 与下面单元格里的内容差一像素。表头是纯文字按钮，不需要这圈边框占位。
        className={cn(
          'w-full border-0 px-0 sm:text-sm',
          numeric ? 'justify-end text-right' : 'justify-start text-left',
        )}
        title={active
          ? t(`按${label}排序（点击切换升降序）`, `Sort by ${label} (click to reverse direction)`)
          : t(`按${label}排序`, `Sort by ${label}`)}
      >
        {numeric ? <>{arrow}{label}</> : <>{label}{arrow}</>}
      </Button>
    )
  }
  /**
   * 额度列的列名。窗口名后面**要带「额度」这个名词**：光一个「周」夹在「套餐」和「RPM」
   * 中间读不出这一列讲的是什么。
   *
   * 「额度」不能塞进 `quotaTitles` 本身——排序菜单那份自己拼（见 credential-workspace 的
   * `sortItems`：`${window}额度使用率`），存进去就成了「周额度额度使用率」。
   */
  const quotaTitle = (window: string | null | undefined, fallback: string) =>
    (window ? t(`${window}额度`, `${window} usage`) : fallback)

  const sortProps = (key: SortKey) =>
    sort === key ? ({ 'aria-sort': dir === 'asc' ? 'ascending' : 'descending' } as const) : {}

  return (
    <TableHeader>
      <TableRow>
        {/* 勾选那列不自己改左内边距：卡片式表格给首列的 td 补了 `border-s`，并把
            padding-inline-start 减掉那 1px，让内容仍落在 px-2.5 这条线上。该规则带
            variant + first 前缀，特异性高于这里写的 pl-4——td 压不动，th 却真的缩进
            16px，于是表头那枚勾选框比每行的都往右挪了 6px。两边一起用默认内边距。 */}
        <TableHead className={cn(COL.select, !selectable && 'p-0')}>
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
        {quotaColumns.primary && (
          <TableHead className={COL.quotaPrimary} {...sortProps('usagePrimary')}>
            {sortable(quotaTitle(quotaTitles?.primary, t('主额度', 'Primary')), 'usagePrimary')}
          </TableHead>
        )}
        {quotaColumns.secondary && (
          <TableHead className={COL.quotaSecondary} {...sortProps('usageSecondary')}>
            {sortable(quotaTitle(quotaTitles?.secondary, t('次额度', 'Secondary')), 'usageSecondary')}
          </TableHead>
        )}
        <TableHead className={cn(COL.rpm, 'text-right')} {...sortProps('rpm')}>
          {sortable(t('RPM', 'RPM'), 'rpm', true)}
        </TableHead>
        <TableHead className={COL.recent} {...sortProps('recent')}>
          {sortable(t('最近使用', 'Last used'), 'recent')}
        </TableHead>
        <TableHead className={cn(COL.cost, 'text-right')} {...sortProps('cost')}>
          {sortable(t('累计花费', 'Total cost'), 'cost', true)}
        </TableHead>
        <TableHead className={cn(COL.toggle, 'text-center')}>
          <span title={t('启用 / 停用该账号', 'Enable or disable this account')}>
            {t('启用', 'Enabled')}
          </span>
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
  quotaColumns = { primary: true, secondary: true },
  quotaTitles,
}: {
  cred: Credential
  now: number
  selectable?: boolean
  selected?: boolean
  /** `extend`：按着 shift 点的，勾选从锚点到这一行之间整段（见 credential-workspace）。 */
  onSelectedChange?: (id: number, next: boolean, extend?: boolean) => void
  /** 与表头同一份判断，见 [CredentialListHeader] 上那条注。 */
  quotaColumns?: { primary: boolean; secondary: boolean }
  /**
   * 与表头同一份列名。行里只用它判断一件事：列头**已经**写着窗口名了没有——写了就不必在
   * 每格再挂一枚 `7d`，没写（整池窗口长度不一致）才挂。见 [CredentialListHeader]。
   */
  quotaTitles?: { primary: string | null; secondary: string | null }
}) {
  const { t, language } = useI18n()
  const [renameOpen, setRenameOpen] = useState(false)
  const [proxyOpen, setProxyOpen] = useState(false)
  const [rpmOpen, setRpmOpen] = useState(false)
  const [usageOpen, setUsageOpen] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [confirmReset, setConfirmReset] = useState(false)
  const [testing, setTesting] = useState(false)

  const actions = useCredentialActions(cred)
  const { toggle, remove, consumeReset } = actions
  const { quota, status } = evaluateCredential(cred, now, language)
  const credentialLabel = displayCredentialLabel(cred.label, language)
  const lastUsed = cred.stats.last_used_at
  // token 累计量与缓存命中率不在这张表上（见 [COL] 的 `cost`），要看去「用量」弹窗。

  return (
    <>
      <TableRow className={cn(selected && 'bg-accent/40', cred.disabled && 'opacity-70')}>
        <TableCell className={cn(COL.select, !selectable && 'p-0')}>
          {selectable && (
            <Checkbox
              checked={selected}
              onCheckedChange={(next, details) => onSelectedChange?.(
                cred.id,
                !!next,
                isRangeSelect(details.event),
              )}
              aria-label={t(`选择 ${credentialLabel}`, `Select ${credentialLabel}`)}
            />
          )}
        </TableCell>
        {/* 身份格：主标识 + 一行副标识。**不放头像**——表格这边一行只有一个字母的圆圈，
            信息量抵不上它吃掉的 42px（卡片那边尺寸够、留着）。这一列是唯一自适应的，填空靠
            副标识那行，不靠装饰。
            副标识照抄卡片的组成（`#3 · …9f31d0`），两个视图对同一个号说同一套话。
            首列之后**只放身份**，控件一个都不放：开关已经搬到行尾（见 [COL] 的 `toggle`）。 */}
        <TableCell className={COL.account}>
          {/* **整格一个提示**，不是分挂在三处原生 `title` 上。表格里以「格」为提示单位是常规
              做法（整行做触发区，扫表时会到处乱弹），而原生 title 只认鼠标悬停，键盘用户一句
              也拿不到。这一格要解释的三件事本来就是同一件：这个号是谁——名字（会被截断）、
              账号标识（掩码尾段单看认不出）、怎么出网（代理地址不该常驻表面）。 */}
          <Tooltip>
            <TooltipTrigger
              render={<div tabIndex={0} />}
              className={cn('min-w-0', HINT_FOCUS_RING)}
            >
              <div className="truncate text-sm font-medium leading-snug">{credentialLabel}</div>
              <div className="flex min-w-0 items-center gap-1.5 overflow-hidden text-xs text-muted-foreground">
                {/* coban 自己的行号：改名、同名、掩码尾段也撞车时，只有它是绝对的。 */}
                <span className="shrink-0 tabular-nums">#{cred.id}</span>
                <span aria-hidden="true">·</span>
                <span className="min-w-0 truncate tabular-nums">{cred.account_id_masked}</span>
                {cred.proxy && (
                  <>
                    <span aria-hidden="true">·</span>
                    <span className="shrink-0">{t('代理', 'proxy')}</span>
                  </>
                )}
              </div>
            </TooltipTrigger>
            {/* 顺序与格子里那两行对应。名字放头一行并加粗：这一格最常被截断的就是它。
                账号那行传了 credentialLabel，备注本来就是邮箱时不再重复念一遍邮箱
                （见 [accountIdTitle]）。 */}
            <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">
              <span className="block font-medium">{credentialLabel}</span>
              <span className="block">{accountIdTitle(cred, language, credentialLabel)}</span>
              {cred.proxy && <span className="block">{proxyTitle(cred.proxy, language)}</span>}
            </TooltipPopup>
          </Tooltip>
        </TableCell>
        <TableCell className={COL.status}>
          <Tooltip>
            <TooltipTrigger
              render={
                <Badge variant={status.variant} tabIndex={0} className="cursor-help">{status.label}</Badge>
              }
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
        {quotaColumns.primary && (
          <TableCell className={COL.quotaPrimary}>
            <ListQuotaMeter
              credentialLabel={credentialLabel}
              window={quota.primary}
              hasSnapshot={quota.hasSnapshot}
              snapshotTs={quota.snapshotTs}
              now={now}
              namedByHeader={quotaTitles?.primary != null}
            />
          </TableCell>
        )}
        {quotaColumns.secondary && (
          <TableCell className={COL.quotaSecondary}>
            <ListQuotaMeter
              credentialLabel={credentialLabel}
              window={quota.secondary}
              hasSnapshot={quota.hasSnapshot}
              snapshotTs={quota.snapshotTs}
              now={now}
              namedByHeader={quotaTitles?.secondary != null}
            />
          </TableCell>
        )}
        <TableCell className={cn(COL.rpm, 'text-right tabular-nums')}>
          <Tooltip>
            <TooltipTrigger render={<span tabIndex={0} />} className={HINT_FOCUS_RING}>
              {cred.rpm}
              <span className="text-muted-foreground">
                {' / '}
                {cred.rpm_limit_effective > 0 ? cred.rpm_limit_effective : '∞'}
              </span>
            </TooltipTrigger>
            <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">
              {t(
                '斜杠左边是最近 60 秒经这个账号转发的请求数，右边是生效的上限（∞ = 不限）',
                'Left of the slash: requests forwarded through this account in the last 60 seconds. Right: the effective limit (∞ = unlimited)',
              )}
            </TooltipPopup>
          </Tooltip>
        </TableCell>
        <TableCell className={cn(COL.recent, 'text-xs text-muted-foreground')}>
          {lastUsed ? relativeTime(lastUsed, now, language) : '—'}
        </TableCell>
        <TableCell className={cn(COL.cost, 'text-right tabular-nums')}>
          {formatUsd(cred.stats.cost_total_usd)}
        </TableCell>
        <TableCell className={cn(COL.toggle, 'text-center')}>
          {/* aria-label 里带账号名：读屏一路 tab 过来时，这一格与它管的那一行早就隔了十列。 */}
          <Switch
            checked={!cred.disabled}
            onCheckedChange={(next) => toggle.mutate(!next)}
            aria-label={`${credentialLabel} — ${switchTitle(cred, language)}`}
            title={switchTitle(cred, language)}
          />
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
              // 行里没有内联编辑位（列宽是写死的），所以走弹窗——不是原生 prompt：桌面已经
              // 没有卡片视图可切，这是宽屏唯一的改名路径（见 [CredentialRenameDialog]）。
              onRename={() => setRenameOpen(true)}
              onRpmLimit={() => setRpmOpen(true)}
              onProxy={() => setProxyOpen(true)}
              onUsage={() => setUsageOpen(true)}
              onTest={() => setTesting(true)}
              onRequestReset={() => setConfirmReset(true)}
              onRequestDelete={() => setConfirmDelete(true)}
            />
          </Menu>
        </TableCell>
      </TableRow>

      <DeferredMount open={renameOpen}>
        <CredentialRenameDialog
          cred={cred}
          open={renameOpen}
          onOpenChange={setRenameOpen}
          rename={actions.rename}
        />
      </DeferredMount>
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
      <DeferredMount open={confirmReset}>
        <ResetQuotaDialog
          cred={cred}
          open={confirmReset}
          onOpenChange={setConfirmReset}
          onConfirm={() => consumeReset.mutate(undefined, { onSettled: () => setConfirmReset(false) })}
          pending={consumeReset.isPending}
        />
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
