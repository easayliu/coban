import { useId, useState } from 'react'
import { useI18n } from '@/lib/i18n'
import { cacheHitRate, cn, formatCompactNumber, formatPercent, type CacheGranularity, type CacheSlot } from '@/lib/utils'

/**
 * 缓存命中率趋势图：一格一根柱子，柱高就是那一格的命中率。
 *
 * 为什么是**柱**而不是折线：格子里会有窟窿（那个时段一条请求都没跑），而折线只会把窟窿两
 * 端连起来——那条斜线是编出来的。柱子做不到这件事：没有柱子就是没有数据。
 *
 * 为什么用 div 而不是 `<svg>`：柱状图的几何全是「按容器宽度均分」，CSS 的 flex 天生就在做
 * 这件事，而 SVG 要么自己量容器宽度、要么用 viewBox 缩放（那会把文字和描边一起放大）。
 * 命中区、键盘焦点、悬浮态也就都是现成的 DOM 行为，不必自己搭一层。
 *
 * 颜色只有一种（`--chart-1`）且**不随取值变化**：柱高已经把「高不高」说了一遍，再让颜色跟着
 * 数值走就是把唯一的空闲通道花在重复信息上，而且绿/黄/红在这套界面里是状态语义，那样画出来
 * 会被读成「这一格告警」。
 */

/** 一格里能读到的全部东西，悬浮/聚焦时原样念出来。 */
function slotReadout(
  slot: CacheSlot,
  granularity: CacheGranularity,
  t: (zh: string, en: string) => string,
  locale: string,
): { when: string; axis: string; rate: string; detail: string } {
  const d = new Date(slot.ts * 1000)
  const p = (n: number) => String(n).padStart(2, '0')
  const day = `${d.getMonth() + 1}/${d.getDate()}`
  // 读数里要带日期（跨天的两个 14:00 得分得清），横轴上只留时钟——那一行每格只有十几像素。
  const when = granularity === 'hour' ? `${day} ${p(d.getHours())}:00` : day
  const axis = granularity === 'hour' ? `${p(d.getHours())}:00` : day
  if (!slot.hasTraffic) {
    return { when, axis, rate: '—', detail: t('这个时段没有请求', 'No requests in this period') }
  }
  return {
    when,
    axis,
    rate: formatPercent(cacheHitRate(slot.inputTokens, slot.cachedTokens)),
    // 体量由调用方按整段的最大值判断，这里只把两个原始数念出来。
    detail: t(
      `命中 ${slot.cachedTokens.toLocaleString(locale)} / 输入 ${slot.inputTokens.toLocaleString(locale)} token`,
      `${slot.cachedTokens.toLocaleString(locale)} cached of ${slot.inputTokens.toLocaleString(locale)} input tokens`,
    ),
  }
}

/** 横轴每隔几格标一个刻度：最多 7 个，再多就挤成一片糊。 */
function tickStep(slots: number): number {
  return Math.max(1, Math.ceil(slots / 7))
}

/**
 * 每一格的**体量权重**（0–1）：这一格的输入 token 占这段时间里最大那格的多少。
 *
 * 为什么必须画出来：柱高是**比率**，而比率不带体量。真实流水里一个小时可能只有 21 个输入
 * token（某个会话的第一轮，本来就没什么可命中的）——它的「0%」和隔壁 14M token 上的「95%」
 * 画成一样响，读起来就是「缓存崩了三小时」，而那三小时其实什么也没发生。
 *
 * 用不透明度而不是别的通道：柱高已经被比率占了，颜色只有一种（也不该跟着取值变），而深浅
 * 天然读作「这根柱子有多少分量」。开方是为了别把中等体量压得太暗（线性下 10% 的体量看着
 * 就快没了），下限 0.3 是为了再小的体量也还看得见——**不是隐藏，是标轻**。
 */
function volumeWeight(inputTokens: number, maxInputTokens: number): number {
  if (maxInputTokens <= 0) return 1
  return 0.3 + 0.7 * Math.sqrt(Math.min(1, inputTokens / maxInputTokens))
}

/** 体量占比低于这条线的格子不参与「最低那格」的直接标注：给一个几百 token 的 0% 挂标签是误导。 */
const DIP_MIN_VOLUME_SHARE = 0.1

