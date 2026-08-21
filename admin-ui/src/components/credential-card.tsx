import { memo, useState } from 'react'
import {
  CalendarDaysIcon,
  CheckIcon,
  ClockIcon,
  EllipsisIcon,
  HashIcon,
  ScrollTextIcon,
  WalletCardsIcon,
  XIcon,
} from 'lucide-react'
import { type Credential } from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import {
  cn,
  displayCredentialLabel,
  formatCompactNumber,
  formatCountdown,
  formatFullTime,
  formatTokens,
  formatUsd,
  relativeTime,
} from '@/lib/utils'
import {
  ConnectivityTestDialog,
  CredentialMenuContent,
  DeferredMount,
  DeleteCredentialDialog,
  credentialExpiryMeta,
  evaluateCredential,
  planBadgeVariant,
  planLabel,
  quotaWindowLabel,
  switchTitle,
  useCredentialActions,
  type QuotaWindowMeta,
} from '@/components/credential-shared'
import { CredentialProxyDialog } from '@/components/credential-proxy-dialog'
import { CredentialRpmDialog } from '@/components/credential-rpm-dialog'
import { CredentialUsageDialog } from '@/components/credential-usage-dialog'
import { Avatar, AvatarFallback } from '@/components/ui/avatar'
import { Badge, badgeVariants, type BadgeProps } from '@/components/ui/badge'
import { Button, buttonVariants } from '@/components/ui/button'
import { Checkbox } from '@/components/ui/checkbox'
import {
  Card,
  CardAction,
  CardDescription,
  CardFooter,
  CardHeader,
  CardPanel,
  CardTitle,
} from '@/components/ui/card'
import { Form } from '@/components/ui/form'
import { Input } from '@/components/ui/input'
import { Menu, MenuTrigger } from '@/components/ui/menu'
import {
  Meter,
  MeterIndicator,
  MeterLabel,
  MeterTrack,
  MeterValue,
} from '@/components/ui/meter'
import { Separator } from '@/components/ui/separator'
import { Spinner } from '@/components/ui/spinner'
import { Switch } from '@/components/ui/switch'
import { Tooltip, TooltipPopup, TooltipTrigger } from '@/components/ui/tooltip'

/**
 * memo 的收益在于「列表本身没变，但父组件重渲染了」这类情况：搜索框每敲一个字、
 * 勾选任意一行、翻页动画，都会重跑一遍工作区。配合稳定的 onSelectedChange 才生效。
 */
