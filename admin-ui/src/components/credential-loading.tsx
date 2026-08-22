import {
  Card,
  CardAction,
  CardDescription,
  CardFooter,
  CardHeader,
  CardPanel,
  CardTitle,
} from '@/components/ui/card'
import { Skeleton } from '@/components/ui/skeleton'
import { useI18n } from '@/lib/i18n'

export function CredentialLoadingState({
  view, selectable = false, count = 10,
}: {
  view: 'card' | 'list'
  selectable?: boolean
  count?: number
}) {
  const { t } = useI18n()
  return (
    <div
      role="status"
      aria-live="polite"
      aria-label={view === 'card'
        ? t('正在加载账号卡片', 'Loading account cards')
        : t('正在加载账号列表', 'Loading account list')}
    >
      <span className="sr-only">{t('加载中', 'Loading')}</span>
      {view === 'card'
        ? <CardSkeletons selectable={selectable} count={count} />
        : <TableSkeletons selectable={selectable} count={count} />}
    </div>
  )
}

function CardSkeletons({ selectable, count }: { selectable: boolean; count: number }) {
  return (
    <ul
      // 与真列表同一套列数（最多两列，1 → 2 的门槛 52rem，见 credential-workspace 那条注）：
      // 骨架和真数据列数不一样的话，加载完会整片重排一次。
      className="relative grid list-none grid-cols-1 items-stretch gap-3 p-0 min-[52rem]:grid-cols-2 sm:gap-4"
      aria-hidden="true"
    >
      {Array.from({ length: count }, (_, index) => (
        <li key={index} className="min-w-0 h-full">
          <Card render={<article />} className="@container/card min-h-[13rem] h-full overflow-hidden">
            <CardHeader className="p-4 pb-3">
              <CardTitle className="text-sm leading-snug">
                <div className="flex items-center gap-3">
                  {selectable && <Skeleton className="size-4 shrink-0" />}
                  <Skeleton className="hidden size-8 shrink-0 rounded-full @sm/card:flex" />
                  <div className="min-w-0 flex-1 space-y-2">
                    <Skeleton className="h-4 w-3/5" />
                    {/* 三段对应真卡片的元信息行：#id · 账号掩码 · 添加于 X。 */}
                    <CardDescription className="flex gap-2 text-xs">
                      <Skeleton className="h-3 w-6" />
                      <Skeleton className="h-3 w-14" />
                      <Skeleton className="h-3 w-20" />
                    </CardDescription>
                  </div>
                </div>
              </CardTitle>
              <CardAction><Skeleton className="size-8" /></CardAction>
            </CardHeader>

            <CardPanel className="space-y-3 px-4 pb-3 sm:pb-4">
              <div className="flex flex-wrap items-center gap-2">
                <Skeleton className="h-5 w-14" />
                <Skeleton className="h-5 w-16" />
                <Skeleton className="h-5 w-9" />
              </div>
              <section className="space-y-2">
                <div className="flex flex-wrap items-start justify-between gap-x-3 gap-y-1.5">
                  <Skeleton className="h-4 w-16" />
                  <Skeleton className="h-4 w-24" />
                </div>
                <div className="grid gap-3 @sm/card:grid-cols-2 @sm/card:gap-4">
                  <QuotaSkeleton />
                  <QuotaSkeleton />
                </div>
              </section>
            </CardPanel>

            {/* 页脚：请求数 · 累计费用 · RPM，开关钉在最右，见 credential-card 的 CardFooter。
                三项现在都是「图标 + 数字」的同一种块，骨架也就画成同高的三条——请求数那格原来
                是颗按钮（h-9），骨架跟着画高一截，真数据一来页脚会矮下去抖一下。 */}
            <CardFooter className="mt-auto grid grid-cols-[minmax(0,1fr)_auto] items-center gap-3 border-t bg-muted/32 px-4 py-2.5 sm:py-3">
              <div className="flex min-w-0 items-center gap-3 @sm/card:gap-4">
                <Skeleton className="h-4 w-14" />
                <Skeleton className="h-4 w-16" />
                <Skeleton className="h-4 w-16" />
              </div>
              <div className="flex items-center gap-2">
                <Skeleton className="h-5 w-9 rounded-full" />
              </div>
            </CardFooter>
          </Card>
        </li>
      ))}
    </ul>
  )
}

/** 一条额度窗口：窗口名标签 · 进度条 · 百分比 · 重置倒计时，见 credential-card 的 QuotaMeter。 */
function QuotaSkeleton() {
  return (
    <div className="flex items-center gap-1.5">
      <Skeleton className="h-4 w-7 shrink-0 rounded-[.25rem]" />
      <Skeleton className="h-1.5 min-w-6 flex-1 rounded-full" />
      <Skeleton className="h-3 w-7 shrink-0" />
      <Skeleton className="h-3 w-10 shrink-0" />
    </div>
  )
}