export function CacheHitColumns({
  slots,
  granularity,
  /** 正在重取：整块降透明度而不是换骨架屏，免得每次轮询都跳一下版。 */
  refetching = false,
  className,
}: {
  slots: CacheSlot[]
  granularity: CacheGranularity
  refetching?: boolean
  className?: string
}) {
  const { t, locale } = useI18n()
  const [active, setActive] = useState<number | null>(null)
  const step = tickStep(slots.length)
  const readouts = slots.map((s) => slotReadout(s, granularity, t, locale))
  const maxInput = Math.max(0, ...slots.map((s) => s.inputTokens))
  // 最低的那一格值得直接标出来：命中率的故事几乎总是「哪一段掉下去了」。只标一个——
  // 每根柱子都挂个数字就没人看了。**只在有分量的格子里挑**：一个 300 token 的小时里的
  // 「0%」是这段流水里最低的没错，但把它标出来是拿噪声当结论。
  const dipIndex = slots.reduce<number | null>((lowest, s, i) => {
    if (!s.hasTraffic || s.inputTokens < maxInput * DIP_MIN_VOLUME_SHARE) return lowest
    const rate = cacheHitRate(s.inputTokens, s.cachedTokens) ?? 1
    const best = lowest == null ? null : cacheHitRate(slots[lowest].inputTokens, slots[lowest].cachedTokens) ?? 1
    return best == null || rate < best ? i : lowest
  }, null)
  // 标签只在**放得下**的时候画：格子多到二十几个时，`69.9%` 比一格还宽，会压到隔壁那根柱子
  // 上，看着像渲染坏了。放不下就不画——那个值仍在悬浮读数与表格里，一个也没丢。
  // 首尾两格也不标：它们的标签会顶到图外面去。
  const dipWorthLabelling =
    dipIndex != null &&
    slots.length <= 12 &&
    slots.filter((s) => s.hasTraffic).length > 2 &&
    dipIndex > 0 &&
    dipIndex < slots.length - 1

  return (
    <div className={cn('transition-opacity', refetching && 'opacity-60', className)}>
      <div className="flex gap-2">
        {/* 纵轴刻度：直接标出来的只有一格，其余的值靠这三档与悬浮读数。 */}
        <div className="flex h-40 w-8 shrink-0 flex-col justify-between py-0 text-end text-2xs text-muted-foreground tabular-nums">
          <span className="-translate-y-1/2">100%</span>
          <span>50%</span>
          <span className="translate-y-1/2">0%</span>
        </div>

        <div className="min-w-0 flex-1">
          <div className="relative h-40">
            {/* 网格线：一格实线细线，比底色只深一档——它是背景，不是数据。 */}
            {[0, 50, 100].map((pct) => (
              <div
                key={pct}
                aria-hidden
                className="absolute inset-x-0 border-t border-border"
                style={{ bottom: `${pct}%` }}
              />
            ))}
            <div className="absolute inset-0 flex items-end">
              {slots.map((slot, i) => {
                const rate = slot.hasTraffic ? cacheHitRate(slot.inputTokens, slot.cachedTokens) ?? 0 : null
                return (
                  <div
                    key={slot.ts}
                    role="img"
                    tabIndex={0}
                    aria-label={`${readouts[i].when} · ${readouts[i].rate} · ${readouts[i].detail}`}
                    onPointerEnter={() => setActive(i)}
                    onPointerLeave={() => setActive((cur) => (cur === i ? null : cur))}
                    onFocus={() => setActive(i)}
                    onBlur={() => setActive((cur) => (cur === i ? null : cur))}
                    // 命中区是整根立柱（连那 1px 的间隙一起），不是画出来的那点像素。
                    className="group relative flex h-full flex-1 items-end justify-center px-px outline-none"
                  >
                    <span
                      aria-hidden
                      className={cn(
                        'absolute inset-0 transition-colors',
                        active === i && 'bg-muted/56',
                        'group-focus-visible:ring-2 group-focus-visible:ring-ring group-focus-visible:ring-inset',
                      )}
                    />
                    {rate == null ? (
                      // 没有请求的时段：一条中性的底线，与「有请求但一点没命中」（同样 2px，
                      // 但是数据色）区分得开。
                      <span
                        aria-hidden
                        className="relative h-0.5 w-full max-w-6 rounded-full bg-muted-foreground/24"
                      />
                    ) : (
                      <span
                        aria-hidden
                        className="relative w-full max-w-6 rounded-t bg-chart-1"
                        // 顶端 4px 圆角、底边贴齐基线；再低的值也留 2px，否则 0.4% 会被舍成看不见。
                        // 深浅 = 这一格的 token 体量（见 volumeWeight）。
                        style={{
                          height: `max(0.125rem, ${rate * 100}%)`,
                          opacity: volumeWeight(slot.inputTokens, maxInput),
                        }}
                      />
                    )}
                    {dipWorthLabelling && i === dipIndex && (
                      <span
                        aria-hidden
                        className="absolute whitespace-nowrap text-2xs text-muted-foreground tabular-nums"
                        // 贴在柱顶上方 4px：直接标在柱子里放不下（这些柱子只有十几像素宽）。
                        style={{ bottom: `calc(max(0.125rem, ${(rate ?? 0) * 100}%) + 0.25rem)` }}
                      >
                        {readouts[i].rate}
                      </span>
                    )}
                  </div>
                )
              })}
            </div>

            {/* 读数：一个节点跟着焦点走，而不是给每根柱子挂一个 tooltip 实例。
                值在前、时段在后——看的人已经知道自己指着哪一格，要的是那个数。

                **画在图里面，不画在图上方。** 上方那版会被外面的滚动容器裁掉半截
                （对话框的正文是 ScrollArea），表现是气泡缺了顶、还压在上一段说明文字上。
                盖住柱顶不是问题：要读的那个数就在气泡里。 */}
            {active != null && (() => {
              // 横向三挡对齐。只夹 `left` 是不够的：气泡宽度是 max-content，最右那一格
              // 居中摆会整个溢出画布——而画布外面就是那个会裁的滚动容器。贴边的两成格子
              // 改成把气泡的边对齐图的边，中间的照常居中。
              const pos = (active + 0.5) / slots.length
              const anchor = pos < 0.2 ? 'start' : pos > 0.8 ? 'end' : 'center'
              return (
                <div
                  role="status"
                  aria-live="off"
                  className={cn(
                    'pointer-events-none absolute top-1 z-10 rounded-lg border bg-popover px-2 py-1',
                    'text-2xs leading-4 text-popover-foreground shadow-md',
                    // `w-max` 是关键：绝对定位默认按「到容器右边还剩多少」收缩，最右那一格
                    // 只剩几十像素，于是每个数字断成一行，气泡被撑成一根竖条（还因此变高、
                    // 更容易被裁）。`max-w` 兜住窄屏，那时才允许折行。
                    'w-max max-w-[min(16rem,100%)]',
                    anchor === 'center' && '-translate-x-1/2',
                  )}
                  style={
                    anchor === 'start'
                      ? { left: 0 }
                      : anchor === 'end'
                        ? { right: 0 }
                        : { left: `${pos * 100}%` }
                  }
                >
                  {/* 率与时段并到一行：气泡越矮越不容易挡住柱子，也越不容易顶出画布。 */}
                  <p className="flex items-baseline gap-1.5 tabular-nums">
                    <span className="font-semibold">{readouts[active].rate}</span>
                    <span className="text-muted-foreground">{readouts[active].when}</span>
                  </p>
                  <p className="text-muted-foreground tabular-nums">{readouts[active].detail}</p>
                </div>
              )
            })()}
          </div>

          {/* 刻度绝对定位、按格心对齐：让它跟着格子等分（每格 flex-1 + truncate）的话，
              24 格时每格只有三十几像素，`22:00` 会被截成 `22:…`——刻度被截等于没有刻度。
              这里允许标签横向溢出自己那一格，反正相邻的刻度隔着 step 格，撞不上。 */}
          <div className="relative mt-1.5 h-4" aria-hidden>
            {slots.map((slot, i) =>
              i % step === 0 ? (
                <span
                  key={slot.ts}
                  className="absolute -translate-x-1/2 whitespace-nowrap text-2xs text-muted-foreground tabular-nums"
                  style={{ left: `${((i + 0.5) / slots.length) * 100}%` }}
                >
                  {readouts[i].axis}
                </span>
              ) : null,
            )}
          </div>
        </div>
      </div>
    </div>
  )
}