export const CredentialCard = memo(function CredentialCard({
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
  /** 收 id 而不是每张卡现做一个闭包，回调引用才能稳定，memo 才拦得住重渲染。 */
  onSelectedChange?: (id: number, next: boolean) => void
}) {
  const { t, language, locale } = useI18n()
  const [editing, setEditing] = useState(false)
  const [name, setName] = useState(cred.label)
  const [proxyOpen, setProxyOpen] = useState(false)
  const [rpmOpen, setRpmOpen] = useState(false)
  const [usageOpen, setUsageOpen] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [testing, setTesting] = useState(false)

  const actions = useCredentialActions(cred, () => setEditing(false))
  const { rename, toggle, remove } = actions
  const { quota, status } = evaluateCredential(cred, now, language)
  const credentialLabel = displayCredentialLabel(cred.label, language)
  const initial = credentialLabel.trim().charAt(0).toUpperCase() || '?'
  const titleId = `credential-card-title-${cred.id}`
  const added = relativeTime(cred.created_at, now, language)
  // 只渲染上游真报过的窗口。卡片是弹性布局，没有的那个直接不占位；表格那边列宽固定，
  // 摘不掉，所以改成显式的「无此窗口」，见 credential-row。
  const reportedWindows = quota.windows.filter((w) => w.reported)
  // 0 = 不限，此时页脚只显示 RPM 本身，不画分母、也不谈「打满」。
  const rpmLimit = cred.rpm_limit_effective
  const rpmFull = rpmLimit > 0 && cred.rpm >= rpmLimit
  const rpmLive = cred.rpm > 0
  const requests = cred.stats.request_total
  const requestsText = formatCompactNumber(requests, locale)
  // **cached 不另加**：上游报的 input 已经含它（见 CredentialStats 的注），
  // 三个一起加会把命中缓存的会话凭空放大一倍。
  const tokens = cred.stats.input_tokens_total + cred.stats.output_tokens_total
  const tokensText = formatTokens(tokens)
  // 页脚四组数字（请求数 / token 数 / 累计费用 / RPM）在窄卡片上排一行还是两行，按字符数
  // 定：都是等宽字形，字符数就是宽度。375px 的屏上实测能容下 17 个字符（`1.2k` + `931K`
  // + `$214.60` + `100/120` 就已经超了），再多才折行，否则尾巴会伸到右边的开关底下。
  const footerChars = requestsText.length
    + tokensText.length
    + formatUsd(cred.stats.cost_total_usd).length
    + `${cred.rpm}${rpmLimit > 0 ? `/${rpmLimit}` : ''}`.length
  const footerStacked = footerChars > 17
  // 所有需处理状态都用同一种渐进披露：卡片只显示状态，详情在悬浮提示里查看。
  // 避免同一条状态再渲染一块说明，把异常卡片单独撑高。
  const statusUsesTooltip = status.attention
  const snapshotTime = quota.snapshotTs != null
    ? formatFullTime(quota.snapshotTs, language)
    : t('未知时间', 'unknown time')
  // token 到期只在**马上就要过期**时才出现：刷新是惰性的（选号之后、发请求之前必刷），
  // 所以「有效 43 分钟」对使用者不构成任何决策依据，常驻只是噪声。
  const expiry = credentialExpiryMeta(cred, language)
  const expiryUrgent = expiry.tone === 'warning'
  // credits 是「基础额度满了还能不能继续跑」。**只在真的有 credits 时才补一枚徽章**：
  // 没有额外 credits 是普通订阅的常态（见 CreditsState 的注），给每个正常账号挂一枚
  // 「无 credits」等于把默认状态说成风险，而卡片上每一枚徽章都在抢注意力。
  const creditsBadge = (() => {
    if (quota.credits === 'unlimited') {
      return {
        label: t('Credits 不限', 'Unlimited credits'),
        variant: 'info' as const,
        title: t(
          '上游报告该账号的额外 credits 不限量：基础额度用满后仍会按量计费继续放行',
          'The upstream reports unlimited extra credits: requests keep flowing at pay-as-you-go rates once the base quota fills up',
        ),
      }
    }
    if (quota.credits === 'available') {
      const balance = quota.creditsBalance
      return {
        label: t('有 credits', 'Credits available'),
        variant: 'info' as const,
        title: balance != null
          ? t(
            `额外 credits 余额 ${balance.toLocaleString(locale)}（${snapshotTime} 的快照）：基础额度用满后按量计费继续放行`,
            `Extra credits balance ${balance.toLocaleString(locale)} (snapshot at ${snapshotTime}): requests keep flowing at pay-as-you-go rates once the base quota fills up`,
          )
          : t(
            '上游报告该账号还有额外 credits：基础额度用满后按量计费继续放行',
            'The upstream reports remaining extra credits: requests keep flowing at pay-as-you-go rates once the base quota fills up',
          ),
      }
    }
    return null
  })()

  return (
    <li className="min-w-0 h-full">
      <Card
        render={<article aria-labelledby={titleId} />}
        className={cn(
          '@container/card h-full overflow-hidden',
          selected && 'ring-2 ring-ring ring-offset-2 ring-offset-background',
        )}
      >
        <CardHeader className="p-4 pb-3">
          <CardTitle className="min-w-0 text-sm leading-snug">
            {editing ? (
              <>
                <h3 id={titleId} className="sr-only">{credentialLabel}</h3>
                <Form
                  className="flex items-center gap-2"
                  onSubmit={(event) => {
                    event.preventDefault()
                    const nextName = name.trim()
                    if (nextName) rename.mutate(nextName)
                  }}
                >
                  <Input
                    value={name}
                    onChange={(event) => setName(event.target.value)}
                    autoFocus
                    aria-label={t('账号名称', 'Account name')}
                  />
                  <Button
                    type="submit"
                    size="icon"
                    variant="outline"
                    loading={rename.isPending}
                    disabled={!name.trim()}
                    aria-label={t('保存账号名称', 'Save account name')}
                  >
                    <CheckIcon />
                  </Button>
                  <Button
                    type="button"
                    size="icon"
                    variant="ghost"
                    aria-label={t('取消重命名', 'Cancel renaming')}
                    onClick={() => {
                      setEditing(false)
                      setName(cred.label)
                    }}
                  >
                    <XIcon />
                  </Button>
                </Form>
              </>
            ) : (
              <div className="flex min-w-0 items-center gap-3">
                {selectable && (
                  <Checkbox
                    checked={selected}
                    onCheckedChange={(checked) => onSelectedChange?.(cred.id, checked)}
                    aria-label={t(`选择 ${credentialLabel}`, `Select ${credentialLabel}`)}
                  />
                )}
                <Avatar className="hidden @sm/card:flex" aria-hidden="true">
                  <AvatarFallback>{initial}</AvatarFallback>
                </Avatar>
                <div className="min-w-0 flex-1">
                  <h3
                    id={titleId}
                    className="block min-w-0 truncate whitespace-nowrap leading-snug"
                    title={credentialLabel}
                  >
                    {credentialLabel}
                  </h3>
                  <CardDescription className="mt-1 flex min-w-0 flex-wrap items-center gap-x-1.5 gap-y-0.5 text-2xs font-normal">
                    <span className="tabular-nums">#{cred.id}</span>
                    <span aria-hidden="true">·</span>
                    {/* 账号 id 的掩码尾段：同名或都叫「未命名」的两个号，只有这一段能区分。 */}
                    <span
                      className="min-w-0 truncate tabular-nums"
                      title={cred.email
                        ? t(
                          `${cred.email}（账号 ${cred.account_id_masked}）`,
                          `${cred.email} (account ${cred.account_id_masked})`,
                        )
                        : t(`账号 ${cred.account_id_masked}`, `Account ${cred.account_id_masked}`)}
                    >
                      {cred.account_id_masked}
                    </span>
                    <span aria-hidden="true">·</span>
                    <span
                      className="inline-flex min-w-0 items-center gap-1"
                      title={t(
                        `添加于 ${formatFullTime(cred.created_at, language)}`,
                        `Added ${formatFullTime(cred.created_at, language)}`,
                      )}
                    >
                      <CalendarDaysIcon className="size-3 shrink-0" />
                      <span>{t(`添加于 ${added}`, `Added ${added}`)}</span>
                    </span>
                    {expiryUrgent && (
                      <span
                        className="inline-flex min-w-0 items-center gap-1 text-warning-foreground"
                        title={t(
                          'access token 即将过期；下一个请求会自动刷新，无需处理',
                          'The access token is about to expire; the next request refreshes it automatically',
                        )}
                      >
                        <ClockIcon className="size-3 shrink-0" />
                        <span>{expiry.label}</span>
                      </span>
                    )}
                  </CardDescription>
                </div>
              </div>
            )}
          </CardTitle>

          {!editing && (
            <CardAction>
              <Menu modal={false}>
                <MenuTrigger
                  className={buttonVariants({ size: 'icon', variant: 'ghost' })}
                  aria-label={t(`打开 ${credentialLabel} 菜单`, `Open menu for ${credentialLabel}`)}
                >
                  <EllipsisIcon />
                </MenuTrigger>
                <CredentialMenuContent
                  cred={cred}
                  actions={actions}
                  onRename={() => {
                    setName(cred.label)
                    setEditing(true)
                  }}
                  onRpmLimit={() => setRpmOpen(true)}
                  onProxy={() => setProxyOpen(true)}
                  onUsage={() => setUsageOpen(true)}
                  onTest={() => setTesting(true)}
                  onRequestDelete={() => setConfirmDelete(true)}
                />
              </Menu>
            </CardAction>
          )}
        </CardHeader>

        <CardPanel className="space-y-3 px-4 pb-3 sm:pb-4">
          <div className="flex flex-wrap items-center gap-2">
            {statusUsesTooltip ? (
              <Tooltip>
                <TooltipTrigger
                  className={cn(badgeVariants({ variant: status.variant }), 'cursor-help')}
                  delay={0}
                  aria-label={t(
                    `${credentialLabel}：${status.label}。${status.detail}`,
                    `${credentialLabel}: ${status.label}. ${status.detail}`,
                  )}
                  aria-live="polite"
                >
                  {status.label}
                </TooltipTrigger>
                <TooltipPopup
                  side="bottom"
                  align="start"
                  className="max-w-80 whitespace-normal break-words text-left leading-5"
                >
                  {status.detail}
                </TooltipPopup>
              </Tooltip>
            ) : (
              <Badge
                variant={status.variant}
                aria-label={t(`${credentialLabel}：${status.label}`, `${credentialLabel}: ${status.label}`)}
              >
                {status.label}
              </Badge>
            )}
            {creditsBadge && (
              <Badge variant={creditsBadge.variant} size="sm" title={creditsBadge.title}>
                {creditsBadge.label}
              </Badge>
            )}
            <Badge
              variant={planBadgeVariant(cred.plan_type)}
              size="sm"
              title={cred.plan_type
                ? t(`上游报告的套餐：${cred.plan_type}`, `Plan reported by upstream: ${cred.plan_type}`)
                : t('上游还没报告套餐档位', 'The upstream has not reported a plan yet')}
            >
              {planLabel(cred.plan_type, language)}
            </Badge>
            <Badge
              variant="outline"
              size="sm"
              title={t('调度优先级，数值越小越优先', 'Scheduling priority; lower values are scheduled first')}
            >
              P{cred.priority}
            </Badge>
            {/* 出站代理只标「有」，具体地址在悬浮提示里：地址常带账号密码，不该常驻卡面。 */}
            {cred.proxy && (
              <Badge
                variant="outline"
                size="sm"
                title={t(
                  `该账号的全部出站流量走 ${cred.proxy}`,
                  `All outbound traffic for this account goes through ${cred.proxy}`,
                )}
              >
                {t('代理', 'Proxy')}
              </Badge>
            )}
          </div>

          <section
            aria-label={t(`${credentialLabel} 的额度窗口`, `Quota windows for ${credentialLabel}`)}
            className="space-y-2"
          >
            <div className="flex flex-wrap items-start justify-between gap-x-3 gap-y-1.5">
              <h4 className="font-medium text-xs text-muted-foreground">{t('额度窗口', 'Quota windows')}</h4>
              {/* 快照可能明显早于最近一次请求（只有解出限流头的那次才更新），所以这个时刻
                  必须常驻：少了它，一个过期的 12% 会被当成现状。 */}
              {quota.snapshotTs != null ? (
                <Tooltip>
                  <TooltipTrigger
                    render={<span />}
                    className="inline-flex items-center gap-1 text-2xs text-muted-foreground"
                  >
                    <ClockIcon className="size-3" />
                    {t(
                      `更新于 ${relativeTime(quota.snapshotTs, now, language)}`,
                      `Updated ${relativeTime(quota.snapshotTs, now, language)}`,
                    )}
                  </TooltipTrigger>
                  <TooltipPopup>{snapshotTime}</TooltipPopup>
                </Tooltip>
              ) : (
                <span className="text-2xs text-muted-foreground">{t('暂无数据', 'No data')}</span>
              )}
            </div>
            {reportedWindows.length > 0 ? (
              // 只有一个窗口时不留空半格：分两列却只填一格，看起来像另一半加载失败了。
              <div
                className={cn(
                  'grid gap-3',
                  reportedWindows.length > 1 && '@sm/card:grid-cols-2 @sm/card:gap-4',
                )}
              >
                {reportedWindows.map((w) => (
                  <QuotaMeter
                    key={w.key}
                    credentialLabel={credentialLabel}
                    window={w}
                    snapshotTs={quota.snapshotTs}
                    now={now}
                  />
                ))}
              </div>
            ) : (
              <p className="text-xs text-muted-foreground">
                {quota.hasSnapshot
                  ? t('上游没有报告任何额度窗口。', 'The upstream reported no quota windows.')
                  : t('还没有额度快照，转发一次请求后出现。', 'No quota snapshot yet; it appears after the first forwarded request.')}
              </p>
            )}
          </section>
        </CardPanel>

        {/* 页脚有两套排布，而不是让一行内容自己折行：折出来的第二行长短随内容而变，
            开关又浮在两行之间，看着像挤坏了。
            数字长到窄卡片一行装不下时（见 [footerChars]）：上行「请求数 ┄ 开关」，
            下行「费用 · RPM」，两行从同一条左边线起、开关钉在右上。
            @sm/card 起（卡片列最小 27rem）宽度够，一律单行、竖线分区。 */}
        <CardFooter
          className={cn(
            'mt-auto items-center border-t bg-muted/32 px-4 py-2.5 sm:py-3 @sm/card:flex @sm/card:gap-4',
            footerStacked
              ? 'grid grid-cols-[minmax(0,1fr)_auto] gap-x-3 gap-y-1.5'
              : 'flex gap-3',
          )}
        >
          {/* 页脚这几项统一用 Tooltip 组件而不是原生 title：原生提示有约 1 秒延迟、
              触屏上完全出不来，样式也不受控，和卡片上方的状态提示不是一套东西。 */}
          <Tooltip>
            <TooltipTrigger
              className={cn(
                buttonVariants({ variant: 'ghost' }),
                // 窄卡片上按钮的横向 padding 收一半：这颗按钮是页脚最宽的一块，
                // 挤掉的每一像素都直接给右边的 RPM。
                'min-w-0 max-w-full justify-self-start justify-start gap-1.5 px-2 @sm/card:gap-2 @sm/card:px-[calc(--spacing(3)-1px)]',
              )}
              onClick={() => setUsageOpen(true)}
              aria-label={t(`查看 ${credentialLabel} 的用量明细`, `View usage details for ${credentialLabel}`)}
              aria-haspopup="dialog"
            >
              <ScrollTextIcon className="text-muted-foreground" />
              <Badge variant="secondary" size="sm" className="tabular-nums">
                {requestsText}
              </Badge>
              <span className="sr-only">{t('条已转发请求', 'forwarded requests')}</span>
            </TooltipTrigger>
            <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">
              {t(
                `经这个账号转发过 ${requests.toLocaleString(locale)} 条请求（含失败的）。点击查看逐条明细`,
                `${requests.toLocaleString(locale)} requests forwarded through this account (failures included). Click for the per-request breakdown`,
              )}
            </TooltipPopup>
          </Tooltip>

          {/* 开关在 DOM 里排第二，两行布局才能把它放进第一行右侧；单行布局下 order-last 再把它推到最右
              （order 不能在两行布局里加：网格是按 DOM 顺序自动填格的，改了顺序开关就掉到第二行去了）。 */}
          <div
            className={cn(
              'flex shrink-0 items-center gap-2 ml-auto @sm/card:order-last',
              !footerStacked && 'order-last',
            )}
          >
            {toggle.isPending && <Spinner />}
            <Switch
              checked={!cred.disabled}
              onCheckedChange={(enabled) => toggle.mutate(!enabled)}
              disabled={toggle.isPending}
              title={switchTitle(cred, language)}
              aria-label={`${credentialLabel}: ${switchTitle(cred, language)}`}
            />
          </div>

          {/* 两行布局下的左内边距对齐上一行按钮的 padding（同一个 --spacing(3)-1px），
              否则钱包图标比上面的卷轴图标突出 11px，两行读起来是错开的。 */}
          <div
            className={cn(
              'flex min-w-0 items-center gap-3 @sm/card:gap-4',
              footerStacked && 'col-span-2 pl-[calc(--spacing(3)-1px)] @sm/card:col-span-1 @sm/card:pl-0',
            )}
          >
            <Separator orientation="vertical" className="hidden h-5 @sm/card:block" />
            {/* token 数排在请求数与费用之间：三个数是同一条链上的粗细——多少条请求、
                烧了多少 token、折成多少钱，挨着放才好互相印证（`40 条 / 395 token` 一眼看出
                都是小请求）。用 formatTokens 而不是 formatCompactNumber：K/M 与价目表的
                MTok 同一量纲，中文 locale 下那个「12万」换算不过去。 */}
            <Tooltip>
              <TooltipTrigger
                render={<span />}
                className="inline-flex shrink-0 items-center gap-1.5 whitespace-nowrap text-xs"
              >
                <HashIcon className="hidden size-3.5 text-muted-foreground @sm/card:inline" aria-hidden />
                <span className="sr-only">{t('累计 token 数', 'Cumulative tokens')}</span>
                <span className="font-medium tabular-nums">{tokensText}</span>
              </TooltipTrigger>
              <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">
                {t(
                  `累计 ${tokens.toLocaleString(locale)} token：输入 ${cred.stats.input_tokens_total.toLocaleString(locale)}（其中命中缓存 ${cred.stats.cached_tokens_total.toLocaleString(locale)}）+ 输出 ${cred.stats.output_tokens_total.toLocaleString(locale)}`,
                  `${tokens.toLocaleString(locale)} tokens total: ${cred.stats.input_tokens_total.toLocaleString(locale)} input (${cred.stats.cached_tokens_total.toLocaleString(locale)} cache hits) + ${cred.stats.output_tokens_total.toLocaleString(locale)} output`,
                )}
              </TooltipPopup>
            </Tooltip>
            <Separator orientation="vertical" className="h-5" />
            <Tooltip>
              <TooltipTrigger
                render={<span />}
                className="inline-flex shrink-0 items-center gap-1.5 whitespace-nowrap text-xs"
              >
                {/* 窄卡片省掉钱包图标：`$` 已经把这串数字标成钱了，省下的宽度留给 RPM。 */}
                <WalletCardsIcon className="hidden size-3.5 text-muted-foreground @sm/card:inline" aria-hidden />
                <span className="sr-only">{t('累计等价 API 费用', 'Cumulative equivalent API cost')}</span>
                <span className="font-medium tabular-nums">{formatUsd(cred.stats.cost_total_usd)}</span>
              </TooltipTrigger>
              <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">
                {t(
                  '累计等价 API 费用：按官方 API 价目估的等价花费，不是账单——订阅模式扣的是额度',
                  'Cumulative equivalent API cost: estimated from official API rates, not a bill — a subscription spends quota, not dollars',
                )}
              </TooltipPopup>
            </Tooltip>
            {/* 常驻：闲置号看不见 RPM 的话，「这个号此刻有没有在跑」就只能靠别处推断。
                零值不喊人——点不呼吸、数字转灰，位置照占，卡片之间这一列才对得齐。 */}
            <Separator orientation="vertical" className="h-5" />
            <Tooltip>
              <TooltipTrigger
                render={<span />}
                className="inline-flex min-w-0 shrink items-center gap-2 whitespace-nowrap text-xs"
              >
                {/* 页脚里唯一的实时值（隔壁两个都是累计量），用呼吸点替掉图标把「活的」画出来。
                    绿色只落在这个 6px 点上：数值本身无好坏之分，颜色留给状态（运行正常 / 冷却）。 */}
                <span className="relative flex size-1.5 shrink-0" aria-hidden>
                  {rpmLive && (
                    <span className="absolute inline-flex size-full animate-ping rounded-full bg-success opacity-60 motion-reduce:hidden" />
                  )}
                  <span
                    className={cn(
                      'relative inline-flex size-1.5 rounded-full',
                      rpmLive ? 'bg-success' : 'bg-muted-foreground/32',
                    )}
                  />
                </span>
                <span className="sr-only">{t('当前 RPM', 'Current RPM')}</span>
                <span className="inline-flex min-w-0 items-baseline gap-1">
                  <span
                    className={cn(
                      'truncate tabular-nums',
                      rpmLive ? 'font-medium' : 'text-muted-foreground',
                      rpmFull && 'text-warning',
                    )}
                  >
                    {cred.rpm}
                    {rpmLimit > 0 && <span className="text-muted-foreground">/{rpmLimit}</span>}
                  </span>
                  <span className="shrink-0 text-2xs text-muted-foreground tracking-wide">RPM</span>
                </span>
              </TooltipTrigger>
              <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">
                {rpmLimit > 0
                  ? t(
                    `当前 RPM：最近 60 秒经这个账号转发的请求数（含失败的）。上限 ${rpmLimit} 条/分钟，打满后新请求分流到别的账号。`,
                    `Current RPM: requests forwarded through this account in the last 60 seconds (failures included). Limited to ${rpmLimit}/min; once full, new requests spill to another account.`,
                  )
                  : t(
                    '当前 RPM：最近 60 秒经这个账号转发的请求数（含失败的）',
                    'Current RPM: requests forwarded through this account in the last 60 seconds (failures included)',
                  )}
              </TooltipPopup>
            </Tooltip>
          </div>
        </CardFooter>

        {/* 没点开过任何一个就一个都不挂：账号一多，这些常关的对话框全是白挂的组件树。 */}
        <DeferredMount open={proxyOpen || rpmOpen || usageOpen || confirmDelete || testing}>
          <CredentialProxyDialog
            cred={cred}
            open={proxyOpen}
            onOpenChange={setProxyOpen}
            proxy={actions.proxy}
          />
          <CredentialRpmDialog
            cred={cred}
            open={rpmOpen}
            onOpenChange={setRpmOpen}
            rpmLimit={actions.rpmLimit}
          />
          <CredentialUsageDialog cred={cred} open={usageOpen} onOpenChange={setUsageOpen} />
          <DeleteCredentialDialog
            cred={cred}
            open={confirmDelete}
            onOpenChange={setConfirmDelete}
            onConfirm={() => remove.mutate()}
            pending={remove.isPending}
          />
          <ConnectivityTestDialog cred={cred} open={testing} onOpenChange={setTesting} />
        </DeferredMount>
      </Card>
    </li>
  )
})

