import { useQuery } from '@tanstack/react-query'
import { RouteIcon } from 'lucide-react'
import { getModelRouting } from '@/api/metrics'
import { useI18n } from '@/lib/i18n'
import { formatPercent, formatUsd } from '@/lib/utils'

/**
 * 这张读数看的跨度：7 天，与概览那格缓存命中率同一个跨度。
 *
 * 改路由是上游那头按天变的策略（某个模型忙、某个档位被收窄），24 小时看不出「这是常态还是
 * 昨天那一阵」；30 天又会把一次已经过去的调整一直挂在页面上。
 */
const ROUTING_HOURS = 7 * 24

/**
 * 「上游有多少请求没给要的那个模型」。
 *
 * 摆在概览那排指标下面而不是另起一格：它是流量的属性而不是号池的状态，而且**绝大多数时候
 * 它只有一行字**——上游老老实实按请求给模型时，这里就该安静。
 *
 * 三种形态：
 * - 这段时间没有能判断的请求（全是错误响应，或压根没流量）：什么都不显示。
 * - 一条都没被改：一行字说清楚，不占地方。它是个好消息，但**不能不说**——什么都不显示的话，
 *   看的人分不清「没被改过」和「coban 根本没在看这件事」。
 * - 有被改的：比例 + 那几对具体是什么。名字本身就是结论（要的是 max、给的是 mini），
 *   所以直接摆出来，不藏进悬浮提示。
 */
export function ModelRoutingNote() {
  const { t, locale } = useI18n()
  const { data } = useQuery({
    queryKey: ['model-routing', ROUTING_HOURS],
    queryFn: () => getModelRouting(ROUTING_HOURS),
    // 与缓存那条曲线同一个节奏：这是个按天看的量，不必跟着 10 秒的实时指标跑。
    refetchInterval: 60_000,
  })
  if (!data || data.observed === 0) return null

  const n = (v: number) => v.toLocaleString(locale)
  const share = data.routed / data.observed

  if (data.routed === 0) {
    return (
      <p className="border-t px-3 py-2.5 text-2xs text-muted-foreground sm:px-4">
        {t(
          `近 7 天上游都给了要的模型：${n(data.observed)} 条请求没有一条被改路由。`,
          `Over the last 7 days the upstream served the model that was asked for on all ${n(data.observed)} requests — none were rerouted.`,
        )}
      </p>
    )
  }

  return (
    <section className="space-y-2 border-t px-3 py-3 sm:px-4" aria-labelledby="model-routing-title">
      <div className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-1">
        <h3 id="model-routing-title" className="flex items-center gap-1.5 text-xs font-medium">
          <RouteIcon className="size-3.5 text-muted-foreground" aria-hidden />
          {t('上游改了路由 · 近 7 天', 'Rerouted by upstream · 7d')}
        </h3>
        <p className="text-2xs tabular-nums text-muted-foreground">
          {t(
            `${formatPercent(share)}（${n(data.routed)} / ${n(data.observed)} 条）`,
            `${formatPercent(share)} (${n(data.routed)} of ${n(data.observed)})`,
          )}
        </p>
      </div>
      <ul className="space-y-1.5">
        {data.pairs.map((p) => {
          const pairShare = p.requests / data.observed
          return (
            <li
              key={`${p.req_model}→${p.model}`}
              className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-3 gap-y-1"
            >
              <div className="flex min-w-0 items-baseline gap-1.5 text-xs">
                {/* 要的那个压成灰、给的那个是正色：一眼看过去，这一列读出来的是「实际拿到
                    的是什么」，要的那个是它的背景。 */}
                <span className="truncate text-muted-foreground">{p.req_model}</span>
                <span className="shrink-0 text-muted-foreground" aria-hidden>→</span>
                <span className="truncate font-medium">{p.model}</span>
              </div>
              <span
                className="shrink-0 text-2xs tabular-nums text-muted-foreground"
                title={t(`这一对上花了 ${formatUsd(p.cost_usd)}`, `${formatUsd(p.cost_usd)} spent on this pair`)}
              >
                {t(`${n(p.requests)} 条`, `${n(p.requests)} req`)} · {formatPercent(pairShare)}
              </span>
              <div
                className="col-span-2 h-1.5 overflow-hidden rounded-full bg-muted-foreground/16"
                role="img"
                aria-label={t(
                  `要 ${p.req_model}、实际给了 ${p.model}：${n(p.requests)} 条，占能判断的请求的 ${formatPercent(pairShare)}`,
                  `Asked for ${p.req_model}, served ${p.model}: ${n(p.requests)} requests, ${formatPercent(pairShare)} of the requests where routing could be observed`,
                )}
              >
                <div
                  className="h-full rounded-full bg-chart-1"
                  style={{ width: `${Math.max(pairShare * 100, pairShare > 0 ? 1.5 : 0)}%` }}
                />
              </div>
            </li>
          )
        })}
      </ul>
      <p className="text-2xs leading-4 text-muted-foreground">
        {t(
          '分母只算「上游真的给了一个模型」的请求——错误响应没有路由可言，算进去会让一段限流期把这个比例冲上去。花费按实际给的那个模型计价，账单认的是它。逐条看在账号的请求明细里。',
          'The denominator counts only requests where the upstream actually served a model — an error response has no routing to speak of, and counting it would let a rate-limited stretch inflate this number. Cost is priced by the model that was served, which is what the bill goes by. Per-request detail lives in each account\'s request log.',
        )}
      </p>
    </section>
  )
}
