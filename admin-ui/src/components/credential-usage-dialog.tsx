import { useRef, useState } from 'react'
import { keepPreviousData, useQuery, useQueryClient } from '@tanstack/react-query'
import { RefreshCwIcon, ScrollTextIcon } from 'lucide-react'
import { listCredentialUsage, type Credential, type UsageLog } from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import { useMediaQuery } from '@/lib/use-media-query'
import {
  cacheHitRate,
  cn,
  displayCredentialLabel,
  extractError,
  formatFullTime,
  formatPercent,
  formatUsd,
} from '@/lib/utils'
import { cacheReasonHint, cacheReasonLabel } from '@/components/cache-reason-breakdown'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Avatar, AvatarFallback } from '@/components/ui/avatar'
import { Badge, type BadgeProps } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  Dialog,
  DialogClose,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogPanel,
  DialogPopup,
  DialogTitle,
} from '@/components/ui/dialog'
import {
  Empty,
  EmptyDescription,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
} from '@/components/ui/empty'
import {
  Pagination,
  PaginationContent,
  PaginationItem,
  PaginationNext,
  PaginationPrevious,
} from '@/components/ui/pagination'
import {
  Select,
  SelectItem,
  SelectPopup,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { Skeleton } from '@/components/ui/skeleton'
import { Spinner } from '@/components/ui/spinner'
import {
  Table,
  TableBody,
  TableCaption,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table'

/** 每页条数可选值。后端上限 200。 */
const PAGE_SIZES = [25, 50, 100] as const

/** 状态码 → 徽章配色。2xx 成功、429 单独一档（额度问题，不是错误），其余 4xx/5xx 红。 */
function statusVariant(status: number): BadgeProps['variant'] {
  if (status >= 200 && status < 300) return 'success'
  if (status === 429) return 'warning'
  if (status >= 400) return 'error'
  return 'secondary'
}

/**
 * 流水时间：日期 + 到秒的时钟。
 *
 * 这里不用 `relativeTime`——明细是按秒排布的，一屏「3 分钟前」看不出先后；也不用
 * `formatFullTime`，那个只到分钟，同一分钟内的连发请求会显示成同一时刻。
 */
function logTime(unixSecs: number): string {
  const d = new Date(unixSecs * 1000)
  const p = (n: number) => String(n).padStart(2, '0')
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`
}

/** 数字列的空值：**缺失**（没嗅探到 usage）与 0 是两回事，缺失显示 `—`。 */
function num(v: number | null | undefined, locale: string): string {
  return v == null ? '—' : v.toLocaleString(locale)
}

export function CredentialUsageDialog({
  cred,
  open,
  onOpenChange,
}: {
  cred: Credential
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const { t, language, locale } = useI18n()
  const qc = useQueryClient()
  const credentialLabel = displayCredentialLabel(cred.label, language)
  const titleRef = useRef<HTMLHeadingElement>(null)
  const [pageSize, setPageSize] = useState<number>(PAGE_SIZES[0])
  const [page, setPage] = useState(0)
  /**
   * 本轮翻页的锚点（首次响应给出，之后每页原样带回）。
   *
   * 放 ref 而不是 state，也不进 queryKey：它在一轮翻页里恒定，进 key 只会让第一页在拿到
   * 锚点后再白白重取一次。整轮钉住同一个快照，翻页期间新到的请求不会把记录往后挤。
   */
  const anchor = useRef<number | null>(null)
  const wideEnoughForTable = useMediaQuery('(min-width: 64rem)')

  const usage = useQuery({
    queryKey: ['credential-usage', cred.id, page, pageSize],
    queryFn: async () => {
      const res = await listCredentialUsage(cred.id, {
        limit: pageSize,
        offset: page * pageSize,
        until: anchor.current ?? undefined,
      })
      if (anchor.current == null) anchor.current = res.anchor
      return res
    },
    enabled: open,
    // 翻页时先留着上一页，避免表格整块闪成骨架屏。
    placeholderData: keepPreviousData,
  })

  const total = usage.data?.total ?? 0
  // 这一段流水（近 30 天、锚点之内）的缓存命中率：**按 token 加权**，不是各条请求命中率的
  // 平均——后者会让一条 300 token 的小请求与一条 17K 前缀的请求等权。分母含命中部分，
  // 见 cacheHitRate 的注。
  const pageCacheRate = cacheHitRate(
    usage.data?.total_input_tokens,
    usage.data?.total_cached_tokens,
  )
  const totalPages = Math.max(1, Math.ceil(total / pageSize))
  const rows = usage.data?.logs ?? []
  // 页码越界（改了每页条数、或刷新后记录变少）时退回最后一页，而不是显示一页空白。
  const currentPage = Math.min(page, totalPages - 1)
  if (currentPage !== page) setPage(currentPage)
  const firstIndex = currentPage * pageSize + 1
  const lastIndex = currentPage * pageSize + rows.length
  const retentionNoteId = `credential-usage-retention-${cred.id}`

  /** 重新取一轮：丢掉锚点回到第一页，于是能看到刚发生的请求。 */
  const reload = () => {
    anchor.current = null
    setPage(0)
    void qc.invalidateQueries({ queryKey: ['credential-usage', cred.id] })
  }

  const handleOpenChange = (next: boolean) => {
    // 关掉即重置，下次打开是新的一轮（新锚点、第一页）。
    if (!next) {
      anchor.current = null
      setPage(0)
    }
    onOpenChange(next)
  }

  const status = usage.isPending
    ? { label: t('正在读取', 'Loading'), variant: 'secondary' as const }
    : usage.error
      ? { label: t('读取失败', 'Failed to load'), variant: 'error' as const }
      : {
          label: t(`共 ${total.toLocaleString(locale)} 条`, `${total.toLocaleString(locale)} total`),
          variant: 'info' as const,
        }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogPopup className="max-w-6xl" initialFocus={titleRef}>
        <DialogHeader className="border-b bg-muted/32 p-4 sm:p-5">
          <div className="flex items-center gap-3 pr-8">
            <Avatar>
              <AvatarFallback><ScrollTextIcon /></AvatarFallback>
            </Avatar>
            <div className="min-w-0 flex-1">
              <div className="flex flex-wrap items-center gap-2">
                <DialogTitle ref={titleRef} tabIndex={-1}>{t('请求明细', 'Request log')}</DialogTitle>
                <Badge variant={status.variant} aria-live="polite">{status.label}</Badge>
                {usage.isFetching && !usage.isPending && <Spinner />}
              </div>
              <DialogDescription className="mt-1 flex min-w-0 items-center gap-1.5">
                <span className="truncate" title={credentialLabel}>{credentialLabel}</span>
                <span aria-hidden="true">·</span>
                <span className="shrink-0 tabular-nums">#{cred.id}</span>
              </DialogDescription>
            </div>
          </div>
        </DialogHeader>

        <DialogPanel className="space-y-3 p-4 pt-3 sm:p-5 sm:pt-3">
          <section className="grid gap-2 rounded-xl border bg-muted/32 px-3 py-2.5 sm:grid-cols-[auto_auto_minmax(0,1fr)] sm:items-center sm:gap-5 sm:px-4">
            <div className="flex items-baseline justify-between gap-4 sm:block">
              <p className="text-2xs font-medium text-muted-foreground">
                {t('近 30 天明细花费', 'Request cost, last 30 days')}
              </p>
              <p className="font-semibold text-sm tabular-nums sm:mt-0.5">
                {usage.data ? formatUsd(usage.data.total_cost) : '—'}
              </p>
            </div>
            {/* 缓存命中率挨着花费放：命中的输入按十分之一计价，两个数是同一件事的两面——
                「花得少」到底是请求少还是缓存吃得好，只有并排看才分得清。 */}
            <div className="flex items-baseline justify-between gap-4 sm:block">
              <p className="text-2xs font-medium text-muted-foreground">
                {t('近 30 天缓存命中率', 'Cache hit rate, last 30 days')}
              </p>
              <p
                className="font-semibold text-sm tabular-nums sm:mt-0.5"
                title={usage.data
                  ? t(
                      `命中缓存 ${usage.data.total_cached_tokens.toLocaleString(locale)} / 输入 ${usage.data.total_input_tokens.toLocaleString(locale)} token（按 token 加权，不是各条请求命中率的平均）`,
                      `${usage.data.total_cached_tokens.toLocaleString(locale)} cached of ${usage.data.total_input_tokens.toLocaleString(locale)} input tokens (token-weighted, not an average of per-request rates)`,
                    )
                  : undefined}
              >
                {formatPercent(pageCacheRate)}
              </p>
            </div>
            <p id={retentionNoteId} className="min-w-0 text-2xs leading-4 text-muted-foreground sm:text-right">
              {t(
                '流水仅保留最近 30 天；卡片上的累计花费来自历史账本，因此两者无需相等。',
                'Logs are retained for 30 days; the card uses the lifetime ledger, so the totals are not expected to match.',
              )}
            </p>
          </section>

          {usage.isPending ? (
            <div
              className="space-y-2"
              role="status"
              aria-label={t('正在读取请求明细', 'Loading request log')}
            >
              {Array.from({ length: 6 }, (_, index) => (
                <Skeleton key={index} className="h-9 w-full" />
              ))}
            </div>
          ) : usage.error ? (
            <Alert variant="error">
              <AlertTitle>{t('请求明细读取失败', 'Failed to load request log')}</AlertTitle>
              <AlertDescription>
                <p className="break-words">{extractError(usage.error, language)}</p>
                <Button
                  type="button"
                  size="sm"
                  variant="destructive-outline"
                  onClick={() => { void usage.refetch() }}
                >
                  <RefreshCwIcon />
                  {t('重试', 'Retry')}
                </Button>
              </AlertDescription>
            </Alert>
          ) : total === 0 ? (
            <Empty className="py-10">
              <EmptyHeader>
                <EmptyMedia variant="icon"><ScrollTextIcon /></EmptyMedia>
                <EmptyTitle className="text-base">{t('暂无请求记录', 'No requests yet')}</EmptyTitle>
                <EmptyDescription>
                  {t(
                    '此账号转发一次请求后就会出现在这里。',
                    'Requests forwarded through this account will show up here.',
                  )}
                </EmptyDescription>
              </EmptyHeader>
            </Empty>
          ) : (
            <>
              {/* 十一列的宽表在窄屏只能横向拖着看，等于没法用；lg 以下换成一条一张的堆叠卡片。
                  这里用媒体查询二选一而不是 CSS 隐藏：一页最多 100 条，两套都建出来是双倍节点。 */}
              {wideEnoughForTable ? (
                <UsageTable
                  rows={rows}
                  credentialLabel={credentialLabel}
                  descriptionId={retentionNoteId}
                  loading={usage.isFetching}
                />
              ) : (
                <UsageCards
                  rows={rows}
                  credentialLabel={credentialLabel}
                  loading={usage.isFetching}
                />
              )}

              <div className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-3 border-t pt-3 text-xs sm:grid-cols-[minmax(0,1fr)_auto_minmax(0,1fr)]">
                <p className="min-w-0 text-muted-foreground tabular-nums">
                  {t(
                    `第 ${firstIndex}–${lastIndex} 条，共 ${total.toLocaleString(locale)} 条`,
                    `${firstIndex}–${lastIndex} of ${total.toLocaleString(locale)}`,
                  )}
                </p>
                <div className="row-start-1 flex items-center gap-2 justify-self-end sm:col-start-3">
                  <span className="whitespace-nowrap text-muted-foreground">{t('每页', 'Per page')}</span>
                  <Select
                    items={PAGE_SIZES.map((size) => ({ value: size, label: String(size) }))}
                    value={pageSize}
                    onValueChange={(value) => {
                      if (value == null) return
                      // 每页条数一变，原来的页码就没有意义了，回到第一页。
                      setPageSize(Number(value))
                      setPage(0)
                    }}
                  >
                    <SelectTrigger size="sm" aria-label={t('每页条数', 'Rows per page')}>
                      <SelectValue />
                    </SelectTrigger>
                    <SelectPopup>
                      {PAGE_SIZES.map((size) => (
                        <SelectItem key={size} value={size}>{size}</SelectItem>
                      ))}
                    </SelectPopup>
                  </Select>
                </div>

              {totalPages > 1 && (
                <Pagination className="col-span-2 row-start-2 justify-center sm:col-span-1 sm:col-start-2 sm:row-start-1">
                  <PaginationContent>
                    <PaginationItem>
                      <PaginationPrevious
                        render={<Button variant="ghost" disabled={usage.isFetching || currentPage === 0} />}
                        aria-disabled={usage.isFetching || currentPage === 0}
                        onClick={() => {
                          setPage((current) => Math.max(0, current - 1))
                        }}
                      />
                    </PaginationItem>
                    <PaginationItem>
                      <span className="whitespace-nowrap px-2 text-foreground text-xs tabular-nums" aria-live="polite">
                        {t(
                          `第 ${currentPage + 1} / ${totalPages} 页`,
                          `Page ${currentPage + 1} of ${totalPages}`,
                        )}
                      </span>
                    </PaginationItem>
                    <PaginationItem>
                      <PaginationNext
                        render={<Button variant="ghost" disabled={usage.isFetching || currentPage >= totalPages - 1} />}
                        aria-disabled={usage.isFetching || currentPage >= totalPages - 1}
                        onClick={() => {
                          setPage((current) => Math.min(totalPages - 1, current + 1))
                        }}
                      />
                    </PaginationItem>
                  </PaginationContent>
                </Pagination>
              )}
              </div>
            </>
          )}
        </DialogPanel>

        <DialogFooter className="px-4 py-3 sm:px-5">
          <Button
            type="button"
            variant="outline"
            className="mr-auto"
            disabled={usage.isFetching}
            onClick={reload}
            title={t('回到第一页并拉取最新记录', 'Jump back to the first page and fetch the newest records')}
          >
            <RefreshCwIcon className={usage.isFetching ? 'animate-spin' : undefined} />
            {t('刷新', 'Refresh')}
          </Button>
          <DialogClose render={<Button variant="outline" />}>{t('关闭', 'Close')}</DialogClose>
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  )
}

/**
 * 窄屏下的请求明细：一条请求一张卡片。
 *
 * 字段顺序按排查时的读法排：先看什么时候、成没成、花了多少，再看模型与 token，
 * 最后才是耗时和来源。UA 只留一行截断——真要看全的场景基本都在桌面端。
 */
function UsageCards({
  rows,
  credentialLabel,
  loading,
}: {
  rows: UsageLog[]
  credentialLabel: string
  loading: boolean
}) {
  const { t, language, locale } = useI18n()
  const ms = (v: number | null) => (v == null ? '—' : `${v.toLocaleString(locale)}ms`)

  return (
    <ul
      className="max-h-[26rem] space-y-2 overflow-y-auto overscroll-contain"
      aria-label={t(`${credentialLabel} 的请求明细`, `Request log for ${credentialLabel}`)}
      aria-busy={loading}
    >
      {rows.map((log) => {
        // 会话 id 是个 UUID，整条显示会把这一格撑破；取前 8 位足够在一屏里区分，
        // 完整值挂在 title 上。
        const sessionShort = log.session_id ? log.session_id.slice(0, 8) : '—'
        return (
          <li key={log.id} className="rounded-lg border bg-card px-3 py-2.5 text-xs">
            <div className="flex min-w-0 items-center gap-2">
              <span className="shrink-0 font-medium tabular-nums" title={formatFullTime(log.ts, language)}>
                {logTime(log.ts)}
              </span>
              <Badge variant={statusVariant(log.status)} size="sm" className="tabular-nums">
                {log.status}
              </Badge>
              <span
                className={cn(
                  'ml-auto shrink-0 font-medium tabular-nums',
                  log.cost_usd == null && 'font-normal text-muted-foreground',
                )}
              >
                {log.cost_usd == null ? '—' : formatUsd(log.cost_usd)}
              </span>
            </div>
            <p className="mt-1 truncate text-muted-foreground" title={log.model ?? undefined}>
              {log.model ?? '—'}
            </p>
            <dl className="mt-2 grid grid-cols-2 gap-x-4 gap-y-1.5">
              <LogFact label={t('输入 / 输出', 'In / out')}>
                {num(log.input_tokens, locale)} / {num(log.output_tokens, locale)}
              </LogFact>
              <LogFact label={t('缓存 / 推理', 'Cached / reasoning')}>
                {num(log.cached_tokens, locale)} / {num(log.reasoning_tokens, locale)}
              </LogFact>
              {/* 单独一格而不是缀在「缓存 / 推理」后面：那一格已经是两个数了，
                  再塞一个百分号进去分不清它属于哪一个。 */}
              <LogFact label={t('缓存率', 'Cache hit')}>
                {formatPercent(cacheHitRate(log.input_tokens, log.cached_tokens))}
              </LogFact>
              <LogFact label={t('首字 / 总耗时', 'TTFT / total')}>
                {ms(log.ttft_ms)} / {ms(log.total_ms)}
              </LogFact>
              <LogFact label={t('会话', 'Session')}>
                <span className="font-mono" title={log.session_id ?? undefined}>{sessionShort}</span>
              </LogFact>
              {/* 缓存结局紧跟着会话：这两个只有摆在一起才有意义——「哪一段对话」+
                  「它这次为什么没命中」。悬浮上去是那一类该怎么处置。 */}
              {log.cache_reason && (
                <LogFact label={t('缓存结局', 'Cache outcome')}>
                  <span title={cacheReasonHint(log.cache_reason, t) ?? undefined}>
                    {cacheReasonLabel(log.cache_reason, t)}
                  </span>
                </LogFact>
              )}
            </dl>
            {log.ua && (
              <p
                className="mt-2 truncate border-t pt-1.5 text-2xs text-muted-foreground"
                title={log.ua}
              >
                {log.ua}
              </p>
            )}
          </li>
        )
      })}
    </ul>
  )
}

function LogFact({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="min-w-0">
      <dt className="text-2xs text-muted-foreground">{label}</dt>
      <dd className="truncate tabular-nums">{children}</dd>
    </div>
  )
}

function UsageTable({
  rows,
  credentialLabel,
  descriptionId,
  loading,
}: {
  rows: UsageLog[]
  credentialLabel: string
  descriptionId: string
  loading: boolean
}) {
  const { t, language, locale } = useI18n()
  return (
    <Table
      render={(
        <div
          className="max-h-[32rem] rounded-xl border bg-card outline-none overscroll-contain focus-visible:ring-2 focus-visible:ring-ring sm:max-h-[min(52vh,32rem)]"
          role="region"
          aria-label={t(`${credentialLabel} 的请求明细表`, `Request log table for ${credentialLabel}`)}
          aria-busy={loading}
          tabIndex={0}
        />
      )}
      className="min-w-[76.5rem] table-fixed text-xs"
      aria-describedby={descriptionId}
    >
      <TableCaption className="sr-only">
        {t(`${credentialLabel} 的请求明细`, `Request log for ${credentialLabel}`)}
      </TableCaption>
      <colgroup>
        <col className="w-[7.5rem]" />
        <col className="w-[4rem]" />
        <col className="w-[9rem]" />
        <col className="w-[4.25rem]" />
        <col className="w-[4.25rem]" />
        <col className="w-[6.5rem]" />
        <col className="w-[4.5rem]" />
        <col className="w-[7.25rem]" />
        <col className="w-[5rem]" />
        <col className="w-[6.5rem]" />
        <col className="w-[18rem]" />
      </colgroup>
      <TableHeader className="sticky top-0 z-10 bg-muted/96 backdrop-blur-sm">
        <TableRow className="bg-muted/72 [&>th]:border-b [&>th]:text-2xs">
          <TableHead scope="colgroup" colSpan={3} className="h-7 text-center">{t('请求', 'Request')}</TableHead>
          <TableHead scope="colgroup" colSpan={4} className="h-7 text-center">Token</TableHead>
          <TableHead scope="colgroup" className="h-7 text-center">{t('性能', 'Performance')}</TableHead>
          <TableHead scope="colgroup" className="h-7 text-center">{t('费用', 'Billing')}</TableHead>
          <TableHead scope="colgroup" colSpan={2} className="h-7 text-center">{t('来源', 'Source')}</TableHead>
        </TableRow>
        <TableRow className="bg-muted/96">
          <TableHead className="whitespace-nowrap">{t('时间', 'Time')}</TableHead>
          <TableHead className="whitespace-nowrap">{t('状态', 'Status')}</TableHead>
          <TableHead className="whitespace-nowrap">{t('模型', 'Model')}</TableHead>
          <TableHead className="whitespace-nowrap text-right">{t('输入', 'Input')}</TableHead>
          <TableHead className="whitespace-nowrap text-right">{t('输出', 'Output')}</TableHead>
          <TableHead className="whitespace-nowrap text-right">{t('缓存 / 推理', 'Cached / reasoning')}</TableHead>
          <TableHead
            className="whitespace-nowrap text-right"
            title={t(
              '这条请求的缓存命中率：命中缓存的输入 token ÷ 输入 token。同一段会话固定落在同一个号上时它才高得起来。',
              "This request's cache hit rate: cached input tokens over input tokens. It only gets high when a conversation keeps landing on the same account.",
            )}
          >
            {t('缓存率', 'Cache hit')}
          </TableHead>
          <TableHead className="whitespace-nowrap text-right">{t('首字 / 总耗时', 'TTFT / total')}</TableHead>
          <TableHead className="whitespace-nowrap text-right">{t('花费', 'Cost')}</TableHead>
          <TableHead className="whitespace-nowrap">{t('会话', 'Session')}</TableHead>
          <TableHead
            className="min-w-52 whitespace-nowrap"
            title={t('来访客户端自报的 User-Agent', 'User-Agent reported by the incoming client')}
          >
            {t('客户端 UA', 'Client UA')}
          </TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {rows.map((log) => {
          const sessionId = log.session_id ?? '—'
          const sessionShort = log.session_id ? log.session_id.slice(0, 8) : '—'
          const ms = (v: number | null) => (v == null ? '—' : `${v.toLocaleString(locale)}ms`)
          return (
            <TableRow key={log.id} className="[&>td]:px-2.5 [&>td]:py-2">
              <TableCell
                className="whitespace-nowrap tabular-nums"
                title={`${formatFullTime(log.ts, language)} · ${log.path}`}
              >
                {logTime(log.ts)}
              </TableCell>
              <TableCell>
                <Badge variant={statusVariant(log.status)} size="sm" className="tabular-nums">
                  {log.status}
                </Badge>
              </TableCell>
              <TableCell className="max-w-40 truncate" title={log.model ?? undefined}>
                {log.model ?? '—'}
              </TableCell>
              <TableCell className="whitespace-nowrap text-right tabular-nums">
                {num(log.input_tokens, locale)}
              </TableCell>
              <TableCell className="whitespace-nowrap text-right tabular-nums">
                {num(log.output_tokens, locale)}
              </TableCell>
              <TableCell className="whitespace-nowrap text-right tabular-nums">
                {num(log.cached_tokens, locale)} / {num(log.reasoning_tokens, locale)}
              </TableCell>
              {/* 命中率为 0（真的一点没命中，比如会话第一轮）与 `—`（这条请求没嗅探到
                  usage，多半是 4xx）不能长得一样：后者转灰，免得被读成「缓存崩了」。 */}
              <TableCell
                className={cn(
                  'whitespace-nowrap text-right tabular-nums',
                  cacheHitRate(log.input_tokens, log.cached_tokens) == null && 'text-muted-foreground',
                )}
              >
                {formatPercent(cacheHitRate(log.input_tokens, log.cached_tokens))}
              </TableCell>
              <TableCell className="whitespace-nowrap text-right tabular-nums">
                {ms(log.ttft_ms)} / {ms(log.total_ms)}
              </TableCell>
              <TableCell
                className={cn(
                  'whitespace-nowrap text-right tabular-nums',
                  log.cost_usd == null && 'text-muted-foreground',
                )}
                title={log.cost_usd == null
                  ? t('模型未在价目表内，无法估算', 'Model is not in the price table, cost cannot be estimated')
                  : undefined}
              >
                {log.cost_usd == null ? '—' : formatUsd(log.cost_usd)}
              </TableCell>
              <TableCell className="whitespace-nowrap font-mono text-xs" title={sessionId}>
                {sessionShort}
              </TableCell>
              <UaCell ua={log.ua} />
            </TableRow>
          )
        })}
      </TableBody>
    </Table>
  )
}

/**
 * UA 单元格。
 *
 * 真实 UA 动辄六七十字符，表格里截断显示，完整内容留在 title 里——不截的话一条长 UA
 * 会把整行撑高，一屏就看不了几条。
 */
function UaCell({ ua }: { ua: string | null }) {
  return (
    <TableCell className="align-top leading-4" title={ua ?? undefined}>
      <span className={cn('block truncate', !ua && 'text-muted-foreground')}>{ua ?? '—'}</span>
    </TableCell>
  )
}