/**
 * 表格版：**每个值都不靠悬浮才读得到**。
 *
 * 悬浮读数只是增强；一个只能用鼠标悬浮才能读的图对读屏与键盘用户等于不存在。
 */
export function CacheHitTable({
  slots,
  granularity,
}: {
  slots: CacheSlot[]
  granularity: CacheGranularity
}) {
  const { t, locale } = useI18n()
  const captionId = useId()
  const rows = slots.filter((s) => s.hasTraffic)

  return (
    <div className="max-h-64 overflow-y-auto rounded-xl border">
      <table className="w-full text-xs" aria-describedby={captionId}>
        <caption id={captionId} className="sr-only">
          {t('缓存命中率按时段明细', 'Cache hit rate by period')}
        </caption>
        <thead className="sticky top-0 bg-muted/96 backdrop-blur-sm">
          <tr className="[&>th]:h-7 [&>th]:border-b [&>th]:px-3 [&>th]:text-2xs [&>th]:font-medium [&>th]:text-muted-foreground">
            <th scope="col" className="text-start">
              {granularity === 'hour' ? t('时段', 'Hour') : t('日期', 'Day')}
            </th>
            <th scope="col" className="text-end">{t('命中率', 'Hit rate')}</th>
            <th scope="col" className="text-end">{t('命中 token', 'Cached')}</th>
            <th scope="col" className="text-end">{t('输入 token', 'Input')}</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((slot) => {
            const r = slotReadout(slot, granularity, t, locale)
            return (
              <tr key={slot.ts} className="[&>td]:border-b [&>td]:px-3 [&>td]:py-1.5 last:[&>td]:border-b-0">
                <td className="whitespace-nowrap tabular-nums">{r.when}</td>
                <td className="whitespace-nowrap text-end font-medium tabular-nums">{r.rate}</td>
                <td className="whitespace-nowrap text-end tabular-nums">
                  {slot.cachedTokens.toLocaleString(locale)}
                </td>
                <td className="whitespace-nowrap text-end tabular-nums">
                  {slot.inputTokens.toLocaleString(locale)}
                </td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </div>
  )
}

/**
 * 概览那一格里的迷你趋势：只讲形状，不带轴也不带交互（整格的说明由外面那格的悬浮提示给）。
 *
 * 最近那一格满色、其余压到 40%：这一格的主角是当下那个数字，趋势是它的背景。
 */
export function CacheHitSparkline({ slots, className }: { slots: CacheSlot[]; className?: string }) {
  const maxInput = Math.max(0, ...slots.map((s) => s.inputTokens))
  return (
    <span aria-hidden className={cn('flex h-5 items-end gap-px', className)}>
      {slots.map((slot, i) => {
        const rate = slot.hasTraffic ? cacheHitRate(slot.inputTokens, slot.cachedTokens) ?? 0 : null
        const last = i === slots.length - 1
        return (
          <span
            key={slot.ts}
            className={cn('w-1.5 rounded-t', rate == null ? 'bg-muted-foreground/24' : 'bg-chart-1')}
            style={{
              height: rate == null ? '0.125rem' : `max(0.125rem, ${rate * 100}%)`,
              // 最近那一格满色，往前的压到四成，再乘体量权重——同大图一个道理：只有几百
              // token 的那天不该和主力那天一样响。
              opacity:
                rate == null ? undefined : (last ? 1 : 0.4) * volumeWeight(slot.inputTokens, maxInput),
            }}
          />
        )
      })}
    </span>
  )
}

/** 一段格子的合计命中率：**按 token 加权**，不是各格命中率的平均。 */
export function aggregateCacheHitRate(slots: CacheSlot[]): {
  rate: number | null
  inputTokens: number
  cachedTokens: number
} {
  const inputTokens = slots.reduce((sum, s) => sum + s.inputTokens, 0)
  const cachedTokens = slots.reduce((sum, s) => sum + s.cachedTokens, 0)
  return { rate: cacheHitRate(inputTokens, cachedTokens), inputTokens, cachedTokens }
}

/** 合计的两个 token 数压成一行短文案（`命中 1.2M / 输入 1.3M`）。 */
export function cacheTotalsText(
  cachedTokens: number,
  inputTokens: number,
  t: (zh: string, en: string) => string,
): string {
  return t(
    `命中 ${formatCompactNumber(cachedTokens)} / 输入 ${formatCompactNumber(inputTokens)}`,
    `${formatCompactNumber(cachedTokens)} of ${formatCompactNumber(inputTokens)} input`,
  )
}
