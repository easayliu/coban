import { memo, useState } from 'react'
import {
  CalendarDaysIcon,
  CheckIcon,
  ClockIcon,
  EllipsisIcon,
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
  formatFullTime,
  formatUsd,
  relativeTime,
} from '@/lib/utils'
import {
  accountIdTitle,
  ConnectivityTestDialog,
  credentialInitial,
  CredentialMenuContent,
  DeferredMount,
  DeleteCredentialDialog,
  ResetQuotaDialog,
  resetCreditsMeta,
  credentialExpiryMeta,
  evaluateCredential,
  isRangeSelect,
  planBadgeVariant,
  planLabel,
  proxyTitle,
  QuotaMeter,
  switchTitle,
  useCredentialActions,
} from '@/components/credential-shared'
import { CredentialProxyDialog } from '@/components/credential-proxy-dialog'
import { CredentialRpmDialog } from '@/components/credential-rpm-dialog'
import { CredentialUsageDialog } from '@/components/credential-usage-dialog'
import { Avatar, AvatarFallback } from '@/components/ui/avatar'
import { Badge, badgeVariants } from '@/components/ui/badge'
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
import { Separator } from '@/components/ui/separator'
import { Spinner } from '@/components/ui/spinner'
import { Switch } from '@/components/ui/switch'
import { HINT_FOCUS_RING, Tooltip, TooltipPopup, TooltipTrigger } from '@/components/ui/tooltip'

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
  /**
   * 收 id 而不是每张卡现做一个闭包，回调引用才能稳定，memo 才拦得住重渲染。
   *
   * `extend`：按着 shift 点的，勾选从锚点到这一张之间整段（见 credential-workspace 的
   * `toggleSelected`）。
   */
  onSelectedChange?: (id: number, next: boolean, extend?: boolean) => void
}) {
  const { t, language, locale } = useI18n()
  const [editing, setEditing] = useState(false)
  const [name, setName] = useState(cred.label)
  const [proxyOpen, setProxyOpen] = useState(false)
  const [rpmOpen, setRpmOpen] = useState(false)
  const [usageOpen, setUsageOpen] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [confirmReset, setConfirmReset] = useState(false)
  const [testing, setTesting] = useState(false)

  const actions = useCredentialActions(cred, () => setEditing(false))
  const { rename, toggle, remove, consumeReset } = actions
  const { quota, status } = evaluateCredential(cred, now, language)
  const credentialLabel = displayCredentialLabel(cred.label, language)
  const initial = credentialInitial(credentialLabel)
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
  const requestsText = formatCompactNumber(requests)
  // 页脚三组数字（请求数 / 累计费用 / RPM）在窄卡片上排一行还是两行，按字符数定。
  // 这与 Luban 的卡片布局保持一致：超出窄屏容量时，上行「请求数 · 开关」、下行「费用 · RPM」。
  const footerChars = requestsText.length
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
  // 重置券：**只有真的有券时才挂徽章**。没查过、查过没券都不摆——理由同 credits 那枚
  // （见下一段），把「还没问」或者「常态」说成一种状态，只是在抢注意力。张数与过期时刻的
  // 读法在 resetCreditsMeta 里，卡片和确认框共用同一份。
  const resetCredits = resetCreditsMeta(cred.stats?.reset_credits ?? null, language)
  const resetBadge = resetCredits?.state === 'available' ? resetCredits : null
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
                    onCheckedChange={(checked, details) => onSelectedChange?.(
                      cred.id,
                      checked,
                      isRangeSelect(details.event),
                    )}
                    aria-label={t(`选择 ${credentialLabel}`, `Select ${credentialLabel}`)}
                  />
                )}
                <Avatar className="hidden @sm/card:flex" aria-hidden="true">
                  <AvatarFallback>{initial}</AvatarFallback>
                </Avatar>
                <div className="min-w-0 flex-1">
                  {/* 标题即主操作：点开这个号的用量明细。业界的卡片规范是「一张卡一个明确的
                      主操作」，而这张卡原来的详情入口是页脚一颗卷轴图标，标题反倒不可点——
                      最显眼的东西不承担最常做的事。页脚那颗按钮随之退成纯数字（见页脚那段注），
                      所以通往用量弹窗的**按钮**仍然只有一个，只是换到了该在的位置。 */}
                  <h3 id={titleId} className="min-w-0">
                    <button
                      type="button"
                      onClick={() => setUsageOpen(true)}
                      aria-haspopup="dialog"
                      title={credentialLabel}
                      className={cn(
                        'block min-w-0 max-w-full cursor-pointer truncate whitespace-nowrap text-left leading-snug',
                        'hover:underline',
                        HINT_FOCUS_RING,
                      )}
                    >
                      {credentialLabel}
                    </button>
                  </h3>
                  {/* 元信息**整行是一个提示触发区**，不是每一项各挂一个。
                      原来这里是三四个原生 `title` 各说一句：键盘用户一个都拿不到（原生提示只认
                      鼠标悬停），样式也和卡上其余提示不是一套。逐项换成 Tooltip 的话每张卡要多
                      两三个焦点位——而这一行讲的本来就是同一件事「这个号是谁、什么时候加的、
                      怎么出网」，一次给全反而比拆开更好读，焦点位也比原来还少一个。 */}
                  <Tooltip>
                    <TooltipTrigger
                      render={<CardDescription tabIndex={0} />}
                      // 窄卡片（手机，卡片宽度不到 @sm/card）上退回 text-xs：text-2xs 是 11px，
                      // 还叠着 muted 前景色，正好压在「次要文字最小尺寸」的下限上（iOS HIG 11pt、
                      // Material labelSmall 11sp 都是底线）。手机上这一行是唯一写着账号 id 与
                      // 添加时间的地方，不该是全页最小的字。卡片宽起来（桌面多列）再收回 11px，
                      // 那时它旁边有足够留白，密度比字号重要。
                      className={cn(
                        'mt-1 flex min-w-0 flex-wrap items-center gap-x-1.5 gap-y-0.5 font-normal',
                        'text-xs @sm/card:text-2xs',
                        HINT_FOCUS_RING,
                      )}
                    >
                      <span className="tabular-nums">#{cred.id}</span>
                      <span aria-hidden="true">·</span>
                      {/* 账号 id 的掩码尾段：同名或都叫「未命名」的两个号，只有这一段能区分。 */}
                      <span className="min-w-0 truncate tabular-nums">{cred.account_id_masked}</span>
                      <span aria-hidden="true">·</span>
                      <span className="inline-flex min-w-0 items-center gap-1">
                        <CalendarDaysIcon className="size-3 shrink-0" />
                        <span>{t(`添加于 ${added}`, `Added ${added}`)}</span>
                      </span>
                      {/* 出站代理只标「有」，地址在提示里：地址常带账号密码，不该常驻卡面。
                          与列表视图同一个位置、同一份说法（见 credential-row 的身份格）。 */}
                      {cred.proxy && (
                        <>
                          <span aria-hidden="true">·</span>
                          <span className="shrink-0">{t('代理', 'proxy')}</span>
                        </>
                      )}
                      {expiryUrgent && (
                        <span className="inline-flex min-w-0 items-center gap-1 text-warning-foreground">
                          <ClockIcon className="size-3 shrink-0" />
                          <span>{expiry.label}</span>
                        </span>
                      )}
                    </TooltipTrigger>
                    {/* 顺序与上面那行逐项对应，读起来才对得上。 */}
                    <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">
                      <span className="block">{accountIdTitle(cred, language)}</span>
                      <span className="block">
                        {t(
                          `添加于 ${formatFullTime(cred.created_at, language)}`,
                          `Added ${formatFullTime(cred.created_at, language)}`,
                        )}
                      </span>
                      {cred.proxy && (
                        <span className="block">{proxyTitle(cred.proxy, language)}</span>
                      )}
                      {expiryUrgent && (
                        <span className="block">
                          {t(
                            'access token 即将过期；下一个请求会自动刷新，无需处理',
                            'The access token is about to expire; the next request refreshes it automatically',
                          )}
                        </span>
                      )}
                    </TooltipPopup>
                  </Tooltip>
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
                  onRequestReset={() => setConfirmReset(true)}
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
                  // 不挂 aria-live：一屏二十张卡，每张都成了活区域，任一个号的状态一变
                  // （30 秒一轮刷新，冷却秒数、额度百分比都在动）读屏就打断用户念一遍。
                  // 状态本身有 aria-label，用户主动读到这张卡时拿得到，不需要它自己喊。
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
            {/* 这一行**按「会不会自己变」排**，不是随手堆：状态（默认字号、抢眼）→ 重置券 /
                credits（有时效，会消失）→ 套餐 / 优先级（sm 字号，属性）。代理原来也在这里，
                已经挪进标题下面那行元信息——它跟 `#3 · …9f31d0` 是一类东西，而且列表视图
                本来就把它放在那一行，两个视图这才对得上。

                说明文字原来挂在原生 `title` 上，键盘用户拿不到（原生提示只认鼠标悬停），而同一
                张卡的页脚早就换成 Tooltip 组件了——两套规则并存说不通。统一走 Tooltip：不传
                `render` 时它渲染成 `<button>`，既进焦点序列，又自带 badgeVariants 里那圈焦点环
                和粗指针下的 44px 热区。代价是每枚徽章占一个焦点位，所以这一行只留真的要解释的
                东西（套餐那枚给的是上游原始串，优先级那枚讲的是调度规则）。 */}
            {resetBadge && (
              <Tooltip>
                <TooltipTrigger className={cn(badgeVariants({ variant: 'success', size: 'sm' }), 'cursor-help')}>
                  {resetBadge.label}
                </TooltipTrigger>
                <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">
                  {resetBadge.title}
                </TooltipPopup>
              </Tooltip>
            )}
            {creditsBadge && (
              <Tooltip>
                <TooltipTrigger
                  className={cn(badgeVariants({ variant: creditsBadge.variant, size: 'sm' }), 'cursor-help')}
                >
                  {creditsBadge.label}
                </TooltipTrigger>
                <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">
                  {creditsBadge.title}
                </TooltipPopup>
              </Tooltip>
            )}
            <Tooltip>
              <TooltipTrigger
                className={cn(
                  badgeVariants({ variant: planBadgeVariant(cred.plan_type), size: 'sm' }),
                  'cursor-help',
                )}
              >
                {planLabel(cred.plan_type, language)}
              </TooltipTrigger>
              <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">
                {cred.plan_type
                  ? t(`上游报告的套餐：${cred.plan_type}`, `Plan reported by upstream: ${cred.plan_type}`)
                  : t('上游还没报告套餐档位', 'The upstream has not reported a plan yet')}
              </TooltipPopup>
            </Tooltip>
            <Tooltip>
              <TooltipTrigger className={cn(badgeVariants({ variant: 'outline', size: 'sm' }), 'cursor-help')}>
                P{cred.priority}
              </TooltipTrigger>
              <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">
                {t('调度优先级，数值越小越优先', 'Scheduling priority; lower values are scheduled first')}
              </TooltipPopup>
            </Tooltip>
            {/* 代理搬到了标题下面那行元信息里（见上面那段注）：它是**属性**，不是状态。 */}
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
                    render={<span tabIndex={0} />}
                    // 字号跟着上面那行元信息走（同一档次要文字），见标题下面那段注。
                    className={cn(
                      'inline-flex items-center gap-1 text-xs @sm/card:text-2xs text-muted-foreground',
                      HINT_FOCUS_RING,
                    )}
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
                <span className="text-xs @sm/card:text-2xs text-muted-foreground">{t('暂无数据', 'No data')}</span>
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
                    usage="facts"
                    showCountdown
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
            开关又浮在两行之间，看着像挤坏了。数字放不下时上行「请求数 · 开关」、下行「费用 · RPM」；
            @sm/card 起卡片宽度足够，再恢复单行布局。 */}
        <CardFooter
          className={cn(
            'mt-auto items-center border-t bg-muted/32 px-4 py-2.5 sm:py-3 @sm/card:flex @sm/card:gap-4',
            footerStacked
              ? 'grid grid-cols-[minmax(0,1fr)_auto] gap-x-3 gap-y-1.5'
              : 'flex gap-3',
          )}
        >
          {/* 页脚这几项统一用 Tooltip 组件而不是原生 title：原生提示有约 1 秒延迟、样式不受控，
              和卡片上方的状态提示不是一套东西（触屏两者都出不来，见 [HINT_FOCUS_RING]）。

              请求数原来是一颗 ghost 按钮，点了开用量明细——那个入口已经搬到标题上了。这里退成
              和隔壁费用 / RPM 一样的「图标 + 数字 + 提示」，三项长得一样才好互相比对，也不再
              有两个按钮通向同一个弹窗。 */}
          <Tooltip>
            <TooltipTrigger
              render={<span tabIndex={0} />}
              className={cn(
                'inline-flex min-w-0 max-w-full shrink items-center gap-1.5 justify-self-start whitespace-nowrap text-xs',
                HINT_FOCUS_RING,
              )}
            >
              <ScrollTextIcon className="size-3.5 shrink-0 text-muted-foreground" aria-hidden />
              <span className="sr-only">{t('已转发请求', 'Forwarded requests')}</span>
              <span className="truncate font-medium tabular-nums">{requestsText}</span>
            </TooltipTrigger>
            <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">
              {t(
                `经这个账号转发过 ${requests.toLocaleString(locale)} 条请求（含失败的）。点标题看逐条明细`,
                `${requests.toLocaleString(locale)} requests forwarded through this account (failures included). Click the title for the per-request breakdown`,
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

          {/* 两行布局下不再补左内边距：上一行那颗按钮退成纯文字之后，两行的图标本来就落在
              同一条线上（原来要补 --spacing(3)-1px 是为了对齐按钮自己的横向 padding）。 */}
          <div
            className={cn(
              'flex min-w-0 items-center gap-3 @sm/card:gap-4',
              footerStacked && 'col-span-2 @sm/card:col-span-1',
            )}
          >
            <Separator orientation="vertical" className="hidden h-5 @sm/card:block" />
            <Tooltip>
              <TooltipTrigger
                render={<span tabIndex={0} />}
                className={cn('inline-flex shrink-0 items-center gap-1.5 whitespace-nowrap text-xs', HINT_FOCUS_RING)}
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
                render={<span tabIndex={0} />}
                className={cn('inline-flex min-w-0 shrink items-center gap-2 whitespace-nowrap text-xs', HINT_FOCUS_RING)}
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
        <DeferredMount open={proxyOpen || rpmOpen || usageOpen || confirmDelete || confirmReset || testing}>
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
          <ResetQuotaDialog
            cred={cred}
            open={confirmReset}
            onOpenChange={setConfirmReset}
            onConfirm={() => consumeReset.mutate(undefined, { onSettled: () => setConfirmReset(false) })}
            pending={consumeReset.isPending}
          />
          <ConnectivityTestDialog cred={cred} open={testing} onOpenChange={setTesting} />
        </DeferredMount>
      </Card>
    </li>
  )
})
