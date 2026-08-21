import { Fragment, useRef, useState } from 'react'
import { keepPreviousData, useQuery } from '@tanstack/react-query'
import { BarChart3Icon, DatabaseZapIcon, TableIcon } from 'lucide-react'
import { getCacheReasons, getCacheSeries, type Metrics } from '@/api/metrics'
import { useI18n } from '@/lib/i18n'
import {
  bucketCacheSeries,
  cacheHitRate,
  extractError,
  formatPercent,
  type CacheGranularity,
} from '@/lib/utils'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Avatar, AvatarFallback } from '@/components/ui/avatar'
import { Badge } from '@/components/ui/badge'
import {
  Dialog,
  DialogDescription,
  DialogHeader,
  DialogPanel,
  DialogPopup,
  DialogTitle,
} from '@/components/ui/dialog'
import { Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from '@/components/ui/empty'
import { Skeleton } from '@/components/ui/skeleton'
import { Spinner } from '@/components/ui/spinner'
import { ToggleGroup, ToggleGroupItem, ToggleGroupSeparator } from '@/components/ui/toggle-group'
import {
  CacheHitColumns,
  CacheHitTable,
  aggregateCacheHitRate,
  cacheTotalsText,
} from '@/components/cache-hit-chart'
import { CacheReasonBreakdown } from '@/components/cache-reason-breakdown'

/**
 * 可选的回看跨度。三档而不是一个自由输入：这张图要回答的是「最近怎么样 / 这几天怎么样 /
 * 这一个月怎么样」，中间那些跨度分不出新的结论。
 *
 * 跨度与分桶粒度是绑定的：24 小时按小时分（更细就成了噪声，一个小时里通常只有几条请求），
 * 更长的跨度按天分（30 天按小时是 720 根柱子，一根不到一像素）。
 */
export const CACHE_RANGES = {
  '24h': { hours: 24, slots: 24, granularity: 'hour' as CacheGranularity },
  '7d': { hours: 7 * 24, slots: 7, granularity: 'day' as CacheGranularity },
  '30d': { hours: 30 * 24, slots: 30, granularity: 'day' as CacheGranularity },
} as const

export type CacheRangeKey = keyof typeof CACHE_RANGES

/** 概览那一格默认看的跨度。7 天：比 24 小时更看得出趋势，又不至于把一次调整摊薄在 30 天里。 */
export const DEFAULT_CACHE_RANGE: CacheRangeKey = '7d'

/** 拉一段趋势并铺成连续的格子。跨度相同的两处（概览那一格与这个对话框）共用同一份缓存。 */
export function useCacheSeries(range: CacheRangeKey, enabled = true) {
  const preset = CACHE_RANGES[range]
  const query = useQuery({
    queryKey: ['cache-series', preset.hours],
    queryFn: () => getCacheSeries(preset.hours),
    enabled,
    // 与实时指标同一个节奏：这条曲线最右那一格是「此刻」，跟着 30 秒的账号列表走会显得停摆。
    refetchInterval: 60_000,
    // 换跨度时先留着上一份，图不闪成骨架屏。
    placeholderData: keepPreviousData,
  })
  const slots = bucketCacheSeries(query.data?.points ?? [], preset.granularity, preset.slots)
  return { query, slots, granularity: preset.granularity }
}

export function CacheHitTrendDialog({
  open,
  onOpenChange,
  metrics,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  /** 全池终身口径，作为趋势的参照写在脚注里（「这一段」相对「一直以来」是高还是低）。 */
  metrics?: Metrics
}) {
  const { t, locale } = useI18n()
  const titleRef = useRef<HTMLHeadingElement>(null)
  const [range, setRange] = useState<CacheRangeKey>(DEFAULT_CACHE_RANGE)
  const [view, setView] = useState<'chart' | 'table'>('chart')
  const { query, slots, granularity } = useCacheSeries(range, open)
  // 归因与曲线分两个请求：曲线是 30 秒一跳的实时口径，而这份分布只在对话框开着时要，
  // 合成一个接口会让概览那一格也跟着拉一份它不显示的数据。
  const reasons = useQuery({
    queryKey: ['cache-reasons', CACHE_RANGES[range].hours],
    queryFn: () => getCacheReasons(CACHE_RANGES[range].hours),
    enabled: open,
    refetchInterval: 60_000,
    placeholderData: keepPreviousData,
  })
  const total = aggregateCacheHitRate(slots)
  const hasTraffic = slots.some((s) => s.hasTraffic)
  const lifetime = cacheHitRate(metrics?.input_tokens_total, metrics?.cached_tokens_total)

  const rangeLabel: Record<CacheRangeKey, string> = {
    '24h': t('近 24 小时', 'Last 24 hours'),
    '7d': t('近 7 天', 'Last 7 days'),
    '30d': t('近 30 天', 'Last 30 days'),
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogPopup className="max-w-3xl" initialFocus={titleRef}>
        <DialogHeader className="border-b bg-muted/32 p-4 sm:p-5">
          <div className="flex items-center gap-3 pr-8">
            <Avatar>
              <AvatarFallback><DatabaseZapIcon /></AvatarFallback>
            </Avatar>
            <div className="min-w-0 flex-1">
              <div className="flex flex-wrap items-center gap-2">
                <DialogTitle ref={titleRef} tabIndex={-1}>
                  {t('缓存命中率趋势', 'Cache hit rate trend')}
                </DialogTitle>
                <Badge variant="info" aria-live="polite">{rangeLabel[range]}</Badge>
                {query.isFetching && !query.isPending && <Spinner />}
              </div>
              <DialogDescription className="mt-1">
                {t(
                  '全池按 token 加权，不是各账号命中率的平均。',
                  'Pooled and token-weighted, not an average of per-account rates.',
                )}
              </DialogDescription>
            </div>
          </div>
        </DialogHeader>

        <DialogPanel className="space-y-3 p-4 pt-3 sm:p-5 sm:pt-3">
          {/* 跨度与视图的开关在图上方一行：图本身不带任何控件。 */}
          <div className="flex flex-wrap items-center justify-between gap-2">
            <ToggleGroup
              value={[range]}
              onValueChange={(values) => {
                const next = values[values.length - 1]
                if (next && next in CACHE_RANGES) setRange(next as CacheRangeKey)
              }}
              variant="outline"
              aria-label={t('回看跨度', 'Time range')}
            >
              {(Object.keys(CACHE_RANGES) as CacheRangeKey[]).map((key, i) => (
                <Fragment key={key}>
                  {i > 0 && <ToggleGroupSeparator />}
                  <ToggleGroupItem value={key} aria-label={rangeLabel[key]}>
                    {key}
                  </ToggleGroupItem>
                </Fragment>
              ))}
            </ToggleGroup>

            <ToggleGroup
              value={[view]}
              onValueChange={(values) => {
                const next = values[values.length - 1]
                if (next === 'chart' || next === 'table') setView(next)
              }}
              variant="outline"
              aria-label={t('图表 / 表格', 'Chart or table')}
            >
              <ToggleGroupItem value="chart" aria-label={t('图表', 'Chart')} title={t('图表', 'Chart')}>
                <BarChart3Icon />
              </ToggleGroupItem>
              <ToggleGroupSeparator />
              <ToggleGroupItem value="table" aria-label={t('表格', 'Table')} title={t('表格', 'Table')}>
                <TableIcon />
              </ToggleGroupItem>
            </ToggleGroup>
          </div>

          {/* 这一段的合计摆在图上方：图讲的是形状，这个数字才是「这段时间到底多少」。 */}
          <section className="flex flex-wrap items-baseline gap-x-3 gap-y-1 rounded-xl border bg-muted/32 px-3 py-2.5 sm:px-4">
            <p className="text-2xs font-medium text-muted-foreground">{rangeLabel[range]}</p>
            <p className="text-2xl font-semibold leading-none">{formatPercent(total.rate)}</p>
            <p className="text-2xs text-muted-foreground tabular-nums">
              {cacheTotalsText(total.cachedTokens, total.inputTokens, t)}
            </p>
            {lifetime != null && (
              <p className="ms-auto text-2xs text-muted-foreground tabular-nums">
                {t(`终身 ${formatPercent(lifetime)}`, `${formatPercent(lifetime)} lifetime`)}
              </p>
            )}
          </section>

          {query.error ? (
            <Alert variant="error">
              <AlertTitle>{t('读取失败', 'Failed to load')}</AlertTitle>
              <AlertDescription>{extractError(query.error)}</AlertDescription>
            </Alert>
          ) : query.isPending ? (
            <Skeleton className="h-48 w-full rounded-xl" />
          ) : !hasTraffic ? (
            <Empty>
              <EmptyHeader>
                <EmptyMedia variant="icon"><DatabaseZapIcon /></EmptyMedia>
                <EmptyTitle>{t('这段时间没有请求', 'No requests in this period')}</EmptyTitle>
                <EmptyDescription>
                  {t(
                    '命中率要有请求才谈得上。换个更长的跨度，或先跑几条请求。',
                    'A hit rate needs traffic. Try a longer range, or send some requests first.',
                  )}
                </EmptyDescription>
              </EmptyHeader>
            </Empty>
          ) : view === 'chart' ? (
            <CacheHitColumns
              slots={slots}
              granularity={granularity}
              refetching={query.isFetching && !query.isPending}
            />
          ) : (
            <CacheHitTable slots={slots} granularity={granularity} />
          )}

          {/* 归因摆在图下面：先看形状（哪几天低），再问为什么低。倒过来的话，一张原因表
              在不知道「低不低」之前是没有参照的。读取失败不挡着上面那张图——曲线是主角。 */}
          {hasTraffic && reasons.data && <CacheReasonBreakdown reasons={reasons.data.reasons} />}

          <p className="text-2xs leading-4 text-muted-foreground">
            {t(
              `命中缓存的输入按十分之一计价，所以这条线基本等于「同一段会话有没有落在同一个号上」。空着的格子是那个时段没有请求——不是命中率掉到 0；柱子的深浅是那一格的 token 体量，很淡的那几根只有几百 token，那个百分比不必当真。流水只保留 30 天，跨度到此为止。`,
              'Cached input bills at a tenth, so this line is essentially "did each conversation keep landing on one account". A gap means no traffic in that period, not a hit rate of zero; a bar\'s opacity is that period\'s token volume, so the faint ones carry only a few hundred tokens and their percentage means little. Request logs are kept for 30 days, which caps the range.',
            )}
          </p>
          <p className="sr-only" aria-live="polite">
            {t(
              `${rangeLabel[range]}缓存命中率 ${formatPercent(total.rate)}，命中 ${total.cachedTokens.toLocaleString(locale)} / 输入 ${total.inputTokens.toLocaleString(locale)} token`,
              `Cache hit rate ${formatPercent(total.rate)} over ${rangeLabel[range].toLowerCase()}, ${total.cachedTokens.toLocaleString(locale)} cached of ${total.inputTokens.toLocaleString(locale)} input tokens`,
            )}
          </p>
        </DialogPanel>
      </DialogPopup>
    </Dialog>
  )
}