function TableSkeletons({ selectable, count }: { selectable: boolean; count: number }) {
  /**
   * 列宽照抄真表格的 [COL]（credential-row）：勾选 / 账号 / 状态 / 优先级 / 套餐 / 两个额度 /
   * RPM / 最近使用 / 花费 / 开关 / 操作。骨架与真数据列宽不一致的话，加载完整张表会横向重排一次。
   */
  const desktopColumns = selectable
    ? 'grid-cols-[2.5rem_minmax(10rem,1fr)_8rem_4rem_6rem_10rem_10rem_6rem_6rem_6rem_3.5rem_2.5rem]'
    : 'grid-cols-[0.75rem_minmax(10rem,1fr)_8rem_4rem_6rem_10rem_10rem_6rem_6rem_6rem_3.5rem_2.5rem]'

  return (
    <div className="overflow-hidden">
      <div className="hidden xl:block">
        <div className={`grid h-10 items-center border-b bg-muted/30 ${desktopColumns}`}>
          <div className="flex justify-center">
            {selectable && <Skeleton className="size-4" />}
          </div>
          <div className="px-2.5"><Skeleton className="h-3 w-14" /></div>
          <div className="px-2.5"><Skeleton className="h-3 w-10" /></div>
          <div className="px-2.5"><Skeleton className="h-3 w-12" /></div>
          <div className="px-2.5"><Skeleton className="h-3 w-10" /></div>
          <div className="px-2.5"><Skeleton className="h-3 w-14" /></div>
          <div className="px-2.5"><Skeleton className="h-3 w-14" /></div>
          <div className="px-2.5"><Skeleton className="h-3 w-10" /></div>
          <div className="px-2.5"><Skeleton className="h-3 w-14" /></div>
          <div className="px-2.5"><Skeleton className="h-3 w-12" /></div>
          <div className="px-2.5"><Skeleton className="h-3 w-8" /></div>
          <div className="px-2.5" />
        </div>
        <div className="divide-y">
          {Array.from({ length: count }, (_, index) => (
            <div
              key={index}
              className={`grid h-[68px] items-center ${desktopColumns}`}
            >
              <div className="flex justify-center">
                {selectable && <Skeleton className="size-4" />}
              </div>
              {/* 身份格：名字一行 + `#3 · …9f31d0` 一行。 */}
              <div className="min-w-0 space-y-2 px-2.5">
                <Skeleton className="h-3.5 w-4/5" />
                <div className="flex items-center gap-2">
                  <Skeleton className="h-2.5 w-8" />
                  <Skeleton className="h-2.5 w-16" />
                </div>
              </div>
              {/* 状态 / 优先级 / 套餐：三枚徽章。 */}
              <div className="px-2.5"><Skeleton className="h-5 w-16 rounded-md" /></div>
              <div className="px-2.5"><Skeleton className="h-5 w-8 rounded-md" /></div>
              <div className="px-2.5"><Skeleton className="h-5 w-12 rounded-md" /></div>
              {Array.from({ length: 2 }, (_, quotaIndex) => (
                <div key={quotaIndex} className="min-w-0 space-y-2 px-2.5">
                  <div className="flex items-center justify-between gap-2">
                    <Skeleton className="h-3 w-10" />
                    <Skeleton className="h-3 w-8" />
                  </div>
                  <Skeleton className="h-1.5 w-full rounded-full" />
                </div>
              ))}
              <div className="min-w-0 px-2.5"><Skeleton className="h-3 w-12" /></div>
              <div className="min-w-0 px-2.5"><Skeleton className="h-3 w-14" /></div>
              <div className="px-2.5"><Skeleton className="h-4 w-14" /></div>
              {/* 行尾：开关，再是 ⋯ 菜单。 */}
              <div className="flex justify-center px-2.5">
                <Skeleton className="h-5 w-9 rounded-full" />
              </div>
              <div className="flex justify-end px-2.5">
                <Skeleton className="size-6 rounded-md" />
              </div>
            </div>
          ))}
        </div>
      </div>

      <div className="divide-y xl:hidden">
        {Array.from({ length: count }, (_, index) => (
          <div key={index} className="px-3 py-3 sm:px-5 sm:py-4">
            <div className="flex items-start gap-3">
              {selectable && <Skeleton className="mt-3 size-4 shrink-0" />}
              <div className="min-w-0 flex-1 space-y-2">
                <div className="flex items-center gap-2">
                  <Skeleton className="h-4 w-3/5" />
                  <Skeleton className="h-3 w-8" />
                </div>
                <Skeleton className="h-3 w-20" />
              </div>
              <div className="flex shrink-0 items-start gap-1">
                <div className="flex flex-col items-end gap-3">
                  <Skeleton className="h-5 w-9 rounded-full" />
                  <Skeleton className="h-5 w-14 rounded-md" />
                </div>
                <Skeleton className="size-7 rounded-md" />
              </div>
            </div>

            <div className="mt-3 grid grid-cols-2 gap-3 border-t border-border/70 pt-3 sm:mt-4 sm:gap-4 sm:pt-4">
              {Array.from({ length: 2 }, (_, quotaIndex) => (
                <div key={quotaIndex} className="min-w-0 space-y-2">
                  <div className="flex items-center justify-between gap-2">
                    <Skeleton className="h-3 w-5" />
                    <Skeleton className="h-3 w-8" />
                  </div>
                  <Skeleton className="h-1.5 w-full rounded-full" />
                </div>
              ))}
            </div>

            <div className="mt-3 grid grid-cols-2 gap-3 border-t border-border/70 pt-3 sm:mt-4 sm:grid-cols-3 sm:gap-4 sm:pt-4">
              {Array.from({ length: 3 }, (_, factIndex) => (
                <div key={factIndex} className="min-w-0">
                  <Skeleton className="h-2.5 w-12" />
                  <Skeleton className="mt-2 h-3 w-14" />
                </div>
              ))}
            </div>
          </div>
        ))}
      </div>
    </div>
  )
}
