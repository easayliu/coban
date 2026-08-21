import { type CacheReasonStat } from '@/api/metrics'
import { useI18n } from '@/lib/i18n'
import { cn, formatPercent, formatTokens } from '@/lib/utils'

/**
 * 缓存结局的对照表：后端那几个标识 → 人话，以及「看见它该做什么」。
 *
 * 每一类都配一句处置建议而不是只有名字：命中率低这件事本身没有动作可做，「客户端每轮在改
 * 前缀」才有。名字告诉你是哪一类，那句话才是这张表存在的理由。
 *
 * 后端新增标识时这里可能缺一条——`reasonMeta` 因此退回显示原始标识，而不是把那一行藏掉：
 * 藏掉会让百分比加不满 100，看的人只会以为自己数错了。
 */
export const CACHE_REASONS: Record<
  string,
  { label: [zh: string, en: string]; hint: [zh: string, en: string]; tone: 'good' | 'neutral' | 'bad' }
> = {
  hit: {
    label: ['命中', 'Hit'],
    hint: [
      '上游报了命中的输入 token。这一行的「白付」是同一批请求里没能命中的那部分尾巴。',
      'Upstream reported cached input tokens. The wasted column here is the uncached tail of those same requests.',
    ],
    tone: 'good',
  },
  first_turn: {
    label: ['新对话第一轮', 'First turn'],
    hint: [
      '第一次见这个会话，输入也只有一项。本来就没有前缀可命中，不是问题。',
      'First time this session appeared and it carried a single input item. There was no prefix to hit — not a problem.',
    ],
    tone: 'neutral',
  },
  new_prefix: {
    label: ['前缀每轮在变', 'Prefix keeps changing'],
    hint: [
      '一段多轮对话以一个从没见过的前缀身份出现，说明客户端每轮在改 instructions 或 tools（两者都进前缀）。这一类是纯亏。coban 刚重启时也会短暂落到这里。',
      'A multi-turn conversation showed up under a never-seen prefix identity, so the client is changing instructions or tools between turns (both are part of the prefix). This one is pure waste. A recent restart also lands here briefly.',
    ],
    tone: 'bad',
  },
  rotated: {
    label: ['换号了', 'Rotated away'],
    hint: [
      '这段对话有落点，但这次没落在那个号上——它在冷却、RPM 满或被停用。上游的缓存按账号存，换号就是从零开始。',
      'The conversation had a placement but did not land on it: that account was cooling down, out of RPM, or disabled. Upstream caches per account, so rotating starts from zero.',
    ],
    tone: 'bad',
  },
  lease_expired: {
    label: ['落点租约过期', 'Lease expired'],
    hint: [
      '同一段对话停得太久，落点租约过期后重新算过。想少见到这一类就把租约时长调长。',
      'The conversation went quiet long enough for its placement lease to expire, so the placement was recomputed. Raise the lease duration to see less of this.',
    ],
    tone: 'neutral',
  },
  upstream_cold: {
    label: ['上游那边凉了', 'Cold upstream'],
    hint: [
      '落点没变、前缀身份也没变，上游就是没有缓存了：要么它自己过期，要么两段开头一样的对话算出了同一个指纹、在上游共用一个会话互相踢。后者可以让客户端自报会话 id 来避开。',
      'Same placement, same prefix identity, yet upstream had nothing: either its own cache expired, or two conversations with identical openings hashed to the same fingerprint and are evicting each other upstream. Having clients send a session id avoids the latter.',
    ],
    tone: 'bad',
  },
  no_usage: {
    label: ['没有用量读数', 'No usage reported'],
    hint: [
      '这条请求没嗅探到用量：错误响应，或者客户端提前断开。谈不上命中与否。',
      'No usage was sniffed from this request — an error response, or the client disconnected early. Neither hit nor miss.',
    ],
    tone: 'neutral',
  },
  probe: {
    label: ['连通性测试', 'Connectivity test'],
    hint: [
      '账号页上手动发的那种探测。它没有会话，单独一类免得混进「未归因」。',
      'The manual probe from the accounts page. It has no session, and is kept separate so it does not muddy "unattributed".',
    ],
    tone: 'neutral',
  },
  unattributed: {
    label: ['未归因', 'Unattributed'],
    hint: [
      '落点租约被关掉了（调度设置里那项），或者这是升级之前的旧流水。归因要靠租约表对照「上次在哪个号上」。',
      'The placement lease is switched off (see scheduling settings), or these are request logs from before the upgrade. Attribution needs the lease table to compare against the previous placement.',
    ],
    tone: 'neutral',
  },
}