/**
 * 窗口用量里的一个事实：一枚小徽章，值在里面、单位在后面，全称与精确值进悬浮提示。
 *
 * 用 `dt`/`dd` 而不是两个 span：标签本身在界面上是省掉的（三个数各带各的单位/符号，
 * 一眼分得清），但读屏得念得出「请求数 399」而不是光一个「399」。
 */
function QuotaFact({
  label,
  value,
  suffix,
  hint,
}: {
  label: string
  value: string
  suffix?: string
  /** 提示里跟在标签后面的明细（精确值、口径说明）；不传则只显示标签。 */
  hint?: string
}) {
  const { t } = useI18n()
  return (
    <Tooltip>
      <TooltipTrigger
        render={<div />}
        delay={0}
        className={cn(
          badgeVariants({ variant: 'secondary', size: 'sm' }),
          'min-w-0 gap-0.5 font-normal',
        )}
      >
        <dt className="sr-only">{label}</dt>
        <dd className="truncate tabular-nums">{value}</dd>
        {suffix && <span className="text-muted-foreground" aria-hidden>{suffix}</span>}
      </TooltipTrigger>
      <TooltipPopup className="max-w-72 whitespace-normal break-words text-left leading-5">
        {hint ? t(`${label}：${hint}`, `${label}: ${hint}`) : label}
      </TooltipPopup>
    </Tooltip>
  )
}

