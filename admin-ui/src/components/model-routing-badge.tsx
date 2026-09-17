import { useQuery } from '@tanstack/react-query'
import { getModelRouting, type ModelRoutingAccount } from '@/api/metrics'
import { useI18n } from '@/lib/i18n'
import { cn, formatPercent, formatUsd } from '@/lib/utils'
import { badgeVariants } from '@/components/ui/badge'
import { Tooltip, TooltipPopup, TooltipTrigger } from '@/components/ui/tooltip'

/**
 * 这枚徽章看的跨度：7 天，与概览那格缓存命中率同一个跨度。
 *
 * 改路由是上游那头按天变的策略（某个模型忙、某个档位被收窄），24 小时看不出「这是常态还是
 * 昨天那一阵」；30 天又会把一次已经过去的调整在卡片上一直挂着。
 */
const ROUTING_HOURS = 7 * 24

/**
 * 各账号被改路由的情况，按 `cred_id` 取用。
 *
 * **整页共用一次查询**：这份数据是一条按号分组的 SQL，而卡片有几十张——每张自己拉一次就是
 * 每轮刷新几十个请求。react-query 的 key 相同即同一份缓存，各卡片只是读它。
 */
function useModelRouting(credId: number): ModelRoutingAccount | undefined {
  const { data } = useQuery({
    queryKey: ['model-routing', ROUTING_HOURS],
    queryFn: () => getModelRouting(ROUTING_HOURS),
    // 与缓存那条曲线同一个节奏：这是个按天看的量，不必跟着 10 秒的实时指标跑。
    refetchInterval: 60_000,
  })
  return data?.accounts.find((a) => a.cred_id === credId)
}

/**
 * 「这个号有请求没拿到要的那个模型」。
 *
 * 只在真发生过时出现——后端压根不回没被改过的号（见 `ModelRouting.accounts`）。摆在账号自己
 * 的徽章行里而不是概览上：改路由是上游**对着某个账号**做的决定（这个号的档位、它此刻的排队
 * 情况），池级那个平均数会把「其中一个号被整体降级了」摊薄成一个谁也看不出的小百分比，而那
 * 恰恰是唯一需要动手的情形。
 *
 * 徽章上只放条数——一句「5 条」就够决定要不要细看；是哪几对、占多少、花了多少在提示里，
 * 逐条在这个号的请求明细里。
 */
export function ModelRoutingBadge({ credId, size = 'sm' }: { credId: number; size?: 'sm' | 'default' }) {
  const { t, locale } = useI18n()
  const routing = useModelRouting(credId)
  if (!routing) return null

  const n = (v: number) => v.toLocaleString(locale)
  const share = routing.observed > 0 ? routing.routed / routing.observed : null

  return (
    <Tooltip>
      <TooltipTrigger className={cn(badgeVariants({ variant: 'warning', size }), 'cursor-help')}>
        {t(`改路由 ${n(routing.routed)}`, `Rerouted ${n(routing.routed)}`)}
      </TooltipTrigger>
      <TooltipPopup className="max-w-80 whitespace-normal text-left leading-5">
        <p>
          {t(
            `近 7 天这个号有 ${n(routing.routed)} 条请求没拿到要的那个模型，占能判断的 ${n(routing.observed)} 条的 ${formatPercent(share)}。`,
            `Over the last 7 days this account had ${n(routing.routed)} requests that did not get the model they asked for — ${formatPercent(share)} of the ${n(routing.observed)} where routing could be observed.`,
          )}
        </p>
        <ul className="mt-1.5 space-y-0.5">
          {routing.pairs.map((p) => (
            <li key={`${p.req_model}→${p.model}`} className="tabular-nums">
              {p.req_model} → <span className="font-medium">{p.model}</span>
              {t(
                `：${n(p.requests)} 条 · ${formatUsd(p.cost_usd)}`,
                `: ${n(p.requests)} req · ${formatUsd(p.cost_usd)}`,
              )}
            </li>
          ))}
        </ul>
        <p className="mt-1.5 text-muted-foreground">
          {t(
            '分母只算上游真的给了一个模型的请求——错误响应没有路由可言。花费按实际给的那个模型计价。逐条看在请求明细里。',
            'The denominator counts only requests where the upstream actually served a model — an error response has no routing to speak of. Cost is priced by the model that was served. Per-request detail lives in the request log.',
          )}
        </p>
      </TooltipPopup>
    </Tooltip>
  )
}