function reasonMeta(reason: string) {
  return CACHE_REASONS[reason]
}

/** 一个原因标识的显示名。没登记过的标识原样回，别藏起来。 */
export function cacheReasonLabel(reason: string, t: (zh: string, en: string) => string): string {
  const meta = reasonMeta(reason)
  return meta ? t(...meta.label) : reason
}

export function cacheReasonHint(reason: string, t: (zh: string, en: string) => string): string | null {
  const meta = reasonMeta(reason)
  return meta ? t(...meta.hint) : null
}

/** 未命中的那部分输入 token——这张表的排序键，也是每一行条子的长度。 */
function wasted(r: CacheReasonStat): number {
  return Math.max(0, r.input_tokens - r.cached_tokens)
}

/**
 * 缓存未命中的原因分布。
 *
 * **按白付的输入 token 排，不按条数**（后端已排好，这里不重排）：要决定「先修哪个」，看的
 * 是这一类原因白付了多少钱，而不是它发生了几次。
 *
 * 条子的长度是「这一类占全部白付的多少」而不是「这一类自己的未命中率」：后者对每个未命中
 * 类别恒等于 100%，画出来是一排等长的条子，什么也没说。
 */
export function CacheReasonBreakdown({ reasons }: { reasons: CacheReasonStat[] }) {
  const { t, locale } = useI18n()
  const total = reasons.reduce((sum, r) => sum + wasted(r), 0)
  const rows = reasons.filter((r) => r.requests > 0)

  if (rows.length === 0) {
    return (
      <p className="text-2xs leading-4 text-muted-foreground">
        {t('这段时间没有可归因的请求。', 'No attributable requests in this period.')}
      </p>
    )
  }

  return (
    <section className="space-y-2 rounded-xl border bg-muted/32 p-3 sm:p-4">
      <div className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-1">
        <h3 className="text-xs font-medium">{t('未命中都花在哪了', 'Where the misses went')}</h3>
        <p className="text-2xs text-muted-foreground tabular-nums">
          {t(
            `共白付 ${formatTokens(total)} 输入 token`,
            `${formatTokens(total)} input tokens paid uncached`,
          )}
        </p>
      </div>
      <ul className="space-y-1.5">
        {rows.map((r) => {
          const w = wasted(r)
          const share = total > 0 ? w / total : 0
          const meta = reasonMeta(r.reason)
          const hint = cacheReasonHint(r.reason, t)
          return (
            <li key={r.reason} className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-3 gap-y-1">
              <div className="flex min-w-0 items-baseline gap-2">
                <span className="truncate text-xs font-medium" title={hint ?? undefined}>
                  {cacheReasonLabel(r.reason, t)}
                </span>
                <span className="shrink-0 text-2xs text-muted-foreground tabular-nums">
                  {t(`${r.requests.toLocaleString(locale)} 条`, `${r.requests.toLocaleString(locale)} req`)}
                </span>
              </div>
              <span className="shrink-0 text-2xs tabular-nums text-muted-foreground">
                {formatTokens(w)} · {formatPercent(share)}
              </span>
              {/* 条子横跨两列：数字在上、条子在下，窄屏上也不会把名字挤成一个字。 */}
              <div
                className="col-span-2 h-1.5 overflow-hidden rounded-full bg-muted-foreground/16"
                role="img"
                aria-label={t(
                  `${cacheReasonLabel(r.reason, t)}：白付 ${w.toLocaleString(locale)} token，占 ${formatPercent(share)}`,
                  `${cacheReasonLabel(r.reason, t)}: ${w.toLocaleString(locale)} tokens paid uncached, ${formatPercent(share)} of the total`,
                )}
              >
                <div
                  className={cn(
                    'h-full rounded-full',
                    meta?.tone === 'bad' ? 'bg-chart-1' : 'bg-muted-foreground/40',
                  )}
                  style={{ width: `${Math.max(share * 100, share > 0 ? 1.5 : 0)}%` }}
                />
              </div>
            </li>
          )
        })}
      </ul>
      <p className="text-2xs leading-4 text-muted-foreground">
        {t(
          '排序按白付的输入 token，不按条数——一条长对话未命中比十条小请求贵得多。深色条子那几类是真的有东西可修（把名字悬起来看该做什么），浅色那几类本来就该未命中。',
          'Ranked by input tokens paid uncached rather than by request count — one long conversation missing costs far more than ten small requests. The darker bars are the ones worth fixing (hover a name for what to do); the pale ones were always going to miss.',
        )}
      </p>
    </section>
  )
}