/** 窗口标签的固定配色：分类色，与占用无关——主额度一色、次额度一色。 */
const WINDOW_VARIANT: Record<QuotaWindowMeta['key'], BadgeProps['variant']> = {
  primary: 'info',
  secondary: 'success',
}

function QuotaMeter({
  credentialLabel,
  window: w,
  snapshotTs,
  now,
}: {
  credentialLabel: string
  window: QuotaWindowMeta
  snapshotTs: number | null
  /** 页面时钟（30 秒一跳），倒计时靠它走，见 [formatCountdown]。 */
  now: number
}) {
  const { t, language, locale } = useI18n()
  const label = quotaWindowLabel(w, language)
  // 窗口过了重置点，上游那份使用率就作废了（[evaluateWindow] 把 percentage 抹成 null），
  // 此时这个窗口的用量确实归了零——直接按 0% 画，不再单独摆一句「已重置 / 暂无数据」：
  // 那句话占着和数据一样大的地方，说的却只是「这里没什么可看」。
  const percentage = w.percentage ?? 0
  const indicatorClass = w.level === 'critical'
    ? 'bg-destructive'
    : w.level === 'warning'
      ? 'bg-warning'
      : 'bg-success'
  const valueClass = w.level === 'critical'
    ? 'text-destructive'
    : w.level === 'warning'
      ? 'text-warning-foreground'
      : 'text-foreground'

  return (
    <Meter value={percentage} max={100} className="gap-1.5">
      {/* 数据先行、进度条随后：三个事实是「这个窗口里发生了什么」，百分比是「还剩多少」。
          分两行排而不是挤成一行——挤在一行时标签与数值交替出现，眼睛得逐个配对。 */}
      {w.usage && (
        <dl className="flex min-w-0 flex-wrap items-center gap-1">
          <QuotaFact
            label={t('请求数', 'Requests')}
            value={formatCompactNumber(w.usage.requests, locale)}
            hint={w.usage.requests.toLocaleString(locale)}
            suffix="req"
          />
          {/* 费用是按价目表估的、token 是上游实报的，两个数**不成正比**：命中缓存的输入按
              十分之一计价，重度吃缓存的号「token 一大堆、花费很少」。所以两项并列而不是
              只留其中一个。不带 `tok` 后缀：`65.7M` 的量纲一眼就是 token（隔壁一个带 req、
              一个带 $），那三个字母只会把这行本就不宽的地方再挤掉一截。 */}
          <QuotaFact
            label={t('总 token', 'Total tokens')}
            value={formatTokens(w.usage.tokens)}
            hint={t(
              `${w.usage.tokens.toLocaleString(locale)}（输入 + 输出，上游 usage 口径；输入已含命中缓存的部分、输出已含 reasoning，不重复计）`,
              `${w.usage.tokens.toLocaleString(locale)} (input + output per the upstream usage fields; input already includes cache hits and output already includes reasoning, so nothing is double counted)`,
            )}
          />
          <QuotaFact
            label={t('等价费用', 'Equivalent cost')}
            value={formatUsd(w.usage.cost_usd)}
            hint={t(
              '按官方 API 价目估的等价花费，不是账单——订阅模式扣的是额度。价目表认不出的模型记 0，所以这是下限',
              'Estimated from official API rates, not a bill — a subscription spends quota. Models missing from the price table count as 0, so this is a lower bound',
            )}
          />
        </dl>
      )}
      <div className="flex min-w-0 items-center gap-1.5">
        {/* 窗口名做成固定色的小标签（主 / 次各一色）：它是分类而不是状态，配色跟右边那组
            表示占用的红黄绿分开，两侧各管一件事。 */}
        <MeterLabel
          className={cn(
            badgeVariants({ variant: WINDOW_VARIANT[w.key], size: 'sm' }),
            'shrink-0 tabular-nums',
          )}
        >
          <span className="sr-only">{t(`${credentialLabel} 的 `, `${credentialLabel} `)}</span>
          {label}
          <span className="sr-only">{t('用量', 'usage')}</span>
        </MeterLabel>
        <MeterTrack className="h-1.5 min-w-6 flex-1 rounded-full">
          <MeterIndicator className={cn(indicatorClass, 'rounded-full')} />
        </MeterTrack>
        <MeterValue
          className={cn('shrink-0 font-medium text-xs', valueClass)}
          title={snapshotTs != null
            ? t(`快照于 ${formatFullTime(snapshotTs, language)}`, `Snapshot at ${formatFullTime(snapshotTs, language)}`)
            : undefined}
        >
          {() => `${percentage}%`}
        </MeterValue>
        {/* 距离重置还有多久。倒计时靠页面那个 30 秒 tick 走（见 useNowSeconds），不会冻住；
            精确到分秒的绝对时刻放在 title 里——倒计时受本地时钟偏差影响，只适合看个大概。 */}
        {w.resetAt != null && w.resetAt > now && (
          <span
            className="shrink-0 whitespace-nowrap text-2xs text-muted-foreground tabular-nums"
            title={t(`${formatFullTime(w.resetAt, language)} 重置`, `Resets ${formatFullTime(w.resetAt, language)}`)}
          >
            {formatCountdown(w.resetAt, now)}
          </span>
        )}
      </div>
    </Meter>
  )
}
